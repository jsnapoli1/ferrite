//! Account security: the second step of a password sign-in, two-factor settings, linked
//! single sign-on identities, and the admin's 2FA reset.
//!
//! A password sign-in for a user with 2FA (or on a server that requires it) answers with a
//! challenge instead of a session: `{"two_factor": "totp" | "enroll", "challenge": …}`.
//! `POST /api/auth/2fa` with the challenge and a code then starts the session.

use super::{ok, signed_in, start_session};
use crate::AppState;
use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::Engine;
use ferrite_core::Error;
use ferrite_core::auth::User;
use ferrite_core::twofactor::{Enrollment, LoginChallenge};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/auth/2fa", post(complete_challenge))
        .route("/api/me/security", get(overview))
        .route("/api/me/2fa/totp", post(begin_totp))
        .route("/api/me/2fa/totp/confirm", post(confirm_totp))
        .route("/api/me/2fa/recovery-codes", post(regenerate_codes))
        .route("/api/me/2fa/disable", post(disable))
        .route("/api/admin/users/{id}/2fa", delete(admin_reset))
        .merge(crate::oidc::router())
}

/// The QR code of an `otpauth://` URI as an SVG data URI (dark modules on white, with the
/// quiet zone, so it scans in either theme).
pub fn qr_data_uri(uri: &str) -> Result<String, Error> {
    let code = qrcode::QrCode::with_error_correction_level(uri.as_bytes(), qrcode::EcLevel::M).map_err(|e| Error::internal(format!("QR code: {e}")))?;
    let svg = code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(200, 200)
        .dark_color(qrcode::render::svg::Color("#000000"))
        .light_color(qrcode::render::svg::Color("#ffffff"))
        .quiet_zone(true)
        .build();
    Ok(format!("data:image/svg+xml;base64,{}", base64::engine::general_purpose::STANDARD.encode(svg)))
}

fn enrollment_json(e: &Enrollment) -> Result<Value, Error> {
    Ok(json!({"secret": e.secret, "otpauth_uri": e.otpauth_uri, "qr": qr_data_uri(&e.otpauth_uri)?}))
}

fn challenge_json(c: &LoginChallenge) -> Result<Value, Error> {
    let mut v = json!({"two_factor": c.kind, "challenge": c.challenge, "expires_at": c.expires_at});
    if let Some(e) = &c.enrollment {
        v["enrollment"] = enrollment_json(e)?;
    }
    Ok(v)
}

/// After a correct password (login, registration, setup): a session, or the 2FA challenge.
pub async fn password_signed_in(s: &AppState, user: &User, headers: &HeaderMap) -> ApiResult<Response> {
    match s.core.begin_login_challenge(user).await? {
        Some(c) => Ok(Json(challenge_json(&c)?).into_response()),
        None => signed_in(s, &user.id, headers).await,
    }
}

fn throttle_key(user_id: &str) -> String {
    format!("2fa:{user_id}")
}

fn check_throttle(s: &AppState, user_id: &str) -> ApiResult<()> {
    if s.throttle.blocked(&throttle_key(user_id)) {
        return Err(ApiError(Error::Unavailable("too many wrong codes; try again in a few minutes".into())));
    }
    Ok(())
}

#[derive(Deserialize)]
struct ChallengeBody {
    challenge: String,
    code: String,
}

async fn complete_challenge(State(s): State<AppState>, headers: HeaderMap, Json(b): Json<ChallengeBody>) -> ApiResult<Response> {
    let user_id = s.core.login_challenge_user(&b.challenge).await?;
    check_throttle(&s, &user_id)?;
    match s.core.complete_login_challenge(&b.challenge, &b.code).await {
        Ok(outcome) => {
            s.throttle.succeed(&throttle_key(&user_id));
            let (_, cookie) = start_session(&s, &outcome.user.id, &headers).await?;
            Ok((
                [(header::SET_COOKIE, cookie)],
                Json(json!({"user": outcome.user, "factor": outcome.factor, "recovery_codes": outcome.recovery_codes})),
            )
                .into_response())
        }
        Err(e) => {
            s.throttle.fail(&throttle_key(&user_id));
            Err(e.into())
        }
    }
}

