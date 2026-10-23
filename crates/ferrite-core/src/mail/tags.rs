//! Tags: IMAP keywords on messages, so other clients see the same tags.
//!
//! The five defaults use the keywords Thunderbird and Nextcloud Mail use (`$label1` Important,
//! `$label2` Work, `$label3` Personal, `$label4` To Do, `$label5` Later); they can be renamed
//! and recoloured, not deleted. A custom tag's keyword is derived from its name the way
//! Nextcloud Mail derives it (`$` + the name in modified UTF-7, spaces as `_`, lowercased), with
//! characters that IMAP does not allow in a keyword replaced. Tags belong to the user and apply
//! to all their accounts. `$label1` is also what the importance classifier sets.

use super::imap::uid_set;
use crate::util::{new_id, now};
use crate::{Core, Error, Result};
use serde::{Serialize, Serializer};

/// The Important tag (also the priority inbox's signal).
pub const IMPORTANT: &str = "$label1";
/// Junk markers (RFC 5788 registry).
pub const JUNK: &str = "$junk";
pub const NOT_JUNK: &str = "$notjunk";

/// (keyword, name, colour) of the default tags.
pub const DEFAULT_TAGS: [(&str, &str, &str); 5] = [
    ("$label1", "Important", "#ff7a66"),
    ("$label2", "Work", "#b4a443"),
    ("$label3", "Personal", "#31cc7c"),
    ("$label4", "To Do", "#317ccc"),
    ("$label5", "Later", "#a85bf7"),
];

/// Keywords with a protocol meaning, never offered as tags.
const SYSTEM_KEYWORDS: &[&str] = &[
    "$junk",
    "$notjunk",
    "junk",
    "nonjunk",
    "notjunk",
    "$forwarded",
    "$mdnsent",
    "$submitpending",
    "$submitted",
    "$phishing",
    "$important",
    "$hasattachment",
    "$hasnoattachment",
    "$autosent",
    "$unknown",
];

/// A keyword's longest length (bytes); some servers refuse longer atoms.
const MAX_KEYWORD: usize = 64;

pub fn is_system_keyword(k: &str) -> bool {
    SYSTEM_KEYWORDS.contains(&k.to_ascii_lowercase().as_str())
}

/// The IMAP keyword for a tag name.
pub fn keyword_for(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::bad("a tag needs a name"));
    }
    let encoded = super::folders::encode_mailbox_name(&name.replace(' ', "_"));
    // RFC 3501 atom-specials, wildcards, quoting and `]` are not allowed in a flag keyword.
    let atom: String = encoded
        .chars()
        .map(|c| if c.is_ascii_control() || "(){ %*\"\\]".contains(c) { '_' } else { c })
        .collect();
    let mut kw = format!("${}", atom.to_ascii_lowercase());
    kw.truncate(MAX_KEYWORD);
    if is_system_keyword(&kw) || DEFAULT_TAGS.iter().any(|(k, ..)| *k == kw) {
        return Err(Error::bad(format!("“{name}” is reserved; pick another name")));
    }
    Ok(kw)
}

/// Whether `k` can be stored as a keyword (an IMAP atom without wildcards).
pub fn valid_keyword(k: &str) -> bool {
    !k.is_empty() && k.len() <= MAX_KEYWORD && !k.starts_with('\\') && k.chars().all(|c| c.is_ascii_graphic() && !"(){%*\"\\]".contains(c))
}

/// The cache column for a set of keywords: lowercased, sorted, space-padded.
pub fn keyword_column<S: AsRef<str>>(keywords: &[S]) -> String {
    let mut v: Vec<String> = keywords
        .iter()
        .map(|k| k.as_ref().trim().to_ascii_lowercase())
        .filter(|k| !k.is_empty())
        .collect();
    v.sort();
    v.dedup();
    if v.is_empty() { String::new() } else { format!(" {} ", v.join(" ")) }
}

pub fn keyword_list(column: &str) -> Vec<&str> {
    column.split_whitespace().collect()
}

/// The pattern matching messages with `keyword`, for `keywords LIKE ? ESCAPE '\'`.
pub fn keyword_like(keyword: &str) -> String {
    let escaped = keyword.to_ascii_lowercase().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    format!("% {escaped} %")
}

pub fn has_keyword(column: &str, keyword: &str) -> bool {
    keyword_list(column).iter().any(|k| k.eq_ignore_ascii_case(keyword))
}

pub fn serialize_keywords<S: Serializer>(column: &str, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.collect_seq(keyword_list(column))
}

