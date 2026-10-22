//! OpenID Connect sign-in (relying party), per OpenID Connect Core 1.0 §3.1: the
//! Authorization Code flow with PKCE (RFC 7636, S256).
//!
//! 1. `GET /api/auth/oidc/{id}/start` creates `state`, `nonce` and a PKCE verifier, stores
//!    them server-side bound to a short-lived browser cookie, and redirects to the provider.
//! 2. `GET /api/auth/oidc/{id}/callback` consumes the state (once), checks the cookie,
//!    exchanges the code (with the verifier) at the token endpoint, validates the ID token
//!    (signature against the provider's JWKS, RS256 or ES256; `iss`, `aud`, `azp`, `exp`,
//!    `iat`, `nonce`), maps the identity to a user, starts a session and redirects into the app.
//!
//! Errors end on the sign-in page with `?sso_error=…`. SSO sessions skip Ferrite's own
//! two-factor step: the provider is responsible for strong authentication.

use crate::AppState;
use crate::auth::cookie;
use crate::error::public_message;
use aws_lc_rs::signature::{self, RsaPublicKeyComponents, UnparsedPublicKey};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine;
use dashmap::DashMap;
use ferrite_core::Error;
use ferrite_core::sso::{ExternalIdentity, OidcProvider};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Binds an authorization request to the browser that started it.
pub const FLOW_COOKIE: &str = "ferrite_oidc";
/// Clock skew tolerated on `exp`, `iat` and `nbf`.
pub const LEEWAY_SECS: i64 = 60;
/// ID tokens issued longer ago than this are refused (the request itself lives 10 minutes).
pub const MAX_TOKEN_AGE_SECS: i64 = 600;
/// Discovery documents and key sets are refreshed after this long (and on an unknown `kid`).
const CACHE_TTL: Duration = Duration::from_secs(3600);

/// The parts of the discovery document (OpenID Connect Discovery 1.0 §3) Ferrite uses.
#[derive(Debug, Clone, Deserialize)]
pub struct Discovery {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
    #[serde(default)]
    pub token_endpoint_auth_methods_supported: Vec<String>,
}

/// A JSON Web Key (RFC 7517), RSA or EC.
#[derive(Debug, Clone, Deserialize)]
pub struct Jwk {
    pub kty: String,
    #[serde(default)]
    pub kid: Option<String>,
    #[serde(default, rename = "use")]
    pub use_: Option<String>,
    #[serde(default)]
    pub alg: Option<String>,
    #[serde(default)]
    pub n: Option<String>,
    #[serde(default)]
    pub e: Option<String>,
    #[serde(default)]
    pub crv: Option<String>,
    #[serde(default)]
    pub x: Option<String>,
    #[serde(default)]
    pub y: Option<String>,
}

#[derive(Deserialize)]
struct JwkSet {
    keys: Vec<Jwk>,
}

struct Cached {
    at: Instant,
    discovery: Discovery,
    keys: Vec<Jwk>,
}

/// Discovery and key cache plus the HTTP client used to talk to providers.
pub struct Oidc {
    http: reqwest::Client,
    cache: DashMap<String, Arc<Cached>>,
}

impl Default for Oidc {
    fn default() -> Self {
        Oidc {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .user_agent(concat!("Ferrite/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default(),
            cache: DashMap::new(),
        }
    }
}

/// Why an ID token was refused. `UnknownKey` makes the caller refresh the key set once.
#[derive(Debug, PartialEq, Eq)]
pub enum TokenError {
    UnknownKey,
    Invalid(String),
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::UnknownKey => f.write_str("the ID token is signed with a key the provider does not publish"),
            TokenError::Invalid(m) => f.write_str(m),
        }
    }
}

fn invalid(m: impl Into<String>) -> TokenError {
    TokenError::Invalid(m.into())
}

/// What the ID token must say.
pub struct Expected<'a> {
    pub issuer: &'a str,
    pub client_id: &'a str,
    pub nonce: &'a str,
    /// Current Unix time.
    pub now: i64,
}

fn b64url(s: &str) -> Result<Vec<u8>, TokenError> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .map_err(|_| invalid("the ID token is not valid base64url"))
}

/// PKCE S256 (RFC 7636 §4.2): BASE64URL(SHA256(ASCII(code_verifier))).
pub fn pkce_challenge(verifier: &str) -> String {
    let d = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(d.as_ref())
}

