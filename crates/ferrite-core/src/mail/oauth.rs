//! Google and Microsoft accounts with OAuth 2.0: IMAP and SMTP sign in with SASL XOAUTH2
//! instead of a password.
//!
//! The administrator registers Ferrite with the provider (client id and secret; for
//! Microsoft also the tenant, `common` by default) and gives it the redirect URI
//! `<public_url>/api/mail/oauth/callback`. A user then connects an account:
//!
//! 1. [`Core::mail_oauth_start`] stores a request (state, PKCE verifier) bound to the user's
//!    browser session and returns the provider's consent URL (Authorization Code + PKCE S256).
//! 2. The provider redirects back; [`Core::mail_oauth_finish`] consumes the state once, checks
//!    the session, exchanges the code (with the verifier), tests IMAP and SMTP with the access
//!    token, and creates the account (or renews the tokens of the one being reconnected).
//!
//! Tokens are sealed at rest (the access token in `password`, the refresh token in
//! `oauth_refresh`). [`Core::mail_fresh_credentials`] refreshes the access token shortly
//! before it expires, once per account at a time.

use super::delegation::MailRight;
use super::settings::OAuthClient;
use super::{Account, NewAccount};
use crate::mailer::SmtpSecurity;
use crate::util::{now, random_token, sha256_hex};
use crate::{Core, Error, Result};
use base64::Engine;
use chrono::Duration as ChronoDuration;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Access tokens closer than this to their expiry are refreshed before use.
pub const REFRESH_MARGIN_SECS: i64 = 300;
/// An authorization request lives this long.
pub const REQUEST_TTL_SECS: i64 = 600;
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// The SASL XOAUTH2 initial client response (before base64):
/// `user=<user>^Aauth=Bearer <token>^A^A`.
pub fn xoauth2(user: &str, token: &str) -> String {
    format!("user={user}\x01auth=Bearer {token}\x01\x01")
}

/// PKCE S256 (RFC 7636 §4.2): BASE64URL(SHA256(ASCII(code_verifier))).
pub fn pkce_challenge(verifier: &str) -> String {
    use sha2::Digest;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()))
}

/// `application/x-www-form-urlencoded` encoding of one value.
pub fn form(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn form_body(pairs: &[(&str, &str)]) -> String {
    pairs.iter().map(|(k, v)| format!("{}={}", form(k), form(v))).collect::<Vec<_>>().join("&")
}

/// A provider with the administrator's configuration applied over its public defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    pub id: &'static str,
    pub label: &'static str,
    pub client_id: String,
    pub auth_url: String,
    pub token_url: String,
    pub scope: &'static str,
    /// Extra authorization parameters (offline access, consent prompt).
    pub extra: &'static str,
    pub imap: (String, u16, SmtpSecurity),
    pub smtp: (String, u16, SmtpSecurity),
}

fn security(s: &str, default: SmtpSecurity) -> SmtpSecurity {
    match s {
        "tls" => SmtpSecurity::Tls,
        "starttls" => SmtpSecurity::Starttls,
        "none" => SmtpSecurity::None,
        _ => default,
    }
}

fn or(v: &str, default: &str) -> String {
    if v.trim().is_empty() { default.to_string() } else { v.trim().to_string() }
}

