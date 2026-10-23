//! Snoozing: a message (or conversation) leaves its folder for the account's Snoozed folder
//! (created when missing) and comes back, unread, at the chosen time. The schedule is in the
//! database, so it survives restarts; a background task wakes due messages every minute.
//! Messages are found again by Message-ID, since their UID changes with every move.

use super::imap::{quote, uid_set};
use super::store::{Folder, MessageSummary};
use super::{Account, sync};
use crate::util::{new_id, now};
use crate::{Core, Error, Result};
use chrono::{DateTime, Datelike, Duration, NaiveTime, TimeZone, Timelike, Utc, Weekday};
use serde::Serialize;

/// Snooze presets as Nextcloud Mail offers them, in the user's time zone `tz` (offset east
/// of UTC in minutes): later today (+1 h, rounded up), tomorrow at 8, this weekend (Saturday
/// 8:00), next week (Monday 8:00).
pub fn presets(now_utc: DateTime<Utc>, tz_minutes: i32) -> Vec<(&'static str, DateTime<Utc>)> {
    let offset = chrono::FixedOffset::east_opt(tz_minutes * 60).unwrap_or_else(|| chrono::FixedOffset::east_opt(0).expect("UTC"));
    let local = now_utc.with_timezone(&offset);
    let at = |date: chrono::NaiveDate, hour: u32| {
        offset
            .from_local_datetime(&date.and_time(NaiveTime::from_hms_opt(hour, 0, 0).expect("valid time")))
            .single()
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or(now_utc)
    };
    let later = {
        let t = local + Duration::hours(1);
        let floor = t - Duration::minutes(t.minute() as i64) - Duration::seconds(t.second() as i64) - Duration::nanoseconds(t.nanosecond() as i64);
        (if floor == t { t } else { floor + Duration::hours(1) }).with_timezone(&Utc)
    };
    let today = local.date_naive();
    let tomorrow = today.succ_opt().unwrap_or(today);
    let mut saturday = today.succ_opt().unwrap_or(today);
    while saturday.weekday() != Weekday::Sat {
        saturday = saturday.succ_opt().unwrap_or(saturday);
    }
    let mut monday = today.succ_opt().unwrap_or(today);
    while monday.weekday() != Weekday::Mon {
        monday = monday.succ_opt().unwrap_or(monday);
    }
    let mut out = vec![("later_today", later), ("tomorrow", at(tomorrow, 8))];
    // On a weekend "this weekend" is pointless; Nextcloud shows it on weekdays only.
    if !matches!(local.weekday(), Weekday::Sat | Weekday::Sun) {
        out.push(("this_weekend", at(saturday, 8)));
    }
    out.push(("next_week", at(monday, 8)));
    out
}

/// A snoozed message as the Snoozed folder lists it.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Snoozed {
    pub account_id: String,
    pub message_id: String,
    pub subject: String,
    pub src_folder: String,
    pub until: DateTime<Utc>,
}

/// Groups due rows by account (each account is woken with one connection).
pub fn by_account(rows: Vec<Snoozed>) -> Vec<(String, Vec<Snoozed>)> {
    let mut out: Vec<(String, Vec<Snoozed>)> = Vec::new();
    for r in rows {
        match out.iter_mut().find(|(a, _)| *a == r.account_id) {
            Some((_, v)) => v.push(r),
            None => out.push((r.account_id.clone(), vec![r])),
        }
    }
    out
}