/// Cached messages of a folder whose keywords differ from what the server reported.
pub(crate) async fn changed_keywords(db: &sqlx::SqlitePool, folder_id: &str, server: &[super::imap::Fetched]) -> Result<Vec<(u32, String)>> {
    let cached: std::collections::HashMap<i64, String> = sqlx::query_as::<_, (i64, String)>("SELECT uid, keywords FROM mail_messages WHERE folder_id = ?")
        .bind(folder_id)
        .fetch_all(db)
        .await?
        .into_iter()
        .collect();
    Ok(server
        .iter()
        .filter_map(|f| {
            let column = keyword_column(&f.keywords);
            match cached.get(&(f.uid as i64)) {
                Some(old) if *old != column => Some((f.uid, column)),
                _ => None,
            }
        })
        .collect())
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Tag {
    pub id: String,
    pub keyword: String,
    pub name: String,
    pub color: String,
    pub is_default: bool,
}

fn valid_color(c: &str) -> Result<String> {
    let c = c.trim().to_ascii_lowercase();
    let ok = c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|x| x.is_ascii_hexdigit());
    if ok { Ok(c) } else { Err(Error::bad("colours are #rrggbb")) }
}

impl Core {
    /// The user's tags (the defaults first), creating the defaults on first use.
    pub async fn mail_tags(&self, user_id: &str) -> Result<Vec<Tag>> {
        for (kw, name, color) in DEFAULT_TAGS {
            sqlx::query("INSERT OR IGNORE INTO mail_tags (id, user_id, keyword, name, color, is_default, created_at) VALUES (?, ?, ?, ?, ?, 1, ?)")
                .bind(new_id())
                .bind(user_id)
                .bind(kw)
                .bind(name)
                .bind(color)
                .bind(now())
                .execute(&self.db)
                .await?;
        }
        Ok(sqlx::query_as(
            "SELECT id, keyword, name, color, is_default FROM mail_tags WHERE user_id = ? ORDER BY is_default DESC, keyword < '$label6' DESC, keyword, lower(name)",
        )
        .bind(user_id)
        .fetch_all(&self.db)
        .await?)
    }

    pub async fn create_mail_tag(&self, user_id: &str, name: &str, color: &str) -> Result<Tag> {
        let name = name.trim();
        if name.chars().count() > 60 {
            return Err(Error::bad("tag names are at most 60 characters"));
        }
        let keyword = keyword_for(name)?;
        let color = valid_color(color)?;
        let tags = self.mail_tags(user_id).await?;
        if tags.iter().any(|t| t.keyword == keyword || t.name.eq_ignore_ascii_case(name)) {
            return Err(Error::conflict(format!("there is already a tag “{name}”")));
        }
        let id = new_id();
        sqlx::query("INSERT INTO mail_tags (id, user_id, keyword, name, color, is_default, created_at) VALUES (?, ?, ?, ?, ?, 0, ?)")
            .bind(&id)
            .bind(user_id)
            .bind(&keyword)
            .bind(name)
            .bind(&color)
            .bind(now())
            .execute(&self.db)
            .await?;
        self.tags_changed(user_id);
        self.mail_tag(user_id, &id).await
    }

    async fn mail_tag(&self, user_id: &str, id: &str) -> Result<Tag> {
        sqlx::query_as("SELECT id, keyword, name, color, is_default FROM mail_tags WHERE id = ? AND user_id = ?")
            .bind(id)
            .bind(user_id)
            .fetch_optional(&self.db)
            .await?
            .ok_or(Error::NotFound)
    }

    /// Renames or recolours a tag. The keyword stays, so messages keep their tag.
    pub async fn update_mail_tag(&self, user_id: &str, id: &str, name: Option<&str>, color: Option<&str>) -> Result<Tag> {
        let tag = self.mail_tag(user_id, id).await?;
        let name = match name.map(str::trim) {
            Some("") => return Err(Error::bad("a tag needs a name")),
            Some(n) if n.chars().count() > 60 => return Err(Error::bad("tag names are at most 60 characters")),
            Some(n) => n.to_string(),
            None => tag.name.clone(),
        };
        let color = match color {
            Some(c) => valid_color(c)?,
            None => tag.color.clone(),
        };
        let clash: Option<String> = sqlx::query_scalar("SELECT id FROM mail_tags WHERE user_id = ? AND lower(name) = lower(?) AND id <> ?")
            .bind(user_id)
            .bind(&name)
            .bind(id)
            .fetch_optional(&self.db)
            .await?;
        if clash.is_some() {
            return Err(Error::conflict(format!("there is already a tag “{name}”")));
        }
        sqlx::query("UPDATE mail_tags SET name = ?, color = ? WHERE id = ?")
            .bind(&name)
            .bind(&color)
            .bind(id)
            .execute(&self.db)
            .await?;
        self.tags_changed(user_id);
        self.mail_tag(user_id, id).await
    }

