//! End-to-end tests: the real router on a real socket, driven over HTTP, WebDAV,
//! WebSockets and MCP.

use base64::Engine;
use ferrite_core::{Config, Core};
use ferrite_server::AppState;
use futures::{SinkExt, StreamExt};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{GetString, ReadTxn, Text, Transact};

struct Server {
    url: String,
    core: Core,
    state: AppState,
    _dir: tempfile::TempDir,
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
    // Tests run in parallel: give each server its own ephemeral SFU port.
    cfg.sfu.udp_bind = "127.0.0.1:0".into();
    tweak(&mut cfg);
    let core = Core::open(cfg).await.unwrap();
    let state = AppState::new(core.clone());
    state.start_background();
    let app = ferrite_server::router(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server { url, core, state, _dir: dir }
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
        let v = resp.json::<Value>().await.unwrap_or(Value::Null);
        (status, v)
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
    async fn login(url: &str, username: &str, password: &str) -> Self {
        let b = Browser::new(url);
        b.post("/api/auth/login", json!({"username": username, "password": password})).await;
        b
    }
}

async fn admin_and_user(url: &str) -> (Browser, Browser, Value) {
    let admin = Browser::new(url);
    let setup = admin
        .post(
            "/api/setup",
            json!({"username": "alice", "password": "alice-password", "display_name": "Alice"}),
        )
        .await;
    admin
        .post(
            "/api/admin/users",
            json!({"username": "bob", "password": "bob-password", "display_name": "Bob"}),
        )
        .await;
    let bob = Browser::login(url, "bob", "bob-password").await;
    (admin, bob, setup["user"].clone())
}

#[tokio::test]
async fn setup_login_csrf_and_tokens() {
    let srv = start(|_| {}).await;
    let b = Browser::new(&srv.url);
    assert_eq!(b.get("/api/status").await["setup_needed"], true);
    let (admin, _bob, user) = admin_and_user(&srv.url).await;
    assert_eq!(user["is_admin"], true);
    assert_eq!(b.get("/api/status").await["setup_needed"], false);
    // A second setup is refused.
    let (s, _) = b
        .call(Method::POST, "/api/setup", Some(json!({"username": "eve", "password": "eve-password"})))
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // Cookie-authenticated writes without the CSRF header are refused.
    let r = admin
        .http
        .post(format!("{}/api/talk/rooms", srv.url))
        .json(&json!({"name": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let r = admin
        .http
        .post(format!("{}/api/me/tokens", srv.url))
        .json(&json!({"name": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    // Wrong passwords are throttled after repeated failures.
    for _ in 0..8 {
        let (s, _) = b
            .call(Method::POST, "/api/auth/login", Some(json!({"username": "bob", "password": "nope"})))
            .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }
    let (s, _) = b
        .call(Method::POST, "/api/auth/login", Some(json!({"username": "bob", "password": "bob-password"})))
        .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    // App tokens work as bearer tokens and respect scopes.
    let t = admin.post("/api/me/tokens", json!({"name": "script", "scopes": ["talk"]})).await;
    let secret = t["secret"].as_str().unwrap();
    let http = reqwest::Client::new();
    let r = http.get(format!("{}/api/talk/rooms", srv.url)).bearer_auth(secret).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let root = admin.get("/api/me").await["root_id"].as_str().unwrap().to_string();
    let r = http
        .get(format!("{}/api/files/{root}/children", srv.url))
        .bearer_auth(secret)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    // Logout ends the session.
    admin.post("/api/auth/logout", json!({})).await;
    let (s, _) = admin.call(Method::GET, "/api/me", None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    drop(srv.core);
}

#[tokio::test]
async fn files_upload_download_share_link() {
    let srv = start(|_| {}).await;
    let (alice, bob, _) = admin_and_user(&srv.url).await;
    let root = alice.get("/api/me").await["root_id"].as_str().unwrap().to_string();
    let folder = alice.post(&format!("/api/files/{root}/mkdir"), json!({"name": "Project"})).await["node"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    // Streaming upload.
    let body = "x".repeat(300_000);
    let r = alice
        .http
        .put(format!("{}/api/files/{folder}/upload?name=data.txt", srv.url))
        .header("X-Ferrite-Request", "1")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let file = r.json::<Value>().await.unwrap()["node"].clone();
    let fid = file["id"].as_str().unwrap();
    assert_eq!(file["size"], 300_000);
    // Range download.
    let r = alice
        .http
        .get(format!("{}/api/files/{fid}/download", srv.url))
        .header("Range", "bytes=10-19")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(r.text().await.unwrap(), "xxxxxxxxxx");
    // HTML is never rendered inline from our origin.
    alice
        .http
        .put(format!("{}/api/files/{folder}/upload?name=x.html", srv.url))
        .header("X-Ferrite-Request", "1")
        .body("<script>alert(1)</script>")
        .send()
        .await
        .unwrap();
    let html_id = alice.get(&format!("/api/files/{folder}/children")).await["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "x.html")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = alice
        .http
        .get(format!("{}/api/files/{html_id}/download?inline=true", srv.url))
        .send()
        .await
        .unwrap();
    assert!(r.headers()["content-disposition"].to_str().unwrap().starts_with("attachment"));
    assert_eq!(r.headers()["content-type"], "application/octet-stream");
    // Bob can't see it until shared.
    let (s, _) = bob.call(Method::GET, &format!("/api/files/{fid}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    alice
        .post(
            &format!("/api/files/{folder}/shares"),
            json!({"kind": "user", "target": "bob", "permissions": 1}),
        )
        .await;
    let bob_root = bob.get("/api/me").await["root_id"].as_str().unwrap().to_string();
    let entries = bob.get(&format!("/api/files/{bob_root}/children")).await;
    assert!(
        entries["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "Project" && e["shared_by"] == "Alice")
    );
    let n = bob.get("/api/notifications").await;
    assert!(n["notifications"][0]["title"].as_str().unwrap().contains("shared"));
    // Public link with password.
    let link = alice
        .post(&format!("/api/files/{folder}/shares"), json!({"kind": "link", "password": "open sesame"}))
        .await;
    let token = link["share"]["token"].as_str().unwrap();
    let anon = Browser::new(&srv.url);
    let info = anon.get(&format!("/api/public/{token}")).await;
    assert_eq!(info["locked"], true);
    let (s, _) = anon.call(Method::GET, &format!("/api/public/{token}/download?path=data.txt"), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = anon
        .call(Method::POST, &format!("/api/public/{token}/unlock"), Some(json!({"password": "wrong"})))
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    anon.post(&format!("/api/public/{token}/unlock"), json!({"password": "open sesame"})).await;
    let info = anon.get(&format!("/api/public/{token}")).await;
    assert_eq!(info["entries"].as_array().unwrap().len(), 2);
    let r = anon
        .http
        .get(format!("{}/api/public/{token}/download?path=data.txt", srv.url))
        .send()
        .await
        .unwrap();
    assert_eq!(r.bytes().await.unwrap().len(), 300_000);
    // Versions and trash.
    alice
        .http
        .put(format!("{}/api/files/{folder}/upload?name=data.txt", srv.url))
        .header("X-Ferrite-Request", "1")
        .body("v2")
        .send()
        .await
        .unwrap();
    let versions = alice.get(&format!("/api/files/{fid}/versions")).await;
    assert_eq!(versions["versions"].as_array().unwrap().len(), 1);
    alice.call(Method::DELETE, &format!("/api/files/{fid}"), None).await;
    assert_eq!(alice.get("/api/trash").await["items"].as_array().unwrap().len(), 1);
    alice.post(&format!("/api/trash/{fid}/restore"), json!({})).await;
    assert_eq!(alice.get(&format!("/api/files/{fid}")).await["path"], "/Project/data.txt");
    // Search sees file contents.
    alice
        .http
        .put(format!("{}/api/files/{folder}/upload?name=notes.md", srv.url))
        .header("X-Ferrite-Request", "1")
        .body("The zebra protocol")
        .send()
        .await
        .unwrap();
    let hits = alice.get("/api/search?q=zebra").await;
    assert_eq!(hits["results"][0]["title"], "notes.md");
}

#[tokio::test]
async fn webdav_round_trip() {
    let srv = start(|_| {}).await;
    let (alice, _bob, _) = admin_and_user(&srv.url).await;
    let token = alice.post("/api/me/tokens", json!({"name": "dav", "scopes": ["files"]})).await["secret"]
        .as_str()
        .unwrap()
        .to_string();
    let http = reqwest::Client::new();
    let base = format!("{}/remote.php/dav/files/alice", srv.url);
    let auth = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("alice:{token}")));
    let dav = |m: &str, path: &str| {
        http.request(Method::from_bytes(m.as_bytes()).unwrap(), format!("{base}{path}"))
            .header("Authorization", auth.clone())
    };
    let r = http.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    assert!(r.headers().contains_key("www-authenticate"));
    assert_eq!(dav("MKCOL", "/Docs").send().await.unwrap().status(), StatusCode::CREATED);
    assert_eq!(dav("MKCOL", "/Docs").send().await.unwrap().status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(dav("MKCOL", "/Nope/Deeper").send().await.unwrap().status(), StatusCode::CONFLICT);
    assert_eq!(
        dav("PUT", "/Docs/h%C3%A9llo%20world.txt").body("hello").send().await.unwrap().status(),
        StatusCode::CREATED
    );
    assert_eq!(
        dav("PUT", "/Docs/h%C3%A9llo%20world.txt").body("hello again").send().await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        dav("PUT", "/Docs/h%C3%A9llo%20world.txt")
            .header("If-None-Match", "*")
            .body("x")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::PRECONDITION_FAILED
    );
    let r = dav("PROPFIND", "/Docs").header("Depth", "1").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::MULTI_STATUS);
    let xml = r.text().await.unwrap();
    assert!(xml.contains("<d:href>/remote.php/dav/files/alice/Docs/</d:href>"), "{xml}");
    assert!(xml.contains("/remote.php/dav/files/alice/Docs/h%C3%A9llo%20world.txt</d:href>"), "{xml}");
    assert!(xml.contains("<d:getcontentlength>11</d:getcontentlength>"));
    assert!(xml.contains("<oc:permissions>RDNVW</oc:permissions>"));
    let r = dav("PROPFIND", "/").header("Depth", "0").send().await.unwrap();
    assert!(r.text().await.unwrap().contains("<d:quota-used-bytes>11</d:quota-used-bytes>"));
    assert_eq!(
        dav("GET", "/Docs/h%C3%A9llo%20world.txt").send().await.unwrap().text().await.unwrap(),
        "hello again"
    );
    let r = dav("COPY", "/Docs/h%C3%A9llo%20world.txt")
        .header("Destination", format!("{base}/Docs/copy.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    let r = dav("MOVE", "/Docs/copy.txt")
        .header("Destination", format!("{base}/moved.txt"))
        .header("Overwrite", "F")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    let r = dav("MOVE", "/moved.txt")
        .header("Destination", format!("{base}/Docs/h%C3%A9llo%20world.txt"))
        .header("Overwrite", "F")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::PRECONDITION_FAILED);
    // Locks: a locked file rejects writes from other clients without the token.
    let r = dav("LOCK", "/moved.txt")
        .body("<?xml version=\"1.0\"?><d:lockinfo xmlns:d=\"DAV:\"/>")
        .send()
        .await
        .unwrap();
    let lock_token = r.headers()["lock-token"].to_str().unwrap().trim_matches(|c| c == '<' || c == '>').to_string();
    assert!(lock_token.starts_with("opaquelocktoken:"));
    assert_eq!(
        dav("PUT", "/moved.txt")
            .header("If", format!("(<{lock_token}>)"))
            .body("locked write")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(dav("UNLOCK", "/moved.txt").send().await.unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(dav("DELETE", "/moved.txt").send().await.unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(dav("GET", "/moved.txt").send().await.unwrap().status(), StatusCode::NOT_FOUND);
    // Other users' trees are off limits; passwords work for Basic auth too.
    let r = http
        .request(Method::from_bytes(b"PROPFIND").unwrap(), format!("{}/remote.php/dav/files/bob/", srv.url))
        .header("Authorization", auth.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let pw = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("alice:alice-password"));
    let r = http
        .get(format!("{}/remote.php/webdav/Docs/h%C3%A9llo%20world.txt", srv.url))
        .header("Authorization", pw)
        .send()
        .await
        .unwrap();
    assert_eq!(r.text().await.unwrap(), "hello again");
    assert_eq!(
        http.get(format!("{}/status.php", srv.url)).send().await.unwrap().json::<Value>().await.unwrap()["productname"],
        "Ferrite"
    );
}

async fn ws_connect(url: &str, token: &str) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let ws_url = format!("{}/api/ws?token={token}", url.replace("http://", "ws://"));
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();
    let hello = next_json(&mut ws).await;
    assert_eq!(hello["type"], "hello");
    ws
}

async fn next_json<S: StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin>(ws: &mut S) -> Value {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("ws message within 5s")
            .unwrap()
            .unwrap();
        if let WsMessage::Text(t) = m {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

async fn next_of_type<S: StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin>(ws: &mut S, ty: &str) -> Value {
    loop {
        let v = next_json(ws).await;
        if v["type"] == ty {
            return v;
        }
    }
}

#[tokio::test]
async fn talk_realtime_and_call_signaling() {
    let srv = start(|_| {}).await;
    let (alice, bob, _) = admin_and_user(&srv.url).await;
    let atok = alice.post("/api/me/tokens", json!({"name": "ws"})).await["secret"]
        .as_str()
        .unwrap()
        .to_string();
    let btok = bob.post("/api/me/tokens", json!({"name": "ws"})).await["secret"].as_str().unwrap().to_string();
    let room = alice.post("/api/talk/rooms", json!({"name": "Design", "invite": ["bob"]})).await["room"]["token"]
        .as_str()
        .unwrap()
        .to_string();
    let mut aws = ws_connect(&srv.url, &atok).await;
    let mut bws = ws_connect(&srv.url, &btok).await;

    let posted = alice.post(&format!("/api/talk/rooms/{room}/messages"), json!({"body": "Hello @bob"})).await;
    let ev = next_of_type(&mut bws, "talk.message").await;
    assert_eq!(ev["message"]["body"], "Hello @bob");
    assert_eq!(ev["message"]["actor_name"], "Alice");
    let mid = posted["message"]["id"].as_i64().unwrap();

    // Typing over the socket reaches the other participant only.
    bws.send(WsMessage::Text(json!({"type": "typing", "room": room}).to_string().into()))
        .await
        .unwrap();
    let t = next_of_type(&mut aws, "talk.typing").await;
    assert_eq!(t["actor"]["name"], "Bob");

    bob.post(&format!("/api/talk/messages/{mid}/reactions"), json!({"emoji": "🎉"})).await;
    let up = next_of_type(&mut aws, "talk.message_updated").await;
    assert_eq!(up["message"]["reactions"]["🎉"].as_array().unwrap().len(), 1);

    // Call: alice joins, bob joins and is told about alice, signals are relayed point to point.
    aws.send(WsMessage::Text(
        json!({"type": "call.join", "room": room, "audio": true, "video": true}).to_string().into(),
    ))
    .await
    .unwrap();
    let a_self = next_of_type(&mut aws, "call.self").await;
    let a_session = a_self["session"].as_str().unwrap().to_string();
    let room_id = a_self["room_id"].as_str().unwrap().to_string();
    bws.send(WsMessage::Text(json!({"type": "call.join", "room": room, "audio": true}).to_string().into()))
        .await
        .unwrap();
    let b_self = next_of_type(&mut bws, "call.self").await;
    assert_eq!(b_self["members"][0]["session"], a_session.as_str());
    let b_session = b_self["session"].as_str().unwrap().to_string();
    bws.send(WsMessage::Text(
        json!({"type": "call.signal", "room_id": room_id, "session": b_session, "to": a_session, "payload": {"sdp": {"type": "offer", "sdp": "v=0"}}})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let sig = next_of_type(&mut aws, "call.signal").await;
    assert_eq!(sig["from"], b_session.as_str());
    assert_eq!(sig["payload"]["sdp"]["type"], "offer");
    // Spoofed sessions are rejected.
    bws.send(WsMessage::Text(
        json!({"type": "call.signal", "room_id": room_id, "session": a_session, "to": b_session, "payload": {}})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let err = next_of_type(&mut bws, "error").await;
    assert_eq!(err["for"], "call.signal");
    // Bob's socket closing removes him from the call.
    drop(bws);
    let left = next_of_type(&mut aws, "call.left").await;
    assert_eq!(left["session"], b_session.as_str());

    // Guests in a public room.
    let public = alice.post("/api/talk/rooms", json!({"name": "Open day", "kind": "public"})).await["room"]["token"]
        .as_str()
        .unwrap()
        .to_string();
    let anon = reqwest::Client::new();
    let g: Value = anon
        .post(format!("{}/api/talk/guest/{public}", srv.url))
        .json(&json!({"name": "Dana"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let secret = g["secret"].as_str().unwrap();
    let r = anon
        .post(format!("{}/api/talk/rooms/{public}/messages", srv.url))
        .header("X-Guest-Token", secret)
        .json(&json!({"body": "Hi from a guest"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let r = anon
        .get(format!("{}/api/talk/rooms/{room}/messages", srv.url))
        .header("X-Guest-Token", secret)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND, "guests only see their own public room");
}

#[tokio::test]
async fn live_document_collaboration_over_websocket() {
    let srv = start(|_| {}).await;
    let (alice, bob, _) = admin_and_user(&srv.url).await;
    let root = alice.get("/api/me").await["root_id"].as_str().unwrap().to_string();
    let doc = alice
        .post(&format!("/api/files/{root}/documents"), json!({"title": "Plan", "kind": "text"}))
        .await["node"]
        .clone();
    let id = doc["id"].as_str().unwrap().to_string();
    alice
        .post(&format!("/api/files/{id}/shares"), json!({"kind": "user", "target": "bob", "permissions": 3}))
        .await;
    let atok = alice.post("/api/me/tokens", json!({"name": "c"})).await["secret"].as_str().unwrap().to_string();
    let btok = bob.post("/api/me/tokens", json!({"name": "c"})).await["secret"].as_str().unwrap().to_string();

    // Two Yjs-protocol clients, implemented with yrs.
    async fn client(
        url: &str,
        id: &str,
        token: &str,
    ) -> (
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
        yrs::Doc,
    ) {
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("{}/api/collab/{id}?token={token}", url.replace("http://", "ws://")))
            .await
            .unwrap();
        let doc = yrs::Doc::new();
        // Answer the server's sync step 1 and apply its state.
        let first = ws.next().await.unwrap().unwrap().into_data();
        apply_frames(&mut ws, &doc, &first).await;
        // Ask for the full state.
        let sv = doc.transact().state_vector();
        ws.send(WsMessage::Binary(
            yrs::sync::Message::Sync(yrs::sync::SyncMessage::SyncStep1(sv)).encode_v1().into(),
        ))
        .await
        .unwrap();
        (ws, doc)
    }
    async fn apply_frames<W: SinkExt<WsMessage> + Unpin>(ws: &mut W, doc: &yrs::Doc, data: &[u8]) {
        let mut dec = yrs::updates::decoder::DecoderV1::new(yrs::encoding::read::Cursor::new(data));
        for msg in yrs::sync::MessageReader::new(&mut dec) {
            match msg.unwrap() {
                yrs::sync::Message::Sync(yrs::sync::SyncMessage::SyncStep1(sv)) => {
                    let u = doc.transact().encode_state_as_update_v1(&sv);
                    let _ = ws
                        .send(WsMessage::Binary(
                            yrs::sync::Message::Sync(yrs::sync::SyncMessage::SyncStep2(u)).encode_v1().into(),
                        ))
                        .await;
                }
                yrs::sync::Message::Sync(yrs::sync::SyncMessage::SyncStep2(u)) | yrs::sync::Message::Sync(yrs::sync::SyncMessage::Update(u)) => {
                    doc.transact_mut().apply_update(yrs::Update::decode_v1(&u).unwrap()).unwrap();
                }
                _ => {}
            }
        }
    }
    let (mut wa, da) = client(&srv.url, &id, &atok).await;
    let (mut wb, db) = client(&srv.url, &id, &btok).await;
    // Wait until both see the initial content.
    for (ws, d) in [(&mut wa, &da), (&mut wb, &db)] {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while d.get_or_insert_text("content").get_string(&d.transact()) != "# Plan\n\n" {
            let data = tokio::time::timeout_at(deadline, ws.next())
                .await
                .expect("initial sync")
                .unwrap()
                .unwrap()
                .into_data();
            apply_frames(ws, d, &data).await;
        }
    }
    // Alice types; the update goes to the server and on to Bob.
    let update = {
        let t = da.get_or_insert_text("content");
        let mut txn = da.transact_mut();
        t.insert(&mut txn, 8, "Ship it ✓");
        txn.encode_update_v1()
    };
    wa.send(WsMessage::Binary(
        yrs::sync::Message::Sync(yrs::sync::SyncMessage::Update(update)).encode_v1().into(),
    ))
    .await
    .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while db.get_or_insert_text("content").get_string(&db.transact()) != "# Plan\n\nShip it ✓" {
        let data = tokio::time::timeout_at(deadline, wb.next())
            .await
            .expect("bob receives the edit")
            .unwrap()
            .unwrap()
            .into_data();
        apply_frames(&mut wb, &db, &data).await;
    }
    // MCP-style server edit shows up in Bob's editor too.
    let mcp_tok = alice.post("/api/me/tokens", json!({"name": "mcp", "scopes": ["mcp", "docs", "files"]})).await["secret"]
        .as_str()
        .unwrap()
        .to_string();
    let r = mcp_call(
        &srv.url,
        &mcp_tok,
        "tools/call",
        json!({"name": "docs_append", "arguments": {"path": "/Plan.md", "text": "\nBy Friday."}}),
    )
    .await;
    assert_eq!(r["result"]["isError"], false, "{r}");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !db.get_or_insert_text("content").get_string(&db.transact()).ends_with("By Friday.") {
        let data = tokio::time::timeout_at(deadline, wb.next())
            .await
            .expect("bob sees the MCP edit")
            .unwrap()
            .unwrap()
            .into_data();
        apply_frames(&mut wb, &db, &data).await;
    }
    drop(wa);
    drop(wb);
    // After everyone leaves, the file holds the merged text.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let r = alice
            .http
            .get(format!("{}/api/files/{id}/download", srv.url))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        if r == "# Plan\n\nShip it ✓\n\nBy Friday." {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "file content: {r:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn mcp_call(url: &str, token: &str, method: &str, params: Value) -> Value {
    let mut p = params;
    p["_meta"] = json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientCapabilities": {}});
    let mut req = reqwest::Client::new()
        .post(format!("{url}/mcp"))
        .bearer_auth(token)
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", method)
        .header("Accept", "application/json, text/event-stream");
    if let Some(n) = p.get("name").or_else(|| p.get("uri")).and_then(Value::as_str) {
        req = req.header("Mcp-Name", ferrite_mcp::client::encode_header_value(n));
    }
    req.json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": p}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn mcp_endpoint_modern_and_legacy() {
    let srv = start(|_| {}).await;
    let (alice, _bob, _) = admin_and_user(&srv.url).await;
    let tok = alice
        .post("/api/me/tokens", json!({"name": "mcp", "scopes": ["mcp", "files", "boards", "docs", "talk"]}))
        .await["secret"]
        .as_str()
        .unwrap()
        .to_string();
    let http = reqwest::Client::new();
    // No credentials.
    let r = http
        .post(format!("{}/mcp", srv.url))
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "server/discover"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    // Foreign origins are refused (DNS rebinding protection).
    let r = http
        .post(format!("{}/mcp", srv.url))
        .header("Origin", "https://evil.example")
        .bearer_auth(&tok)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let d = mcp_call(&srv.url, &tok, "server/discover", json!({})).await;
    assert_eq!(d["result"]["supportedVersions"][0], "2026-07-28");
    assert_eq!(d["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], "ferrite");
    let tools = mcp_call(&srv.url, &tok, "tools/list", json!({})).await;
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in [
        "files_write",
        "files_share",
        "talk_post",
        "boards_add_card",
        "docs_create",
        "mindmap_add",
        "canvas_add",
        "sheet_set",
        "search",
    ] {
        assert!(names.contains(&expected), "missing {expected}");
    }
    assert!(!names.contains(&"craft_open"), "tools outside the token's scopes are hidden");
    // Write a file, build a board from it, create a mind map and a canvas that arranges them.
    let r = mcp_call(
        &srv.url,
        &tok,
        "tools/call",
        json!({"name": "files_write", "arguments": {"path": "/Launch/brief.md", "content": "# Launch\nWe launch in May."}}),
    )
    .await;
    assert_eq!(r["result"]["structuredContent"]["path"], "/Launch/brief.md", "{r}");
    let b = mcp_call(&srv.url, &tok, "tools/call", json!({"name": "boards_create", "arguments": {"title": "Launch"}})).await;
    let board = b["result"]["structuredContent"]["id"].as_str().unwrap().to_string();
    let c = mcp_call(&srv.url, &tok, "tools/call", json!({"name": "boards_add_card", "arguments": {"board": "Launch", "stack": "To do", "title": "Press kit", "labels": ["Urgent"], "assignees": ["alice"], "due": "2026-11-02"}})).await;
    let card = c["result"]["structuredContent"]["card_id"].as_str().unwrap().to_string();
    mcp_call(
        &srv.url,
        &tok,
        "tools/call",
        json!({"name": "boards_link", "arguments": {"card_id": card, "kind": "file", "target": "/Launch/brief.md"}}),
    )
    .await;
    mcp_call(
        &srv.url,
        &tok,
        "tools/call",
        json!({"name": "boards_move_card", "arguments": {"card_id": card, "stack": "Doing"}}),
    )
    .await;
    let g = mcp_call(&srv.url, &tok, "tools/call", json!({"name": "boards_get", "arguments": {"board": board}})).await;
    let doing = &g["result"]["structuredContent"]["stacks"][1];
    assert_eq!(doing["title"], "Doing");
    assert_eq!(doing["cards"][0]["labels"][0], "Urgent");
    assert_eq!(doing["cards"][0]["links"][0]["title"], "brief.md");
    mcp_call(
        &srv.url,
        &tok,
        "tools/call",
        json!({"name": "docs_create", "arguments": {"folder": "/Launch", "title": "Ideas", "kind": "mindmap"}}),
    )
    .await;
    let m = mcp_call(&srv.url, &tok, "tools/call", json!({"name": "mindmap_add", "arguments": {"path": "/Launch/Ideas.fmind", "nodes": [{"text": "Channels", "children": [{"text": "Blog"}, {"text": "Podcast"}]}, {"text": "Budget"}]}})).await;
    assert_eq!(m["result"]["structuredContent"]["created"].as_array().unwrap().len(), 4, "{m}");
    let read = mcp_call(
        &srv.url,
        &tok,
        "tools/call",
        json!({"name": "docs_read", "arguments": {"path": "/Launch/Ideas.fmind"}}),
    )
    .await;
    assert_eq!(
        read["result"]["structuredContent"]["outline"],
        "- Ideas\n  - Channels\n    - Blog\n    - Podcast\n  - Budget\n"
    );
    mcp_call(
        &srv.url,
        &tok,
        "tools/call",
        json!({"name": "docs_create", "arguments": {"folder": "/Launch", "title": "Overview", "kind": "canvas"}}),
    )
    .await;
    let cv = mcp_call(&srv.url, &tok, "tools/call", json!({"name": "canvas_add", "arguments": {"path": "/Launch/Overview.fcanvas", "items": [
        {"type": "file", "ref": "/Launch/brief.md"}, {"type": "mindmap", "ref": "/Launch/Ideas.fmind"}, {"type": "board", "ref": "Launch"}, {"type": "note", "text": "Kickoff Monday"}]}})).await;
    let ids = cv["result"]["structuredContent"]["created"].as_array().unwrap().clone();
    assert_eq!(ids.len(), 4, "{cv}");
    mcp_call(
        &srv.url,
        &tok,
        "tools/call",
        json!({"name": "canvas_connect", "arguments": {"path": "/Launch/Overview.fcanvas", "from": ids[0], "to": ids[1], "label": "inspires"}}),
    )
    .await;
    let canvas = mcp_call(
        &srv.url,
        &tok,
        "tools/call",
        json!({"name": "docs_read", "arguments": {"path": "/Launch/Overview.fcanvas"}}),
    )
    .await;
    assert_eq!(canvas["result"]["structuredContent"]["items"].as_array().unwrap().len(), 4);
    assert_eq!(canvas["result"]["structuredContent"]["edges"][0]["label"], "inspires");
    // Sheets with A1 references.
    mcp_call(
        &srv.url,
        &tok,
        "tools/call",
        json!({"name": "docs_create", "arguments": {"title": "Budget", "kind": "sheet"}}),
    )
    .await;
    mcp_call(
        &srv.url,
        &tok,
        "tools/call",
        json!({"name": "sheet_set", "arguments": {"path": "/Budget.csv", "cells": {"A1": "Item", "B1": "Cost", "A2": "Ads", "B2": 1200, "B3": "=SUM(B2:B2)"}}}),
    )
    .await;
    let csv = mcp_call(
        &srv.url,
        &tok,
        "tools/call",
        json!({"name": "files_read", "arguments": {"path": "/Budget.csv"}}),
    )
    .await;
    assert_eq!(csv["result"]["content"][0]["text"], "Item,Cost\nAds,1200\n,=SUM(B2:B2)\n");
    // Tool errors are results the model can read, not protocol errors.
    let e = mcp_call(&srv.url, &tok, "tools/call", json!({"name": "files_read", "arguments": {"path": "/nope.txt"}})).await;
    assert_eq!(e["result"]["isError"], true);
    // Resources.
    let res = mcp_call(&srv.url, &tok, "resources/read", json!({"uri": "ferrite://files/Launch/brief.md"})).await;
    assert_eq!(res["result"]["contents"][0]["text"], "# Launch\nWe launch in May.");
    // Header/body mismatch.
    let r = http
        .post(format!("{}/mcp", srv.url))
        .bearer_auth(&tok)
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "tools/list")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "search", "arguments": {}, "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientCapabilities": {}}}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert_eq!(r.json::<Value>().await.unwrap()["error"]["code"], -32020);
    // Legacy client flow (initialize, then header-versioned requests).
    let r = http.post(format!("{}/mcp", srv.url)).bearer_auth(&tok).json(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "old", "version": "1"}}})).send().await.unwrap();
    assert!(r.headers().contains_key("mcp-session-id"));
    assert_eq!(r.json::<Value>().await.unwrap()["result"]["protocolVersion"], "2025-06-18");
    let r: Value = http
        .post(format!("{}/mcp", srv.url))
        .bearer_auth(&tok)
        .header("MCP-Protocol-Version", "2025-06-18")
        .json(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "files_list", "arguments": {"path": "/Launch"}}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["result"]["structuredContent"]["entries"].as_array().unwrap().len(), 3);
    // Ferrite's own client speaks to the endpoint too (dual-era probe picks modern).
    let client = ferrite_mcp::Client::http(&format!("{}/mcp", srv.url), vec![("Authorization".into(), format!("Bearer {tok}"))])
        .await
        .unwrap();
    assert_eq!(client.era(), &ferrite_mcp::Era::Modern("2026-07-28".into()));
    let r = client.call_tool("files_info", json!({"path": "/Launch/brief.md"})).await.unwrap();
    assert_eq!(r.structured_content.unwrap()["live_editor"], "text");
}

/// Minimal OpenAI-compatible model server that calls `files_list` once, then answers.
async fn mock_llm() -> String {
    use axum::{Json, Router, routing::post};
    async fn chat(Json(body): Json<Value>) -> Json<Value> {
        let msgs = body["messages"].as_array().unwrap();
        let last = msgs.last().unwrap();
        if last["role"] == "tool" {
            let listing = last["content"].as_str().unwrap_or("");
            let answer = if listing.contains("report.md") {
                "You have report.md in Reports."
            } else {
                "Nothing found."
            };
            return Json(json!({"choices": [{"finish_reason": "stop", "message": {"role": "assistant", "content": answer}}]}));
        }
        let has_tool = body["tools"]
            .as_array()
            .is_some_and(|t| t.iter().any(|t| t["function"]["name"] == "files_list"));
        assert!(
            !body["tools"].as_array().unwrap().iter().any(|t| t["function"]["name"] == "files_delete"),
            "read-only by default"
        );
        if !has_tool {
            return Json(json!({"choices": [{"finish_reason": "stop", "message": {"role": "assistant", "content": "no tools"}}]}));
        }
        Json(
            json!({"choices": [{"finish_reason": "tool_calls", "message": {"role": "assistant", "content": null, "tool_calls": [
            {"id": "c1", "type": "function", "function": {"name": "files_list", "arguments": "{\"path\":\"/Reports\"}"}}]}}]}),
        )
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, Router::new().route("/v1/chat/completions", post(chat))).await.unwrap() });
    url
}

#[tokio::test]
async fn assistant_and_talk_bot_use_tools() {
    let srv = start(|_| {}).await;
    let (alice, bob, _) = admin_and_user(&srv.url).await;
    let llm = mock_llm().await;
    let p = alice
        .post("/api/ai/providers", json!({"name": "Local", "kind": "openai", "base_url": llm}))
        .await;
    let (s, _) = bob
        .call(
            Method::POST,
            "/api/ai/providers",
            Some(json!({"name": "x", "kind": "openai", "base_url": "http://x"})),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "only admins configure providers");
    let (s, _) = alice
        .call(
            Method::PUT,
            "/api/ai/roles",
            Some(json!({"role": "chat", "provider_id": p["id"], "model": "llama3.2"})),
        )
        .await;
    assert!(s.is_success());
    alice
        .http
        .put(format!(
            "{}/api/files/{}/upload?name=report.md",
            srv.url,
            alice
                .post(
                    &format!("/api/files/{}/mkdir", alice.get("/api/me").await["root_id"].as_str().unwrap()),
                    json!({"name": "Reports"})
                )
                .await["node"]["id"]
                .as_str()
                .unwrap()
        ))
        .header("X-Ferrite-Request", "1")
        .body("Q3")
        .send()
        .await
        .unwrap();
    // Without consent the assistant declines.
    let (s, v) = alice
        .call(
            Method::POST,
            "/api/ai/chat",
            Some(json!({"messages": [{"role": "user", "content": "What reports do I have?"}]})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    alice.call(Method::PATCH, "/api/me", Some(json!({"ai_consent": true}))).await;
    let r = alice
        .post("/api/ai/chat", json!({"messages": [{"role": "user", "content": "What reports do I have?"}]}))
        .await;
    assert_eq!(r["answer"], "You have report.md in Reports.");
    assert_eq!(r["trace"][0]["type"], "tool_call");
    assert_eq!(r["trace"][0]["name"], "files_list");
    assert_eq!(alice.get("/api/ai/jobs").await["jobs"][0]["status"], "done");
    // Talk bot answers @assistant mentions with the asker's permissions.
    let room = alice.post("/api/talk/rooms", json!({"name": "Ops", "invite": ["bob"]})).await["room"]["token"]
        .as_str()
        .unwrap()
        .to_string();
    alice
        .post(&format!("/api/talk/rooms/{room}/messages"), json!({"body": "@assistant which reports exist?"}))
        .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let msgs = bob.get(&format!("/api/talk/rooms/{room}/messages")).await;
        if let Some(m) = msgs["messages"].as_array().unwrap().iter().find(|m| m["actor_kind"] == "bot") {
            assert_eq!(m["body"], "You have report.md in Reports.");
            assert!(m["reply_to"].is_i64());
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "bot did not answer");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Bob hasn't consented: the bot says so instead of reading his data.
    bob.post(&format!("/api/talk/rooms/{room}/messages"), json!({"body": "@assistant hi"})).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let msgs = bob.get(&format!("/api/talk/rooms/{room}/messages")).await;
        if msgs["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["actor_kind"] == "bot" && m["body"].as_str().unwrap().contains("@bob"))
        {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "bot did not answer bob");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn craft_bridge_drives_app_mcp_and_writes_back() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_craft.py");
    let srv = start(|c| {
        c.craft.commands.insert(
            "wordcraft".into(),
            ferrite_core::config::CraftCommand {
                command: "python3".into(),
                args: vec!["-I".into(), fixture.to_string_lossy().into_owned()],
            },
        );
    })
    .await;
    let (alice, bob, _) = admin_and_user(&srv.url).await;
    let tok = alice.post("/api/me/tokens", json!({"name": "mcp"})).await["secret"]
        .as_str()
        .unwrap()
        .to_string();
    mcp_call(
        &srv.url,
        &tok,
        "tools/call",
        json!({"name": "files_write", "arguments": {"path": "/Contract.docx", "content": "Terms."}}),
    )
    .await;
    let apps = mcp_call(&srv.url, &tok, "tools/call", json!({"name": "craft_apps", "arguments": {}})).await;
    let wc = apps["result"]["structuredContent"]["apps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "wordcraft")
        .unwrap()
        .clone();
    assert_eq!(wc["installed"], true);
    let open = mcp_call(
        &srv.url,
        &tok,
        "tools/call",
        json!({"name": "craft_open", "arguments": {"path": "/Contract.docx"}}),
    )
    .await;
    let sc = &open["result"]["structuredContent"];
    assert_eq!(sc["app"], "WordCraft", "{open}");
    let session = sc["session"].as_str().unwrap().to_string();
    let doc_path = sc["document_path"].as_str().unwrap().to_string();
    assert!(sc["tools"].as_array().unwrap().iter().any(|t| t["name"] == "open_document"));
    // The original is locked for others while the app has it.
    let docx = alice.get("/api/files/resolve?path=/Contract.docx").await["entry"].clone();
    assert_eq!(docx["lock"]["owner_app"], "wordcraft");
    let (s, _) = bob.call(Method::GET, "/api/files/resolve?path=/Contract.docx", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    for (tool, args) in [
        ("open_document", json!({"path": doc_path})),
        ("type_text", json!({"text": " Signed."})),
        ("save_document", json!({})),
        ("export_pdf", json!({})),
    ] {
        let r = mcp_call(
            &srv.url,
            &tok,
            "tools/call",
            json!({"name": "craft_call", "arguments": {"session": session, "tool": tool, "arguments": args}}),
        )
        .await;
        assert_eq!(r["result"]["isError"], false, "{tool}: {r}");
    }
    let closed = mcp_call(&srv.url, &tok, "tools/call", json!({"name": "craft_close", "arguments": {"session": session}})).await;
    let mut saved: Vec<String> = serde_json::from_value(closed["result"]["structuredContent"]["saved"].clone()).unwrap();
    saved.sort();
    assert_eq!(saved, vec!["Contract.docx", "Contract.pdf"]);
    let text = mcp_call(
        &srv.url,
        &tok,
        "tools/call",
        json!({"name": "files_read", "arguments": {"path": "/Contract.docx"}}),
    )
    .await;
    assert_eq!(text["result"]["content"][0]["text"], "Terms. Signed.");
    let versions = mcp_call(
        &srv.url,
        &tok,
        "tools/call",
        json!({"name": "files_versions", "arguments": {"path": "/Contract.docx"}}),
    )
    .await;
    assert_eq!(
        versions["result"]["structuredContent"]["versions"].as_array().unwrap().len(),
        1,
        "previous content kept as a version"
    );
    let docx = alice.get("/api/files/resolve?path=/Contract.docx").await["entry"].clone();
    assert!(docx["lock"].is_null(), "lock released");
}

#[tokio::test]
async fn caldav_carddav_sync_and_mcp_tools() {
    let srv = start(|_| {}).await;
    let (alice, _bob, _) = admin_and_user(&srv.url).await;
    let tok = alice.post("/api/me/tokens", json!({"name": "phone", "scopes": ["groupware", "mcp"]})).await["secret"]
        .as_str()
        .unwrap()
        .to_string();
    let files_only = alice.post("/api/me/tokens", json!({"name": "f", "scopes": ["files"]})).await["secret"]
        .as_str()
        .unwrap()
        .to_string();
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let basic = |t: &str| format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("alice:{t}")));
    let dav = |m: &str, path: &str| {
        http.request(Method::from_bytes(m.as_bytes()).unwrap(), format!("{}{path}", srv.url))
            .header("Authorization", basic(&tok))
    };
    // Discovery.
    let r = http.get(format!("{}/.well-known/caldav", srv.url)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(r.headers()["location"], "/remote.php/dav/");
    let r = dav("PROPFIND", "/remote.php/dav/")
        .header("Depth", "0")
        .body("<d:propfind xmlns:d=\"DAV:\"><d:prop><d:current-user-principal/></d:prop></d:propfind>")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::MULTI_STATUS);
    let xml = r.text().await.unwrap();
    assert!(
        xml.contains("<cal:calendar-home-set><d:href>/remote.php/dav/calendars/alice/</d:href>"),
        "{xml}"
    );
    assert!(xml.contains("/remote.php/dav/addressbooks/users/alice/"));
    let r = http
        .request(Method::from_bytes(b"PROPFIND").unwrap(), format!("{}/remote.php/dav/", srv.url))
        .header("Authorization", basic(&files_only))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN, "files-only tokens can't sync calendars");
    let home = dav("PROPFIND", "/remote.php/dav/calendars/alice/")
        .header("Depth", "1")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(home.contains("<d:href>/remote.php/dav/calendars/alice/personal/</d:href>"), "{home}");
    assert!(home.contains("<cal:comp name=\"VEVENT\"/>"));
    // Create, read, query.
    let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:launch-1\r\nDTSTART:20261105T140000Z\r\nDTEND:20261105T150000Z\r\nSUMMARY:Launch review & demo\r\nX-CUSTOM:keep me\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let r = dav("PUT", "/remote.php/dav/calendars/alice/personal/launch-1.ics")
        .header("If-None-Match", "*")
        .header("Content-Type", "text/calendar")
        .body(ics)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    let etag = r.headers()["etag"].to_str().unwrap().to_string();
    assert_eq!(
        dav("GET", "/remote.php/dav/calendars/alice/personal/launch-1.ics")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        ics
    );
    let r = dav("PUT", "/remote.php/dav/calendars/alice/personal/launch-1.ics")
        .header("If-Match", "\"stale\"")
        .body(ics)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::PRECONDITION_FAILED);
    let r = dav("PUT", "/remote.php/dav/calendars/alice/personal/other.ics").body(ics).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CONFLICT, "same UID twice in one calendar");
    let query = |start: &str, end: &str| {
        format!(
            "<c:calendar-query xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\"><d:prop><d:getetag/><c:calendar-data/></d:prop><c:filter><c:comp-filter name=\"VCALENDAR\"><c:comp-filter name=\"VEVENT\"><c:time-range start=\"{start}\" end=\"{end}\"/></c:comp-filter></c:comp-filter></c:filter></c:calendar-query>"
        )
    };
    let hit = dav("REPORT", "/remote.php/dav/calendars/alice/personal/")
        .header("Depth", "1")
        .body(query("20261101T000000Z", "20261201T000000Z"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(hit.contains("Launch review &amp; demo") && hit.contains(&etag.replace('"', "&quot;")), "{hit}");
    let miss = dav("REPORT", "/remote.php/dav/calendars/alice/personal/")
        .body(query("20270101T000000Z", "20270201T000000Z"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!miss.contains("launch-1"));
    let mg = dav("REPORT", "/remote.php/dav/calendars/alice/personal/")
        .body("<c:calendar-multiget xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\"><d:prop><c:calendar-data/></d:prop><d:href>/remote.php/dav/calendars/alice/personal/launch-1.ics</d:href><d:href>/remote.php/dav/calendars/alice/personal/missing.ics</d:href></c:calendar-multiget>")
        .send().await.unwrap().text().await.unwrap();
    assert!(mg.contains("X-CUSTOM:keep me") && mg.contains("404 Not Found"));
    // Sync tokens: initial full sync, then no changes, then a stale token forces a resync.
    let sync = |token: &str| {
        dav("REPORT", "/remote.php/dav/calendars/alice/personal/").body(format!("<d:sync-collection xmlns:d=\"DAV:\"><d:sync-token>{token}</d:sync-token><d:sync-level>1</d:sync-level><d:prop><d:getetag/></d:prop></d:sync-collection>"))
    };
    let full = sync("").send().await.unwrap().text().await.unwrap();
    let token = full.split("<d:sync-token>").nth(1).unwrap().split('<').next().unwrap().to_string();
    assert!(full.contains("launch-1.ics"));
    let same = sync(&token).send().await.unwrap().text().await.unwrap();
    assert!(!same.contains("launch-1.ics"));
    dav("DELETE", "/remote.php/dav/calendars/alice/personal/launch-1.ics").send().await.unwrap();
    let r = sync(&token).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert!(r.text().await.unwrap().contains("valid-sync-token"));
    // New calendars from clients.
    let r = dav("MKCALENDAR", "/remote.php/dav/calendars/alice/work/")
        .body("<c:mkcalendar xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\"><d:set><d:prop><d:displayname>Work</d:displayname></d:prop></d:set></c:mkcalendar>")
        .send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    let cals = alice.get("/api/calendar/calendars").await;
    assert_eq!(cals["calendars"].as_array().unwrap().len(), 2);
    // CardDAV.
    let vcf = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:ada\r\nFN:Ada Lovelace\r\nEMAIL:ada@example.org\r\nEND:VCARD\r\n";
    let books = dav("PROPFIND", "/remote.php/dav/addressbooks/users/alice/")
        .header("Depth", "1")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(books.contains("<card:addressbook/>") && books.contains("/remote.php/dav/addressbooks/users/alice/contacts/"));
    assert_eq!(
        dav("PUT", "/remote.php/dav/addressbooks/users/alice/contacts/ada.vcf")
            .body(vcf)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CREATED
    );
    let aq = dav("REPORT", "/remote.php/dav/addressbooks/users/alice/contacts/")
        .body("<card:addressbook-query xmlns:d=\"DAV:\" xmlns:card=\"urn:ietf:params:xml:ns:carddav\"><d:prop><card:address-data/></d:prop></card:addressbook-query>")
        .send().await.unwrap().text().await.unwrap();
    assert!(aq.contains("FN:Ada Lovelace"));
    let r = dav("PUT", "/remote.php/dav/addressbooks/users/alice/contacts/bad.vcf")
        .body(ics)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN, "events don't go in address books");
    // The assistant's tools see and change the same data.
    let c = mcp_call(&srv.url, &tok, "tools/call", json!({"name": "calendar_create_event", "arguments": {"summary": "Dentist", "start": "2026-11-10T09:00:00Z", "end": "2026-11-10T09:30:00Z", "location": "Main St"}})).await;
    let id = c["result"]["structuredContent"]["event"]["id"].as_str().unwrap().to_string();
    let u = mcp_call(
        &srv.url,
        &tok,
        "tools/call",
        json!({"name": "calendar_update_event", "arguments": {"event_id": id, "start": "2026-11-11T10:00:00Z"}}),
    )
    .await;
    let ev = &u["result"]["structuredContent"]["event"];
    assert_eq!(ev["ends_at"], "2026-11-11T10:30:00Z", "duration kept: {u}");
    assert_eq!(ev["location"], "Main St");
    let l = mcp_call(
        &srv.url,
        &tok,
        "tools/call",
        json!({"name": "calendar_events", "arguments": {"from": "2026-11-01", "to": "2026-12-01"}}),
    )
    .await;
    assert_eq!(l["result"]["structuredContent"]["events"][0]["summary"], "Dentist");
    let found = mcp_call(
        &srv.url,
        &tok,
        "tools/call",
        json!({"name": "contacts_search", "arguments": {"query": "lovelace"}}),
    )
    .await;
    assert_eq!(found["result"]["structuredContent"]["contacts"][0]["emails"][0], "ada@example.org");
    // And phones see the assistant's event over CalDAV.
    let all = dav("REPORT", "/remote.php/dav/calendars/alice/personal/")
        .body(query("20261101T000000Z", "20261201T000000Z"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(all.contains("SUMMARY:Dentist") && all.contains("DTSTART:20261111T100000Z"), "{all}");
}

/// OpenAI-compatible embeddings whose dimensions are "concepts": synonyms share a dimension,
/// so a query can match a document that has none of its words.
async fn mock_embedder() -> String {
    use axum::{Json, Router, routing::post};
    fn embed(text: &str) -> Vec<f32> {
        let mut v = vec![0f32; 16];
        for w in text.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
            let dim = match w {
                "invoice" | "bill" | "payment" | "unpaid" | "outstanding" => 0,
                "car" | "vehicle" | "automobile" => 1,
                "meeting" | "standup" | "sync" => 2,
                _ => 3 + w.bytes().map(usize::from).sum::<usize>() % 13,
            };
            v[dim] += if dim < 3 { 1.0 } else { 0.2 };
        }
        v
    }
    async fn embeddings(Json(body): Json<Value>) -> Json<Value> {
        let inputs: Vec<String> = match &body["input"] {
            Value::String(s) => vec![s.clone()],
            Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
            _ => vec![],
        };
        let data: Vec<Value> = inputs.iter().enumerate().map(|(i, t)| json!({"index": i, "embedding": embed(t)})).collect();
        Json(json!({"data": data, "model": body["model"]}))
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, Router::new().route("/v1/embeddings", post(embeddings))).await.unwrap() });
    url
}

#[tokio::test]
async fn semantic_search_finds_meaning_and_respects_permissions() {
    let srv = start(|_| {}).await;
    let (alice, bob, _) = admin_and_user(&srv.url).await;
    let upload = |who: &Browser, name: &str, body: &str| {
        let (http, url, name, body) = (who.http.clone(), srv.url.clone(), name.to_string(), body.to_string());
        async move {
            let me: Value = http.get(format!("{url}/api/me")).send().await.unwrap().json().await.unwrap();
            let r = http
                .put(format!("{url}/api/files/{}/upload?name={name}", me["root_id"].as_str().unwrap()))
                .header("X-Ferrite-Request", "1")
                .body(body)
                .send()
                .await
                .unwrap();
            assert!(r.status().is_success());
        }
    };
    upload(&alice, "acme.txt", "Outstanding invoice from Acme, due next week.").await;
    upload(&alice, "garage.txt", "The car needs new tyres before winter.").await;
    upload(&bob, "bob-private.txt", "My own invoice that Alice must never see.").await;

    // Full text alone cannot connect "bill" with "invoice".
    let r = alice.get("/api/search?q=bill").await;
    assert_eq!(r["results"].as_array().unwrap().len(), 0, "{r}");
    assert_eq!(r["semantic"], false);

    let p = alice
        .post(
            "/api/ai/providers",
            json!({"name": "Embeddings", "kind": "openai", "base_url": mock_embedder().await}),
        )
        .await;
    let (st, _) = alice
        .call(
            Method::PUT,
            "/api/ai/roles",
            Some(json!({"role": "embed", "provider_id": p["id"], "model": "concepts"})),
        )
        .await;
    assert!(st.is_success());
    let (provider, model) = ferrite_server::ai::provider_for_role(&srv.core, "embed").await.unwrap();
    let n = srv.state.tools.semantic.sync(&provider, &model, true).await.unwrap();
    assert!(n >= 3, "embedded {n} rows");
    // Unchanged rows are not embedded again.
    assert_eq!(srv.state.tools.semantic.sync(&provider, &model, false).await.unwrap(), 0);

    let r = alice.get("/api/search?q=bill").await;
    assert_eq!(r["semantic"], true);
    let titles: Vec<&str> = r["results"].as_array().unwrap().iter().map(|h| h["title"].as_str().unwrap()).collect();
    assert_eq!(titles.first(), Some(&"acme.txt"), "{r}");
    assert!(!titles.contains(&"bob-private.txt"), "other users' files never leak: {titles:?}");
    let r = alice.get("/api/search?q=automobile").await;
    assert_eq!(r["results"][0]["title"], "garage.txt", "{r}");

    // An edit is picked up and re-embedded.
    upload(&alice, "acme.txt", "Team standup notes.").await;
    srv.state.tools.semantic.sync(&provider, &model, false).await.unwrap();
    let r = alice.get("/api/search?q=meeting").await;
    assert_eq!(r["results"][0]["title"], "acme.txt", "{r}");

    // The MCP search tool uses the same hybrid search.
    let token = alice.post("/api/me/tokens", json!({"name": "mcp"})).await["secret"]
        .as_str()
        .unwrap()
        .to_string();
    let v = mcp_call(&srv.url, &token, "tools/call", json!({"name": "search", "arguments": {"query": "vehicle"}})).await;
    let sc = &v["result"]["structuredContent"];
    assert_eq!(sc["semantic"], true, "{v}");
    assert_eq!(sc["results"][0]["title"], "garage.txt", "{v}");
    drop(bob);
}
