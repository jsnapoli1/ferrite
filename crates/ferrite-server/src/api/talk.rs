use crate::AppState;
use crate::auth::{Auth, authenticate};
use crate::error::{ApiError, ApiResult};
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use ferrite_core::Error;
use ferrite_core::talk::{Actor, NewMessage};
use serde::Deserialize;
use serde_json::{Value, json};

pub mod moderation;

pub const GUEST_HEADER: &str = "x-guest-token";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/talk/rooms", get(rooms).post(create_room))
        .route("/api/talk/open", get(open_rooms))
        .route("/api/talk/direct", post(direct))
        .route("/api/talk/note-to-self", post(note_to_self))
        .route("/api/talk/rooms/{token}", get(room).patch(update_room).delete(delete_room))
        .route("/api/talk/rooms/{token}/join", post(join))
        .route("/api/talk/rooms/{token}/prefs", post(prefs))
        .route("/api/talk/rooms/{token}/participants", get(participants).post(add_participant))
        .route("/api/talk/rooms/{token}/participants/{user_id}", delete(remove_participant).patch(set_role))
        .route("/api/talk/rooms/{token}/messages", get(messages).post(post_message))
        .route("/api/talk/rooms/{token}/read", post(read))
        .route("/api/talk/rooms/{token}/typing", post(typing))
        .route("/api/talk/rooms/{token}/polls", post(create_poll))
        .route("/api/talk/rooms/{token}/share-file", post(share_file))
        .route("/api/talk/rooms/{token}/call", get(call_members))
        .route("/api/talk/messages/{id}", patch(edit).delete(delete_message))
        .route("/api/talk/messages/{id}/reactions", post(react))
        .route("/api/talk/messages/{id}/vote", post(vote))
        .route("/api/talk/messages/{id}/close", post(close_poll))
        .route("/api/talk/guest/{token}", post(guest_join).get(guest_room))
        .merge(moderation::router())
}

/// A signed-in user, or a guest of a public room (`X-Guest-Token`).
pub struct TalkActor {
    pub actor: Actor,
    pub auth: Option<Auth>,
}

impl FromRequestParts<AppState> for TalkActor {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        if let Some(secret) = parts.headers.get(GUEST_HEADER).and_then(|v| v.to_str().ok()) {
            let (actor, _) = state.core.resolve_guest(secret).await?;
            return Ok(TalkActor { actor, auth: None });
        }
        let auth = authenticate(state, &parts.headers, &parts.method, false).await?;
        auth.require_scope("talk")?;
        Ok(TalkActor {
            actor: Actor::user(&auth.user.id, &auth.user.display_name),
            auth: Some(auth),
        })
    }
}

impl TalkActor {
    fn user(&self) -> ApiResult<&Auth> {
        self.auth.as_ref().ok_or(ApiError(Error::Forbidden))
    }
}

fn ok() -> Json<Value> {
    Json(json!({"ok": true}))
}

async fn rooms(State(s): State<AppState>, a: TalkActor) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"rooms": s.core.list_rooms(&a.user()?.user.id).await?})))
}

async fn open_rooms(State(s): State<AppState>, a: TalkActor) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"rooms": s.core.listable_rooms(&a.user()?.user.id).await?})))
}

#[derive(Deserialize)]
struct NewRoom {
    name: String,
    #[serde(default = "group")]
    kind: String,
    #[serde(default)]
    invite: Vec<String>,
}

fn group() -> String {
    "group".into()
}

async fn create_room(State(s): State<AppState>, a: TalkActor, Json(b): Json<NewRoom>) -> ApiResult<Json<Value>> {
    let uid = &a.user()?.user.id;
    let room = s.core.create_room(uid, &b.kind, &b.name, &b.invite).await?;
    Ok(Json(json!({"room": s.core.room_view(uid, &room.token).await?})))
}

#[derive(Deserialize)]
struct DirectBody {
    username: String,
}

async fn direct(State(s): State<AppState>, a: TalkActor, Json(b): Json<DirectBody>) -> ApiResult<Json<Value>> {
    let uid = &a.user()?.user.id;
    let room = s.core.direct_room(uid, &b.username).await?;
    Ok(Json(json!({"room": s.core.room_view(uid, &room.token).await?})))
}

async fn note_to_self(State(s): State<AppState>, a: TalkActor) -> ApiResult<Json<Value>> {
    let uid = &a.user()?.user.id;
    let room = s.core.note_to_self(uid).await?;
    Ok(Json(json!({"room": s.core.room_view(uid, &room.token).await?})))
}

