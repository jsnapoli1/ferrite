//! Per-account settings of the mail integrations: the ManageSieve connection (by default the
//! IMAP host, user name and password), whether the autoresponder follows the user's absence,
//! automatic handling of meeting invitations, and AI follow-up reminders.
//!
//! An account set up from an admin provisioning template with a Sieve server starts with
//! that server. Reading the settings needs [`MailRight::Read`]; changing the ManageSieve
//! connection (it holds credentials) needs [`MailRight::Authenticate`], everything else
//! [`MailRight::Configure`].

use super::delegation::MailRight;
use super::managesieve::SieveConn;
use super::{Account, ServerSettings, parse_security, security_str};
use crate::mailer::SmtpSecurity;
use crate::util::now;
use crate::{Core, Error, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_SIEVE_PORT: u16 = 4190;

#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    sieve_enabled: bool,
    sieve_host: String,
    sieve_port: i64,
    sieve_security: String,
    sieve_username: Option<String>,
    sieve_password: Option<String>,
    ooo_follows_absence: bool,
    imip_auto_update: bool,
    imip_auto_create: bool,
    followup_enabled: bool,
}

/// The settings as the API shows them (never the password).
#[derive(Debug, Clone, Serialize)]
pub struct AccountIntegrations {
    pub account_id: String,
    pub sieve_enabled: bool,
    /// Empty: the IMAP host.
    pub sieve_host: String,
    pub sieve_port: u16,
    pub sieve_security: String,
    /// None: the IMAP user name.
    pub sieve_username: Option<String>,
    /// A separate ManageSieve password is stored (else the IMAP password is used).
    pub sieve_custom_password: bool,
    /// The autoresponder is set from the user's absence (Settings → Profile).
    pub ooo_follows_absence: bool,
    /// Updates and cancellations from organizers change events already in the calendar.
    pub imip_auto_update: bool,
    /// New invitations are put in the calendar as tentative before they are answered.
    pub imip_auto_create: bool,
    /// Sent mail is checked for replies the AI expects (when AI is available).
    pub followup_enabled: bool,
    /// The Sieve server comes from the administrator's provisioning template.
    pub sieve_provisioned: bool,
    #[serde(skip)]
    sealed_password: Option<String>,
}

/// Changes; `None` keeps the current value. An empty user name or password means "the IMAP one".
#[derive(Debug, Clone, Default, Deserialize)]
pub struct IntegrationsInput {
    pub sieve_enabled: Option<bool>,
    pub sieve_host: Option<String>,
    pub sieve_port: Option<u16>,
    pub sieve_security: Option<SmtpSecurity>,
    pub sieve_username: Option<String>,
    #[serde(default, skip_serializing)]
    pub sieve_password: Option<String>,
    pub ooo_follows_absence: Option<bool>,
    pub imip_auto_update: Option<bool>,
    pub imip_auto_create: Option<bool>,
    pub followup_enabled: Option<bool>,
}

impl AccountIntegrations {
    /// The settings a user's explicit "apply" acts with: updates and cancellations apply,
    /// nothing new is created.
    pub(crate) fn manual() -> AccountIntegrations {
        AccountIntegrations {
            account_id: String::new(),
            sieve_enabled: false,
            sieve_host: String::new(),
            sieve_port: DEFAULT_SIEVE_PORT,
            sieve_security: String::new(),
            sieve_username: None,
            sieve_custom_password: false,
            ooo_follows_absence: false,
            imip_auto_update: true,
            imip_auto_create: false,
            followup_enabled: false,
            sieve_provisioned: false,
            sealed_password: None,
        }
    }
}

fn sieve_context(account_id: &str) -> String {
    format!("{account_id}:sieve")
}

