//! Call recording. A moderator starts it; the SFU writes one WebM file per person as the
//! media passes through (no re-encoding). Everyone in the call sees that it is recording and
//! the conversation gets a notice. When it stops (by a moderator, when the call ends, or
//! after `MAX_DURATION`), the files go into the starting moderator's Files under
//! `Talk/Recordings/<conversation> <date time>/` and are shared into the conversation.
//! Recording needs the built-in SFU: in a peer-to-peer call the server never sees the media.

use crate::AppState;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use ferrite_core::files::WriteOpts;
use ferrite_core::talk::{Actor, Room};
use ferrite_core::{Error, Result};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

/// Recordings stop by themselves after this long.
const MAX_DURATION: Duration = Duration::from_secs(4 * 3600);

struct Active {
    started_by: String,
    started_by_name: String,
    started_at: DateTime<Utc>,
    dir: PathBuf,
    /// Display name per call session, kept for people who leave before the end.
    names: Mutex<HashMap<String, String>>,
}

#[derive(Default)]
pub struct Recordings {
    active: DashMap<String, Active>,
}

impl Recordings {
    pub fn is_recording(&self, room_id: &str) -> bool {
        self.active.contains_key(room_id)
    }

    /// Remembers who a call session belongs to, for naming the files.
    pub fn note_member(&self, room_id: &str, session: &str, name: &str) {
        if let Some(a) = self.active.get(room_id) {
            if let Ok(mut names) = a.names.lock() {
                names.insert(session.to_string(), name.to_string());
            }
        }
    }
}

/// Whether `actor` may start and stop recordings in `room`: its owner and moderators, or
/// either person in a one-to-one conversation. Guests never can.
pub async fn can_record(s: &AppState, actor: &Actor, room: &Room) -> Result<bool> {
    if actor.kind != "user" {
        return Ok(false);
    }
    let role = s.core.participant_role(&room.id, &actor.id).await?;
    Ok(matches!(role.as_deref(), Some("owner") | Some("moderator")) || (room.kind == "one_to_one" && role.is_some()))
}

fn announce(s: &AppState, room_id: &str, active: bool, by: &str) {
    let ev = json!({"type": "call.recording", "room_id": room_id, "active": active, "by": by});
    let core = s.core.clone();
    let room_id = room_id.to_string();
    tokio::spawn(async move {
        let _ = core.publish_room(&room_id, &ev).await;
    });
}

pub async fn start(s: &AppState, actor: &Actor, room_id: &str) -> Result<()> {
    let sfu = s
        .sfu
        .clone()
        .ok_or_else(|| Error::Unavailable("recording needs the built-in SFU, which is not running on this server".into()))?;
    let room = s.core.room(room_id).await?;
    if !can_record(s, actor, &room).await? {
        return Err(Error::Forbidden);
    }
    let members = s.core.call_members(room_id);
    if members.is_empty() {
        return Err(Error::bad("there is no call to record"));
    }
    if s.recordings.is_recording(room_id) {
        return Err(Error::conflict("this call is already being recorded"));
    }
    let started_at = Utc::now();
    let dir = s
        .core
        .config
        .data_dir
        .join("tmp")
        .join("recordings")
        .join(format!("{room_id}-{}", started_at.timestamp_millis()));
    let names = members.iter().map(|m| (m.session.clone(), m.actor.name.clone())).collect();
    s.recordings.active.insert(
        room_id.to_string(),
        Active {
            started_by: actor.id.clone(),
            started_by_name: actor.name.clone(),
            started_at,
            dir: dir.clone(),
            names: Mutex::new(names),
        },
    );
    let room_key = room_id.to_string();
    if let Err(e) = tokio::task::spawn_blocking(move || sfu.start_recording(&room_key, dir))
        .await
        .map_err(|e| Error::internal(e.to_string()))?
    {
        s.recordings.active.remove(room_id);
        return Err(Error::Unavailable(e));
    }
    announce(s, room_id, true, &actor.name);
    s.core.post_system(room_id, &format!("{} started recording the call", actor.name)).await?;
    // Stop by itself after the maximum duration (unless this recording ended before).
    let (s2, room_key) = (s.clone(), room_id.to_string());
    tokio::spawn(async move {
        tokio::time::sleep(MAX_DURATION).await;
        if s2.recordings.active.get(&room_key).is_some_and(|a| a.started_at == started_at) {
            if let Err(e) = stop(&s2, None, &room_key).await {
                tracing::warn!(error = %e, "stopping a recording at its time limit failed");
            }
        }
    });
    Ok(())
}