fn verify_with(alg: &str, key: &Jwk, message: &[u8], sig: &[u8]) -> bool {
    match alg {
        "RS256" => {
            let (Some(n), Some(e)) = (key.n.as_deref().and_then(|v| b64url(v).ok()), key.e.as_deref().and_then(|v| b64url(v).ok())) else {
                return false;
            };
            let strip = |v: &[u8]| v.iter().position(|b| *b != 0).map(|i| v[i..].to_vec()).unwrap_or_default();
            RsaPublicKeyComponents { n: strip(&n), e: strip(&e) }
                .verify(&signature::RSA_PKCS1_2048_8192_SHA256, message, sig)
                .is_ok()
        }
        "ES256" => {
            let (Some(x), Some(y)) = (key.x.as_deref().and_then(|v| b64url(v).ok()), key.y.as_deref().and_then(|v| b64url(v).ok())) else {
                return false;
            };
            if x.len() != 32 || y.len() != 32 || sig.len() != 64 {
                return false;
            }
            let mut point = Vec::with_capacity(65);
            point.push(0x04);
            point.extend_from_slice(&x);
            point.extend_from_slice(&y);
            UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, point).verify(message, sig).is_ok()
        }
        _ => false,
    }
}

fn num_claim(claims: &Map<String, Value>, name: &str) -> Result<Option<i64>, TokenError> {
    match claims.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_i64()
            .or_else(|| v.as_f64().map(|f| f as i64))
            .map(Some)
            .ok_or_else(|| invalid(format!("the ID token's {name} claim is not a number"))),
    }
}