impl Core {
    /// An account's integration settings (defaults when never saved).
    pub(crate) async fn integrations_of(&self, a: &Account) -> Result<AccountIntegrations> {
        let row: Option<Row> = sqlx::query_as(
            "SELECT sieve_enabled, sieve_host, sieve_port, sieve_security, sieve_username, sieve_password, ooo_follows_absence,
             imip_auto_update, imip_auto_create, followup_enabled FROM mail_account_integrations WHERE account_id = ?",
        )
        .bind(&a.id)
        .fetch_optional(&self.db)
        .await?;
        let default_security = if a.imap_security == "none" { "none" } else { "starttls" };
        let provisioned = self.mail_provisioned_sieve(&a.id).await?;
        let sieve_provisioned = provisioned.is_some();
        Ok(match row {
            Some(r) => AccountIntegrations {
                account_id: a.id.clone(),
                sieve_enabled: r.sieve_enabled,
                sieve_host: r.sieve_host,
                sieve_port: u16::try_from(r.sieve_port).unwrap_or(DEFAULT_SIEVE_PORT),
                sieve_security: r.sieve_security,
                sieve_username: r.sieve_username,
                sieve_custom_password: r.sieve_password.is_some(),
                ooo_follows_absence: r.ooo_follows_absence,
                imip_auto_update: r.imip_auto_update,
                imip_auto_create: r.imip_auto_create,
                followup_enabled: r.followup_enabled,
                sieve_provisioned,
                sealed_password: r.sieve_password,
            },
            None => AccountIntegrations {
                account_id: a.id.clone(),
                sieve_enabled: sieve_provisioned,
                sieve_host: provisioned.as_ref().map(|p| p.host.clone()).unwrap_or_default(),
                sieve_port: provisioned.as_ref().map_or(DEFAULT_SIEVE_PORT, |p| p.port),
                sieve_security: provisioned.as_ref().map_or(default_security, |p| security_str(p.security)).into(),
                sieve_username: provisioned.as_ref().map(|p| p.username.clone()),
                sieve_custom_password: false,
                ooo_follows_absence: false,
                imip_auto_update: true,
                imip_auto_create: false,
                followup_enabled: true,
                sieve_provisioned,
                sealed_password: None,
            },
        })
    }

    pub async fn mail_integrations(&self, user_id: &str, account_id: &str) -> Result<AccountIntegrations> {
        let a = self.mail_access(user_id, account_id, MailRight::Read).await?;
        self.integrations_of(&a).await
    }

    /// The ManageSieve connection of an account, with the password decrypted. Without a
    /// separate ManageSieve password, an OAuth account signs in with its access token.
    pub(crate) fn sieve_settings(&self, a: &Account, s: &AccountIntegrations) -> Result<ServerSettings> {
        let imap = self.imap_settings(a)?;
        let xoauth2 = imap.xoauth2 && s.sealed_password.is_none();
        let password = match &s.sealed_password {
            Some(sealed) => self.secret_box().open(&sieve_context(&a.id), sealed)?,
            None => imap.password,
        };
        Ok(ServerSettings {
            host: if s.sieve_host.trim().is_empty() {
                a.imap_host.clone()
            } else {
                s.sieve_host.trim().to_string()
            },
            port: s.sieve_port,
            security: parse_security(&s.sieve_security),
            username: s.sieve_username.clone().filter(|u| !u.trim().is_empty()).unwrap_or(imap.username),
            password,
            xoauth2,
        })
    }

    /// A logged-in ManageSieve session for an account with Sieve turned on.
    pub(crate) async fn sieve_conn(&self, a: &Account) -> Result<SieveConn> {
        let s = self.integrations_of(a).await?;
        if !s.sieve_enabled {
            return Err(Error::bad("filters are not turned on for this account (enable Sieve in its filter settings)"));
        }
        self.sieve_connect(a, &s).await
    }

    /// Logs in to ManageSieve with fresh credentials, under the admin's connection policy.
    async fn sieve_connect(&self, a: &Account, s: &AccountIntegrations) -> Result<SieveConn> {
        let a = self.mail_fresh_credentials(a).await?;
        SieveConn::connect(&self.sieve_settings(&a, s)?, self.mail_policy().await).await
    }

