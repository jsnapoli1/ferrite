//! Talk moderation and breakout rooms over REST. Every rule is enforced in ferrite-core; these
//! handlers only authenticate (session cookie with the CSRF header, an app token with the
//! `talk` scope, or a guest secret) and translate.

use super::{TalkActor, ok};
use crate::AppState;
use crate::error::{ApiError, ApiResult};
use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use ferrite_core::Error;
use ferrite_core::talk::breakout::Assignment;
use ferrite_core::talk::moderation::{CallAction, ModerationUpdate, WRONG_ROOM_PASSWORD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/talk/rooms/{token}/moderation", axum::routing::patch(update))
        .route("/api/talk/rooms/{token}/lobby", get(lobby))
        .route("/api/talk/rooms/{token}/call/moderate", post(moderate_call))
        .route("/api/talk/rooms/{token}/breakouts", get(breakouts).post(configure).delete(remove))
        .route("/api/talk/rooms/{token}/breakouts/assign", post(assign))
        .route("/api/talk/rooms/{token}/breakouts/start", post(start))
        .route("/api/talk/rooms/{token}/breakouts/stop", post(stop))
        .route("/api/talk/rooms/{token}/breakouts/broadcast", post(broadcast))
        .route("/api/talk/rooms/{token}/breakouts/choose", post(choose))
        .route("/api/talk/rooms/{token}/help", post(help))
}

/// Background work: drop SFU connections of call sessions the server ended, and run the Talk
/// timers (scheduled lobby openings, breakout room ends) also after a restart.
pub fn spawn_background(state: &AppState) {
    if let Some(sfu) = state.sfu.clone() {
        let mut ended = state.core.ended_call_sessions();
        tokio::spawn(async move {
            loop {
                match ended.recv().await {
                    Ok(session) => sfu.disconnect(&session),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }
    let core = state.core.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        loop {
            tick.tick().await;
            core.run_talk_timers().await;
        }
    });
}

async fn update(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>, Json(b): Json<ModerationUpdate>) -> ApiResult<Json<Value>> {
    let room = s.core.update_moderation(&a.user()?.user.id, &token, b).await?;
    Ok(Json(json!({"room": room})))
}

async fn lobby(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"waiting": s.core.lobby_waiting(&a.user()?.user.id, &token).await?})))
}

#[derive(Deserialize)]
struct ModerateBody {
    action: String,
    session: Option<String>,
}

async fn moderate_call(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>, Json(b): Json<ModerateBody>) -> ApiResult<Json<Value>> {
    let action = CallAction::parse(&b.action)?;
    let n = s.core.moderate_call(&a.user()?.user.id, &token, action, b.session.as_deref()).await?;
    Ok(Json(json!({"ok": true, "affected": n})))
}

async fn breakouts(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"breakouts": s.core.breakouts(&a.actor, &token).await?})))
}

#[derive(Deserialize)]
struct ConfigureBody {
    mode: String,
    count: usize,
}

async fn configure(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>, Json(b): Json<ConfigureBody>) -> ApiResult<Json<Value>> {
    let v = s.core.configure_breakouts(&a.user()?.user.id, &token, &b.mode, b.count).await?;
    Ok(Json(json!({"breakouts": v})))
}

async fn remove(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"breakouts": s.core.remove_breakouts(&a.user()?.user.id, &token).await?})))
}

#[derive(Deserialize)]
struct AssignBody {
    assignments: Vec<Assignment>,
}

async fn assign(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>, Json(b): Json<AssignBody>) -> ApiResult<Json<Value>> {
    let v = s.core.assign_breakouts(&a.user()?.user.id, &token, &b.assignments).await?;
    Ok(Json(json!({"breakouts": v})))
}

#[derive(Deserialize)]
struct StartBody {
    minutes: Option<i64>,
}

async fn start(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>, Json(b): Json<StartBody>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"breakouts": s.core.start_breakouts(&a.user()?.user.id, &token, b.minutes).await?})))
}

async fn stop(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"breakouts": s.core.stop_breakouts(Some(&a.user()?.user.id), &token).await?})))
}

#[derive(Deserialize)]
struct BroadcastBody {
    body: String,
}

async fn broadcast(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>, Json(b): Json<BroadcastBody>) -> ApiResult<Json<Value>> {
    let n = s.core.broadcast_to_breakouts(&a.user()?.user.id, &token, &b.body).await?;
    Ok(Json(json!({"ok": true, "rooms": n})))
}

#[derive(Deserialize)]
struct ChooseBody {
    room: String,
}

async fn choose(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>, Json(b): Json<ChooseBody>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"breakouts": s.core.choose_breakout(&a.actor, &token, &b.room).await?})))
}

#[derive(Deserialize)]
struct HelpBody {
    #[serde(default = "yes")]
    on: bool,
}

fn yes() -> bool {
    true
}

async fn help(State(s): State<AppState>, a: TalkActor, Path(token): Path<String>, Json(b): Json<HelpBody>) -> ApiResult<Json<Value>> {
    s.core.request_breakout_help(&a.actor, &token, b.on).await?;
    Ok(ok())
}

/// Throttles password guesses for one public room (8 failures per 10 minutes), the same way
/// public share links are protected.
pub async fn with_password_throttle<T>(s: &AppState, token: &str, attempt: impl Future<Output = Result<T, Error>>) -> ApiResult<T> {
    let key = format!("talk-room:{token}");
    if s.throttle.blocked(&key) {
        return Err(ApiError(Error::Unavailable("too many wrong passwords; try again later".into())));
    }
    match attempt.await {
        Ok(v) => {
            s.throttle.succeed(&key);
            Ok(v)
        }
        Err(Error::BadRequest(m)) if m == WRONG_ROOM_PASSWORD => {
            s.throttle.fail(&key);
            Err(ApiError(Error::BadRequest(m)))
        }
        Err(e) => Err(ApiError(e)),
    }
}
