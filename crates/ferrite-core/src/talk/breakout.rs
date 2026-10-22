//! Breakout rooms: a moderator splits a group or public conversation into N smaller rooms for
//! a while, then brings everyone back.
//!
//! Each breakout room is an ordinary room of kind `breakout` linked to its main conversation
//! (`parent_id`), so chat, files and calls reuse everything rooms already do. Moderators of
//! the main conversation are moderators of every breakout room; assigned users are its
//! participants; guests reach it through `breakout_assignments`. Assignment is automatic
//! (balanced), manual (by a moderator) or free (participants pick a room while they run).
//! Breakout chats stay readable after the rooms stop, until they are removed or set up anew.

use super::{Actor, ROOM_COLS, Room};
use crate::util::{new_id, now, short_token};
use crate::{Core, Error, Result};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;

pub const MAX_BREAKOUT_ROOMS: usize = 20;
/// Longest breakout session a timer can be set for.
pub const MAX_BREAKOUT_MINUTES: i64 = 24 * 60;

/// Someone who can be assigned to a breakout room.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BreakoutMember {
    pub kind: String,
    pub id: String,
    pub name: String,
    pub online: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BreakoutRoomView {
    pub id: String,
    pub token: String,
    pub name: String,
    pub help_requested_at: Option<DateTime<Utc>>,
    pub call_active: bool,
    pub members: Vec<BreakoutMember>,
}

/// The breakout rooms of a main conversation, as everyone in it sees them.
#[derive(Debug, Clone, Serialize)]
pub struct BreakoutView {
    pub room_id: String,
    pub token: String,
    /// "auto" | "manual" | "free", or None when no breakout rooms are set up.
    pub mode: Option<String>,
    /// "stopped" | "running"
    pub status: String,
    pub ends_at: Option<DateTime<Utc>>,
    pub rooms: Vec<BreakoutRoomView>,
    /// Participants (not moderators) without a breakout room.
    pub unassigned: Vec<BreakoutMember>,
}

/// One assignment change: `room` is a breakout room token, or None for "stay in the main room".
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Assignment {
    pub kind: String,
    pub id: String,
    pub room: Option<String>,
}

impl Core {
    /// A main conversation the user moderates, by token.
    async fn breakout_parent(&self, actor_id: &str, token: &str) -> Result<Room> {
        let room = self.room_for(actor_id, token).await?;
        if !room.is_groupish() {
            return Err(Error::bad("breakout rooms need a group or public conversation"));
        }
        self.require_moderator(&room, actor_id).await?;
        Ok(room)
    }

    async fn breakout_children(&self, parent_id: &str) -> Result<Vec<Room>> {
        Ok(
            sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {ROOM_COLS} FROM rooms WHERE parent_id = ? ORDER BY rowid")))
                .bind(parent_id)
                .fetch_all(&self.db)
                .await?,
        )
    }

    /// Non-moderator participants of a main conversation and its guests who are online now:
    /// the people breakout rooms are for.
    async fn breakout_candidates(&self, parent: &Room) -> Result<Vec<BreakoutMember>> {
        let users: Vec<(String, String)> = sqlx::query_as(
            "SELECT p.user_id, u.display_name FROM participants p JOIN users u ON u.id = p.user_id
             WHERE p.room_id = ? AND p.role = 'member' ORDER BY u.display_name COLLATE NOCASE",
        )
        .bind(&parent.id)
        .fetch_all(&self.db)
        .await?;
        let guests: Vec<(String, String)> = sqlx::query_as("SELECT id, display_name FROM guests WHERE room_id = ? ORDER BY created_at")
            .bind(&parent.id)
            .fetch_all(&self.db)
            .await?;
        let mut out: Vec<BreakoutMember> = users
            .into_iter()
            .map(|(id, name)| BreakoutMember {
                online: self.hub.is_online(&id),
                kind: "user".into(),
                id,
                name,
            })
            .collect();
        out.extend(
            guests
                .into_iter()
                .filter(|(id, _)| self.hub.is_online(&format!("guest:{id}")))
                .map(|(id, name)| BreakoutMember {
                    kind: "guest".into(),
                    id,
                    name,
                    online: true,
                }),
        );
        Ok(out)
    }