impl Core {
    /// Snoozes messages until `until` (in the future, at most a few years).
    pub async fn mail_snooze(&self, user_id: &str, ids: &[String], until: DateTime<Utc>) -> Result<()> {
        if until <= now() + Duration::seconds(30) {
            return Err(Error::bad("pick a time in the future"));
        }
        if until > now() + Duration::days(366 * 5) {
            return Err(Error::bad("snoozing is limited to five years"));
        }
        for (a, f, msgs) in self.group_messages(user_id, ids).await? {
            if msgs.iter().any(|m| m.message_id.is_none()) {
                return Err(Error::bad("a message without a Message-ID can't be snoozed"));
            }
            let dest = self.ensure_role_folder(&a, "snoozed", "Snoozed").await?;
            let src = if f.id == dest.id {
                // Snoozing again from the Snoozed folder: keep where they came from.
                None
            } else {
                Some(f.name.clone())
            };
            let mut tx = self.mail_tx().await?;
            for m in &msgs {
                let mid = m.message_id.clone().unwrap_or_default();
                sqlx::query(
                    "INSERT INTO mail_snoozed (id, account_id, message_id, subject, src_folder, until, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)
                     ON CONFLICT(account_id, message_id) DO UPDATE SET until = excluded.until,
                     src_folder = CASE WHEN ? IS NULL THEN mail_snoozed.src_folder ELSE excluded.src_folder END",
                )
                .bind(new_id())
                .bind(&a.id)
                .bind(&mid)
                .bind(&m.subject)
                .bind(src.as_deref().unwrap_or(&f.name))
                .bind(until)
                .bind(now())
                .bind(&src)
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
            if f.id != dest.id {
                self.move_group(&a, &f, &msgs, &dest).await?;
            }
            self.mail_publish(&a, &serde_json::json!({"type": "mail.changed", "account_id": a.id, "what": "snoozed"}));
        }
        Ok(())
    }

    /// Brings snoozed messages back to their folders now.
    pub async fn mail_unsnooze(&self, user_id: &str, ids: &[String]) -> Result<()> {
        for (a, f, msgs) in self.group_messages(user_id, ids).await? {
            let rows: Vec<Snoozed> = self.snoozed_rows(&a.id, &msgs).await?;
            for (dest_name, group) in group_by_dest(&msgs, &rows) {
                let dest = self.wake_destination(&a, &dest_name).await?;
                if dest.id != f.id {
                    self.move_group(&a, &f, &group, &dest).await?;
                }
            }
            let mids: Vec<&str> = msgs.iter().filter_map(|m| m.message_id.as_deref()).collect();
            sqlx::query("DELETE FROM mail_snoozed WHERE account_id = ? AND message_id IN (SELECT value FROM json_each(?))")
                .bind(&a.id)
                .bind(serde_json::to_string(&mids)?)
                .execute(&self.db)
                .await?;
            self.mail_publish(&a, &serde_json::json!({"type": "mail.changed", "account_id": a.id, "what": "snoozed"}));
        }
        Ok(())
    }

    async fn snoozed_rows(&self, account_id: &str, msgs: &[MessageSummary]) -> Result<Vec<Snoozed>> {
        let mids: Vec<&str> = msgs.iter().filter_map(|m| m.message_id.as_deref()).collect();
        Ok(sqlx::query_as(
            "SELECT account_id, message_id, subject, src_folder, until FROM mail_snoozed WHERE account_id = ? AND message_id IN (SELECT value FROM json_each(?))",
        )
        .bind(account_id)
        .bind(serde_json::to_string(&mids)?)
        .fetch_all(&self.db)
        .await?)
    }

    /// The folder a message wakes up in: where it came from, else the inbox.
    async fn wake_destination(&self, a: &Account, src_folder: &str) -> Result<Folder> {
        let found: Option<Folder> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {} FROM mail_folders WHERE account_id = ? AND name = ? AND selectable = 1",
            super::store::FOLDER_COLS
        )))
        .bind(&a.id)
        .bind(src_folder)
        .fetch_optional(&self.db)
        .await?;
        match found {
            Some(f) => Ok(f),
            None => self
                .folder_by_role(&a.id, "inbox")
                .await?
                .ok_or_else(|| Error::Unavailable("the account has no inbox".into())),
        }
    }

    /// The user's snoozed messages (for the Snoozed folder's "until" labels).
    pub async fn mail_snoozed(&self, user_id: &str) -> Result<Vec<Snoozed>> {
        Ok(sqlx::query_as(
            "SELECT account_id, message_id, subject, src_folder, until FROM mail_snoozed WHERE account_id IN (SELECT value FROM json_each(?)) ORDER BY until",
        )
        .bind(serde_json::to_string(
            &self.mail_accounts_for(user_id).await?.into_iter().map(|a| a.id).collect::<Vec<_>>(),
        )?)
        .fetch_all(&self.db)
        .await?)
    }

    /// Wakes every message whose time has come. Returns how many came back.
    pub async fn wake_snoozed(&self) -> Result<usize> {
        let due: Vec<Snoozed> =
            sqlx::query_as("SELECT account_id, message_id, subject, src_folder, until FROM mail_snoozed WHERE until <= ? ORDER BY until LIMIT 500")
                .bind(now())
                .fetch_all(&self.db)
                .await?;
        let mut woken = 0;
        for (account_id, rows) in by_account(due) {
            let a = match self.mail_account_by_id(&account_id).await {
                Ok(a) => a,
                Err(Error::NotFound) => continue,
                Err(e) => return Err(e),
            };
            match self.wake_account(&a, &rows).await {
                Ok(n) => woken += n,
                Err(e) => tracing::info!(account = %a.id, error = %e, "waking snoozed mail failed; retrying later"),
            }
        }
        Ok(woken)
    }

    async fn wake_account(&self, a: &Account, rows: &[Snoozed]) -> Result<usize> {
        let Some(snoozed) = self.folder_by_role(&a.id, "snoozed").await? else {
            // The folder is gone (deleted elsewhere): nothing left to bring back.
            self.drop_snoozed(&a.id, rows).await?;
            return Ok(0);
        };
        let mut conn = self.imap_conn(a).await?;
        conn.select(&snoozed.name).await?;
        let mut found: Vec<(u32, &Snoozed)> = Vec::new();
        let mut gone: Vec<&Snoozed> = Vec::new();
        for r in rows {
            let uids = conn.uid_search(&format!("HEADER Message-ID {}", quote(&format!("<{}>", r.message_id)))).await?;
            match uids.into_iter().max() {
                Some(u) => found.push((u, r)),
                None => gone.push(r),
            }
        }
        let mut woken = 0;
        let mut dests: Vec<(String, Vec<u32>)> = Vec::new();
        for (uid, r) in &found {
            match dests.iter_mut().find(|(d, _)| *d == r.src_folder) {
                Some((_, v)) => v.push(*uid),
                None => dests.push((r.src_folder.clone(), vec![*uid])),
            }
        }
        for (dest_name, uids) in dests {
            let dest = self.wake_destination(a, &dest_name).await?;
            let set = uid_set(&uids);
            // Back as unread, so it is noticed.
            conn.uid_store(&set, "-FLAGS.SILENT (\\Seen)").await?;
            conn.uid_move(&set, &dest.name).await?;
            woken += uids.len();
            let (core, a2, dest_id) = (self.clone(), a.clone(), dest.id.clone());
            tokio::spawn(async move {
                if let Err(e) = core.sync_mail_folder(&a2, &dest_id).await {
                    tracing::debug!(error = %e, "syncing after waking snoozed mail failed");
                }
            });
        }
        drop(conn);
        let done: Vec<Snoozed> = found.iter().map(|(_, r)| (*r).clone()).chain(gone.into_iter().cloned()).collect();
        self.drop_snoozed(&a.id, &done).await?;
        if woken > 0 {
            let _: sync::SyncReport = self.sync_mail_folder(a, &snoozed.id).await.unwrap_or_default();
            self.mail_publish(a, &serde_json::json!({"type": "mail.changed", "account_id": a.id, "what": "snoozed"}));
        }
        Ok(woken)
    }

    async fn drop_snoozed(&self, account_id: &str, rows: &[Snoozed]) -> Result<()> {
        let mids: Vec<&str> = rows.iter().map(|r| r.message_id.as_str()).collect();
        sqlx::query("DELETE FROM mail_snoozed WHERE account_id = ? AND message_id IN (SELECT value FROM json_each(?))")
            .bind(account_id)
            .bind(serde_json::to_string(&mids)?)
            .execute(&self.db)
            .await?;
        Ok(())
    }
}