    /// Saves settings. Turning Sieve on or changing its connection is tested first; turning on
    /// "follow my absence" applies the absence right away.
    pub async fn set_mail_integrations(&self, user_id: &str, account_id: &str, input: &IntegrationsInput) -> Result<AccountIntegrations> {
        let a = self.mail_access(user_id, account_id, MailRight::Configure).await?;
        let cur = self.integrations_of(&a).await?;
        let mut next = cur.clone();
        if let Some(v) = input.sieve_enabled {
            next.sieve_enabled = v;
        }
        if let Some(h) = &input.sieve_host {
            let h = h.trim();
            if h.contains(char::is_whitespace) || h.contains('/') {
                return Err(Error::bad(format!("{h:?} is not a host name")));
            }
            next.sieve_host = h.to_string();
        }
        if let Some(p) = input.sieve_port {
            if p == 0 {
                return Err(Error::bad("the ManageSieve port must be between 1 and 65535"));
            }
            next.sieve_port = p;
        }
        if let Some(s) = input.sieve_security {
            next.sieve_security = security_str(s).to_string();
        }
        if let Some(u) = &input.sieve_username {
            next.sieve_username = Some(u.trim().to_string()).filter(|u| !u.is_empty());
        }
        if let Some(p) = &input.sieve_password {
            next.sealed_password = if p.is_empty() {
                None
            } else {
                Some(self.secret_box().seal(&sieve_context(&a.id), p)?)
            };
            next.sieve_custom_password = next.sealed_password.is_some();
        }
        for (field, value) in [
            (&mut next.ooo_follows_absence, input.ooo_follows_absence),
            (&mut next.imip_auto_update, input.imip_auto_update),
            (&mut next.imip_auto_create, input.imip_auto_create),
            (&mut next.followup_enabled, input.followup_enabled),
        ] {
            if let Some(v) = value {
                *field = v;
            }
        }
        let connection_changed = next.sieve_host != cur.sieve_host
            || next.sieve_port != cur.sieve_port
            || next.sieve_security != cur.sieve_security
            || next.sieve_username != cur.sieve_username
            || input.sieve_password.is_some();
        if connection_changed {
            // The connection holds credentials: only who may change how the account signs in.
            self.mail_access(user_id, account_id, MailRight::Authenticate).await?;
        }
        if next.sieve_enabled && (!cur.sieve_enabled || connection_changed) {
            let conn = self.sieve_connect(&a, &next).await?;
            conn.logout().await;
        }
        sqlx::query(
            "INSERT INTO mail_account_integrations (account_id, sieve_enabled, sieve_host, sieve_port, sieve_security, sieve_username, sieve_password,
             ooo_follows_absence, imip_auto_update, imip_auto_create, followup_enabled, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(account_id) DO UPDATE SET sieve_enabled = excluded.sieve_enabled, sieve_host = excluded.sieve_host, sieve_port = excluded.sieve_port,
             sieve_security = excluded.sieve_security, sieve_username = excluded.sieve_username, sieve_password = excluded.sieve_password,
             ooo_follows_absence = excluded.ooo_follows_absence, imip_auto_update = excluded.imip_auto_update, imip_auto_create = excluded.imip_auto_create,
             followup_enabled = excluded.followup_enabled, updated_at = excluded.updated_at",
        )
        .bind(&a.id)
        .bind(next.sieve_enabled)
        .bind(&next.sieve_host)
        .bind(i64::from(next.sieve_port))
        .bind(&next.sieve_security)
        .bind(&next.sieve_username)
        .bind(&next.sealed_password)
        .bind(next.ooo_follows_absence)
        .bind(next.imip_auto_update)
        .bind(next.imip_auto_create)
        .bind(next.followup_enabled)
        .bind(now())
        .execute(&self.db)
        .await?;
        if next.ooo_follows_absence && next.sieve_enabled && (!cur.ooo_follows_absence || !cur.sieve_enabled) {
            // The account owner's absence, also when a delegate turns this on.
            self.apply_absence_to_account(&a.user_id, &a).await?;
        }
        self.integrations_of(&a).await
    }
}
