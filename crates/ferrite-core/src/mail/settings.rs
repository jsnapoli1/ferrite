//! Administrator settings for Mail (Settings → Administration → Mail), stored as one JSON
//! document in `kv` under `mail_settings` and cached in memory.
//!
//! Other parts of Mail read them through the getters on [`Core`]:
//!
//! | Getter | Setting |
//! |---|---|
//! | [`Core::mail_settings`] | everything (a snapshot) |
//! | [`Core::mail_allow_user_accounts`] | users may add their own accounts |
//! | [`Core::mail_attachment_size_limit`] | attachment size limit in bytes (`None`: no limit) |
//! | [`Core::mail_llm_processing`] | AI ("LLM processing") of mail is allowed |
//! | [`Core::mail_importance_default`] | importance classification is on for users who did not choose |
//! | [`Core::mail_gravatar`] | sender avatars may be looked up on Gravatar |
//! | [`Core::mail_ui_defaults`] | default layout, threading, sort order and density |
//! | [`Core::mail_sync_interval`] | background sync interval |
//!
//! TLS verification and "allow local servers" default to the secure choice; secrets (OAuth
//! client secrets) are sealed with the server secret and never returned.

use super::crypto::SecretBox;
use crate::{Core, Error, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

const KEY: &str = "mail_settings";
/// The background sync interval is kept within these bounds.
pub const MIN_SYNC_INTERVAL: u64 = 60;
pub const MAX_SYNC_INTERVAL: u64 = 24 * 3600;
const DEFAULT_SYNC_INTERVAL: u64 = 900;

fn yes() -> bool {
    true
}

fn default_interval() -> u64 {
    DEFAULT_SYNC_INTERVAL
}

/// Thresholds for the anti-abuse alerts; 0 turns a rule off.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AbuseSettings {
    pub enabled: bool,
    /// Recipients (To, Cc and Bcc) in one message.
    pub max_recipients: u32,
    /// Messages per user in a sliding window.
    pub per_15m: u32,
    pub per_hour: u32,
    pub per_day: u32,
}

/// Defaults for users who have not set their own Mail preferences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiDefaults {
    /// `vertical-split` (three panes), `horizontal-split` (list above the message) or `list`.
    pub layout: String,
    /// Group messages into conversations.
    pub threaded: bool,
    /// `newest` or `oldest` first.
    pub sort: String,
    /// No avatars or previews in the list.
    pub compact: bool,
}

impl Default for UiDefaults {
    fn default() -> Self {
        UiDefaults {
            layout: "vertical-split".into(),
            threaded: true,
            sort: "newest".into(),
            compact: false,
        }
    }
}

