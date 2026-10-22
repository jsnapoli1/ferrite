//! Calendar sharing, public subscription links and invitation answers for the web UI.

use crate::AppState;
use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use axum::extract::{Path, Query, State};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use ferrite_core::Error;
use serde::Deserialize;
use serde_json::{Value, json};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/calendar/calendars/{uri}/shares", get(shares).post(add_share))
        .route("/api/calendar/calendars/{uri}/shares/{id}", patch(update_share).delete(remove_share))
        .route("/api/calendar/calendars/{uri}/publish", get(publication).post(publish).delete(unpublish))
        .route("/api/calendar/events/{id}/respond", post(respond))
        .route("/api/calendar/attendees", get(attendee_suggestions))
        .route("/api/calendar/share-targets", get(share_targets))
}

/// The caller's own calendar `uri` (sharing is managed by owners only).
async fn owned(s: &AppState, auth: &Auth, uri: &str) -> ApiResult<()> {
    auth.require_scope("groupware")?;
    let cal = s.core.calendar_ref(&auth.user.id, uri).await?;
    if !cal.is_owner() {
        return Err(ApiError(Error::Forbidden));
    }
    Ok(())
}

async fn shares(State(s): State<AppState>, auth: Auth, Path(uri): Path<String>) -> ApiResult<Json<Value>> {
    owned(&s, &auth, &uri).await?;
    Ok(Json(json!({
        "shares": s.core.calendar_shares(&auth.user.id, &uri).await?,
        "public_url": s.core.calendar_publication(&auth.user.id, &uri).await?,
    })))
}

#[derive(Deserialize)]
struct NewShare {
    /// "user" or "group"
    kind: String,
    /// Username or group name.
    target: String,
    #[serde(default)]
    writable: bool,
}

async fn add_share(State(s): State<AppState>, auth: Auth, Path(uri): Path<String>, Json(b): Json<NewShare>) -> ApiResult<Json<Value>> {
    owned(&s, &auth, &uri).await?;
    let share = s.core.share_calendar(&auth.user, &uri, &b.kind, &b.target, b.writable).await?;
    Ok(Json(json!({"share": share})))
}

#[derive(Deserialize)]
struct ShareUpdate {
    writable: bool,
}

async fn update_share(State(s): State<AppState>, auth: Auth, Path((uri, id)): Path<(String, String)>, Json(b): Json<ShareUpdate>) -> ApiResult<Json<Value>> {
    owned(&s, &auth, &uri).await?;
    Ok(Json(
        json!({"share": s.core.update_calendar_share(&auth.user.id, &uri, &id, b.writable).await?}),
    ))
}

async fn remove_share(State(s): State<AppState>, auth: Auth, Path((uri, id)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    owned(&s, &auth, &uri).await?;
    s.core.unshare_calendar(&auth.user.id, &uri, &id).await?;
    Ok(Json(json!({"ok": true})))
}

async fn publication(State(s): State<AppState>, auth: Auth, Path(uri): Path<String>) -> ApiResult<Json<Value>> {
    owned(&s, &auth, &uri).await?;
    Ok(Json(json!({"public_url": s.core.calendar_publication(&auth.user.id, &uri).await?})))
}

async fn publish(State(s): State<AppState>, auth: Auth, Path(uri): Path<String>) -> ApiResult<Json<Value>> {
    owned(&s, &auth, &uri).await?;
    Ok(Json(json!({"public_url": s.core.publish_calendar(&auth.user.id, &uri).await?})))
}

async fn unpublish(State(s): State<AppState>, auth: Auth, Path(uri): Path<String>) -> ApiResult<Json<Value>> {
    owned(&s, &auth, &uri).await?;
    s.core.unpublish_calendar(&auth.user.id, &uri).await?;
    Ok(Json(json!({"public_url": null})))
}

#[derive(Deserialize)]
struct Respond {
    /// accepted, tentative or declined
    response: String,
}

async fn respond(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<Respond>) -> ApiResult<Json<Value>> {
    auth.require_scope("groupware")?;
    let partstat = ferrite_core::scheduling::parse_response(&b.response)?;
    s.core.respond_to_event(&auth.user.id, &id, partstat).await?;
    Ok(Json(json!({"event": s.core.event_view(&auth.user.id, &id).await?})))
}

#[derive(Deserialize)]
struct Q {
    #[serde(default)]
    q: String,
}

/// People to invite: Ferrite users with an email address, then contacts' addresses.
async fn attendee_suggestions(State(s): State<AppState>, auth: Auth, Query(q): Query<Q>) -> ApiResult<Json<Value>> {
    auth.require_scope("groupware")?;
    let needle = q.q.trim().to_lowercase();
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for u in s.core.list_users(None, 1000).await? {
        let Some(email) = u.email.as_deref().map(ferrite_core::itip::normalize_address) else {
            continue;
        };
        let hay = format!("{} {} {email}", u.display_name, u.username).to_lowercase();
        if u.disabled || u.id == auth.user.id || !hay.contains(&needle) || !seen.insert(email.clone()) {
            continue;
        }
        out.push(json!({"name": u.display_name, "email": email, "username": u.username, "user": true}));
    }
    for c in s.core.contacts(&auth.user.id, None).await? {
        for e in &c.emails {
            let email = ferrite_core::itip::normalize_address(e);
            let hay = format!("{} {email}", c.name).to_lowercase();
            if !ferrite_core::itip::valid_email(&email) || !hay.contains(&needle) || !seen.insert(email.clone()) {
                continue;
            }
            out.push(json!({"name": c.name, "email": email, "user": false}));
        }
    }
    out.truncate(20);
    Ok(Json(json!({"people": out})))
}

/// Users and groups a calendar can be shared with.
async fn share_targets(State(s): State<AppState>, auth: Auth, Query(q): Query<Q>) -> ApiResult<Json<Value>> {
    auth.require_scope("groupware")?;
    let needle = q.q.trim().to_lowercase();
    let users: Vec<Value> = s
        .core
        .list_users(Some(q.q.trim()), 20)
        .await?
        .into_iter()
        .filter(|u| !u.disabled && u.id != auth.user.id)
        .map(|u| json!({"username": u.username, "display_name": u.display_name}))
        .collect();
    let groups: Vec<Value> = s
        .core
        .list_groups()
        .await?
        .into_iter()
        .filter(|g| g.name.to_lowercase().contains(&needle))
        .take(20)
        .map(|g| json!({"name": g.name}))
        .collect();
    Ok(Json(json!({"users": users, "groups": groups})))
}
