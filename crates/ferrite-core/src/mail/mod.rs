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
//! * `crypto`: passwords at rest. * `imap`: connections and pool. * `sync`: folder and
//!   message sync, IDLE. * `store`: reading and acting on cached mail. * `compose`: replies,
//!   drafts, sending. * `parse`: MIME, threading, sanitizing. * `autoconfig`: provider
//!   settings from Thunderbird's ISPDB.

pub mod autoconfig;
pub mod compose;
pub mod crypto;
pub mod imap;
pub mod parse;
pub mod store;
pub mod sync;

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
    pub password: String,
}

impl std::fmt::Debug for ServerSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerSettings")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("security", &self.security)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// A connected mail account. Passwords are sealed and never serialized.
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
}

const ACCOUNT_COLS: &str = "id, user_id, name, email, imap_host, imap_port, imap_security, imap_username, smtp_host, smtp_port, smtp_security, \
     smtp_username, password, smtp_password, save_sent, signature, last_error, synced_at, created_at";

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

fn security_str(s: SmtpSecurity) -> &'static str {
    match s {
        SmtpSecurity::Tls => "tls",
        SmtpSecurity::Starttls => "starttls",
        SmtpSecurity::None => "none",
    }
}

fn parse_security(s: &str) -> SmtpSecurity {
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
    fn forget(&self, account_id: &str) {
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

    pub async fn mail_accounts(&self, user_id: &str) -> Result<Vec<Account>> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {ACCOUNT_COLS} FROM mail_accounts WHERE user_id = ? ORDER BY created_at"
        )))
        .bind(user_id)
        .fetch_all(&self.db)
        .await?)
    }

    /// An account of `user_id` (NotFound for anyone else's).
    pub async fn mail_account(&self, user_id: &str, id: &str) -> Result<Account> {
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {ACCOUNT_COLS} FROM mail_accounts WHERE id = ? AND user_id = ?"
        )))
        .bind(id)
        .bind(user_id)
        .fetch_optional(&self.db)
        .await?
        .ok_or(Error::NotFound)
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
        })
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
        let conn = imap::ImapConn::connect(imap).await?;
        conn.logout().await;
        let transport = crate::mailer::transport(smtp)?;
        match tokio::time::timeout(Duration::from_secs(30), transport.test_connection()).await {
            Ok(Ok(true)) => Ok(()),
            Ok(Ok(false)) => Err(Error::bad(format!("the SMTP server {}:{} did not accept the connection", smtp.host, smtp.port))),
            Ok(Err(e)) => Err(Error::bad(format!("SMTP: {e}"))),
            Err(_) => Err(Error::bad(format!("SMTP: connecting to {}:{} timed out", smtp.host, smtp.port))),
        }
    }

    /// Adds an account after testing it, then starts syncing its folders in the background.
    pub async fn create_mail_account(&self, user_id: &str, input: &AccountInput) -> Result<Account> {
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
        let id = new_id();
        let sb = self.secret_box();
        let sealed = sb.seal(&id, &password)?;
        let sealed_smtp = smtp_password.as_deref().map(|p| sb.seal(&id, p)).transpose()?;
        let save_sent = input.save_sent.unwrap_or_else(|| !provider_saves_sent(&smtp_host));
        sqlx::query(
            "INSERT INTO mail_accounts (id, user_id, name, email, imap_host, imap_port, imap_security, imap_username, smtp_host, smtp_port,
             smtp_security, smtp_username, password, smtp_password, save_sent, signature, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(user_id)
        .bind(&name)
        .bind(&email)
        .bind(&imap_host)
        .bind(imap_port as i64)
        .bind(security_str(imap_security))
        .bind(&imap_username)
        .bind(&smtp_host)
        .bind(smtp_port as i64)
        .bind(security_str(smtp_security))
        .bind(&smtp_username)
        .bind(&sealed)
        .bind(&sealed_smtp)
        .bind(save_sent)
        .bind(input.signature.as_deref().unwrap_or(""))
        .bind(now())
        .execute(&self.db)
        .await?;
        let account = self.mail_account(user_id, &id).await?;
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

    /// Changes an account. Connection changes (servers, usernames, passwords) are tested
    /// first and reset the account's connections.
    pub async fn update_mail_account(&self, user_id: &str, id: &str, input: &AccountInput) -> Result<Account> {
        let cur = self.mail_account(user_id, id).await?;
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
            || new_password.is_some()
            || new_smtp_password.is_some();
        if connection_changed {
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
            self.ensure_mail_idle(user_id).await;
        }
        self.hub
            .publish([user_id], &serde_json::json!({"type": "mail.changed", "account_id": id, "what": "accounts"}));
        self.mail_account(user_id, id).await
    }

    /// Removes an account and its cache (the mail stays on the server).
    pub async fn delete_mail_account(&self, user_id: &str, id: &str) -> Result<()> {
        let a = self.mail_account(user_id, id).await?;
        self.mail.forget(&a.id);
        sqlx::query("DELETE FROM mail_accounts WHERE id = ?").bind(&a.id).execute(&self.db).await?;
        self.hub
            .publish([user_id], &serde_json::json!({"type": "mail.changed", "account_id": id, "what": "accounts"}));
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
    /// closing idle pooled connections, expiring composer uploads.
    pub fn spawn_mail_background(&self) {
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
                }
            }
        });
    }
}