/// One OAuth 2.0 provider as the administrator configured it. Endpoints and servers default
/// to the provider's public ones; overriding them serves national clouds and test setups.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OAuthClient {
    pub client_id: String,
    /// Sealed; never serialized to clients (see [`MailSettingsView`]).
    pub client_secret: String,
    /// Microsoft only: the directory (tenant) id, or `common`/`organizations`.
    pub tenant: String,
    pub auth_url: String,
    pub token_url: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_security: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OAuthSettings {
    pub google: OAuthClient,
    pub microsoft: OAuthClient,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MailSettings {
    /// Users may connect their own accounts (provisioned accounts are always available).
    #[serde(default = "yes")]
    pub allow_user_accounts: bool,
    /// "Report spam" forwards the message (as an attachment) here.
    pub spam_report_address: String,
    /// "Not spam" forwards the message here.
    pub ham_report_address: String,
    /// Largest attachment in bytes; `None` means no limit. ([compose] links bigger files.)
    pub attachment_size_limit: Option<u64>,
    pub abuse: AbuseSettings,
    /// Seconds between background syncs of every account.
    #[serde(default = "default_interval")]
    pub sync_interval: u64,
    /// AI features may process mail.
    pub llm_processing: bool,
    /// Importance classification (priority inbox) for users who did not choose.
    #[serde(default = "yes")]
    pub importance_default: bool,
    /// Avatars from Gravatar (sends a hash of sender addresses to Gravatar).
    pub gravatar: bool,
    pub ui_defaults: UiDefaults,
    /// Verify mail servers' TLS certificates.
    #[serde(default = "yes")]
    pub verify_tls: bool,
    /// Allow servers on loopback, private and link-local addresses.
    pub allow_local_servers: bool,
    pub oauth: OAuthSettings,
}

impl Default for MailSettings {
    fn default() -> Self {
        MailSettings {
            allow_user_accounts: true,
            spam_report_address: String::new(),
            ham_report_address: String::new(),
            attachment_size_limit: None,
            abuse: AbuseSettings::default(),
            sync_interval: DEFAULT_SYNC_INTERVAL,
            llm_processing: false,
            importance_default: true,
            gravatar: false,
            ui_defaults: UiDefaults::default(),
            verify_tls: true,
            allow_local_servers: false,
            oauth: OAuthSettings::default(),
        }
    }
}

/// What the admin page shows: the settings without secrets, plus which secrets are set.
#[derive(Debug, Clone, Serialize)]
pub struct MailSettingsView {
    #[serde(flatten)]
    pub settings: MailSettings,
    pub google_secret_set: bool,
    pub microsoft_secret_set: bool,
}

/// A change from the admin page. Every field is optional; `client_secret: ""` keeps the
/// stored secret, `null` clears it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MailSettingsPatch {
    pub allow_user_accounts: Option<bool>,
    pub spam_report_address: Option<String>,
    pub ham_report_address: Option<String>,
    #[serde(default, deserialize_with = "crate::util::double_option")]
    pub attachment_size_limit: Option<Option<u64>>,
    pub abuse: Option<AbuseSettings>,
    pub sync_interval: Option<u64>,
    pub llm_processing: Option<bool>,
    pub importance_default: Option<bool>,
    pub gravatar: Option<bool>,
    pub ui_defaults: Option<UiDefaults>,
    pub verify_tls: Option<bool>,
    pub allow_local_servers: Option<bool>,
    pub google: Option<OAuthClientPatch>,
    pub microsoft: Option<OAuthClientPatch>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct OAuthClientPatch {
    pub client_id: Option<String>,
    #[serde(default, deserialize_with = "crate::util::double_option")]
    pub client_secret: Option<Option<String>>,
    pub tenant: Option<String>,
    pub auth_url: Option<String>,
    pub token_url: Option<String>,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
    pub imap_security: Option<String>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_security: Option<String>,
}

fn valid_address(a: &str) -> Result<String> {
    let a = a.trim();
    if a.is_empty() || super::parse::Addr::parse(a).is_some_and(|x| x.address.contains('@') && x.name.is_empty()) {
        Ok(a.to_string())
    } else {
        Err(Error::bad(format!("{a:?} is not an email address")))
    }
}

fn valid_url(u: &str) -> Result<String> {
    let u = u.trim();
    if u.is_empty() || u.starts_with("https://") || u.starts_with("http://") {
        Ok(u.to_string())
    } else {
        Err(Error::bad(format!("{u:?} is not an http(s) URL")))
    }
}

fn valid_security(s: &str) -> Result<String> {
    match s.trim() {
        "" | "tls" | "starttls" | "none" => Ok(s.trim().to_string()),
        other => Err(Error::bad(format!("unknown security {other:?}; use tls, starttls or none"))),
    }
}

impl OAuthClient {
    fn apply(&mut self, p: OAuthClientPatch, sb: &SecretBox, context: &str) -> Result<()> {
        if let Some(v) = p.client_id {
            self.client_id = v.trim().to_string();
        }
        match p.client_secret {
            Some(Some(s)) if !s.is_empty() => self.client_secret = sb.seal(context, s.trim())?,
            Some(None) => self.client_secret.clear(),
            _ => {}
        }
        if let Some(v) = p.tenant {
            let t = v.trim();
            if !t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.') {
                return Err(Error::bad("the tenant is a directory id, a domain, or common/organizations/consumers"));
            }
            self.tenant = t.to_string();
        }
        if let Some(v) = p.auth_url {
            self.auth_url = valid_url(&v)?;
        }
        if let Some(v) = p.token_url {
            self.token_url = valid_url(&v)?;
        }
        if let Some(v) = p.imap_host {
            self.imap_host = v.trim().to_string();
        }
        if let Some(v) = p.imap_port {
            self.imap_port = v;
        }
        if let Some(v) = p.imap_security {
            self.imap_security = valid_security(&v)?;
        }
        if let Some(v) = p.smtp_host {
            self.smtp_host = v.trim().to_string();
        }
        if let Some(v) = p.smtp_port {
            self.smtp_port = v;
        }
        if let Some(v) = p.smtp_security {
            self.smtp_security = valid_security(&v)?;
        }
        Ok(())
    }
}

