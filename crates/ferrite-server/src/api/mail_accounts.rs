//! Webmail account features beyond the basics: the user-visible Mail configuration, account
//! order, delegation, the connection test, and connecting Google/Microsoft accounts with
//! OAuth 2.0. Everything requires the `mail` scope; account rights are checked in core
//! (`mail_access`).

use crate::AppState;
use crate::auth::{Auth, MaybeAuth};
use crate::error::{ApiError, ApiResult, public_message};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use ferrite_core::Error;
use ferrite_core::mail::autoconfig::ConnectivityTest;
use ferrite_core::mail::oauth::{OAuthStart, form};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/mail/config", get(config))
        .route("/api/mail/accounts/order", put(set_order))
        .route("/api/mail/accounts/{id}/delegations", get(delegations).post(delegate))
        .route("/api/mail/delegations/{id}", delete(undelegate))
        .route("/api/mail/autoconfig/test", post(test_connectivity))
        .route("/api/mail/oauth/start", post(oauth_start))
        .route("/api/mail/oauth/callback", get(oauth_callback))
}

fn scope(auth: &Auth) -> Result<(), ApiError> {
    auth.require_scope("mail")
}

/// What the Mail app needs to know about the server's Mail settings (no secrets).
async fn config(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    let m = s.core.mail_settings().await;
    Ok(Json(json!({
        "allow_user_accounts": m.allow_user_accounts,
        "attachment_size_limit": m.attachment_size_limit,
        "llm_processing": m.llm_processing,
        "importance_default": m.importance_default,
        "gravatar": m.gravatar,
        "ui_defaults": m.ui_defaults,
        "spam_reporting": {"spam": !m.spam_report_address.is_empty(), "ham": !m.ham_report_address.is_empty()},
        "oauth": s.core.mail_oauth_providers().await,
    })))
}

#[derive(Deserialize)]
struct OrderBody {
    ids: Vec<String>,
}

async fn set_order(State(s): State<AppState>, auth: Auth, Json(b): Json<OrderBody>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    if b.ids.len() > 500 {
        return Err(ApiError(Error::bad("too many accounts")));
    }
    s.core.mail_set_account_order(&auth.user.id, &b.ids).await?;
    Ok(Json(json!({"ok": true})))
}

async fn delegations(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"delegations": s.core.mail_delegations(&auth.user.id, &id).await?})))
}

#[derive(Deserialize)]
struct DelegateBody {
    /// Username (or user id) to share the account with.
    user: String,
}

async fn delegate(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<DelegateBody>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"delegation": s.core.mail_delegate(&auth.user.id, &id, &b.user).await?})))
}

async fn undelegate(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    s.core.mail_undelegate(&auth.user.id, &id, false).await?;
    Ok(Json(json!({"ok": true})))
}

async fn test_connectivity(State(s): State<AppState>, auth: Auth, Json(t): Json<ConnectivityTest>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"result": s.core.mail_test_connectivity(&t).await})))
}

/// Starts connecting a Google or Microsoft account; returns the consent URL to open. Only
/// from a browser session: the callback has to come back to the same session.
async fn oauth_start(State(s): State<AppState>, auth: Auth, Json(b): Json<OAuthStart>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    if !auth.via_cookie {
        return Err(ApiError(Error::bad("connecting an account needs a browser session")));
    }
    let url = s.core.mail_oauth_start(&auth.user.id, &auth.token.id, &b).await?;
    Ok(Json(json!({"url": url, "redirect_uri": s.core.mail_oauth_redirect_uri()})))
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

fn see_other(location: &str) -> Response {
    let mut r = StatusCode::SEE_OTHER.into_response();
    if let Ok(v) = HeaderValue::from_str(location) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// The provider sends the browser back here; the account is created (or reconnected) and the
/// browser lands in Mail, with `oauth_error` when something went wrong.
async fn oauth_callback(State(s): State<AppState>, MaybeAuth(auth): MaybeAuth, Query(q): Query<CallbackQuery>) -> Response {
    let r = async {
        let auth = auth
            .filter(|a| a.via_cookie)
            .ok_or_else(|| Error::bad("sign in to Ferrite first, then connect the account again"))?;
        if let Some(err) = q.error {
            return Err(Error::bad(format!(
                "the provider did not connect the account: {}",
                q.error_description.unwrap_or(err)
            )));
        }
        let state = q.state.ok_or_else(|| Error::bad("the provider's answer has no state"))?;
        let code = q.code.ok_or_else(|| Error::bad("the provider's answer has no code"))?;
        s.core.mail_oauth_finish(&auth.user.id, &auth.token.id, &state, &code).await
    }
    .await;
    match r {
        Ok(a) => see_other(&format!("/mail?account={}&oauth=connected", form(&a.id))),
        Err(e) => {
            tracing::info!(error = %e, "connecting a mail account with OAuth failed");
            see_other(&format!("/mail?oauth_error={}", form(&public_message(&e))))
        }
    }
}