impl Provider {
    /// `google` or `microsoft` with `c` applied; `None` for other ids.
    pub fn resolve(id: &str, c: &OAuthClient) -> Option<Provider> {
        let port = |p: u16, d: u16| if p == 0 { d } else { p };
        match id {
            "google" => Some(Provider {
                id: "google",
                label: "Google",
                client_id: c.client_id.clone(),
                auth_url: or(&c.auth_url, "https://accounts.google.com/o/oauth2/v2/auth"),
                token_url: or(&c.token_url, "https://oauth2.googleapis.com/token"),
                scope: "https://mail.google.com/",
                extra: "access_type=offline&prompt=consent",
                imap: (
                    or(&c.imap_host, "imap.gmail.com"),
                    port(c.imap_port, 993),
                    security(&c.imap_security, SmtpSecurity::Tls),
                ),
                smtp: (
                    or(&c.smtp_host, "smtp.gmail.com"),
                    port(c.smtp_port, 465),
                    security(&c.smtp_security, SmtpSecurity::Tls),
                ),
            }),
            "microsoft" => {
                let tenant = or(&c.tenant, "common");
                Some(Provider {
                    id: "microsoft",
                    label: "Microsoft",
                    client_id: c.client_id.clone(),
                    auth_url: or(&c.auth_url, &format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/authorize")),
                    token_url: or(&c.token_url, &format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token")),
                    scope: "offline_access https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send",
                    extra: "response_mode=query&prompt=select_account",
                    imap: (
                        or(&c.imap_host, "outlook.office365.com"),
                        port(c.imap_port, 993),
                        security(&c.imap_security, SmtpSecurity::Tls),
                    ),
                    smtp: (
                        or(&c.smtp_host, "smtp.office365.com"),
                        port(c.smtp_port, 587),
                        security(&c.smtp_security, SmtpSecurity::Starttls),
                    ),
                })
            }
            _ => None,
        }
    }

    /// The consent URL for one request.
    pub fn authorize_url(&self, redirect_uri: &str, state: &str, verifier: &str, login_hint: &str) -> String {
        format!(
            "{}{}response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256&login_hint={}&{}",
            self.auth_url,
            if self.auth_url.contains('?') { '&' } else { '?' },
            form(&self.client_id),
            form(redirect_uri),
            form(self.scope),
            form(state),
            form(&pkce_challenge(verifier)),
            form(login_hint),
            self.extra,
        )
    }
}

/// A provider in the account dialog.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderStatus {
    pub id: &'static str,
    pub label: &'static str,
    pub configured: bool,
}

/// What the browser asks to start.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OAuthStart {
    pub provider: String,
    pub email: String,
    #[serde(default)]
    pub name: String,
    /// Reconnect this account (renew its tokens) instead of adding one.
    pub account_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct RequestRow {
    user_id: String,
    session_id: String,
    provider: String,
    verifier: String,
    email: String,
    name: String,
    account_id: Option<String>,
    expires_at: chrono::DateTime<chrono::Utc>,
}

impl Core {
    /// The redirect URI to register with the providers.
    pub fn mail_oauth_redirect_uri(&self) -> String {
        format!("{}/api/mail/oauth/callback", self.config.public_url.trim_end_matches('/'))
    }

    /// Which providers the administrator configured.
    pub async fn mail_oauth_providers(&self) -> Vec<ProviderStatus> {
        let s = self.mail_settings().await;
        [("google", "Google", &s.oauth.google), ("microsoft", "Microsoft", &s.oauth.microsoft)]
            .into_iter()
            .map(|(id, label, c)| ProviderStatus {
                id,
                label,
                configured: !c.client_id.is_empty(),
            })
            .collect()
    }

    async fn mail_oauth_provider(&self, id: &str) -> Result<Provider> {
        let s = self.mail_settings().await;
        let c = match id {
            "google" => &s.oauth.google,
            "microsoft" => &s.oauth.microsoft,
            _ => return Err(Error::bad(format!("unknown OAuth provider {id:?}"))),
        };
        let p = Provider::resolve(id, c).ok_or(Error::NotFound)?;
        if p.client_id.is_empty() {
            return Err(Error::bad(format!(
                "{} accounts are not set up on this server; an administrator can add the client in Settings → Administration → Mail",
                p.label
            )));
        }
        Ok(p)
    }

    pub(crate) fn mail_http(&self, allow_local: bool) -> Result<reqwest::Client> {
        let cell = if allow_local { &self.mail.http_local } else { &self.mail.http_public };
        if let Some(c) = cell.get() {
            return Ok(c.clone());
        }
        let mut b = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("Ferrite/", env!("CARGO_PKG_VERSION")));
        if !allow_local {
            b = b.dns_resolver(std::sync::Arc::new(crate::net::PublicOnlyResolver));
        }
        let client = b.build().map_err(Error::internal)?;
        Ok(cell.get_or_init(|| client).clone())
    }