impl MailSettings {
    /// Applies a patch (validated); secrets are sealed with `sb`.
    pub fn apply(&mut self, p: MailSettingsPatch, sb: &SecretBox) -> Result<()> {
        if let Some(v) = p.allow_user_accounts {
            self.allow_user_accounts = v;
        }
        if let Some(v) = p.spam_report_address {
            self.spam_report_address = valid_address(&v)?;
        }
        if let Some(v) = p.ham_report_address {
            self.ham_report_address = valid_address(&v)?;
        }
        if let Some(v) = p.attachment_size_limit {
            if v == Some(0) {
                return Err(Error::bad("the attachment size limit must be more than 0 bytes (or empty for no limit)"));
            }
            self.attachment_size_limit = v;
        }
        if let Some(v) = p.abuse {
            self.abuse = v;
        }
        if let Some(v) = p.sync_interval {
            if !(MIN_SYNC_INTERVAL..=MAX_SYNC_INTERVAL).contains(&v) {
                return Err(Error::bad(format!(
                    "the sync interval must be between {MIN_SYNC_INTERVAL} seconds and {} hours",
                    MAX_SYNC_INTERVAL / 3600
                )));
            }
            self.sync_interval = v;
        }
        if let Some(v) = p.llm_processing {
            self.llm_processing = v;
        }
        if let Some(v) = p.importance_default {
            self.importance_default = v;
        }
        if let Some(v) = p.gravatar {
            self.gravatar = v;
        }
        if let Some(v) = p.ui_defaults {
            if !["vertical-split", "horizontal-split", "list"].contains(&v.layout.as_str()) {
                return Err(Error::bad("layout must be vertical-split, horizontal-split or list"));
            }
            if !["newest", "oldest"].contains(&v.sort.as_str()) {
                return Err(Error::bad("sort must be newest or oldest"));
            }
            self.ui_defaults = v;
        }
        if let Some(v) = p.verify_tls {
            self.verify_tls = v;
        }
        if let Some(v) = p.allow_local_servers {
            self.allow_local_servers = v;
        }
        if let Some(g) = p.google {
            self.oauth.google.apply(g, sb, "mail-oauth-google")?;
        }
        if let Some(m) = p.microsoft {
            self.oauth.microsoft.apply(m, sb, "mail-oauth-microsoft")?;
        }
        Ok(())
    }

    pub fn view(&self) -> MailSettingsView {
        let mut s = self.clone();
        let google_secret_set = !s.oauth.google.client_secret.is_empty();
        let microsoft_secret_set = !s.oauth.microsoft.client_secret.is_empty();
        s.oauth.google.client_secret.clear();
        s.oauth.microsoft.client_secret.clear();
        MailSettingsView {
            settings: s,
            google_secret_set,
            microsoft_secret_set,
        }
    }
}

