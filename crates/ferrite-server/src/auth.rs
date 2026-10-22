//! Request authentication.
//!
//! * `Authorization: Bearer ft_…` — app tokens (MCP clients, scripts, sync clients).
//! * `ferrite_session` cookie — the web UI. State-changing requests must also carry the
//!   `X-Ferrite-Request` header, which browsers won't send cross-site without a CORS
//!   preflight that this server never grants (CSRF protection).
//! * `Authorization: Basic` — username plus app token or password, for WebDAV clients.

use crate::AppState;
use crate::error::ApiError;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method};
use base64::Engine;
use dashmap::DashMap;
use ferrite_core::Error;
use ferrite_core::auth::{TokenInfo, User};
use std::time::{Duration, Instant};

pub const SESSION_COOKIE: &str = "ferrite_session";
pub const CSRF_HEADER: &str = "x-ferrite-request";
/// Why a correct password is refused over Basic auth: the account needs a second factor,
/// which Basic auth can't carry. App tokens keep working.
pub const BASIC_NEEDS_APP_TOKEN: &str =
    "two-factor authentication is enabled for this account: use an app token as the password (create one in Settings → Tokens & MCP)";

#[derive(Clone)]
pub struct Auth {
    pub user: User,
    pub token: TokenInfo,
    pub via_cookie: bool,
}

impl Auth {
    pub fn require_scope(&self, scope: &str) -> Result<(), ApiError> {
        if self.token.allows(scope) { Ok(()) } else { Err(ApiError(Error::Forbidden)) }
    }
    pub fn require_admin(&self) -> Result<(), ApiError> {
        if self.user.is_admin && self.token.allows("admin") {
            Ok(())
        } else {
            Err(ApiError(Error::Forbidden))
        }
    }
}

pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all("cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|kv| {
            let (k, v) = kv.trim().split_once('=')?;
            (k == name).then(|| v.to_string())
        })
}

/// Failed-login bookkeeping: after 8 failures in 10 minutes a key is refused until the window
/// passes, which makes online password guessing impractical.
#[derive(Default)]
pub struct Throttle {
    failures: DashMap<String, (u32, Instant)>,
}

const MAX_FAILURES: u32 = 8;
const WINDOW: Duration = Duration::from_secs(600);

impl Throttle {
    pub fn blocked(&self, key: &str) -> bool {
        match self.failures.get(key) {
            Some(e) if e.1.elapsed() < WINDOW => e.0 >= MAX_FAILURES,
            _ => false,
        }
    }
    pub fn fail(&self, key: &str) {
        let mut e = self.failures.entry(key.to_string()).or_insert((0, Instant::now()));
        if e.1.elapsed() >= WINDOW {
            *e = (0, Instant::now());
        }
        e.0 += 1;
    }
    pub fn succeed(&self, key: &str) {
        self.failures.remove(key);
    }
}

pub async fn authenticate(state: &AppState, headers: &HeaderMap, method: &Method, allow_basic: bool) -> Result<Auth, ApiError> {
    if let Some(h) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        if let Some(secret) = h.strip_prefix("Bearer ").map(str::trim) {
            let (user, token) = state.core.resolve_token(secret).await?;
            return Ok(Auth {
                user,
                token,
                via_cookie: false,
            });
        }
        if let (true, Some(b64)) = (allow_basic, h.strip_prefix("Basic ")) {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .map_err(|_| ApiError(Error::Unauthorized))?;
            let raw = String::from_utf8(raw).map_err(|_| ApiError(Error::Unauthorized))?;
            let (username, secret) = raw.split_once(':').ok_or(ApiError(Error::Unauthorized))?;
            let key = format!("basic:{}", username.to_lowercase());
            if state.throttle.blocked(&key) {
                return Err(ApiError(Error::Unavailable("too many failed attempts, try again later".into())));
            }
            if secret.starts_with("ft_") {
                if let Ok((user, token)) = state.core.resolve_token(secret).await {
                    if user.username.eq_ignore_ascii_case(username) {
                        state.throttle.succeed(&key);
                        return Ok(Auth {
                            user,
                            token,
                            via_cookie: false,
                        });
                    }
                }
            } else if let Ok(user) = state.core.authenticate(username, secret).await {
                state.throttle.succeed(&key);
                if state.core.config.require_two_factor || state.core.two_factor_enabled(&user.id).await? {
                    return Err(ApiError(Error::BadRequest(BASIC_NEEDS_APP_TOKEN.into())));
                }
                let token = TokenInfo {
                    id: String::new(),
                    user_id: user.id.clone(),
                    kind: "password".into(),
                    name: "basic".into(),
                    // Passwords over Basic auth serve sync clients only (files and groupware).
                    scopes: "files,groupware".into(),
                    created_at: ferrite_core::util::now(),
                    last_used_at: None,
                    expires_at: None,
                };
                return Ok(Auth {
                    user,
                    token,
                    via_cookie: false,
                });
            }
            state.throttle.fail(&key);
            return Err(ApiError(Error::Unauthorized));
        }
    }
    if let Some(secret) = cookie(headers, SESSION_COOKIE) {
        let (user, token) = state.core.resolve_token(&secret).await?;
        let safe = matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
        if !safe && !headers.contains_key(CSRF_HEADER) {
            return Err(ApiError(Error::Forbidden));
        }
        return Ok(Auth { user, token, via_cookie: true });
    }
    Err(ApiError(Error::Unauthorized))
}

impl FromRequestParts<AppState> for Auth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        authenticate(state, &parts.headers, &parts.method, false).await
    }
}

/// Like [`Auth`] but optional (pages that work signed in or out).
pub struct MaybeAuth(pub Option<Auth>);

impl FromRequestParts<AppState> for MaybeAuth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        Ok(MaybeAuth(authenticate(state, &parts.headers, &parts.method, false).await.ok()))
    }
}

/// 401 with a Basic challenge for WebDAV-style clients; `message` explains the refusal.
pub fn basic_challenge(message: &str) -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut r = (axum::http::StatusCode::UNAUTHORIZED, message.to_string()).into_response();
    r.headers_mut().insert(
        axum::http::header::WWW_AUTHENTICATE,
        axum::http::HeaderValue::from_static("Basic realm=\"Ferrite\", charset=\"UTF-8\""),
    );
    r
}

pub fn session_cookie(secret: &str, secure: bool, max_age_secs: i64) -> String {
    format!(
        "{SESSION_COOKIE}={secret}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}{}",
        if secure { "; Secure" } else { "" }
    )
}
