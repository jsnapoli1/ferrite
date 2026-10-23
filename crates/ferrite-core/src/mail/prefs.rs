//! Mail preferences: per user (layout, density, threading, sorting, search), with defaults an
//! administrator can set for everyone, and per-account inbox settings (importance
//! classification, junk handling, trash retention, the folders used for each role).
//!
//! The admin defaults for layout, threading, sorting, density and importance classification
//! are the Mail admin settings' (`ui_defaults`, `importance_default` in `settings`); the other
//! defaults are kept in kv `mail.defaults`.
//!
//! A user's stored preferences only hold what they changed; everything else follows the
//! admin defaults, then the built-in ones.

use super::delegation::MailRight;
use crate::{Core, Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

const DEFAULTS_KEY: &str = "mail.defaults";

/// Roles a user can point at a folder of their choice.
pub const CHOOSABLE_ROLES: [&str; 6] = ["drafts", "sent", "trash", "archive", "junk", "snoozed"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    /// The list alone; a message opens in its place.
    List,
    /// List and message side by side.
    Vertical,
    /// List above the message.
    Horizontal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    Newest,
    Oldest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MailPrefs {
    pub layout: Layout,
    /// No avatars or previews, checkboxes always shown.
    pub compact: bool,
    /// One row per conversation in the list (else one per message; opening a message shows
    /// its conversation either way).
    pub threaded: bool,
    pub sort: Sort,
    /// Starred messages in their own section at the top of the list.
    pub favorites_top: bool,
    pub show_unsubscribed: bool,
    /// Search message bodies too (the cached index and the server's TEXT search).
    pub body_search: bool,
    /// Show the priority inbox (Important / Others) in the folder list.
    pub priority_inbox: bool,
    /// Show the unified inbox across accounts.
    pub all_inboxes: bool,
}

impl Default for MailPrefs {
    fn default() -> Self {
        MailPrefs {
            layout: Layout::Vertical,
            compact: false,
            threaded: false,
            sort: Sort::Newest,
            favorites_top: true,
            show_unsubscribed: false,
            body_search: true,
            priority_inbox: true,
            all_inboxes: true,
        }
    }
}

/// What an administrator sets for everyone: the preferences' defaults and whether importance
/// classification starts on for new accounts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MailDefaults {
    #[serde(flatten)]
    pub prefs: MailPrefs,
    pub classify: bool,
}

impl Default for MailDefaults {
    fn default() -> Self {
        MailDefaults {
            prefs: MailPrefs::default(),
            classify: true,
        }
    }
}

/// Per-account inbox settings as the API shows them (roles as folder ids).
#[derive(Debug, Clone, Serialize)]
pub struct AccountInboxSettings {
    pub classify: bool,
    pub move_junk: bool,
    pub trash_retention_days: i64,
    /// role -> folder id currently used.
    pub roles: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AccountInboxInput {
    pub classify: Option<bool>,
    pub move_junk: Option<bool>,
    pub trash_retention_days: Option<i64>,
    /// role -> folder id (or "" to go back to automatic detection).
    pub roles: Option<BTreeMap<String, String>>,
}

#[derive(sqlx::FromRow)]
pub(crate) struct AccountPrefsRow {
    pub classify: Option<bool>,
    pub move_junk: bool,
    pub trash_retention_days: i64,
    pub roles: String,
    pub namespaces: Option<String>,
}

/// Layers JSON objects key by key (later wins).
pub fn layered(layers: &[&Value]) -> Value {
    let mut out = Map::new();
    for l in layers {
        if let Some(o) = l.as_object() {
            for (k, v) in o {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    Value::Object(out)
}

/// Checks a preferences patch: known keys with values of the right type.
pub fn validate_prefs_patch(patch: &Value) -> Result<Map<String, Value>> {
    let obj = patch.as_object().ok_or_else(|| Error::bad("preferences are an object"))?;
    let base = serde_json::to_value(MailPrefs::default())?;
    if let Some(k) = obj.keys().find(|k| base.get(k.as_str()).is_none()) {
        return Err(Error::bad(format!("unknown preference {k:?}")));
    }
    let merged = layered(&[&base, patch]);
    serde_json::from_value::<MailPrefs>(merged).map_err(|e| Error::bad(format!("invalid preferences: {e}")))?;
    Ok(obj.clone())
}

impl Core {
    /// The administrator's defaults (built-in values where none were set).
    pub async fn mail_defaults(&self) -> Result<MailDefaults> {
        let stored: Value = match self.kv_get(DEFAULTS_KEY).await? {
            Some(s) => serde_json::from_str(&s).unwrap_or(Value::Null),
            None => Value::Null,
        };
        let base = serde_json::to_value(MailDefaults::default())?;
        let mut d: MailDefaults = serde_json::from_value(layered(&[&base, &stored])).unwrap_or_default();
        let ui = self.mail_ui_defaults().await;
        d.prefs.layout = match ui.layout.as_str() {
            "list" => Layout::List,
            "horizontal-split" => Layout::Horizontal,
            _ => Layout::Vertical,
        };
        d.prefs.sort = if ui.sort == "oldest" { Sort::Oldest } else { Sort::Newest };
        d.prefs.threaded = ui.threaded;
        d.prefs.compact = ui.compact;
        d.classify = self.mail_importance_default().await;
        Ok(d)
    }

    /// Changes the administrator's defaults (keys of `patch` only).
    pub async fn set_mail_defaults(&self, patch: &Value) -> Result<MailDefaults> {
        let obj = patch.as_object().ok_or_else(|| Error::bad("defaults are an object"))?;
        let mut prefs_part = obj.clone();
        let classify = prefs_part.remove("classify");
        if classify.as_ref().is_some_and(|c| !c.is_boolean()) {
            return Err(Error::bad("classify is true or false"));
        }
        validate_prefs_patch(&Value::Object(prefs_part))?;
        let current = serde_json::to_value(self.mail_defaults().await?)?;
        let next: MailDefaults = serde_json::from_value(layered(&[&current, patch])).map_err(|e| Error::bad(format!("invalid defaults: {e}")))?;
        // Layout, threading, sorting, density and classification live in the admin settings.
        let ui = super::settings::UiDefaults {
            layout: match next.prefs.layout {
                Layout::List => "list",
                Layout::Horizontal => "horizontal-split",
                Layout::Vertical => "vertical-split",
            }
            .into(),
            threaded: next.prefs.threaded,
            sort: if next.prefs.sort == Sort::Oldest { "oldest" } else { "newest" }.into(),
            compact: next.prefs.compact,
        };
        self.mail_update_settings(super::settings::MailSettingsPatch {
            ui_defaults: Some(ui),
            importance_default: Some(next.classify),
            ..Default::default()
        })
        .await?;
        self.kv_set(DEFAULTS_KEY, &serde_json::to_string(&next)?).await?;
        self.mail_defaults().await
    }

    async fn stored_prefs(&self, user_id: &str) -> Result<Value> {
        let s: Option<String> = sqlx::query_scalar("SELECT prefs FROM mail_prefs WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(&self.db)
            .await?;
        Ok(s.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null))
    }

    /// A user's preferences: their own choices over the admin defaults.
    pub async fn mail_prefs(&self, user_id: &str) -> Result<MailPrefs> {
        let defaults = serde_json::to_value(self.mail_defaults().await?.prefs)?;
        let own = self.stored_prefs(user_id).await?;
        Ok(serde_json::from_value(layered(&[&defaults, &own])).unwrap_or_default())
    }

    /// Saves changed preferences (only the keys in `patch`).
    pub async fn set_mail_prefs(&self, user_id: &str, patch: &Value) -> Result<MailPrefs> {
        let obj = validate_prefs_patch(patch)?;
        let own = self.stored_prefs(user_id).await?;
        let next = layered(&[&own, &Value::Object(obj)]);
        sqlx::query("INSERT INTO mail_prefs (user_id, prefs) VALUES (?, ?) ON CONFLICT(user_id) DO UPDATE SET prefs = excluded.prefs")
            .bind(user_id)
            .bind(serde_json::to_string(&next)?)
            .execute(&self.db)
            .await?;
        self.hub.publish([user_id], &serde_json::json!({"type": "mail.changed", "what": "prefs"}));
        self.mail_prefs(user_id).await
    }

    pub(crate) async fn account_prefs_row(&self, account_id: &str) -> Result<AccountPrefsRow> {
        sqlx::query("INSERT OR IGNORE INTO mail_account_prefs (account_id) VALUES (?)")
            .bind(account_id)
            .execute(&self.db)
            .await?;
        Ok(
            sqlx::query_as("SELECT classify, move_junk, trash_retention_days, roles, namespaces FROM mail_account_prefs WHERE account_id = ?")
                .bind(account_id)
                .fetch_one(&self.db)
                .await?,
        )
    }

    /// Whether importance classification is on for an account.
    pub async fn classify_enabled(&self, account_id: &str) -> Result<bool> {
        Ok(match self.account_prefs_row(account_id).await?.classify {
            Some(v) => v,
            None => self.mail_importance_default().await,
        })
    }

    /// Role -> IMAP folder name chosen by the user.
    pub(crate) async fn role_overrides(&self, account_id: &str) -> Result<BTreeMap<String, String>> {
        Ok(serde_json::from_str(&self.account_prefs_row(account_id).await?.roles).unwrap_or_default())
    }

    pub async fn account_inbox_settings(&self, user_id: &str, account_id: &str) -> Result<AccountInboxSettings> {
        let a = self.mail_access(user_id, account_id, MailRight::Read).await?;
        let row = self.account_prefs_row(&a.id).await?;
        let roles: BTreeMap<String, String> = sqlx::query_as::<_, (String, String)>(
            "SELECT role, id FROM mail_folders WHERE account_id = ? AND role IS NOT NULL AND role <> 'inbox' ORDER BY length(name)",
        )
        .bind(&a.id)
        .fetch_all(&self.db)
        .await?
        .into_iter()
        .rev()
        .collect();
        Ok(AccountInboxSettings {
            classify: match row.classify {
                Some(v) => v,
                None => self.mail_importance_default().await,
            },
            move_junk: row.move_junk,
            trash_retention_days: row.trash_retention_days,
            roles,
        })
    }

    pub async fn set_account_inbox_settings(&self, user_id: &str, account_id: &str, input: &AccountInboxInput) -> Result<AccountInboxSettings> {
        let a = self.mail_access(user_id, account_id, MailRight::Configure).await?;
        let row = self.account_prefs_row(&a.id).await?;
        if let Some(d) = input.trash_retention_days {
            if !(0..=3650).contains(&d) {
                return Err(Error::bad("trash retention is 0 (keep) to 3650 days"));
            }
        }
        let mut roles: BTreeMap<String, String> = serde_json::from_str(&row.roles).unwrap_or_default();
        if let Some(changes) = &input.roles {
            for (role, folder_id) in changes {
                if !CHOOSABLE_ROLES.contains(&role.as_str()) {
                    return Err(Error::bad(format!("{role:?} is not a folder role")));
                }
                if folder_id.is_empty() {
                    roles.remove(role);
                    continue;
                }
                let f = self.folder_row(folder_id).await?;
                if f.account_id != a.id || !f.selectable {
                    return Err(Error::bad("pick a folder of this account"));
                }
                if f.role.as_deref() == Some("inbox") {
                    return Err(Error::bad("the inbox can't take another role"));
                }
                roles.insert(role.clone(), f.name);
            }
        }
        sqlx::query("UPDATE mail_account_prefs SET classify = ?, move_junk = ?, trash_retention_days = ?, roles = ? WHERE account_id = ?")
            .bind(input.classify.or(row.classify))
            .bind(input.move_junk.unwrap_or(row.move_junk))
            .bind(input.trash_retention_days.unwrap_or(row.trash_retention_days))
            .bind(serde_json::to_string(&roles)?)
            .bind(&a.id)
            .execute(&self.db)
            .await?;
        if input.roles.is_some() {
            // Detected roles come back on the next folder sync; apply the choices now.
            self.sync_mail_folders(&a).await?;
        }
        if input.classify == Some(true) {
            self.classifier_taught(&a.id).await;
        }
        self.account_inbox_settings(user_id, &a.id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn layers_and_validation() {
        let base = serde_json::to_value(MailPrefs::default()).unwrap();
        let admin = json!({"layout": "list", "compact": true});
        let user = json!({"compact": false});
        let p: MailPrefs = serde_json::from_value(layered(&[&base, &admin, &user])).unwrap();
        assert_eq!(p.layout, Layout::List, "admin default applies");
        assert!(!p.compact, "the user's choice wins");
        assert!(!p.threaded);
        assert!(validate_prefs_patch(&json!({"sort": "oldest"})).is_ok());
        assert!(validate_prefs_patch(&json!({"sort": "sideways"})).is_err());
        assert!(validate_prefs_patch(&json!({"layout": 3})).is_err());
        assert!(validate_prefs_patch(&json!({"nonsense": true})).is_err());
        assert!(validate_prefs_patch(&json!([1])).is_err());
    }

    #[test]
    fn defaults_round_trip() {
        let d: MailDefaults = serde_json::from_value(json!({"classify": false, "layout": "horizontal"})).unwrap();
        assert!(!d.classify);
        assert_eq!(d.prefs.layout, Layout::Horizontal);
        assert_eq!(d.prefs.sort, Sort::Newest);
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["layout"], "horizontal");
        assert_eq!(v["classify"], false);
    }
}