async fn room(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>) -> ApiResult<Json<Value>> {
    let uid = &a.user()?.user.id;
    let view = s.core.room_view(uid, &token).await?;
    let call = s.core.call_members(&view.room.id);
    Ok(Json(json!({"room": view, "call": call})))
}

#[derive(Deserialize)]
struct RoomUpdate {
    name: Option<String>,
    description: Option<String>,
    read_only: Option<bool>,
    ai_enabled: Option<bool>,
}

async fn update_room(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>, Json(b): Json<RoomUpdate>) -> ApiResult<Json<Value>> {
    let uid = &a.user()?.user.id;
    let r = s
        .core
        .update_room(uid, &token, b.name.as_deref(), b.description.as_deref(), b.read_only, b.ai_enabled)
        .await?;
    Ok(Json(json!({"room": r})))
}

async fn delete_room(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>) -> ApiResult<Json<Value>> {
    s.core.delete_room(&a.user()?.user.id, &token).await?;
    Ok(ok())
}

#[derive(Deserialize, Default)]
struct JoinBody {
    password: Option<String>,
}

async fn join(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>, body: axum::body::Bytes) -> ApiResult<Json<Value>> {
    let uid = &a.user()?.user.id;
    // The body is optional: only password-protected rooms need one.
    let b: JoinBody = if body.is_empty() {
        JoinBody::default()
    } else {
        serde_json::from_slice(&body).map_err(Error::from)?
    };
    moderation::with_password_throttle(&s, &token, s.core.join_public(uid, &token, b.password.as_deref())).await?;
    Ok(Json(json!({"room": s.core.room_view(uid, &token).await?})))
}

#[derive(Deserialize)]
struct Prefs {
    notify: Option<String>,
    favorite: Option<bool>,
}

async fn prefs(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>, Json(b): Json<Prefs>) -> ApiResult<Json<Value>> {
    s.core.set_room_prefs(&a.user()?.user.id, &token, b.notify.as_deref(), b.favorite).await?;
    Ok(ok())
}

async fn participants(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>) -> ApiResult<Json<Value>> {
    let room = s.core.room_by_token(&token).await?;
    s.core.actor_in_room(&a.actor, &room).await?;
    let list: Vec<Value> = s
        .core
        .participants(&room.id)
        .await?
        .into_iter()
        .map(|p| {
            let online = s.core.hub.is_online(&p.user_id);
            let mut v = serde_json::to_value(p).unwrap_or_default();
            v["online"] = json!(online);
            v
        })
        .collect();
    Ok(Json(json!({"participants": list})))
}

#[derive(Deserialize)]
struct UsernameBody {
    username: String,
}

async fn add_participant(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>, Json(b): Json<UsernameBody>) -> ApiResult<Json<Value>> {
    s.core.add_participant(&a.user()?.user.id, &token, &b.username).await?;
    Ok(ok())
}

async fn remove_participant(State(s): State<AppState>, a: TalkActor, Path((token, user_id)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    s.core.remove_participant(&a.user()?.user.id, &token, &user_id).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct RoleBody {
    role: String,
}

async fn set_role(State(s): State<AppState>, a: TalkActor, Path((token, user_id)): Path<(String, String)>, Json(b): Json<RoleBody>) -> ApiResult<Json<Value>> {
    s.core.set_participant_role(&a.user()?.user.id, &token, &user_id, &b.role).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct Page {
    before: Option<i64>,
    after: Option<i64>,
    limit: Option<i64>,
}

async fn messages(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>, Query(p): Query<Page>) -> ApiResult<Json<Value>> {
    let msgs = s.core.list_messages(&a.actor, &token, p.before, p.after, p.limit.unwrap_or(50)).await?;
    Ok(Json(json!({"messages": msgs})))
}

async fn post_message(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>, Json(b): Json<NewMessage>) -> ApiResult<Json<Value>> {
    // Clients may only post text; files and polls have their own endpoints.
    let b = NewMessage { kind: None, meta: None, ..b };
    Ok(Json(json!({"message": s.core.post_message(&a.actor, &token, b).await?})))
}

#[derive(Deserialize)]
struct ReadBody {
    message_id: i64,
}

async fn read(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>, Json(b): Json<ReadBody>) -> ApiResult<Json<Value>> {
    if let Some(auth) = &a.auth {
        s.core.mark_read(&auth.user.id, &token, b.message_id).await?;
    }
    Ok(ok())
}

async fn typing(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>) -> ApiResult<Json<Value>> {
    s.core.typing(&a.actor, &token).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct PollBody {
    question: String,
    options: Vec<String>,
    #[serde(default)]
    multiple: bool,
}

async fn create_poll(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>, Json(b): Json<PollBody>) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({"message": s.core.create_poll(&a.actor, &token, &b.question, &b.options, b.multiple).await?}),
    ))
}

#[derive(Deserialize)]
struct ShareFileBody {
    node_id: String,
    #[serde(default)]
    caption: String,
}

async fn share_file(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>, Json(b): Json<ShareFileBody>) -> ApiResult<Json<Value>> {
    let uid = &a.user()?.user.id;
    Ok(Json(json!({"message": s.core.share_file_to_room(uid, &token, &b.node_id, &b.caption).await?})))
}

async fn call_members(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>) -> ApiResult<Json<Value>> {
    let room = s.core.room_by_token(&token).await?;
    s.core.actor_in_room(&a.actor, &room).await?;
    Ok(Json(
        json!({"members": s.core.call_members(&room.id), "hands": s.core.raised_hands(&room.id), "ice_servers": s.core.config.ice_servers}),
    ))
}

#[derive(Deserialize)]
struct EditBody {
    body: String,
}

async fn edit(State(s): State<AppState>, a: TalkActor, Path(id): Path<i64>, Json(b): Json<EditBody>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"message": s.core.edit_message(&a.actor, id, &b.body).await?})))
}

