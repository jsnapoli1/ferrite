//! WebSockets: `/api/ws` carries every live event for a client (one socket per tab), and
//! `/api/collab/{file_id}` speaks the binary y-websocket protocol for live documents.

use crate::AppState;
use crate::auth::{SESSION_COOKIE, cookie};
use crate::error::{ApiError, ApiResult};
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use ferrite_core::Error;
use ferrite_core::talk::Actor;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

#[derive(Deserialize)]
pub struct WsQuery {
    /// App token (for non-browser clients that can't set headers on the upgrade request).
    token: Option<String>,
    /// Guest secret for public rooms.
    guest: Option<String>,
}

/// Browsers send cookies on cross-site WebSocket upgrades, so cookie-authenticated sockets
/// must come from our own origin.
fn same_origin(headers: &HeaderMap, public_url: &str) -> bool {
    let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let host = headers.get("host").and_then(|v| v.to_str().ok()).unwrap_or("");
    let origin_host = origin.split("://").nth(1).unwrap_or("");
    origin_host == host || origin.trim_end_matches('/') == public_url
}

async fn ws_actor(s: &AppState, headers: &HeaderMap, q: &WsQuery) -> ApiResult<(Actor, Option<String>)> {
    if let Some(g) = &q.guest {
        let (actor, _) = s.core.resolve_guest(g).await?;
        return Ok((actor, None));
    }
    let secret = match (&q.token, cookie(headers, SESSION_COOKIE)) {
        (Some(t), _) => t.clone(),
        (None, Some(c)) => {
            if !same_origin(headers, &s.core.config.public_url) {
                return Err(ApiError(Error::Forbidden));
            }
            c
        }
        _ => return Err(ApiError(Error::Unauthorized)),
    };
    let (user, _) = s.core.resolve_token(&secret).await?;
    Ok((Actor::user(&user.id, &user.display_name), Some(user.id)))
}

pub async fn events(State(s): State<AppState>, headers: HeaderMap, Query(q): Query<WsQuery>, ws: WebSocketUpgrade) -> ApiResult<Response> {
    let (actor, _) = ws_actor(&s, &headers, &q).await?;
    // Event frames are small; a 4 KiB read buffer (tungstenite defaults to 128 KiB per socket)
    // is what keeps thousands of idle connections cheap.
    Ok(ws
        .max_message_size(256 * 1024)
        .read_buffer_size(4 * 1024)
        .on_upgrade(move |socket| event_loop(s, actor, socket)))
}

async fn event_loop(s: AppState, actor: Actor, mut socket: WebSocket) {
    let key = actor.hub_key();
    let (conn, mut rx) = s.core.hub.connect(&key);
    // Call sessions opened on this socket, so their SFU connections end with it.
    let mut sessions: Vec<String> = Vec::new();
    let hello = json!({"type": "hello", "conn": conn, "actor": actor});
    if socket.send(Message::Text(hello.to_string().into())).await.is_err() {
        s.core.hub.disconnect(&key, conn);
        return;
    }
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Some(payload) => {
                    if socket.send(Message::Text(payload.as_ref().into())).await.is_err() {
                        break;
                    }
                }
                // Dropped by the hub (fell too far behind): close so the client resyncs.
                None => break,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(text))) => {
                    let reply = match serde_json::from_str::<Value>(&text) {
                        Ok(v) => {
                            let r = handle_client_event(&s, &actor, conn, v).await;
                            if let Some(sess) = r.as_ref().filter(|r| r["type"] == "call.self").and_then(|r| r["session"].as_str()) {
                                sessions.push(sess.to_string());
                            }
                            r
                        }
                        Err(_) => Some(json!({"type": "error", "error": "invalid JSON"})),
                    };
                    if let Some(r) = reply {
                        if socket.send(Message::Text(r.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            }
        }
    }
    s.core.hub.disconnect(&key, conn);
    s.core.leave_calls_of_connection(&key, conn).await;
    s.core.lobby_leave_connection(&key, conn).await;
    if let Some(sfu) = &s.sfu {
        for sess in sessions {
            sfu.disconnect(&sess);
        }
    }
    crate::recording::after_leave(&s).await;
}

