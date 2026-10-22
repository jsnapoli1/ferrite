//! Web Push end to end: a real server delivers to a mock push service (axum); the test
//! decrypts each push with the subscription's private key, as a browser would.

use aws_lc_rs::agreement;
use aws_lc_rs::encoding::{AsBigEndian, EcPrivateKeyBin};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode as AxStatus};
use axum::response::IntoResponse;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ferrite_core::{Config, Core};
use ferrite_server::AppState;
use ferrite_server::push::crypto;
use futures::SinkExt;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

struct Server {
    url: String,
    _core: Core,
    _dir: tempfile::TempDir,
}

async fn start(allow_insecure: bool) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let mut cfg = Config {
        data_dir: dir.path().to_path_buf(),
        public_url: url.clone(),
        ..Config::default()
    };
    cfg.sfu.enabled = false;
    cfg.push.allow_insecure_endpoints = allow_insecure;
    let core = Core::open(cfg).await.unwrap();
    let state = AppState::new(core.clone());
    state.start_background();
    let app = ferrite_server::router(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server { url, _core: core, _dir: dir }
}

struct Browser {
    http: reqwest::Client,
    url: String,
}

impl Browser {
    async fn login(url: &str, username: &str, password: &str) -> Self {
        let b = Browser {
            http: reqwest::Client::builder()
                .cookie_store(true)
                .user_agent("Mozilla/5.0 (Android 14; Mobile; rv:131.0) Gecko/131.0 Firefox/131.0")
                .build()
                .unwrap(),
            url: url.to_string(),
        };
        b.post("/api/auth/login", json!({"username": username, "password": password})).await;
        b
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
    async fn put(&self, path: &str, body: Value) -> Value {
        let (s, v) = self.call(Method::PUT, path, Some(body)).await;
        assert!(s.is_success(), "PUT {path}: {s} {v}");
        v
    }
    async fn upload(&self, folder: &str, name: &str, body: &str) -> String {
        let r = self
            .http
            .put(format!("{}/api/files/{folder}/upload?name={name}", self.url))
            .header("X-Ferrite-Request", "1")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        r.json::<Value>().await.unwrap()["node"]["id"].as_str().unwrap().to_string()
    }
}

/// What the mock push service received.
#[derive(Debug, Clone)]
struct Received {
    device: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

#[derive(Clone)]
struct Mock {
    tx: mpsc::UnboundedSender<Received>,
    /// How many requests each device has seen (drives the flaky device).
    hits: Arc<Mutex<HashMap<String, u32>>>,
}

/// RFC 8030 push resources at /push/{device}. Devices named `gone-*` answer 410; `flaky-*`
/// answer 503 (Retry-After: 0) the first time.
async fn push_resource(State(m): State<Mock>, Path(device): Path<String>, headers: HeaderMap, body: axum::body::Bytes) -> impl IntoResponse {
    let n = {
        let mut hits = m.hits.lock().unwrap();
        let n = hits.entry(device.clone()).or_default();
        *n += 1;
        *n
    };
    let headers = headers
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let _ = m.tx.send(Received {
        device: device.clone(),
        headers,
        body: body.to_vec(),
    });
    if device.starts_with("gone") {
        return (AxStatus::GONE, [("retry-after", "0")]).into_response();
    }
    if device.starts_with("flaky") && n == 1 {
        return (AxStatus::SERVICE_UNAVAILABLE, [("retry-after", "0")]).into_response();
    }
    (AxStatus::CREATED, [("location", format!("/m/{device}"))]).into_response()
}

async fn mock_push_service() -> (String, mpsc::UnboundedReceiver<Received>, Arc<Mutex<HashMap<String, u32>>>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let hits = Arc::new(Mutex::new(HashMap::new()));
    let app = axum::Router::new()
        .route("/push/{device}", axum::routing::post(push_resource))
        .with_state(Mock { tx, hits: hits.clone() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, rx, hits)
}

/// A browser's push subscription: its P-256 key pair and auth secret.
struct Device {
    endpoint: String,
    private: Vec<u8>,
    public: Vec<u8>,
    auth: [u8; 16],
}

impl Device {
    fn new(endpoint: String) -> Self {
        let key = agreement::PrivateKey::generate(&agreement::ECDH_P256).unwrap();
        let public = key.compute_public_key().unwrap().as_ref().to_vec();
        let bin: EcPrivateKeyBin = key.as_be_bytes().unwrap();
        let mut auth = [0u8; 16];
        aws_lc_rs::rand::fill(&mut auth).unwrap();
        Device {
            endpoint,
            private: bin.as_ref().to_vec(),
            public,
            auth,
        }
    }
    fn json(&self) -> Value {
        json!({"endpoint": self.endpoint, "expirationTime": null, "keys": {"p256dh": URL_SAFE_NO_PAD.encode(&self.public), "auth": URL_SAFE_NO_PAD.encode(self.auth)}})
    }
    fn open(&self, r: &Received) -> Value {
        assert_eq!(r.headers["content-encoding"], "aes128gcm");
        assert!(r.body.len() <= 4096);
        serde_json::from_slice(&crypto::decrypt(&r.body, &self.private, &self.auth).unwrap()).unwrap()
    }
}

async fn next_push(rx: &mut mpsc::UnboundedReceiver<Received>) -> Received {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("a push within 10s")
        .unwrap()
}

async fn assert_no_push(rx: &mut mpsc::UnboundedReceiver<Received>) {
    if let Ok(Some(r)) = tokio::time::timeout(Duration::from_millis(1500), rx.recv()).await {
        panic!("unexpected push to {}", r.device);
    }
}

/// Checks the VAPID header like a push service: the JWT verifies with `k`, which is the
/// server's published key, `aud` is the endpoint's origin and `exp` is within 24 hours.
fn check_vapid(r: &Received, public_key: &str, origin: &str) {
    let auth = &r.headers["authorization"];
    let rest = auth.strip_prefix("vapid t=").expect("vapid scheme");
    let (token, k) = rest.split_once(", k=").unwrap();
    assert_eq!(k, public_key);
    let claims = crypto::verify_jwt(token, &URL_SAFE_NO_PAD.decode(k).unwrap()).unwrap();
    assert_eq!(claims["aud"], origin);
    let exp = claims["exp"].as_i64().unwrap();
    let now = chrono::Utc::now().timestamp();
    assert!(exp > now && exp <= now + 24 * 3600, "exp within 24h");
    assert!(claims["sub"].as_str().unwrap().starts_with("mailto:"));
}

#[tokio::test]
async fn pushes_mentions_shares_and_calls_to_devices() {
    let srv = start(true).await;
    let (mock, mut rx, hits) = mock_push_service().await;
    let setup = Browser {
        http: reqwest::Client::builder().cookie_store(true).build().unwrap(),
        url: srv.url.clone(),
    };
    setup
        .post(
            "/api/setup",
            json!({"username": "alice", "password": "alice-password", "display_name": "Alice"}),
        )
        .await;
    setup
        .post(
            "/api/admin/users",
            json!({"username": "bob", "password": "bob-password", "display_name": "Bob"}),
        )
        .await;
    let alice = setup;
    let bob = Browser::login(&srv.url, "bob", "bob-password").await;

    // The server's VAPID key, and bob's phone subscribing.
    let overview = bob.get("/api/push").await;
    let public_key = overview["public_key"].as_str().unwrap().to_string();
    assert_eq!(URL_SAFE_NO_PAD.decode(&public_key).unwrap().len(), 65);
    assert_eq!(overview["subscriptions"], json!([]));
    assert_eq!(overview["settings"]["enabled"], true);
    let phone = Device::new(format!("{mock}/push/bob-phone"));
    let sub = bob.post("/api/push/subscriptions", phone.json()).await["subscription"].clone();
    assert_eq!(sub["label"], "Firefox on Android");
    assert_eq!(sub["service"], "127.0.0.1");
    // Re-subscribing the same endpoint updates it instead of adding a device.
    assert_eq!(bob.post("/api/push/subscriptions", phone.json()).await["subscription"]["id"], sub["id"]);
    assert_eq!(bob.get("/api/push").await["subscriptions"].as_array().unwrap().len(), 1);
    // Bad keys are refused.
    let mut bad = phone.json();
    bad["keys"]["auth"] = json!("c2hvcnQ");
    assert_eq!(bob.call(Method::POST, "/api/push/subscriptions", Some(bad)).await.0, StatusCode::BAD_REQUEST);

    // A mention while bob is offline.
    let room = alice.post("/api/talk/rooms", json!({"name": "Design", "invite": ["bob"]})).await["room"]["token"]
        .as_str()
        .unwrap()
        .to_string();
    alice
        .post(&format!("/api/talk/rooms/{room}/messages"), json!({"body": "Can you check this @bob?"}))
        .await;
    let r = next_push(&mut rx).await;
    assert_eq!(r.device, "bob-phone");
    check_vapid(&r, &public_key, &mock);
    assert_eq!(r.headers["ttl"], "86400");
    assert_eq!(r.headers["urgency"], "normal");
    let topic = &r.headers["topic"];
    assert_eq!(topic.len(), 32);
    assert!(topic.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    let p = phone.open(&r);
    assert_eq!(p["kind"], "notification");
    assert_eq!(p["title"], "Alice mentioned you in “Design”");
    assert_eq!(p["body"], "Can you check this @bob?");
    assert_eq!(p["url"], format!("/talk/{room}"));
    assert_eq!(p["tag"], format!("chat:/talk/{room}"));
    assert_eq!(p["actions"], json!([]));
    let nid = p["notification_id"].as_i64().unwrap();
    assert_eq!(bob.get("/api/notifications").await["notifications"][0]["id"], nid);
    // Nothing beyond what the notification shows.
    let mut keys: Vec<&str> = p.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(keys, ["actions", "body", "kind", "notification_id", "tag", "title", "ts", "url"]);

    // While bob is online (a tab open on his laptop) a mention stores no notification (he sees
    // it live), but his phone still gets the push.
    let token = bob.post("/api/me/tokens", json!({"name": "laptop"})).await["secret"]
        .as_str()
        .unwrap()
        .to_string();
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("{}/api/ws?token={token}", srv.url.replace("http://", "ws://")))
        .await
        .unwrap();
    use futures::StreamExt;
    while let Some(Ok(m)) = ws.next().await {
        if matches!(&m, WsMessage::Text(t) if t.contains("\"hello\"")) {
            break;
        }
    }
    let stored = bob.get("/api/notifications").await["notifications"].as_array().unwrap().len();
    alice
        .post(&format!("/api/talk/rooms/{room}/messages"), json!({"body": "@bob one more thing"}))
        .await;
    let p = phone.open(&next_push(&mut rx).await);
    assert_eq!(p["title"], "Alice mentioned you in “Design”");
    assert_eq!(p["tag"], format!("chat:/talk/{room}"));
    assert_eq!(p["notification_id"], Value::Null);
    assert_eq!(bob.get("/api/notifications").await["notifications"].as_array().unwrap().len(), stored);
    ws.close(None).await.unwrap();
    drop(ws);

    // A share.
    let root = alice.get("/api/me").await["root_id"].as_str().unwrap().to_string();
    let report = alice.upload(&root, "report.txt", "numbers").await;
    alice
        .post(&format!("/api/files/{report}/shares"), json!({"kind": "user", "target": "bob"}))
        .await;
    let r = next_push(&mut rx).await;
    assert!(!r.headers.contains_key("topic"), "shares don't collapse");
    let p = phone.open(&r);
    assert_eq!(p["title"], "Alice shared “report.txt” with you");
    assert_eq!(p["url"], format!("/files?node={report}"));
    assert!(p["tag"].as_str().unwrap().starts_with("n:"));

    // An incoming call: alice starts one; bob (in the room, not in the call) is rung.
    let atok = alice.post("/api/me/tokens", json!({"name": "ws"})).await["secret"]
        .as_str()
        .unwrap()
        .to_string();
    let (mut aws, _) = tokio_tungstenite::connect_async(format!("{}/api/ws?token={atok}", srv.url.replace("http://", "ws://")))
        .await
        .unwrap();
    aws.send(WsMessage::Text(json!({"type": "call.join", "room": room, "audio": true}).to_string().into()))
        .await
        .unwrap();
    let r = next_push(&mut rx).await;
    assert_eq!(r.headers["urgency"], "high");
    assert_eq!(r.headers["ttl"], "60");
    check_vapid(&r, &public_key, &mock);
    let p = phone.open(&r);
    assert_eq!(p["kind"], "call");
    assert_eq!(p["title"], "Alice is calling in Design");
    assert_eq!(p["url"], format!("/talk/{room}"));
    assert_eq!(p["tag"], format!("call:{room}"));
    assert_eq!(
        p["actions"],
        json!([{"action": "join", "title": "Join", "url": format!("/talk/{room}?call=1")}, {"action": "dismiss", "title": "Dismiss"}])
    );
    assert!(p["notification_id"].is_null());
    drop(aws);

    // Categories: shares off → no push; chat still on.
    let s = bob
        .put(
            "/api/push/settings",
            json!({"enabled": true, "chat": true, "calls": true, "calendar": true, "files": false, "boards": true}),
        )
        .await;
    assert_eq!(s["settings"]["files"], false);
    let second = alice.upload(&root, "second.txt", "more").await;
    alice
        .post(&format!("/api/files/{second}/shares"), json!({"kind": "user", "target": "bob"}))
        .await;
    assert_no_push(&mut rx).await;
    alice.post(&format!("/api/talk/rooms/{room}/messages"), json!({"body": "@bob ping"})).await;
    assert_eq!(phone.open(&next_push(&mut rx).await)["body"], "@bob ping");
    // Calls off: starting a call rings nobody.
    bob.put(
        "/api/push/settings",
        json!({"enabled": true, "chat": true, "calls": false, "calendar": true, "files": true, "boards": true}),
    )
    .await;
    let (mut aws, _) = tokio_tungstenite::connect_async(format!("{}/api/ws?token={atok}", srv.url.replace("http://", "ws://")))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    aws.send(WsMessage::Text(json!({"type": "call.join", "room": room, "audio": true}).to_string().into()))
        .await
        .unwrap();
    assert_no_push(&mut rx).await;
    drop(aws);
    // …and with calls back on, the next call rings again (so the silence above was the setting).
    bob.put(
        "/api/push/settings",
        json!({"enabled": true, "chat": true, "calls": true, "calendar": true, "files": true, "boards": true}),
    )
    .await;
    let (mut aws, _) = tokio_tungstenite::connect_async(format!("{}/api/ws?token={atok}", srv.url.replace("http://", "ws://")))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    aws.send(WsMessage::Text(json!({"type": "call.join", "room": room, "audio": true}).to_string().into()))
        .await
        .unwrap();
    assert_eq!(phone.open(&next_push(&mut rx).await)["kind"], "call");
    drop(aws);
    // Master switch off: nothing at all, not even a test.
    bob.put(
        "/api/push/settings",
        json!({"enabled": false, "chat": true, "calls": true, "calendar": true, "files": true, "boards": true}),
    )
    .await;
    alice
        .post(&format!("/api/talk/rooms/{room}/messages"), json!({"body": "@bob are you there"}))
        .await;
    assert_no_push(&mut rx).await;
    assert_eq!(bob.call(Method::POST, "/api/push/test", None).await.0, StatusCode::BAD_REQUEST);
    bob.put(
        "/api/push/settings",
        json!({"enabled": true, "chat": true, "calls": true, "calendar": true, "files": true, "boards": true}),
    )
    .await;

    // Retries: a 503 is retried and then delivered.
    let tablet = Device::new(format!("{mock}/push/flaky-tablet"));
    bob.post("/api/push/subscriptions", tablet.json()).await;
    alice.post(&format!("/api/talk/rooms/{room}/messages"), json!({"body": "@bob retry"})).await;
    let mut seen = Vec::new();
    for _ in 0..3 {
        seen.push(next_push(&mut rx).await);
    }
    assert_eq!(seen.iter().filter(|r| r.device == "flaky-tablet").count(), 2, "one retry after 503");
    assert_eq!(seen.iter().filter(|r| r.device == "bob-phone").count(), 1);
    let last = seen.iter().rev().find(|r| r.device == "flaky-tablet").unwrap();
    assert_eq!(tablet.open(last)["body"], "@bob retry");
    assert_eq!(hits.lock().unwrap()["flaky-tablet"], 2);

    // 410 Gone removes the device; the test push reports per device.
    let old = Device::new(format!("{mock}/push/gone-laptop"));
    bob.post("/api/push/subscriptions", old.json()).await;
    assert_eq!(bob.get("/api/push").await["subscriptions"].as_array().unwrap().len(), 3);
    let t = bob.post("/api/push/test", json!({})).await;
    assert_eq!(t["delivered"], 2);
    let results: Vec<&str> = t["results"].as_array().unwrap().iter().map(|x| x["status"].as_str().unwrap()).collect();
    assert_eq!(results.iter().filter(|s| **s == "removed").count(), 1);
    for _ in 0..3 {
        let r = next_push(&mut rx).await;
        if r.device == "bob-phone" {
            let p = phone.open(&r);
            assert_eq!(p["kind"], "test");
            assert_eq!(p["title"], "Ferrite test notification");
            assert_eq!(r.headers["urgency"], "high");
        }
    }
    let devices = bob.get("/api/push").await["subscriptions"].clone();
    assert_eq!(devices.as_array().unwrap().len(), 2);
    assert!(devices.as_array().unwrap().iter().all(|d| d["endpoint"] != old.endpoint.as_str()));

    // Removing a device; other users can't remove it.
    let phone_id = devices.as_array().unwrap().iter().find(|d| d["endpoint"] == phone.endpoint.as_str()).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        alice.call(Method::DELETE, &format!("/api/push/subscriptions/{phone_id}"), None).await.0,
        StatusCode::NOT_FOUND
    );
    bob.call(Method::DELETE, &format!("/api/push/subscriptions/{phone_id}"), None).await;
    assert_eq!(bob.get("/api/push").await["subscriptions"].as_array().unwrap().len(), 1);

    // An endpoint belongs to whoever subscribed it last (shared browser, another account).
    alice.post("/api/push/subscriptions", tablet.json()).await;
    assert_eq!(bob.get("/api/push").await["subscriptions"], json!([]));
    assert_eq!(alice.get("/api/push").await["subscriptions"].as_array().unwrap().len(), 1);

    // Only sessions and full-access tokens manage push.
    let files_token = bob.post("/api/me/tokens", json!({"name": "sync", "scopes": ["files"]})).await["secret"]
        .as_str()
        .unwrap()
        .to_string();
    let r = reqwest::Client::new()
        .get(format!("{}/api/push", srv.url))
        .bearer_auth(files_token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn refuses_private_and_insecure_endpoints() {
    let srv = start(false).await;
    let admin = Browser {
        http: reqwest::Client::builder().cookie_store(true).build().unwrap(),
        url: srv.url.clone(),
    };
    admin.post("/api/setup", json!({"username": "alice", "password": "alice-password"})).await;
    let (mock, mut rx, _) = mock_push_service().await;
    for endpoint in [
        format!("{mock}/push/x"),
        mock.replace("http://", "https://") + "/push/x",
        "https://localhost/push/x".to_string(),
        "https://10.0.0.7/push/x".to_string(),
        "https://192.168.1.20/push/x".to_string(),
        "https://169.254.169.254/latest/meta-data".to_string(),
        "https://[::1]/push/x".to_string(),
        "https://[fd00::1]/push/x".to_string(),
        "https://[::ffff:127.0.0.1]/push/x".to_string(),
        "https://user:pw@93.184.215.14/push/x".to_string(),
        "file:///etc/passwd".to_string(),
    ] {
        let d = Device::new(endpoint.clone());
        let (s, v) = admin.call(Method::POST, "/api/push/subscriptions", Some(d.json())).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{endpoint} must be refused: {v}");
    }
    assert_eq!(admin.get("/api/push").await["subscriptions"], json!([]));
    assert_no_push(&mut rx).await;
}
