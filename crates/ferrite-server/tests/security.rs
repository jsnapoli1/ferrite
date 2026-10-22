//! End-to-end tests of account security: TOTP two-factor sign-in (enrollment, codes,
//! recovery codes, replay, Basic auth, admin reset, required 2FA, security notices) and
//! OpenID Connect single sign-on against a mock identity provider.

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::rsa::KeySize;
use aws_lc_rs::signature::{self, EcdsaKeyPair, KeyPair, RsaKeyPair, RsaPublicKeyComponents};
use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode as AxStatus, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use ferrite_core::mailer::sink::{Received, sink};
use ferrite_core::mailer::{SmtpSecurity, SmtpSettings};
use ferrite_core::sso::OidcProvider;
use ferrite_core::twofactor::{PERIOD, base32_decode, format_code, hotp};
use ferrite_core::{Config, Core};
use ferrite_server::AppState;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

struct Server {
    url: String,
    core: Core,
    dir: tempfile::TempDir,
}

async fn start(tweak: impl FnOnce(&mut Config)) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let mut cfg = Config {
        data_dir: dir.path().to_path_buf(),
        public_url: url.clone(),
        ..Config::default()
    };
    cfg.sfu.enabled = false;
    tweak(&mut cfg);
    let core = Core::open(cfg).await.unwrap();
    let state = AppState::new(core.clone());
    let app = ferrite_server::router(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server { url, core, dir }
}

fn smtp(port: u16) -> Option<SmtpSettings> {
    Some(SmtpSettings {
        host: "127.0.0.1".into(),
        port,
        security: SmtpSecurity::None,
        username: None,
        password: None,
        from: "Ferrite <noreply@example.org>".into(),
    })
}

async fn next_mail(rx: &mut UnboundedReceiver<Received>) -> Received {
    tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("a mail arrives").unwrap()
}

/// A browser-like client: cookie session + the CSRF header the web app sends.
struct Browser {
    http: reqwest::Client,
    url: String,
}

impl Browser {
    fn new(url: &str) -> Self {
        Browser {
            http: reqwest::Client::builder().cookie_store(true).build().unwrap(),
            url: url.to_string(),
        }
    }
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = self.http.request(method, format!("{}{path}", self.url)).header("X-Ferrite-Request", "1");
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        (status, resp.json::<Value>().await.unwrap_or(Value::Null))
    }
    async fn get(&self, path: &str) -> Value {
        let (s, v) = self.call(Method::GET, path, None).await;
        assert!(s.is_success(), "GET {path}: {s} {v}");
        v
    }
    async fn post(&self, path: &str, body: Value) -> Value {
        let (s, v) = self.call(Method::POST, path, Some(body)).await;
        assert!(s.is_success(), "POST {path}: {s} {v}");
        v
    }
    async fn signed_in(&self) -> bool {
        self.call(Method::GET, "/api/me", None).await.0.is_success()
    }
}

/// Hands out TOTP codes for successive, strictly increasing time steps, as an authenticator
/// app used several times within a minute would (each step is accepted once).
struct Authenticator {
    key: Vec<u8>,
    last: Option<u64>,
}