async fn handle_client_event(s: &AppState, actor: &Actor, conn: u64, v: Value) -> Option<Value> {
    let ty = v["type"].as_str().unwrap_or("");
    let str_of = |k: &str| v[k].as_str().unwrap_or("").to_string();
    let result: Result<Option<Value>, Error> = async {
        match ty {
            "ping" => Ok(Some(json!({"type": "pong"}))),
            "typing" => {
                s.core.typing(actor, &str_of("room")).await?;
                Ok(None)
            }
            "call.join" => {
                let (session, members, room) = s.core.join_call(actor, &str_of("room"), conn, v["audio"].as_bool().unwrap_or(true), v["video"].as_bool().unwrap_or(false)).await?;
                let recording = s.recordings.is_recording(&room.id);
                if recording {
                    s.recordings.note_member(&room.id, &session, &actor.name);
                }
                let can_record = s.sfu.is_some() && crate::recording::can_record(s, actor, &room).await?;
                // Validated when the configuration was loaded.
                let (height, fps) = s.core.config.calls.quality().unwrap_or((720, 30));
                let quality = json!({"height": height, "fps": fps});
                Ok(Some(json!({"type": "call.self", "room_id": room.id, "room": room.token, "session": session, "members": members, "ice_servers": s.core.config.ice_servers, "sfu": s.sfu.is_some(), "recording": recording, "can_record": can_record, "quality": quality})))
            }
            "call.leave" => {
                let (room_id, session) = (str_of("room_id"), str_of("session"));
                if s.core.owns_session(&room_id, &session, &actor.hub_key(), conn) {
                    s.core.leave_call(&room_id, &session).await?;
                    if let Some(sfu) = &s.sfu {
                        sfu.disconnect(&session);
                    }
                    crate::recording::after_leave(s).await;
                }
                Ok(None)
            }
            "call.record" => {
                let room_id = str_of("room_id");
                match str_of("action").as_str() {
                    "start" => crate::recording::start(s, actor, &room_id).await?,
                    "stop" => crate::recording::stop(s, Some(actor), &room_id).await?,
                    _ => return Err(Error::bad("action must be start or stop")),
                }
                Ok(None)
            }
            "sfu.offer" => {
                let (room_id, session) = (str_of("room_id"), str_of("session"));
                if !s.core.owns_session(&room_id, &session, &actor.hub_key(), conn) {
                    return Err(Error::Forbidden);
                }
                // The lobby may have closed since the session joined.
                s.core.check_call_session(actor, &room_id).await?;
                let sfu = s.sfu.as_ref().ok_or_else(|| Error::Unavailable("the SFU is not running on this server".into()))?;
                let answer = sfu.connect(&room_id, &session, v["sdp"].clone()).map_err(Error::bad)?;
                Ok(Some(json!({"type": "sfu.answer", "room_id": room_id, "session": session, "sdp": answer})))
            }
            "call.signal" => {
                let (room_id, session) = (str_of("room_id"), str_of("session"));
                if !s.core.owns_session(&room_id, &session, &actor.hub_key(), conn) {
                    return Err(Error::Forbidden);
                }
                s.core.relay_signal(&room_id, &session, &str_of("to"), v["payload"].clone())?;
                Ok(None)
            }
            "call.state" => {
                let (room_id, session) = (str_of("room_id"), str_of("session"));
                if !s.core.owns_session(&room_id, &session, &actor.hub_key(), conn) {
                    return Err(Error::Forbidden);
                }
                s.core.update_call_state(&room_id, &session, v["audio"].as_bool(), v["video"].as_bool(), v["screen"].as_bool(), v["hand"].as_bool()).await?;
                Ok(None)
            }
            // Someone waiting in a lobby, so moderators see who is there.
            "lobby.wait" => {
                s.core.lobby_wait(actor, &str_of("room"), conn).await?;
                Ok(None)
            }
            "lobby.leave" => {
                s.core.lobby_unwait(actor, &str_of("room"), conn).await?;
                Ok(None)
            }
            other => Err(Error::bad(format!("unknown event {other}"))),
        }
    }
    .await;
    match result {
        Ok(r) => r,
        Err(e) => Some(json!({"type": "error", "for": ty, "error": crate::error::public_message(&e)})),
    }
}

pub async fn collab(
    State(s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    let (_, user_id) = ws_actor(&s, &headers, &q).await?;
    let user_id = user_id.ok_or(ApiError(Error::Forbidden))?;
    // Open before upgrading so permission errors become proper HTTP errors.
    let (conn, initial) = s.core.collab_open(&user_id, &id).await?;
    Ok(ws
        .max_message_size(16 * 1024 * 1024)
        .read_buffer_size(16 * 1024)
        .on_upgrade(move |socket| collab_loop(s, conn, initial, socket)))
}

async fn collab_loop(s: AppState, mut conn: ferrite_core::collab::CollabConn, initial: Vec<u8>, mut socket: WebSocket) {
    let ok = socket.send(Message::Binary(initial.into())).await.is_ok();
    if ok {
        loop {
            tokio::select! {
                update = conn.rx.recv() => match update {
                    Ok(frame) => {
                        if ferrite_core::collab::is_reset(&frame) {
                            let _ = socket.send(Message::Close(Some(CloseFrame { code: 4000, reason: "document replaced; reload".into() }))).await;
                            break;
                        }
                        if socket.send(Message::Binary(Arc::<[u8]>::clone(&frame).to_vec().into())).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(_)) => {
                        let _ = socket.send(Message::Close(Some(CloseFrame { code: 4001, reason: "too slow; reconnect".into() }))).await;
                        break;
                    }
                    Err(RecvError::Closed) => break,
                },
                msg = socket.recv() => match msg {
                    Some(Ok(Message::Binary(data))) => match s.core.collab_receive(&mut conn, &data) {
                        Ok(replies) => {
                            for r in replies {
                                if socket.send(Message::Binary(r.into())).await.is_err() {
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, "bad collab frame");
                            break;
                        }
                    },
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
    s.core.collab_close(conn).await;
}
