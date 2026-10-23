//! The outbox: the web composer queues messages here instead of sending them itself. A
//! background sender sends each one when it is due: right away after the user's undo-send
//! delay, or at the time chosen for "send later". Failures are retried with backoff
//! (1, 5, 15, 60, 240 minutes) and shown in the outbox; a message the server refuses for
//! good (a bad address, a permanent SMTP rejection) waits there until the user edits it,
//! sends it again or deletes it.
//!
//! Attachments and inline images are copied into `mail_outbox_files` when a message is
//! queued, so a message scheduled for next week does not depend on composer uploads (kept a
//! day) or on files that may change meanwhile. Undo sets a message aside (no send time) and
//! reopens it in the composer.

use super::compose::{AttachmentRef, Compose};
use super::richtext;
use crate::util::{new_id, now};
use crate::{Core, Error, Result};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Weak};
use std::time::Duration;
use tokio::sync::Notify;

/// Sending stops being retried after this many failures.
pub const MAX_ATTEMPTS: i64 = 6;
/// The sender looks at the queue at least this often.
const IDLE_WAIT: Duration = Duration::from_secs(30);
/// Messages sent at the same time.
const BATCH: i64 = 20;

/// Senders to wake when a message is queued (one per running core).
static WAKERS: LazyLock<Mutex<Vec<Weak<Notify>>>> = LazyLock::new(|| Mutex::new(Vec::new()));

fn wake_senders() {
    let mut w = WAKERS.lock();
    w.retain(|n| match n.upgrade() {
        Some(n) => {
            n.notify_one();
            true
        }
        None => false,
    });
}

/// How long to wait before attempt `attempts + 1` (after `attempts` failures); `None` once
/// it is time to give up.
pub fn retry_delay(attempts: i64) -> Option<chrono::Duration> {
    let minutes = match attempts {
        ..=0 => 0,
        1 => 1,
        2 => 5,
        3 => 15,
        4 => 60,
        5 => 240,
        _ => return None,
    };
    Some(chrono::Duration::minutes(minutes))
}

