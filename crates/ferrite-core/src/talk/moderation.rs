//! Moderation of group and public conversations: the lobby (with an optional opening time),
//! room passwords for public rooms, who may start calls, and moderators acting on call
//! members (ask to mute, lower a hand, remove from the call).
//!
//! Every check happens here, so the REST API, the event socket, the SFU offer path and the
//! MCP tools all enforce the same rules.

use super::{Actor, CallMember, Room};
use crate::util::{hash_password, now, verify_password};
use crate::{Core, Error, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::broadcast;

/// The error message for a wrong room password (the server throttles on it).
pub const WRONG_ROOM_PASSWORD: &str = "wrong password";

/// Moderation settings to change; absent fields stay as they are.
#[derive(Debug, Default, Deserialize)]
pub struct ModerationUpdate {
    pub lobby: Option<bool>,
    /// When the lobby opens by itself; `Some(None)` clears the time.
    #[serde(default, deserialize_with = "crate::util::double_option")]
    pub lobby_timer: Option<Option<DateTime<Utc>>>,
    /// New password for a public room; `Some(None)` (or an empty string) removes it.
    #[serde(default, deserialize_with = "crate::util::double_option")]
    pub password: Option<Option<String>>,
    /// "everyone" or "moderators".
    pub call_permission: Option<String>,
}

/// Moderator actions on call members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallAction {
    /// Ask one member to mute their microphone.
    Mute,
    /// Ask every member except the moderator to mute.
    MuteAll,
    LowerHand,
    /// End the member's call session (they may join again).
    Remove,
}

impl CallAction {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "mute" => Self::Mute,
            "mute_all" => Self::MuteAll,
            "lower_hand" => Self::LowerHand,
            "remove" => Self::Remove,
            _ => return Err(Error::bad("action must be mute, mute_all, lower_hand or remove")),
        })
    }
}

impl Core {
    // ---- settings ------------------------------------------------------------------------

    /// Changes lobby, password and call settings of a group or public room (moderators only).
    pub async fn update_moderation(&self, actor_id: &str, token: &str, up: ModerationUpdate) -> Result<Room> {
        let room = self.room_for(actor_id, token).await?;
        if !room.is_groupish() {
            return Err(Error::bad("only group and public conversations have moderation settings"));
        }
        self.require_moderator(&room, actor_id).await?;
        let me = self.user(actor_id).await?;
        if let Some(p) = &up.call_permission {
            if !matches!(p.as_str(), "everyone" | "moderators") {
                return Err(Error::bad("call_permission must be everyone or moderators"));
            }
            sqlx::query("UPDATE rooms SET call_permission = ? WHERE id = ?")
                .bind(p)
                .bind(&room.id)
                .execute(&self.db)
                .await?;
        }
        if let Some(pw) = &up.password {
            if room.kind != "public" {
                return Err(Error::bad("only public conversations can have a password"));
            }
            let pw = pw.as_deref().filter(|p| !p.is_empty());
            if pw.is_some_and(|p| p.chars().count() < 4 || p.len() > 200) {
                return Err(Error::bad("the password must be 4-200 characters"));
            }
            let hash = match pw {
                Some(p) => Some(hash_password(p)?),
                None => None,
            };
            sqlx::query("UPDATE rooms SET password_hash = ? WHERE id = ?")
                .bind(&hash)
                .bind(&room.id)
                .execute(&self.db)
                .await?;
            let text = if hash.is_some() { "set a password" } else { "removed the password" };
            self.post_system(&room.id, &format!("{} {text}", me.display_name)).await?;
        }
        let was_active = room.lobby_active();
        if up.lobby.is_some() || up.lobby_timer.is_some() {
            let lobby = up.lobby.unwrap_or(room.lobby);
            let timer = match up.lobby_timer {
                Some(t) => t,
                None => room.lobby_timer,
            };
            if lobby && timer.is_some_and(|t| t <= now()) {
                return Err(Error::bad("the lobby opening time must be in the future"));
            }
            sqlx::query("UPDATE rooms SET lobby = ?, lobby_timer = ? WHERE id = ?")
                .bind(lobby)
                .bind(if lobby { timer } else { None })
                .bind(&room.id)
                .execute(&self.db)
                .await?;
            self.talk.audience.remove(&room.id);
            if let (true, Some(t)) = (lobby, timer) {
                self.schedule_talk_timer(t);
            }
        }
        let updated = self.room(&room.id).await?;
        if updated.lobby_active() != was_active {
            if updated.lobby_active() {
                // Non-moderators leave the call and wait.
                self.end_calls_where(&room.id, "lobby", |_| true, Some(&room)).await;
                self.post_system(&room.id, &format!("{} turned the lobby on", me.display_name)).await?;
            } else {
                self.talk.waiting.remove(&room.id);
                self.post_system(&room.id, &format!("{} opened the lobby", me.display_name)).await?;
            }
        }
        self.publish_lobby(&updated).await?;
        self.publish_room(&room.id, &json!({"type": "talk.room_updated", "room": updated})).await?;
        Ok(updated)
    }