    /// Deletes a custom tag and takes its keyword off the user's cached messages (on the
    /// server too).
    pub async fn delete_mail_tag(&self, user_id: &str, id: &str) -> Result<()> {
        let tag = self.mail_tag(user_id, id).await?;
        if tag.is_default {
            return Err(Error::bad("the default tags can be renamed but not deleted"));
        }
        let ids: Vec<String> =
            sqlx::query_scalar("SELECT m.id FROM mail_messages m WHERE m.account_id IN (SELECT value FROM json_each(?)) AND m.keywords LIKE ? ESCAPE '\\'")
                .bind(serde_json::to_string(
                    &self.mail_accounts_for(user_id).await?.into_iter().map(|a| a.id).collect::<Vec<_>>(),
                )?)
                .bind(keyword_like(&tag.keyword))
                .fetch_all(&self.db)
                .await?;
        for chunk in ids.chunks(500) {
            if let Err(e) = self.mail_set_keyword(user_id, chunk, &tag.keyword, false).await {
                tracing::info!(error = %e, "removing a deleted tag from messages failed");
            }
        }
        sqlx::query("DELETE FROM mail_tags WHERE id = ?").bind(id).execute(&self.db).await?;
        self.tags_changed(user_id);
        Ok(())
    }

    fn tags_changed(&self, user_id: &str) {
        self.hub.publish([user_id], &serde_json::json!({"type": "mail.changed", "what": "tags"}));
    }

    /// Adds or removes a keyword on messages, on the server and in the cache. Marking
    /// messages important or not teaches the account's importance classifier.
    pub async fn mail_set_keyword(&self, user_id: &str, ids: &[String], keyword: &str, on: bool) -> Result<()> {
        self.set_keyword(user_id, ids, keyword, on, true).await
    }

    /// [`Core::mail_set_keyword`]; `teach` is false for the classifier's own labels.
    pub(crate) async fn set_keyword(&self, user_id: &str, ids: &[String], keyword: &str, on: bool, teach: bool) -> Result<()> {
        let keyword = keyword.trim().to_ascii_lowercase();
        if !valid_keyword(&keyword) {
            return Err(Error::bad(format!("{keyword:?} is not a valid tag keyword")));
        }
        let op = format!("{}FLAGS.SILENT ({keyword})", if on { "+" } else { "-" });
        for (a, f, msgs) in self.group_messages(user_id, ids).await? {
            let uids: Vec<u32> = msgs.iter().map(|m| m.uid as u32).collect();
            let mut conn = self.imap_conn(&a).await?;
            conn.ensure_selected(&f.name).await?;
            conn.uid_store(&uid_set(&uids), &op).await?;
            drop(conn);
            let mut tx = self.mail_tx().await?;
            for m in &msgs {
                let mut list: Vec<&str> = keyword_list(&m.keywords).into_iter().filter(|k| *k != keyword).collect();
                if on {
                    list.push(&keyword);
                }
                sqlx::query("UPDATE mail_messages SET keywords = ? WHERE id = ?")
                    .bind(keyword_column(&list))
                    .bind(&m.id)
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await?;
            if keyword == IMPORTANT && teach {
                self.classifier_taught(&a.id).await;
            }
            self.mail_changed(&a, &[&f.id]);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_from_names() {
        assert_eq!(keyword_for("Project X").unwrap(), "$project_x");
        assert_eq!(keyword_for("  Receipts ").unwrap(), "$receipts");
        // Non-ASCII in modified UTF-7, like Nextcloud Mail (mb_convert_encoding UTF7-IMAP).
        assert_eq!(keyword_for("Büro").unwrap(), "$b&apw-ro");
        // Characters IMAP forbids in keywords are replaced.
        assert_eq!(keyword_for("a(b)*c\"d").unwrap(), "$a_b__c_d");
        assert!(keyword_for("").is_err());
        assert!(keyword_for("Junk").is_err(), "$junk is a protocol keyword");
        let long = keyword_for(&"x".repeat(200)).unwrap();
        assert_eq!(long.len(), 64);
        assert!(valid_keyword(&long));
    }

    #[test]
    fn keyword_columns() {
        assert_eq!(keyword_column(&["Work", "$label1", "work", ""]), " $label1 work ");
        assert_eq!(keyword_column::<&str>(&[]), "");
        let col = keyword_column(&["$label1", "$label4"]);
        assert!(has_keyword(&col, "$LABEL4"));
        assert!(!has_keyword(&col, "$label"));
        assert_eq!(keyword_list(&col), vec!["$label1", "$label4"]);
        assert_eq!(keyword_like("$Label1"), "% $label1 %");
        // LIKE wildcards in a keyword are escaped.
        assert_eq!(keyword_like("a_b%"), "% a\\_b\\% %");
    }

    #[test]
    fn keyword_validity() {
        assert!(valid_keyword("$label1"));
        assert!(valid_keyword("nonjunk"));
        assert!(!valid_keyword("\\Seen"));
        assert!(!valid_keyword("two words"));
        assert!(!valid_keyword("a*"));
        assert!(!valid_keyword(""));
        assert!(is_system_keyword("$Junk"));
        assert!(!is_system_keyword("$label2"));
    }
}