    /// The breakout rooms of a main conversation (`token` may also name one of its breakout
    /// rooms). Anyone who may read the main conversation may see them.
    pub async fn breakouts(&self, actor: &Actor, token: &str) -> Result<BreakoutView> {
        let mut room = self.room_by_token(token).await?;
        if room.kind == "breakout" {
            self.actor_member_of(actor, &room).await?;
            room = self.room(room.parent_id.as_deref().unwrap_or_default()).await?;
            if self.actor_member_of(actor, &room).await.is_err() {
                return Err(Error::NotFound);
            }
        } else {
            self.actor_in_room(actor, &room).await?;
        }
        self.breakout_view(&room).await
    }

    pub async fn breakout_view(&self, parent: &Room) -> Result<BreakoutView> {
        let parent = self.room(&parent.id).await?;
        let children = self.breakout_children(&parent.id).await?;
        let rows: Vec<(String, String, String)> = sqlx::query_as("SELECT actor_kind, actor_id, room_id FROM breakout_assignments WHERE parent_id = ?")
            .bind(&parent.id)
            .fetch_all(&self.db)
            .await?;
        let mut names: HashMap<(String, String), String> = HashMap::new();
        for (kind, id, _) in &rows {
            let name: Option<String> = if kind == "user" {
                sqlx::query_scalar("SELECT display_name FROM users WHERE id = ?")
            } else {
                sqlx::query_scalar("SELECT display_name FROM guests WHERE id = ?")
            }
            .bind(id)
            .fetch_optional(&self.db)
            .await?;
            names.insert((kind.clone(), id.clone()), name.unwrap_or_else(|| "Unknown".into()));
        }
        let rooms = children
            .iter()
            .map(|c| {
                let mut members: Vec<BreakoutMember> = rows
                    .iter()
                    .filter(|(_, _, r)| *r == c.id)
                    .map(|(kind, id, _)| BreakoutMember {
                        name: names.get(&(kind.clone(), id.clone())).cloned().unwrap_or_default(),
                        online: self.hub.is_online(&if kind == "guest" { format!("guest:{id}") } else { id.clone() }),
                        kind: kind.clone(),
                        id: id.clone(),
                    })
                    .collect();
                members.sort_by_key(|m| m.name.to_lowercase());
                BreakoutRoomView {
                    id: c.id.clone(),
                    token: c.token.clone(),
                    name: c.name.clone(),
                    help_requested_at: c.help_requested_at,
                    call_active: self.talk.calls.get(&c.id).is_some_and(|m| !m.is_empty()),
                    members,
                }
            })
            .collect();
        let unassigned = if children.is_empty() {
            Vec::new()
        } else {
            self.breakout_candidates(&parent)
                .await?
                .into_iter()
                .filter(|m| !rows.iter().any(|(k, i, _)| *k == m.kind && *i == m.id))
                .collect()
        };
        Ok(BreakoutView {
            room_id: parent.id.clone(),
            token: parent.token.clone(),
            mode: parent.breakout_mode.clone(),
            status: parent.breakout_status.clone(),
            ends_at: parent.breakout_ends_at,
            rooms,
            unassigned,
        })
    }

    /// Tells everyone in the main conversation and its breakout rooms the new state.
    async fn publish_breakouts(&self, parent: &Room) -> Result<BreakoutView> {
        let view = self.breakout_view(parent).await?;
        let ev = json!({"type": "talk.breakouts", "room_id": parent.id, "breakouts": view});
        self.publish_room_all(&parent.id, &ev).await?;
        Ok(view)
    }

