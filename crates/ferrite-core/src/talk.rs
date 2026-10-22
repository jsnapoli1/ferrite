//! Talk: conversations, messages, reactions, polls, read markers, guests and calls.
//!
//! Nextcloud Talk without its separate "high performance backend" polls the server every few
//! seconds per open conversation. Here every event is pushed over one WebSocket per client
//! from an in-memory hub, and call media flows peer to peer with the server only relaying
//! signaling, so a small home server can carry many concurrent conversations and calls.
//!
//! Moderation (lobby, room passwords, call moderation) and breakout rooms live in the
//! `moderation` and `breakout` submodules.

pub mod breakout;
pub mod moderation;

use crate::util::{new_id, now, random_token, sha256_hex, short_token};
use crate::{Core, Error, Result};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::types::Json;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tokio::sync::broadcast;

pub const BOT_ACTOR_ID: &str = "assistant";
pub const MAX_MESSAGE_LEN: usize = 32_000;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Room {
    pub id: String,
    pub token: String,
    pub kind: String,
    pub name: String,
    pub description: String,
    pub created_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_activity_at: DateTime<Utc>,
    pub read_only: bool,
    pub ai_enabled: bool,
    /// Lobby on: only moderators can read, write and call (until `lobby_timer`, when set).
    pub lobby: bool,
    pub lobby_timer: Option<DateTime<Utc>>,
    /// Public rooms: whether guests and new participants need a password.
    pub has_password: bool,
    /// Who may start a call: "everyone" or "moderators".
    pub call_permission: String,
    /// Breakout rooms: the main conversation they belong to.
    pub parent_id: Option<String>,
    /// Main conversations: how people are assigned to breakout rooms (none when there are none).
    pub breakout_mode: Option<String>,
    pub breakout_status: String,
    pub breakout_ends_at: Option<DateTime<Utc>>,
    /// Breakout rooms: when someone in it asked the moderators for help.
    pub help_requested_at: Option<DateTime<Utc>>,
}

const ROOM_COLS: &str = "id, token, kind, name, description, created_by, created_at, last_activity_at, read_only, ai_enabled, \
                         lobby, lobby_timer, password_hash IS NOT NULL AS has_password, call_permission, parent_id, breakout_mode, \
                         breakout_status, breakout_ends_at, help_requested_at";

impl Room {
    /// Whether the lobby currently holds back non-moderators (a passed timer counts as open,
    /// even before the background sweep has switched the lobby off).
    pub fn lobby_active(&self) -> bool {
        self.lobby && self.lobby_timer.is_none_or(|t| t > now())
    }
    /// Group-like rooms, where moderation, lobby and breakout rooms apply.
    pub fn is_groupish(&self) -> bool {
        matches!(self.kind.as_str(), "group" | "public")
    }
}

/// A room as one participant sees it.
#[derive(Debug, Clone, Serialize)]
pub struct RoomView {
    #[serde(flatten)]
    pub room: Room,
    pub display_name: String,
    pub role: String,
    pub notify: String,
    pub favorite: bool,
    pub unread: i64,
    pub unread_mention: bool,
    pub last_read_message_id: i64,
    pub last_message: Option<Message>,
    pub participant_count: i64,
    pub call_active: bool,
    /// Breakout rooms: the main conversation's token and name.
    pub parent_token: Option<String>,
    pub parent_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Message {
    pub id: i64,
    pub room_id: String,
    pub actor_kind: String,
    pub actor_id: String,
    pub actor_name: String,
    pub kind: String,
    pub body: String,
    pub reply_to: Option<i64>,
    pub meta: Json<Value>,
    pub created_at: DateTime<Utc>,
    pub edited_at: Option<DateTime<Utc>>,
    pub deleted: bool,
}

const MSG_COLS: &str = "id, room_id, actor_kind, actor_id, actor_name, kind, body, reply_to, meta, created_at, edited_at, deleted";

#[derive(Debug, Clone, Serialize)]
pub struct MessageView {
    #[serde(flatten)]
    pub message: Message,
    /// emoji -> actor ids
    pub reactions: BTreeMap<String, Vec<String>>,
    pub parent: Option<ParentPreview>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ParentPreview {
    pub id: i64,
    pub actor_name: String,
    pub body: String,
    pub deleted: bool,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Participant {
    pub user_id: String,
    pub username: String,
    pub display_name: String,
    pub role: String,
    pub last_read_message_id: i64,
}

/// Who is acting: a signed-in user, a guest in a public room, or a bot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Actor {
    pub kind: String,
    pub id: String,
    pub name: String,
}

impl Actor {
    pub fn user(id: &str, name: &str) -> Self {
        Self {
            kind: "user".into(),
            id: id.into(),
            name: name.into(),
        }
    }
    pub fn bot() -> Self {
        Self {
            kind: "bot".into(),
            id: BOT_ACTOR_ID.into(),
            name: "Assistant".into(),
        }
    }
    pub fn system() -> Self {
        Self {
            kind: "system".into(),
            id: "system".into(),
            name: "System".into(),
        }
    }
    /// Key under which this actor's connections are registered in the hub.
    pub fn hub_key(&self) -> String {
        match self.kind.as_str() {
            "guest" => format!("guest:{}", self.id),
            _ => self.id.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct NewMessage {
    pub body: String,
    pub reply_to: Option<i64>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub meta: Option<Value>,
}

/// One connection's presence in a call.
#[derive(Debug, Clone, Serialize)]
pub struct CallMember {
    pub session: String,
    pub actor: Actor,
    #[serde(skip)]
    pub conn: u64,
    pub audio: bool,
    pub video: bool,
    pub screen: bool,
    pub hand: bool,
    /// When the hand went up: raised hands are a queue ordered by this.
    pub hand_raised_at: Option<DateTime<Utc>>,
    pub joined_at: DateTime<Utc>,
}

/// Who receives a room's events, cached until membership, roles or the lobby change.
pub struct Audience {
    /// Hub keys of every participant and guest.
    pub all: Vec<String>,
    /// Hub keys of owners and moderators (the only receivers while the lobby is on).
    pub moderators: Vec<String>,
    lobby: bool,
    lobby_timer: Option<DateTime<Utc>>,
}

impl Audience {
    /// The receivers of ordinary room events right now.
    pub fn current(&self) -> &[String] {
        if self.lobby && self.lobby_timer.is_none_or(|t| t > now()) {
            &self.moderators
        } else {
            &self.all
        }
    }
}

/// In-memory state shared by the Talk service.
pub struct TalkState {
    /// room id -> everyone who should receive room events.
    audience: DashMap<String, Arc<Audience>>,
    /// room id -> active call members.
    calls: DashMap<String, Vec<CallMember>>,
    /// room id -> people waiting in the lobby (one entry per connection).
    waiting: DashMap<String, Vec<(Actor, u64)>>,
    /// Call sessions the server ended (moderation, lobby, breakout rooms), so the SFU can
    /// drop their media connections.
    ended: broadcast::Sender<String>,
}

impl Default for TalkState {
    fn default() -> Self {
        Self {
            audience: DashMap::new(),
            calls: DashMap::new(),
            waiting: DashMap::new(),
            ended: broadcast::channel(256).0,
        }
    }
}

fn mention_names(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, _) in body.match_indices('@') {
        if i > 0 && body[..i].chars().last().is_some_and(|c| c.is_alphanumeric()) {
            continue;
        }
        let name: String = body[i + 1..].chars().take_while(|c| c.is_ascii_alphanumeric() || "._-".contains(*c)).collect();
        let name = name.trim_end_matches(['.', '-']).to_string();
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

impl Core {
    // ---- rooms ---------------------------------------------------------------

    pub async fn room(&self, id: &str) -> Result<Room> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {ROOM_COLS} FROM rooms WHERE id = ?")))
            .bind(id)
            .fetch_one(&self.db)
            .await?)
    }

    pub async fn room_by_token(&self, token: &str) -> Result<Room> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {ROOM_COLS} FROM rooms WHERE token = ?")))
            .bind(token)
            .fetch_one(&self.db)
            .await?)
    }

