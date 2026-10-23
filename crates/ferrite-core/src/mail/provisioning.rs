//! Provisioning: administrator-defined templates that give every matching user a mail
//! account automatically.
//!
//! A template matches users by email domain (`*`: anyone with an address), by group, or all
//! users. Its patterns may use `%USERID%` (the Ferrite username) and `%EMAIL%` (the user's
//! email address); a template whose pattern needs an email skips users without one.
//!
//! Two ways to log in to the users' mailboxes:
//!
//! * **Master password** (`master`): one password, typically a Dovecot master user
//!   (`%USERID%*master`), sealed with the server secret. It opens every provisioned mailbox,
//!   so whoever gets it (and the server secret) reads everyone's mail; but users' own
//!   passwords are never stored.
//! * **Login password** (`login`): each user's own Ferrite sign-in password, captured when
//!   they sign in with it and sealed with the server secret. Only users a login-mode template
//!   matches have it stored, and it is deleted when no template needs it. Whoever gets the
//!   database and the server secret recovers those passwords, which people may reuse
//!   elsewhere; and it needs password sign-ins (single sign-on never sees a password, so
//!   those users get no account until they sign in with one).
//!
//! Provisioned accounts are managed by the administrator: users can change only the sender
//! name, signature and Sent copy (see the rights matrix in `delegation`). [`Core::mail_provision_all`]
//! reconciles every user (creating, updating and removing accounts); it runs when templates
//! change, on each background sync round, and when a user signs in.

use super::{Account, NewAccount, parse_security, security_str};
use crate::auth::User;
use crate::mailer::SmtpSecurity;
use crate::util::{new_id, now};
use crate::{Core, Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const COLS: &str = "id, name, match_kind, match_value, email_pattern, name_pattern, imap_host, imap_port, imap_security, imap_username_pattern, \
     smtp_host, smtp_port, smtp_security, smtp_username_pattern, sieve_enabled, sieve_host, sieve_port, sieve_security, password_mode, \
     master_password, enabled, created_at, updated_at";

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Provisioning {
    pub id: String,
    pub name: String,
    /// `domain`, `group` or `all`.
    pub match_kind: String,
    pub match_value: String,
    pub email_pattern: String,
    pub name_pattern: String,
    pub imap_host: String,
    pub imap_port: i64,
    pub imap_security: String,
    pub imap_username_pattern: String,
    pub smtp_host: String,
    pub smtp_port: i64,
    pub smtp_security: String,
    pub smtp_username_pattern: String,
    pub sieve_enabled: bool,
    pub sieve_host: String,
    pub sieve_port: i64,
    pub sieve_security: String,
    /// `master` or `login`.
    pub password_mode: String,
    #[serde(skip)]
    pub master_password: Option<String>,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Accounts this template created.
    #[sqlx(skip)]
    pub accounts: i64,
    /// Matching users still waiting for an account (login mode: no password sign-in yet).
    #[sqlx(skip)]
    pub pending: i64,
    #[sqlx(skip)]
    pub master_password_set: bool,
}

/// Create/update input; on update every field is optional and `master_password: ""` keeps it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProvisioningInput {
    pub name: Option<String>,
    pub match_kind: Option<String>,
    pub match_value: Option<String>,
    pub email_pattern: Option<String>,
    pub name_pattern: Option<String>,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
    pub imap_security: Option<SmtpSecurity>,
    pub imap_username_pattern: Option<String>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_security: Option<SmtpSecurity>,
    pub smtp_username_pattern: Option<String>,
    pub sieve_enabled: Option<bool>,
    pub sieve_host: Option<String>,
    pub sieve_port: Option<u16>,
    pub sieve_security: Option<SmtpSecurity>,
    pub password_mode: Option<String>,
    #[serde(default, skip_serializing)]
    pub master_password: Option<String>,
    pub enabled: Option<bool>,
}