    /// Sets up `count` breakout rooms (replacing earlier ones and their chats). In automatic
    /// mode participants are spread evenly right away; moderators can still move them.
    pub async fn configure_breakouts(&self, actor_id: &str, token: &str, mode: &str, count: usize) -> Result<BreakoutView> {
        let parent = self.breakout_parent(actor_id, token).await?;
        if !matches!(mode, "auto" | "manual" | "free") {
            return Err(Error::bad("mode must be auto, manual or free"));
        }
        if !(1..=MAX_BREAKOUT_ROOMS).contains(&count) {
            return Err(Error::bad(format!("between 1 and {MAX_BREAKOUT_ROOMS} breakout rooms")));
        }
        if parent.breakout_status == "running" {
            return Err(Error::conflict("stop the breakout rooms before setting them up again"));
        }
        self.delete_breakout_rooms(&parent).await?;
        let moderators: Vec<String> = sqlx::query_scalar("SELECT user_id FROM participants WHERE room_id = ? AND role IN ('owner', 'moderator')")
            .bind(&parent.id)
            .fetch_all(&self.db)
            .await?;
        let mut tx = self.db.begin().await?;
        let mut ids = Vec::with_capacity(count);
        let t = now();
        for i in 1..=count {
            let id = new_id();
            sqlx::query(
                "INSERT INTO rooms (id, token, kind, name, created_by, created_at, last_activity_at, ai_enabled, parent_id, read_only)
                 VALUES (?, ?, 'breakout', ?, ?, ?, ?, ?, ?, 1)",
            )
            .bind(&id)
            .bind(short_token(10))
            .bind(format!("Room {i}"))
            .bind(actor_id)
            .bind(t)
            .bind(t)
            .bind(parent.ai_enabled)
            .bind(&parent.id)
            .execute(&mut *tx)
            .await?;
            for m in &moderators {
                sqlx::query("INSERT INTO participants (room_id, user_id, role, joined_at) VALUES (?, ?, 'moderator', ?)")
                    .bind(&id)
                    .bind(m)
                    .bind(t)
                    .execute(&mut *tx)
                    .await?;
            }
            ids.push(id);
        }
        sqlx::query("UPDATE rooms SET breakout_mode = ?, breakout_status = 'stopped', breakout_ends_at = NULL WHERE id = ?")
            .bind(mode)
            .bind(&parent.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        if mode == "auto" {
            self.assign_evenly(&parent, &ids).await?;
        }
        let me = self.user(actor_id).await?;
        let how = match mode {
            "auto" => "assigned automatically",
            "manual" => "assigned by moderators",
            _ => "chosen by participants",
        };
        self.post_system(
            &parent.id,
            &format!(
                "{} set up {count} breakout {} ({how})",
                me.display_name,
                if count == 1 { "room" } else { "rooms" }
            ),
        )
        .await?;
        self.publish_breakouts(&parent).await
    }

    /// Spreads unassigned candidates over the rooms, filling the emptiest first (ties in room
    /// order), after shuffling them so groups differ from one session to the next.
    async fn assign_evenly(&self, parent: &Room, room_ids: &[String]) -> Result<()> {
        if room_ids.is_empty() {
            return Ok(());
        }
        let assigned: Vec<(String, String, String)> = sqlx::query_as("SELECT actor_kind, actor_id, room_id FROM breakout_assignments WHERE parent_id = ?")
            .bind(&parent.id)
            .fetch_all(&self.db)
            .await?;
        let mut load: Vec<usize> = room_ids.iter().map(|r| assigned.iter().filter(|(_, _, x)| x == r).count()).collect();
        let mut todo: Vec<BreakoutMember> = self
            .breakout_candidates(parent)
            .await?
            .into_iter()
            .filter(|m| !assigned.iter().any(|(k, i, _)| *k == m.kind && *i == m.id))
            .collect();
        let mut keys = vec![0u8; todo.len() * 8];
        rand::fill(&mut keys[..]);
        let mut keyed: Vec<(u64, BreakoutMember)> = todo
            .drain(..)
            .enumerate()
            .map(|(i, m)| (u64::from_le_bytes(keys[i * 8..i * 8 + 8].try_into().expect("8 bytes")), m))
            .collect();
        keyed.sort_by_key(|(k, _)| *k);
        for (_, m) in keyed {
            let (slot, _) = load.iter().enumerate().min_by_key(|(i, n)| (**n, *i)).expect("at least one room");
            self.set_assignment(parent, &m.kind, &m.id, Some(&room_ids[slot])).await?;
            load[slot] += 1;
        }
        Ok(())
    }

    /// Puts one person in a breakout room (or back in the main room with `None`), keeping
    /// breakout participants in step. Returns whether anything changed.
    async fn set_assignment(&self, parent: &Room, kind: &str, id: &str, room_id: Option<&str>) -> Result<bool> {
        let current: Option<String> = sqlx::query_scalar("SELECT room_id FROM breakout_assignments WHERE parent_id = ? AND actor_kind = ? AND actor_id = ?")
            .bind(&parent.id)
            .bind(kind)
            .bind(id)
            .fetch_optional(&self.db)
            .await?;
        if current.as_deref() == room_id {
            return Ok(false);
        }
        if let Some(old) = &current {
            sqlx::query("DELETE FROM breakout_assignments WHERE parent_id = ? AND actor_kind = ? AND actor_id = ?")
                .bind(&parent.id)
                .bind(kind)
                .bind(id)
                .execute(&self.db)
                .await?;
            if kind == "user" {
                sqlx::query("DELETE FROM participants WHERE room_id = ? AND user_id = ? AND role = 'member'")
                    .bind(old)
                    .bind(id)
                    .execute(&self.db)
                    .await?;
            }
            self.end_actor_calls(old, kind, id, "breakout").await;
            self.talk.audience.remove(old);
        }
        if let Some(new) = room_id {
            sqlx::query("INSERT INTO breakout_assignments (parent_id, actor_kind, actor_id, room_id) VALUES (?, ?, ?, ?)")
                .bind(&parent.id)
                .bind(kind)
                .bind(id)
                .bind(new)
                .execute(&self.db)
                .await?;
            if kind == "user" {
                sqlx::query("INSERT OR IGNORE INTO participants (room_id, user_id, role, joined_at) VALUES (?, ?, 'member', ?)")
                    .bind(new)
                    .bind(id)
                    .bind(now())
                    .execute(&self.db)
                    .await?;
            }
            self.talk.audience.remove(new);
            if parent.breakout_status == "running" {
                // Moving into a running breakout room leaves the main call.
                self.end_actor_calls(&parent.id, kind, id, "breakout").await;
            }
        }
        Ok(true)
    }

    /// A moderator assigns people to breakout rooms (any mode; `room: None` unassigns).
    pub async fn assign_breakouts(&self, actor_id: &str, token: &str, assignments: &[Assignment]) -> Result<BreakoutView> {
        let parent = self.breakout_parent(actor_id, token).await?;
        let children = self.breakout_children(&parent.id).await?;
        if children.is_empty() {
            return Err(Error::bad("set up breakout rooms first"));
        }
        let candidates = self.breakout_candidates(&parent).await?;
        for a in assignments {
            // Only participants of the main conversation (and its guests) can be assigned.
            let known = candidates.iter().any(|c| c.kind == a.kind && c.id == a.id)
                || match a.kind.as_str() {
                    "user" => self.participant_role(&parent.id, &a.id).await?.as_deref() == Some("member"),
                    "guest" => sqlx::query_scalar::<_, String>("SELECT id FROM guests WHERE id = ? AND room_id = ?")
                        .bind(&a.id)
                        .bind(&parent.id)
                        .fetch_optional(&self.db)
                        .await?
                        .is_some(),
                    _ => false,
                };
            if !known {
                return Err(Error::bad(format!("{} {} is not a participant of this conversation", a.kind, a.id)));
            }
            let target = match &a.room {
                Some(t) => Some(
                    children
                        .iter()
                        .find(|c| c.token == *t || c.id == *t)
                        .ok_or_else(|| Error::bad("no such breakout room"))?
                        .id
                        .as_str(),
                ),
                None => None,
            };
            self.set_assignment(&parent, &a.kind, &a.id, target).await?;
        }
        self.publish_breakouts(&parent).await
    }

    /// Opens the breakout rooms: assigned participants move there (their clients leave the
    /// main call and join their room). `minutes` sets a timer after which they stop.
    pub async fn start_breakouts(&self, actor_id: &str, token: &str, minutes: Option<i64>) -> Result<BreakoutView> {
        let parent = self.breakout_parent(actor_id, token).await?;
        let children = self.breakout_children(&parent.id).await?;
        if children.is_empty() {
            return Err(Error::bad("set up breakout rooms first"));
        }
        if parent.breakout_status == "running" {
            return Err(Error::conflict("breakout rooms are already running"));
        }
        if minutes.is_some_and(|m| !(1..=MAX_BREAKOUT_MINUTES).contains(&m)) {
            return Err(Error::bad(format!("the timer must be 1-{MAX_BREAKOUT_MINUTES} minutes")));
        }
        if parent.breakout_mode.as_deref() == Some("auto") {
            // People who joined since the rooms were set up.
            let ids: Vec<String> = children.iter().map(|c| c.id.clone()).collect();
            self.assign_evenly(&parent, &ids).await?;
        }
        let ends_at = minutes.map(|m| now() + Duration::minutes(m));
        sqlx::query("UPDATE rooms SET breakout_status = 'running', breakout_ends_at = ? WHERE id = ?")
            .bind(ends_at)
            .bind(&parent.id)
            .execute(&self.db)
            .await?;
        sqlx::query("UPDATE rooms SET read_only = 0, help_requested_at = NULL WHERE parent_id = ?")
            .bind(&parent.id)
            .execute(&self.db)
            .await?;
        let parent = self.room(&parent.id).await?;
        // Assigned participants leave the main call (moderators stay).
        let assigned: Vec<(String, String)> = sqlx::query_as("SELECT actor_kind, actor_id FROM breakout_assignments WHERE parent_id = ?")
            .bind(&parent.id)
            .fetch_all(&self.db)
            .await?;
        self.end_calls_where(
            &parent.id,
            "breakout",
            |m| assigned.iter().any(|(k, i)| *k == m.actor.kind && *i == m.actor.id),
            Some(&parent),
        )
        .await;
        if let Some(t) = ends_at {
            self.schedule_talk_timer(t);
        }
        let me = self.user(actor_id).await?;
        self.post_system(&parent.id, &format!("{} started the breakout rooms", me.display_name)).await?;
        for c in &children {
            self.post_system(&c.id, "The breakout room started").await?;
        }
        self.publish_breakouts(&parent).await
    }

    /// Closes the breakout rooms (`actor_id: None` when their timer ran out): their calls
    /// end, their chats become read-only, and everyone goes back to the main conversation.
    pub async fn stop_breakouts(&self, actor_id: Option<&str>, token: &str) -> Result<BreakoutView> {
        let parent = match actor_id {
            Some(a) => self.breakout_parent(a, token).await?,
            None => self.room_by_token(token).await?,
        };
        let stopped = sqlx::query("UPDATE rooms SET breakout_status = 'stopped', breakout_ends_at = NULL WHERE id = ? AND breakout_status = 'running'")
            .bind(&parent.id)
            .execute(&self.db)
            .await?
            .rows_affected();
        if stopped == 0 {
            return Err(Error::conflict("breakout rooms are not running"));
        }
        sqlx::query("UPDATE rooms SET read_only = 1, help_requested_at = NULL WHERE parent_id = ?")
            .bind(&parent.id)
            .execute(&self.db)
            .await?;
        let children = self.breakout_children(&parent.id).await?;
        for c in &children {
            self.end_room_calls(&c.id, "breakout_ended").await;
            self.post_system(&c.id, "The breakout room ended").await?;
        }
        let text = match actor_id {
            Some(a) => format!("{} stopped the breakout rooms", self.user(a).await?.display_name),
            None => "The breakout rooms ended".to_string(),
        };
        self.post_system(&parent.id, &text).await?;
        self.publish_breakouts(&parent).await
    }

    /// Stops running breakout rooms whose timer has passed.
    pub(crate) async fn stop_due_breakouts(&self) -> Result<()> {
        let due: Vec<String> =
            sqlx::query_scalar("SELECT token FROM rooms WHERE breakout_status = 'running' AND breakout_ends_at IS NOT NULL AND breakout_ends_at <= ?")
                .bind(now())
                .fetch_all(&self.db)
                .await?;
        for token in due {
            match self.stop_breakouts(None, &token).await {
                Ok(_) | Err(Error::Conflict(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Removes the (stopped) breakout rooms of a conversation, with their chats.
    pub async fn remove_breakouts(&self, actor_id: &str, token: &str) -> Result<BreakoutView> {
        let parent = self.breakout_parent(actor_id, token).await?;
        if parent.breakout_status == "running" {
            return Err(Error::conflict("stop the breakout rooms first"));
        }
        self.delete_breakout_rooms(&parent).await?;
        sqlx::query("UPDATE rooms SET breakout_mode = NULL, breakout_ends_at = NULL WHERE id = ?")
            .bind(&parent.id)
            .execute(&self.db)
            .await?;
        self.publish_breakouts(&parent).await
    }

    async fn delete_breakout_rooms(&self, parent: &Room) -> Result<()> {
        let children = self.breakout_children(&parent.id).await?;
        if children.is_empty() {
            return Ok(());
        }
        let mut audience: Vec<String> = Vec::new();
        for c in &children {
            self.end_room_calls(&c.id, "ended").await;
            audience.extend(self.room_audience(&c.id).await?.all.iter().cloned());
            sqlx::query("DELETE FROM fts WHERE object_kind = 'message' AND scope = ?")
                .bind(&c.id)
                .execute(&self.db)
                .await?;
        }
        sqlx::query("DELETE FROM rooms WHERE parent_id = ?").bind(&parent.id).execute(&self.db).await?;
        for c in &children {
            self.talk.audience.remove(&c.id);
        }
        self.hub.publish(audience.iter().map(String::as_str), &json!({"type": "talk.rooms_changed"}));
        Ok(())
    }

    /// Posts a moderator's message into every breakout room.
    pub async fn broadcast_to_breakouts(&self, actor_id: &str, token: &str, body: &str) -> Result<usize> {
        let parent = self.breakout_parent(actor_id, token).await?;
        let body = body.trim();
        if body.is_empty() || body.len() > super::MAX_MESSAGE_LEN {
            return Err(Error::bad("the message must not be empty"));
        }
        if parent.breakout_status != "running" {
            return Err(Error::conflict("breakout rooms are not running"));
        }
        let me = self.user(actor_id).await?;
        let actor = Actor::user(actor_id, &me.display_name);
        let children = self.breakout_children(&parent.id).await?;
        for c in &children {
            self.insert_message(&c.id, &actor, "text", body, None, json!({"broadcast": true})).await?;
        }
        Ok(children.len())
    }

    /// A participant picks their breakout room (free choice while the rooms run).
    pub async fn choose_breakout(&self, actor: &Actor, token: &str, child: &str) -> Result<BreakoutView> {
        let parent = self.room_by_token(token).await?;
        self.actor_in_room(actor, &parent).await?;
        if self.is_moderator(actor, &parent).await? {
            return Err(Error::bad("moderators can visit every breakout room"));
        }
        if parent.breakout_mode.as_deref() != Some("free") || parent.breakout_status != "running" {
            return Err(Error::Forbidden);
        }
        let children = self.breakout_children(&parent.id).await?;
        let target = children.iter().find(|c| c.token == child).ok_or(Error::NotFound)?;
        if !matches!(actor.kind.as_str(), "user" | "guest") {
            return Err(Error::Forbidden);
        }
        self.set_assignment(&parent, &actor.kind, &actor.id, Some(&target.id)).await?;
        self.publish_breakouts(&parent).await
    }

    /// Someone in a running breakout room asks the moderators for help (`on: false` withdraws
    /// the request; moderators dismiss it the same way).
    pub async fn request_breakout_help(&self, actor: &Actor, child_token: &str, on: bool) -> Result<BreakoutView> {
        let child = self.room_by_token(child_token).await?;
        if child.kind != "breakout" {
            return Err(Error::bad("help is asked from a breakout room"));
        }
        self.actor_in_room(actor, &child).await?;
        let parent = self.room(child.parent_id.as_deref().unwrap_or_default()).await?;
        if parent.breakout_status != "running" {
            return Err(Error::conflict("breakout rooms are not running"));
        }
        sqlx::query("UPDATE rooms SET help_requested_at = ? WHERE id = ?")
            .bind(if on { Some(now()) } else { None })
            .bind(&child.id)
            .execute(&self.db)
            .await?;
        if on {
            let mods: Vec<String> = sqlx::query_scalar("SELECT user_id FROM participants WHERE room_id = ? AND role IN ('owner', 'moderator')")
                .bind(&parent.id)
                .fetch_all(&self.db)
                .await?;
            let link = format!("/talk/{}", child.token);
            let ev = json!({"type": "talk.breakout_help", "room_id": parent.id, "room": {"id": child.id, "token": child.token, "name": child.name}, "actor": actor, "link": link});
            self.hub.publish(mods.iter().map(String::as_str), &ev);
            for m in &mods {
                // Online moderators get the live notice; the others find it in notifications.
                if !self.hub.is_online(m) {
                    self.notify(m, "talk", &format!("{} asks for help in {}", actor.name, child.name), &parent.name, &link)
                        .await?;
                }
            }
            self.post_system(&child.id, &format!("{} asked a moderator for help", actor.name)).await?;
        }
        self.publish_breakouts(&parent).await
    }

    /// Whether `actor` may join the call of `room` as far as breakout rooms go: a breakout
    /// room calls only while running, and people assigned to one leave the main call.
    pub(crate) async fn check_breakout_call(&self, actor: &Actor, room: &Room, is_mod: bool) -> Result<()> {
        if room.kind == "breakout" {
            let parent = self.room(room.parent_id.as_deref().unwrap_or_default()).await?;
            if parent.breakout_status != "running" {
                return Err(Error::conflict("this breakout room has ended"));
            }
            return Ok(());
        }
        if room.breakout_status == "running" && !is_mod {
            let assigned: Option<String> =
                sqlx::query_scalar("SELECT room_id FROM breakout_assignments WHERE parent_id = ? AND actor_kind = ? AND actor_id = ?")
                    .bind(&room.id)
                    .bind(&actor.kind)
                    .bind(&actor.id)
                    .fetch_optional(&self.db)
                    .await?;
            if assigned.is_some() {
                return Err(Error::conflict("breakout rooms are running: join your breakout room"));
            }
        }
        Ok(())
    }

    /// A user left (or was removed from) a main conversation: drop them from its breakout rooms.
    pub(crate) async fn leave_breakouts(&self, parent: &Room, user_id: &str) -> Result<()> {
        for c in self.breakout_children(&parent.id).await? {
            self.end_actor_calls(&c.id, "user", user_id, "removed").await;
            sqlx::query("DELETE FROM participants WHERE room_id = ? AND user_id = ?")
                .bind(&c.id)
                .bind(user_id)
                .execute(&self.db)
                .await?;
            self.talk.audience.remove(&c.id);
        }
        sqlx::query("DELETE FROM breakout_assignments WHERE parent_id = ? AND actor_kind = 'user' AND actor_id = ?")
            .bind(&parent.id)
            .bind(user_id)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// Keeps the moderators of every breakout room equal to those of its main conversation
    /// (after a promotion or demotion there).
    pub(crate) async fn sync_breakout_moderators(&self, parent: &Room) -> Result<()> {
        let children = self.breakout_children(&parent.id).await?;
        if children.is_empty() {
            return Ok(());
        }
        let roles: Vec<(String, String)> = sqlx::query_as("SELECT user_id, role FROM participants WHERE room_id = ?")
            .bind(&parent.id)
            .fetch_all(&self.db)
            .await?;
        for c in &children {
            for (uid, role) in &roles {
                let assigned: Option<String> =
                    sqlx::query_scalar("SELECT room_id FROM breakout_assignments WHERE parent_id = ? AND actor_kind = 'user' AND actor_id = ?")
                        .bind(&parent.id)
                        .bind(uid)
                        .fetch_optional(&self.db)
                        .await?;
                if role == "owner" || role == "moderator" {
                    sqlx::query(
                        "INSERT INTO participants (room_id, user_id, role, joined_at) VALUES (?, ?, 'moderator', ?)
                         ON CONFLICT (room_id, user_id) DO UPDATE SET role = 'moderator'",
                    )
                    .bind(&c.id)
                    .bind(uid)
                    .bind(now())
                    .execute(&self.db)
                    .await?;
                } else if assigned.as_deref() == Some(c.id.as_str()) {
                    sqlx::query("UPDATE participants SET role = 'member' WHERE room_id = ? AND user_id = ?")
                        .bind(&c.id)
                        .bind(uid)
                        .execute(&self.db)
                        .await?;
                } else {
                    sqlx::query("DELETE FROM participants WHERE room_id = ? AND user_id = ? AND role = 'moderator'")
                        .bind(&c.id)
                        .bind(uid)
                        .execute(&self.db)
                        .await?;
                }
            }
            self.talk.audience.remove(&c.id);
        }
        Ok(())
    }
}