    /// Starts connecting an account: returns the provider's consent URL. `session_id` is the
    /// id of the caller's browser session; the callback must come back in the same one.
    pub async fn mail_oauth_start(&self, user_id: &str, session_id: &str, req: &OAuthStart) -> Result<String> {
        let p = self.mail_oauth_provider(&req.provider).await?;
        let email = req.email.trim().to_string();
        if super::parse::Addr::parse(&email).is_none_or(|a| !a.name.is_empty()) {
            return Err(Error::bad(format!("{email:?} is not an email address")));
        }
        match &req.account_id {
            Some(id) => {
                let a = self.mail_access(user_id, id, MailRight::Authenticate).await?;
                if a.oauth_provider.as_deref() != Some(p.id) {
                    return Err(Error::bad(format!("{} does not sign in with {}", a.email, p.label)));
                }
            }
            None => {
                if !self.mail_allow_user_accounts().await {
                    return Err(Error::bad("adding mail accounts is turned off on this server"));
                }
            }
        }
        let state = random_token(24);
        let verifier = random_token(48);
        sqlx::query("DELETE FROM mail_oauth_requests WHERE expires_at < ?")
            .bind(now())
            .execute(&self.db)
            .await?;
        sqlx::query(
            "INSERT INTO mail_oauth_requests (state_hash, user_id, session_id, provider, verifier, email, name, account_id, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(sha256_hex(state.as_bytes()))
        .bind(user_id)
        .bind(session_id)
        .bind(p.id)
        .bind(&verifier)
        .bind(&email)
        .bind(req.name.trim())
        .bind(&req.account_id)
        .bind(now() + ChronoDuration::seconds(REQUEST_TTL_SECS))
        .execute(&self.db)
        .await?;
        Ok(p.authorize_url(&self.mail_oauth_redirect_uri(), &state, &verifier, &email))
    }

    /// Finishes connecting: the provider redirected back with `code` for `state`. Returns the
    /// new or reconnected account.
    pub async fn mail_oauth_finish(&self, user_id: &str, session_id: &str, state: &str, code: &str) -> Result<Account> {
        let expired = || Error::bad("this sign-in has expired or was already used; please start again");
        let row: RequestRow = sqlx::query_as(
            "DELETE FROM mail_oauth_requests WHERE state_hash = ? RETURNING user_id, session_id, provider, verifier, email, name, account_id, expires_at",
        )
        .bind(sha256_hex(state.as_bytes()))
        .fetch_optional(&self.db)
        .await?
        .ok_or_else(expired)?;
        if row.expires_at < now() {
            return Err(expired());
        }
        if row.user_id != user_id || row.session_id != session_id {
            return Err(Error::bad("this sign-in was started in another browser session; please start again"));
        }
        let p = self.mail_oauth_provider(&row.provider).await?;
        let tokens = self
            .mail_token_request(
                &p,
                &[
                    ("grant_type", "authorization_code"),
                    ("code", code),
                    ("redirect_uri", &self.mail_oauth_redirect_uri()),
                    ("code_verifier", &row.verifier),
                ],
            )
            .await?;
        let refresh = tokens
            .refresh_token
            .clone()
            .ok_or_else(|| Error::bad(format!("{} did not grant offline access (no refresh token)", p.label)))?;
        let expires_at = now() + ChronoDuration::seconds(tokens.expires_in.unwrap_or(3600).max(60));
        // Prove the token works for both protocols before anything is stored.
        let imap = super::ServerSettings {
            host: p.imap.0.clone(),
            port: p.imap.1,
            security: p.imap.2,
            username: row.email.clone(),
            password: tokens.access_token.clone(),
            xoauth2: true,
        };
        let smtp = crate::mailer::SmtpSettings {
            host: p.smtp.0.clone(),
            port: p.smtp.1,
            security: p.smtp.2,
            username: Some(row.email.clone()),
            password: Some(tokens.access_token.clone()),
            from: row.email.clone(),
        };
        self.test_mail_connection(&imap, &smtp, true).await?;
        let sb = self.secret_box();
        if let Some(id) = &row.account_id {
            let a = self.mail_access(user_id, id, MailRight::Authenticate).await?;
            sqlx::query(
                "UPDATE mail_accounts SET password = ?, oauth_refresh = ?, oauth_expires_at = ?, imap_username = ?, smtp_username = ?, last_error = NULL WHERE id = ?",
            )
            .bind(sb.seal(&a.id, &tokens.access_token)?)
            .bind(sb.seal(&a.id, &refresh)?)
            .bind(expires_at)
            .bind(&row.email)
            .bind(&row.email)
            .bind(&a.id)
            .execute(&self.db)
            .await?;
            self.mail.forget(&a.id);
            self.mail_publish(&a, &serde_json::json!({"type": "mail.changed", "account_id": a.id, "what": "accounts"}));
            self.ensure_mail_idle(user_id).await;
            return self.mail_access(user_id, &a.id, MailRight::Read).await;
        }
        if !self.mail_allow_user_accounts().await {
            return Err(Error::bad("adding mail accounts is turned off on this server"));
        }
        let new = NewAccount {
            user_id: user_id.to_string(),
            name: if row.name.is_empty() {
                self.user(user_id).await?.display_name
            } else {
                row.name.clone()
            },
            email: row.email.clone(),
            imap_host: p.imap.0.clone(),
            imap_port: p.imap.1,
            imap_security: p.imap.2,
            imap_username: row.email.clone(),
            smtp_host: p.smtp.0.clone(),
            smtp_port: p.smtp.1,
            smtp_security: p.smtp.2,
            smtp_username: row.email.clone(),
            password: tokens.access_token.clone(),
            smtp_password: None,
            // Both providers file sent mail themselves.
            save_sent: false,
            signature: String::new(),
            auth_method: "xoauth2",
            oauth_provider: Some(p.id.to_string()),
            oauth_refresh: Some(refresh),
            oauth_expires_at: Some(expires_at),
            provisioning_id: None,
        };
        let id = self.insert_mail_account(&new).await?;
        tracing::info!(account = %id, provider = p.id, "mail account connected with OAuth");
        self.after_account_added(user_id, &id).await
    }

    /// The account with a usable access token: refreshed (and stored) when it expires within
    /// [`REFRESH_MARGIN_SECS`]. Password accounts are returned as they are.
    pub async fn mail_fresh_credentials(&self, a: &Account) -> Result<Account> {
        if a.auth_method != "xoauth2" {
            return Ok(a.clone());
        }
        let soon = now() + ChronoDuration::seconds(REFRESH_MARGIN_SECS);
        if a.oauth_expires_at.is_some_and(|t| t > soon) {
            return Ok(a.clone());
        }
        let lock = self.mail.refresh_locks.entry(a.id.clone()).or_default().clone();
        let _guard = lock.lock().await;
        // Another request may have refreshed it while this one waited.
        let cur = self.mail_account_by_id(&a.id).await?;
        if cur.oauth_expires_at.is_some_and(|t| t > soon) {
            return Ok(cur);
        }
        let provider = cur.oauth_provider.clone().unwrap_or_default();
        let r = async {
            let p = self.mail_oauth_provider(&provider).await?;
            let sealed = cur
                .oauth_refresh
                .as_deref()
                .ok_or_else(|| Error::bad("the account has no refresh token; sign in again"))?;
            let refresh = self.secret_box().open(&cur.id, sealed)?;
            let tokens = self
                .mail_token_request(&p, &[("grant_type", "refresh_token"), ("refresh_token", &refresh)])
                .await?;
            let sb = self.secret_box();
            // Providers may rotate the refresh token.
            let next_refresh = match &tokens.refresh_token {
                Some(r) => sb.seal(&cur.id, r)?,
                None => sealed.to_string(),
            };
            let expires_at = now() + ChronoDuration::seconds(tokens.expires_in.unwrap_or(3600).max(60));
            sqlx::query("UPDATE mail_accounts SET password = ?, oauth_refresh = ?, oauth_expires_at = ? WHERE id = ?")
                .bind(sb.seal(&cur.id, &tokens.access_token)?)
                .bind(next_refresh)
                .bind(expires_at)
                .bind(&cur.id)
                .execute(&self.db)
                .await?;
            Ok::<_, Error>(())
        }
        .await;
        if let Err(e) = r {
            let msg = format!("signing in to {} failed: {e}; reconnect the account", cur.email);
            self.set_mail_error(&cur.id, Some(&msg)).await;
            return Err(Error::Unavailable(msg));
        }
        tracing::debug!(account = %cur.id, "OAuth access token refreshed");
        self.mail_account_by_id(&cur.id).await
    }

    async fn mail_token_request(&self, p: &Provider, grant: &[(&str, &str)]) -> Result<TokenResponse> {
        let allow_local = self.mail_settings().await.allow_local_servers;
        let url = crate::net::check_url(&p.token_url, allow_local)?;
        let secret = self.mail_oauth_secret(p.id).await?;
        let mut pairs: Vec<(&str, &str)> = grant.to_vec();
        pairs.push(("client_id", &p.client_id));
        if !secret.is_empty() {
            pairs.push(("client_secret", &secret));
        }
        if grant.iter().any(|(k, v)| *k == "grant_type" && *v == "refresh_token") {
            pairs.push(("scope", p.scope));
        }
        let resp = self
            .mail_http(allow_local)?
            .post(url)
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(form_body(&pairs))
            .send()
            .await
            .map_err(|e| Error::Unavailable(format!("cannot reach {}: {e}", p.label)))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            let why = v
                .get("error_description")
                .or_else(|| v.get("error"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("no details");
            return Err(Error::bad(format!("{} refused ({status}): {why}", p.label)));
        }
        serde_json::from_value(v).map_err(|_| Error::bad(format!("{} sent an unexpected token response", p.label)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xoauth2_matches_googles_example() {
        // https://developers.google.com/gmail/imap/xoauth2-protocol
        let s = xoauth2("someuser@example.com", "ya29.vF9dft4qmTc2Nvb3RlckBhdHRhdmlzdGEuY29tCg");
        assert_eq!(
            base64::engine::general_purpose::STANDARD.encode(s),
            "dXNlcj1zb21ldXNlckBleGFtcGxlLmNvbQFhdXRoPUJlYXJlciB5YTI5LnZGOWRmdDRxbVRjMk52YjNSbGNrQmhkSFJoZG1semRHRXVZMjl0Q2cBAQ=="
        );
    }

    #[test]
    fn pkce_rfc7636_appendix_b() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn providers_and_consent_urls() {
        let empty = OAuthClient {
            client_id: "cid".into(),
            ..Default::default()
        };
        let g = Provider::resolve("google", &empty).unwrap();
        assert_eq!(g.imap, ("imap.gmail.com".into(), 993, SmtpSecurity::Tls));
        assert_eq!(g.smtp, ("smtp.gmail.com".into(), 465, SmtpSecurity::Tls));
        let m = Provider::resolve("microsoft", &empty).unwrap();
        assert!(m.token_url.contains("/common/oauth2/v2.0/token"));
        assert_eq!(m.smtp, ("smtp.office365.com".into(), 587, SmtpSecurity::Starttls));
        let tenant = OAuthClient {
            client_id: "cid".into(),
            tenant: "contoso.onmicrosoft.com".into(),
            ..Default::default()
        };
        assert!(Provider::resolve("microsoft", &tenant).unwrap().auth_url.contains("/contoso.onmicrosoft.com/"));
        assert!(Provider::resolve("yahoo", &empty).is_none());
        let url = g.authorize_url("https://cloud.example/api/mail/oauth/callback", "st", "verifier", "a+b@example.org");
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?response_type=code&client_id=cid&"));
        assert!(url.contains("redirect_uri=https%3A%2F%2Fcloud.example%2Fapi%2Fmail%2Foauth%2Fcallback"));
        assert!(url.contains("scope=https%3A%2F%2Fmail.google.com%2F"));
        assert!(url.contains(&format!("code_challenge={}&code_challenge_method=S256", pkce_challenge("verifier"))));
        assert!(url.contains("login_hint=a%2Bb%40example.org"));
        assert!(url.contains("access_type=offline"));
        assert_eq!(form("a b&c=d/é"), "a+b%26c%3Dd%2F%C3%A9");
    }
}