/// What a reconcile did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ProvisionReport {
    pub created: usize,
    pub updated: usize,
    pub removed: usize,
    /// Matching users without an account yet (login mode, no captured password).
    pub pending: usize,
}

/// Sieve settings of a provisioned account (for the Sieve integration).
#[derive(Debug, Clone, Serialize)]
pub struct ProvisionedSieve {
    pub host: String,
    pub port: u16,
    pub security: SmtpSecurity,
    pub username: String,
}

/// Expands `%USERID%` and `%EMAIL%`; `None` when the pattern needs an email the user lacks.
pub fn expand(pattern: &str, username: &str, email: Option<&str>) -> Option<String> {
    let mut s = pattern.replace("%USERID%", username);
    if s.contains("%EMAIL%") {
        s = s.replace("%EMAIL%", email.filter(|e| !e.trim().is_empty())?.trim());
    }
    Some(s.trim().to_string())
}

/// Whether a template's match rule covers a user (`groups`: the user's group ids).
pub fn matches(kind: &str, value: &str, user_email: Option<&str>, groups: &[String]) -> bool {
    match kind {
        "all" => true,
        "group" => groups.iter().any(|g| g == value),
        "domain" => {
            let Some(email) = user_email else { return false };
            let Some((_, domain)) = email.trim().rsplit_once('@') else {
                return false;
            };
            value.trim() == "*" || domain.eq_ignore_ascii_case(value.trim().trim_start_matches('@'))
        }
        _ => false,
    }
}

/// The account a template gives a user (password filled in later).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub email: String,
    pub name: String,
    pub imap_username: String,
    pub smtp_username: String,
}

/// Renders a template for a user; `None` when a pattern can't be filled.
pub fn render(p: &Provisioning, user: &User) -> Option<Rendered> {
    let email = user.email.as_deref();
    let rendered = Rendered {
        email: expand(&p.email_pattern, &user.username, email)?,
        name: if p.name_pattern.trim().is_empty() {
            user.display_name.clone()
        } else {
            expand(&p.name_pattern, &user.username, email)?
        },
        imap_username: expand(&p.imap_username_pattern, &user.username, email)?,
        smtp_username: expand(&p.smtp_username_pattern, &user.username, email)?,
    };
    super::parse::Addr::parse(&rendered.email).filter(|a| a.name.is_empty())?;
    Some(rendered)
}

fn check_mode(m: &str) -> Result<()> {
    if m == "master" || m == "login" {
        Ok(())
    } else {
        Err(Error::bad("password mode must be master or login"))
    }
}

fn check_match(kind: &str, value: &str) -> Result<()> {
    match kind {
        "all" => Ok(()),
        "group" if !value.trim().is_empty() => Ok(()),
        "domain" if value.trim() == "*" || super::autoconfig::domain_of(&format!("x@{}", value.trim().trim_start_matches('@'))).is_some() => Ok(()),
        "group" | "domain" => Err(Error::bad(format!("enter the {kind} this template applies to"))),
        _ => Err(Error::bad("match by domain, group or all")),
    }
}

impl Core {
    pub async fn mail_provisionings(&self) -> Result<Vec<Provisioning>> {
        let mut list: Vec<Provisioning> = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {COLS} FROM mail_provisionings ORDER BY created_at")))
            .fetch_all(&self.db)
            .await?;
        let users = self.provisioning_candidates().await?;
        for p in &mut list {
            p.master_password_set = p.master_password.is_some();
            p.accounts = sqlx::query_scalar("SELECT COUNT(*) FROM mail_accounts WHERE provisioning_id = ?")
                .bind(&p.id)
                .fetch_one(&self.db)
                .await?;
            if p.enabled {
                let mut pending = 0;
                for (u, groups) in &users {
                    if matches(&p.match_kind, &p.match_value, u.email.as_deref(), groups) && render(p, u).is_some() {
                        let has: Option<String> = sqlx::query_scalar("SELECT id FROM mail_accounts WHERE provisioning_id = ? AND user_id = ?")
                            .bind(&p.id)
                            .bind(&u.id)
                            .fetch_optional(&self.db)
                            .await?;
                        pending += i64::from(has.is_none());
                    }
                }
                p.pending = pending;
            }
        }
        Ok(list)
    }

