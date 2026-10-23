//! What the reader shows about a message's trustworthiness, and the S/MIME and PGP layers:
//!
//! * Reading: [`Core::mail_unwrap`] turns an S/MIME message into the readable one
//!   (decrypting with the user's keys, verifying and removing signatures), so bodies,
//!   attachments and replies use the inner content. Decrypted text is cached with the body
//!   (marked `encrypted`) but never goes into previews, the shared search index or the
//!   semantic index; with the user's opt-in it goes into a separate Mail-only index.
//! * [`Core::mail_security_report`]: trusted sender, internal/external sender, phishing
//!   findings, DKIM (verified once per message and stored), S/MIME layers with the signer's
//!   chain, and PGP (for Mailvelope in the browser).
//! * Sending: [`Core::mail_secure_outgoing`] signs and/or encrypts a built message, or wraps
//!   Mailvelope's armored text as PGP/MIME.
//! * View source and all attachments as a ZIP (stored entries, streamed, size-capped).

use super::dkim::{self, Dkim};
use super::parse::{self, Addr};
use super::phishing;
use super::smime::{self, CertInfo, ChainCheck, Kind};
use super::trust::{self, List};
use crate::util::now;
use crate::{Core, Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;

/// S/MIME layers unwrapped at most (encrypted, signed, signed again…).
const MAX_LAYERS: usize = 4;
/// View source: messages up to this size are shown, larger ones are cut (download them).
pub const SOURCE_CAP: usize = 2 * 1024 * 1024;
/// A ZIP of a message's attachments holds at most this much.
pub const ZIP_CAP: usize = 100 * 1024 * 1024;
/// PGP armor handed to the browser at most.
const ARMOR_CAP: usize = 4 * 1024 * 1024;
/// DKIM temporary failures are retried after this long.
const DKIM_RETRY: chrono::Duration = chrono::Duration::minutes(15);

/// `[mail_security]` in ferrite.toml.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MailSecurityConfig {
    /// One DNS server (host:port) for DKIM key lookups instead of the system's
    /// (`FERRITE_MAIL_DNS`).
    pub dns_server: Option<String>,
    /// A PEM bundle of S/MIME trust anchors instead of the system bundle
    /// (`FERRITE_SMIME_TRUST_STORE`).
    pub smime_trust_store: Option<PathBuf>,
    /// Lets the image proxy and favicon fetcher reach private and loopback addresses. Only for
    /// tests (`FERRITE_MAIL_PROXY_ALLOW_PRIVATE=1`); never in production.
    pub proxy_allow_private: bool,
}

/// Live state: the DKIM verifier (resolver and key cache), built on first use.
#[derive(Default)]
pub struct SecurityState {
    dkim: OnceLock<Dkim>,
}

impl SecurityState {
    pub fn dkim(&self, server: Option<&str>) -> &Dkim {
        self.dkim.get_or_init(|| {
            let addr = server.and_then(|s| {
                s.parse::<std::net::SocketAddr>()
                    .or_else(|_| s.parse::<std::net::IpAddr>().map(|ip| std::net::SocketAddr::new(ip, 53)))
                    .map_err(|_| tracing::warn!(server = s, "mail_security.dns_server is not host:port; using the system resolver"))
                    .ok()
            });
            Dkim::new(addr)
        })
    }
}

// ---- reading ---------------------------------------------------------------------------------

