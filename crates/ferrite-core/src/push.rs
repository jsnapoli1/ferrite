//! Web Push bookkeeping: device subscriptions, per-user preferences and the queue of pushes.
//!
//! Every [`Core::notify`] and every newly started call becomes a [`PushMessage`] on a bounded
//! queue; the server's push sender drains it, encrypts (RFC 8291) and delivers to the user's
//! devices. Producers never wait: when the queue is full the push is dropped (the in-app
//! notification still exists).

use crate::talk::{Actor, Room};
use crate::util::{new_id, now};
use crate::{Core, Error, Result};
use base64::Engine;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// Most devices one account may register.
pub const MAX_SUBSCRIPTIONS: i64 = 20;
const QUEUE: usize = 1024;

/// `[push]` settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PushConfig {
    /// Accept `http:` endpoints and endpoints on private or loopback addresses. Only for tests
    /// and development against a local push service: it disables the SSRF guard.
    pub allow_insecure_endpoints: bool,
    /// Contact URI sent to push services in the VAPID `sub` claim (`mailto:` or `https:`).
    /// Defaults to `public_url` when it is https, else `mailto:webpush@<public host>`.
    pub contact: Option<String>,
}

/// User preferences; categories gate which pushes reach their devices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct PushPrefs {
    pub enabled: bool,
    pub chat: bool,
    pub calls: bool,
    pub calendar: bool,
    pub files: bool,
    pub boards: bool,
}

impl Default for PushPrefs {
    fn default() -> Self {
        Self {
            enabled: true,
            chat: true,
            calls: true,
            calendar: true,
            files: true,
            boards: true,
        }
    }
}

impl PushPrefs {
    pub fn allows(&self, category: Category) -> bool {
        self.enabled
            && match category {
                Category::Chat => self.chat,
                Category::Calls => self.calls,
                Category::Calendar => self.calendar,
                Category::Files => self.files,
                Category::Boards => self.boards,
                Category::Other | Category::Test => true,
            }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Chat,
    Calls,
    Calendar,
    Files,
    Boards,
    Other,
    /// "Send test notification": ignores the category switches (but still needs a device).
    Test,
}

impl Category {
    /// Category of a notification kind (as passed to [`Core::notify`]).
    pub fn of_kind(kind: &str) -> Self {
        match kind {
            "talk" | "chat" | "mention" => Category::Chat,
            "call" => Category::Calls,
            "calendar" | "event" | "invitation" | "itip" => Category::Calendar,
            "share" | "file" | "files" => Category::Files,
            "board" | "boards" | "deck" => Category::Boards,
            _ => Category::Other,
        }
    }
}

/// RFC 8030 §5.3 message urgency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Urgency {
    VeryLow,
    Low,
    Normal,
    High,
}

impl Urgency {
    pub fn as_str(self) -> &'static str {
        match self {
            Urgency::VeryLow => "very-low",
            Urgency::Low => "low",
            Urgency::Normal => "normal",
            Urgency::High => "high",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PushAction {
    pub action: String,
    pub title: String,
    /// Where the action leads; `None` just closes the notification.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// One push for one user. Only `payload()` leaves the server, encrypted to each device.
#[derive(Debug, Clone)]
pub struct PushMessage {
    pub user_id: String,
    pub category: Category,
    /// "notification", "call" or "test" (the service worker styles each).
    pub kind: &'static str,
    pub title: String,
    pub body: String,
    /// App path to open on click.
    pub url: String,
    /// Notifications with the same tag replace each other on the device.
    pub tag: String,
    pub actions: Vec<PushAction>,
    pub urgency: Urgency,
    /// Seconds the push service keeps the message for an offline device.
    pub ttl: u32,
    /// Messages with the same topic replace each other while undelivered (hashed by the sender).
    pub topic: Option<String>,
    /// The stored notification this push mirrors (marked read when the push is clicked).
    pub notification_id: Option<i64>,
}

/// Longest title and body put in a push: the encrypted record must stay under 4 KiB.
const MAX_TITLE: usize = 160;
const MAX_BODY: usize = 400;

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

impl PushMessage {
    /// The JSON the service worker receives.
    pub fn payload(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": self.kind,
            "title": clip(&self.title, MAX_TITLE),
            "body": clip(&self.body, MAX_BODY),
            "url": self.url,
            "tag": self.tag,
            "actions": self.actions,
            "notification_id": self.notification_id,
            "ts": now().timestamp_millis(),
        })
    }
}

/// The bounded queue between producers ([`Core::notify`], calls) and the sender.
pub struct PushQueue {
    tx: mpsc::Sender<PushMessage>,
    rx: Mutex<Option<mpsc::Receiver<PushMessage>>>,
}

impl Default for PushQueue {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel(QUEUE);
        Self { tx, rx: Mutex::new(Some(rx)) }
    }
}

impl PushQueue {
    /// Enqueues without waiting.
    pub fn send(&self, msg: PushMessage) {
        if let Err(mpsc::error::TrySendError::Full(m)) = self.tx.try_send(msg) {
            tracing::warn!(user = %m.user_id, "push queue full; dropping a push");
        }
    }