/// Validates a compact-serialized ID token (OpenID Connect Core 1.0 §3.1.3.7) and returns
/// its claims.
pub fn verify_id_token(token: &str, keys: &[Jwk], want: &Expected) -> Result<Map<String, Value>, TokenError> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(invalid("the ID token is not a signed JWT"));
    }
    let header: Map<String, Value> = serde_json::from_slice(&b64url(parts[0])?).map_err(|_| invalid("the ID token header is not JSON"))?;
    let alg = header.get("alg").and_then(Value::as_str).unwrap_or_default();
    let (kty, crv) = match alg {
        "RS256" => ("RSA", None),
        "ES256" => ("EC", Some("P-256")),
        other => return Err(invalid(format!("unsupported ID token algorithm {other:?} (RS256 and ES256 are accepted)"))),
    };
    if header.contains_key("crit") {
        return Err(invalid("the ID token uses critical header extensions"));
    }
    let kid = header.get("kid").and_then(Value::as_str);
    let candidates: Vec<&Jwk> = keys
        .iter()
        .filter(|k| k.kty == kty && (crv.is_none() || k.crv.as_deref() == crv))
        .filter(|k| k.use_.as_deref().is_none_or(|u| u == "sig"))
        .filter(|k| k.alg.as_deref().is_none_or(|a| a == alg))
        .filter(|k| kid.is_none() || k.kid.as_deref() == kid)
        .collect();
    if candidates.is_empty() {
        return Err(TokenError::UnknownKey);
    }
    let message = format!("{}.{}", parts[0], parts[1]);
    let sig = b64url(parts[2])?;
    if !candidates.iter().any(|k| verify_with(alg, k, message.as_bytes(), &sig)) {
        return Err(invalid("the ID token signature is not valid"));
    }
    let claims: Map<String, Value> = serde_json::from_slice(&b64url(parts[1])?).map_err(|_| invalid("the ID token payload is not JSON"))?;
    if claims.get("iss").and_then(Value::as_str) != Some(want.issuer) {
        return Err(invalid("the ID token was issued by another issuer"));
    }
    let audiences: Vec<&str> = match claims.get("aud") {
        Some(Value::String(a)) => vec![a.as_str()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if !audiences.contains(&want.client_id) {
        return Err(invalid("the ID token is meant for another client (aud)"));
    }
    if audiences.iter().any(|a| *a != want.client_id) {
        return Err(invalid("the ID token lists audiences Ferrite does not trust"));
    }
    if let Some(azp) = claims.get("azp") {
        if azp.as_str() != Some(want.client_id) {
            return Err(invalid("the ID token's authorized party (azp) is another client"));
        }
    }
    let exp = num_claim(&claims, "exp")?.ok_or_else(|| invalid("the ID token has no expiry"))?;
    if want.now >= exp + LEEWAY_SECS {
        return Err(invalid("the ID token has expired"));
    }
    let iat = num_claim(&claims, "iat")?.ok_or_else(|| invalid("the ID token has no issue time"))?;
    if iat > want.now + LEEWAY_SECS || iat < want.now - MAX_TOKEN_AGE_SECS {
        return Err(invalid("the ID token's issue time is too far from now"));
    }
    if num_claim(&claims, "nbf")?.is_some_and(|nbf| nbf > want.now + LEEWAY_SECS) {
        return Err(invalid("the ID token is not valid yet"));
    }
    let nonce = claims.get("nonce").and_then(Value::as_str).unwrap_or_default();
    if aws_lc_rs::constant_time::verify_slices_are_equal(nonce.as_bytes(), want.nonce.as_bytes()).is_err() {
        return Err(invalid("the ID token's nonce does not match this sign-in"));
    }
    if claims.get("sub").and_then(Value::as_str).is_none_or(str::is_empty) {
        return Err(invalid("the ID token has no subject"));
    }
    Ok(claims)
}

/// The identity a validated ID token describes.
pub fn identity(claims: &Map<String, Value>, username_claim: &str) -> ExternalIdentity {
    let s = |k: &str| claims.get(k).and_then(Value::as_str).map(str::to_string).filter(|v| !v.is_empty());
    ExternalIdentity {
        subject: s("sub").unwrap_or_default(),
        username: s(username_claim),
        email: s("email"),
        // Some providers send the boolean as a string.
        email_verified: matches!(claims.get("email_verified"), Some(Value::Bool(true))) || claims.get("email_verified").and_then(Value::as_str) == Some("true"),
        name: s("name"),
        groups: match claims.get("groups") {
            Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
            Some(Value::String(g)) => vec![g.clone()],
            _ => Vec::new(),
        },
    }
}

/// Form-encodes a value (`application/x-www-form-urlencoded`, also used for query strings).
fn form(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

impl Oidc {
    async fn fetch_json<T: serde::de::DeserializeOwned>(&self, url: &str, what: &str) -> Result<T, Error> {
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| Error::Unavailable(format!("cannot reach the identity provider ({what}): {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Unavailable(format!("the identity provider's {what} answered {}", resp.status())));
        }
        resp.json()
            .await
            .map_err(|e| Error::Unavailable(format!("the identity provider's {what} is not valid: {e}")))
    }

    async fn load(&self, p: &OidcProvider) -> Result<Arc<Cached>, Error> {
        let url = format!("{}/.well-known/openid-configuration", p.issuer.trim_end_matches('/'));
        let discovery: Discovery = self.fetch_json(&url, "discovery document").await?;
        // OpenID Connect Discovery 1.0 §4.3: the issuer must be exactly the one configured.
        if discovery.issuer != p.issuer {
            return Err(Error::Unavailable(format!(
                "the provider's discovery document names issuer {:?}, but {:?} is configured",
                discovery.issuer, p.issuer
            )));
        }
        let keys: JwkSet = self.fetch_json(&discovery.jwks_uri, "key set").await?;
        let cached = Arc::new(Cached {
            at: Instant::now(),
            discovery,
            keys: keys.keys,
        });
        self.cache.insert(p.id.clone(), cached.clone());
        Ok(cached)
    }

    async fn provider(&self, p: &OidcProvider) -> Result<Arc<Cached>, Error> {
        match self.cache.get(&p.id).map(|c| c.clone()) {
            Some(c) if c.at.elapsed() < CACHE_TTL => Ok(c),
            _ => self.load(p).await,
        }
    }

    /// Exchanges an authorization code for tokens and returns the ID token.
    async fn exchange(&self, p: &OidcProvider, d: &Discovery, code: &str, redirect_uri: &str, verifier: &str) -> Result<String, Error> {
        let mut body = format!(
            "grant_type=authorization_code&code={}&redirect_uri={}&code_verifier={}",
            form(code),
            form(redirect_uri),
            form(verifier)
        );
        let methods = &d.token_endpoint_auth_methods_supported;
        // client_secret_basic is the default (and what an empty list means); RFC 6749 §2.3.1
        // form-encodes both parts before Basic encoding.
        let use_post = !methods.is_empty() && !methods.iter().any(|m| m == "client_secret_basic") && methods.iter().any(|m| m == "client_secret_post");
        let mut req = self.http.post(&d.token_endpoint).header(header::ACCEPT, "application/json");
        if use_post {
            body.push_str(&format!("&client_id={}&client_secret={}", form(&p.client_id), form(&p.client_secret)));
        } else {
            let basic = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", form(&p.client_id), form(&p.client_secret)));
            req = req.header(header::AUTHORIZATION, format!("Basic {basic}"));
        }
        let resp = req
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|e| Error::Unavailable(format!("cannot reach the identity provider's token endpoint: {e}")))?;
        let status = resp.status();
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            let why = v
                .get("error_description")
                .or_else(|| v.get("error"))
                .and_then(Value::as_str)
                .unwrap_or("no details");
            return Err(Error::bad(format!("the identity provider refused the code ({status}): {why}")));
        }
        v.get("id_token")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| Error::bad("the identity provider did not return an ID token"))
    }

    /// Validates `token`, refreshing the key set once if it is signed by an unknown key
    /// (the provider rotated its keys).
    async fn validate(&self, p: &OidcProvider, cached: Arc<Cached>, token: &str, nonce: &str) -> Result<Map<String, Value>, Error> {
        let want = Expected {
            issuer: &p.issuer,
            client_id: &p.client_id,
            nonce,
            now: ferrite_core::util::now().timestamp(),
        };
        match verify_id_token(token, &cached.keys, &want) {
            Ok(c) => Ok(c),
            Err(TokenError::UnknownKey) => {
                let fresh = self.load(p).await?;
                verify_id_token(token, &fresh.keys, &want).map_err(|e| Error::bad(e.to_string()))
            }
            Err(e) => Err(Error::bad(e.to_string())),
        }
    }
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/auth/oidc/{id}/start", get(start))
        .route("/api/auth/oidc/{id}/callback", get(callback))
}