/// The verdict on one signature layer.
#[derive(Debug, Clone, Serialize)]
pub struct SignatureView {
    /// The signature matches the content.
    pub valid: bool,
    pub problem: Option<String>,
    pub signer: Option<CertInfo>,
    pub chain: Option<ChainCheck>,
    /// The signer's certificate names the From address.
    pub matches_sender: bool,
    pub signed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SmimeView {
    pub encrypted: bool,
    pub decrypted: bool,
    /// Why it could not be decrypted.
    pub decrypt_error: Option<String>,
    pub signatures: Vec<SignatureView>,
}

/// A message as the reader sees it, with the S/MIME layers removed.
#[derive(Debug, Clone)]
pub struct Unwrapped {
    pub raw: Vec<u8>,
    pub smime: Option<SmimeView>,
    /// The content was encrypted (decrypted or not).
    pub encrypted: bool,
    /// Certificates of signers whose signature verified (to collect).
    pub verified_signers: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PgpKind {
    /// multipart/encrypted; protocol="application/pgp-encrypted" (RFC 3156)
    PgpMime,
    /// An armored PGP MESSAGE in the text.
    Inline,
    /// multipart/signed with an application/pgp-signature.
    PgpMimeSigned,
    /// An armored clear-signed text.
    InlineSigned,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PgpView {
    pub kind: PgpKind,
    /// The armored block for Mailvelope (encrypted kinds and inline signed text).
    pub armored: Option<String>,
}

/// PGP in a (readable) message.
pub fn detect_pgp(raw: &[u8], text: &str) -> Option<PgpView> {
    let (he, bs) = smime::split_header(raw);
    let (ty, params) = smime::content_type(&raw[..he]);
    let protocol = params
        .iter()
        .find(|(k, _)| k == "protocol")
        .map(|(_, v)| v.to_ascii_lowercase())
        .unwrap_or_default();
    let boundary = params.iter().find(|(k, _)| k == "boundary").map(|(_, v)| v.clone());
    if ty == "multipart/encrypted" && protocol == "application/pgp-encrypted" {
        let armored = boundary.and_then(|b| {
            smime::multipart_parts(&raw[bs.min(raw.len())..], &b).into_iter().find_map(|p| {
                let s = String::from_utf8_lossy(&p).into_owned();
                armor_block(&s, "PGP MESSAGE")
            })
        });
        return Some(PgpView {
            kind: PgpKind::PgpMime,
            armored,
        });
    }
    if ty == "multipart/signed" && protocol == "application/pgp-signature" {
        return Some(PgpView {
            kind: PgpKind::PgpMimeSigned,
            armored: None,
        });
    }
    if let Some(a) = armor_block(text, "PGP MESSAGE") {
        return Some(PgpView {
            kind: PgpKind::Inline,
            armored: Some(a),
        });
    }
    if let Some(start) = text.find("-----BEGIN PGP SIGNED MESSAGE-----") {
        let end_marker = "-----END PGP SIGNATURE-----";
        if let Some(e) = text[start..].find(end_marker) {
            return Some(PgpView {
                kind: PgpKind::InlineSigned,
                armored: Some(text[start..start + e + end_marker.len()].to_string()).filter(|a| a.len() <= ARMOR_CAP),
            });
        }
    }
    None
}

fn armor_block(s: &str, label: &str) -> Option<String> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = s.find(&begin)?;
    let stop = s[start..].find(&end)? + start + end.len();
    let block = s[start..stop].replace("\r\n", "\n");
    (block.len() <= ARMOR_CAP).then_some(block)
}

/// The From address of a raw message.
fn from_of(raw: &[u8]) -> Option<Addr> {
    mail_parser::MessageParser::default().parse_headers(raw).and_then(|m| {
        let f = m.from()?.first()?;
        Some(Addr {
            name: f.name().unwrap_or("").trim().to_string(),
            address: f.address()?.trim().to_string(),
        })
    })
}

impl Core {
    /// Removes the S/MIME layers of `raw` for `user_id`: decrypts with their keys, verifies and
    /// strips signatures. Plain messages come back unchanged (and cheaply).
    pub async fn mail_unwrap(&self, user_id: &str, raw: &[u8]) -> Unwrapped {
        let mut current = raw.to_vec();
        let mut view = SmimeView::default();
        let mut any = false;
        let mut verified_signers = Vec::new();
        let from = from_of(raw);
        for _ in 0..MAX_LAYERS {
            match smime::classify(&current) {
                Kind::Plain => break,
                Kind::Enveloped(der) => {
                    any = true;
                    view.encrypted = true;
                    let keys = match self.smime_decryption_keys(user_id).await {
                        Ok(k) => k,
                        Err(e) => {
                            view.decrypt_error = Some(e.to_string());
                            break;
                        }
                    };
                    if keys.is_empty() {
                        view.decrypt_error = Some("this message is encrypted with S/MIME; import your certificate with its private key to read it".into());
                        break;
                    }
                    match smime::decrypt(&der, &keys) {
                        Ok(entity) => {
                            view.decrypted = true;
                            current = smime::with_entity(&current, &entity);
                        }
                        Err(e) => {
                            view.decrypt_error = Some(e.to_string());
                            break;
                        }
                    }
                }
                Kind::SignedOpaque(der) => {
                    any = true;
                    let v = smime::verify(&der, None, &[]);
                    let content = v.content.clone();
                    view.signatures
                        .push(self.signature_view(user_id, &v, from.as_ref(), &mut verified_signers).await);
                    match content {
                        Some(entity) => current = smime::with_entity(&current, &smime::canonical_crlf(&entity)),
                        None => break,
                    }
                }
                Kind::SignedDetached { content, signature } => {
                    any = true;
                    let v = smime::verify(&signature, Some(&content), &[]);
                    view.signatures
                        .push(self.signature_view(user_id, &v, from.as_ref(), &mut verified_signers).await);
                    current = smime::with_entity(&current, &content);
                }
            }
        }
        Unwrapped {
            raw: if any { current } else { raw.to_vec() },
            encrypted: view.encrypted,
            smime: any.then_some(view),
            verified_signers,
        }
    }

    async fn signature_view(&self, user_id: &str, v: &smime::Verification, from: Option<&Addr>, verified: &mut Vec<Vec<u8>>) -> SignatureView {
        let signer = v.signer.as_deref().and_then(|c| smime::cert_info(c).ok());
        let chain = match &v.signer {
            Some(c) if v.valid => {
                let anchors = self.smime_anchors(user_id).await.unwrap_or_default();
                let mut pool = v.certificates.clone();
                pool.extend(self.smime_intermediates(user_id).await.unwrap_or_default());
                Some(smime::check_chain(c, &pool, &anchors, now()))
            }
            _ => None,
        };
        let matches_sender = match (&signer, from) {
            (Some(s), Some(f)) => s.emails.contains(&f.address.to_lowercase()),
            _ => false,
        };
        if v.valid {
            if let Some(c) = &v.signer {
                // Only a correct signature whose chain is not broken (expired or untrusted
                // still lets the user encrypt to them; an invalid chain does not).
                if chain.as_ref().is_some_and(|c| c.status != "invalid") && matches_sender {
                    verified.push(c.clone());
                }
            }
        }
        SignatureView {
            valid: v.valid,
            problem: v.problem.clone(),
            signer,
            chain,
            matches_sender,
            signed_at: v.signing_time,
        }
    }