    /// The receiving end, for the one sender task.
    pub fn take_receiver(&self) -> Option<mpsc::Receiver<PushMessage>> {
        self.rx.lock().take()
    }
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct PushSubscription {
    pub id: String,
    #[serde(skip_serializing)]
    pub user_id: String,
    pub endpoint: String,
    #[serde(skip_serializing)]
    pub p256dh: String,
    #[serde(skip_serializing)]
    pub auth: String,
    pub label: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

const SUB_COLS: &str = "id, user_id, endpoint, p256dh, auth, label, created_at, last_used_at";

fn decode_key(value: &str, len: usize, what: &str) -> Result<Vec<u8>> {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value.trim().trim_end_matches('='))
        .map_err(|_| Error::bad(format!("{what} is not base64url")))?;
    if raw.len() != len {
        return Err(Error::bad(format!("{what} must be {len} bytes")));
    }
    Ok(raw)
}

impl PushSubscription {
    /// The user agent's ECDH public key (uncompressed P-256 point).
    pub fn public_key(&self) -> Result<Vec<u8>> {
        decode_key(&self.p256dh, 65, "p256dh")
    }
    pub fn auth_secret(&self) -> Result<Vec<u8>> {
        decode_key(&self.auth, 16, "auth")
    }
}

impl Core {
    pub async fn push_subscriptions(&self, user_id: &str) -> Result<Vec<PushSubscription>> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {SUB_COLS} FROM push_subscriptions WHERE user_id = ? ORDER BY created_at"
        )))
        .bind(user_id)
        .fetch_all(&self.db)
        .await?)
    }

    /// Registers (or re-registers) a device. The endpoint must already have passed the
    /// server's endpoint checks; keys are validated here.
    pub async fn add_push_subscription(&self, user_id: &str, endpoint: &str, p256dh: &str, auth: &str, label: &str) -> Result<PushSubscription> {
        let key = decode_key(p256dh, 65, "p256dh")?;
        if key[0] != 0x04 {
            return Err(Error::bad("p256dh must be an uncompressed P-256 point"));
        }
        decode_key(auth, 16, "auth")?;
        let label: String = label.trim().chars().take(80).collect();
        let label = if label.is_empty() { "Browser".to_string() } else { label };
        let existing: Option<(String, String)> = sqlx::query_as("SELECT id, user_id FROM push_subscriptions WHERE endpoint = ?")
            .bind(endpoint)
            .fetch_optional(&self.db)
            .await?;
        if existing.as_ref().is_none_or(|(_, owner)| owner != user_id) {
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM push_subscriptions WHERE user_id = ?")
                .bind(user_id)
                .fetch_one(&self.db)
                .await?;
            if count >= MAX_SUBSCRIPTIONS {
                return Err(Error::bad("too many devices receive push notifications; remove one first"));
            }
        }
        let id: String = sqlx::query_scalar(
            "INSERT INTO push_subscriptions (id, user_id, endpoint, p256dh, auth, label, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(endpoint) DO UPDATE SET user_id = excluded.user_id, p256dh = excluded.p256dh, auth = excluded.auth,
               label = excluded.label, created_at = CASE WHEN push_subscriptions.user_id = excluded.user_id THEN push_subscriptions.created_at ELSE excluded.created_at END
             RETURNING id",
        )
        .bind(new_id())
        .bind(user_id)
        .bind(endpoint)
        .bind(p256dh.trim().trim_end_matches('='))
        .bind(auth.trim().trim_end_matches('='))
        .bind(&label)
        .bind(now())
        .fetch_one(&self.db)
        .await?;
        Ok(
            sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {SUB_COLS} FROM push_subscriptions WHERE id = ?")))
                .bind(id)
                .fetch_one(&self.db)
                .await?,
        )
    }

    pub async fn remove_push_subscription(&self, user_id: &str, id: &str) -> Result<()> {
        let r = sqlx::query("DELETE FROM push_subscriptions WHERE id = ? AND user_id = ?")
            .bind(id)
            .bind(user_id)
            .execute(&self.db)
            .await?;
        if r.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    /// Forgets an endpoint the push service reported as gone (404/410).
    pub async fn remove_push_endpoint(&self, endpoint: &str) -> Result<()> {
        sqlx::query("DELETE FROM push_subscriptions WHERE endpoint = ?")
            .bind(endpoint)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    pub async fn touch_push_subscription(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE push_subscriptions SET last_used_at = ? WHERE id = ?")
            .bind(now())
            .bind(id)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    pub async fn push_prefs(&self, user_id: &str) -> Result<PushPrefs> {
        Ok(
            sqlx::query_as("SELECT enabled, chat, calls, calendar, files, boards FROM push_settings WHERE user_id = ?")
                .bind(user_id)
                .fetch_optional(&self.db)
                .await?
                .unwrap_or_default(),
        )
    }

    pub async fn set_push_prefs(&self, user_id: &str, p: PushPrefs) -> Result<PushPrefs> {
        sqlx::query(
            "INSERT INTO push_settings (user_id, enabled, chat, calls, calendar, files, boards) VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(user_id) DO UPDATE SET enabled = excluded.enabled, chat = excluded.chat, calls = excluded.calls,
               calendar = excluded.calendar, files = excluded.files, boards = excluded.boards",
        )
        .bind(user_id)
        .bind(p.enabled)
        .bind(p.chat)
        .bind(p.calls)
        .bind(p.calendar)
        .bind(p.files)
        .bind(p.boards)
        .execute(&self.db)
        .await?;
        Ok(p)
    }

    /// Mirrors a stored notification to the user's devices.
    pub(crate) fn push_notification(&self, id: i64, user_id: &str, kind: &str, title: &str, body: &str, link: &str) {
        let category = Category::of_kind(kind);
        // Chat notifications of one conversation replace each other; everything else stacks.
        let (tag, topic) = match category {
            Category::Chat => (format!("chat:{link}"), Some(format!("chat:{link}"))),
            _ => (format!("n:{id}"), None),
        };
        self.push.send(PushMessage {
            user_id: user_id.to_string(),
            category,
            kind: "notification",
            title: title.to_string(),
            body: body.to_string(),
            url: if link.is_empty() { "/".into() } else { link.to_string() },
            tag,
            actions: Vec::new(),
            urgency: Urgency::Normal,
            ttl: 24 * 3600,
            topic,
            notification_id: Some(id),
        });
    }

    /// Rings everyone in the room who isn't in its call yet ("Alice is calling in Ops").
    /// Called when a call starts; the lookups run in the background.
    pub fn push_incoming_call(&self, room: &Room, caller: &Actor) {
        let core = self.clone();
        let room = room.clone();
        let caller = caller.clone();
        tokio::spawn(async move {
            if let Err(e) = core.ring_room(&room, &caller).await {
                tracing::warn!(room = %room.id, error = %e, "incoming call push failed");
            }
        });
    }

    async fn ring_room(&self, room: &Room, caller: &Actor) -> Result<()> {
        let in_call: Vec<String> = self.call_members(&room.id).into_iter().map(|m| m.actor.id).collect();
        let members: Vec<(String, String)> =
            sqlx::query_as("SELECT p.user_id, p.notify FROM participants p JOIN users u ON u.id = p.user_id WHERE p.room_id = ? AND u.disabled = 0")
                .bind(&room.id)
                .fetch_all(&self.db)
                .await?;
        let room_url = format!("/talk/{}", room.token);
        let join_url = format!("{room_url}?call=1");
        for (user_id, notify) in members {
            if user_id == caller.id || notify == "none" || in_call.contains(&user_id) {
                continue;
            }
            let title = if room.kind == "one_to_one" {
                format!("{} is calling you", caller.name)
            } else {
                format!("{} is calling in {}", caller.name, self.room_display_name(room, &user_id).await?)
            };
            self.push.send(PushMessage {
                user_id,
                category: Category::Calls,
                kind: "call",
                title,
                body: "Incoming call in Ferrite Talk".into(),
                url: room_url.clone(),
                tag: format!("call:{}", room.token),
                actions: vec![
                    PushAction {
                        action: "join".into(),
                        title: "Join".into(),
                        url: Some(join_url.clone()),
                    },
                    PushAction {
                        action: "dismiss".into(),
                        title: "Dismiss".into(),
                        url: None,
                    },
                ],
                urgency: Urgency::High,
                // A ring is pointless once the call is likely over.
                ttl: 60,
                topic: Some(format!("call:{}", room.id)),
                notification_id: None,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_and_prefs() {
        assert_eq!(Category::of_kind("talk"), Category::Chat);
        assert_eq!(Category::of_kind("share"), Category::Files);
        assert_eq!(Category::of_kind("board"), Category::Boards);
        assert_eq!(Category::of_kind("calendar"), Category::Calendar);
        assert_eq!(Category::of_kind("something-new"), Category::Other);
        let p = PushPrefs {
            chat: false,
            ..Default::default()
        };
        assert!(!p.allows(Category::Chat));
        assert!(p.allows(Category::Files));
        assert!(p.allows(Category::Test));
        let off = PushPrefs {
            enabled: false,
            ..Default::default()
        };
        assert!(!off.allows(Category::Test));
        assert!(!off.allows(Category::Other));
    }

    #[test]
    fn payload_is_clipped() {
        let m = PushMessage {
            user_id: "u".into(),
            category: Category::Chat,
            kind: "notification",
            title: "t".repeat(500),
            body: "b".repeat(5000),
            url: "/talk/x".into(),
            tag: "chat:/talk/x".into(),
            actions: Vec::new(),
            urgency: Urgency::Normal,
            ttl: 60,
            topic: None,
            notification_id: Some(3),
        };
        let p = m.payload();
        assert_eq!(p["title"].as_str().unwrap().chars().count(), MAX_TITLE);
        assert_eq!(p["body"].as_str().unwrap().chars().count(), MAX_BODY);
        assert!(p.to_string().len() < 3000);
        assert_eq!(p["notification_id"], 3);
    }
}