/// 2FA settings change only from an interactive session (like minting tokens).
fn interactive(auth: &Auth) -> ApiResult<()> {
    if auth.via_cookie { Ok(()) } else { Err(ApiError(Error::Forbidden)) }
}

async fn overview(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    let identities: Vec<Value> = s
        .core
        .linked_identities(&auth.user.id)
        .await?
        .into_iter()
        .map(|l| {
            let name = s
                .core
                .oidc_provider(&l.provider)
                .map(|p| p.display_name.clone())
                .unwrap_or_else(|| l.provider.clone());
            json!({"provider": l.provider, "provider_name": name, "subject": l.subject, "email": l.email,
                   "created_at": l.created_at, "last_login_at": l.last_login_at})
        })
        .collect();
    Ok(Json(json!({
        "two_factor": s.core.two_factor_status(&auth.user.id).await?,
        "identities": identities,
        "providers": crate::oidc::public_providers(&s),
    })))
}

async fn begin_totp(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    interactive(&auth)?;
    let e = s.core.begin_totp_enrollment(&auth.user).await?;
    Ok(Json(enrollment_json(&e)?))
}

#[derive(Deserialize)]
struct CodeBody {
    code: String,
}

async fn confirm_totp(State(s): State<AppState>, auth: Auth, Json(b): Json<CodeBody>) -> ApiResult<Json<Value>> {
    interactive(&auth)?;
    check_throttle(&s, &auth.user.id)?;
    match s.core.confirm_totp_enrollment(&auth.user, &b.code).await {
        Ok(codes) => {
            s.throttle.succeed(&throttle_key(&auth.user.id));
            Ok(Json(json!({"recovery_codes": codes})))
        }
        Err(e) => {
            s.throttle.fail(&throttle_key(&auth.user.id));
            Err(e.into())
        }
    }
}

#[derive(Deserialize)]
struct ReauthBody {
    password: String,
    #[serde(default)]
    code: Option<String>,
}

/// Re-checks the password (throttled like a sign-in).
async fn check_password(s: &AppState, user: &User, password: &str) -> ApiResult<()> {
    let key = format!("login:{}", user.username.to_lowercase());
    if s.throttle.blocked(&key) {
        return Err(ApiError(Error::Unavailable("too many failed attempts; try again in a few minutes".into())));
    }
    match s.core.authenticate(&user.username, password).await {
        Ok(_) => Ok(()),
        Err(_) => {
            s.throttle.fail(&key);
            Err(ApiError(Error::bad("your password is wrong")))
        }
    }
}

async fn regenerate_codes(State(s): State<AppState>, auth: Auth, Json(b): Json<ReauthBody>) -> ApiResult<Json<Value>> {
    interactive(&auth)?;
    check_password(&s, &auth.user, &b.password).await?;
    Ok(Json(json!({"recovery_codes": s.core.regenerate_recovery_codes(&auth.user.id).await?})))
}

async fn disable(State(s): State<AppState>, auth: Auth, Json(b): Json<ReauthBody>) -> ApiResult<Json<Value>> {
    interactive(&auth)?;
    if s.core.config.require_two_factor {
        return Err(ApiError(Error::bad(
            "two-factor authentication is required on this server and can't be turned off",
        )));
    }
    check_password(&s, &auth.user, &b.password).await?;
    check_throttle(&s, &auth.user.id)?;
    let code = b.code.unwrap_or_default();
    if let Err(e) = s.core.verify_second_factor(&auth.user.id, &code).await {
        s.throttle.fail(&throttle_key(&auth.user.id));
        return Err(e.into());
    }
    s.throttle.succeed(&throttle_key(&auth.user.id));
    s.core.disable_two_factor(&auth.user, false).await?;
    Ok(ok())
}

async fn admin_reset(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    let user = s.core.user(&id).await?;
    let was = s.core.disable_two_factor(&user, true).await?;
    Ok(Json(json!({"ok": true, "was_enabled": was})))
}