/// Stops recording and saves the files. `by` is checked like `start`; None means the server
/// stops it (the call ended or the time limit passed).
pub async fn stop(s: &AppState, by: Option<&Actor>, room_id: &str) -> Result<()> {
    let room = s.core.room(room_id).await?;
    if let Some(actor) = by {
        if !can_record(s, actor, &room).await? {
            return Err(Error::Forbidden);
        }
    }
    let Some((_, active)) = s.recordings.active.remove(room_id) else {
        return Err(Error::bad("this call is not being recorded"));
    };
    let sfu = s.sfu.clone().ok_or_else(|| Error::Unavailable("the SFU is not running".into()))?;
    let room_key = room_id.to_string();
    let files = tokio::task::spawn_blocking(move || sfu.stop_recording(&room_key))
        .await
        .map_err(|e| Error::internal(e.to_string()))?;
    let stopped_by = by.map(|a| a.name.clone());
    announce(s, room_id, false, stopped_by.as_deref().unwrap_or(""));
    // Copying long recordings into Files takes a while: do it without holding up the caller.
    let s = s.clone();
    tokio::spawn(async move {
        let result = save(&s, &room, &active, files).await;
        let _ = tokio::fs::remove_dir_all(&active.dir).await;
        let who = stopped_by
            .map(|n| format!("{n} stopped the recording"))
            .unwrap_or_else(|| "The recording stopped".to_string());
        let note = match result {
            Ok(0) => format!("{who}; nothing was recorded."),
            Ok(n) => format!("{who}; {n} file{} saved to {}'s Files.", if n == 1 { "" } else { "s" }, active.started_by_name),
            Err(e) => {
                tracing::warn!(room = %room.id, error = %e, "saving a recording failed");
                format!("{who}, but saving it failed: {e}")
            }
        };
        let _ = s.core.post_system(&room.id, &note).await;
    });
    Ok(())
}

/// Moves the finished files into the starter's Files and shares them into the conversation.
async fn save(s: &AppState, room: &Room, active: &Active, files: Vec<ferrite_sfu::RecordedFile>) -> Result<usize> {
    if files.is_empty() {
        return Ok(0);
    }
    let user = &active.started_by;
    let core = &s.core;
    let mut folder = core.ensure_root(user).await?;
    for name in ["Talk", "Recordings"] {
        folder = match core.child_by_name(user, &folder, name).await? {
            Some(n) => n.id,
            None => core.mkdir(user, &folder, name).await?.id,
        };
    }
    // Each recording gets its own folder: "<conversation> <date time>", numbered if taken.
    let title = format!("{} {}", room.name.trim(), active.started_at.format("%Y-%m-%d %H.%M"));
    let title: String = title.chars().map(|c| if matches!(c, '/' | '\\') { '-' } else { c }).collect();
    let mut n = 1;
    folder = loop {
        let name = if n == 1 { title.clone() } else { format!("{title} ({n})") };
        if core.child_by_name(user, &folder, &name).await?.is_none() {
            break core.mkdir(user, &folder, &name).await?.id;
        }
        n += 1;
    };
    let names = active.names.lock().map(|n| n.clone()).unwrap_or_default();
    let mut used: HashMap<String, usize> = HashMap::new();
    let mut saved = 0;
    for f in files {
        let person = names.get(&f.session).cloned().unwrap_or_else(|| "Participant".to_string());
        let person: String = person.chars().map(|c| if matches!(c, '/' | '\\' | '\0') { '-' } else { c }).collect();
        let n = used.entry(person.clone()).or_insert(0);
        *n += 1;
        let file_name = if *n == 1 { format!("{person}.webm") } else { format!("{person} ({n}).webm") };
        let reader = tokio::fs::File::open(&f.path)
            .await
            .map_err(|e| Error::internal(format!("reading recording: {e}")))?;
        let opts = WriteOpts {
            create_only: true,
            mime: Some("video/webm".into()),
            ..WriteOpts::user()
        };
        let node = core.write_file(user, &folder, &file_name, reader, opts).await?;
        let minutes = f.duration_ms / 60_000;
        let length = if minutes > 0 {
            format!("{minutes} min")
        } else {
            format!("{} s", f.duration_ms / 1000)
        };
        core.share_file_to_room(user, &room.token, &node.id, &format!("Call recording: {person} ({length})"))
            .await?;
        saved += 1;
    }
    Ok(saved)
}

/// Called after people leave a call: a recording stops when its call has ended.
pub async fn after_leave(s: &AppState) {
    let ended: Vec<String> = s
        .recordings
        .active
        .iter()
        .map(|e| e.key().clone())
        .filter(|room_id| s.core.call_members(room_id).is_empty())
        .collect();
    for room_id in ended {
        if let Err(e) = stop(s, None, &room_id).await {
            tracing::warn!(room = %room_id, error = %e, "saving the recording of an ended call failed");
        }
    }
}
