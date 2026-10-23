//! The per-user S/MIME certificate store: the user's own certificates with their private keys
//! (sealed at rest with a key derived from the server secret, bound to the row), certificates
//! of correspondents (collected from verified signed mail or imported), and issuers the user
//! trusts in addition to the server's trust store. Own certificates are assigned to an
//! account or one of its sending addresses for signing and decrypting.

use super::crypto::SecretBox;
use super::smime::{self, CertInfo, Identity, PrivateKey};
use crate::util::{new_id, now};
use crate::{Core, Error, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

/// Certificates per user.
const MAX_CERTS: i64 = 500;
/// An imported file at most (a PKCS#12 with a long chain is a few KB).
pub const MAX_IMPORT_BYTES: usize = 256 * 1024;
/// Expiry warnings start this long before the end of validity.
pub const EXPIRY_WARNING: Duration = Duration::days(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CertKind {
    /// The user's own, with a private key.
    Own,
    /// A correspondent's (for encrypting to them, and recognizing their signatures).
    Contact,
    /// An issuer the user trusts.
    Authority,
}

impl CertKind {
    fn as_str(self) -> &'static str {
        match self {
            CertKind::Own => "own",
            CertKind::Contact => "contact",
            CertKind::Authority => "authority",
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    id: String,
    kind: String,
    source: String,
    emails: String,
    cert_der: Vec<u8>,
    chain_der: Option<Vec<u8>>,
    key_sealed: Option<String>,
    created_at: DateTime<Utc>,
}

const COLS: &str = "id, kind, source, emails, cert_der, chain_der, key_sealed, created_at";

/// A stored certificate as the API shows it (never the key).
#[derive(Debug, Clone, Serialize)]
pub struct CertView {
    pub id: String,
    pub kind: String,
    pub source: String,
    #[serde(flatten)]
    pub info: CertInfo,
    pub has_key: bool,
    pub expired: bool,
    /// Ends within [`EXPIRY_WARNING`].
    pub expires_soon: bool,
    pub created_at: DateTime<Utc>,
}

/// An own certificate assigned to an account address.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct IdentityView {
    pub account_id: String,
    pub email: String,
    pub cert_id: String,
    pub sign_default: bool,
    pub encrypt_default: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IdentityInput {
    pub account_id: String,
    /// The account address or an alias; default the account address.
    pub email: Option<String>,
    /// None removes the assignment.
    pub cert_id: Option<String>,
    #[serde(default)]
    pub sign_default: bool,
    #[serde(default)]
    pub encrypt_default: bool,
}

/// Whether recipients can be encrypted to.
#[derive(Debug, Clone, Serialize)]
pub struct RecipientStatus {
    pub email: String,
    pub has_certificate: bool,
    pub not_after: Option<DateTime<Utc>>,
}

fn split_der_chain(chain: &[u8]) -> Vec<Vec<u8>> {
    // Concatenated DER certificates: walk the outer SEQUENCE headers.
    let mut out = Vec::new();
    let mut rest = chain;
    while !rest.is_empty() {
        match x509_parser::parse_x509_certificate(rest) {
            Ok((tail, _)) => {
                out.push(rest[..rest.len() - tail.len()].to_vec());
                rest = tail;
            }
            Err(_) => break,
        }
    }
    out
}

/// The server's trust anchors: `[mail_security] smime_trust_store` (a PEM bundle), else the
/// system bundle. Loaded once.
fn system_anchors(path: Option<&std::path::Path>) -> &'static Vec<Vec<u8>> {
    static ANCHORS: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    ANCHORS.get_or_init(|| {
        let candidates: Vec<std::path::PathBuf> = match path {
            Some(p) => vec![p.to_path_buf()],
            None => [
                "/etc/ssl/certs/ca-certificates.crt",
                "/etc/pki/tls/certs/ca-bundle.crt",
                "/etc/ssl/cert.pem",
                "/etc/ssl/ca-bundle.pem",
            ]
            .iter()
            .map(std::path::PathBuf::from)
            .collect(),
        };
        for c in candidates {
            if let Ok(data) = std::fs::read(&c) {
                let certs = smime::read_certificates(&data);
                if !certs.is_empty() {
                    tracing::debug!(path = %c.display(), count = certs.len(), "S/MIME trust store loaded");
                    return certs;
                }
            }
        }
        Vec::new()
    })
}

impl Core {
    fn smime_box(&self) -> SecretBox {
        SecretBox::new(&self.secret)
    }

    fn view(&self, r: &Row) -> Result<CertView> {
        let info = smime::cert_info(&r.cert_der).map_err(Error::internal)?;
        let t = now();
        Ok(CertView {
            id: r.id.clone(),
            kind: r.kind.clone(),
            source: r.source.clone(),
            expired: info.not_after < t,
            expires_soon: info.not_after >= t && info.not_after < t + EXPIRY_WARNING,
            has_key: r.key_sealed.is_some(),
            info,
            created_at: r.created_at,
        })
    }

    async fn smime_rows(&self, user_id: &str, kind: Option<CertKind>) -> Result<Vec<Row>> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {COLS} FROM mail_smime_certs WHERE user_id = ? AND (? IS NULL OR kind = ?) ORDER BY created_at"
        )))
        .bind(user_id)
        .bind(kind.map(CertKind::as_str))
        .bind(kind.map(CertKind::as_str))
        .fetch_all(&self.db)
        .await?)
    }

    pub async fn smime_certificates(&self, user_id: &str) -> Result<Vec<CertView>> {
        let mut out = Vec::new();
        for r in self.smime_rows(user_id, None).await? {
            match self.view(&r) {
                Ok(v) => out.push(v),
                Err(e) => tracing::warn!(cert = %r.id, error = %e, "unreadable stored certificate"),
            }
        }
        Ok(out)
    }

    pub async fn smime_certificate(&self, user_id: &str, id: &str) -> Result<(CertView, Vec<u8>)> {
        let r: Row = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {COLS} FROM mail_smime_certs WHERE user_id = ? AND id = ?")))
            .bind(user_id)
            .bind(id)
            .fetch_optional(&self.db)
            .await?
            .ok_or(Error::NotFound)?;
        Ok((self.view(&r)?, r.cert_der))
    }

    /// Stores a certificate (with its key and chain for own ones). An existing entry for the
    /// same certificate is upgraded (a contact certificate becomes own when its key arrives).
    async fn smime_store(&self, user_id: &str, kind: CertKind, source: &str, cert: &[u8], chain: &[Vec<u8>], key: Option<&PrivateKey>) -> Result<CertView> {
        let info = smime::cert_info(cert).map_err(Error::bad)?;
        let existing: Option<(String, String)> = sqlx::query_as("SELECT id, kind FROM mail_smime_certs WHERE user_id = ? AND fingerprint = ?")
            .bind(user_id)
            .bind(&info.fingerprint)
            .fetch_optional(&self.db)
            .await?;
        if let Some((id, old_kind)) = &existing {
            // Never downgrade an own certificate (and never drop its key) by importing it again.
            if old_kind == "own" || kind != CertKind::Own {
                if old_kind != kind.as_str() && kind == CertKind::Authority && old_kind == "contact" {
                    sqlx::query("UPDATE mail_smime_certs SET kind = 'authority' WHERE id = ?")
                        .bind(id)
                        .execute(&self.db)
                        .await?;
                }
                return Ok(self.smime_certificate(user_id, id).await?.0);
            }
        }
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mail_smime_certs WHERE user_id = ?")
            .bind(user_id)
            .fetch_one(&self.db)
            .await?;
        if existing.is_none() && count >= MAX_CERTS {
            return Err(Error::bad(format!("at most {MAX_CERTS} certificates")));
        }
        let id = existing.as_ref().map(|(id, _)| id.clone()).unwrap_or_else(new_id);
        let sealed = match key {
            Some(k) => {
                let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, k.pkcs8.as_slice());
                Some(self.smime_box().seal(&format!("smime-key:{id}"), &b64)?)
            }
            None => None,
        };
        let chain_der: Option<Vec<u8>> = (!chain.is_empty()).then(|| chain.concat());
        sqlx::query(
            "INSERT INTO mail_smime_certs (id, user_id, kind, source, emails, subject, issuer, serial, not_before, not_after, fingerprint, cert_der, chain_der, key_sealed, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, source = excluded.source, chain_der = excluded.chain_der, key_sealed = excluded.key_sealed",
        )
        .bind(&id)
        .bind(user_id)
        .bind(kind.as_str())
        .bind(source)
        .bind(serde_json::to_string(&info.emails)?)
        .bind(&info.subject)
        .bind(&info.issuer)
        .bind(&info.serial)
        .bind(info.not_before)
        .bind(info.not_after)
        .bind(&info.fingerprint)
        .bind(cert)
        .bind(&chain_der)
        .bind(&sealed)
        .bind(now())
        .execute(&self.db)
        .await?;
        if kind == CertKind::Own {
            // Encrypted mail read before this key arrived is re-read with it.
            self.smime_forget_encrypted_bodies(user_id).await?;
        }
        self.hub
            .publish([user_id], &serde_json::json!({"type": "mail.security", "what": "certificates"}));
        Ok(self.smime_certificate(user_id, &id).await?.0)
    }

    /// Imports a PKCS#12 file (certificate, key and chain) as an own certificate.
    pub async fn smime_import_pkcs12(&self, user_id: &str, data: &[u8], password: &str) -> Result<CertView> {
        if data.len() > MAX_IMPORT_BYTES {
            return Err(Error::bad("the file is too large for a certificate"));
        }
        let id = smime::read_pkcs12(data, password).map_err(Error::bad)?;
        self.smime_check_own(&id)?;
        self.smime_store(user_id, CertKind::Own, "pkcs12", &id.cert, &id.chain, Some(&id.key)).await
    }

    /// Imports PEM or DER: a certificate (the first one; further ones are its chain) and,
    /// optionally, its private key (possibly encrypted with `password`). Without a key, a CA
    /// certificate becomes a trusted issuer and anything else a correspondent's certificate.
    pub async fn smime_import_pem(&self, user_id: &str, cert_data: &[u8], key_data: Option<&[u8]>, password: Option<&str>) -> Result<CertView> {
        if cert_data.len() > MAX_IMPORT_BYTES || key_data.is_some_and(|k| k.len() > MAX_IMPORT_BYTES) {
            return Err(Error::bad("the file is too large for a certificate"));
        }
        let mut certs = smime::read_certificates(cert_data);
        if certs.is_empty() {
            return Err(Error::bad("no certificate found (PEM or DER expected)"));
        }
        // With a key, the certificate for that key leads, whatever the order in the file.
        let key = match key_data {
            Some(k) if !k.is_empty() => Some(smime::read_private_key(k, password).map_err(Error::bad)?),
            _ => smime::read_private_key(cert_data, password).ok(),
        };
        match key {
            Some(key) => {
                let pos = certs
                    .iter()
                    .position(|c| smime::key_matches(&key, c))
                    .ok_or_else(|| Error::bad("the private key does not belong to the certificate"))?;
                let leaf = certs.remove(pos);
                let id = Identity { cert: leaf, chain: certs, key };
                self.smime_check_own(&id)?;
                self.smime_store(user_id, CertKind::Own, "pem", &id.cert, &id.chain, Some(&id.key)).await
            }
            None => {
                let leaf = certs.remove(0);
                let info = smime::cert_info(&leaf).map_err(Error::bad)?;
                let kind = if info.is_ca { CertKind::Authority } else { CertKind::Contact };
                if kind == CertKind::Contact && info.emails.is_empty() {
                    return Err(Error::bad("the certificate names no email address"));
                }
                self.smime_store(user_id, kind, "imported", &leaf, &[], None).await
            }
        }
    }

    fn smime_check_own(&self, id: &Identity) -> Result<()> {
        let info = smime::cert_info(&id.cert).map_err(Error::bad)?;
        if info.emails.is_empty() {
            return Err(Error::bad("the certificate names no email address"));
        }
        if !info.email_protection {
            return Err(Error::bad("the certificate is not meant for email (no email protection usage)"));
        }
        Ok(())
    }

    /// Deletes a certificate. Deleting an own certificate also forgets mail decrypted with it.
    pub async fn smime_delete(&self, user_id: &str, id: &str) -> Result<()> {
        let (view, _) = self.smime_certificate(user_id, id).await?;
        sqlx::query("DELETE FROM mail_smime_certs WHERE user_id = ? AND id = ?")
            .bind(user_id)
            .bind(id)
            .execute(&self.db)
            .await?;
        if view.kind == "own" {
            self.smime_forget_encrypted_bodies(user_id).await?;
            self.mail_drop_decrypted_index(user_id).await?;
        }
        self.hub
            .publish([user_id], &serde_json::json!({"type": "mail.security", "what": "certificates"}));
        Ok(())
    }

    /// Drops cached bodies of encrypted mail (they hold decrypted text, or none for want of a
    /// key); they are decrypted again (or not) when next read.
    pub(crate) async fn smime_forget_encrypted_bodies(&self, user_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM mail_bodies WHERE encrypted = 1 AND account_id IN (SELECT id FROM mail_accounts WHERE user_id = ?)")
            .bind(user_id)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    fn unseal(&self, r: &Row) -> Result<Option<PrivateKey>> {
        let Some(sealed) = &r.key_sealed else { return Ok(None) };
        let b64 = self.smime_box().open(&format!("smime-key:{}", r.id), sealed)?;
        let der = zeroize::Zeroizing::new(
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64.as_bytes()).map_err(|_| Error::internal("stored private key is corrupt"))?,
        );
        Ok(Some(PrivateKey::from_der(&der).map_err(Error::internal)?))
    }

    /// The user's own certificates with their keys (for decrypting).
    pub(crate) async fn smime_decryption_keys(&self, user_id: &str) -> Result<Vec<(Vec<u8>, PrivateKey)>> {
        let mut out = Vec::new();
        for r in self.smime_rows(user_id, Some(CertKind::Own)).await? {
            match self.unseal(&r) {
                Ok(Some(k)) => out.push((r.cert_der, k)),
                Ok(None) => {}
                Err(e) => tracing::warn!(cert = %r.id, error = %e, "S/MIME key cannot be opened"),
            }
        }
        Ok(out)
    }

    /// The signing identity for sending as `email` from `account_id`: the certificate assigned
    /// to that address (else to the account address), else an own valid certificate for it.
    pub(crate) async fn smime_signing_identity(&self, user_id: &str, account_id: &str, email: &str) -> Result<Option<Identity>> {
        let email = email.trim().to_lowercase();
        let assigned: Option<String> = sqlx::query_scalar(
            "SELECT i.cert_id FROM mail_smime_identities i JOIN mail_accounts a ON a.id = i.account_id
             WHERE a.user_id = ? AND i.account_id = ? AND (i.email = ? OR i.email = lower(a.email)) ORDER BY i.email = ? DESC LIMIT 1",
        )
        .bind(user_id)
        .bind(account_id)
        .bind(&email)
        .bind(&email)
        .fetch_optional(&self.db)
        .await?;
        let rows = self.smime_rows(user_id, Some(CertKind::Own)).await?;
        let t = now();
        let pick = rows.iter().find(|r| Some(&r.id) == assigned.as_ref()).or_else(|| {
            rows.iter().find(|r| {
                serde_json::from_str::<Vec<String>>(&r.emails).is_ok_and(|e| e.contains(&email))
                    && smime::cert_info(&r.cert_der).is_ok_and(|i| i.not_after > t && i.not_before <= t)
            })
        });
        let Some(r) = pick else { return Ok(None) };
        let Some(key) = self.unseal(r)? else { return Ok(None) };
        Ok(Some(Identity {
            cert: r.cert_der.clone(),
            chain: r.chain_der.as_deref().map(split_der_chain).unwrap_or_default(),
            key,
        }))
    }

    /// The best certificate to encrypt to `email`: valid now, for email, RSA; the one that
    /// lasts longest.
    pub(crate) async fn smime_recipient_cert(&self, user_id: &str, email: &str) -> Result<Option<(Vec<u8>, CertInfo)>> {
        let email = email.trim().to_lowercase();
        let like = format!("%\"{}\"%", email.replace(['%', '_', '"'], ""));
        let rows: Vec<Vec<u8>> = sqlx::query_scalar("SELECT cert_der FROM mail_smime_certs WHERE user_id = ? AND kind IN ('own', 'contact') AND emails LIKE ?")
            .bind(user_id)
            .bind(&like)
            .fetch_all(&self.db)
            .await?;
        let t = now();
        Ok(rows
            .into_iter()
            .filter_map(|der| smime::cert_info(&der).ok().map(|i| (der, i)))
            .filter(|(_, i)| i.emails.contains(&email) && i.not_after > t && i.not_before <= t && i.email_protection && i.key_type == "rsa")
            .max_by_key(|(_, i)| i.not_after))
    }

    pub async fn smime_recipients_status(&self, user_id: &str, emails: &[String]) -> Result<Vec<RecipientStatus>> {
        let mut out = Vec::new();
        for e in emails.iter().take(200) {
            let Some(a) = super::parse::Addr::parse(e) else { continue };
            let c = self.smime_recipient_cert(user_id, &a.address).await?;
            out.push(RecipientStatus {
                email: a.address,
                has_certificate: c.is_some(),
                not_after: c.map(|(_, i)| i.not_after),
            });
        }
        Ok(out)
    }

    /// Keeps the certificate of a verified signer for encrypting to them later.
    pub(crate) async fn smime_collect(&self, user_id: &str, cert: &[u8]) -> Result<()> {
        let Ok(info) = smime::cert_info(cert) else { return Ok(()) };
        if info.emails.is_empty() || info.is_ca || !info.email_protection {
            return Ok(());
        }
        let known: Option<String> = sqlx::query_scalar("SELECT id FROM mail_smime_certs WHERE user_id = ? AND fingerprint = ?")
            .bind(user_id)
            .bind(&info.fingerprint)
            .fetch_optional(&self.db)
            .await?;
        if known.is_none() {
            self.smime_store(user_id, CertKind::Contact, "collected", cert, &[], None).await?;
        }
        Ok(())
    }

    /// Trust anchors for `user_id`: the server's store plus the issuers the user trusts.
    pub(crate) async fn smime_anchors(&self, user_id: &str) -> Result<Vec<Vec<u8>>> {
        let mut anchors = system_anchors(self.config.mail_security.smime_trust_store.as_deref()).clone();
        let mine: Vec<Vec<u8>> = sqlx::query_scalar("SELECT cert_der FROM mail_smime_certs WHERE user_id = ? AND kind = 'authority'")
            .bind(user_id)
            .fetch_all(&self.db)
            .await?;
        anchors.extend(mine);
        Ok(anchors)
    }

    /// Certificates that can serve as intermediates (own chains and stored certificates).
    pub(crate) async fn smime_intermediates(&self, user_id: &str) -> Result<Vec<Vec<u8>>> {
        let rows = self.smime_rows(user_id, None).await?;
        let mut out = Vec::new();
        for r in rows {
            if let Some(c) = &r.chain_der {
                out.extend(split_der_chain(c));
            }
            out.push(r.cert_der);
        }
        Ok(out)
    }

    pub async fn smime_identities(&self, user_id: &str) -> Result<Vec<IdentityView>> {
        Ok(sqlx::query_as(
            "SELECT i.account_id, i.email, i.cert_id, i.sign_default, i.encrypt_default FROM mail_smime_identities i
             JOIN mail_accounts a ON a.id = i.account_id WHERE a.user_id = ? ORDER BY i.account_id, i.email",
        )
        .bind(user_id)
        .fetch_all(&self.db)
        .await?)
    }

    /// Assigns (or with `cert_id: None` removes) the certificate of an account address.
    pub async fn smime_set_identity(&self, user_id: &str, input: &IdentityInput) -> Result<Vec<IdentityView>> {
        // Which key an account signs with is part of how it authenticates its mail.
        let account = self.mail_access(user_id, &input.account_id, super::delegation::MailRight::Authenticate).await?;
        let email = input.email.as_deref().unwrap_or(&account.email).trim().to_lowercase();
        if super::parse::Addr::parse(&email).is_none() {
            return Err(Error::bad(format!("{email:?} is not an email address")));
        }
        match &input.cert_id {
            None => {
                sqlx::query("DELETE FROM mail_smime_identities WHERE account_id = ? AND email = ?")
                    .bind(&account.id)
                    .bind(&email)
                    .execute(&self.db)
                    .await?;
            }
            Some(cert_id) => {
                let (view, _) = self.smime_certificate(user_id, cert_id).await?;
                if view.kind != "own" || !view.has_key {
                    return Err(Error::bad("only your own certificates (with their private key) can be assigned"));
                }
                if !view.info.emails.contains(&email) {
                    return Err(Error::bad(format!("the certificate is not issued for {email}")));
                }
                sqlx::query(
                    "INSERT INTO mail_smime_identities (account_id, email, cert_id, sign_default, encrypt_default) VALUES (?, ?, ?, ?, ?)
                     ON CONFLICT(account_id, email) DO UPDATE SET cert_id = excluded.cert_id, sign_default = excluded.sign_default, encrypt_default = excluded.encrypt_default",
                )
                .bind(&account.id)
                .bind(&email)
                .bind(cert_id)
                .bind(input.sign_default)
                .bind(input.encrypt_default)
                .execute(&self.db)
                .await?;
            }
        }
        self.hub.publish([user_id], &serde_json::json!({"type": "mail.security", "what": "identities"}));
        self.smime_identities(user_id).await
    }
}