async fn delete_message(State(s): State<AppState>, a: TalkActor, Path(id): Path<i64>) -> ApiResult<Json<Value>> {
    s.core.delete_message(&a.actor, id).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct ReactBody {
    emoji: String,
    #[serde(default = "yes")]
    add: bool,
}

fn yes() -> bool {
    true
}

async fn react(State(s): State<AppState>, a: TalkActor, Path(id): Path<i64>, Json(b): Json<ReactBody>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"message": s.core.react(&a.actor, id, &b.emoji, b.add).await?})))
}

#[derive(Deserialize)]
struct VoteBody {
    choices: Vec<usize>,
}

async fn vote(State(s): State<AppState>, a: TalkActor, Path(id): Path<i64>, Json(b): Json<VoteBody>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"message": s.core.vote(&a.actor, id, &b.choices).await?})))
}

async fn close_poll(State(s): State<AppState>, a: TalkActor, Path(id): Path<i64>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"message": s.core.close_poll(&a.actor, id).await?})))
}

#[derive(Deserialize)]
struct GuestBody {
    name: String,
    password: Option<String>,
}

/// Joins a public room as a guest; the returned secret goes in `X-Guest-Token`.
async fn guest_join(State(s): State<AppState>, Path(token): Path<String>, Json(b): Json<GuestBody>) -> ApiResult<Json<Value>> {
    let (actor, room, secret) = moderation::with_password_throttle(&s, &token, s.core.create_guest(&token, &b.name, b.password.as_deref())).await?;
    let info = guest_room_info(&s, &room).await?;
    Ok(Json(json!({"guest": actor, "secret": secret, "room": info})))
}

/// Public information about a public room (for the guest join page).
/// With `X-Guest-Token`, also says whether that guest identity still belongs to the room.
async fn guest_room(State(s): State<AppState>, headers: axum::http::HeaderMap, Path(token): Path<String>) -> ApiResult<Json<Value>> {
    let room = s.core.room_by_token(&token).await?;
    if room.kind != "public" {
        return Err(ApiError(Error::NotFound));
    }
    let mut out = json!({"room": guest_room_info(&s, &room).await?});
    if let Some(secret) = headers.get(GUEST_HEADER).and_then(|v| v.to_str().ok()) {
        let valid = matches!(s.core.resolve_guest(secret).await, Ok((_, r)) if r.id == room.id);
        out["guest_valid"] = json!(valid);
    }
    Ok(Json(out))
}

/// What a guest may know about a public room: enough to filter live events and show its state.
async fn guest_room_info(s: &AppState, room: &ferrite_core::talk::Room) -> ApiResult<Value> {
    let participants: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM participants WHERE room_id = ?")
        .bind(&room.id)
        .fetch_one(&s.core.db)
        .await
        .map_err(Error::from)?;
    Ok(json!({
        "id": room.id,
        "token": room.token,
        "name": room.name,
        "description": room.description,
        "read_only": room.read_only,
        "ai_enabled": room.ai_enabled,
        "participant_count": participants,
        "lobby": room.lobby_active(),
        "lobby_timer": room.lobby_timer,
        "has_password": room.has_password,
        "call_permission": room.call_permission,
        "breakout_mode": room.breakout_mode,
        "breakout_status": room.breakout_status,
    }))
}