    pub async fn participant_role(&self, room_id: &str, user_id: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT role FROM participants WHERE room_id = ? AND user_id = ?")
            .bind(room_id)
            .bind(user_id)
            .fetch_optional(&self.db)
            .await?)
    }

    /// Room by token if the user participates in it.
    pub async fn room_for(&self, user_id: &str, token: &str) -> Result<Room> {
        let room = self.room_by_token(token).await?;
        if self.participant_role(&room.id, user_id).await?.is_none() {
            return Err(Error::NotFound);
        }
        Ok(room)
    }

    /// Whether an actor moderates a room (owners and moderators; never guests or bots).
    pub async fn is_moderator(&self, actor: &Actor, room: &Room) -> Result<bool> {
        if actor.kind != "user" {
            return Ok(false);
        }
        Ok(matches!(
            self.participant_role(&room.id, &actor.id).await?.as_deref(),
            Some("owner" | "moderator")
        ))
    }

    async fn require_moderator(&self, room: &Room, user_id: &str) -> Result<()> {
        match self.participant_role(&room.id, user_id).await?.as_deref() {
            Some("owner") | Some("moderator") => Ok(()),
            Some(_) => Err(Error::Forbidden),
            None => Err(Error::NotFound),
        }
    }

    pub async fn create_room(&self, user_id: &str, kind: &str, name: &str, invite: &[String]) -> Result<Room> {
        if !matches!(kind, "group" | "public") {
            return Err(Error::bad("kind must be group or public (use direct_room for one to one)"));
        }
        let name = name.trim();
        if name.is_empty() || name.len() > 200 {
            return Err(Error::bad("room name must be 1-200 characters"));
        }
        let id = new_id();
        let t = now();
        let mut tx = self.db.begin().await?;
        sqlx::query("INSERT INTO rooms (id, token, kind, name, created_by, created_at, last_activity_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
            .bind(&id)
            .bind(short_token(10))
            .bind(kind)
            .bind(name)
            .bind(user_id)
            .bind(t)
            .bind(t)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO participants (room_id, user_id, role, joined_at) VALUES (?, ?, 'owner', ?)")
            .bind(&id)
            .bind(user_id)
            .bind(t)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        for username in invite {
            if let Ok(u) = self.user_by_name(username).await {
                if u.id != user_id {
                    self.add_participant_internal(&id, &u.id, "member").await?;
                }
            }
        }
        let creator = self.user(user_id).await?;
        self.post_system(&id, &format!("{} created the conversation", creator.display_name)).await?;
        self.room(&id).await
    }

    /// Returns (creating if needed) the one-to-one conversation between two users.
    pub async fn direct_room(&self, user_id: &str, other_username: &str) -> Result<Room> {
        let other = self.user_by_name(other_username).await.map_err(|_| Error::bad("no such user"))?;
        if other.id == user_id {
            return self.note_to_self(user_id).await;
        }
        let mut pair = [user_id.to_string(), other.id.clone()];
        pair.sort();
        let key = pair.join(":");
        if let Some(id) = sqlx::query_scalar::<_, String>("SELECT id FROM rooms WHERE pair_key = ?")
            .bind(&key)
            .fetch_optional(&self.db)
            .await?
        {
            // Re-add a participant who left earlier.
            for u in &pair {
                if self.participant_role(&id, u).await?.is_none() {
                    self.add_participant_internal(&id, u, "owner").await?;
                }
            }
            return self.room(&id).await;
        }
        let id = new_id();
        let t = now();
        let mut tx = self.db.begin().await?;
        sqlx::query(
            "INSERT INTO rooms (id, token, kind, name, created_by, created_at, last_activity_at, pair_key) VALUES (?, ?, 'one_to_one', '', ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(short_token(10))
        .bind(user_id)
        .bind(t)
        .bind(t)
        .bind(&key)
        .execute(&mut *tx)
        .await?;
        for u in &pair {
            sqlx::query("INSERT INTO participants (room_id, user_id, role, notify, joined_at) VALUES (?, ?, 'owner', 'all', ?)")
                .bind(&id)
                .bind(u)
                .bind(t)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        self.talk.audience.remove(&id);
        self.room(&id).await
    }

    pub async fn note_to_self(&self, user_id: &str) -> Result<Room> {
        let key = format!("self:{user_id}");
        if let Some(id) = sqlx::query_scalar::<_, String>("SELECT id FROM rooms WHERE pair_key = ?")
            .bind(&key)
            .fetch_optional(&self.db)
            .await?
        {
            return self.room(&id).await;
        }
        let id = new_id();
        let t = now();
        let mut tx = self.db.begin().await?;
        sqlx::query("INSERT INTO rooms (id, token, kind, name, created_by, created_at, last_activity_at, pair_key) VALUES (?, ?, 'note_to_self', 'Note to self', ?, ?, ?, ?)")
            .bind(&id)
            .bind(short_token(10))
            .bind(user_id)
            .bind(t)
            .bind(t)
            .bind(&key)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO participants (room_id, user_id, role, joined_at) VALUES (?, ?, 'owner', ?)")
            .bind(&id)
            .bind(user_id)
            .bind(t)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.room(&id).await
    }

    /// The user's conversations. Breakout rooms are reached through their main conversation,
    /// so they are not listed.
    pub async fn list_rooms(&self, user_id: &str) -> Result<Vec<RoomView>> {
        self.room_views(user_id, None).await
    }

    /// Views of the user's rooms: all listed ones, or only the room `only` (any kind).
    async fn room_views(&self, user_id: &str, only: Option<&str>) -> Result<Vec<RoomView>> {
        #[derive(sqlx::FromRow)]
        struct Row {
            id: String,
            role: String,
            notify: String,
            favorite: bool,
            last_read_message_id: i64,
            unread: i64,
            participant_count: i64,
        }
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT r.id, p.role, p.notify, p.favorite, p.last_read_message_id,
                    (SELECT COUNT(*) FROM messages m WHERE m.room_id = r.id AND m.id > p.last_read_message_id
                        AND m.deleted = 0 AND m.actor_id != p.user_id AND m.kind != 'system') AS unread,
                    (SELECT COUNT(*) FROM participants p2 WHERE p2.room_id = r.id) AS participant_count
             FROM rooms r JOIN participants p ON p.room_id = r.id
             WHERE p.user_id = ?1 AND ((?2 IS NULL AND r.kind != 'breakout') OR r.id = ?2)
             ORDER BY p.favorite DESC, r.last_activity_at DESC",
        )
        .bind(user_id)
        .bind(only)
        .fetch_all(&self.db)
        .await?;
        let me = self.user(user_id).await?;
        let mut out = Vec::with_capacity(rows.len());
        for mut row in rows {
            let room = self.room(&row.id).await?;
            // Someone waiting in the lobby sees the conversation but nothing of its history.
            let waiting = room.lobby_active() && !matches!(row.role.as_str(), "owner" | "moderator");
            let last_message: Option<Message> = if waiting {
                row.unread = 0;
                None
            } else {
                sqlx::query_as(sqlx::AssertSqlSafe(format!(
                    "SELECT {MSG_COLS} FROM messages WHERE room_id = ? ORDER BY id DESC LIMIT 1"
                )))
                .bind(&room.id)
                .fetch_optional(&self.db)
                .await?
            };
            let unread_mention = if row.unread > 0 {
                // LIKE narrows the candidates; `mentions` then applies the same token rules as the
                // web client so `@bobby` is not a mention of `bob`.
                let pat = format!("%@{}%", me.username);
                let bodies: Vec<String> =
                    sqlx::query_scalar("SELECT body FROM messages WHERE room_id = ? AND id > ? AND deleted = 0 AND (body LIKE ? OR body LIKE '%@all%')")
                        .bind(&room.id)
                        .bind(row.last_read_message_id)
                        .bind(pat)
                        .fetch_all(&self.db)
                        .await?;
                room.kind == "one_to_one" || bodies.iter().any(|b| mentions(b, &me.username) || mentions(b, "all"))
            } else {
                false
            };
            let display_name = self.room_display_name(&room, user_id).await?;
            let call_active = self.talk.calls.get(&room.id).is_some_and(|c| !c.is_empty());
            let parent = match &room.parent_id {
                Some(pid) => self.room(pid).await.ok(),
                None => None,
            };
            out.push(RoomView {
                display_name,
                role: row.role,
                notify: row.notify,
                favorite: row.favorite,
                unread: row.unread,
                unread_mention,
                last_read_message_id: row.last_read_message_id,
                last_message,
                participant_count: row.participant_count,
                call_active,
                parent_token: parent.as_ref().map(|p| p.token.clone()),
                parent_name: parent.map(|p| p.name),
                room,
            });
        }
        Ok(out)
    }

    pub async fn room_view(&self, user_id: &str, token: &str) -> Result<RoomView> {
        let room = self.room_for(user_id, token).await?;
        self.room_views(user_id, Some(&room.id)).await?.pop().ok_or(Error::NotFound)
    }

    pub async fn room_display_name(&self, room: &Room, viewer_id: &str) -> Result<String> {
        if room.kind == "one_to_one" {
            let other: Option<String> =
                sqlx::query_scalar("SELECT u.display_name FROM participants p JOIN users u ON u.id = p.user_id WHERE p.room_id = ? AND p.user_id != ? LIMIT 1")
                    .bind(&room.id)
                    .bind(viewer_id)
                    .fetch_optional(&self.db)
                    .await?;
            if let Some(o) = other {
                return Ok(o);
            }
            if let Some(pair) = sqlx::query_scalar::<_, Option<String>>("SELECT pair_key FROM rooms WHERE id = ?")
                .bind(&room.id)
                .fetch_one(&self.db)
                .await?
            {
                for uid in pair.split(':') {
                    if uid != viewer_id {
                        if let Ok(u) = self.user(uid).await {
                            return Ok(u.display_name);
                        }
                    }
                }
            }
        }
        Ok(room.name.clone())
    }

    /// Public rooms the user hasn't joined yet ("open conversations").
    pub async fn listable_rooms(&self, user_id: &str) -> Result<Vec<Room>> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {ROOM_COLS} FROM rooms WHERE kind = 'public' AND id NOT IN (SELECT room_id FROM participants WHERE user_id = ?) ORDER BY last_activity_at DESC LIMIT 100"
        )))
        .bind(user_id)
        .fetch_all(&self.db)
        .await?)
    }

    pub async fn participants(&self, room_id: &str) -> Result<Vec<Participant>> {
        Ok(sqlx::query_as(
            "SELECT p.user_id, u.username, u.display_name, p.role, p.last_read_message_id FROM participants p
             JOIN users u ON u.id = p.user_id WHERE p.room_id = ? ORDER BY u.display_name COLLATE NOCASE",
        )
        .bind(room_id)
        .fetch_all(&self.db)
        .await?)
    }

    async fn add_participant_internal(&self, room_id: &str, user_id: &str, role: &str) -> Result<()> {
        sqlx::query("INSERT OR IGNORE INTO participants (room_id, user_id, role, joined_at) VALUES (?, ?, ?, ?)")
            .bind(room_id)
            .bind(user_id)
            .bind(role)
            .bind(now())
            .execute(&self.db)
            .await?;
        self.talk.audience.remove(room_id);
        self.hub.publish([user_id], &json!({"type": "talk.rooms_changed"}));
        Ok(())
    }

    pub async fn add_participant(&self, actor_id: &str, token: &str, username: &str) -> Result<()> {
        let room = self.room_for(actor_id, token).await?;
        if room.kind == "one_to_one" || room.kind == "note_to_self" {
            return Err(Error::bad("cannot add people to this conversation"));
        }
        self.require_moderator(&room, actor_id).await?;
        let user = self.user_by_name(username).await.map_err(|_| Error::bad("no such user"))?;
        if self.participant_role(&room.id, &user.id).await?.is_some() {
            return Ok(());
        }
        self.add_participant_internal(&room.id, &user.id, "member").await?;
        let adder = self.user(actor_id).await?;
        self.post_system(&room.id, &format!("{} added {}", adder.display_name, user.display_name))
            .await?;
        self.notify(
            &user.id,
            "talk",
            &format!("{} added you to “{}”", adder.display_name, room.name),
            "",
            &format!("/talk/{}", room.token),
        )
        .await?;
        Ok(())
    }

    /// Joins a public room as a user. A password-protected room needs its password, unless the
    /// user is already a participant.
    pub async fn join_public(&self, user_id: &str, token: &str, password: Option<&str>) -> Result<Room> {
        let room = self.room_by_token(token).await?;
        if room.kind != "public" {
            return Err(Error::NotFound);
        }
        if self.participant_role(&room.id, user_id).await?.is_none() {
            self.check_room_password(&room, password).await?;
            self.add_participant_internal(&room.id, user_id, "member").await?;
            let u = self.user(user_id).await?;
            self.post_system(&room.id, &format!("{} joined", u.display_name)).await?;
        }
        Ok(room)
    }

    pub async fn remove_participant(&self, actor_id: &str, token: &str, user_id: &str) -> Result<()> {
        let room = self.room_for(actor_id, token).await?;
        if actor_id != user_id {
            self.require_moderator(&room, actor_id).await?;
        }
        sqlx::query("DELETE FROM participants WHERE room_id = ? AND user_id = ?")
            .bind(&room.id)
            .bind(user_id)
            .execute(&self.db)
            .await?;
        self.talk.audience.remove(&room.id);
        // Leaving a conversation also leaves its call and its breakout rooms.
        self.end_actor_calls(&room.id, "user", user_id, "removed").await;
        self.leave_breakouts(&room, user_id).await?;
        self.hub.publish([user_id], &json!({"type": "talk.rooms_changed"}));
        let u = self.user(user_id).await?;
        let text = if actor_id == user_id {
            format!("{} left", u.display_name)
        } else {
            format!("{} was removed", u.display_name)
        };
        self.post_system(&room.id, &text).await?;
        // Remaining owners keep the room alive; an empty group room is deleted.
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM participants WHERE room_id = ?")
            .bind(&room.id)
            .fetch_one(&self.db)
            .await?;
        if left == 0 && room.kind != "one_to_one" {
            sqlx::query("DELETE FROM rooms WHERE id = ?").bind(&room.id).execute(&self.db).await?;
        }
        Ok(())
    }

    pub async fn set_participant_role(&self, actor_id: &str, token: &str, user_id: &str, role: &str) -> Result<()> {
        if !matches!(role, "moderator" | "member") {
            return Err(Error::bad("role must be moderator or member"));
        }
        let room = self.room_for(actor_id, token).await?;
        self.require_moderator(&room, actor_id).await?;
        sqlx::query("UPDATE participants SET role = ? WHERE room_id = ? AND user_id = ? AND role != 'owner'")
            .bind(role)
            .bind(&room.id)
            .bind(user_id)
            .execute(&self.db)
            .await?;
        self.talk.audience.remove(&room.id);
        // Moderators of a main conversation moderate its breakout rooms too.
        self.sync_breakout_moderators(&room).await?;
        self.hub.publish([user_id], &json!({"type": "talk.rooms_changed"}));
        Ok(())
    }

    pub async fn update_room(
        &self,
        actor_id: &str,
        token: &str,
        name: Option<&str>,
        description: Option<&str>,
        read_only: Option<bool>,
        ai_enabled: Option<bool>,
    ) -> Result<Room> {
        let room = self.room_for(actor_id, token).await?;
        self.require_moderator(&room, actor_id).await?;
        if let Some(n) = name.map(str::trim).filter(|n| !n.is_empty()) {
            sqlx::query("UPDATE rooms SET name = ? WHERE id = ?")
                .bind(n)
                .bind(&room.id)
                .execute(&self.db)
                .await?;
        }
        if let Some(d) = description {
            sqlx::query("UPDATE rooms SET description = ? WHERE id = ?")
                .bind(d)
                .bind(&room.id)
                .execute(&self.db)
                .await?;
        }
        if let Some(r) = read_only {
            sqlx::query("UPDATE rooms SET read_only = ? WHERE id = ?")
                .bind(r)
                .bind(&room.id)
                .execute(&self.db)
                .await?;
        }
        if let Some(a) = ai_enabled {
            sqlx::query("UPDATE rooms SET ai_enabled = ? WHERE id = ?")
                .bind(a)
                .bind(&room.id)
                .execute(&self.db)
                .await?;
        }
        let updated = self.room(&room.id).await?;
        self.publish_room(&room.id, &json!({"type": "talk.room_updated", "room": updated})).await?;
        Ok(updated)
    }

    pub async fn set_room_prefs(&self, user_id: &str, token: &str, notify: Option<&str>, favorite: Option<bool>) -> Result<()> {
        let room = self.room_for(user_id, token).await?;
        if let Some(n) = notify {
            if !matches!(n, "all" | "mentions" | "none") {
                return Err(Error::bad("notify must be all, mentions or none"));
            }
            sqlx::query("UPDATE participants SET notify = ? WHERE room_id = ? AND user_id = ?")
                .bind(n)
                .bind(&room.id)
                .bind(user_id)
                .execute(&self.db)
                .await?;
        }
        if let Some(f) = favorite {
            sqlx::query("UPDATE participants SET favorite = ? WHERE room_id = ? AND user_id = ?")
                .bind(f)
                .bind(&room.id)
                .bind(user_id)
                .execute(&self.db)
                .await?;
        }
        Ok(())
    }

    pub async fn delete_room(&self, actor_id: &str, token: &str) -> Result<()> {
        let room = self.room_for(actor_id, token).await?;
        if room.kind == "breakout" {
            return Err(Error::bad("breakout rooms are removed from their main conversation"));
        }
        if self.participant_role(&room.id, actor_id).await?.as_deref() != Some("owner") {
            return Err(Error::Forbidden);
        }
        let audience = self.room_audience(&room.id).await?;
        // Breakout rooms go with their main conversation (the foreign key cascades).
        let mut ids: Vec<String> = sqlx::query_scalar("SELECT id FROM rooms WHERE parent_id = ?")
            .bind(&room.id)
            .fetch_all(&self.db)
            .await?;
        ids.push(room.id.clone());
        for id in &ids {
            self.end_room_calls(id, "ended").await;
            sqlx::query("DELETE FROM fts WHERE object_kind = 'message' AND scope = ?")
                .bind(id)
                .execute(&self.db)
                .await?;
        }
        sqlx::query("DELETE FROM rooms WHERE id = ?").bind(&room.id).execute(&self.db).await?;
        for id in &ids {
            self.talk.audience.remove(id);
            self.talk.waiting.remove(id);
        }
        self.hub
            .publish(audience.all.iter().map(String::as_str), &json!({"type": "talk.rooms_changed"}));
        Ok(())
    }

    /// Participants and guests of a room (cached until membership, roles or the lobby
    /// change). Guests of a main conversation reach the breakout room they are assigned to.
    pub async fn room_audience(&self, room_id: &str) -> Result<Arc<Audience>> {
        if let Some(a) = self.talk.audience.get(room_id) {
            return Ok(a.clone());
        }
        let members: Vec<(String, String)> = sqlx::query_as("SELECT user_id, role FROM participants WHERE room_id = ?")
            .bind(room_id)
            .fetch_all(&self.db)
            .await?;
        let guests: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM guests WHERE room_id = ?1
             UNION SELECT actor_id FROM breakout_assignments WHERE room_id = ?1 AND actor_kind = 'guest'",
        )
        .bind(room_id)
        .fetch_all(&self.db)
        .await?;
        let (lobby, lobby_timer): (bool, Option<DateTime<Utc>>) = sqlx::query_as("SELECT lobby, lobby_timer FROM rooms WHERE id = ?")
            .bind(room_id)
            .fetch_optional(&self.db)
            .await?
            .unwrap_or((false, None));
        let moderators = members
            .iter()
            .filter(|(_, role)| role == "owner" || role == "moderator")
            .map(|(u, _)| u.clone())
            .collect();
        let mut all: Vec<String> = members.into_iter().map(|(u, _)| u).collect();
        all.extend(guests.into_iter().map(|g| format!("guest:{g}")));
        let arc = Arc::new(Audience {
            all,
            moderators,
            lobby,
            lobby_timer,
        });
        self.talk.audience.insert(room_id.to_string(), arc.clone());
        Ok(arc)
    }

    /// Sends a room event to everyone who may follow the room right now (only moderators
    /// while the lobby is on).
    pub async fn publish_room(&self, room_id: &str, event: &Value) -> Result<()> {
        let audience = self.room_audience(room_id).await?;
        self.hub.publish(audience.current().iter().map(String::as_str), event);
        Ok(())
    }

    /// Sends a room event to every participant and guest, waiting or not.
    pub async fn publish_room_all(&self, room_id: &str, event: &Value) -> Result<()> {
        let audience = self.room_audience(room_id).await?;
        self.hub.publish(audience.all.iter().map(String::as_str), event);
        Ok(())
    }

    // ---- guests --------------------------------------------------------------

    /// Creates a guest identity for a public room (which may need its password). Returns
    /// (actor, room, secret).
    pub async fn create_guest(&self, token: &str, display_name: &str, password: Option<&str>) -> Result<(Actor, Room, String)> {
        let room = self.room_by_token(token).await?;
        if room.kind != "public" {
            return Err(Error::NotFound);
        }
        self.check_room_password(&room, password).await?;
        let name = display_name.trim();
        if name.is_empty() || name.len() > 64 {
            return Err(Error::bad("guest name must be 1-64 characters"));
        }
        let id = new_id();
        let secret = format!("fg_{}", random_token(24));
        sqlx::query("INSERT INTO guests (id, room_id, display_name, secret_hash, created_at) VALUES (?, ?, ?, ?, ?)")
            .bind(&id)
            .bind(&room.id)
            .bind(format!("{name} (guest)"))
            .bind(sha256_hex(secret.as_bytes()))
            .bind(now())
            .execute(&self.db)
            .await?;
        self.talk.audience.remove(&room.id);
        let actor = Actor {
            kind: "guest".into(),
            id,
            name: format!("{name} (guest)"),
        };
        self.post_system(&room.id, &format!("{} joined", actor.name)).await?;
        Ok((actor, room, secret))
    }

    pub async fn resolve_guest(&self, secret: &str) -> Result<(Actor, Room)> {
        let row: (String, String, String) = sqlx::query_as("SELECT id, room_id, display_name FROM guests WHERE secret_hash = ?")
            .bind(sha256_hex(secret.as_bytes()))
            .fetch_optional(&self.db)
            .await?
            .ok_or(Error::Unauthorized)?;
        let room = self.room(&row.1).await.map_err(|_| Error::Unauthorized)?;
        if room.kind != "public" {
            return Err(Error::Unauthorized);
        }
        Ok((
            Actor {
                kind: "guest".into(),
                id: row.0,
                name: row.2,
            },
            room,
        ))
    }

    /// Checks that an actor may read/write a room: a participant (or guest) of it, and not
    /// held back by its lobby.
    pub async fn actor_in_room(&self, actor: &Actor, room: &Room) -> Result<()> {
        self.actor_member_of(actor, room).await?;
        if room.lobby_active() && !matches!(actor.kind.as_str(), "bot" | "system") && !self.is_moderator(actor, room).await? {
            return Err(Error::Forbidden);
        }
        Ok(())
    }

    /// Checks that an actor belongs to a room, ignoring the lobby. Guests belong to their
    /// public room and to the breakout room of it they are assigned to.
    pub async fn actor_member_of(&self, actor: &Actor, room: &Room) -> Result<()> {
        match actor.kind.as_str() {
            "user" => self.participant_role(&room.id, &actor.id).await?.map(|_| ()).ok_or(Error::NotFound),
            "guest" => {
                let ok: Option<String> = sqlx::query_scalar(
                    "SELECT id FROM guests WHERE id = ?1 AND room_id = ?2
                     UNION SELECT actor_id FROM breakout_assignments WHERE actor_kind = 'guest' AND actor_id = ?1 AND room_id = ?2",
                )
                .bind(&actor.id)
                .bind(&room.id)
                .fetch_optional(&self.db)
                .await?;
                ok.map(|_| ()).ok_or(Error::NotFound)
            }
            "bot" | "system" => Ok(()),
            _ => Err(Error::Forbidden),
        }
    }

    // ---- messages ------------------------------------------------------------

    pub async fn message(&self, id: i64) -> Result<Message> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {MSG_COLS} FROM messages WHERE id = ?")))
            .bind(id)
            .fetch_one(&self.db)
            .await?)
    }

    /// Posts a system notice (e.g. "Alice started recording the call") into a room.
    pub async fn post_system(&self, room_id: &str, text: &str) -> Result<Message> {
        self.insert_message(room_id, &Actor::system(), "system", text, None, json!({})).await
    }

    async fn insert_message(&self, room_id: &str, actor: &Actor, kind: &str, body: &str, reply_to: Option<i64>, meta: Value) -> Result<Message> {
        let t = now();
        let mut tx = self.db.begin().await?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO messages (room_id, actor_kind, actor_id, actor_name, kind, body, reply_to, meta, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(room_id)
        .bind(&actor.kind)
        .bind(&actor.id)
        .bind(&actor.name)
        .bind(kind)
        .bind(body)
        .bind(reply_to)
        .bind(Json(&meta))
        .bind(t)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("UPDATE rooms SET last_activity_at = ? WHERE id = ?")
            .bind(t)
            .bind(room_id)
            .execute(&mut *tx)
            .await?;
        if kind != "system" {
            sqlx::query("INSERT INTO fts (object_kind, object_id, scope, title, body) VALUES ('message', ?, ?, ?, ?)")
                .bind(id.to_string())
                .bind(room_id)
                .bind(&actor.name)
                .bind(body)
                .execute(&mut *tx)
                .await?;
        }
        // The author has obviously read everything up to their own message.
        if actor.kind == "user" {
            sqlx::query("UPDATE participants SET last_read_message_id = ? WHERE room_id = ? AND user_id = ?")
                .bind(id)
                .bind(room_id)
                .bind(&actor.id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        let msg = self.message(id).await?;
        let view = self.message_view(msg.clone()).await?;
        self.publish_room(room_id, &json!({"type": "talk.message", "room_id": room_id, "message": view}))
            .await?;
        let _ = self.message_events.send(Arc::new(msg.clone()));
        Ok(msg)
    }

    pub async fn post_message(&self, actor: &Actor, token: &str, nm: NewMessage) -> Result<MessageView> {
        let room = self.room_by_token(token).await?;
        self.actor_in_room(actor, &room).await?;
        let body = nm.body.trim();
        let kind = nm.kind.as_deref().unwrap_or("text");
        if !matches!(kind, "text" | "file" | "poll") {
            return Err(Error::bad("invalid message kind"));
        }
        if body.is_empty() && kind == "text" {
            return Err(Error::bad("message is empty"));
        }
        if body.len() > MAX_MESSAGE_LEN {
            return Err(Error::bad("message is too long"));
        }
        if room.read_only
            && actor.kind != "bot"
            && (actor.kind != "user" || !matches!(self.participant_role(&room.id, &actor.id).await?.as_deref(), Some("owner" | "moderator")))
        {
            return Err(Error::Forbidden);
        }
        if let Some(parent) = nm.reply_to {
            let p = self.message(parent).await.map_err(|_| Error::bad("reply target not found"))?;
            if p.room_id != room.id {
                return Err(Error::bad("reply target is in another conversation"));
            }
        }
        let msg = self
            .insert_message(&room.id, actor, kind, body, nm.reply_to, nm.meta.unwrap_or_else(|| json!({})))
            .await?;
        self.notify_mentions(&room, actor, &msg).await?;
        self.message_view(msg).await
    }

    async fn notify_mentions(&self, room: &Room, actor: &Actor, msg: &Message) -> Result<()> {
        let names = mention_names(&msg.body);
        let parts: Vec<(String, String, String)> =
            sqlx::query_as("SELECT p.user_id, u.username, p.notify FROM participants p JOIN users u ON u.id = p.user_id WHERE p.room_id = ?")
                .bind(&room.id)
                .fetch_all(&self.db)
                .await?;
        let everyone = names.iter().any(|n| n == "all");
        let room_name = room.name.clone();
        for (user_id, username, notify) in &parts {
            if *user_id == actor.id {
                continue;
            }
            let mentioned = everyone || names.iter().any(|n| n.eq_ignore_ascii_case(username));
            let wants = match notify.as_str() {
                "all" => true,
                "mentions" => mentioned || room.kind == "one_to_one",
                _ => false,
            };
            if !wants {
                continue;
            }
            let title = if room.kind == "one_to_one" {
                format!("{} sent you a message", actor.name)
            } else if mentioned {
                format!("{} mentioned you in “{room_name}”", actor.name)
            } else {
                format!("New message in “{room_name}”")
            };
            let preview: String = msg.body.chars().take(140).collect();
            let link = format!("/talk/{}", room.token);
            // Online users see the message live, so they get no stored notification, but
            // their other devices (a phone) still get the push.
            if self.hub.is_online(user_id) {
                self.push_chat(user_id, &title, &preview, &link);
            } else {
                self.notify(user_id, "talk", &title, &preview, &link).await?;
            }
        }
        Ok(())
    }

    pub async fn message_view(&self, message: Message) -> Result<MessageView> {
        let mut views = self.message_views(vec![message]).await?;
        Ok(views.pop().expect("one view per message"))
    }

    /// Views for a page of messages with two queries in total (reactions, reply parents)
    /// rather than two per message.
    pub async fn message_views(&self, messages: Vec<Message>) -> Result<Vec<MessageView>> {
        if messages.is_empty() {
            return Ok(Vec::new());
        }
        let ids = json_ids(messages.iter().map(|m| m.id));
        let rows: Vec<(i64, String, String)> =
            sqlx::query_as("SELECT message_id, emoji, actor_id FROM reactions WHERE message_id IN (SELECT value FROM json_each(?)) ORDER BY created_at")
                .bind(&ids)
                .fetch_all(&self.db)
                .await?;
        let mut reactions: HashMap<i64, BTreeMap<String, Vec<String>>> = HashMap::new();
        for (mid, emoji, actor) in rows {
            reactions.entry(mid).or_default().entry(emoji).or_default().push(actor);
        }
        let parent_ids: Vec<i64> = messages.iter().filter_map(|m| m.reply_to).collect();
        let mut parents: HashMap<i64, ParentPreview> = HashMap::new();
        if !parent_ids.is_empty() {
            let pids = json_ids(parent_ids.iter().copied());
            let rows: Vec<Message> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT {MSG_COLS} FROM messages WHERE id IN (SELECT value FROM json_each(?))"
            )))
            .bind(&pids)
            .fetch_all(&self.db)
            .await?;
            for p in rows {
                parents.insert(
                    p.id,
                    ParentPreview {
                        id: p.id,
                        actor_name: p.actor_name,
                        body: if p.deleted { String::new() } else { p.body.chars().take(200).collect() },
                        deleted: p.deleted,
                    },
                );
            }
        }
        Ok(messages
            .into_iter()
            .map(|message| {
                let parent = message.reply_to.and_then(|pid| parents.get(&pid).cloned());
                let reactions = reactions.remove(&message.id).unwrap_or_default();
                MessageView { message, reactions, parent }
            })
            .collect())
    }

    /// Messages in a room, newest last. `before`/`after` page by message id.
    pub async fn list_messages(&self, actor: &Actor, token: &str, before: Option<i64>, after: Option<i64>, limit: i64) -> Result<Vec<MessageView>> {
        let room = self.room_by_token(token).await?;
        self.actor_in_room(actor, &room).await?;
        let limit = limit.clamp(1, 200);
        let msgs: Vec<Message> = if let Some(a) = after {
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT {MSG_COLS} FROM messages WHERE room_id = ? AND id > ? ORDER BY id ASC LIMIT ?"
            )))
            .bind(&room.id)
            .bind(a)
            .bind(limit)
            .fetch_all(&self.db)
            .await?
        } else {
            let mut v: Vec<Message> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT {MSG_COLS} FROM messages WHERE room_id = ? AND id < ? ORDER BY id DESC LIMIT ?"
            )))
            .bind(&room.id)
            .bind(before.unwrap_or(i64::MAX))
            .bind(limit)
            .fetch_all(&self.db)
            .await?;
            v.reverse();
            v
        };
        self.message_views(msgs).await
    }

    pub async fn edit_message(&self, actor: &Actor, id: i64, body: &str) -> Result<MessageView> {
        let msg = self.message(id).await?;
        if msg.actor_id != actor.id || msg.deleted || msg.kind == "system" {
            return Err(Error::Forbidden);
        }
        let body = body.trim();
        if body.is_empty() || body.len() > MAX_MESSAGE_LEN {
            return Err(Error::bad("invalid message"));
        }
        sqlx::query("UPDATE messages SET body = ?, edited_at = ? WHERE id = ?")
            .bind(body)
            .bind(now())
            .bind(id)
            .execute(&self.db)
            .await?;
        sqlx::query("UPDATE fts SET body = ? WHERE object_kind = 'message' AND object_id = ?")
            .bind(body)
            .bind(id.to_string())
            .execute(&self.db)
            .await?;
        let view = self.message_view(self.message(id).await?).await?;
        self.publish_room(&msg.room_id, &json!({"type": "talk.message_updated", "room_id": msg.room_id, "message": view}))
            .await?;
        Ok(view)
    }

    pub async fn delete_message(&self, actor: &Actor, id: i64) -> Result<()> {
        let msg = self.message(id).await?;
        let room = self.room(&msg.room_id).await?;
        let is_mod = actor.kind == "user" && matches!(self.participant_role(&room.id, &actor.id).await?.as_deref(), Some("owner" | "moderator"));
        if msg.actor_id != actor.id && !is_mod {
            return Err(Error::Forbidden);
        }
        sqlx::query("UPDATE messages SET deleted = 1, body = '', meta = '{}' WHERE id = ?")
            .bind(id)
            .execute(&self.db)
            .await?;
        sqlx::query("DELETE FROM fts WHERE object_kind = 'message' AND object_id = ?")
            .bind(id.to_string())
            .execute(&self.db)
            .await?;
        sqlx::query("DELETE FROM reactions WHERE message_id = ?").bind(id).execute(&self.db).await?;
        let view = self.message_view(self.message(id).await?).await?;
        self.publish_room(&room.id, &json!({"type": "talk.message_updated", "room_id": room.id, "message": view}))
            .await?;
        Ok(())
    }

    pub async fn react(&self, actor: &Actor, id: i64, emoji: &str, add: bool) -> Result<MessageView> {
        let msg = self.message(id).await?;
        let room = self.room(&msg.room_id).await?;
        self.actor_in_room(actor, &room).await?;
        if emoji.is_empty() || emoji.chars().count() > 8 || msg.deleted {
            return Err(Error::bad("invalid reaction"));
        }
        if add {
            sqlx::query("INSERT OR IGNORE INTO reactions (message_id, actor_id, emoji, created_at) VALUES (?, ?, ?, ?)")
                .bind(id)
                .bind(&actor.id)
                .bind(emoji)
                .bind(now())
                .execute(&self.db)
                .await?;
        } else {
            sqlx::query("DELETE FROM reactions WHERE message_id = ? AND actor_id = ? AND emoji = ?")
                .bind(id)
                .bind(&actor.id)
                .bind(emoji)
                .execute(&self.db)
                .await?;
        }
        let view = self.message_view(msg).await?;
        self.publish_room(&room.id, &json!({"type": "talk.message_updated", "room_id": room.id, "message": view}))
            .await?;
        Ok(view)
    }

    pub async fn mark_read(&self, user_id: &str, token: &str, message_id: i64) -> Result<()> {
        let room = self.room_for(user_id, token).await?;
        sqlx::query("UPDATE participants SET last_read_message_id = MAX(last_read_message_id, ?) WHERE room_id = ? AND user_id = ?")
            .bind(message_id)
            .bind(&room.id)
            .bind(user_id)
            .execute(&self.db)
            .await?;
        self.publish_room(
            &room.id,
            &json!({"type": "talk.read", "room_id": room.id, "user_id": user_id, "message_id": message_id}),
        )
        .await?;
        Ok(())
    }

    /// Ephemeral typing indicator; never stored.
    pub async fn typing(&self, actor: &Actor, token: &str) -> Result<()> {
        let room = self.room_by_token(token).await?;
        self.actor_in_room(actor, &room).await?;
        let audience = self.room_audience(&room.id).await?;
        let me = actor.hub_key();
        self.hub.publish(
            audience.current().iter().filter(|k| **k != me).map(String::as_str),
            &json!({"type": "talk.typing", "room_id": room.id, "actor": actor}),
        );
        Ok(())
    }

    // ---- polls ---------------------------------------------------------------

    pub async fn create_poll(&self, actor: &Actor, token: &str, question: &str, options: &[String], multiple: bool) -> Result<MessageView> {
        let opts: Vec<String> = options.iter().map(|o| o.trim().to_string()).filter(|o| !o.is_empty()).collect();
        if question.trim().is_empty() || opts.len() < 2 || opts.len() > 20 {
            return Err(Error::bad("a poll needs a question and 2-20 options"));
        }
        let meta = json!({"question": question.trim(), "options": opts, "multiple": multiple, "votes": {}, "closed": false});
        self.post_message(
            actor,
            token,
            NewMessage {
                body: question.trim().to_string(),
                reply_to: None,
                kind: Some("poll".into()),
                meta: Some(meta),
            },
        )
        .await
    }

    pub async fn vote(&self, actor: &Actor, message_id: i64, choices: &[usize]) -> Result<MessageView> {
        let msg = self.message(message_id).await?;
        let room = self.room(&msg.room_id).await?;
        self.actor_in_room(actor, &room).await?;
        if msg.kind != "poll" || msg.deleted {
            return Err(Error::bad("not a poll"));
        }
        let mut meta = msg.meta.0.clone();
        if meta["closed"].as_bool().unwrap_or(false) {
            return Err(Error::conflict("poll is closed"));
        }
        let n = meta["options"].as_array().map(|a| a.len()).unwrap_or(0);
        let multiple = meta["multiple"].as_bool().unwrap_or(false);
        if choices.iter().any(|c| *c >= n) || (!multiple && choices.len() > 1) {
            return Err(Error::bad("invalid choice"));
        }
        meta["votes"][&actor.id] = json!(choices);
        self.update_meta(&msg, meta).await
    }

    pub async fn close_poll(&self, actor: &Actor, message_id: i64) -> Result<MessageView> {
        let msg = self.message(message_id).await?;
        if msg.kind != "poll" || msg.actor_id != actor.id {
            return Err(Error::Forbidden);
        }
        let mut meta = msg.meta.0.clone();
        meta["closed"] = json!(true);
        self.update_meta(&msg, meta).await
    }

    async fn update_meta(&self, msg: &Message, meta: Value) -> Result<MessageView> {
        sqlx::query("UPDATE messages SET meta = ? WHERE id = ?")
            .bind(Json(&meta))
            .bind(msg.id)
            .execute(&self.db)
            .await?;
        let view = self.message_view(self.message(msg.id).await?).await?;
        self.publish_room(&msg.room_id, &json!({"type": "talk.message_updated", "room_id": msg.room_id, "message": view}))
            .await?;
        Ok(view)
    }

    /// Shares a file into a conversation: every current and future participant can read it.
    pub async fn share_file_to_room(&self, user_id: &str, token: &str, node_id: &str, caption: &str) -> Result<MessageView> {
        let room = self.room_for(user_id, token).await?;
        let (node, _) = self.node_for(user_id, node_id, crate::files::perm::READ).await?;
        let existing: Option<String> = sqlx::query_scalar("SELECT id FROM shares WHERE node_id = ? AND target_room = ?")
            .bind(&node.id)
            .bind(&room.id)
            .fetch_optional(&self.db)
            .await?;
        if existing.is_none() {
            self.create_share(
                user_id,
                &node.id,
                crate::shares::NewShare {
                    kind: "room".into(),
                    target: Some(room.token.clone()),
                    permissions: Some(crate::files::perm::READ),
                    password: None,
                    expires_at: None,
                    label: None,
                },
            )
            .await?;
        }
        let me = self.user(user_id).await?;
        let meta = json!({"node_id": node.id, "name": node.name, "size": node.size, "mime": node.mime, "kind": node.kind});
        let body = if caption.trim().is_empty() {
            node.name.clone()
        } else {
            caption.trim().to_string()
        };
        self.post_message(
            &Actor::user(user_id, &me.display_name),
            token,
            NewMessage {
                body,
                reply_to: None,
                kind: Some("file".into()),
                meta: Some(meta),
            },
        )
        .await
    }

    // ---- calls ---------------------------------------------------------------

    pub fn call_members(&self, room_id: &str) -> Vec<CallMember> {
        self.talk.calls.get(room_id).map(|c| c.clone()).unwrap_or_default()
    }

    /// Joins the call of a room from one connection. Returns the new session id and the
    /// members already present (the joiner sends them offers).
    pub async fn join_call(&self, actor: &Actor, token: &str, conn: u64, audio: bool, video: bool) -> Result<(String, Vec<CallMember>, Room)> {
        let room = self.room_by_token(token).await?;
        self.actor_in_room(actor, &room).await?;
        // Who may start a call, and breakout rooms (which only call while they run).
        self.check_call_allowed(actor, &room).await?;
        let session = short_token(16);
        let member = CallMember {
            session: session.clone(),
            actor: actor.clone(),
            conn,
            audio,
            video,
            screen: false,
            hand: false,
            hand_raised_at: None,
            joined_at: now(),
        };
        let (others, started) = {
            let mut entry = self.talk.calls.entry(room.id.clone()).or_default();
            // One session per connection: rejoining replaces the previous one.
            entry.retain(|m| !(m.actor == *actor && m.conn == conn));
            let started = entry.is_empty();
            let others = entry.clone();
            entry.push(member.clone());
            (others, started)
        };
        if started {
            self.post_system(&room.id, &format!("{} started a call", actor.name)).await?;
            self.push_incoming_call(&room, actor);
        }
        self.publish_room(&room.id, &json!({"type": "call.joined", "room_id": room.id, "member": member}))
            .await?;
        Ok((session, others, room))
    }

    pub async fn leave_call(&self, room_id: &str, session: &str) -> Result<()> {
        let (left, remaining) = match self.talk.calls.get_mut(room_id) {
            Some(mut members) => {
                let before = members.len();
                members.retain(|m| m.session != session);
                (before != members.len(), members.len())
            }
            None => (false, 0),
        };
        let ended = remaining == 0;
        if ended {
            self.talk.calls.remove_if(room_id, |_, m| m.is_empty());
        }
        if left {
            self.publish_room(
                room_id,
                &json!({"type": "call.left", "room_id": room_id, "session": session, "remaining": remaining, "ended": ended}),
            )
            .await?;
            if ended {
                self.post_system(room_id, "Call ended").await?;
            }
        }
        Ok(())
    }

    /// Drops every call session belonging to a closed connection.
    pub async fn leave_calls_of_connection(&self, hub_key: &str, conn: u64) {
        let mut to_leave = Vec::new();
        for entry in self.talk.calls.iter() {
            for m in entry.value() {
                if m.conn == conn && m.actor.hub_key() == hub_key {
                    to_leave.push((entry.key().clone(), m.session.clone()));
                }
            }
        }
        for (room, session) in to_leave {
            let _ = self.leave_call(&room, &session).await;
        }
    }

    pub async fn update_call_state(
        &self,
        room_id: &str,
        session: &str,
        audio: Option<bool>,
        video: Option<bool>,
        screen: Option<bool>,
        hand: Option<bool>,
    ) -> Result<()> {
        let member = {
            let mut members = self.talk.calls.get_mut(room_id).ok_or(Error::NotFound)?;
            let m = members.iter_mut().find(|m| m.session == session).ok_or(Error::NotFound)?;
            if let Some(a) = audio {
                m.audio = a;
            }
            if let Some(v) = video {
                m.video = v;
            }
            if let Some(s) = screen {
                m.screen = s;
            }
            if let Some(h) = hand {
                if h && !m.hand {
                    m.hand_raised_at = Some(now());
                } else if !h {
                    m.hand_raised_at = None;
                }
                m.hand = h;
            }
            m.clone()
        };
        self.publish_room(room_id, &json!({"type": "call.state", "room_id": room_id, "member": member}))
            .await
    }

    /// Relays a WebRTC signaling payload (offer/answer/ICE) between two sessions of the same call.
    pub fn relay_signal(&self, room_id: &str, from_session: &str, to_session: &str, payload: Value) -> Result<()> {
        let members = self.talk.calls.get(room_id).ok_or(Error::NotFound)?;
        let from = members.iter().find(|m| m.session == from_session).ok_or(Error::Forbidden)?;
        let to = members.iter().find(|m| m.session == to_session).ok_or(Error::NotFound)?;
        let ev: Arc<str> =
            Arc::from(json!({"type": "call.signal", "room_id": room_id, "from": from_session, "from_actor": from.actor, "payload": payload}).to_string());
        self.hub.send_raw(&to.actor.hub_key(), &ev, Some(to.conn));
        Ok(())
    }

    /// The call session `session` belongs to `hub_key`/`conn` (prevents spoofed signaling).
    pub fn owns_session(&self, room_id: &str, session: &str, hub_key: &str, conn: u64) -> bool {
        self.talk
            .calls
            .get(room_id)
            .is_some_and(|m| m.iter().any(|m| m.session == session && m.conn == conn && m.actor.hub_key() == hub_key))
    }
}

