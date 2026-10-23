//! Identities and composer settings: aliases (other addresses an account sends as, each with
//! an optional signature of its own), per-account composer settings (default editor mode,
//! the rich signature and whether it goes above or below the quote), per-user sending
//! preferences (the undo-send delay) and the admin's attachment size limit.

use super::Account;
use super::parse::Addr;
use super::richtext;
use crate::util::{new_id, now};
use crate::{Core, Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Key of the admin's attachment size limit (bytes; 0 or missing: no limit).
const ATTACHMENT_LIMIT_KEY: &str = "mail.attachment_size_limit";
/// Undo-send delays the user can choose (seconds; 0 sends at once).
pub const UNDO_SEND_CHOICES: &[i64] = &[0, 5, 10, 20, 30];
pub const DEFAULT_UNDO_SEND_SECONDS: i64 = 10;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Alias {
    pub id: String,
    pub account_id: String,
    pub name: String,
    pub email: String,
    /// Own text signature; `None` uses the account's.
    pub signature: Option<String>,
    pub signature_html: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AliasInput {
    pub name: Option<String>,
    pub email: Option<String>,
    /// Rich signature; `Some("")` removes the alias's own signature.
    pub signature_html: Option<String>,
}

/// Composer settings of an account.
#[derive(Debug, Clone, Serialize)]
pub struct ComposePrefs {
    pub account_id: String,
    /// `plaintext` or `richtext`.
    pub editor_mode: String,
    /// The signature as HTML (the text one converted when there is no rich version).
    pub signature_html: String,
    pub signature_text: String,
    pub signature_above_quote: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ComposePrefsInput {
    pub editor_mode: Option<String>,
    /// The rich signature (sanitized; its text rendering becomes the text signature).
    pub signature_html: Option<String>,
    pub signature_above_quote: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SendPrefs {
    pub undo_send_seconds: i64,
}

/// Who a message is from, and the signature that goes with it.
#[derive(Debug, Clone, Serialize)]
pub struct Identity {
    pub alias_id: Option<String>,
    pub name: String,
    pub email: String,
    pub signature_text: String,
    pub signature_html: String,
}

impl Identity {
    /// `Name <address>` for the From header.
    pub fn sender(&self) -> String {
        Addr {
            name: self.name.clone(),
            address: self.email.clone(),
        }
        .display()
    }
}

/// A signature from its stored forms: the rich one while it still matches the text one (the
/// text signature can also be edited in the account dialog), else the text one converted.
fn signature_pair(text: &str, html: Option<&str>) -> (String, String) {
    let text = text.trim_end().to_string();
    match html.filter(|h| richtext::html_to_text(h).trim() == text.trim()) {
        Some(h) if !text.trim().is_empty() => (text, richtext::signature_block(Some(h), "")),
        _ => {
            let block = richtext::signature_block(None, &text);
            (text, block)
        }
    }
}

/// Sanitizes a rich signature: (html, text), both empty when it has no content.
fn clean_signature(html: &str) -> (Option<String>, String) {
    let clean = richtext::sanitize(html);
    if !richtext::has_content(&clean) {
        return (None, String::new());
    }
    let text = richtext::html_to_text(&clean);
    (Some(clean), text)
}

impl Core {
    pub async fn mail_compose_prefs(&self, user_id: &str, account_id: &str) -> Result<ComposePrefs> {
        let a = self.mail_account(user_id, account_id).await?;
        self.compose_prefs_of(&a).await
    }

    pub(crate) async fn compose_prefs_of(&self, a: &Account) -> Result<ComposePrefs> {
        let row: Option<(String, Option<String>, bool)> =
            sqlx::query_as("SELECT editor_mode, signature_html, signature_above_quote FROM mail_compose_prefs WHERE account_id = ?")
                .bind(&a.id)
                .fetch_optional(&self.db)
                .await?;
        let (editor_mode, html, above) = row.unwrap_or_else(|| ("plaintext".into(), None, true));
        let (signature_text, signature_html) = signature_pair(&a.signature, html.as_deref());
        Ok(ComposePrefs {
            account_id: a.id.clone(),
            editor_mode,
            signature_html,
            signature_text,
            signature_above_quote: above,
        })
    }

    /// Changes an account's composer settings. A new rich signature also replaces the text
    /// signature with its plain rendering.
    pub async fn set_mail_compose_prefs(&self, user_id: &str, account_id: &str, input: &ComposePrefsInput) -> Result<ComposePrefs> {
        let a = self.mail_account(user_id, account_id).await?;
        let cur: Option<(String, Option<String>, bool)> =
            sqlx::query_as("SELECT editor_mode, signature_html, signature_above_quote FROM mail_compose_prefs WHERE account_id = ?")
                .bind(&a.id)
                .fetch_optional(&self.db)
                .await?;
        let (mut mode, mut html, mut above) = cur.unwrap_or_else(|| ("plaintext".into(), None, true));
        if let Some(m) = &input.editor_mode {
            if !matches!(m.as_str(), "plaintext" | "richtext") {
                return Err(Error::bad("the editor mode is plaintext or richtext"));
            }
            mode = m.clone();
        }
        if let Some(a) = input.signature_above_quote {
            above = a;
        }
        if let Some(h) = &input.signature_html {
            let (clean, text) = clean_signature(h);
            html = clean;
            sqlx::query("UPDATE mail_accounts SET signature = ? WHERE id = ?")
                .bind(&text)
                .bind(&a.id)
                .execute(&self.db)
                .await?;
        }
        sqlx::query(
            "INSERT INTO mail_compose_prefs (account_id, editor_mode, signature_html, signature_above_quote) VALUES (?, ?, ?, ?)
             ON CONFLICT(account_id) DO UPDATE SET editor_mode = excluded.editor_mode, signature_html = excluded.signature_html,
             signature_above_quote = excluded.signature_above_quote",
        )
        .bind(&a.id)
        .bind(&mode)
        .bind(&html)
        .bind(above)
        .execute(&self.db)
        .await?;
        self.hub
            .publish([user_id], &json!({"type": "mail.changed", "account_id": a.id, "what": "accounts"}));
        self.mail_compose_prefs(user_id, account_id).await
    }

    pub async fn mail_aliases(&self, user_id: &str, account_id: &str) -> Result<Vec<Alias>> {
        let a = self.mail_account(user_id, account_id).await?;
        Ok(sqlx::query_as(
            "SELECT id, account_id, name, email, signature, signature_html, created_at FROM mail_aliases WHERE account_id = ? ORDER BY lower(email)",
        )
        .bind(&a.id)
        .fetch_all(&self.db)
        .await?)
    }

    /// An alias of one of the user's accounts (with its account).
    pub async fn mail_alias(&self, user_id: &str, alias_id: &str) -> Result<(Account, Alias)> {
        let alias: Alias = sqlx::query_as("SELECT id, account_id, name, email, signature, signature_html, created_at FROM mail_aliases WHERE id = ?")
            .bind(alias_id)
            .fetch_optional(&self.db)
            .await?
            .ok_or(Error::NotFound)?;
        let a = self.mail_account(user_id, &alias.account_id).await?;
        Ok((a, alias))
    }

    pub async fn create_mail_alias(&self, user_id: &str, account_id: &str, input: &AliasInput) -> Result<Alias> {
        let a = self.mail_account(user_id, account_id).await?;
        let email = input.email.as_deref().map(str::trim).unwrap_or("").to_string();
        if Addr::parse(&email).is_none_or(|x| x.address != email) {
            return Err(Error::bad(format!("{email:?} is not an email address")));
        }
        if email.eq_ignore_ascii_case(&a.email) {
            return Err(Error::bad("that is the account's own address"));
        }
        let name = input.name.as_deref().map(str::trim).unwrap_or("").to_string();
        let (html, text) = match &input.signature_html {
            Some(h) => clean_signature(h),
            None => (None, String::new()),
        };
        let id = new_id();
        let r = sqlx::query("INSERT INTO mail_aliases (id, account_id, name, email, signature, signature_html, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
            .bind(&id)
            .bind(&a.id)
            .bind(&name)
            .bind(&email)
            .bind(html.as_ref().map(|_| text))
            .bind(&html)
            .bind(now())
            .execute(&self.db)
            .await;
        if let Err(sqlx::Error::Database(e)) = &r {
            if e.is_unique_violation() {
                return Err(Error::conflict(format!("{email} is already an alias of this account")));
            }
        }
        r?;
        self.hub
            .publish([user_id], &json!({"type": "mail.changed", "account_id": a.id, "what": "accounts"}));
        Ok(self.mail_alias(user_id, &id).await?.1)
    }

    pub async fn update_mail_alias(&self, user_id: &str, alias_id: &str, input: &AliasInput) -> Result<Alias> {
        let (a, cur) = self.mail_alias(user_id, alias_id).await?;
        let email = match &input.email {
            Some(e) => {
                let e = e.trim().to_string();
                if Addr::parse(&e).is_none_or(|x| x.address != e) {
                    return Err(Error::bad(format!("{e:?} is not an email address")));
                }
                if e.eq_ignore_ascii_case(&a.email) {
                    return Err(Error::bad("that is the account's own address"));
                }
                e
            }
            None => cur.email.clone(),
        };
        let name = input.name.as_deref().map(str::trim).map(String::from).unwrap_or(cur.name.clone());
        let (html, text) = match &input.signature_html {
            Some(h) => {
                let (html, text) = clean_signature(h);
                (html.clone(), html.map(|_| text))
            }
            None => (cur.signature_html.clone(), cur.signature.clone()),
        };
        let r = sqlx::query("UPDATE mail_aliases SET name = ?, email = ?, signature = ?, signature_html = ? WHERE id = ?")
            .bind(&name)
            .bind(&email)
            .bind(&text)
            .bind(&html)
            .bind(&cur.id)
            .execute(&self.db)
            .await;
        if let Err(sqlx::Error::Database(e)) = &r {
            if e.is_unique_violation() {
                return Err(Error::conflict(format!("{email} is already an alias of this account")));
            }
        }
        r?;
        self.hub
            .publish([user_id], &json!({"type": "mail.changed", "account_id": a.id, "what": "accounts"}));
        Ok(self.mail_alias(user_id, alias_id).await?.1)
    }

    pub async fn delete_mail_alias(&self, user_id: &str, alias_id: &str) -> Result<()> {
        let (a, alias) = self.mail_alias(user_id, alias_id).await?;
        sqlx::query("DELETE FROM mail_aliases WHERE id = ?").bind(&alias.id).execute(&self.db).await?;
        self.hub
            .publish([user_id], &json!({"type": "mail.changed", "account_id": a.id, "what": "accounts"}));
        Ok(())
    }

    /// The identity a message of `a` is sent as: one of its aliases, or the account itself.
    pub async fn mail_identity(&self, a: &Account, alias_id: Option<&str>) -> Result<Identity> {
        let prefs = self.compose_prefs_of(a).await?;
        if let Some(id) = alias_id.filter(|s| !s.is_empty()) {
            let alias: Alias =
                sqlx::query_as("SELECT id, account_id, name, email, signature, signature_html, created_at FROM mail_aliases WHERE id = ? AND account_id = ?")
                    .bind(id)
                    .bind(&a.id)
                    .fetch_optional(&self.db)
                    .await?
                    .ok_or_else(|| Error::bad("that alias does not belong to this account"))?;
            let (signature_text, signature_html) = match (&alias.signature, &alias.signature_html) {
                (Some(t), h) => signature_pair(t, h.as_deref()),
                (None, _) => (prefs.signature_text, prefs.signature_html),
            };
            return Ok(Identity {
                alias_id: Some(alias.id),
                name: if alias.name.is_empty() { a.name.clone() } else { alias.name },
                email: alias.email,
                signature_text,
                signature_html,
            });
        }
        Ok(Identity {
            alias_id: None,
            name: a.name.clone(),
            email: a.email.clone(),
            signature_text: prefs.signature_text,
            signature_html: prefs.signature_html,
        })
    }

    /// The alias of `a` among `addresses` (a reply goes out from the address it was sent to).
    pub(crate) async fn alias_for_addresses(&self, a: &Account, addresses: &[&str]) -> Result<Option<String>> {
        let aliases: Vec<(String, String)> = sqlx::query_as("SELECT id, email FROM mail_aliases WHERE account_id = ?")
            .bind(&a.id)
            .fetch_all(&self.db)
            .await?;
        Ok(aliases
            .into_iter()
            .find(|(_, e)| addresses.iter().any(|x| x.trim().eq_ignore_ascii_case(e)))
            .map(|(id, _)| id))
    }

    pub async fn mail_send_prefs(&self, user_id: &str) -> Result<SendPrefs> {
        let undo: Option<i64> = sqlx::query_scalar("SELECT undo_send_seconds FROM mail_send_prefs WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(&self.db)
            .await?;
        Ok(SendPrefs {
            undo_send_seconds: undo.unwrap_or(DEFAULT_UNDO_SEND_SECONDS),
        })
    }

    pub async fn set_mail_send_prefs(&self, user_id: &str, undo_send_seconds: i64) -> Result<SendPrefs> {
        if !UNDO_SEND_CHOICES.contains(&undo_send_seconds) {
            return Err(Error::bad("undo send waits 0, 5, 10, 20 or 30 seconds"));
        }
        sqlx::query(
            "INSERT INTO mail_send_prefs (user_id, undo_send_seconds) VALUES (?, ?) ON CONFLICT(user_id) DO UPDATE SET undo_send_seconds = excluded.undo_send_seconds",
        )
        .bind(user_id)
        .bind(undo_send_seconds)
        .execute(&self.db)
        .await?;
        self.mail_send_prefs(user_id).await
    }

    /// The admin's limit for a single attachment in bytes (0: none). Larger files go out as
    /// share links.
    pub async fn mail_attachment_limit(&self) -> Result<i64> {
        Ok(self.kv_get(ATTACHMENT_LIMIT_KEY).await?.and_then(|v| v.parse().ok()).unwrap_or(0).max(0))
    }

    pub async fn set_mail_attachment_limit(&self, bytes: i64) -> Result<i64> {
        if bytes < 0 {
            return Err(Error::bad("the attachment size limit cannot be negative"));
        }
        self.kv_set(ATTACHMENT_LIMIT_KEY, &bytes.to_string()).await?;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_prefer_the_rich_version_while_it_matches() {
        let (t, h) = signature_pair("Alice *A.*", Some("<div>Alice <b>A.</b></div>"));
        assert_eq!(t, "Alice *A.*");
        assert!(h.contains("<b>A.</b>") && h.contains("ferrite-signature"), "{h}");
        // The text signature was edited elsewhere: it wins.
        let (_, h) = signature_pair("Alice Archer", Some("<div>Alice <b>A.</b></div>"));
        assert!(h.contains("Alice Archer") && !h.contains("<b>"), "{h}");
        assert_eq!(signature_pair("", None), (String::new(), String::new()));
        assert_eq!(clean_signature("<p><br></p>"), (None, String::new()));
        let (html, text) = clean_signature("<p>Bob <script>x</script><i>B</i></p>");
        assert_eq!((html.as_deref(), text.as_str()), (Some("<p>Bob <i>B</i></p>"), "Bob /B/"));
    }
}
