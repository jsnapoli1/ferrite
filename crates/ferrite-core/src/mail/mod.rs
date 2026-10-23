//! Webmail: users connect their existing IMAP/SMTP accounts.
//!
//! The IMAP server is the source of truth. Ferrite keeps a cache in SQLite: the folder list,
//! envelopes (synced incrementally by UID, reset when UIDVALIDITY changes) and bodies fetched
//! on demand under a per-account size cap. Cached messages are indexed for search with kind
//! `mail`, scoped to their owner. While the owner is online, one IMAP IDLE connection per
//! account watches the inbox, so new mail arrives live (a hub event plus a notification).
//!
//! Connections are bounded: at most [`imap::POOL_SIZE`] pooled command connections per
//! account (closed after two idle minutes) plus the IDLE connection while the owner is online.
//!
//! Every operation on an account, its folders or messages is checked by
//! [`Core::mail_access`] (owner or delegate, with the right the operation needs; see
//! `delegation`).
//!
//! * `crypto`: passwords at rest. * `imap`: connections and pool. * `sync`: folder and
//!   message sync, IDLE. * `store`: reading and acting on cached mail. * `compose`: replies,
//!   drafts, sending. * `parse`: MIME, threading, sanitizing.
//! * `autoconfig` and `dns`: provider settings (ISPDB, the provider's own
//!   autoconfig/autodiscover, MX records).
//! * `delegation`: the access check and sharing accounts. * `oauth`: Google and Microsoft
//!   (XOAUTH2). * `provisioning`: admin templates. * `settings`: admin settings.
//! * `abuse`: sending alerts. * `antispam`: report spam / not spam. * `background`: periodic
//!   sync of every account.
//! * `security`, `trust`, `phishing`, `dkim`, `smime`, `smime_store`: what the reader shows
//!   about a message's trustworthiness, S/MIME and PGP.

pub mod abuse;
pub mod antispam;
pub mod autoconfig;
pub mod background;
pub mod compose;
pub mod crypto;
pub mod delegation;
pub mod dkim;
pub mod dns;
pub mod imap;
pub mod oauth;
pub mod parse;
pub mod phishing;
pub mod provisioning;
pub mod security;
pub mod settings;
pub mod smime;
pub mod smime_store;
pub mod store;
pub mod sync;
pub mod trust;

pub use delegation::{MailRight, MailRole};

use crate::mailer::{SmtpSecurity, SmtpSettings};
use crate::util::{new_id, now};
use crate::{Core, Error, Result};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Connection settings for one server (IMAP or SMTP), with the decrypted password.
#[derive(Clone)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    pub security: SmtpSecurity,
    pub username: String,
    /// The password, or the OAuth access token when `xoauth2`.
    pub password: String,
    /// Sign in with SASL XOAUTH2 (OAuth accounts) instead of LOGIN.
    pub xoauth2: bool,
}

impl std::fmt::Debug for ServerSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerSettings")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("security", &self.security)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("xoauth2", &self.xoauth2)
            .finish()
    }
}

/// A connected mail account. Passwords and tokens are sealed and never serialized. The
/// viewer-dependent fields (`role`, `delegated`, `owner_name`, `permissions`) are filled by
/// [`Core::mail_access`] and [`Core::mail_accounts_for`].
#[derive(Clone, Serialize, sqlx::FromRow)]
pub struct Account {
    pub id: String,
    #[serde(skip)]
    pub user_id: String,
    pub name: String,
    pub email: String,
    pub imap_host: String,
    pub imap_port: i64,
    pub imap_security: String,
    pub imap_username: String,
    pub smtp_host: String,
    pub smtp_port: i64,
    pub smtp_security: String,
    pub smtp_username: String,
    #[serde(skip)]
    pub password: String,
    #[serde(skip)]
    pub smtp_password: Option<String>,
    pub save_sent: bool,
    pub signature: String,
    pub last_error: Option<String>,
    pub synced_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// `password` or `xoauth2`.
    pub auth_method: String,
    /// `google` or `microsoft` for OAuth accounts.
    pub oauth_provider: Option<String>,
    #[serde(skip)]
    pub oauth_refresh: Option<String>,
    #[serde(skip)]
    pub oauth_expires_at: Option<DateTime<Utc>>,
    /// The provisioning template that created the account (it is managed by the admin).
    pub provisioning_id: Option<String>,
    /// Storage used and allowed in bytes (IMAP QUOTA), when the server reports it.
    pub quota_used: Option<i64>,
    pub quota_limit: Option<i64>,
    #[sqlx(skip)]
    pub role: Option<MailRole>,
    /// Shared with the viewer by its owner.
    #[sqlx(skip)]
    pub delegated: bool,
    /// The owner's display name, on delegated accounts.
    #[sqlx(skip)]
    pub owner_name: Option<String>,
    #[sqlx(skip)]
    pub permissions: delegation::AccountPermissions,
}