#[cfg(test)]
mod tests {
    use super::mention_names;

    #[test]
    fn mentions_are_parsed() {
        assert_eq!(mention_names("hi @alice and @bob.smith, also @all."), vec!["alice", "bob.smith", "all"]);
        assert!(mention_names("mail me at a@b.com").is_empty());
    }
}

/// Whether `body` contains an `@name` mention, using the web client's token rules: the `@` must
/// not follow a word character, and the name is the longest run of `[A-Za-z0-9._-]` ending in an
/// alphanumeric (so a trailing full stop is punctuation, but `@bobby` is not `@bob`).
pub fn mentions(body: &str, name: &str) -> bool {
    let bytes = body.as_bytes();
    let token = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-');
    let mut i = 0;
    while let Some(off) = body[i..].find('@') {
        let at = i + off;
        i = at + 1;
        if at > 0 && (bytes[at - 1].is_ascii_alphanumeric() || matches!(bytes[at - 1], b'_' | b'@')) {
            continue;
        }
        let mut end = at + 1;
        while end < bytes.len() && token(bytes[end]) {
            end += 1;
        }
        while end > at + 1 && !bytes[end - 1].is_ascii_alphanumeric() {
            end -= 1;
        }
        if body[at + 1..end].eq_ignore_ascii_case(name) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod mention_tests {
    use super::mentions;

    #[test]
    fn mention_tokens() {
        assert!(mentions("hi @bob", "bob"));
        assert!(mentions("thanks @Bob.", "bob"));
        assert!(mentions("@bob, look", "bob"));
        assert!(!mentions("hi @bobby", "bob"));
        assert!(!mentions("mail bob@bob.org", "bob"));
        assert!(mentions("ping @all please", "all"));
        assert!(!mentions("@ballot", "all"));
        assert!(mentions("@bob.smith hi", "bob.smith"));
        assert!(!mentions("@bob.smith hi", "bob"));
    }
}

/// A JSON array of ids, for `IN (SELECT value FROM json_each(?))` with a single bind.
fn json_ids(ids: impl Iterator<Item = i64>) -> String {
    format!("[{}]", ids.map(|i| i.to_string()).collect::<Vec<_>>().join(","))
}