/// Messages grouped by the folder they go back to (the inbox for those without a record).
fn group_by_dest(msgs: &[MessageSummary], rows: &[Snoozed]) -> Vec<(String, Vec<MessageSummary>)> {
    let mut out: Vec<(String, Vec<MessageSummary>)> = Vec::new();
    for m in msgs {
        let dest = rows
            .iter()
            .find(|r| Some(r.message_id.as_str()) == m.message_id.as_deref())
            .map(|r| r.src_folder.clone())
            .unwrap_or_else(|| "INBOX".to_string());
        match out.iter_mut().find(|(d, _)| *d == dest) {
            Some((_, v)) => v.push(m.clone()),
            None => out.push((dest, vec![m.clone()])),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn presets_on_a_weekday() {
        // Wednesday 2026-10-21 14:20 in UTC+2.
        let p = presets(utc("2026-10-21T12:20:00Z"), 120);
        let get = |k: &str| p.iter().find(|(n, _)| *n == k).map(|(_, t)| *t);
        assert_eq!(get("later_today"), Some(utc("2026-10-21T14:00:00Z")), "16:00 local: an hour on, rounded up");
        assert_eq!(get("tomorrow"), Some(utc("2026-10-22T06:00:00Z")), "8:00 local");
        assert_eq!(get("this_weekend"), Some(utc("2026-10-24T06:00:00Z")), "Saturday 8:00");
        assert_eq!(get("next_week"), Some(utc("2026-10-26T06:00:00Z")), "Monday 8:00");
    }

    #[test]
    fn presets_on_a_weekend() {
        // Saturday 2026-10-24 09:00 UTC.
        let p = presets(utc("2026-10-24T09:00:00Z"), 0);
        assert!(p.iter().all(|(n, _)| *n != "this_weekend"));
        assert_eq!(p.iter().find(|(n, _)| *n == "next_week").unwrap().1, utc("2026-10-26T08:00:00Z"));
        assert_eq!(p[0].1, utc("2026-10-24T10:00:00Z"), "exactly on the hour stays");
    }

    #[test]
    fn grouping() {
        let row = |a: &str, m: &str, f: &str| Snoozed {
            account_id: a.into(),
            message_id: m.into(),
            subject: String::new(),
            src_folder: f.into(),
            until: utc("2026-10-24T09:00:00Z"),
        };
        let g = by_account(vec![row("a", "1", "INBOX"), row("b", "2", "INBOX"), row("a", "3", "Work")]);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].1.len(), 2);
        assert_eq!(g[1].0, "b");
    }
}