pub(crate) const ACCOUNT_COLS: &str = "id, user_id, name, email, imap_host, imap_port, imap_security, imap_username, smtp_host, smtp_port, smtp_security, \
     smtp_username, password, smtp_password, save_sent, signature, last_error, synced_at, created_at, auth_method, oauth_provider, oauth_refresh, \
     oauth_expires_at, provisioning_id, quota_used, quota_limit";

impl Account {
    /// `Name <email>` for From headers.
    pub fn sender(&self) -> String {
        parse::Addr {
            name: self.name.clone(),
            address: self.email.clone(),
        }
        .display()
    }
}

/// Create/update input. On update, `None` (or an empty password) keeps the current value.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AccountInput {
    pub name: Option<String>,
    pub email: Option<String>,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
    pub imap_security: Option<SmtpSecurity>,
    pub imap_username: Option<String>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_security: Option<SmtpSecurity>,
    pub smtp_username: Option<String>,
    /// False for SMTP relays that take mail without logging in (stored as an empty SMTP
    /// username).
    pub smtp_auth: Option<bool>,
    #[serde(default, skip_serializing)]
    pub password: Option<String>,
    /// A separate SMTP password; empty means "same as IMAP".
    #[serde(default, skip_serializing)]
    pub smtp_password: Option<String>,
    pub save_sent: Option<bool>,
    pub signature: Option<String>,
}

/// A row to insert into `mail_accounts` (user-added, OAuth or provisioned). Secrets are
/// plain here and sealed on insert.
pub(crate) struct NewAccount {
    pub user_id: String,
    pub name: String,
    pub email: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_security: SmtpSecurity,
    pub imap_username: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: SmtpSecurity,
    pub smtp_username: String,
    pub password: String,
    pub smtp_password: Option<String>,
    pub save_sent: bool,
    pub signature: String,
    pub auth_method: &'static str,
    pub oauth_provider: Option<String>,
    pub oauth_refresh: Option<String>,
    pub oauth_expires_at: Option<DateTime<Utc>>,
    pub provisioning_id: Option<String>,
}

pub(crate) fn security_str(s: SmtpSecurity) -> &'static str {
    match s {
        SmtpSecurity::Tls => "tls",
        SmtpSecurity::Starttls => "starttls",
        SmtpSecurity::None => "none",
    }
}

pub(crate) fn parse_security(s: &str) -> SmtpSecurity {
    match s {
        "tls" => SmtpSecurity::Tls,
        "none" => SmtpSecurity::None,
        _ => SmtpSecurity::Starttls,
    }
}

/// Providers whose SMTP servers file sent mail in Sent themselves (a second copy would
/// duplicate it).
pub fn provider_saves_sent(smtp_host: &str) -> bool {
    let h = smtp_host.trim().to_ascii_lowercase();
    ["gmail.com", "googlemail.com", "office365.com", "outlook.com"]
        .iter()
        .any(|d| h == *d || h.ends_with(&format!(".{d}")))
}

