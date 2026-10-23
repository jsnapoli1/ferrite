//! Settings → Administration → Mail: the Mail admin settings, provisioning templates, an
//! overview of delegations and the anti-abuse alerts. Administrators only (`admin` scope).

use crate::AppState;
use crate::auth::Auth;
use crate::error::ApiResult;
use axum::extract::{Path, State};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use ferrite_core::mail::provisioning::ProvisioningInput;
use ferrite_core::mail::settings::MailSettingsPatch;
use serde_json::{Value, json};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/mail/settings", get(settings).patch(update_settings))
        .route("/api/admin/mail/provisionings", get(provisionings).post(create_provisioning))
        .route("/api/admin/mail/provisionings/{id}", patch(update_provisioning).delete(delete_provisioning))
        .route("/api/admin/mail/provisionings/{id}/deprovision", post(deprovision))
        .route("/api/admin/mail/provision", post(provision_all))
        .route("/api/admin/mail/delegations", get(delegations))
        .route("/api/admin/mail/delegations/{id}", delete(revoke_delegation))
        .route("/api/admin/mail/abuse", get(abuse))
        .route("/api/admin/mail/sync", post(sync_now))
}

async fn settings_view(s: &AppState) -> Value {
    json!({
        "settings": s.core.mail_settings().await.view(),
        "redirect_uri": s.core.mail_oauth_redirect_uri(),
    })
}

async fn settings(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    Ok(Json(settings_view(&s).await))
}

async fn update_settings(State(s): State<AppState>, auth: Auth, Json(p): Json<MailSettingsPatch>) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    s.core.mail_update_settings(p).await?;
    tracing::info!(admin = %auth.user.username, "mail settings changed");
    Ok(Json(settings_view(&s).await))
}

async fn provisionings(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    Ok(Json(json!({"provisionings": s.core.mail_provisionings().await?})))
}

async fn create_provisioning(State(s): State<AppState>, auth: Auth, Json(input): Json<ProvisioningInput>) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    let (p, report) = s.core.mail_create_provisioning(&input).await?;
    Ok(Json(json!({"provisioning": p, "report": report})))
}

async fn update_provisioning(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(input): Json<ProvisioningInput>) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    let (p, report) = s.core.mail_update_provisioning(&id, &input).await?;
    Ok(Json(json!({"provisioning": p, "report": report})))
}

async fn delete_provisioning(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    Ok(Json(json!({"removed": s.core.mail_delete_provisioning(&id).await?})))
}

async fn deprovision(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    Ok(Json(json!({"removed": s.core.mail_deprovision(&id).await?})))
}

async fn provision_all(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    Ok(Json(json!({"report": s.core.mail_provision_all().await?})))
}

async fn delegations(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    Ok(Json(json!({"delegations": s.core.mail_all_delegations().await?})))
}

async fn revoke_delegation(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    s.core.mail_undelegate(&auth.user.id, &id, true).await?;
    Ok(Json(json!({"ok": true})))
}

async fn abuse(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    Ok(Json(json!({"alerts": s.core.mail_abuse_alerts(100).await?})))
}

/// Runs a background sync round now (it is skipped when one is already running).
async fn sync_now(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    Ok(Json(json!({"report": s.core.mail_background_round().await})))
}
