//! Calendar and contacts API for the web UI (the same data is served over CalDAV/CardDAV).

use crate::AppState;
use crate::auth::Auth;
use crate::error::ApiResult;
use axum::extract::{Path, Query, State};
use axum::routing::{delete, get, patch};
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use ferrite_core::groupware::{ContactInput, EventInput, KIND_ADDRESSBOOK, KIND_CALENDAR};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/calendar/calendars", get(calendars).post(create_calendar))
        .route("/api/calendar/calendars/{uri}", patch(update_calendar).delete(delete_calendar))
        .route("/api/calendar/events", get(events).post(create_event))
        .route("/api/calendar/events/{id}", patch(update_event).delete(delete_event))
        .route("/api/contacts", get(contacts).post(create_contact))
        .route("/api/contacts/{id}", delete(delete_contact))
}

fn ok() -> Json<Value> {
    Json(json!({"ok": true}))
}

async fn calendars(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    auth.require_scope("groupware")?;
    let base = format!("{}/remote.php/dav/", s.core.config.public_url);
    Ok(Json(json!({
        "calendars": s.core.dav_collections(&auth.user.id, KIND_CALENDAR).await?,
        "addressbooks": s.core.dav_collections(&auth.user.id, KIND_ADDRESSBOOK).await?,
        "dav_url": base,
    })))
}

#[derive(Deserialize)]
struct NewCalendar {
    name: String,
    color: Option<String>,
    /// e.g. "VEVENT,VTODO"
    components: Option<String>,
}

async fn create_calendar(State(s): State<AppState>, auth: Auth, Json(b): Json<NewCalendar>) -> ApiResult<Json<Value>> {
    auth.require_scope("groupware")?;
    let slug: String = b
        .name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    let uri = if slug.is_empty() {
        ferrite_core::util::short_token(8)
    } else {
        format!("{slug}-{}", ferrite_core::util::short_token(4))
    };
    let c = s
        .core
        .create_dav_collection(&auth.user.id, KIND_CALENDAR, &uri, &b.name, b.color.as_deref(), b.components.as_deref())
        .await?;
    Ok(Json(json!({"calendar": c})))
}

#[derive(Deserialize)]
struct CalendarUpdate {
    name: Option<String>,
    color: Option<String>,
}

async fn update_calendar(State(s): State<AppState>, auth: Auth, Path(uri): Path<String>, Json(b): Json<CalendarUpdate>) -> ApiResult<Json<Value>> {
    let c = s.core.dav_collection(&auth.user.id, KIND_CALENDAR, &uri).await?;
    s.core.update_dav_collection(&c, b.name.as_deref(), b.color.as_deref()).await?;
    Ok(ok())
}

async fn delete_calendar(State(s): State<AppState>, auth: Auth, Path(uri): Path<String>) -> ApiResult<Json<Value>> {
    let c = s.core.dav_collection(&auth.user.id, KIND_CALENDAR, &uri).await?;
    s.core.delete_dav_collection(&c).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct Range {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
}

async fn events(State(s): State<AppState>, auth: Auth, Query(r): Query<Range>) -> ApiResult<Json<Value>> {
    auth.require_scope("groupware")?;
    let from = r.from.unwrap_or_else(|| Utc::now() - Duration::days(31));
    let to = r.to.unwrap_or_else(|| Utc::now() + Duration::days(62));
    Ok(Json(json!({"events": s.core.events_between(&auth.user.id, from, to).await?})))
}

async fn create_event(State(s): State<AppState>, auth: Auth, Json(e): Json<EventInput>) -> ApiResult<Json<Value>> {
    auth.require_scope("groupware")?;
    Ok(Json(json!({"event": s.core.create_event(&auth.user.id, &e).await?})))
}

async fn update_event(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(e): Json<EventInput>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"event": s.core.update_event(&auth.user.id, &id, &e).await?})))
}

async fn delete_event(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    s.core.delete_event(&auth.user.id, &id).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct ContactQ {
    q: Option<String>,
}

async fn contacts(State(s): State<AppState>, auth: Auth, Query(q): Query<ContactQ>) -> ApiResult<Json<Value>> {
    auth.require_scope("groupware")?;
    Ok(Json(json!({"contacts": s.core.contacts(&auth.user.id, q.q.as_deref()).await?})))
}

async fn create_contact(State(s): State<AppState>, auth: Auth, Json(c): Json<ContactInput>) -> ApiResult<Json<Value>> {
    auth.require_scope("groupware")?;
    Ok(Json(json!({"contact": s.core.create_contact(&auth.user.id, &c).await?})))
}

async fn delete_contact(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    s.core.delete_contact(&auth.user.id, &id).await?;
    Ok(ok())
}