/// Live state: connection pools, IDLE watchers and per-folder sync locks.
#[derive(Default)]
pub struct MailState {
    pools: DashMap<String, Arc<imap::Pool>>,
    idlers: DashMap<String, sync::IdleTask>,
    folder_locks: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
    /// When a folder list or folder was last synced (debounces background refreshes).
    last_sync: DashMap<String, Instant>,
    /// DKIM verifier (see `security`).
    pub security: security::SecurityState,
    /// Delegates per account (who else receives the account's live events).
    delegates: DashMap<String, Vec<String>>,
    /// The admin settings, loaded on first use.
    settings: parking_lot::RwLock<Option<Arc<settings::MailSettings>>>,
    settings_write: tokio::sync::Mutex<()>,
    /// One OAuth token refresh per account at a time.
    refresh_locks: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
    /// HTTP clients for OAuth and autoconfig: public addresses only, or any address.
    http_public: std::sync::OnceLock<reqwest::Client>,
    http_local: std::sync::OnceLock<reqwest::Client>,
    /// A background sync round is running.
    background_running: std::sync::atomic::AtomicBool,
    /// One provisioning reconcile at a time.
    provisioning: tokio::sync::Mutex<()>,
}

impl MailState {
    fn pool(&self, account_id: &str) -> Arc<imap::Pool> {
        self.pools.entry(account_id.to_string()).or_default().clone()
    }

    fn folder_lock(&self, folder_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.folder_locks.entry(folder_id.to_string()).or_default().clone()
    }

    /// Drops pooled connections and the IDLE watcher of an account (settings changed or
    /// account removed).
    pub(crate) fn forget(&self, account_id: &str) {
        if let Some((_, p)) = self.pools.remove(account_id) {
            p.close_all();
        }
        if let Some((_, t)) = self.idlers.remove(account_id) {
            t.handle.abort();
        }
    }

    /// Open IMAP connections right now (pooled idle ones plus IDLE watchers), for tests and
    /// the admin dashboard.
    pub fn open_idle_connections(&self) -> usize {
        self.pools.iter().map(|p| p.reap(imap::IDLE_CLOSE)).sum::<usize>() + self.idlers.iter().filter(|t| !t.handle.is_finished()).count()
    }
}

fn required(v: &Option<String>, what: &str) -> Result<String> {
    v.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .ok_or_else(|| Error::bad(format!("{what} is required")))
}

impl Core {
    /// A write transaction that takes the write lock up front (BEGIN IMMEDIATE). Mail syncs
    /// read before they write; a deferred transaction could then fail at once with
    /// SQLITE_BUSY when another sync committed in between, instead of waiting its turn.
    pub(crate) async fn mail_tx(&self) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>> {
        Ok(self.db.begin_with("BEGIN IMMEDIATE").await?)
    }

    fn secret_box(&self) -> crypto::SecretBox {
        crypto::SecretBox::new(&self.secret)
    }