    async fn mail_provisioning(&self, id: &str) -> Result<Provisioning> {
        sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {COLS} FROM mail_provisionings WHERE id = ?")))
            .bind(id)
            .fetch_optional(&self.db)
            .await?
            .ok_or(Error::NotFound)
    }

    /// Creates a template (administrators) and provisions its users.
    pub async fn mail_create_provisioning(&self, input: &ProvisioningInput) -> Result<(Provisioning, ProvisionReport)> {
        let req = |v: &Option<String>, what: &str| {
            v.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .ok_or_else(|| Error::bad(format!("{what} is required")))
        };
        let mode = input.password_mode.clone().unwrap_or_else(|| "master".into());
        check_mode(&mode)?;
        let kind = input.match_kind.clone().unwrap_or_else(|| "domain".into());
        let value = input.match_value.clone().unwrap_or_default();
        check_match(&kind, &value)?;
        let master = input.master_password.clone().filter(|p| !p.is_empty());
        if mode == "master" && master.is_none() {
            return Err(Error::bad("enter the master password"));
        }
        let imap_security = input.imap_security.unwrap_or(SmtpSecurity::Tls);
        let smtp_security = input.smtp_security.unwrap_or(SmtpSecurity::Starttls);
        let id = new_id();
        let sealed = match (&mode[..], &master) {
            ("master", Some(m)) => Some(self.secret_box().seal(&format!("provisioning:{id}"), m)?),
            _ => None,
        };
        let email_pattern = req(&input.email_pattern, "the email pattern")?;
        if expand(&email_pattern, "user", Some("user@example.org")).is_none_or(|e| super::parse::Addr::parse(&e).is_none()) {
            return Err(Error::bad("the email pattern must give an address, e.g. %USERID%@example.org or %EMAIL%"));
        }
        sqlx::query(
            "INSERT INTO mail_provisionings (id, name, match_kind, match_value, email_pattern, name_pattern, imap_host, imap_port, imap_security,
             imap_username_pattern, smtp_host, smtp_port, smtp_security, smtp_username_pattern, sieve_enabled, sieve_host, sieve_port, sieve_security,
             password_mode, master_password, enabled, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(input.name.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("Mail accounts"))
        .bind(&kind)
        .bind(value.trim())
        .bind(&email_pattern)
        .bind(input.name_pattern.as_deref().unwrap_or("").trim())
        .bind(req(&input.imap_host, "the IMAP server")?)
        .bind(input.imap_port.unwrap_or(if imap_security == SmtpSecurity::Tls { 993 } else { 143 }) as i64)
        .bind(security_str(imap_security))
        .bind(
            input
                .imap_username_pattern
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("%EMAIL%"),
        )
        .bind(req(&input.smtp_host, "the SMTP server")?)
        .bind(input.smtp_port.unwrap_or(match smtp_security {
            SmtpSecurity::Tls => 465,
            SmtpSecurity::Starttls => 587,
            SmtpSecurity::None => 25,
        }) as i64)
        .bind(security_str(smtp_security))
        .bind(input.smtp_username_pattern.as_deref().unwrap_or("%EMAIL%").trim())
        .bind(input.sieve_enabled.unwrap_or(false))
        .bind(input.sieve_host.as_deref().unwrap_or("").trim())
        .bind(input.sieve_port.unwrap_or(4190) as i64)
        .bind(security_str(input.sieve_security.unwrap_or(SmtpSecurity::Starttls)))
        .bind(&mode)
        .bind(&sealed)
        .bind(input.enabled.unwrap_or(true))
        .bind(now())
        .bind(now())
        .execute(&self.db)
        .await?;
        let report = self.mail_provision_all().await?;
        Ok((self.mail_provisioning(&id).await?, report))
    }

    /// Changes a template and re-provisions (accounts follow the new settings).
    pub async fn mail_update_provisioning(&self, id: &str, input: &ProvisioningInput) -> Result<(Provisioning, ProvisionReport)> {
        let mut p = self.mail_provisioning(id).await?;
        let set = |v: &Option<String>, cur: &mut String| {
            if let Some(x) = v {
                *cur = x.trim().to_string();
            }
        };
        set(&input.name, &mut p.name);
        set(&input.match_kind, &mut p.match_kind);
        set(&input.match_value, &mut p.match_value);
        set(&input.email_pattern, &mut p.email_pattern);
        set(&input.name_pattern, &mut p.name_pattern);
        set(&input.imap_host, &mut p.imap_host);
        set(&input.imap_username_pattern, &mut p.imap_username_pattern);
        set(&input.smtp_host, &mut p.smtp_host);
        set(&input.smtp_username_pattern, &mut p.smtp_username_pattern);
        set(&input.sieve_host, &mut p.sieve_host);
        set(&input.password_mode, &mut p.password_mode);
        if let Some(v) = input.imap_port {
            p.imap_port = v.into();
        }
        if let Some(v) = input.smtp_port {
            p.smtp_port = v.into();
        }
        if let Some(v) = input.sieve_port {
            p.sieve_port = v.into();
        }
        if let Some(v) = input.imap_security {
            p.imap_security = security_str(v).into();
        }
        if let Some(v) = input.smtp_security {
            p.smtp_security = security_str(v).into();
        }
        if let Some(v) = input.sieve_security {
            p.sieve_security = security_str(v).into();
        }
        if let Some(v) = input.sieve_enabled {
            p.sieve_enabled = v;
        }
        if let Some(v) = input.enabled {
            p.enabled = v;
        }
        check_mode(&p.password_mode)?;
        check_match(&p.match_kind, &p.match_value)?;
        if p.imap_host.is_empty() || p.smtp_host.is_empty() {
            return Err(Error::bad("the IMAP and SMTP servers are required"));
        }
        if expand(&p.email_pattern, "user", Some("user@example.org")).is_none_or(|e| super::parse::Addr::parse(&e).is_none()) {
            return Err(Error::bad("the email pattern must give an address, e.g. %USERID%@example.org or %EMAIL%"));
        }
        match input.master_password.as_deref() {
            Some(m) if !m.is_empty() => p.master_password = Some(self.secret_box().seal(&format!("provisioning:{id}"), m)?),
            _ => {}
        }
        if p.password_mode == "master" && p.master_password.is_none() {
            return Err(Error::bad("enter the master password"));
        }
        if p.password_mode == "login" {
            p.master_password = None;
        }
        sqlx::query(
            "UPDATE mail_provisionings SET name = ?, match_kind = ?, match_value = ?, email_pattern = ?, name_pattern = ?, imap_host = ?, imap_port = ?,
             imap_security = ?, imap_username_pattern = ?, smtp_host = ?, smtp_port = ?, smtp_security = ?, smtp_username_pattern = ?, sieve_enabled = ?,
             sieve_host = ?, sieve_port = ?, sieve_security = ?, password_mode = ?, master_password = ?, enabled = ?, updated_at = ? WHERE id = ?",
        )
        .bind(&p.name)
        .bind(&p.match_kind)
        .bind(&p.match_value)
        .bind(&p.email_pattern)
        .bind(&p.name_pattern)
        .bind(&p.imap_host)
        .bind(p.imap_port)
        .bind(&p.imap_security)
        .bind(&p.imap_username_pattern)
        .bind(&p.smtp_host)
        .bind(p.smtp_port)
        .bind(&p.smtp_security)
        .bind(&p.smtp_username_pattern)
        .bind(p.sieve_enabled)
        .bind(&p.sieve_host)
        .bind(p.sieve_port)
        .bind(&p.sieve_security)
        .bind(&p.password_mode)
        .bind(&p.master_password)
        .bind(p.enabled)
        .bind(now())
        .bind(id)
        .execute(&self.db)
        .await?;
        let report = self.mail_provision_all().await?;
        Ok((self.mail_provisioning(id).await?, report))
    }

    /// Removes every account of a template and turns the template off (so the next
    /// reconcile does not create them again). Returns how many accounts were removed.
    pub async fn mail_deprovision(&self, id: &str) -> Result<usize> {
        let p = self.mail_provisioning(id).await?;
        sqlx::query("UPDATE mail_provisionings SET enabled = 0, updated_at = ? WHERE id = ?")
            .bind(now())
            .bind(&p.id)
            .execute(&self.db)
            .await?;
        let accounts = self.provisioned_accounts(Some(&p.id)).await?;
        for a in &accounts {
            self.remove_mail_account_row(a).await?;
        }
        self.forget_unneeded_login_secrets().await?;
        tracing::info!(template = %p.id, removed = accounts.len(), "mail accounts deprovisioned");
        Ok(accounts.len())
    }

    /// Deprovisions and deletes a template.
    pub async fn mail_delete_provisioning(&self, id: &str) -> Result<usize> {
        let n = self.mail_deprovision(id).await?;
        sqlx::query("DELETE FROM mail_provisionings WHERE id = ?").bind(id).execute(&self.db).await?;
        Ok(n)
    }

    async fn provisioned_accounts(&self, template: Option<&str>) -> Result<Vec<Account>> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {} FROM mail_accounts WHERE provisioning_id IS NOT NULL AND (? IS NULL OR provisioning_id = ?)",
            super::ACCOUNT_COLS
        )))
        .bind(template)
        .bind(template)
        .fetch_all(&self.db)
        .await?)
    }

    /// Active users with their group ids.
    async fn provisioning_candidates(&self) -> Result<Vec<(User, Vec<String>)>> {
        let users = self.list_users(None, 1_000_000).await?;
        let memberships: Vec<(String, String)> = sqlx::query_as("SELECT user_id, group_id FROM group_members").fetch_all(&self.db).await?;
        Ok(users
            .into_iter()
            .filter(|u| !u.disabled)
            .map(|u| {
                let groups = memberships.iter().filter(|(uid, _)| *uid == u.id).map(|(_, g)| g.clone()).collect();
                (u, groups)
            })
            .collect())
    }

    /// The password a template logs in with for `user_id`, if known.
    async fn provisioning_password(&self, p: &Provisioning, user_id: &str) -> Result<Option<String>> {
        let sb = self.secret_box();
        match p.password_mode.as_str() {
            "master" => p.master_password.as_deref().map(|m| sb.open(&format!("provisioning:{}", p.id), m)).transpose(),
            _ => {
                let sealed: Option<String> = sqlx::query_scalar("SELECT password FROM mail_login_secrets WHERE user_id = ?")
                    .bind(user_id)
                    .fetch_optional(&self.db)
                    .await?;
                sealed.map(|s| sb.open(&format!("login:{user_id}"), &s)).transpose()
            }
        }
    }

    /// Reconciles every user with the templates: creates missing accounts, updates changed
    /// ones and removes those no template covers any more. Users who are disabled are left
    /// alone.
    pub async fn mail_provision_all(&self) -> Result<ProvisionReport> {
        let _guard = self.mail.provisioning.lock().await;
        let templates: Vec<Provisioning> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {COLS} FROM mail_provisionings WHERE enabled = 1 ORDER BY created_at"
        )))
        .fetch_all(&self.db)
        .await?;
        let candidates = self.provisioning_candidates().await?;
        let existing = self.provisioned_accounts(None).await?;
        let mut report = ProvisionReport::default();
        for (user, groups) in &candidates {
            for p in &templates {
                if !matches(&p.match_kind, &p.match_value, user.email.as_deref(), groups) {
                    continue;
                }
                let Some(r) = render(p, user) else { continue };
                let current = existing
                    .iter()
                    .find(|a| a.user_id == user.id && a.provisioning_id.as_deref() == Some(p.id.as_str()));
                let password = self.provisioning_password(p, &user.id).await?;
                match (current, password) {
                    (Some(a), password) => {
                        if self.update_provisioned(a, p, &r, password.as_deref()).await? {
                            report.updated += 1;
                        }
                    }
                    (None, Some(password)) => {
                        self.insert_provisioned(user, p, &r, password).await?;
                        report.created += 1;
                    }
                    (None, None) => report.pending += 1,
                }
            }
        }
        // Accounts whose template is gone, off, or no longer matches their (active) user.
        for a in &existing {
            let Some((user, groups)) = candidates.iter().find(|(u, _)| u.id == a.user_id) else {
                continue;
            };
            let keep = templates.iter().any(|p| {
                a.provisioning_id.as_deref() == Some(p.id.as_str())
                    && matches(&p.match_kind, &p.match_value, user.email.as_deref(), groups)
                    && render(p, user).is_some()
            });
            if !keep {
                self.remove_mail_account_row(a).await?;
                report.removed += 1;
            }
        }
        self.forget_unneeded_login_secrets().await?;
        if report.created + report.updated + report.removed > 0 {
            tracing::info!(
                created = report.created,
                updated = report.updated,
                removed = report.removed,
                "mail provisioning reconciled"
            );
        }
        Ok(report)
    }

    async fn insert_provisioned(&self, user: &User, p: &Provisioning, r: &Rendered, password: String) -> Result<()> {
        let smtp_host = p.smtp_host.clone();
        let id = self
            .insert_mail_account(&NewAccount {
                user_id: user.id.clone(),
                name: r.name.clone(),
                email: r.email.clone(),
                imap_host: p.imap_host.clone(),
                imap_port: p.imap_port as u16,
                imap_security: parse_security(&p.imap_security),
                imap_username: r.imap_username.clone(),
                smtp_host: smtp_host.clone(),
                smtp_port: p.smtp_port as u16,
                smtp_security: parse_security(&p.smtp_security),
                smtp_username: r.smtp_username.clone(),
                password,
                smtp_password: None,
                save_sent: !super::provider_saves_sent(&smtp_host),
                signature: String::new(),
                auth_method: "password",
                oauth_provider: None,
                oauth_refresh: None,
                oauth_expires_at: None,
                provisioning_id: Some(p.id.clone()),
            })
            .await?;
        tracing::info!(account = %id, user = %user.username, template = %p.id, "mail account provisioned");
        self.after_account_added(&user.id, &id).await?;
        Ok(())
    }

    /// Brings a provisioned account in line with its template; true when something changed.
    async fn update_provisioned(&self, a: &Account, p: &Provisioning, r: &Rendered, password: Option<&str>) -> Result<bool> {
        let sb = self.secret_box();
        let password_changed = match password {
            Some(pw) => sb.open(&a.id, &a.password).map(|cur| cur != pw).unwrap_or(true),
            None => false,
        };
        let changed = a.email != r.email
            || a.imap_host != p.imap_host
            || a.imap_port != p.imap_port
            || a.imap_security != p.imap_security
            || a.imap_username != r.imap_username
            || a.smtp_host != p.smtp_host
            || a.smtp_port != p.smtp_port
            || a.smtp_security != p.smtp_security
            || a.smtp_username != r.smtp_username
            || a.smtp_password.is_some()
            || password_changed;
        if !changed {
            return Ok(false);
        }
        let sealed = match password {
            Some(pw) if password_changed => sb.seal(&a.id, pw)?,
            _ => a.password.clone(),
        };
        sqlx::query(
            "UPDATE mail_accounts SET email = ?, imap_host = ?, imap_port = ?, imap_security = ?, imap_username = ?, smtp_host = ?, smtp_port = ?,
             smtp_security = ?, smtp_username = ?, password = ?, smtp_password = NULL, last_error = NULL WHERE id = ?",
        )
        .bind(&r.email)
        .bind(&p.imap_host)
        .bind(p.imap_port)
        .bind(&p.imap_security)
        .bind(&r.imap_username)
        .bind(&p.smtp_host)
        .bind(p.smtp_port)
        .bind(&p.smtp_security)
        .bind(&r.smtp_username)
        .bind(&sealed)
        .bind(&a.id)
        .execute(&self.db)
        .await?;
        self.mail.forget(&a.id);
        self.mail_publish(a, &serde_json::json!({"type": "mail.changed", "account_id": a.id, "what": "accounts"}));
        Ok(true)
    }

    /// Whether a login-mode template applies to the user.
    async fn needs_login_password(&self, user: &User) -> Result<bool> {
        let templates: Vec<Provisioning> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {COLS} FROM mail_provisionings WHERE enabled = 1 AND password_mode = 'login'"
        )))
        .fetch_all(&self.db)
        .await?;
        if templates.is_empty() {
            return Ok(false);
        }
        let groups: Vec<String> = self.user_groups(&user.id).await?.into_iter().map(|g| g.id).collect();
        Ok(templates
            .iter()
            .any(|p| matches(&p.match_kind, &p.match_value, user.email.as_deref(), &groups) && render(p, user).is_some()))
    }

    async fn forget_unneeded_login_secrets(&self) -> Result<()> {
        let holders: Vec<String> = sqlx::query_scalar("SELECT user_id FROM mail_login_secrets").fetch_all(&self.db).await?;
        for uid in holders {
            let keep = match self.user(&uid).await {
                Ok(u) => self.needs_login_password(&u).await?,
                Err(_) => false,
            };
            if !keep {
                sqlx::query("DELETE FROM mail_login_secrets WHERE user_id = ?")
                    .bind(&uid)
                    .execute(&self.db)
                    .await?;
            }
        }
        Ok(())
    }

    /// Called when a user proves their password (sign-in, sign-up, password change): keeps it
    /// sealed if a login-mode template needs it, and provisions the user. Never fails the
    /// sign-in; problems are logged.
    pub async fn mail_password_login(&self, user_id: &str, password: &str) {
        let r = async {
            let user = self.user(user_id).await?;
            let has_templates: Option<i64> = sqlx::query_scalar("SELECT 1 FROM mail_provisionings WHERE enabled = 1 LIMIT 1")
                .fetch_optional(&self.db)
                .await?;
            if has_templates.is_none() {
                return Ok(());
            }
            if self.needs_login_password(&user).await? {
                let sealed = self.secret_box().seal(&format!("login:{user_id}"), password)?;
                sqlx::query(
                    "INSERT INTO mail_login_secrets (user_id, password, updated_at) VALUES (?, ?, ?)
                     ON CONFLICT(user_id) DO UPDATE SET password = excluded.password, updated_at = excluded.updated_at",
                )
                .bind(user_id)
                .bind(&sealed)
                .bind(now())
                .execute(&self.db)
                .await?;
            }
            self.mail_provision_all().await.map(|_| ())
        }
        .await;
        if let Err(e) = r {
            tracing::warn!(user = %user_id, error = %e, "mail provisioning at sign-in failed");
        }
    }

    /// The Sieve settings a template gives a provisioned account (`None` when it has none):
    /// the ManageSieve server logs in like IMAP does.
    pub async fn mail_provisioned_sieve(&self, account_id: &str) -> Result<Option<ProvisionedSieve>> {
        let a = self.mail_account_by_id(account_id).await?;
        let Some(pid) = a.provisioning_id.as_deref() else { return Ok(None) };
        let p = self.mail_provisioning(pid).await?;
        if !p.sieve_enabled || p.sieve_host.is_empty() {
            return Ok(None);
        }
        Ok(Some(ProvisionedSieve {
            host: p.sieve_host,
            port: p.sieve_port as u16,
            security: parse_security(&p.sieve_security),
            username: a.imap_username,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(name: &str, email: Option<&str>) -> User {
        User {
            id: format!("id-{name}"),
            username: name.into(),
            display_name: format!("{name} Display"),
            email: email.map(String::from),
            is_admin: false,
            quota_bytes: None,
            ai_consent: false,
            disabled: false,
            created_at: now(),
        }
    }

    fn template() -> Provisioning {
        Provisioning {
            id: "p1".into(),
            name: "Staff".into(),
            match_kind: "domain".into(),
            match_value: "corp.example".into(),
            email_pattern: "%USERID%@mail.corp.example".into(),
            name_pattern: String::new(),
            imap_host: "imap.corp.example".into(),
            imap_port: 993,
            imap_security: "tls".into(),
            imap_username_pattern: "%USERID%*master".into(),
            smtp_host: "smtp.corp.example".into(),
            smtp_port: 587,
            smtp_security: "starttls".into(),
            smtp_username_pattern: "%EMAIL%".into(),
            sieve_enabled: false,
            sieve_host: String::new(),
            sieve_port: 4190,
            sieve_security: "starttls".into(),
            password_mode: "master".into(),
            master_password: None,
            enabled: true,
            created_at: now(),
            updated_at: now(),
            accounts: 0,
            pending: 0,
            master_password_set: false,
        }
    }

    #[test]
    fn patterns() {
        assert_eq!(expand("%USERID%@example.org", "ada", None).as_deref(), Some("ada@example.org"));
        assert_eq!(expand("%EMAIL%", "ada", Some(" Ada@Corp.example ")).as_deref(), Some("Ada@Corp.example"));
        assert_eq!(expand("%EMAIL%", "ada", None), None, "needs an email");
        assert_eq!(expand("%EMAIL%", "ada", Some("")), None);
        assert_eq!(expand("%USERID%*master", "ada", None).as_deref(), Some("ada*master"));
        assert_eq!(expand("static", "ada", None).as_deref(), Some("static"));
        let t = template();
        let r = render(&t, &user("ada", Some("ada@corp.example"))).unwrap();
        assert_eq!(
            r,
            Rendered {
                email: "ada@mail.corp.example".into(),
                name: "ada Display".into(),
                imap_username: "ada*master".into(),
                smtp_username: "ada@corp.example".into()
            }
        );
        // SMTP username pattern needs %EMAIL%; without an email the template can't apply.
        assert!(render(&t, &user("bob", None)).is_none());
        // A pattern that doesn't give an address is refused.
        let mut bad = template();
        bad.email_pattern = "%USERID%".into();
        assert!(render(&bad, &user("ada", Some("ada@corp.example"))).is_none());
    }

    #[test]
    fn matching_rules() {
        let groups = vec!["g-staff".to_string()];
        assert!(matches("domain", "corp.example", Some("ada@CORP.example"), &[]));
        assert!(matches("domain", "@corp.example", Some("ada@corp.example"), &[]));
        assert!(!matches("domain", "corp.example", Some("ada@evilcorp.example"), &[]));
        assert!(!matches("domain", "corp.example", Some("ada@sub.corp.example"), &[]), "exact domains only");
        assert!(!matches("domain", "corp.example", None, &[]));
        assert!(matches("domain", "*", Some("x@any.example"), &[]));
        assert!(!matches("domain", "*", None, &[]));
        assert!(matches("group", "g-staff", None, &groups));
        assert!(!matches("group", "g-other", None, &groups));
        assert!(matches("all", "", None, &[]));
        assert!(!matches("bogus", "", Some("a@b.c"), &groups));
        assert!(check_match("domain", "corp.example").is_ok());
        assert!(check_match("domain", "").is_err());
        assert!(check_match("domain", "not a domain").is_err());
        assert!(check_match("group", " ").is_err());
        assert!(check_mode("plaintext").is_err());
    }
}