    /// Indexes decrypted text for Mail search when the user opted in.
    pub(crate) async fn mail_index_decrypted(&self, user_id: &str, message_id: &str, text: &str) -> Result<()> {
        if !self.mail_security_settings(user_id).await?.index_decrypted {
            return Ok(());
        }
        let mut tx = self.db.begin().await?;
        sqlx::query("DELETE FROM mail_decrypted_fts WHERE message_id = ?")
            .bind(message_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO mail_decrypted_fts (message_id, user_id, body) VALUES (?, ?, ?)")
            .bind(message_id)
            .bind(user_id)
            .bind(text)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO mail_decrypted_indexed (message_id, user_id) VALUES (?, ?) ON CONFLICT(message_id) DO NOTHING")
            .bind(message_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Ids of the user's decrypted messages matching `q` (Mail search only).
    pub(crate) async fn mail_search_decrypted(&self, user_id: &str, q: &str) -> Result<Vec<String>> {
        let Some(query) = crate::search::fts_query(q) else { return Ok(vec![]) };
        Ok(
            sqlx::query_scalar("SELECT message_id FROM mail_decrypted_fts WHERE mail_decrypted_fts MATCH ? AND user_id = ? ORDER BY rank LIMIT 200")
                .bind(&query)
                .bind(user_id)
                .fetch_all(&self.db)
                .await?,
        )
    }

    /// With the opt-in: decrypts (and so indexes) encrypted messages of the searched folders
    /// that were never opened, a bounded number per search.
    pub(crate) async fn mail_index_pending_encrypted(&self, user_id: &str, a: &super::Account, f: &super::store::Folder) -> Result<()> {
        if !self.mail_security_settings(user_id).await?.index_decrypted || self.smime_decryption_keys(&a.user_id).await?.is_empty() {
            return Ok(());
        }
        let uids = {
            let mut conn = self.imap_conn(a).await?;
            let sel = conn.select(&f.name).await?;
            if f.uidvalidity.is_some_and(|v| v as u32 != sel.uidvalidity) {
                return Ok(());
            }
            let mut u = conn.uid_search("HEADER Content-Type \"pkcs7-mime\"").await?;
            u.sort_unstable();
            u
        };
        let newest: Vec<u32> = uids.into_iter().rev().take(25).collect();
        let pending: Vec<String> = sqlx::query_scalar(
            "SELECT m.id FROM mail_messages m LEFT JOIN mail_bodies b ON b.message_id = m.id
             WHERE m.folder_id = ? AND m.uid IN (SELECT value FROM json_each(?)) AND b.message_id IS NULL",
        )
        .bind(&f.id)
        .bind(serde_json::to_string(&newest)?)
        .fetch_all(&self.db)
        .await?;
        for id in pending {
            if let Err(e) = self.mail_message(user_id, &id).await {
                tracing::debug!(message = %id, error = %e, "indexing an encrypted message failed");
            }
        }
        Ok(())
    }

    /// Of `ids`, the messages whose decrypted content the assistant must not see: encrypted
    /// ones, unless the user allowed the assistant to read decrypted mail.
    pub async fn mail_hidden_from_assistant(&self, user_id: &str, ids: &[String]) -> Result<std::collections::HashSet<String>> {
        if self.mail_security_settings(user_id).await?.assistant_decrypted || ids.is_empty() {
            return Ok(Default::default());
        }
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT b.message_id FROM mail_bodies b JOIN mail_accounts a ON a.id = b.account_id
             WHERE a.user_id = ? AND b.encrypted = 1 AND b.message_id IN (SELECT value FROM json_each(?))",
        )
        .bind(user_id)
        .bind(serde_json::to_string(ids)?)
        .fetch_all(&self.db)
        .await?;
        Ok(rows.into_iter().collect())
    }

    /// The message with its S/MIME layers removed (for attachments and inline parts).
    pub async fn mail_readable_raw(&self, user_id: &str, id: &str) -> Result<(super::store::MessageSummary, Vec<u8>)> {
        let (a, _) = self.message_row(user_id, id).await?;
        let (m, raw) = self.mail_raw(user_id, id).await?;
        // Decrypted with the account owner's keys (a delegate reads on their behalf).
        let u = self.mail_unwrap(&a.user_id, &raw).await;
        Ok((m, u.raw))
    }

    /// DKIM for a cached message: the stored result, else verified now (and stored).
    pub async fn mail_dkim(&self, user_id: &str, id: &str, raw: &[u8], from_domain: Option<&str>) -> Result<dkim::Verdict> {
        let (_, m) = self.message_row(user_id, id).await?;
        type Stored = (String, Option<String>, Option<String>, Option<String>, DateTime<Utc>);
        let stored: Option<Stored> = sqlx::query_as("SELECT status, domain, selector, detail, checked_at FROM mail_dkim_results WHERE message_id = ?")
            .bind(&m.id)
            .fetch_optional(&self.db)
            .await?;
        if let Some((status, domain, selector, detail, at)) = stored {
            let status = dkim::Status::parse(&status);
            if status != dkim::Status::Temperror || now() - at < DKIM_RETRY {
                return Ok(dkim::Verdict {
                    status,
                    domain,
                    selector,
                    detail,
                });
            }
        }
        let verdict = self
            .mail
            .security
            .dkim(self.config.mail_security.dns_server.as_deref())
            .verify(raw, from_domain)
            .await;
        sqlx::query(
            "INSERT INTO mail_dkim_results (message_id, status, domain, selector, detail, checked_at) VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(message_id) DO UPDATE SET status = excluded.status, domain = excluded.domain, selector = excluded.selector,
             detail = excluded.detail, checked_at = excluded.checked_at",
        )
        .bind(&m.id)
        .bind(verdict.status.as_str())
        .bind(&verdict.domain)
        .bind(&verdict.selector)
        .bind(&verdict.detail)
        .bind(now())
        .execute(&self.db)
        .await?;
        Ok(verdict)
    }

    /// IMAP keywords of a message, read from the server (they are not cached).
    async fn mail_keywords(&self, a: &super::Account, m: &super::store::MessageSummary) -> Result<Vec<String>> {
        let f = self.folder_row(&m.folder_id).await?;
        let mut conn = self.imap_conn(a).await?;
        conn.ensure_selected(&f.name).await?;
        let fetched = conn.uid_fetch(&m.uid.to_string(), "(UID FLAGS)").await?;
        Ok(fetched.into_iter().find(|x| x.uid as i64 == m.uid).map(|x| x.keywords).unwrap_or_default())
    }

    /// Addresses of the user's contacts whose name is `name` (exactly, ignoring case).
    async fn contact_addresses_named(&self, user_id: &str, name: &str) -> Result<Vec<String>> {
        let n = name.trim();
        if n.is_empty() {
            return Ok(vec![]);
        }
        let mut out: Vec<String> = self
            .contacts(user_id, Some(n))
            .await?
            .into_iter()
            .filter(|c| c.name.trim().eq_ignore_ascii_case(n))
            .flat_map(|c| c.emails)
            .map(|e| e.trim().to_lowercase())
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// Everything the reader shows about a message's trustworthiness.
    pub async fn mail_security_report(&self, user_id: &str, id: &str) -> Result<SecurityReport> {
        let (a, m) = self.message_row(user_id, id).await?;
        let (_, raw) = self.mail_raw(user_id, id).await?;
        // The account owner's keys decrypt; signers are collected for the owner too.
        let unwrapped = self.mail_unwrap(&a.user_id, &raw).await;
        for c in &unwrapped.verified_signers {
            if let Err(e) = self.smime_collect(&a.user_id, c).await {
                tracing::debug!(error = %e, "collecting a signer certificate failed");
            }
        }
        let parsed = parse::parse_body(&unwrapped.raw);
        let env = parse::parse_body(&raw).envelope;
        let from = env.from.clone().or_else(|| from_of(&raw));
        let from_addr = from.as_ref().map(|f| f.address.clone()).unwrap_or_else(|| m.from_addr.clone());
        let trusted = self.mail_trusted_sender(user_id, &from_addr).await?;
        let settings = self.mail_security_settings(user_id).await?;
        let internal_list = self.mail_list_entries(user_id, List::Internal).await?;
        let own_addresses: Vec<String> = self.mail_accounts(user_id).await?.into_iter().map(|x| x.email.to_lowercase()).collect();
        let external = settings.internal_enabled
            && !internal_list.is_empty()
            && !own_addresses.contains(&from_addr.to_lowercase())
            && trust::matches(&internal_list, &from_addr).is_none();
        let keywords = match self.mail_keywords(&a, &m).await {
            Ok(k) => k,
            Err(e) => {
                tracing::debug!(error = %e, "reading message keywords failed");
                vec![]
            }
        };
        let contacts = match &from {
            Some(f) => self.contact_addresses_named(user_id, &f.name).await?,
            None => vec![],
        };
        let html = parsed.html.as_deref().map(|h| parse::sanitize_html(h, false, |c| c.to_string()).html);
        let phishing = phishing::check(&phishing::Input {
            from: from.as_ref(),
            reply_to: &env.reply_to,
            date: env.date,
            now: now(),
            html: html.as_deref(),
            keywords: &keywords,
            contact_addresses: &contacts,
        });
        let dkim = self.mail_dkim(user_id, id, &raw, trust::domain_of(&from_addr).as_deref()).await?;
        let pgp = detect_pgp(&unwrapped.raw, &parsed.text);
        Ok(SecurityReport {
            message_id: m.id,
            sender: from_addr,
            trusted_sender: trusted.map(|e| TrustedBy { kind: e.kind, value: e.value }),
            external,
            phishing,
            dkim,
            smime: unwrapped.smime,
            pgp,
            keywords,
        })
    }

    /// The raw message as text for "view source", cut at [`SOURCE_CAP`].
    pub async fn mail_source(&self, user_id: &str, id: &str) -> Result<(String, bool)> {
        let (_, raw) = self.mail_raw(user_id, id).await?;
        let cut = raw.len() > SOURCE_CAP;
        let mut end = raw.len().min(SOURCE_CAP);
        while end > 0 && end < raw.len() && (raw[end] & 0xc0) == 0x80 {
            end -= 1;
        }
        Ok((String::from_utf8_lossy(&raw[..end]).into_owned(), cut))
    }

    /// The non-inline attachments of a message for a ZIP: (unique file name, bytes), at most
    /// [`ZIP_CAP`] in total.
    pub async fn mail_attachments_for_zip(&self, user_id: &str, id: &str) -> Result<(super::store::MessageSummary, Vec<(String, Vec<u8>)>)> {
        let (m, raw) = self.mail_readable_raw(user_id, id).await?;
        let parsed = parse::parse_body(&raw);
        let mut total = 0usize;
        let mut names: Vec<String> = Vec::new();
        let mut out = Vec::new();
        for a in parsed.attachments.iter().filter(|a| !a.inline) {
            let Some((name, _, data)) = parse::attachment(&raw, a.index) else { continue };
            total += data.len();
            if total > ZIP_CAP {
                return Err(Error::bad(format!(
                    "the attachments are larger than {} MB; download them one by one",
                    ZIP_CAP / 1024 / 1024
                )));
            }
            let name = zip_name(&name, &names);
            names.push(name.clone());
            out.push((name, data));
        }
        if out.is_empty() {
            return Err(Error::bad("this message has no attachments"));
        }
        Ok((m, out))
    }

    /// Signs and/or encrypts an outgoing message for sending as `from` from `account_id`, or
    /// wraps Mailvelope's armored text as PGP/MIME. `wire` is the message as it goes over SMTP
    /// (without Bcc), `copy` the Sent copy (with Bcc). Returns them transformed; recipients
    /// must all have certificates when encrypting (else a clear error, nothing is sent).
    pub async fn mail_secure_outgoing(
        &self,
        user_id: &str,
        account_id: &str,
        opts: &OutgoingSecurity,
        recipients: &[String],
        wire: &[u8],
        copy: &[u8],
    ) -> Result<(Vec<u8>, Vec<u8>)> {
        if opts.pgp_mime {
            if opts.smime_sign || opts.smime_encrypt {
                return Err(Error::bad("choose either S/MIME or PGP, not both"));
            }
            return Ok((pgp_mime(wire)?, pgp_mime(copy)?));
        }
        if !opts.smime_sign && !opts.smime_encrypt {
            return Ok((wire.to_vec(), copy.to_vec()));
        }
        let from = from_of(wire).ok_or_else(|| Error::bad("the message has no sender"))?;
        let mut entity = smime::entity_of(wire);
        if opts.smime_sign {
            let id = self.smime_signing_identity(user_id, account_id, &from.address).await?.ok_or_else(|| {
                Error::bad(format!(
                    "there is no S/MIME certificate for {}; import one in Mail settings or turn signing off",
                    from.address
                ))
            })?;
            let micalg = smime::micalg(&smime::digest_for(id.key.kind));
            let sig = smime::sign_detached(&entity, &id, now()).map_err(Error::bad)?;
            entity = smime::signed_entity(&entity, &sig, micalg);
        }
        if opts.smime_encrypt {
            let mut certs = Vec::new();
            let mut missing = Vec::new();
            for r in recipients {
                let Some(addr) = Addr::parse(r) else { continue };
                match self.smime_recipient_cert(user_id, &addr.address).await? {
                    Some((c, _)) => certs.push(c),
                    None => missing.push(addr.address),
                }
            }
            if !missing.is_empty() {
                return Err(Error::bad(format!(
                    "no S/MIME certificate for {}: they need to send you signed mail first, or import their certificate; or send without encryption",
                    missing.join(", ")
                )));
            }
            // The sender can read the Sent copy too.
            match self.smime_signing_identity(user_id, account_id, &from.address).await? {
                Some(me) => certs.push(me.cert),
                None => {
                    if let Some((c, _)) = self.smime_recipient_cert(user_id, &from.address).await? {
                        certs.push(c);
                    }
                }
            }
            certs.sort();
            certs.dedup();
            let der = smime::encrypt(&entity, &certs).map_err(Error::bad)?;
            entity = smime::enveloped_entity(&der);
        }
        Ok((smime::with_entity(wire, &entity), smime::with_entity(copy, &entity)))
    }
}

/// Wraps a message whose text body is Mailvelope's armored PGP message as PGP/MIME
/// (RFC 3156). Attachments cannot go along unencrypted.
fn pgp_mime(raw: &[u8]) -> Result<Vec<u8>> {
    let parsed = parse::parse_body(raw);
    if parsed.attachments.iter().any(|a| !a.inline) {
        return Err(Error::bad(
            "attachments cannot be encrypted with Mailvelope; remove them or send them separately",
        ));
    }
    let armored = armor_block(&parsed.text, "PGP MESSAGE").ok_or_else(|| Error::bad("the message is not encrypted yet (no PGP MESSAGE block)"))?;
    let mut b = [0u8; 12];
    rand::fill(&mut b);
    let boundary = format!("----=_pgp_{}", hex::encode(b));
    let entity = format!(
        "Content-Type: multipart/encrypted; protocol=\"application/pgp-encrypted\"; boundary=\"{boundary}\"\r\n\r\nThis is an OpenPGP/MIME encrypted message (RFC 4880 and 3156)\r\n--{boundary}\r\nContent-Type: application/pgp-encrypted\r\nContent-Description: PGP/MIME version identification\r\n\r\nVersion: 1\r\n\r\n--{boundary}\r\nContent-Type: application/octet-stream; name=\"encrypted.asc\"\r\nContent-Description: OpenPGP encrypted message\r\nContent-Disposition: inline; filename=\"encrypted.asc\"\r\n\r\n{}\r\n\r\n--{boundary}--\r\n",
        armored.replace('\n', "\r\n")
    );
    Ok(smime::with_entity(raw, entity.as_bytes()))
}

/// A file name not yet in `taken` ("a.pdf", "a (2).pdf", …), safe inside a ZIP.
fn zip_name(name: &str, taken: &[String]) -> String {
    let clean: String = name
        .chars()
        .map(|c| if c.is_control() || "/\\:*?\"<>|".contains(c) { '_' } else { c })
        .collect::<String>()
        .trim_start_matches('.')
        .trim()
        .chars()
        .take(150)
        .collect();
    let clean = if clean.is_empty() { "attachment".to_string() } else { clean };
    if !taken.iter().any(|t| t.eq_ignore_ascii_case(&clean)) {
        return clean;
    }
    let (stem, ext) = match clean.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (clean.clone(), String::new()),
    };
    (2..)
        .map(|n| format!("{stem} ({n}){ext}"))
        .find(|c| !taken.iter().any(|t| t.eq_ignore_ascii_case(c)))
        .expect("a free name exists")
}

/// What the composer asked for.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutgoingSecurity {
    #[serde(default)]
    pub smime_sign: bool,
    #[serde(default)]
    pub smime_encrypt: bool,
    /// The text is Mailvelope's armored PGP message: send it as PGP/MIME.
    #[serde(default)]
    pub pgp_mime: bool,
}

impl OutgoingSecurity {
    pub fn any(&self) -> bool {
        self.smime_sign || self.smime_encrypt || self.pgp_mime
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TrustedBy {
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SecurityReport {
    pub message_id: String,
    pub sender: String,
    /// Remote images load without asking (the sender or their domain is trusted).
    pub trusted_sender: Option<TrustedBy>,
    /// The sender is outside the user's internal addresses (when that check is on).
    pub external: bool,
    pub phishing: Vec<phishing::Finding>,
    pub dkim: dkim::Verdict,
    pub smime: Option<SmimeView>,
    pub pgp: Option<PgpView>,
    pub keywords: Vec<String>,
}

// ---- ZIP -------------------------------------------------------------------------------------

/// A ZIP writer for stored (uncompressed) entries, streaming each entry as it is added.
/// Attachments are mostly compressed already; storing keeps it fast and simple. No ZIP64:
/// the archive stays under [`ZIP_CAP`].
#[derive(Default)]
pub struct ZipWriter {
    offset: u32,
    central: Vec<u8>,
    count: u16,
}

impl ZipWriter {
    /// The bytes of one entry (local header and data).
    pub fn entry(&mut self, name: &str, data: &[u8], modified: DateTime<Utc>) -> Vec<u8> {
        let crc = crc32fast::hash(data);
        let (time, date) = dos_time(modified);
        let name_b = name.as_bytes();
        let size = data.len() as u32;
        let mut local = Vec::with_capacity(30 + name_b.len() + data.len());
        local.extend_from_slice(&0x04034b50u32.to_le_bytes());
        local.extend_from_slice(&20u16.to_le_bytes()); // version needed
        local.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 names
        local.extend_from_slice(&0u16.to_le_bytes()); // stored
        local.extend_from_slice(&time.to_le_bytes());
        local.extend_from_slice(&date.to_le_bytes());
        local.extend_from_slice(&crc.to_le_bytes());
        local.extend_from_slice(&size.to_le_bytes());
        local.extend_from_slice(&size.to_le_bytes());
        local.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(name_b);
        local.extend_from_slice(data);
        let c = &mut self.central;
        c.extend_from_slice(&0x02014b50u32.to_le_bytes());
        c.extend_from_slice(&0x031eu16.to_le_bytes()); // made by: Unix, 3.0
        c.extend_from_slice(&20u16.to_le_bytes());
        c.extend_from_slice(&0x0800u16.to_le_bytes());
        c.extend_from_slice(&0u16.to_le_bytes());
        c.extend_from_slice(&time.to_le_bytes());
        c.extend_from_slice(&date.to_le_bytes());
        c.extend_from_slice(&crc.to_le_bytes());
        c.extend_from_slice(&size.to_le_bytes());
        c.extend_from_slice(&size.to_le_bytes());
        c.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
        c.extend_from_slice(&[0; 8]); // extra, comment, disk start, internal attributes
        c.extend_from_slice(&(0o100644u32 << 16).to_le_bytes()); // external: regular file rw-r--r--
        c.extend_from_slice(&self.offset.to_le_bytes());
        c.extend_from_slice(name_b);
        self.offset += local.len() as u32;
        self.count += 1;
        local
    }

    /// The central directory and its end record.
    pub fn finish(self) -> Vec<u8> {
        let mut out = self.central.clone();
        out.extend_from_slice(&0x06054b50u32.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&self.count.to_le_bytes());
        out.extend_from_slice(&self.count.to_le_bytes());
        out.extend_from_slice(&(self.central.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }
}

fn dos_time(t: DateTime<Utc>) -> (u16, u16) {
    use chrono::{Datelike, Timelike};
    let year = (t.year().clamp(1980, 2107) - 1980) as u16;
    let date = (year << 9) | ((t.month() as u16) << 5) | t.day() as u16;
    let time = ((t.hour() as u16) << 11) | ((t.minute() as u16) << 5) | (t.second() as u16 / 2);
    (time, date)
}

// ---- image proxy URLs ------------------------------------------------------------------------

/// How long a proxied image URL in a rendered message works.
pub const PROXY_URL_TTL: chrono::Duration = chrono::Duration::hours(12);
/// Remote URLs longer than this are not proxied.
pub const PROXY_MAX_URL: usize = 4096;

impl Core {
    fn proxy_key(&self) -> aws_lc_rs::hmac::Key {
        use aws_lc_rs::hkdf;
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, b"ferrite mail").extract(self.secret.as_bytes());
        let info = [b"image proxy url v1".as_slice()];
        let mut key = [0u8; 32];
        prk.expand(&info, hkdf::HKDF_SHA256)
            .and_then(|okm| okm.fill(&mut key))
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        aws_lc_rs::hmac::Key::new(aws_lc_rs::hmac::HMAC_SHA256, &key)
    }

    fn proxy_tag(&self, user_id: &str, url: &str, expires: i64) -> aws_lc_rs::hmac::Tag {
        let msg = format!("{user_id}\0{expires}\0{url}");
        aws_lc_rs::hmac::sign(&self.proxy_key(), msg.as_bytes())
    }

    /// A same-origin URL that loads `url` through the image proxy, for `user_id` only, until
    /// [`PROXY_URL_TTL`] from now. None for anything but http(s) URLs of sane length.
    pub fn mail_proxy_url(&self, user_id: &str, url: &str) -> Option<String> {
        use base64::Engine;
        let lower = url.trim().to_ascii_lowercase();
        if url.len() > PROXY_MAX_URL || !(lower.starts_with("http://") || lower.starts_with("https://")) {
            return None;
        }
        let expires = (now() + PROXY_URL_TTL).timestamp();
        let tag = self.proxy_tag(user_id, url.trim(), expires);
        Some(format!(
            "/api/mail/proxy?u={}&e={expires}&s={}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(url.trim()),
            hex::encode(tag.as_ref())
        ))
    }

    /// Checks a proxy request's parameters; returns the remote URL when they were made by
    /// [`Core::mail_proxy_url`] for this user and have not expired.
    pub fn mail_proxy_check(&self, user_id: &str, u: &str, e: i64, s: &str) -> Result<String> {
        use base64::Engine;
        if e < now().timestamp() {
            return Err(Error::Forbidden);
        }
        let url = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(u)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .ok_or(Error::Forbidden)?;
        let tag = hex::decode(s).map_err(|_| Error::Forbidden)?;
        let msg = format!("{user_id}\0{e}\0{url}");
        aws_lc_rs::hmac::verify(&self.proxy_key(), msg.as_bytes(), &tag).map_err(|_| Error::Forbidden)?;
        Ok(url)
    }
}

// ---- avatars ---------------------------------------------------------------------------------

/// A cached network picture (or a cached miss when `data` is None).
#[derive(Debug, Clone)]
pub struct CachedAvatar {
    pub mime: Option<String>,
    pub data: Option<Vec<u8>>,
    pub fetched_at: DateTime<Utc>,
}

impl Core {
    /// The photo of the user's contact with this address: inline (`PHOTO;ENCODING=b`) or a
    /// `data:` URI. Remote photo URLs are not fetched.
    pub async fn mail_contact_photo(&self, user_id: &str, email: &str) -> Result<Option<(String, Vec<u8>)>> {
        use base64::Engine;
        let email = email.trim().to_lowercase();
        for c in self.dav_collections(user_id, crate::groupware::KIND_ADDRESSBOOK).await? {
            for o in self.dav_objects(&c).await? {
                if !o.data.to_lowercase().contains(&email) {
                    continue;
                }
                let lines = crate::groupware::unfold(&o.data);
                let has_email = lines.iter().any(|l| {
                    let (name, _, value) = crate::groupware::split_prop(l);
                    name == "EMAIL" && value.trim().trim_start_matches("mailto:").eq_ignore_ascii_case(&email)
                });
                if !has_email {
                    continue;
                }
                for l in &lines {
                    let (name, params, value) = crate::groupware::split_prop(l);
                    if name != "PHOTO" {
                        continue;
                    }
                    let value = value.trim();
                    let (mime, b64) = if let Some(rest) = value.strip_prefix("data:") {
                        let Some((meta, data)) = rest.split_once(',') else { continue };
                        if !meta.ends_with(";base64") {
                            continue;
                        }
                        (meta.trim_end_matches(";base64").to_string(), data.to_string())
                    } else if params
                        .iter()
                        .any(|(k, v)| k == "ENCODING" && (v.eq_ignore_ascii_case("b") || v.eq_ignore_ascii_case("base64")))
                    {
                        let ty = params
                            .iter()
                            .find(|(k, _)| k == "TYPE")
                            .map(|(_, v)| v.to_ascii_lowercase())
                            .unwrap_or_else(|| "jpeg".into());
                        (format!("image/{}", ty.trim_start_matches("image/")), value.to_string())
                    } else {
                        continue;
                    };
                    if !super::security::is_safe_image_mime(&mime) {
                        continue;
                    }
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64.split_whitespace().collect::<String>()) {
                        if !bytes.is_empty() && bytes.len() <= 512 * 1024 {
                            return Ok(Some((mime, bytes)));
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    pub async fn mail_avatar_cache_get(&self, key: &str) -> Result<Option<CachedAvatar>> {
        type Row = (Option<String>, Option<Vec<u8>>, DateTime<Utc>);
        let row: Option<Row> = sqlx::query_as("SELECT mime, data, fetched_at FROM mail_avatar_cache WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.db)
            .await?;
        Ok(row.map(|(mime, data, fetched_at)| CachedAvatar { mime, data, fetched_at }))
    }

    pub async fn mail_avatar_cache_put(&self, key: &str, found: Option<(&str, &[u8])>) -> Result<()> {
        sqlx::query(
            "INSERT INTO mail_avatar_cache (key, mime, data, fetched_at) VALUES (?, ?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET mime = excluded.mime, data = excluded.data, fetched_at = excluded.fetched_at",
        )
        .bind(key)
        .bind(found.map(|f| f.0))
        .bind(found.map(|f| f.1))
        .bind(now())
        .execute(&self.db)
        .await?;
        Ok(())
    }
}

/// Raster image types shown as avatars and through the image proxy. SVG is excluded from
/// avatars (it is a document type).
pub fn is_safe_image_mime(mime: &str) -> bool {
    matches!(
        mime.trim().to_ascii_lowercase().as_str(),
        "image/png" | "image/jpeg" | "image/jpg" | "image/gif" | "image/webp" | "image/avif" | "image/bmp" | "image/x-icon" | "image/vnd.microsoft.icon"
    )
}

/// The image type of `data` from its first bytes (what is served, whatever the server said).
pub fn sniff_image(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if data.len() > 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else if data.len() > 12 && &data[4..8] == b"ftyp" && (&data[8..12] == b"avif" || &data[8..12] == b"avis") {
        Some("image/avif")
    } else if data.starts_with(b"BM") && data.len() > 26 {
        Some("image/bmp")
    } else if data.starts_with(&[0, 0, 1, 0]) || data.starts_with(&[0, 0, 2, 0]) {
        Some("image/x-icon")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_pgp() {
        let pgp_mime = b"Content-Type: multipart/encrypted; protocol=\"application/pgp-encrypted\"; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: application/pgp-encrypted\r\n\r\nVersion: 1\r\n--b\r\nContent-Type: application/octet-stream\r\n\r\n-----BEGIN PGP MESSAGE-----\r\n\r\nwcBMA0x\r\n-----END PGP MESSAGE-----\r\n--b--\r\n";
        let v = detect_pgp(pgp_mime, "").unwrap();
        assert_eq!(v.kind, PgpKind::PgpMime);
        assert_eq!(v.armored.as_deref(), Some("-----BEGIN PGP MESSAGE-----\n\nwcBMA0x\n-----END PGP MESSAGE-----"));
        let v = detect_pgp(
            b"Content-Type: text/plain\r\n\r\n",
            "hi\n-----BEGIN PGP MESSAGE-----\nabc\n-----END PGP MESSAGE-----\nbye",
        )
        .unwrap();
        assert_eq!(v.kind, PgpKind::Inline);
        let v = detect_pgp(
            b"Content-Type: text/plain\r\n\r\n",
            "-----BEGIN PGP SIGNED MESSAGE-----\nHash: SHA256\n\nhello\n-----BEGIN PGP SIGNATURE-----\nx\n-----END PGP SIGNATURE-----",
        )
        .unwrap();
        assert_eq!(v.kind, PgpKind::InlineSigned);
        assert!(
            detect_pgp(
                b"Content-Type: multipart/signed; protocol=\"application/pgp-signature\"; boundary=b\r\n\r\n",
                ""
            )
            .is_some_and(|v| v.kind == PgpKind::PgpMimeSigned)
        );
        assert!(detect_pgp(b"Content-Type: text/plain\r\n\r\n", "BEGIN PGP MESSAGE without armor").is_none());
        // An unterminated block is not handed to Mailvelope.
        assert!(detect_pgp(b"Content-Type: text/plain\r\n\r\n", "-----BEGIN PGP MESSAGE-----\nabc").is_none());
    }

    #[test]
    fn wraps_pgp_mime() {
        let raw = b"From: a@example.org\r\nTo: b@example.org\r\nSubject: s\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n-----BEGIN PGP MESSAGE-----\r\n\r\nwcBMA0x\r\n-----END PGP MESSAGE-----\r\n";
        let out = String::from_utf8(pgp_mime(raw).unwrap()).unwrap();
        assert!(out.starts_with("From: a@example.org\r\nTo: b@example.org\r\nSubject: s\r\nMIME-Version: 1.0\r\nContent-Type: multipart/encrypted; protocol=\"application/pgp-encrypted\""), "{out}");
        assert!(out.contains("Version: 1") && out.contains("-----BEGIN PGP MESSAGE-----\r\n\r\nwcBMA0x"));
        assert_eq!(detect_pgp(out.as_bytes(), "").unwrap().kind, PgpKind::PgpMime);
        assert!(pgp_mime(b"From: a@example.org\r\nContent-Type: text/plain\r\n\r\nplain text\r\n").is_err());
    }

    #[test]
    fn zip_is_readable() {
        let mut z = ZipWriter::default();
        let mut bytes = z.entry("a.txt", b"hello", Utc::now());
        bytes.extend(z.entry("dir_b.bin", &[0u8, 1, 2, 3], Utc::now()));
        bytes.extend(z.finish());
        // End of central directory: 2 entries, central directory right after the data.
        let eocd = &bytes[bytes.len() - 22..];
        assert_eq!(&eocd[..4], &0x06054b50u32.to_le_bytes());
        assert_eq!(u16::from_le_bytes([eocd[10], eocd[11]]), 2);
        let cd_offset = u32::from_le_bytes(eocd[16..20].try_into().unwrap()) as usize;
        assert_eq!(&bytes[cd_offset..cd_offset + 4], &0x02014b50u32.to_le_bytes());
        assert_eq!(&bytes[..4], &0x04034b50u32.to_le_bytes());
        // When unzip is around, it must accept the archive and its CRCs.
        if std::process::Command::new("unzip").arg("-v").output().is_ok() {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("t.zip");
            std::fs::write(&p, &bytes).unwrap();
            let out = std::process::Command::new("unzip").arg("-t").arg(&p).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
        }
    }

    #[test]
    fn zip_names_are_unique_and_safe() {
        let taken = vec!["a.pdf".to_string()];
        assert_eq!(zip_name("a.pdf", &taken), "a (2).pdf");
        assert_eq!(zip_name("A.PDF", &taken), "A (2).PDF");
        assert_eq!(zip_name("../../etc/passwd", &[]), "_.._etc_passwd");
        assert_eq!(zip_name("C:\\x\\y.txt", &[]), "C__x_y.txt");
        assert_eq!(zip_name("..", &[]), "attachment");
        assert_eq!(zip_name("a\u{0}b", &[]), "a_b");
    }

    #[test]
    fn image_sniffing() {
        assert_eq!(sniff_image(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert_eq!(sniff_image(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff_image(b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>"), None);
        assert_eq!(sniff_image(b"<html>"), None);
        assert!(is_safe_image_mime("image/PNG") && !is_safe_image_mime("image/svg+xml") && !is_safe_image_mime("text/html"));
    }
}