    /// The accounts `user_id` owns (not those delegated to them: see [`Core::mail_accounts_for`]).
    pub async fn mail_accounts(&self, user_id: &str) -> Result<Vec<Account>> {
        let rows: Vec<Account> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {ACCOUNT_COLS} FROM mail_accounts WHERE user_id = ? ORDER BY created_at"
        )))
        .bind(user_id)
        .fetch_all(&self.db)
        .await?;
        Ok(rows.into_iter().map(|a| a.for_viewer(MailRole::Owner, None)).collect())
    }

    /// An account `user_id` owns (NotFound for anyone else's, delegates included).
    ///
    /// Kept for code written before delegation: a thin wrapper around the central check with
    /// [`MailRight::Owner`]. New code calls [`Core::mail_access`] with the right it needs, which
    /// also lets delegates in where they belong.
    pub async fn mail_account(&self, user_id: &str, id: &str) -> Result<Account> {
        self.mail_access(user_id, id, MailRight::Owner).await
    }

    pub(crate) async fn mail_account_by_id(&self, id: &str) -> Result<Account> {
        sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {ACCOUNT_COLS} FROM mail_accounts WHERE id = ?")))
            .bind(id)
            .fetch_optional(&self.db)
            .await?
            .ok_or(Error::NotFound)
    }

    /// IMAP settings with the decrypted password.
    pub fn imap_settings(&self, a: &Account) -> Result<ServerSettings> {
        Ok(ServerSettings {
            host: a.imap_host.clone(),
            port: a.imap_port as u16,
            security: parse_security(&a.imap_security),
            username: a.imap_username.clone(),
            password: self.secret_box().open(&a.id, &a.password)?,
            xoauth2: a.auth_method == "xoauth2",
        })
    }

    /// How mail connections may be made (admin settings: TLS verification, local servers).
    pub async fn mail_policy(&self) -> imap::ConnectPolicy {
        let s = self.mail_settings().await;
        imap::ConnectPolicy {
            verify_tls: s.verify_tls,
            allow_local: s.allow_local_servers,
        }
    }

    /// An SMTP transport for an account: its fresh credentials (XOAUTH2 for OAuth accounts)
    /// under the admin's connection policy.
    pub async fn mail_smtp_transport(&self, a: &Account) -> Result<lettre::AsyncSmtpTransport<lettre::Tokio1Executor>> {
        let a = self.mail_fresh_credentials(a).await?;
        let settings = self.smtp_settings(&a)?;
        self.mail_transport(&settings, a.auth_method == "xoauth2").await
    }

    async fn mail_transport(&self, smtp: &SmtpSettings, xoauth2: bool) -> Result<lettre::AsyncSmtpTransport<lettre::Tokio1Executor>> {
        let policy = self.mail_policy().await;
        crate::net::resolve(&smtp.host, smtp.port, policy.allow_local).await?;
        crate::mailer::transport_with(
            smtp,
            &crate::mailer::TransportOptions {
                xoauth2,
                accept_invalid_certs: !policy.verify_tls,
            },
        )
    }

    /// SMTP settings with the decrypted password (the IMAP one unless a separate one is set).
    pub fn smtp_settings(&self, a: &Account) -> Result<SmtpSettings> {
        let sealed = a.smtp_password.as_deref().unwrap_or(&a.password);
        let auth = !a.smtp_username.is_empty();
        Ok(SmtpSettings {
            host: a.smtp_host.clone(),
            port: a.smtp_port as u16,
            security: parse_security(&a.smtp_security),
            username: auth.then(|| a.smtp_username.clone()),
            password: if auth { Some(self.secret_box().open(&a.id, sealed)?) } else { None },
            from: a.sender(),
        })
    }

    /// Logs in to IMAP and connects (and authenticates) to SMTP, so bad settings are caught
    /// when they are saved.
    pub async fn test_mail_settings(&self, imap: &ServerSettings, smtp: &SmtpSettings) -> Result<()> {
        self.test_mail_connection(imap, smtp, imap.xoauth2).await
    }

    /// [`Core::test_mail_settings`] with the SMTP mechanism given (XOAUTH2 or the default).
    pub async fn test_mail_connection(&self, imap: &ServerSettings, smtp: &SmtpSettings, smtp_xoauth2: bool) -> Result<()> {
        self.test_imap(imap).await?;
        self.test_smtp(smtp, smtp_xoauth2).await
    }

    /// Connects and logs in to IMAP under the admin's policy.
    pub async fn test_imap(&self, imap: &ServerSettings) -> Result<()> {
        let conn = imap::ImapConn::connect_with(imap, self.mail_policy().await).await?;
        conn.logout().await;
        Ok(())
    }

    /// Connects (and authenticates, when credentials are given) to SMTP.
    pub async fn test_smtp(&self, smtp: &SmtpSettings, xoauth2: bool) -> Result<()> {
        let transport = self.mail_transport(smtp, xoauth2).await?;
        match tokio::time::timeout(Duration::from_secs(30), transport.test_connection()).await {
            Ok(Ok(true)) => Ok(()),
            Ok(Ok(false)) => Err(Error::bad(format!("the SMTP server {}:{} did not accept the connection", smtp.host, smtp.port))),
            Ok(Err(e)) => Err(Error::bad(format!("SMTP: {e}"))),
            Err(_) => Err(Error::bad(format!("SMTP: connecting to {}:{} timed out", smtp.host, smtp.port))),
        }
    }

    /// Inserts an account row (sealing its secrets); returns its id.
    pub(crate) async fn insert_mail_account(&self, n: &NewAccount) -> Result<String> {
        let id = new_id();
        let sb = self.secret_box();
        let sealed = sb.seal(&id, &n.password)?;
        let sealed_smtp = n.smtp_password.as_deref().map(|p| sb.seal(&id, p)).transpose()?;
        let sealed_refresh = n.oauth_refresh.as_deref().map(|p| sb.seal(&id, p)).transpose()?;
        sqlx::query(
            "INSERT INTO mail_accounts (id, user_id, name, email, imap_host, imap_port, imap_security, imap_username, smtp_host, smtp_port,
             smtp_security, smtp_username, password, smtp_password, save_sent, signature, created_at, auth_method, oauth_provider, oauth_refresh,
             oauth_expires_at, provisioning_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&n.user_id)
        .bind(&n.name)
        .bind(&n.email)
        .bind(&n.imap_host)
        .bind(n.imap_port as i64)
        .bind(security_str(n.imap_security))
        .bind(&n.imap_username)
        .bind(&n.smtp_host)
        .bind(n.smtp_port as i64)
        .bind(security_str(n.smtp_security))
        .bind(&n.smtp_username)
        .bind(&sealed)
        .bind(&sealed_smtp)
        .bind(n.save_sent)
        .bind(&n.signature)
        .bind(now())
        .bind(n.auth_method)
        .bind(&n.oauth_provider)
        .bind(&sealed_refresh)
        .bind(n.oauth_expires_at)
        .bind(&n.provisioning_id)
        .execute(&self.db)
        .await?;
        Ok(id)
    }

    /// After an account was added: start syncing its folders in the background and tell the
    /// owner's open tabs.
    pub(crate) async fn after_account_added(&self, user_id: &str, id: &str) -> Result<Account> {
        let account = self.mail_access(user_id, id, MailRight::Read).await?;
        let core = self.clone();
        let acc = account.clone();
        tokio::spawn(async move {
            if let Err(e) = core.sync_mail_folders(&acc).await {
                tracing::warn!(account = %acc.id, error = %e, "initial mail folder sync failed");
            }
            core.ensure_mail_idle(&acc.user_id).await;
        });
        self.hub
            .publish([user_id], &serde_json::json!({"type": "mail.changed", "account_id": id, "what": "accounts"}));
        Ok(account)
    }

    /// Adds an account after testing it, then starts syncing its folders in the background.
    pub async fn create_mail_account(&self, user_id: &str, input: &AccountInput) -> Result<Account> {
        if !self.mail_allow_user_accounts().await {
            return Err(Error::bad("adding mail accounts is turned off on this server"));
        }
        let email = required(&input.email, "email address")?;
        if parse::Addr::parse(&email).is_none() {
            return Err(Error::bad(format!("{email:?} is not an email address")));
        }
        let password = input
            .password
            .clone()
            .filter(|p| !p.is_empty())
            .ok_or_else(|| Error::bad("password is required"))?;
        let imap_host = required(&input.imap_host, "IMAP server")?;
        let smtp_host = required(&input.smtp_host, "SMTP server")?;
        let imap_security = input.imap_security.unwrap_or(SmtpSecurity::Tls);
        let smtp_security = input.smtp_security.unwrap_or(SmtpSecurity::Starttls);
        let imap_port = input.imap_port.unwrap_or(match imap_security {
            SmtpSecurity::Tls => 993,
            _ => 143,
        });
        let smtp_port = input.smtp_port.unwrap_or(match smtp_security {
            SmtpSecurity::Tls => 465,
            SmtpSecurity::Starttls => 587,
            SmtpSecurity::None => 25,
        });
        let imap_username = input.imap_username.clone().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| email.clone());
        let smtp_username = if input.smtp_auth == Some(false) {
            String::new()
        } else {
            input
                .smtp_username
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| imap_username.clone())
        };
        let smtp_password = input.smtp_password.clone().filter(|p| !p.is_empty());
        let name = input.name.clone().map(|n| n.trim().to_string()).unwrap_or_default();
        let imap = ServerSettings {
            host: imap_host.clone(),
            port: imap_port,
            security: imap_security,
            username: imap_username.clone(),
            password: password.clone(),
            xoauth2: false,
        };
        let smtp_auth = !smtp_username.is_empty();
        let smtp = SmtpSettings {
            host: smtp_host.clone(),
            port: smtp_port,
            security: smtp_security,
            username: smtp_auth.then(|| smtp_username.clone()),
            password: smtp_auth.then(|| smtp_password.clone().unwrap_or_else(|| password.clone())),
            from: email.clone(),
        };
        self.test_mail_settings(&imap, &smtp).await?;
        let save_sent = input.save_sent.unwrap_or_else(|| !provider_saves_sent(&smtp_host));
        let id = self
            .insert_mail_account(&NewAccount {
                user_id: user_id.to_string(),
                name,
                email,
                imap_host,
                imap_port,
                imap_security,
                imap_username,
                smtp_host,
                smtp_port,
                smtp_security,
                smtp_username,
                password,
                smtp_password,
                save_sent,
                signature: input.signature.clone().unwrap_or_default(),
                auth_method: "password",
                oauth_provider: None,
                oauth_refresh: None,
                oauth_expires_at: None,
                provisioning_id: None,
            })
            .await?;
        self.after_account_added(user_id, &id).await
    }

    /// Changes an account. Connection changes (servers, usernames, passwords, the address)
    /// need [`MailRight::Authenticate`] (the owner of an account they added themselves); the
    /// sender name, signature and Sent copy need [`MailRight::Configure`] (delegates and
    /// provisioned accounts too). Connection changes are tested first and reset the account's
    /// connections.
    pub async fn update_mail_account(&self, user_id: &str, id: &str, input: &AccountInput) -> Result<Account> {
        let cur = self.mail_access(user_id, id, MailRight::Configure).await?;
        let pick = |v: &Option<String>, old: &str| {
            v.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .unwrap_or_else(|| old.to_string())
        };
        let email = pick(&input.email, &cur.email);
        if parse::Addr::parse(&email).is_none() {
            return Err(Error::bad(format!("{email:?} is not an email address")));
        }
        let new_password = input.password.clone().filter(|p| !p.is_empty());
        let new_smtp_password = input.smtp_password.clone();
        let imap_host = pick(&input.imap_host, &cur.imap_host);
        let smtp_host = pick(&input.smtp_host, &cur.smtp_host);
        let imap_port = input.imap_port.map(i64::from).unwrap_or(cur.imap_port);
        let smtp_port = input.smtp_port.map(i64::from).unwrap_or(cur.smtp_port);
        let imap_security = input.imap_security.map(security_str).unwrap_or(&cur.imap_security).to_string();
        let smtp_security = input.smtp_security.map(security_str).unwrap_or(&cur.smtp_security).to_string();
        let imap_username = pick(&input.imap_username, &cur.imap_username);
        let smtp_username = match input.smtp_auth {
            Some(false) => String::new(),
            Some(true) if cur.smtp_username.is_empty() => pick(&input.smtp_username, &imap_username),
            _ => pick(&input.smtp_username, &cur.smtp_username),
        };
        let sb = self.secret_box();
        let sealed = match &new_password {
            Some(p) => sb.seal(id, p)?,
            None => cur.password.clone(),
        };
        let sealed_smtp = match &new_smtp_password {
            Some(p) if p.is_empty() => None,
            Some(p) => Some(sb.seal(id, p)?),
            None => cur.smtp_password.clone(),
        };
        let mut next = cur.clone();
        next.email = email;
        next.name = input.name.as_deref().map(|n| n.trim().to_string()).unwrap_or(cur.name.clone());
        next.imap_host = imap_host;
        next.imap_port = imap_port;
        next.imap_security = imap_security;
        next.imap_username = imap_username;
        next.smtp_host = smtp_host;
        next.smtp_port = smtp_port;
        next.smtp_security = smtp_security;
        next.smtp_username = smtp_username;
        next.password = sealed;
        next.smtp_password = sealed_smtp;
        next.save_sent = input.save_sent.unwrap_or(cur.save_sent);
        next.signature = input.signature.clone().unwrap_or(cur.signature.clone());
        let connection_changed = next.imap_host != cur.imap_host
            || next.imap_port != cur.imap_port
            || next.imap_security != cur.imap_security
            || next.imap_username != cur.imap_username
            || next.smtp_host != cur.smtp_host
            || next.smtp_port != cur.smtp_port
            || next.smtp_security != cur.smtp_security
            || next.smtp_username != cur.smtp_username
            || next.email != cur.email
            || new_password.is_some()
            || new_smtp_password.as_deref().is_some_and(|p| !p.is_empty() || cur.smtp_password.is_some());
        if connection_changed {
            let role = cur.role.unwrap_or(MailRole::Delegate);
            if !delegation::allowed(role, cur.provisioning_id.is_some(), MailRight::Authenticate) {
                return Err(Error::Forbidden);
            }
            if cur.auth_method == "xoauth2" {
                // The address and servers of OAuth accounts come from the provider.
                return Err(Error::bad("this account signs in with OAuth; reconnect it to change how it signs in"));
            }
            self.test_mail_settings(&self.imap_settings(&next)?, &self.smtp_settings(&next)?).await?;
        }
        sqlx::query(
            "UPDATE mail_accounts SET name = ?, email = ?, imap_host = ?, imap_port = ?, imap_security = ?, imap_username = ?, smtp_host = ?,
             smtp_port = ?, smtp_security = ?, smtp_username = ?, password = ?, smtp_password = ?, save_sent = ?, signature = ?, last_error = NULL WHERE id = ?",
        )
        .bind(&next.name)
        .bind(&next.email)
        .bind(&next.imap_host)
        .bind(next.imap_port)
        .bind(&next.imap_security)
        .bind(&next.imap_username)
        .bind(&next.smtp_host)
        .bind(next.smtp_port)
        .bind(&next.smtp_security)
        .bind(&next.smtp_username)
        .bind(&next.password)
        .bind(&next.smtp_password)
        .bind(next.save_sent)
        .bind(&next.signature)
        .bind(id)
        .execute(&self.db)
        .await?;
        if connection_changed {
            self.mail.forget(id);
            self.ensure_mail_idle(&cur.user_id).await;
        }
        self.mail_publish(&cur, &serde_json::json!({"type": "mail.changed", "account_id": id, "what": "accounts"}));
        self.mail_access(user_id, id, MailRight::Read).await
    }

    /// Removes an account and its cache (the mail stays on the server). Owner only, and not
    /// for provisioned accounts (the administrator removes those).
    pub async fn delete_mail_account(&self, user_id: &str, id: &str) -> Result<()> {
        let a = self.mail_access(user_id, id, MailRight::Remove).await?;
        self.remove_mail_account_row(&a).await
    }

    /// Drops an account (no access check: the caller decided).
    pub(crate) async fn remove_mail_account_row(&self, a: &Account) -> Result<()> {
        let audience = self.mail_audience(a);
        self.mail.forget(&a.id);
        sqlx::query("DELETE FROM mail_accounts WHERE id = ?").bind(&a.id).execute(&self.db).await?;
        self.mail.delegates.remove(&a.id);
        self.hub.publish(
            audience.iter().map(String::as_str),
            &serde_json::json!({"type": "mail.changed", "account_id": a.id, "what": "accounts"}),
        );
        Ok(())
    }

    pub(crate) async fn set_mail_error(&self, account_id: &str, error: Option<&str>) {
        let _ = sqlx::query("UPDATE mail_accounts SET last_error = ?, synced_at = CASE WHEN ? IS NULL THEN ? ELSE synced_at END WHERE id = ?")
            .bind(error)
            .bind(error)
            .bind(now())
            .bind(account_id)
            .execute(&self.db)
            .await;
    }

    /// Runs mail housekeeping for the life of the process: IDLE watchers for online users,
    /// closing idle pooled connections, expiring composer uploads, and (see `background`) the
    /// periodic sync of every account with provisioning.
    pub fn spawn_mail_background(&self) {
        self.spawn_mail_sync_all();
        let core = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(15));
            let mut rounds: u64 = 0;
            loop {
                tick.tick().await;
                rounds += 1;
                core.manage_idlers().await;
                for p in core.mail.pools.iter() {
                    p.reap(imap::IDLE_CLOSE);
                }
                if rounds % 240 == 1 {
                    let cutoff = now() - chrono::Duration::hours(24);
                    if let Err(e) = sqlx::query("DELETE FROM mail_uploads WHERE created_at < ?")
                        .bind(cutoff)
                        .execute(&core.db)
                        .await
                    {
                        tracing::warn!(error = %e, "expiring mail uploads failed");
                    }
                    core.mail_prune_send_log().await;
                }
            }
        });
    }
}
