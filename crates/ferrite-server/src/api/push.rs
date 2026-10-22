//! Web Push API: the server's VAPID key, this user's devices, preferences and a test push.
//!
//! Registering a device hands the server a channel to the user's notifications, so it needs a
//! browser session or a full-access token.

use crate::AppState;
use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::push::{Outcome, check_endpoint, device_label};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, header};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use ferrite_core::Error;
use ferrite_core::push::{PushPrefs, PushSubscription};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/push", get(overview))
        .route("/api/push/subscriptions", post(subscribe))
        .route("/api/push/subscriptions/{id}", delete(unsubscribe))
        .route("/api/push/settings", put(update_settings))
        .route("/api/push/test", post(test))
}

fn require_full(auth: &Auth) -> ApiResult<()> {
    auth.require_scope("*")
}

fn device(sub: &PushSubscription) -> Value {
    let service = reqwest::Url::parse(&sub.endpoint)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    json!({
        "id": sub.id,
        "label": sub.label,
        "endpoint": sub.endpoint,
        "service": service,
        "created_at": sub.created_at,
        "last_used_at": sub.last_used_at,
    })
}

async fn overview(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    require_full(&auth)?;
    let vapid = s.push.vapid().await?;
    let subs = s.core.push_subscriptions(&auth.user.id).await?;
    Ok(Json(json!({
        "public_key": vapid.public_key_b64(),
        "settings": s.core.push_prefs(&auth.user.id).await?,
        "subscriptions": subs.iter().map(device).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
struct Keys {
    p256dh: String,
    auth: String,
}

/// The browser's `PushSubscription.toJSON()`, plus optional extras.
#[derive(Deserialize)]
struct NewSubscription {
    endpoint: String,
    keys: Keys,
    label: Option<String>,
    /// The endpoint this one replaces (after the browser renewed the subscription).
    replaces: Option<String>,
}

async fn subscribe(State(s): State<AppState>, auth: Auth, headers: HeaderMap, Json(n): Json<NewSubscription>) -> ApiResult<Json<Value>> {
    require_full(&auth)?;
    let url = check_endpoint(&n.endpoint, s.core.config.push.allow_insecure_endpoints).await?;
    let label = match n.label.as_deref().map(str::trim).filter(|l| !l.is_empty()) {
        Some(l) => l.to_string(),
        None => device_label(headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("")),
    };
    let sub = s
        .core
        .add_push_subscription(&auth.user.id, url.as_str(), &n.keys.p256dh, &n.keys.auth, &label)
        .await?;
    if let Some(old) = n.replaces.as_deref().filter(|o| *o != sub.endpoint) {
        if let Some(prev) = s.core.push_subscriptions(&auth.user.id).await?.into_iter().find(|p| p.endpoint == old) {
            s.core.remove_push_subscription(&auth.user.id, &prev.id).await?;
        }
    }
    Ok(Json(json!({"subscription": device(&sub)})))
}

async fn unsubscribe(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    require_full(&auth)?;
    s.core.remove_push_subscription(&auth.user.id, &id).await?;
    Ok(Json(json!({"ok": true})))
}

async fn update_settings(State(s): State<AppState>, auth: Auth, Json(p): Json<PushPrefs>) -> ApiResult<Json<Value>> {
    require_full(&auth)?;
    Ok(Json(json!({"settings": s.core.set_push_prefs(&auth.user.id, p).await?})))
}

async fn test(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    require_full(&auth)?;
    let results = s.push.send_test(&auth.user.id).await?;
    if results.is_empty() {
        return Err(ApiError(Error::bad("no device receives push notifications yet")));
    }
    let report: Vec<Value> = results
        .iter()
        .map(|(sub, outcome)| {
            let (status, error) = match outcome {
                Outcome::Delivered => ("delivered", None),
                Outcome::Gone => ("removed", Some("the push service no longer knows this device")),
                Outcome::Failed(e) => ("failed", Some(e.as_str())),
            };
            json!({"id": sub.id, "label": sub.label, "status": status, "error": error})
        })
        .collect();
    let delivered = results.iter().filter(|(_, o)| *o == Outcome::Delivered).count();
    Ok(Json(json!({"delivered": delivered, "results": report})))
}
