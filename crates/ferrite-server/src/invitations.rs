//! Public calendar endpoints that work without an account: answering an emailed invitation
//! (`/invitation/<token>` in the web app uses `/api/invitation/<token>`) and the read-only
//! ICS subscription feed of a published calendar.

use crate::AppState;
use crate::error::ApiResult;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/invitation/{token}", get(invitation).post(respond))
        .route("/remote.php/dav/public-calendars/{token}", get(feed))
}

async fn invitation(State(s): State<AppState>, Path(token): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "invitation": s.core.invitation(&token).await?,
        "public_url": s.core.config.public_url,
    })))
}

#[derive(Deserialize)]
struct Respond {
    response: String,
}

async fn respond(State(s): State<AppState>, Path(token): Path<String>, Json(b): Json<Respond>) -> ApiResult<Json<Value>> {
    let partstat = ferrite_core::scheduling::parse_response(&b.response)?;
    Ok(Json(json!({"invitation": s.core.respond_to_invitation(&token, partstat).await?})))
}

/// `GET /remote.php/dav/public-calendars/<token>[.ics]`: the calendar as one VCALENDAR.
async fn feed(State(s): State<AppState>, Path(token): Path<String>) -> Response {
    let token = token.strip_suffix(".ics").unwrap_or(&token);
    match s.core.public_calendar_feed(token).await {
        Ok((name, ics)) => {
            let mut r = (StatusCode::OK, ics).into_response();
            let h = r.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/calendar; charset=utf-8"));
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            let file: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || " -_".contains(*c)).collect();
            if let Ok(v) = HeaderValue::from_str(&format!(
                "inline; filename=\"{}.ics\"",
                if file.trim().is_empty() { "calendar" } else { file.trim() }
            )) {
                h.insert(header::CONTENT_DISPOSITION, v);
            }
            r
        }
        Err(_) => (StatusCode::NOT_FOUND, "this calendar link does not exist or was revoked").into_response(),
    }
}