    /// Tells everyone in a room (including those waiting) the lobby state.
    async fn publish_lobby(&self, room: &Room) -> Result<()> {
        self.publish_room_all(
            &room.id,
            &json!({"type": "talk.lobby", "room_id": room.id, "lobby": room.lobby_active(), "lobby_timer": room.lobby_timer, "has_password": room.has_password, "call_permission": room.call_permission}),
        )
        .await
    }

    /// Fails unless `password` matches the room's password (rooms without one accept anything).
    pub async fn check_room_password(&self, room: &Room, password: Option<&str>) -> Result<()> {
        let hash: Option<String> = sqlx::query_scalar("SELECT password_hash FROM rooms WHERE id = ?")
            .bind(&room.id)
            .fetch_one(&self.db)
            .await?;
        match hash {
            None => Ok(()),
            Some(h) => match password {
                Some(p) if verify_password(p, &h) => Ok(()),
                Some(p) if !p.is_empty() => Err(Error::bad(WRONG_ROOM_PASSWORD)),
                _ => Err(Error::bad("this conversation needs a password")),
            },
        }
    }

    // ---- timers -----------------------------------------------------------------------------

    /// Runs the Talk timers (lobby openings, breakout room ends) at `at`. The server also runs
    /// them periodically, which covers timers set before a restart.
    pub(crate) fn schedule_talk_timer(&self, at: DateTime<Utc>) {
        let core = self.clone();
        let wait = (at - now()).to_std().unwrap_or_default();
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            core.run_talk_timers().await;
        });
    }

    /// Opens lobbies whose time has come and stops breakout rooms whose time is up.
    pub async fn run_talk_timers(&self) {
        if let Err(e) = self.open_due_lobbies().await {
            tracing::warn!(error = %e, "opening scheduled lobbies failed");
        }
        if let Err(e) = self.stop_due_breakouts().await {
            tracing::warn!(error = %e, "ending breakout rooms failed");
        }
    }

    async fn open_due_lobbies(&self) -> Result<()> {
        let due: Vec<String> = sqlx::query_scalar("SELECT id FROM rooms WHERE lobby = 1 AND lobby_timer IS NOT NULL AND lobby_timer <= ?")
            .bind(now())
            .fetch_all(&self.db)
            .await?;
        for id in due {
            // Only one sweep wins the update, so the opening is announced once.
            let changed = sqlx::query("UPDATE rooms SET lobby = 0, lobby_timer = NULL WHERE id = ? AND lobby = 1")
                .bind(&id)
                .execute(&self.db)
                .await?
                .rows_affected();
            if changed == 0 {
                continue;
            }
            self.talk.audience.remove(&id);
            self.talk.waiting.remove(&id);
            let room = self.room(&id).await?;
            self.post_system(&id, "The lobby opened").await?;
            self.publish_lobby(&room).await?;
            self.publish_room(&id, &json!({"type": "talk.room_updated", "room": room})).await?;
        }
        Ok(())
    }

    // ---- lobby waiting list -----------------------------------------------------------------

    /// Registers a connection as waiting in a room's lobby, so moderators see who is there.
    pub async fn lobby_wait(&self, actor: &Actor, token: &str, conn: u64) -> Result<()> {
        let room = self.room_by_token(token).await?;
        self.actor_member_of(actor, &room).await?;
        if !room.lobby_active() || self.is_moderator(actor, &room).await? {
            return Ok(());
        }
        {
            let mut list = self.talk.waiting.entry(room.id.clone()).or_default();
            if list.iter().any(|(a, c)| a == actor && *c == conn) {
                return Ok(());
            }
            list.push((actor.clone(), conn));
        }
        self.publish_waiting(&room.id).await
    }

    /// Removes a connection from a room's lobby.
    pub async fn lobby_unwait(&self, actor: &Actor, token: &str, conn: u64) -> Result<()> {
        let room = self.room_by_token(token).await?;
        if self.drop_waiting(&room.id, |a, c| a == actor && c == conn) {
            self.publish_waiting(&room.id).await?;
        }
        Ok(())
    }

    /// Drops every lobby entry of a closed connection.
    pub async fn lobby_leave_connection(&self, hub_key: &str, conn: u64) {
        let rooms: Vec<String> = self.talk.waiting.iter().map(|e| e.key().clone()).collect();
        for room_id in rooms {
            if self.drop_waiting(&room_id, |a, c| c == conn && a.hub_key() == hub_key) {
                let _ = self.publish_waiting(&room_id).await;
            }
        }
    }

    fn drop_waiting(&self, room_id: &str, matches: impl Fn(&Actor, u64) -> bool) -> bool {
        let Some(mut list) = self.talk.waiting.get_mut(room_id) else {
            return false;
        };
        let before = list.len();
        list.retain(|(a, c)| !matches(a, *c));
        let changed = list.len() != before;
        let empty = list.is_empty();
        drop(list);
        if empty {
            self.talk.waiting.remove_if(room_id, |_, l| l.is_empty());
        }
        changed
    }

    /// Distinct people waiting in a room's lobby, in arrival order.
    pub fn waiting_in_lobby(&self, room_id: &str) -> Vec<Actor> {
        let mut out: Vec<Actor> = Vec::new();
        if let Some(list) = self.talk.waiting.get(room_id) {
            for (a, _) in list.iter() {
                if !out.contains(a) {
                    out.push(a.clone());
                }
            }
        }
        out
    }

    /// Who is waiting in the lobby (moderators only).
    pub async fn lobby_waiting(&self, actor_id: &str, token: &str) -> Result<Vec<Actor>> {
        let room = self.room_for(actor_id, token).await?;
        self.require_moderator(&room, actor_id).await?;
        Ok(self.waiting_in_lobby(&room.id))
    }

    async fn publish_waiting(&self, room_id: &str) -> Result<()> {
        let audience = self.room_audience(room_id).await?;
        let waiting = self.waiting_in_lobby(room_id);
        self.hub.publish(
            audience.moderators.iter().map(String::as_str),
            &json!({"type": "talk.lobby_waiting", "room_id": room_id, "waiting": waiting}),
        );
        Ok(())
    }

    // ---- calls --------------------------------------------------------------------------------

    /// Whether `actor` may join the call of `room` now: starting a call may be limited to
    /// moderators, and breakout rooms decide who calls where.
    pub(crate) async fn check_call_allowed(&self, actor: &Actor, room: &Room) -> Result<()> {
        if !matches!(room.kind.as_str(), "group" | "public" | "breakout") {
            return Ok(());
        }
        let is_mod = self.is_moderator(actor, room).await?;
        let active = self.talk.calls.get(&room.id).is_some_and(|c| !c.is_empty());
        if !active && room.call_permission == "moderators" && !is_mod {
            return Err(Error::conflict("only moderators can start a call in this conversation"));
        }
        self.check_breakout_call(actor, room, is_mod).await
    }

    /// Re-checks, for the SFU offer path, that a call session's owner may still be in the call.
    pub async fn check_call_session(&self, actor: &Actor, room_id: &str) -> Result<()> {
        let room = self.room(room_id).await?;
        self.actor_in_room(actor, &room).await
    }

    /// Call sessions the server ends from now on (to drop their SFU connections).
    pub fn ended_call_sessions(&self) -> broadcast::Receiver<String> {
        self.talk.ended.subscribe()
    }

    /// A moderator acts on the call of a room. `session` names the member (not for MuteAll).
    /// Returns how many members were affected.
    pub async fn moderate_call(&self, actor_id: &str, token: &str, action: CallAction, session: Option<&str>) -> Result<usize> {
        let room = self.room_for(actor_id, token).await?;
        self.require_moderator(&room, actor_id).await?;
        let me = self.user(actor_id).await?;
        let by = Actor::user(actor_id, &me.display_name);
        let members = self.call_members(&room.id);
        let targets: Vec<CallMember> = match action {
            CallAction::MuteAll => members.into_iter().filter(|m| !(m.actor.kind == "user" && m.actor.id == actor_id)).collect(),
            _ => {
                let s = session.ok_or_else(|| Error::bad("`session` is required"))?;
                vec![members.into_iter().find(|m| m.session == s).ok_or(Error::NotFound)?]
            }
        };
        for m in &targets {
            match action {
                CallAction::Mute | CallAction::MuteAll => self.send_moderated(&room.id, m, "mute", Some(&by)),
                CallAction::LowerHand => {
                    if !m.hand {
                        continue;
                    }
                    self.send_moderated(&room.id, m, "lower_hand", Some(&by));
                    self.update_call_state(&room.id, &m.session, None, None, None, Some(false)).await?;
                }
                CallAction::Remove => {
                    self.end_call_session(&room.id, m, "removed", Some(&by)).await;
                    self.post_system(&room.id, &format!("{} removed {} from the call", me.display_name, m.actor.name))
                        .await?;
                }
            }
        }
        Ok(targets.len())
    }

    /// Tells the connection of one call member what a moderator (or the server) did to it.
    fn send_moderated(&self, room_id: &str, m: &CallMember, action: &str, by: Option<&Actor>) {
        let ev: Arc<str> = Arc::from(json!({"type": "call.moderated", "room_id": room_id, "session": m.session, "action": action, "by": by}).to_string());
        self.hub.send_raw(&m.actor.hub_key(), &ev, Some(m.conn));
    }

    /// Ends one call session on the server's side: the member is told why, everyone sees them
    /// leave, and the SFU drops their media connection.
    pub(crate) async fn end_call_session(&self, room_id: &str, m: &CallMember, reason: &str, by: Option<&Actor>) {
        self.send_moderated(room_id, m, reason, by);
        if let Err(e) = self.leave_call(room_id, &m.session).await {
            tracing::warn!(error = %e, "ending a call session failed");
        }
        let _ = self.talk.ended.send(m.session.clone());
    }

    /// Ends the call sessions of members matching `pred`; with `room`, moderators of that room
    /// are kept.
    pub(crate) async fn end_calls_where(&self, room_id: &str, reason: &str, pred: impl Fn(&CallMember) -> bool, keep_moderators_of: Option<&Room>) {
        for m in self.call_members(room_id) {
            if !pred(&m) {
                continue;
            }
            if let Some(r) = keep_moderators_of {
                if self.is_moderator(&m.actor, r).await.unwrap_or(false) {
                    continue;
                }
            }
            self.end_call_session(room_id, &m, reason, None).await;
        }
    }

    /// Ends every call session of one actor in a room.
    pub(crate) async fn end_actor_calls(&self, room_id: &str, kind: &str, id: &str, reason: &str) {
        self.end_calls_where(room_id, reason, |m| m.actor.kind == kind && m.actor.id == id, None).await;
    }

    /// Ends the whole call of a room.
    pub(crate) async fn end_room_calls(&self, room_id: &str, reason: &str) {
        self.end_calls_where(room_id, reason, |_| true, None).await;
    }

    /// Raised hands in a room's call, longest waiting first.
    pub fn raised_hands(&self, room_id: &str) -> Vec<Value> {
        let mut hands: Vec<CallMember> = self.call_members(room_id).into_iter().filter(|m| m.hand).collect();
        hands.sort_by_key(|m| m.hand_raised_at);
        hands
            .into_iter()
            .map(|m| json!({"session": m.session, "actor": m.actor, "raised_at": m.hand_raised_at}))
            .collect()
    }
}