/// Providers for the sign-in page: `[{id, name}]`.
pub fn public_providers(s: &AppState) -> Value {
    json!(
        s.core
            .config
            .oidc
            .iter()
            .map(|p| json!({"id": p.id, "name": p.display_name}))
            .collect::<Vec<_>>()
    )
}

fn redirect_uri(s: &AppState, id: &str) -> String {
    format!("{}/api/auth/oidc/{id}/callback", s.core.config.public_url)
}

fn flow_cookie(value: &str, secure: bool, max_age: i64) -> String {
    format!(
        "{FLOW_COOKIE}={value}; Path=/api/auth/oidc; HttpOnly; SameSite=Lax; Max-Age={max_age}{}",
        if secure { "; Secure" } else { "" }
    )
}

fn see_other(location: &str, cookies: &[String]) -> Response {
    let mut r = StatusCode::SEE_OTHER.into_response();
    if let Ok(v) = HeaderValue::from_str(location) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    for c in cookies {
        if let Ok(v) = HeaderValue::from_str(c) {
            r.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    r
}

/// Back to the sign-in page with a message; the flow cookie is cleared.
fn failed(s: &AppState, message: &str) -> Response {
    see_other(&format!("/?sso_error={}", form(message)), &[flow_cookie("", s.core.config.secure_cookies, 0)])
}

/// Only same-site paths: no scheme, no host, no protocol-relative `//`.
fn safe_return_to(r: Option<&str>) -> String {
    match r {
        Some(p) if p.starts_with('/') && !p.starts_with("//") && !p.starts_with("/\\") && !p.contains(['\r', '\n']) => p.to_string(),
        _ => "/".into(),
    }
}

#[derive(Deserialize)]
struct StartQuery {
    return_to: Option<String>,
}

async fn start(State(s): State<AppState>, Path(id): Path<String>, Query(q): Query<StartQuery>) -> Response {
    let Some(p) = s.core.oidc_provider(&id).cloned() else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "no such sign-in provider"}))).into_response();
    };
    let cached = match s.oidc.provider(&p).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(provider = %p.id, error = %e, "OIDC discovery failed");
            return failed(&s, &format!("{} is not reachable right now: {}", p.display_name, public_message(&e)));
        }
    };
    let browser = ferrite_core::util::random_token(24);
    let state = ferrite_core::util::random_token(24);
    let req = s.core.new_oidc_request(&p.id, &browser, &safe_return_to(q.return_to.as_deref()));
    if let Err(e) = s.core.save_oidc_request(&state, &req).await {
        return failed(&s, &public_message(&e));
    }
    let endpoint = &cached.discovery.authorization_endpoint;
    let url = format!(
        "{endpoint}{}response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&nonce={}&code_challenge={}&code_challenge_method=S256",
        if endpoint.contains('?') { '&' } else { '?' },
        form(&p.client_id),
        form(&redirect_uri(&s, &p.id)),
        form(&p.scopes),
        form(&state),
        form(&req.nonce),
        form(&pkce_challenge(&req.verifier)),
    );
    let max_age = ferrite_core::sso::REQUEST_TTL_SECS;
    see_other(&url, &[flow_cookie(&browser, s.core.config.secure_cookies, max_age)])
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