impl Authenticator {
    fn new(secret_b32: &str) -> Self {
        Authenticator {
            key: base32_decode(secret_b32).unwrap(),
            last: None,
        }
    }
    fn current_step() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() / PERIOD
    }
    async fn code(&mut self) -> String {
        loop {
            let cur = Self::current_step();
            let step = self.last.map_or(cur - 1, |l| (l + 1).max(cur - 1));
            if step <= cur + 1 {
                self.last = Some(step);
                return format_code(hotp(&self.key, step, 6), 6);
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

async fn enroll(b: &Browser) -> (Authenticator, Vec<String>) {
    let e = b.post("/api/me/2fa/totp", json!({})).await;
    assert!(e["otpauth_uri"].as_str().unwrap().starts_with("otpauth://totp/Ferrite:"));
    assert!(
        e["otpauth_uri"]
            .as_str()
            .unwrap()
            .contains(&format!("secret={}", e["secret"].as_str().unwrap()))
    );
    assert!(e["qr"].as_str().unwrap().starts_with("data:image/svg+xml;base64,"));
    let mut app = Authenticator::new(e["secret"].as_str().unwrap());
    let (s, _) = b.call(Method::POST, "/api/me/2fa/totp/confirm", Some(json!({"code": "000000"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a wrong code doesn't enable 2FA");
    let r = b.post("/api/me/2fa/totp/confirm", json!({"code": app.code().await})).await;
    let codes: Vec<String> = serde_json::from_value(r["recovery_codes"].clone()).unwrap();
    assert_eq!(codes.len(), 10);
    (app, codes)
}

/// Password step; returns the challenge.
async fn password_step(b: &Browser, user: &str, password: &str) -> Value {
    let r = b.post("/api/auth/login", json!({"username": user, "password": password})).await;
    assert!(!b.signed_in().await, "no session before the second factor");
    r
}

async fn dav_propfind(url: &str, user: &str, secret: &str) -> reqwest::Response {
    reqwest::Client::new()
        .request(Method::from_bytes(b"PROPFIND").unwrap(), format!("{url}/remote.php/dav/files/{user}/"))
        .header("Depth", "0")
        .header(
            "Authorization",
            format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{secret}"))),
        )
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn totp_two_factor_sign_in() {
    let (port, mut mail) = sink().await;
    let srv = start(|c| c.smtp = smtp(port)).await;
    let admin = Browser::new(&srv.url);
    admin
        .post(
            "/api/setup",
            json!({"username": "alice", "password": "alice-password", "email": "alice@example.org"}),
        )
        .await;
    admin
        .post(
            "/api/admin/users",
            json!({"username": "bob", "password": "bob-password", "email": "bob@example.org", "display_name": "Bob"}),
        )
        .await;
    let bob = Browser::new(&srv.url);
    let r = bob.post("/api/auth/login", json!({"username": "bob", "password": "bob-password"})).await;
    assert_eq!(r["user"]["username"], "bob", "no 2FA yet: a session right away");
    // Basic auth with the password works before 2FA.
    assert_eq!(dav_propfind(&srv.url, "bob", "bob-password").await.status().as_u16(), 207);

    // Enroll: secret, URI, QR code; confirm; ten recovery codes; a security notice.
    let (mut app, codes) = enroll(&bob).await;
    let m = next_mail(&mut mail).await;
    assert_eq!(m.to, vec!["bob@example.org"]);
    assert!(m.data.contains("Subject: Ferrite: Two-factor authentication was turned on"), "{}", m.data);
    let sec = bob.get("/api/me/security").await;
    assert_eq!(sec["two_factor"]["enabled"], true);
    assert_eq!(sec["two_factor"]["recovery_codes_left"], 10);
    let (s, _) = bob.call(Method::POST, "/api/me/2fa/totp", Some(json!({}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "already enabled");

    // Sign in: the password alone gives a challenge, not a session.
    let phone = Browser::new(&srv.url);
    let ch = password_step(&phone, "bob", "bob-password").await;
    assert_eq!(ch["two_factor"], "totp");
    assert!(ch.get("user").is_none());
    let challenge = ch["challenge"].as_str().unwrap().to_string();
    let (s, _) = phone
        .call(Method::POST, "/api/auth/2fa", Some(json!({"challenge": challenge, "code": "123456"})))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let used = app.code().await;
    let r = phone.post("/api/auth/2fa", json!({"challenge": challenge, "code": used})).await;
    assert_eq!(r["user"]["username"], "bob");
    assert_eq!(r["factor"], "totp");
    assert!(phone.signed_in().await);
    // The challenge was single-use.
    let (s, v) = phone
        .call(Method::POST, "/api/auth/2fa", Some(json!({"challenge": challenge, "code": app.code().await})))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");

    // A code is accepted once: the same code again is refused; a recovery code works.
    let laptop = Browser::new(&srv.url);
    let ch = password_step(&laptop, "bob", "bob-password").await;
    let challenge = ch["challenge"].as_str().unwrap();
    let (s, _) = laptop
        .call(Method::POST, "/api/auth/2fa", Some(json!({"challenge": challenge, "code": used})))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a replayed code is refused");
    let r = laptop
        .post("/api/auth/2fa", json!({"challenge": challenge, "code": codes[0].to_uppercase()}))
        .await;
    assert_eq!(r["factor"], "recovery_code");
    assert!(laptop.signed_in().await);
    assert_eq!(laptop.get("/api/me/security").await["two_factor"]["recovery_codes_left"], 9);
    // A recovery code works once.
    let tablet = Browser::new(&srv.url);
    let ch = password_step(&tablet, "bob", "bob-password").await;
    let (s, _) = tablet
        .call(Method::POST, "/api/auth/2fa", Some(json!({"challenge": ch["challenge"], "code": codes[0]})))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a used recovery code is refused");

    // Basic auth with the password is refused (and says why); app tokens keep working.
    let r = dav_propfind(&srv.url, "bob", "bob-password").await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    assert!(r.headers().contains_key("www-authenticate"));
    assert!(r.text().await.unwrap().contains("app token"));
    let token = phone.post("/api/me/tokens", json!({"name": "sync"})).await["secret"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(dav_propfind(&srv.url, "bob", &token).await.status().as_u16(), 207);
    let r = reqwest::Client::new()
        .get(format!("{}/api/me", srv.url))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "bearer app tokens are unaffected");

    // Regenerating recovery codes needs the password; old codes stop working.
    let (s, _) = phone
        .call(Method::POST, "/api/me/2fa/recovery-codes", Some(json!({"password": "wrong-password"})))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let fresh: Vec<String> =
        serde_json::from_value(phone.post("/api/me/2fa/recovery-codes", json!({"password": "bob-password"})).await["recovery_codes"].clone()).unwrap();
    assert_eq!(fresh.len(), 10);
    let ch = password_step(&tablet, "bob", "bob-password").await;
    let (s, _) = tablet
        .call(Method::POST, "/api/auth/2fa", Some(json!({"challenge": ch["challenge"], "code": codes[1]})))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "codes from before the regeneration are void");
    tablet.post("/api/auth/2fa", json!({"challenge": ch["challenge"], "code": fresh[0]})).await;

    // The admin sees who uses 2FA and can reset it; that sends a notice too.
    let users = admin.get("/api/admin/users").await;
    let row = |name: &str| {
        users["users"]
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["user"]["username"] == name)
            .unwrap()
            .clone()
    };
    assert_eq!(row("bob")["two_factor"], true);
    assert_eq!(row("alice")["two_factor"], false);
    let (s, _) = bob
        .call(
            Method::DELETE,
            &format!("/api/admin/users/{}/2fa", row("bob")["user"]["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "only admins reset 2FA");
    let r = admin
        .call(
            Method::DELETE,
            &format!("/api/admin/users/{}/2fa", row("bob")["user"]["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(r.1["was_enabled"], true);
    let m = next_mail(&mut mail).await;
    assert!(m.data.contains("Subject: Ferrite: Two-factor authentication was turned off"));
    let r = Browser::new(&srv.url)
        .post("/api/auth/login", json!({"username": "bob", "password": "bob-password"}))
        .await;
    assert_eq!(r["user"]["username"], "bob", "after the reset the password alone signs in");
    assert_eq!(dav_propfind(&srv.url, "bob", "bob-password").await.status().as_u16(), 207);

    // Turning it off yourself needs the password and a code.
    let (mut app, _) = enroll(&bob).await;
    next_mail(&mut mail).await;
    let (s, _) = bob
        .call(Method::POST, "/api/me/2fa/disable", Some(json!({"password": "bob-password", "code": "000000"})))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = bob
        .call(
            Method::POST,
            "/api/me/2fa/disable",
            Some(json!({"password": "nope-nope", "code": app.code().await})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    bob.post("/api/me/2fa/disable", json!({"password": "bob-password", "code": app.code().await}))
        .await;
    assert_eq!(bob.get("/api/me/security").await["two_factor"]["enabled"], false);
    let m = next_mail(&mut mail).await;
    assert!(m.data.contains("turned off"));
}

#[tokio::test]
async fn wrong_codes_are_throttled_and_cli_resets() {
    let srv = start(|_| {}).await;
    let admin = Browser::new(&srv.url);
    admin.post("/api/setup", json!({"username": "carol", "password": "carol-password"})).await;
    let (mut app, _) = enroll(&admin).await;
    // Five wrong codes discard a challenge; eight in ten minutes block the account's codes.
    let b = Browser::new(&srv.url);
    let mut failures = 0;
    while failures < 8 {
        let ch = password_step(&b, "carol", "carol-password").await;
        for _ in 0..5 {
            if failures == 8 {
                break;
            }
            let (s, _) = b
                .call(Method::POST, "/api/auth/2fa", Some(json!({"challenge": ch["challenge"], "code": "999999"})))
                .await;
            assert_eq!(s, StatusCode::BAD_REQUEST);
            failures += 1;
        }
        if failures % 5 == 0 {
            let (s, _) = b
                .call(
                    Method::POST,
                    "/api/auth/2fa",
                    Some(json!({"challenge": ch["challenge"], "code": app.code().await})),
                )
                .await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "the challenge is gone after five wrong codes");
        }
    }
    let ch = password_step(&b, "carol", "carol-password").await;
    let (s, v) = b
        .call(
            Method::POST,
            "/api/auth/2fa",
            Some(json!({"challenge": ch["challenge"], "code": app.code().await})),
        )
        .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{v}");
    assert!(v["error"].as_str().unwrap().contains("too many"));

    // `ferrite 2fa-reset <user>` turns it off from the command line.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ferrite"))
        .args(["--config", "/nonexistent/ferrite.toml", "2fa-reset", "carol"])
        .env("FERRITE_DATA_DIR", srv.dir.path())
        .env("FERRITE_SFU", "0")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("turned off for carol"));
    assert!(!srv.core.two_factor_enabled(&srv.core.user_by_name("carol").await.unwrap().id).await.unwrap());
}

#[tokio::test]
async fn required_two_factor_enrolls_at_sign_in() {
    let (port, mut mail) = sink().await;
    let srv = start(|c| {
        c.smtp = smtp(port);
        c.require_two_factor = true;
    })
    .await;
    // Even the first administrator sets up 2FA before getting a session.
    let admin = Browser::new(&srv.url);
    let r = admin
        .post(
            "/api/setup",
            json!({"username": "dora", "password": "dora-password", "email": "dora@example.org"}),
        )
        .await;
    assert_eq!(r["two_factor"], "enroll");
    assert!(!admin.signed_in().await);
    let enrollment = &r["enrollment"];
    assert!(enrollment["qr"].as_str().unwrap().starts_with("data:image/svg+xml;base64,"));
    let mut app = Authenticator::new(enrollment["secret"].as_str().unwrap());
    let (s, _) = admin
        .call(Method::POST, "/api/auth/2fa", Some(json!({"challenge": r["challenge"], "code": "000000"})))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let done = admin
        .post("/api/auth/2fa", json!({"challenge": r["challenge"], "code": app.code().await}))
        .await;
    assert_eq!(done["recovery_codes"].as_array().unwrap().len(), 10);
    assert!(admin.signed_in().await);
    let m = next_mail(&mut mail).await;
    assert_eq!(m.to, vec!["dora@example.org"]);
    assert!(m.data.contains("turned on"));
    let sec = admin.get("/api/me/security").await;
    assert_eq!(
        (sec["two_factor"]["enabled"].clone(), sec["two_factor"]["required"].clone()),
        (json!(true), json!(true))
    );
    // It can't be switched off while required.
    let (s, _) = admin
        .call(
            Method::POST,
            "/api/me/2fa/disable",
            Some(json!({"password": "dora-password", "code": app.code().await})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // A new user enrolls at their first sign-in; Basic auth with a password is refused even
    // before that.
    admin.post("/api/admin/users", json!({"username": "eve", "password": "eve-password"})).await;
    assert_eq!(dav_propfind(&srv.url, "eve", "eve-password").await.status(), StatusCode::UNAUTHORIZED);
    let eve = Browser::new(&srv.url);
    let r = password_step(&eve, "eve", "eve-password").await;
    assert_eq!(r["two_factor"], "enroll");
    let mut eve_app = Authenticator::new(r["enrollment"]["secret"].as_str().unwrap());
    eve.post("/api/auth/2fa", json!({"challenge": r["challenge"], "code": eve_app.code().await}))
        .await;
    assert!(eve.signed_in().await);
    // Next time it's the ordinary code step.
    let again = Browser::new(&srv.url);
    let r = password_step(&again, "eve", "eve-password").await;
    assert_eq!(r["two_factor"], "totp");
    again
        .post("/api/auth/2fa", json!({"challenge": r["challenge"], "code": eve_app.code().await}))
        .await;
    assert!(again.signed_in().await);
}

// ---- OpenID Connect against a mock identity provider ---------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Tamper {
    None,
    Nonce,
    Audience,
    Expired,
    State,
}

enum Key {
    Rsa(RsaKeyPair),
    Ec(EcdsaKeyPair),
}

struct Pending {
    nonce: String,
    challenge: String,
    redirect_uri: String,
}

struct Idp {
    issuer: String,
    key: Key,
    pending: Mutex<HashMap<String, Pending>>,
    /// Claims about the person who "signs in" next.
    person: Mutex<Value>,
    tamper: Mutex<Tamper>,
}

struct MockIdp(Arc<Idp>);

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

const CLIENT_ID: &str = "ferrite-client";
const CLIENT_SECRET: &str = "s3cret/with:odd chars";

impl MockIdp {
    async fn start(es256: bool) -> MockIdp {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let key = if es256 {
            Key::Ec(EcdsaKeyPair::generate(&signature::ECDSA_P256_SHA256_FIXED_SIGNING).unwrap())
        } else {
            Key::Rsa(RsaKeyPair::generate(KeySize::Rsa2048).unwrap())
        };
        let idp = Arc::new(Idp {
            issuer,
            key,
            pending: Mutex::new(HashMap::new()),
            person: Mutex::new(json!({})),
            tamper: Mutex::new(Tamper::None),
        });
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .route("/authorize", get(authorize))
            .route("/token", post(token))
            .with_state(idp.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        MockIdp(idp)
    }
    fn provider(&self, id: &str, f: impl FnOnce(&mut OidcProvider)) -> OidcProvider {
        let mut p = OidcProvider {
            id: id.into(),
            display_name: format!("{id} login"),
            issuer: self.0.issuer.clone(),
            client_id: CLIENT_ID.into(),
            client_secret: CLIENT_SECRET.into(),
            scopes: "openid email profile".into(),
            username_claim: "preferred_username".into(),
            auto_create: false,
            admin_group: None,
            link_by_email: false,
        };
        f(&mut p);
        p
    }
    fn next(&self, person: Value, tamper: Tamper) {
        *self.0.person.lock().unwrap() = person;
        *self.0.tamper.lock().unwrap() = tamper;
    }
}

async fn discovery(State(idp): State<Arc<Idp>>) -> Json<Value> {
    let i = &idp.issuer;
    Json(json!({
        "issuer": i,
        "authorization_endpoint": format!("{i}/authorize"),
        "token_endpoint": format!("{i}/token"),
        "jwks_uri": format!("{i}/jwks"),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256", "ES256"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["client_secret_basic"],
    }))
}

async fn jwks(State(idp): State<Arc<Idp>>) -> Json<Value> {
    let key = match &idp.key {
        Key::Rsa(kp) => {
            let c = RsaPublicKeyComponents::<Vec<u8>>::from(kp.public_key());
            json!({"kty": "RSA", "kid": "rsa-1", "use": "sig", "alg": "RS256", "n": b64(&c.n), "e": b64(&c.e)})
        }
        Key::Ec(kp) => {
            let p = kp.public_key().as_ref();
            json!({"kty": "EC", "kid": "ec-1", "use": "sig", "crv": "P-256", "x": b64(&p[1..33]), "y": b64(&p[33..65])})
        }
    };
    Json(json!({"keys": [key]}))
}

async fn authorize(State(idp): State<Arc<Idp>>, Query(q): Query<HashMap<String, String>>) -> Response {
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["client_id"], CLIENT_ID);
    assert_eq!(q["code_challenge_method"], "S256");
    assert!(q["scope"].split(' ').any(|s| s == "openid"));
    let code = ferrite_core::util::random_token(16);
    idp.pending.lock().unwrap().insert(
        code.clone(),
        Pending {
            nonce: q["nonce"].clone(),
            challenge: q["code_challenge"].clone(),
            redirect_uri: q["redirect_uri"].clone(),
        },
    );
    let state = if *idp.tamper.lock().unwrap() == Tamper::State {
        "forged-state".to_string()
    } else {
        q["state"].clone()
    };
    let enc = |s: &str| percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string();
    let location = format!("{}?code={}&state={}", q["redirect_uri"], enc(&code), enc(&state));
    (AxStatus::SEE_OTHER, [(header::LOCATION, location)]).into_response()
}

async fn token(State(idp): State<Arc<Idp>>, headers: HeaderMap, Form(f): Form<HashMap<String, String>>) -> Response {
    let bad = |e: &str| (AxStatus::BAD_REQUEST, Json(json!({"error": e}))).into_response();
    // client_secret_basic: both parts form-encoded, then Basic.
    let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default();
    let raw = base64::engine::general_purpose::STANDARD
        .decode(auth.trim_start_matches("Basic "))
        .unwrap_or_default();
    let raw = String::from_utf8(raw).unwrap_or_default();
    let (id, secret) = raw.split_once(':').unwrap_or_default();
    let dec = |s: &str| percent_encoding::percent_decode_str(&s.replace('+', " ")).decode_utf8_lossy().to_string();
    if dec(id) != CLIENT_ID || dec(secret) != CLIENT_SECRET {
        return (AxStatus::UNAUTHORIZED, Json(json!({"error": "invalid_client"}))).into_response();
    }
    if f.get("grant_type").map(String::as_str) != Some("authorization_code") {
        return bad("unsupported_grant_type");
    }
    let Some(p) = idp.pending.lock().unwrap().remove(f.get("code").map(String::as_str).unwrap_or_default()) else {
        return bad("invalid_grant");
    };
    if f.get("redirect_uri") != Some(&p.redirect_uri) {
        return bad("invalid_grant");
    }
    // PKCE (RFC 7636 §4.6): S256 of the verifier must equal the challenge.
    if ferrite_server::oidc::pkce_challenge(f.get("code_verifier").map(String::as_str).unwrap_or_default()) != p.challenge {
        return bad("invalid_grant");
    }
    let tamper = *idp.tamper.lock().unwrap();
    let now = chrono::Utc::now().timestamp();
    let mut claims = idp.person.lock().unwrap().clone();
    claims["iss"] = json!(idp.issuer);
    claims["aud"] = json!(if tamper == Tamper::Audience { "someone-else" } else { CLIENT_ID });
    claims["nonce"] = json!(if tamper == Tamper::Nonce { "another-nonce".to_string() } else { p.nonce });
    claims["iat"] = json!(if tamper == Tamper::Expired { now - 400 } else { now });
    claims["exp"] = json!(if tamper == Tamper::Expired { now - 100 } else { now + 300 });
    let (alg, kid) = match idp.key {
        Key::Rsa(_) => ("RS256", "rsa-1"),
        Key::Ec(_) => ("ES256", "ec-1"),
    };
    let input = format!(
        "{}.{}",
        b64(json!({"alg": alg, "kid": kid, "typ": "JWT"}).to_string().as_bytes()),
        b64(claims.to_string().as_bytes())
    );
    let sig = match &idp.key {
        Key::Rsa(kp) => {
            let mut sig = vec![0u8; kp.public_modulus_len()];
            kp.sign(&signature::RSA_PKCS1_SHA256, &SystemRandom::new(), input.as_bytes(), &mut sig).unwrap();
            sig
        }
        Key::Ec(kp) => kp.sign(&SystemRandom::new(), input.as_bytes()).unwrap().as_ref().to_vec(),
    };
    Json(json!({"access_token": "at", "token_type": "Bearer", "expires_in": 300, "id_token": format!("{input}.{}", b64(&sig))})).into_response()
}

/// Runs the whole browser round trip (start → IdP → callback → app) and returns the page
/// it ends on.
async fn sso(b: &Browser, provider: &str, return_to: &str) -> reqwest::Url {
    let r = b
        .http
        .get(format!("{}/api/auth/oidc/{provider}/start?return_to={return_to}", b.url))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    r.url().clone()
}

fn sso_error(u: &reqwest::Url) -> Option<String> {
    u.query_pairs().find(|(k, _)| k == "sso_error").map(|(_, v)| v.to_string())
}

#[tokio::test]
async fn oidc_single_sign_on() {
    let corp = MockIdp::start(false).await;
    let partner = MockIdp::start(true).await;
    let providers = vec![
        corp.provider("corp", |p| {
            p.auto_create = true;
            p.admin_group = Some("ferrite-admins".into());
        }),
        partner.provider("partner", |p| p.link_by_email = true),
    ];
    let srv = start(move |c| c.oidc = providers).await;
    let admin = Browser::new(&srv.url);
    admin.post("/api/setup", json!({"username": "alice", "password": "alice-password"})).await;
    admin
        .post(
            "/api/admin/users",
            json!({"username": "bob", "password": "bob-password", "email": "Bob@Example.org"}),
        )
        .await;

    // Providers are listed for the sign-in page.
    let status = Browser::new(&srv.url).get("/api/status").await;
    assert_eq!(
        status["sso"],
        json!([{"id": "corp", "name": "corp login"}, {"id": "partner", "name": "partner login"}])
    );

    // First sign-in creates the account (with admin rights from the group), then the same
    // subject maps to the same user.
    let ada = json!({"sub": "corp-ada", "preferred_username": "ada", "name": "Ada Lovelace", "email": "ada@example.org",
                     "email_verified": true, "groups": ["staff", "ferrite-admins"]});
    corp.next(ada.clone(), Tamper::None);
    let b = Browser::new(&srv.url);
    let end = sso(&b, "corp", "/files").await;
    assert_eq!((end.path(), sso_error(&end)), ("/files", None));
    let me = b.get("/api/me").await;
    assert_eq!(
        (me["user"]["username"].as_str(), me["user"]["display_name"].as_str()),
        (Some("ada"), Some("Ada Lovelace"))
    );
    assert_eq!(me["user"]["is_admin"], true);
    let ada_id = me["user"]["id"].clone();
    let sec = b.get("/api/me/security").await;
    assert_eq!(sec["identities"][0]["provider"], "corp");
    assert_eq!(sec["identities"][0]["provider_name"], "corp login");
    assert_eq!(sec["identities"][0]["subject"], "corp-ada");

    // SSO users skip Ferrite's own 2FA (the provider handles it); leaving the admin group
    // removes the rights from an account the provider created.
    enroll(&b).await;
    let mut ada_again = ada.clone();
    ada_again["groups"] = json!(["staff"]);
    corp.next(ada_again, Tamper::None);
    let b2 = Browser::new(&srv.url);
    let end = sso(&b2, "corp", "/").await;
    assert_eq!(sso_error(&end), None);
    let me = b2.get("/api/me").await;
    assert_eq!(me["user"]["id"], ada_id);
    assert_eq!(me["user"]["is_admin"], false);

    // Every validation failure ends on the sign-in page with a reason, and no session.
    for (tamper, needle) in [
        (Tamper::Nonce, "nonce"),
        (Tamper::Audience, "another client"),
        (Tamper::Expired, "expired"),
        (Tamper::State, "expired or was already used"),
    ] {
        corp.next(ada.clone(), tamper);
        let b = Browser::new(&srv.url);
        let end = sso(&b, "corp", "/").await;
        let err = sso_error(&end).unwrap_or_default();
        assert!(err.contains(needle), "{needle}: got {err:?}");
        assert!(!b.signed_in().await);
    }

    // The callback must come back to the browser that started the sign-in.
    corp.next(ada.clone(), Tamper::None);
    let no_redirects = || reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let starter = no_redirects();
    let r = starter.get(format!("{}/api/auth/oidc/corp/start", srv.url)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::SEE_OTHER);
    let set_cookie = r.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Lax"));
    let to_idp = r.headers()["location"].to_str().unwrap().to_string();
    assert!(to_idp.contains("code_challenge_method=S256") && to_idp.contains("nonce=") && to_idp.contains("state="));
    let r = no_redirects().get(&to_idp).send().await.unwrap();
    let callback = r.headers()["location"].to_str().unwrap().to_string();
    let r = no_redirects().get(&callback).send().await.unwrap();
    let back = r.headers()["location"].to_str().unwrap().to_string();
    assert!(back.contains("sso_error=") && back.contains("another%20browser"), "{back}");
    // …and a state is usable once.
    let r = starter.get(&callback).send().await.unwrap();
    assert!(r.headers()["location"].to_str().unwrap().contains("already%20used"));

    // Without auto_create, an unknown identity gets a clear error.
    partner.next(
        json!({"sub": "p-zed", "preferred_username": "zed", "email": "zed@example.org", "email_verified": true}),
        Tamper::None,
    );
    let b = Browser::new(&srv.url);
    let err = sso_error(&sso(&b, "partner", "/").await).unwrap_or_default();
    assert!(err.contains("no Ferrite account is linked"), "{err}");
    assert!(srv.core.user_by_name("zed").await.is_err());

    // link_by_email only trusts verified addresses (ES256 tokens from this provider).
    partner.next(json!({"sub": "p-bob", "email": "bob@example.org", "email_verified": false}), Tamper::None);
    let err = sso_error(&sso(&Browser::new(&srv.url), "partner", "/").await).unwrap_or_default();
    assert!(err.contains("no Ferrite account is linked"), "{err}");
    partner.next(json!({"sub": "p-bob", "email": "bob@example.org", "email_verified": "true"}), Tamper::None);
    let b = Browser::new(&srv.url);
    assert_eq!(sso_error(&sso(&b, "partner", "/").await), None);
    assert_eq!(b.get("/api/me").await["user"]["username"], "bob");
    // Linked now: later sign-ins match the subject even when the email changes.
    partner.next(json!({"sub": "p-bob", "email": "robert@example.org", "email_verified": true}), Tamper::None);
    let b = Browser::new(&srv.url);
    assert_eq!(sso_error(&sso(&b, "partner", "/").await), None);
    assert_eq!(b.get("/api/me").await["user"]["username"], "bob");
    let ids = b.get("/api/me/security").await["identities"].clone();
    assert_eq!(ids.as_array().unwrap().len(), 1);
    assert_eq!(ids[0]["provider"], "partner");

    // Auto-create refuses to take over an existing username.
    corp.next(json!({"sub": "corp-imposter", "preferred_username": "alice"}), Tamper::None);
    let err = sso_error(&sso(&Browser::new(&srv.url), "corp", "/").await).unwrap_or_default();
    assert!(err.contains("already taken"), "{err}");
}