/// Whether a failure is worth retrying (the server was unreachable or busy, not "no").
fn transient(e: &Error) -> bool {
    match e {
        Error::BadRequest(_) | Error::NotFound | Error::Forbidden => false,
        Error::Unavailable(m) => !m.contains("permanent error"),
        _ => true,
    }
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct OutboxEntry {
    pub id: String,
    pub account_id: String,
    pub subject: String,
    #[sqlx(json)]
    pub recipients: Vec<String>,
    /// When it goes out; `None` while it is set aside (undone, or failed for good).
    pub send_at: Option<DateTime<Utc>>,
    /// queued, sending or failed
    pub status: String,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A queued message with its content, for editing.
#[derive(Debug, Clone, Serialize)]
pub struct OutboxMessage {
    #[serde(flatten)]
    pub entry: OutboxEntry,
    pub compose: Compose,
    pub attachment_names: Vec<(String, usize)>,
}

const COLS: &str = "id, account_id, subject, recipients, send_at, status, attempts, last_error, next_attempt_at, created_at, updated_at";

fn outbox_image_url(outbox_id: &str, file_id: &str) -> String {
    format!("/api/mail/outbox/{outbox_id}/files/{file_id}")
}

impl Core {
    pub async fn mail_outbox(&self, user_id: &str) -> Result<Vec<OutboxEntry>> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {COLS} FROM mail_outbox WHERE user_id = ? ORDER BY COALESCE(send_at, created_at)"
        )))
        .bind(user_id)
        .fetch_all(&self.db)
        .await?)
    }

    async fn outbox_entry(&self, user_id: &str, id: &str) -> Result<OutboxEntry> {
        sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {COLS} FROM mail_outbox WHERE id = ? AND user_id = ?")))
            .bind(id)
            .bind(user_id)
            .fetch_optional(&self.db)
            .await?
            .ok_or(Error::NotFound)
    }

    /// A queued message with its content (attachments as references to its own copies).
    pub async fn mail_outbox_message(&self, user_id: &str, id: &str) -> Result<OutboxMessage> {
        let entry = self.outbox_entry(user_id, id).await?;
        let raw: String = sqlx::query_scalar("SELECT compose FROM mail_outbox WHERE id = ?")
            .bind(id)
            .fetch_one(&self.db)
            .await?;
        let compose: Compose = serde_json::from_str(&raw)?;
        let files: Vec<(String, String, i64)> = sqlx::query_as("SELECT id, name, size FROM mail_outbox_files WHERE outbox_id = ?")
            .bind(id)
            .fetch_all(&self.db)
            .await?;
        let attachment_names = compose
            .attachments
            .iter()
            .map(|r| match r {
                AttachmentRef::Outbox { id } => files
                    .iter()
                    .find(|f| &f.0 == id)
                    .map(|f| (f.1.clone(), f.2 as usize))
                    .unwrap_or_else(|| ("attachment".into(), 0)),
                _ => ("attachment".into(), 0),
            })
            .collect();
        Ok(OutboxMessage {
            entry,
            compose,
            attachment_names,
        })
    }

    /// One file kept with a queued message: (name, MIME type, bytes).
    pub async fn mail_outbox_file(&self, user_id: &str, id: &str, file_id: &str) -> Result<(String, String, Vec<u8>)> {
        self.outbox_entry(user_id, id).await?;
        sqlx::query_as("SELECT name, mime, data FROM mail_outbox_files WHERE id = ? AND outbox_id = ?")
            .bind(file_id)
            .bind(id)
            .fetch_optional(&self.db)
            .await?
            .ok_or(Error::NotFound)
    }

    /// Copies a message's attachments and inline images into the outbox entry `id` and
    /// returns the message pointing at those copies. References to the entry's own copies are
    /// kept; copies no longer referenced are removed.
    async fn store_outbox_files(&self, user_id: &str, id: &str, c: &Compose) -> Result<Compose> {
        let mut out = c.clone();
        let mut keep: Vec<String> = Vec::new();
        let own: Vec<String> = sqlx::query_scalar("SELECT id FROM mail_outbox_files WHERE outbox_id = ?")
            .bind(id)
            .fetch_all(&self.db)
            .await?;
        let insert = |name: String, mime: String, data: Vec<u8>| {
            let fid = new_id();
            let db = self.db.clone();
            let oid = id.to_string();
            let f2 = fid.clone();
            async move {
                sqlx::query("INSERT INTO mail_outbox_files (id, outbox_id, name, mime, size, data) VALUES (?, ?, ?, ?, ?, ?)")
                    .bind(&f2)
                    .bind(&oid)
                    .bind(&name)
                    .bind(&mime)
                    .bind(data.len() as i64)
                    .bind(&data)
                    .execute(&db)
                    .await
                    .map(|_| f2)
            }
        };
        out.attachments.clear();
        for r in &c.attachments {
            match r {
                AttachmentRef::Outbox { id: fid } if own.contains(fid) => {
                    keep.push(fid.clone());
                    out.attachments.push(r.clone());
                }
                _ => {
                    let (name, mime, data) = self.attachment_data(user_id, r).await?;
                    let fid = insert(name, mime, data).await?;
                    out.attachments.push(AttachmentRef::Outbox { id: fid });
                }
            }
        }
        if let Some(html) = &c.html {
            let clean = richtext::sanitize(html);
            let mut map: HashMap<String, String> = HashMap::new();
            let own_prefix = format!("/api/mail/outbox/{id}/files/");
            for src in richtext::image_sources(&clean) {
                if let Some(fid) = src.strip_prefix(&own_prefix) {
                    if own.iter().any(|o| o == fid) {
                        keep.push(fid.to_string());
                        continue;
                    }
                }
                let found = self.image_for_outbox(user_id, &src).await?;
                if let Some((name, mime, data)) = found {
                    let fid = insert(name, mime, data).await?;
                    map.insert(src.clone(), outbox_image_url(id, &fid));
                }
            }
            out.html = Some(richtext::rewrite_images(&clean, |src| {
                Some(map.get(src).cloned().unwrap_or_else(|| src.to_string()))
            }));
        }
        let unused: Vec<&String> = own.iter().filter(|f| !keep.contains(f)).collect();
        if !unused.is_empty() {
            sqlx::query("DELETE FROM mail_outbox_files WHERE outbox_id = ? AND id IN (SELECT value FROM json_each(?))")
                .bind(id)
                .bind(serde_json::to_string(&unused)?)
                .execute(&self.db)
                .await?;
        }
        Ok(out)
    }

    /// The bytes of an inline image source for copying into the outbox.
    async fn image_for_outbox(&self, user_id: &str, src: &str) -> Result<Option<(String, String, Vec<u8>)>> {
        if let Some(rest) = src.strip_prefix("/api/mail/uploads/").and_then(|r| r.strip_suffix("/content")) {
            return Ok(self.mail_upload_content(user_id, rest).await.ok());
        }
        if let Some(rest) = src.strip_prefix("/api/mail/outbox/") {
            if let Some((_, fid)) = rest.split_once("/files/") {
                return Ok(self.attachment_data(user_id, &AttachmentRef::Outbox { id: fid.to_string() }).await.ok());
            }
        }
        if let Some(rest) = src.strip_prefix("/api/mail/messages/") {
            if let Some((mid, cid)) = rest.split_once("/cid/") {
                let cid = percent_decode(cid);
                return Ok(self.mail_inline_part(user_id, mid, &cid).await.ok());
            }
        }
        // data: images stay embedded until the message is built.
        Ok(None)
    }

    /// The send time for a message: the chosen time, never earlier than now plus the user's
    /// undo-send delay (so "send" can still be undone).
    async fn send_time(&self, user_id: &str, send_at: Option<DateTime<Utc>>) -> Result<DateTime<Utc>> {
        let undo = self.mail_send_prefs(user_id).await?.undo_send_seconds;
        let earliest = now() + chrono::Duration::seconds(undo);
        Ok(match send_at {
            Some(t) if t > earliest => t,
            _ => earliest,
        })
    }

    async fn write_outbox(&self, user_id: &str, id: &str, account_id: &str, c: &Compose, send_at: Option<DateTime<Utc>>) -> Result<()> {
        let recipients: Vec<&String> = c.to.iter().chain(&c.cc).chain(&c.bcc).collect();
        sqlx::query(
            "UPDATE mail_outbox SET account_id = ?, compose = ?, subject = ?, recipients = ?, send_at = ?, status = 'queued', attempts = 0,
             last_error = NULL, next_attempt_at = NULL, updated_at = ? WHERE id = ? AND user_id = ?",
        )
        .bind(account_id)
        .bind(serde_json::to_string(c)?)
        .bind(c.subject.trim())
        .bind(serde_json::to_string(&recipients)?)
        .bind(send_at)
        .bind(now())
        .bind(id)
        .bind(user_id)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// Queues a message: sent at `send_at` (scheduled), or after the undo-send delay.
    pub async fn mail_outbox_enqueue(&self, user_id: &str, account_id: &str, c: &Compose, send_at: Option<DateTime<Utc>>) -> Result<OutboxEntry> {
        let a = self.mail_account(user_id, account_id).await?;
        let id = new_id();
        let t = now();
        sqlx::query("INSERT INTO mail_outbox (id, user_id, account_id, compose, created_at, updated_at) VALUES (?, ?, ?, '{}', ?, ?)")
            .bind(&id)
            .bind(user_id)
            .bind(&a.id)
            .bind(t)
            .bind(t)
            .execute(&self.db)
            .await?;
        let r = async {
            let stored = self.store_outbox_files(user_id, &id, c).await?;
            // Everything that would make sending fail for good is caught now.
            self.build_outgoing(user_id, &a, &stored, false).await?;
            let when = self.send_time(user_id, send_at).await?;
            self.write_outbox(user_id, &id, &a.id, &stored, Some(when)).await
        }
        .await;
        if let Err(e) = r {
            let _ = sqlx::query("DELETE FROM mail_outbox WHERE id = ?").bind(&id).execute(&self.db).await;
            return Err(e);
        }
        self.forget_uploads(user_id, c).await;
        self.outbox_changed(user_id, &id, "queued");
        wake_senders();
        self.outbox_entry(user_id, &id).await
    }

    /// Replaces a queued message (edited after undo or from the outbox) and schedules it.
    pub async fn mail_outbox_update(&self, user_id: &str, id: &str, account_id: &str, c: &Compose, send_at: Option<DateTime<Utc>>) -> Result<OutboxEntry> {
        let cur = self.outbox_entry(user_id, id).await?;
        if cur.status == "sending" {
            return Err(Error::conflict("this message is being sent right now"));
        }
        let a = self.mail_account(user_id, account_id).await?;
        let stored = self.store_outbox_files(user_id, id, c).await?;
        self.build_outgoing(user_id, &a, &stored, false).await?;
        let when = self.send_time(user_id, send_at).await?;
        self.write_outbox(user_id, id, &a.id, &stored, Some(when)).await?;
        self.forget_uploads(user_id, c).await;
        self.outbox_changed(user_id, id, "queued");
        wake_senders();
        self.outbox_entry(user_id, id).await
    }

    /// Saves edits to a set-aside message without scheduling it (the composer was closed).
    pub async fn mail_outbox_keep(&self, user_id: &str, id: &str, account_id: &str, c: &Compose) -> Result<OutboxEntry> {
        let cur = self.outbox_entry(user_id, id).await?;
        if cur.status == "sending" {
            return Err(Error::conflict("this message is being sent right now"));
        }
        let a = self.mail_account(user_id, account_id).await?;
        let stored = self.store_outbox_files(user_id, id, c).await?;
        self.write_outbox(user_id, id, &a.id, &stored, None).await?;
        self.forget_uploads(user_id, c).await;
        self.outbox_changed(user_id, id, "held");
        self.outbox_entry(user_id, id).await
    }

    /// Composer uploads copied into the outbox are not needed any more.
    async fn forget_uploads(&self, user_id: &str, original: &Compose) {
        let mut ids: Vec<String> = original
            .attachments
            .iter()
            .filter_map(|r| match r {
                AttachmentRef::Upload { id } => Some(id.clone()),
                _ => None,
            })
            .collect();
        if let Some(h) = &original.html {
            ids.extend(
                richtext::image_sources(&richtext::sanitize(h))
                    .into_iter()
                    .filter_map(|s| s.strip_prefix("/api/mail/uploads/").and_then(|r| r.strip_suffix("/content")).map(String::from)),
            );
        }
        if !ids.is_empty() {
            let _ = sqlx::query("DELETE FROM mail_uploads WHERE user_id = ? AND id IN (SELECT value FROM json_each(?))")
                .bind(user_id)
                .bind(serde_json::to_string(&ids).unwrap_or_default())
                .execute(&self.db)
                .await;
        }
    }

    /// Sends a queued message now (also a failed or set-aside one: attempts start over).
    pub async fn mail_outbox_send_now(&self, user_id: &str, id: &str) -> Result<OutboxEntry> {
        let cur = self.outbox_entry(user_id, id).await?;
        if cur.status == "sending" {
            return Ok(cur);
        }
        sqlx::query("UPDATE mail_outbox SET send_at = ?, status = 'queued', attempts = 0, next_attempt_at = NULL, updated_at = ? WHERE id = ?")
            .bind(now())
            .bind(now())
            .bind(id)
            .execute(&self.db)
            .await?;
        self.outbox_changed(user_id, id, "queued");
        wake_senders();
        self.outbox_entry(user_id, id).await
    }

    /// Sets a message aside (undo send): it stays in the outbox without a send time.
    pub async fn mail_outbox_hold(&self, user_id: &str, id: &str) -> Result<OutboxEntry> {
        let cur = self.outbox_entry(user_id, id).await?;
        if cur.status == "sending" {
            return Err(Error::conflict("too late: this message is being sent"));
        }
        sqlx::query("UPDATE mail_outbox SET send_at = NULL, status = 'queued', next_attempt_at = NULL, updated_at = ? WHERE id = ?")
            .bind(now())
            .bind(id)
            .execute(&self.db)
            .await?;
        self.outbox_changed(user_id, id, "held");
        self.outbox_entry(user_id, id).await
    }

    pub async fn mail_outbox_delete(&self, user_id: &str, id: &str) -> Result<()> {
        let cur = self.outbox_entry(user_id, id).await?;
        if cur.status == "sending" {
            return Err(Error::conflict("this message is being sent right now"));
        }
        sqlx::query("DELETE FROM mail_outbox WHERE id = ?").bind(id).execute(&self.db).await?;
        self.outbox_changed(user_id, id, "deleted");
        Ok(())
    }

    fn outbox_changed(&self, user_id: &str, id: &str, what: &str) {
        self.hub.publish([user_id], &json!({"type": "mail.outbox", "what": what, "id": id}));
    }

    /// Runs the outbox sender for the life of the process.
    pub fn spawn_mail_outbox(&self) {
        let core = self.clone();
        let notify = Arc::new(Notify::new());
        WAKERS.lock().push(Arc::downgrade(&notify));
        tokio::spawn(async move {
            // A message caught mid-send by a restart may or may not have gone out; it is
            // retried (like any other failure) rather than silently dropped.
            let _ = sqlx::query(
                "UPDATE mail_outbox SET status = 'failed', attempts = attempts + 1, next_attempt_at = ?,
                 last_error = 'Sending was interrupted by a server restart' WHERE status = 'sending'",
            )
            .bind(now())
            .execute(&core.db)
            .await;
            loop {
                let next = match core.send_due_outbox().await {
                    Ok(n) => n,
                    Err(e) => {
                        tracing::warn!(error = %e, "outbox run failed");
                        None
                    }
                };
                let wait = next.map(|t| (t - now()).to_std().unwrap_or(Duration::ZERO)).unwrap_or(IDLE_WAIT).min(IDLE_WAIT);
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = notify.notified() => {}
                }
            }
        });
    }

    /// Sends every due message; returns when the next one is due.
    pub async fn send_due_outbox(&self) -> Result<Option<DateTime<Utc>>> {
        let t = now();
        let due: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM mail_outbox WHERE send_at IS NOT NULL AND send_at <= ? AND status IN ('queued', 'failed')
             AND (next_attempt_at IS NULL OR next_attempt_at <= ?) ORDER BY send_at LIMIT ?",
        )
        .bind(t)
        .bind(t)
        .bind(BATCH)
        .fetch_all(&self.db)
        .await?;
        let mut tasks = tokio::task::JoinSet::new();
        for id in due {
            let claimed = sqlx::query("UPDATE mail_outbox SET status = 'sending', updated_at = ? WHERE id = ? AND status IN ('queued', 'failed')")
                .bind(now())
                .bind(&id)
                .execute(&self.db)
                .await?
                .rows_affected();
            if claimed == 1 {
                let core = self.clone();
                tasks.spawn(async move { core.send_outbox_entry(&id).await });
            }
        }
        while let Some(r) = tasks.join_next().await {
            if let Ok(Err(e)) = r {
                tracing::warn!(error = %e, "outbox bookkeeping failed");
            }
        }
        let next: Option<DateTime<Utc>> = sqlx::query_scalar(
            "SELECT MIN(CASE WHEN next_attempt_at IS NOT NULL AND next_attempt_at > send_at THEN next_attempt_at ELSE send_at END)
             FROM mail_outbox WHERE send_at IS NOT NULL AND status IN ('queued', 'failed')",
        )
        .fetch_one(&self.db)
        .await?;
        Ok(next)
    }

    async fn send_outbox_entry(&self, id: &str) -> Result<()> {
        let (user_id, account_id, raw, subject, attempts): (String, String, String, String, i64) =
            sqlx::query_as("SELECT user_id, account_id, compose, subject, attempts FROM mail_outbox WHERE id = ?")
                .bind(id)
                .fetch_one(&self.db)
                .await?;
        let result = match serde_json::from_str::<Compose>(&raw) {
            Ok(c) => self.send_composed(&user_id, &account_id, &c).await,
            Err(e) => Err(Error::bad(format!("the queued message is unreadable: {e}"))),
        };
        match result {
            Ok(message_id) => {
                sqlx::query("DELETE FROM mail_outbox WHERE id = ?").bind(id).execute(&self.db).await?;
                self.hub.publish(
                    [user_id.as_str()],
                    &json!({"type": "mail.outbox", "what": "sent", "id": id, "subject": subject, "message_id": message_id}),
                );
            }
            Err(e) => {
                let attempts = attempts + 1;
                let error = e.to_string();
                let retry = if transient(&e) { retry_delay(attempts) } else { None };
                let next = retry.map(|d| now() + d);
                tracing::info!(outbox = id, attempts, error = %error, retry = ?next, "sending a queued message failed");
                // Without a retry the message is set aside until the user acts on it.
                sqlx::query(
                    "UPDATE mail_outbox SET status = 'failed', attempts = ?, last_error = ?, next_attempt_at = ?,
                     send_at = CASE WHEN ? IS NULL THEN NULL ELSE send_at END, updated_at = ? WHERE id = ?",
                )
                .bind(attempts)
                .bind(&error)
                .bind(next)
                .bind(next)
                .bind(now())
                .bind(id)
                .execute(&self.db)
                .await?;
                self.hub.publish(
                    [user_id.as_str()],
                    &json!({"type": "mail.outbox", "what": "failed", "id": id, "subject": subject, "error": error, "retry_at": next}),
                );
            }
        }
        Ok(())
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;
    use crate::auth::NewUser;
    use crate::mailer::sink;

    #[test]
    fn retries_back_off_and_stop() {
        let mins: Vec<Option<i64>> = (1..=7).map(|a| retry_delay(a).map(|d| d.num_minutes())).collect();
        assert_eq!(mins, vec![Some(1), Some(5), Some(15), Some(60), Some(240), None, None]);
        assert!(transient(&Error::Unavailable("sending mail failed: connection refused".into())));
        assert!(!transient(&Error::Unavailable(
            "sending mail failed: permanent error (550): no such user".into()
        )));
        assert!(!transient(&Error::bad("To: \"x\" is not an email address")));
        assert_eq!(percent_decode("a%40b%2Fc%"), "a@b/c%");
    }

    async fn core_with_account(smtp_port: u16) -> (tempfile::TempDir, Core, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config {
            data_dir: dir.path().to_path_buf(),
            ..Config::default()
        };
        cfg.sfu.enabled = false;
        let core = Core::open(cfg).await.unwrap();
        let user = core
            .create_user(NewUser {
                username: "alice".into(),
                password: "alice-password".into(),
                display_name: Some("Alice".into()),
                email: Some("alice@example.org".into()),
                is_admin: false,
                quota_bytes: None,
            })
            .await
            .unwrap();
        let id = new_id();
        let sealed = core.secret_box().seal(&id, "pw").unwrap();
        sqlx::query(
            "INSERT INTO mail_accounts (id, user_id, name, email, imap_host, imap_port, imap_security, imap_username, smtp_host, smtp_port,
             smtp_security, smtp_username, password, save_sent, signature, created_at)
             VALUES (?, ?, 'Alice', 'alice@example.org', '127.0.0.1', 1, 'none', 'alice', '127.0.0.1', ?, 'none', '', ?, 0, 'Alice A.', ?)",
        )
        .bind(&id)
        .bind(&user.id)
        .bind(smtp_port as i64)
        .bind(&sealed)
        .bind(now())
        .execute(&core.db)
        .await
        .unwrap();
        (dir, core, user.id, id)
    }

    fn message(subject: &str) -> Compose {
        Compose {
            to: vec!["Bob <bob@example.org>".into()],
            subject: subject.into(),
            html: Some("<p>Hello <b>Bob</b></p><img src=\"data:image/png;base64,iVBORw0KGgo=\" alt=\"dot\">".into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn queued_messages_are_sent_when_due_and_can_be_undone() {
        let (port, mut rx) = sink::sink().await;
        let (_dir, core, user, acc) = core_with_account(port).await;
        core.set_mail_send_prefs(&user, 0).await.unwrap();
        let up = core.mail_upload(&user, "notes.txt", "text/plain", b"the notes".to_vec()).await.unwrap();
        let mut c = message("Now");
        c.attachments = vec![AttachmentRef::Upload { id: up.id.clone() }];
        let e = core.mail_outbox_enqueue(&user, &acc, &c, None).await.unwrap();
        assert_eq!(e.status, "queued");
        assert_eq!(e.recipients, vec!["Bob <bob@example.org>"]);
        // The upload was copied into the outbox and is gone from the composer's uploads.
        assert!(core.mail_upload_content(&user, &up.id).await.is_err());
        let stored = core.mail_outbox_message(&user, &e.id).await.unwrap();
        assert!(matches!(stored.compose.attachments[0], AttachmentRef::Outbox { .. }));
        assert_eq!(stored.attachment_names[0].0, "notes.txt");
        core.send_due_outbox().await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap();
        assert_eq!(got.to, vec!["bob@example.org"]);
        let parsed = mail_parser::MessageParser::default().parse(got.data.as_bytes()).unwrap();
        assert!(parsed.body_html(0).unwrap().contains("<b>Bob</b>"));
        assert!(parsed.body_text(0).unwrap().contains("Hello *Bob*"));
        assert!(parsed.attachments().any(|p| mail_parser::MimeHeaders::attachment_name(p) == Some("notes.txt")));
        assert!(
            parsed.parts.iter().any(|p| mail_parser::MimeHeaders::content_id(p).is_some()),
            "inline image as a cid part"
        );
        assert!(core.mail_outbox(&user).await.unwrap().is_empty(), "sent messages leave the outbox");

        // Scheduled for later: not sent yet; held (undo): never sent; send now: sent.
        let later = core
            .mail_outbox_enqueue(&user, &acc, &message("Later"), Some(now() + chrono::Duration::hours(2)))
            .await
            .unwrap();
        let next = core.send_due_outbox().await.unwrap().unwrap();
        assert!(next > now() + chrono::Duration::minutes(119), "next run is the scheduled time");
        core.mail_outbox_hold(&user, &later.id).await.unwrap();
        assert_eq!(core.send_due_outbox().await.unwrap(), None);
        assert!(rx.try_recv().is_err(), "nothing was sent");
        core.mail_outbox_send_now(&user, &later.id).await.unwrap();
        core.send_due_outbox().await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap();
        assert!(got.data.contains("Subject: Later"));

        // The undo delay keeps a message back.
        core.set_mail_send_prefs(&user, 10).await.unwrap();
        let undoable = core.mail_outbox_enqueue(&user, &acc, &message("Undo me"), None).await.unwrap();
        assert!(undoable.send_at.unwrap() > now() + chrono::Duration::seconds(8));
        core.send_due_outbox().await.unwrap();
        core.mail_outbox_delete(&user, &undoable.id).await.unwrap();
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn failures_are_retried_with_backoff() {
        // Nothing listens on this port.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let (_dir, core, user, acc) = core_with_account(closed).await;
        core.set_mail_send_prefs(&user, 0).await.unwrap();
        let e = core.mail_outbox_enqueue(&user, &acc, &message("Retry"), None).await.unwrap();
        core.send_due_outbox().await.unwrap();
        let failed = core.mail_outbox(&user).await.unwrap().remove(0);
        assert_eq!((failed.id.as_str(), failed.status.as_str(), failed.attempts), (e.id.as_str(), "failed", 1));
        assert!(failed.last_error.unwrap().contains("sending mail failed"));
        let retry = failed.next_attempt_at.unwrap();
        assert!(
            retry > now() + chrono::Duration::seconds(50) && retry < now() + chrono::Duration::seconds(70),
            "{retry}"
        );
        assert!(failed.send_at.is_some(), "still scheduled");
        // Not due again before the retry time.
        core.send_due_outbox().await.unwrap();
        assert_eq!(core.mail_outbox(&user).await.unwrap()[0].attempts, 1);
        // Bad input is refused when queuing, not retried later.
        let mut bad = message("Bad");
        bad.to = vec!["not an address".into()];
        assert!(matches!(core.mail_outbox_enqueue(&user, &acc, &bad, None).await, Err(Error::BadRequest(_))));
        assert_eq!(core.mail_outbox(&user).await.unwrap().len(), 1);
    }
}