async fn callback(State(s): State<AppState>, Path(id): Path<String>, Query(q): Query<CallbackQuery>, headers: HeaderMap) -> Response {
    match finish(&s, &id, q, &headers).await {
        Ok(r) => r,
        Err(e) => {
            tracing::info!(provider = %id, error = %e, "SSO sign-in refused");
            failed(&s, &public_message(&e))
        }
    }
}

async fn finish(s: &AppState, id: &str, q: CallbackQuery, headers: &HeaderMap) -> Result<Response, Error> {
    let p = s.core.oidc_provider(id).cloned().ok_or(Error::NotFound)?;
    if let Some(err) = q.error {
        let why = q.error_description.unwrap_or(err);
        return Err(Error::bad(format!("{} did not sign you in: {why}", p.display_name)));
    }
    let state = q.state.ok_or_else(|| Error::bad("the sign-in response has no state"))?;
    let expired = || Error::bad("this sign-in has expired or was already used; please try again");
    let req = s.core.take_oidc_request(&state).await?.ok_or_else(expired)?;
    if req.provider != p.id {
        return Err(expired());
    }
    let browser = cookie(headers, FLOW_COOKIE).unwrap_or_default();
    if browser.is_empty() || ferrite_core::util::sha256_hex(browser.as_bytes()) != req.browser_hash {
        return Err(Error::bad("this sign-in was started in another browser or tab; please try again"));
    }
    let code = q.code.ok_or_else(|| Error::bad("the sign-in response has no code"))?;
    let cached = s.oidc.provider(&p).await?;
    let token = s.oidc.exchange(&p, &cached.discovery, &code, &redirect_uri(s, &p.id), &req.verifier).await?;
    let claims = s.oidc.validate(&p, cached, &token, &req.nonce).await?;
    let user = s.core.sso_user(&p, &identity(&claims, &p.username_claim)).await?;
    let (_, session) = crate::api::start_session(s, &user.id, headers).await.map_err(|e| e.0)?;
    tracing::info!(provider = %p.id, user = %user.username, "signed in with SSO");
    Ok(see_other(&req.return_to, &[session, flow_cookie("", s.core.config.secure_cookies, 0)]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::rand::SystemRandom;
    use aws_lc_rs::rsa::KeySize;
    use aws_lc_rs::signature::{EcdsaKeyPair, KeyPair, RsaKeyPair};

    fn enc(b: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
    }

    #[test]
    fn pkce_rfc7636_appendix_b() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    fn claims(now: i64) -> Value {
        json!({"iss": "https://idp.test", "aud": "ferrite", "sub": "u-1", "nonce": "n0nce", "iat": now, "exp": now + 300,
               "preferred_username": "ada", "email": "ada@example.org", "email_verified": "true", "groups": ["staff", "admins"]})
    }

    fn want(now: i64) -> Expected<'static> {
        Expected {
            issuer: "https://idp.test",
            client_id: "ferrite",
            nonce: "n0nce",
            now,
        }
    }

    fn rsa() -> (RsaKeyPair, Jwk) {
        let kp = RsaKeyPair::generate(KeySize::Rsa2048).unwrap();
        let c = RsaPublicKeyComponents::<Vec<u8>>::from(kp.public_key());
        let jwk = serde_json::from_value(json!({"kty": "RSA", "kid": "r1", "use": "sig", "alg": "RS256", "n": enc(&c.n), "e": enc(&c.e)})).unwrap();
        (kp, jwk)
    }

    fn sign_rs256(kp: &RsaKeyPair, kid: &str, claims: &Value) -> String {
        let input = format!(
            "{}.{}",
            enc(json!({"alg": "RS256", "kid": kid}).to_string().as_bytes()),
            enc(claims.to_string().as_bytes())
        );
        let mut sig = vec![0u8; kp.public_modulus_len()];
        kp.sign(&signature::RSA_PKCS1_SHA256, &SystemRandom::new(), input.as_bytes(), &mut sig).unwrap();
        format!("{input}.{}", enc(&sig))
    }

    #[test]
    fn rs256_tokens() {
        let now = 1_800_000_000;
        let (kp, jwk) = rsa();
        let keys = vec![jwk];
        let good = sign_rs256(&kp, "r1", &claims(now));
        let c = verify_id_token(&good, &keys, &want(now)).unwrap();
        let id = identity(&c, "preferred_username");
        assert_eq!((id.subject.as_str(), id.username.as_deref(), id.email_verified), ("u-1", Some("ada"), true));
        assert_eq!(id.groups, vec!["staff", "admins"]);

        // Tampered payload, unknown kid, wrong algorithm.
        let mut parts: Vec<String> = good.split('.').map(String::from).collect();
        let mut forged = claims(now);
        forged["sub"] = json!("someone-else");
        parts[1] = enc(forged.to_string().as_bytes());
        assert_eq!(
            verify_id_token(&parts.join("."), &keys, &want(now)),
            Err(invalid("the ID token signature is not valid"))
        );
        assert_eq!(
            verify_id_token(&sign_rs256(&kp, "r2", &claims(now)), &keys, &want(now)),
            Err(TokenError::UnknownKey)
        );
        let none = format!("{}.{}.", enc(br#"{"alg":"none"}"#), enc(claims(now).to_string().as_bytes()));
        assert!(matches!(verify_id_token(&none, &keys, &want(now)), Err(TokenError::Invalid(m)) if m.contains("unsupported")));

        // Claim checks.
        let check = |f: &dyn Fn(&mut Value), needle: &str| {
            let mut c = claims(now);
            f(&mut c);
            match verify_id_token(&sign_rs256(&kp, "r1", &c), &keys, &want(now)) {
                Err(TokenError::Invalid(m)) => assert!(m.contains(needle), "{m} should mention {needle}"),
                other => panic!("expected a refusal mentioning {needle}, got {other:?}"),
            }
        };
        check(&|c| c["iss"] = json!("https://evil.test"), "issuer");
        check(&|c| c["aud"] = json!("other-client"), "aud");
        check(&|c| c["aud"] = json!(["ferrite", "other-client"]), "audiences");
        check(&|c| c["azp"] = json!("other-client"), "azp");
        check(&|c| c["exp"] = json!(now - 120), "expired");
        check(&|c| c["iat"] = json!(now - 3600), "issue time");
        check(&|c| c["iat"] = json!(now + 3600), "issue time");
        check(&|c| c["nonce"] = json!("other"), "nonce");
        check(&|c| drop(c.as_object_mut().unwrap().remove("nonce")), "nonce");
        check(&|c| c["nbf"] = json!(now + 3600), "not valid yet");
        // Within the leeway.
        let mut c = claims(now);
        c["exp"] = json!(now - 30);
        assert!(verify_id_token(&sign_rs256(&kp, "r1", &c), &keys, &want(now)).is_ok());
        // An array audience with only us is fine.
        let mut c = claims(now);
        c["aud"] = json!(["ferrite"]);
        assert!(verify_id_token(&sign_rs256(&kp, "r1", &c), &keys, &want(now)).is_ok());
    }

    #[test]
    fn es256_tokens() {
        let now = 1_800_000_000;
        let kp = EcdsaKeyPair::generate(&signature::ECDSA_P256_SHA256_FIXED_SIGNING).unwrap();
        let point = kp.public_key().as_ref().to_vec();
        let jwk: Jwk = serde_json::from_value(json!({"kty": "EC", "crv": "P-256", "kid": "e1", "x": enc(&point[1..33]), "y": enc(&point[33..65])})).unwrap();
        let input = format!("{}.{}", enc(br#"{"alg":"ES256","kid":"e1"}"#), enc(claims(now).to_string().as_bytes()));
        let sig = kp.sign(&SystemRandom::new(), input.as_bytes()).unwrap();
        let token = format!("{input}.{}", enc(sig.as_ref()));
        assert!(verify_id_token(&token, std::slice::from_ref(&jwk), &want(now)).is_ok());
        // An RSA key with the same kid can't verify an ES256 token.
        let (_, mut rsa_jwk) = rsa();
        rsa_jwk.kid = Some("e1".into());
        assert_eq!(verify_id_token(&token, &[rsa_jwk], &want(now)), Err(TokenError::UnknownKey));
        let mut bad = token.clone();
        bad.replace_range(bad.len() - 4.., "AAAA");
        assert!(verify_id_token(&bad, &[jwk], &want(now)).is_err());
    }

    #[test]
    fn return_to_stays_on_site() {
        assert_eq!(safe_return_to(Some("/files/1")), "/files/1");
        assert_eq!(safe_return_to(Some("//evil.test")), "/");
        assert_eq!(safe_return_to(Some("https://evil.test")), "/");
        assert_eq!(safe_return_to(Some("/\\evil.test")), "/");
        assert_eq!(safe_return_to(None), "/");
    }
}
