//! Per-user lists that change how mail is shown: trusted senders (remote images load without
//! asking) and internal addresses (everything else gets an "external" warning in the reader
//! and before sending), plus the user's mail security settings.
//!
//! Entries are an address (`ada@example.org`) or a whole domain (`example.org`); both match
//! exactly and case-insensitively. A domain entry does not cover its subdomains, so trusting
//! `example.org` does not trust `mail.example.org.attacker.test` or `evil.example.org`.

use super::parse::Addr;
use crate::util::{new_id, now};
use crate::{Core, Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Entries per list and user.
pub const MAX_ENTRIES: i64 = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    Address,
    Domain,
}

impl EntryKind {
    fn as_str(self) -> &'static str {
        match self {
            EntryKind::Address => "address",
            EntryKind::Domain => "domain",
        }
    }
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Entry {
    pub id: String,
    pub kind: String,
    pub value: String,
    pub created_at: DateTime<Utc>,
}

/// Which list: trusted senders or internal addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum List {
    Trusted,
    Internal,
}

impl List {
    fn table(self) -> &'static str {
        match self {
            List::Trusted => "mail_trusted_senders",
            List::Internal => "mail_internal_addresses",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecuritySettings {
    /// Warn about senders and recipients outside the internal addresses.
    pub internal_enabled: bool,
    /// Decrypted S/MIME text is indexed for Mail search (never for global search or the
    /// assistant).
    pub index_decrypted: bool,
    /// The assistant may read decrypted S/MIME text through its mail tools.
    pub assistant_decrypted: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SettingsPatch {
    pub internal_enabled: Option<bool>,
    pub index_decrypted: Option<bool>,
    pub assistant_decrypted: Option<bool>,
}

/// The domain of an address, lower case.
pub fn domain_of(address: &str) -> Option<String> {
    let (local, domain) = address.trim().rsplit_once('@')?;
    let d = domain.trim().trim_end_matches('.').to_lowercase();
    (!local.is_empty() && !d.is_empty()).then_some(d)
}

/// A host name: dot-separated labels of letters, digits and hyphens (IDNs in Unicode are
/// accepted as letters), not starting or ending a label with a hyphen.
pub fn valid_domain(d: &str) -> bool {
    !d.is_empty()
        && d.len() <= 253
        && d.contains('.')
        && d.split('.').all(|l| {
            !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.chars().all(|c| c.is_alphanumeric() || c == '-')
        })
}

/// Normalizes user input into an entry: `@example.org` or `example.org` is a domain,
/// anything with a local part an address.
pub fn normalize(kind: Option<EntryKind>, value: &str) -> Result<(EntryKind, String)> {
    let mut v = value.trim().to_lowercase();
    if v.starts_with('<') && v.ends_with('>') {
        v = v[1..v.len() - 1].trim().to_string();
    }
    let kind = kind.unwrap_or(if v.starts_with('@') || !v.contains('@') {
        EntryKind::Domain
    } else {
        EntryKind::Address
    });
    match kind {
        EntryKind::Domain => {
            let d = v.trim_start_matches('@').trim_end_matches('.').to_string();
            if !valid_domain(&d) {
                return Err(Error::bad(format!("{value:?} is not a domain name")));
            }
            Ok((kind, d))
        }
        EntryKind::Address => {
            let a = Addr::parse(&v)
                .filter(|a| a.address.matches('@').count() == 1)
                .ok_or_else(|| Error::bad(format!("{value:?} is not an email address")))?;
            let d = domain_of(&a.address).ok_or_else(|| Error::bad(format!("{value:?} is not an email address")))?;
            if !valid_domain(&d) {
                return Err(Error::bad(format!("{value:?} is not an email address")));
            }
            Ok((kind, a.address.to_lowercase()))
        }
    }
}

/// Whether `address` is covered by `entries` (exact address or exact domain).
pub fn matches(entries: &[Entry], address: &str) -> Option<Entry> {
    let a = address.trim().to_lowercase();
    let d = domain_of(&a)?;
    entries
        .iter()
        .find(|e| (e.kind == "address" && e.value == a) || (e.kind == "domain" && e.value == d))
        .cloned()
}

impl Core {
    pub async fn mail_list_entries(&self, user_id: &str, list: List) -> Result<Vec<Entry>> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT id, kind, value, created_at FROM {} WHERE user_id = ? ORDER BY kind, value",
            list.table()
        )))
        .bind(user_id)
        .fetch_all(&self.db)
        .await?)
    }

    /// Adds an entry (adding one that exists returns the existing one).
    pub async fn mail_add_entry(&self, user_id: &str, list: List, kind: Option<EntryKind>, value: &str) -> Result<Entry> {
        let (kind, value) = normalize(kind, value)?;
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {} WHERE user_id = ?", list.table())))
            .bind(user_id)
            .fetch_one(&self.db)
            .await?;
        if count >= MAX_ENTRIES {
            return Err(Error::bad(format!("at most {MAX_ENTRIES} entries")));
        }
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO {} (id, user_id, kind, value, created_at) VALUES (?, ?, ?, ?, ?) ON CONFLICT(user_id, kind, value) DO NOTHING",
            list.table()
        )))
        .bind(new_id())
        .bind(user_id)
        .bind(kind.as_str())
        .bind(&value)
        .bind(now())
        .execute(&self.db)
        .await?;
        let e: Entry = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT id, kind, value, created_at FROM {} WHERE user_id = ? AND kind = ? AND value = ?",
            list.table()
        )))
        .bind(user_id)
        .bind(kind.as_str())
        .bind(&value)
        .fetch_one(&self.db)
        .await?;
        self.hub.publish([user_id], &serde_json::json!({"type": "mail.security", "what": "lists"}));
        Ok(e)
    }

    /// Removes an entry by id, or by value when `id` is not one of the user's ids.
    pub async fn mail_remove_entry(&self, user_id: &str, list: List, id_or_value: &str) -> Result<()> {
        let by_id = sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {} WHERE user_id = ? AND id = ?", list.table())))
            .bind(user_id)
            .bind(id_or_value)
            .execute(&self.db)
            .await?;
        if by_id.rows_affected() == 0 {
            let (kind, value) = normalize(None, id_or_value).map_err(|_| Error::NotFound)?;
            let r = sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {} WHERE user_id = ? AND kind = ? AND value = ?",
                list.table()
            )))
            .bind(user_id)
            .bind(kind.as_str())
            .bind(value)
            .execute(&self.db)
            .await?;
            if r.rows_affected() == 0 {
                return Err(Error::NotFound);
            }
        }
        self.hub.publish([user_id], &serde_json::json!({"type": "mail.security", "what": "lists"}));
        Ok(())
    }

    /// The trusted-sender entry covering `address`, if any.
    pub async fn mail_trusted_sender(&self, user_id: &str, address: &str) -> Result<Option<Entry>> {
        Ok(matches(&self.mail_list_entries(user_id, List::Trusted).await?, address))
    }

    pub async fn mail_security_settings(&self, user_id: &str) -> Result<SecuritySettings> {
        let row: Option<(bool, bool, bool)> =
            sqlx::query_as("SELECT internal_enabled, index_decrypted, assistant_decrypted FROM mail_security_settings WHERE user_id = ?")
                .bind(user_id)
                .fetch_optional(&self.db)
                .await?;
        Ok(row
            .map(|(internal_enabled, index_decrypted, assistant_decrypted)| SecuritySettings {
                internal_enabled,
                index_decrypted,
                assistant_decrypted,
            })
            .unwrap_or_default())
    }

    pub async fn mail_update_security_settings(&self, user_id: &str, p: &SettingsPatch) -> Result<SecuritySettings> {
        let cur = self.mail_security_settings(user_id).await?;
        let next = SecuritySettings {
            internal_enabled: p.internal_enabled.unwrap_or(cur.internal_enabled),
            index_decrypted: p.index_decrypted.unwrap_or(cur.index_decrypted),
            assistant_decrypted: p.assistant_decrypted.unwrap_or(cur.assistant_decrypted),
        };
        sqlx::query(
            "INSERT INTO mail_security_settings (user_id, internal_enabled, index_decrypted, assistant_decrypted, updated_at) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(user_id) DO UPDATE SET internal_enabled = excluded.internal_enabled, index_decrypted = excluded.index_decrypted,
             assistant_decrypted = excluded.assistant_decrypted, updated_at = excluded.updated_at",
        )
        .bind(user_id)
        .bind(next.internal_enabled)
        .bind(next.index_decrypted)
        .bind(next.assistant_decrypted)
        .bind(now())
        .execute(&self.db)
        .await?;
        if cur.index_decrypted && !next.index_decrypted {
            self.mail_drop_decrypted_index(user_id).await?;
        }
        self.hub.publish([user_id], &serde_json::json!({"type": "mail.security", "what": "settings"}));
        Ok(next)
    }

    /// Removes the user's decrypted text from the Mail search index (opting out).
    pub(crate) async fn mail_drop_decrypted_index(&self, user_id: &str) -> Result<()> {
        let mut tx = self.db.begin().await?;
        sqlx::query("DELETE FROM mail_decrypted_fts WHERE user_id = ?").bind(user_id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM mail_decrypted_indexed WHERE user_id = ?").bind(user_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Recipients that are outside the user's internal addresses (empty when the check is off
    /// or the list is empty).
    pub async fn mail_external_recipients(&self, user_id: &str, recipients: &[String]) -> Result<Vec<String>> {
        let settings = self.mail_security_settings(user_id).await?;
        let entries = self.mail_list_entries(user_id, List::Internal).await?;
        if !settings.internal_enabled || entries.is_empty() {
            return Ok(vec![]);
        }
        Ok(recipients
            .iter()
            .filter_map(|r| Addr::parse(r))
            .filter(|a| matches(&entries, &a.address).is_none())
            .map(|a| a.address)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: &str, value: &str) -> Entry {
        Entry {
            id: value.into(),
            kind: kind.into(),
            value: value.into(),
            created_at: now(),
        }
    }

    #[test]
    fn normalizes_input() {
        assert_eq!(normalize(None, " Ada@Example.ORG ").unwrap(), (EntryKind::Address, "ada@example.org".into()));
        assert_eq!(normalize(None, "@Example.org").unwrap(), (EntryKind::Domain, "example.org".into()));
        assert_eq!(normalize(None, "example.org.").unwrap(), (EntryKind::Domain, "example.org".into()));
        assert_eq!(normalize(Some(EntryKind::Domain), "münchen.de").unwrap().1, "münchen.de");
        assert_eq!(normalize(None, "Ada <ada@example.org>").unwrap().1, "ada@example.org");
        for bad in ["", "localhost", "exa mple.org", "-x.org", "a@b", "a@-b.org", "x@y@z.org", "<script>.org", "a..b"] {
            assert!(normalize(None, bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(normalize(Some(EntryKind::Address), "example.org").is_err());
    }

    #[test]
    fn matching_is_exact() {
        let list = vec![entry("address", "ada@example.org"), entry("domain", "trusted.test")];
        assert!(matches(&list, "ADA@example.org").is_some());
        assert!(matches(&list, "bob@example.org").is_none());
        assert!(matches(&list, "anyone@Trusted.Test").is_some());
        // Neither subdomains nor look-alike suffixes are covered.
        assert!(matches(&list, "x@mail.trusted.test").is_none());
        assert!(matches(&list, "x@trusted.test.evil.example").is_none());
        assert!(matches(&list, "x@eviltrusted.test").is_none());
        // The address part after the last @ decides.
        assert!(matches(&list, "\"ada@example.org\"@evil.example").is_none());
        assert!(matches(&list, "not-an-address").is_none());
        assert_eq!(domain_of("a@B.Example."), Some("b.example".into()));
    }
}