impl Core {
    /// The current settings (cached; loaded once).
    pub async fn mail_settings(&self) -> Arc<MailSettings> {
        if let Some(s) = self.mail.settings.read().clone() {
            return s;
        }
        let loaded = match self.kv_get(KEY).await {
            Ok(Some(json)) => serde_json::from_str(&json).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "mail settings are unreadable; using the defaults");
                MailSettings::default()
            }),
            Ok(None) => MailSettings::default(),
            Err(e) => {
                tracing::warn!(error = %e, "loading mail settings failed; using the defaults");
                return Arc::new(MailSettings::default());
            }
        };
        let s = Arc::new(loaded);
        *self.mail.settings.write() = Some(s.clone());
        s
    }

    /// Applies an administrator's change and returns the new settings.
    pub async fn mail_update_settings(&self, patch: MailSettingsPatch) -> Result<Arc<MailSettings>> {
        let _guard = self.mail.settings_write.lock().await;
        let mut next = (*self.mail_settings().await).clone();
        next.apply(patch, &self.secret_box())?;
        self.kv_set(KEY, &serde_json::to_string(&next)?).await?;
        let s = Arc::new(next);
        *self.mail.settings.write() = Some(s.clone());
        Ok(s)
    }

    pub async fn mail_allow_user_accounts(&self) -> bool {
        self.mail_settings().await.allow_user_accounts
    }

    /// Bytes; `None` when there is no limit.
    pub async fn mail_attachment_size_limit(&self) -> Option<u64> {
        self.mail_settings().await.attachment_size_limit
    }

    pub async fn mail_llm_processing(&self) -> bool {
        self.mail_settings().await.llm_processing
    }

    pub async fn mail_importance_default(&self) -> bool {
        self.mail_settings().await.importance_default
    }

    pub async fn mail_gravatar(&self) -> bool {
        self.mail_settings().await.gravatar
    }

    pub async fn mail_ui_defaults(&self) -> UiDefaults {
        self.mail_settings().await.ui_defaults.clone()
    }

    pub async fn mail_sync_interval(&self) -> Duration {
        Duration::from_secs(self.mail_settings().await.sync_interval.clamp(MIN_SYNC_INTERVAL, MAX_SYNC_INTERVAL))
    }

    /// The decrypted client secret of an OAuth provider (`google` or `microsoft`).
    pub(crate) async fn mail_oauth_secret(&self, provider: &str) -> Result<String> {
        let s = self.mail_settings().await;
        let sealed = match provider {
            "google" => &s.oauth.google.client_secret,
            "microsoft" => &s.oauth.microsoft.client_secret,
            _ => return Err(Error::NotFound),
        };
        if sealed.is_empty() {
            return Ok(String::new());
        }
        self.secret_box().open(&format!("mail-oauth-{provider}"), sealed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_secure_and_partial_documents_load() {
        let d = MailSettings::default();
        assert!(d.verify_tls && !d.allow_local_servers && d.allow_user_accounts && !d.llm_processing && !d.gravatar);
        // The migration writes only one key for existing installations.
        let s: MailSettings = serde_json::from_str(r#"{"allow_local_servers":true}"#).unwrap();
        assert!(s.allow_local_servers && s.verify_tls && s.allow_user_accounts && s.importance_default);
        assert_eq!(s.sync_interval, DEFAULT_SYNC_INTERVAL);
        assert_eq!(s.ui_defaults.layout, "vertical-split");
    }

    #[test]
    fn patches_validate_and_seal_secrets() {
        let sb = SecretBox::new("k");
        let mut s = MailSettings::default();
        let p: MailSettingsPatch = serde_json::from_value(serde_json::json!({
            "spam_report_address": "spam@example.org", "attachment_size_limit": 1048576, "sync_interval": 300,
            "google": {"client_id": "cid", "client_secret": "shh"},
            "abuse": {"enabled": true, "max_recipients": 50, "per_hour": 100},
        }))
        .unwrap();
        s.apply(p, &sb).unwrap();
        assert_eq!(s.spam_report_address, "spam@example.org");
        assert_eq!(s.attachment_size_limit, Some(1048576));
        assert_eq!(s.abuse.per_hour, 100);
        assert!(!s.oauth.google.client_secret.contains("shh"));
        assert_eq!(sb.open("mail-oauth-google", &s.oauth.google.client_secret).unwrap(), "shh");
        let view = serde_json::to_value(s.view()).unwrap();
        assert_eq!(view["google_secret_set"], true);
        assert_eq!(view["oauth"]["google"]["client_secret"], "");
        // An empty secret keeps it; null clears it; no limit is null.
        let keep: MailSettingsPatch = serde_json::from_value(serde_json::json!({"google": {"client_secret": ""}, "attachment_size_limit": null})).unwrap();
        s.apply(keep, &sb).unwrap();
        assert!(!s.oauth.google.client_secret.is_empty());
        assert_eq!(s.attachment_size_limit, None);
        let clear: MailSettingsPatch = serde_json::from_value(serde_json::json!({"google": {"client_secret": null}})).unwrap();
        s.apply(clear, &sb).unwrap();
        assert!(s.oauth.google.client_secret.is_empty());
        for bad in [
            serde_json::json!({"spam_report_address": "not an address"}),
            serde_json::json!({"sync_interval": 5}),
            serde_json::json!({"attachment_size_limit": 0}),
            serde_json::json!({"ui_defaults": {"layout": "sideways"}}),
            serde_json::json!({"microsoft": {"tenant": "a/b"}}),
            serde_json::json!({"google": {"token_url": "file:///etc/passwd"}}),
        ] {
            let p: MailSettingsPatch = serde_json::from_value(bad.clone()).unwrap();
            assert!(s.clone().apply(p, &sb).is_err(), "{bad} should be refused");
        }
    }
}
