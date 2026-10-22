//! Talk moderation and breakout rooms, end to end: the lobby (REST, event socket, SFU offer
//! path, search, scheduled opening), room passwords with throttling, call moderation and call
//! permissions, breakout rooms (assignment modes, live moves, broadcast, help, timer), and the
//! MCP tools. Every check is made as the people who must be refused, not only the moderator.

use ferrite_core::{Config, Core};
use ferrite_server::AppState;
use futures::{SinkExt, StreamExt};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message as WsMessage;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Server {
    url: String,
    core: Core,
    _state: AppState,
    _dir: tempfile::TempDir,
}

async fn start() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let mut cfg = Config {
        data_dir: dir.path().to_path_buf(),
        public_url: url.clone(),
        ..Config::default()
    };
    cfg.sfu.udp_bind = "127.0.0.1:0".into();
    let core = Core::open(cfg).await.unwrap();
    let state = AppState::new(core.clone());
    state.start_background();
    let app = ferrite_server::router(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server {
        url,
        core,
        _state: state,
        _dir: dir,
    }
}

/// A browser-like client: cookie session plus the CSRF header the web app sends (or a guest
/// with its secret).
struct Client {
    http: reqwest::Client,
    url: String,
    guest: Option<String>,
}

impl Client {
    fn new(url: &str) -> Self {
        Client {
            http: reqwest::Client::builder().cookie_store(true).build().unwrap(),
            url: url.to_string(),
            guest: None,
        }
    }
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = self.http.request(method, format!("{}{path}", self.url)).header("X-Ferrite-Request", "1");
        if let Some(g) = &self.guest {
            req = req.header("X-Guest-Token", g);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        (status, resp.json::<Value>().await.unwrap_or(Value::Null))
    }
    async fn ok(&self, method: Method, path: &str, body: Option<Value>) -> Value {
        let (s, v) = self.call(method.clone(), path, body).await;
        assert!(s.is_success(), "{method} {path}: {s} {v}");
        v
    }
    async fn get(&self, path: &str) -> Value {
        self.ok(Method::GET, path, None).await
    }
    async fn post(&self, path: &str, body: Value) -> Value {
        self.ok(Method::POST, path, Some(body)).await
    }
    async fn status(&self, method: Method, path: &str, body: Option<Value>) -> StatusCode {
        self.call(method, path, body).await.0
    }
    async fn login(url: &str, username: &str) -> Self {
        let c = Client::new(url);
        c.post("/api/auth/login", json!({"username": username, "password": format!("{username}-password")}))
            .await;
        c
    }
    async fn token(&self) -> String {
        self.post("/api/me/tokens", json!({"name": "ws"})).await["secret"].as_str().unwrap().to_string()
    }
}

/// alice (admin), bob, carol and dave.
async fn people(url: &str) -> (Client, Client, Client, Client) {
    let alice = Client::new(url);
    alice
        .post(
            "/api/setup",
            json!({"username": "alice", "password": "alice-password", "display_name": "Alice"}),
        )
        .await;
    for (u, n) in [("bob", "Bob"), ("carol", "Carol"), ("dave", "Dave")] {
        alice
            .post(
                "/api/admin/users",
                json!({"username": u, "password": format!("{u}-password"), "display_name": n}),
            )
            .await;
    }
    (
        alice,
        Client::login(url, "bob").await,
        Client::login(url, "carol").await,
        Client::login(url, "dave").await,
    )
}

async fn ws_connect(url: &str, query: &str) -> Ws {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("{}/api/ws?{query}", url.replace("http://", "ws://")))
        .await
        .unwrap();
    assert_eq!(next_json(&mut ws).await["type"], "hello");
    ws
}

async fn next_json(ws: &mut Ws) -> Value {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(8), ws.next())
            .await
            .expect("ws message within 8s")
            .unwrap()
            .unwrap();
        if let WsMessage::Text(t) = m {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

/// The next event of `ty` (skipping others).
async fn next_of(ws: &mut Ws, ty: &str) -> Value {
    loop {
        let v = next_json(ws).await;
        if v["type"] == ty {
            return v;
        }
    }
}

/// The next event of `ty` that satisfies `pred`.
async fn next_where(ws: &mut Ws, ty: &str, pred: impl Fn(&Value) -> bool) -> Value {
    loop {
        let v = next_of(ws, ty).await;
        if pred(&v) {
            return v;
        }
    }
}

/// Asserts no event of `ty` arrives within `ms`.
async fn none_of(ws: &mut Ws, ty: &str, ms: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    while let Ok(Some(Ok(m))) = tokio::time::timeout_at(deadline, ws.next()).await {
        if let WsMessage::Text(t) = m {
            let v: Value = serde_json::from_str(&t).unwrap();
            assert_ne!(v["type"], ty, "unexpected {v}");
        }
    }
}

async fn send(ws: &mut Ws, v: Value) {
    ws.send(WsMessage::Text(v.to_string().into())).await.unwrap();
}

async fn join_call(ws: &mut Ws, room: &str) -> Value {
    send(ws, json!({"type": "call.join", "room": room, "audio": true})).await;
    loop {
        let v = next_json(ws).await;
        if v["type"] == "call.self" || (v["type"] == "error" && v["for"] == "call.join") {
            return v;
        }
    }
}

async fn mcp_tool(url: &str, token: &str, name: &str, args: Value) -> Value {
    let v: Value = reqwest::Client::new()
        .post(format!("{url}/mcp"))
        .bearer_auth(token)
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    v["result"].clone()
}

fn tool_text(r: &Value) -> String {
    r["content"][0]["text"].as_str().unwrap_or_default().to_string()
}

#[tokio::test]
async fn lobby_is_enforced_everywhere_and_opens_on_time() {
    let srv = start().await;
    let (alice, bob, _carol, _dave) = people(&srv.url).await;
    let room = alice.post("/api/talk/rooms", json!({"name": "Town hall", "invite": ["bob"]})).await["room"]["token"]
        .as_str()
        .unwrap()
        .to_string();
    let base = format!("/api/talk/rooms/{room}");
    alice.post(&format!("{base}/messages"), json!({"body": "agenda draft zebra"})).await;
    let (atok, btok) = (alice.token().await, bob.token().await);
    let mut aws = ws_connect(&srv.url, &format!("token={atok}")).await;
    let mut bws = ws_connect(&srv.url, &format!("token={btok}")).await;

    // Only moderators change moderation settings, and only with the CSRF header.
    assert_eq!(
        bob.status(Method::PATCH, &format!("{base}/moderation"), Some(json!({"lobby": true}))).await,
        StatusCode::FORBIDDEN
    );
    let raw = alice
        .http
        .patch(format!("{}{base}/moderation", srv.url))
        .json(&json!({"lobby": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(raw.status(), StatusCode::FORBIDDEN, "cookie writes need the CSRF header");

    // Bob is in the call when the lobby closes: the server ends his session.
    let b_self = join_call(&mut bws, &room).await;
    let b_session = b_self["session"].as_str().unwrap().to_string();
    let room_id = b_self["room_id"].as_str().unwrap().to_string();
    alice.ok(Method::PATCH, &format!("{base}/moderation"), Some(json!({"lobby": true}))).await;
    let kicked = next_of(&mut bws, "call.moderated").await;
    assert_eq!(kicked["action"], "lobby");
    let lobby_ev = next_of(&mut bws, "talk.lobby").await;
    assert_eq!(lobby_ev["lobby"], true);
    assert!(srv.core.call_members(&room_id).is_empty());
    // The old session cannot reach the SFU any more.
    send(
        &mut bws,
        json!({"type": "sfu.offer", "room_id": room_id, "session": b_session, "sdp": {"type": "offer", "sdp": "v=0"}}),
    )
    .await;
    assert_eq!(next_of(&mut bws, "error").await["for"], "sfu.offer");

    // Waiting: no history, no posting, no call, no participants, no previews, no search hits.
    for (m, p) in [
        (Method::GET, format!("{base}/messages")),
        (Method::GET, format!("{base}/participants")),
        (Method::GET, format!("{base}/call")),
    ] {
        assert_eq!(bob.status(m, &p, None).await, StatusCode::FORBIDDEN, "{p}");
    }
    assert_eq!(
        bob.status(Method::POST, &format!("{base}/messages"), Some(json!({"body": "let me in"}))).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(join_call(&mut bws, &room).await["type"], "error");
    let view = bob.get(&base).await["room"].clone();
    assert_eq!(view["lobby"], true);
    assert!(view["last_message"].is_null());
    assert!(bob.get("/api/search?q=zebra").await["results"].as_array().unwrap().is_empty());
    assert!(!alice.get("/api/search?q=zebra").await["results"].as_array().unwrap().is_empty());
    // Messages posted meanwhile reach moderators only.
    alice.post(&format!("{base}/messages"), json!({"body": "moderators only"})).await;
    next_where(&mut aws, "talk.message", |e| e["message"]["body"] == "moderators only").await;
    none_of(&mut bws, "talk.message", 400).await;

    // Moderators see who waits.
    send(&mut bws, json!({"type": "lobby.wait", "room": room})).await;
    let w = next_of(&mut aws, "talk.lobby_waiting").await;
    assert_eq!(w["waiting"][0]["name"], "Bob");
    assert_eq!(alice.get(&format!("{base}/lobby")).await["waiting"][0]["name"], "Bob");
    assert_eq!(bob.status(Method::GET, &format!("{base}/lobby"), None).await, StatusCode::FORBIDDEN);

    // A scheduled opening: the lobby opens by itself and waiting clients are told live.
    let at = (chrono::Utc::now() + chrono::Duration::milliseconds(1500)).to_rfc3339();
    alice
        .ok(Method::PATCH, &format!("{base}/moderation"), Some(json!({"lobby": true, "lobby_timer": at})))
        .await;
    let opened = next_where(&mut bws, "talk.lobby", |e| e["lobby"] == false).await;
    assert!(opened["lobby_timer"].is_null());
    assert!(alice.get(&format!("{base}/lobby")).await["waiting"].as_array().unwrap().is_empty());
    let msgs = bob.get(&format!("{base}/messages")).await;
    assert!(msgs["messages"].as_array().unwrap().iter().any(|m| m["body"] == "moderators only"));
    bob.post(&format!("{base}/messages"), json!({"body": "thanks"})).await;
    assert_eq!(join_call(&mut bws, &room).await["type"], "call.self");
    // An opening time in the past is refused.
    let past = (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();
    assert_eq!(
        alice
            .status(Method::PATCH, &format!("{base}/moderation"), Some(json!({"lobby": true, "lobby_timer": past})))
            .await,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn guests_wait_in_the_lobby_and_need_the_password() {
    let srv = start().await;
    let (alice, _bob, carol, _dave) = people(&srv.url).await;
    let room = alice.post("/api/talk/rooms", json!({"name": "Open day", "kind": "public"})).await["room"]["token"]
        .as_str()
        .unwrap()
        .to_string();
    let base = format!("/api/talk/rooms/{room}");
    // Passwords only for public rooms, at least 4 characters, stored as argon2 hashes.
    let group = alice.post("/api/talk/rooms", json!({"name": "Closed"})).await["room"]["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        alice
            .status(
                Method::PATCH,
                &format!("/api/talk/rooms/{group}/moderation"),
                Some(json!({"password": "secret"}))
            )
            .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        alice
            .status(Method::PATCH, &format!("{base}/moderation"), Some(json!({"password": "abc"})))
            .await,
        StatusCode::BAD_REQUEST
    );
    let r = alice
        .ok(Method::PATCH, &format!("{base}/moderation"), Some(json!({"password": "open sesame"})))
        .await;
    assert_eq!(r["room"]["has_password"], true);
    assert!(r["room"].get("password_hash").is_none());
    let hash: String = sqlx::query_scalar("SELECT password_hash FROM rooms WHERE token = ?")
        .bind(&room)
        .fetch_one(&srv.core.db)
        .await
        .unwrap();
    assert!(hash.starts_with("$argon2"));
    assert_eq!(
        Client::new(&srv.url).get(&format!("/api/talk/guest/{room}")).await["room"]["has_password"],
        true
    );

    // Guests: no password, wrong password, right password.
    let anon = Client::new(&srv.url);
    let gpath = format!("/api/talk/guest/{room}");
    assert_eq!(
        anon.status(Method::POST, &gpath, Some(json!({"name": "Mallory"}))).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        anon.status(Method::POST, &gpath, Some(json!({"name": "Mallory", "password": "nope"}))).await,
        StatusCode::BAD_REQUEST
    );
    let g = anon.post(&gpath, json!({"name": "Dana", "password": "open sesame"})).await;
    let mut guest = Client::new(&srv.url);
    guest.guest = Some(g["secret"].as_str().unwrap().to_string());

    // Signed-in non-participants joining by link need it too; participants never do.
    assert_eq!(
        carol.status(Method::POST, &format!("{base}/join"), Some(json!({}))).await,
        StatusCode::BAD_REQUEST
    );
    carol.post(&format!("{base}/join"), json!({"password": "open sesame"})).await;
    carol.post(&format!("{base}/join"), json!({})).await;

    // Wrong guesses are throttled per room.
    for _ in 0..8 {
        anon.status(Method::POST, &gpath, Some(json!({"name": "Mallory", "password": "guess"}))).await;
    }
    assert_eq!(
        anon.status(Method::POST, &gpath, Some(json!({"name": "Mallory", "password": "open sesame"})))
            .await,
        StatusCode::SERVICE_UNAVAILABLE
    );

    // The guest waits in the lobby like everyone else, and a guest is never a moderator.
    let mut gws = ws_connect(&srv.url, &format!("guest={}", guest.guest.as_ref().unwrap())).await;
    alice.ok(Method::PATCH, &format!("{base}/moderation"), Some(json!({"lobby": true}))).await;
    assert_eq!(next_of(&mut gws, "talk.lobby").await["lobby"], true);
    assert_eq!(guest.status(Method::GET, &format!("{base}/messages"), None).await, StatusCode::FORBIDDEN);
    assert_eq!(
        guest.status(Method::POST, &format!("{base}/messages"), Some(json!({"body": "hi"}))).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(join_call(&mut gws, &room).await["type"], "error");
    assert_eq!(
        guest.status(Method::PATCH, &format!("{base}/moderation"), Some(json!({"lobby": false}))).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(guest.get(&gpath).await["room"]["lobby"], true);
    alice
        .ok(Method::PATCH, &format!("{base}/moderation"), Some(json!({"lobby": false, "password": null})))
        .await;
    assert_eq!(next_of(&mut gws, "talk.lobby").await["lobby"], false);
    guest.post(&format!("{base}/messages"), json!({"body": "hello from the guest"})).await;
    assert_eq!(Client::new(&srv.url).get(&gpath).await["room"]["has_password"], false);
}

#[tokio::test]
async fn moderators_mute_lower_hands_and_remove_from_calls() {
    let srv = start().await;
    let (alice, bob, carol, _dave) = people(&srv.url).await;
    let room = alice.post("/api/talk/rooms", json!({"name": "Standup", "invite": ["bob", "carol"]})).await["room"]["token"]
        .as_str()
        .unwrap()
        .to_string();
    let base = format!("/api/talk/rooms/{room}");
    let mut aws = ws_connect(&srv.url, &format!("token={}", alice.token().await)).await;
    let mut bws = ws_connect(&srv.url, &format!("token={}", bob.token().await)).await;
    let mut cws = ws_connect(&srv.url, &format!("token={}", carol.token().await)).await;
    let a = join_call(&mut aws, &room).await;
    let b = join_call(&mut bws, &room).await;
    let c = join_call(&mut cws, &room).await;
    let room_id = a["room_id"].as_str().unwrap().to_string();
    let (bs, cs) = (b["session"].as_str().unwrap().to_string(), c["session"].as_str().unwrap().to_string());

    // Participants cannot moderate.
    for action in ["mute", "mute_all", "lower_hand", "remove"] {
        assert_eq!(
            carol
                .status(Method::POST, &format!("{base}/call/moderate"), Some(json!({"action": action, "session": bs})))
                .await,
            StatusCode::FORBIDDEN,
            "{action}"
        );
    }

    // Ask to mute: only the target's connection is told.
    alice.post(&format!("{base}/call/moderate"), json!({"action": "mute", "session": bs})).await;
    let m = next_of(&mut bws, "call.moderated").await;
    assert_eq!((m["action"].as_str(), m["by"]["name"].as_str()), (Some("mute"), Some("Alice")));
    none_of(&mut cws, "call.moderated", 300).await;
    let all = alice.post(&format!("{base}/call/moderate"), json!({"action": "mute_all"})).await;
    assert_eq!(all["affected"], 2, "everyone but the moderator");
    assert_eq!(next_of(&mut cws, "call.moderated").await["action"], "mute");

    // Raised hands form a queue in the order they went up; moderators lower them.
    send(&mut cws, json!({"type": "call.state", "room_id": room_id, "session": cs, "hand": true})).await;
    next_where(&mut aws, "call.state", |e| e["member"]["session"] == cs.as_str()).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    send(&mut bws, json!({"type": "call.state", "room_id": room_id, "session": bs, "hand": true})).await;
    next_where(&mut aws, "call.state", |e| e["member"]["session"] == bs.as_str()).await;
    let hands = alice.get(&format!("{base}/call")).await["hands"].clone();
    assert_eq!(hands[0]["actor"]["name"], "Carol");
    assert_eq!(hands[1]["actor"]["name"], "Bob");
    alice
        .post(&format!("{base}/call/moderate"), json!({"action": "lower_hand", "session": cs}))
        .await;
    assert_eq!(next_of(&mut cws, "call.moderated").await["action"], "lower_hand");
    let st = next_where(&mut aws, "call.state", |e| e["member"]["session"] == cs.as_str()).await;
    assert_eq!(st["member"]["hand"], false);
    assert_eq!(alice.get(&format!("{base}/call")).await["hands"][0]["actor"]["name"], "Bob");

    // Removing ends the session on the server; the person may join again.
    alice.post(&format!("{base}/call/moderate"), json!({"action": "remove", "session": bs})).await;
    next_where(&mut bws, "call.moderated", |e| e["action"] == "removed").await;
    assert_eq!(next_where(&mut aws, "call.left", |e| e["session"] == bs.as_str()).await["remaining"], 2);
    assert!(!srv.core.call_members(&room_id).iter().any(|m| m.session == bs));
    send(&mut bws, json!({"type": "call.state", "room_id": room_id, "session": bs, "audio": false})).await;
    assert_eq!(next_of(&mut bws, "error").await["for"], "call.state", "the ended session is gone");
    assert_eq!(join_call(&mut bws, &room).await["type"], "call.self");

    // Removed from the conversation: out of the call too.
    let bob_id = bob.get("/api/me").await["user"]["id"].as_str().unwrap().to_string();
    let (s, _) = alice.call(Method::DELETE, &format!("{base}/participants/{bob_id}"), None).await;
    assert!(s.is_success());
    assert!(!srv.core.call_members(&room_id).iter().any(|m| m.actor.id == bob_id));

    // Who may start a call.
    assert_eq!(
        carol
            .status(Method::PATCH, &format!("{base}/moderation"), Some(json!({"call_permission": "moderators"})))
            .await,
        StatusCode::FORBIDDEN
    );
    alice
        .ok(Method::PATCH, &format!("{base}/moderation"), Some(json!({"call_permission": "moderators"})))
        .await;
    send(&mut aws, json!({"type": "call.leave", "room_id": room_id, "session": a["session"]})).await;
    send(&mut cws, json!({"type": "call.leave", "room_id": room_id, "session": cs})).await;
    next_where(&mut aws, "call.left", |e| e["ended"] == true).await;
    let refused = join_call(&mut cws, &room).await;
    assert_eq!(refused["type"], "error");
    assert!(refused["error"].as_str().unwrap().contains("moderators"));
    assert_eq!(join_call(&mut aws, &room).await["type"], "call.self");
    assert_eq!(join_call(&mut cws, &room).await["type"], "call.self", "joining a running call is fine");
}

#[tokio::test]
async fn breakout_rooms_move_people_and_bring_them_back() {
    let srv = start().await;
    let (alice, bob, carol, dave) = people(&srv.url).await;
    let room = alice
        .post(
            "/api/talk/rooms",
            json!({"name": "Workshop", "kind": "public", "invite": ["bob", "carol", "dave"]}),
        )
        .await["room"]["token"]
        .as_str()
        .unwrap()
        .to_string();
    let base = format!("/api/talk/rooms/{room}");
    let mut aws = ws_connect(&srv.url, &format!("token={}", alice.token().await)).await;
    let mut bws = ws_connect(&srv.url, &format!("token={}", bob.token().await)).await;
    let _cws = ws_connect(&srv.url, &format!("token={}", carol.token().await)).await;
    let anon = Client::new(&srv.url);
    let g = anon.post(&format!("/api/talk/guest/{room}"), json!({"name": "Gil"})).await;
    let mut guest = Client::new(&srv.url);
    guest.guest = Some(g["secret"].as_str().unwrap().to_string());
    let gid = g["guest"]["id"].as_str().unwrap().to_string();
    let mut gws = ws_connect(&srv.url, &format!("guest={}", guest.guest.as_ref().unwrap())).await;

    // Only moderators set up breakout rooms; not in one-to-one rooms; 1-20 rooms.
    assert_eq!(
        bob.status(Method::POST, &format!("{base}/breakouts"), Some(json!({"mode": "auto", "count": 2})))
            .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        alice
            .status(Method::POST, &format!("{base}/breakouts"), Some(json!({"mode": "auto", "count": 21})))
            .await,
        StatusCode::BAD_REQUEST
    );
    let direct = alice.post("/api/talk/direct", json!({"username": "bob"})).await["room"]["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        alice
            .status(
                Method::POST,
                &format!("/api/talk/rooms/{direct}/breakouts"),
                Some(json!({"mode": "auto", "count": 2}))
            )
            .await,
        StatusCode::BAD_REQUEST
    );

    // Automatic: four people (three users and the online guest) spread 2 + 2.
    let v = alice.post(&format!("{base}/breakouts"), json!({"mode": "auto", "count": 2})).await["breakouts"].clone();
    let sizes: Vec<usize> = v["rooms"].as_array().unwrap().iter().map(|r| r["members"].as_array().unwrap().len()).collect();
    assert_eq!(sizes, vec![2, 2]);
    assert!(v["unassigned"].as_array().unwrap().is_empty());
    let rooms: Vec<(String, String)> = v["rooms"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["token"].as_str().unwrap().to_string(), r["name"].as_str().unwrap().to_string()))
        .collect();
    // Manual moves: bob and the guest in Room 1, carol and dave in Room 2.
    let id_of = |c: &Client| {
        let c = c.http.clone();
        let url = srv.url.clone();
        async move {
            c.get(format!("{url}/api/me")).send().await.unwrap().json::<Value>().await.unwrap()["user"]["id"]
                .as_str()
                .unwrap()
                .to_string()
        }
    };
    let (bid, cid, did) = (id_of(&bob).await, id_of(&carol).await, id_of(&dave).await);
    alice
        .post(
            &format!("{base}/breakouts/assign"),
            json!({"assignments": [
                {"kind": "user", "id": bid, "room": rooms[0].0}, {"kind": "guest", "id": gid, "room": rooms[0].0},
                {"kind": "user", "id": cid, "room": rooms[1].0}, {"kind": "user", "id": did, "room": rooms[1].0},
            ]}),
        )
        .await;
    assert_eq!(
        bob.status(
            Method::POST,
            &format!("{base}/breakouts/assign"),
            Some(json!({"assignments": [{"kind": "user", "id": bid, "room": rooms[1].0}]}))
        )
        .await,
        StatusCode::FORBIDDEN
    );
    // Breakout rooms are not listed, and cannot be deleted on their own.
    assert!(
        !bob.get("/api/talk/rooms").await["rooms"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["kind"] == "breakout")
    );
    assert_eq!(
        alice.status(Method::DELETE, &format!("/api/talk/rooms/{}", rooms[0].0), None).await,
        StatusCode::BAD_REQUEST
    );

    // Bob is in the main call; starting moves him (the server ends that session).
    let b = join_call(&mut bws, &room).await;
    let main_id = b["room_id"].as_str().unwrap().to_string();
    let started = alice.post(&format!("{base}/breakouts/start"), json!({"minutes": 30})).await["breakouts"].clone();
    assert_eq!(started["status"], "running");
    assert!(started["ends_at"].is_string());
    assert_eq!(next_of(&mut bws, "call.moderated").await["action"], "breakout");
    let ev = next_where(&mut bws, "talk.breakouts", |e| e["breakouts"]["status"] == "running").await;
    assert!(
        ev["breakouts"]["rooms"][0]["members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == bid.as_str())
    );
    next_where(&mut gws, "talk.breakouts", |e| e["breakouts"]["status"] == "running").await;
    assert!(srv.core.call_members(&main_id).is_empty());
    // Assigned people call in their breakout room, not in the main one.
    let refused = join_call(&mut bws, &room).await;
    assert!(refused["error"].as_str().unwrap().contains("breakout"));
    let bb = join_call(&mut bws, &rooms[0].0).await;
    assert_eq!(bb["type"], "call.self");
    let r1_id = bb["room_id"].as_str().unwrap().to_string();
    // ...and only reach their own room.
    assert_eq!(
        bob.status(Method::GET, &format!("/api/talk/rooms/{}/messages", rooms[1].0), None).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        guest.status(Method::GET, &format!("/api/talk/rooms/{}/messages", rooms[1].0), None).await,
        StatusCode::NOT_FOUND
    );
    guest
        .post(&format!("/api/talk/rooms/{}/messages", rooms[0].0), json!({"body": "guest in room 1"}))
        .await;
    // Moderators visit any room.
    alice.get(&format!("/api/talk/rooms/{}/messages", rooms[1].0)).await;
    assert_eq!(
        alice.get(&format!("/api/talk/rooms/{}", rooms[1].0)).await["room"]["parent_token"],
        room.as_str()
    );

    // Broadcast reaches every breakout room; only moderators may send one.
    assert_eq!(
        bob.status(Method::POST, &format!("{base}/breakouts/broadcast"), Some(json!({"body": "x"})))
            .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        alice.post(&format!("{base}/breakouts/broadcast"), json!({"body": "Five minutes left"})).await["rooms"],
        2
    );
    for (t, _) in &rooms {
        let msgs = alice.get(&format!("/api/talk/rooms/{t}/messages")).await;
        let last = msgs["messages"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(
            (last["body"].as_str(), last["meta"]["broadcast"].as_bool()),
            (Some("Five minutes left"), Some(true))
        );
    }

    // Asking for help notifies the moderators with a link to the room.
    guest.post(&format!("/api/talk/rooms/{}/help", rooms[0].0), json!({"on": true})).await;
    let help = next_of(&mut aws, "talk.breakout_help").await;
    assert_eq!(help["room"]["name"], rooms[0].1.as_str());
    assert_eq!(help["link"], format!("/talk/{}", rooms[0].0));
    let v = alice.get(&format!("{base}/breakouts")).await["breakouts"].clone();
    assert!(v["rooms"][0]["help_requested_at"].is_string());
    assert_eq!(
        dave.status(Method::POST, &format!("/api/talk/rooms/{}/help", rooms[0].0), Some(json!({})))
            .await,
        StatusCode::NOT_FOUND
    );

    // A participant cannot stop them; the moderator can.
    assert_eq!(
        bob.status(Method::POST, &format!("{base}/breakouts/stop"), Some(json!({}))).await,
        StatusCode::FORBIDDEN
    );
    alice.post(&format!("{base}/breakouts/stop"), json!({})).await;
    assert_eq!(next_of(&mut bws, "call.moderated").await["action"], "breakout_ended");
    assert_eq!(
        next_where(&mut bws, "talk.breakouts", |e| e["breakouts"]["status"] == "stopped").await["breakouts"]["rooms"][0]["help_requested_at"],
        Value::Null
    );
    assert!(srv.core.call_members(&r1_id).is_empty());
    // Chats stay readable, but the rooms are closed for writing and calls.
    let msgs = bob.get(&format!("/api/talk/rooms/{}/messages", rooms[0].0)).await;
    assert!(msgs["messages"].as_array().unwrap().iter().any(|m| m["body"] == "guest in room 1"));
    assert_eq!(
        bob.status(Method::POST, &format!("/api/talk/rooms/{}/messages", rooms[0].0), Some(json!({"body": "late"})))
            .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(join_call(&mut bws, &rooms[0].0).await["type"], "error");
    assert_eq!(join_call(&mut bws, &room).await["type"], "call.self", "back in the main call");

    // Free choice: participants pick their room while the rooms run; the timer stops them.
    alice.post(&format!("{base}/breakouts"), json!({"mode": "free", "count": 3})).await;
    let v = alice.get(&format!("{base}/breakouts")).await["breakouts"].clone();
    let t3 = v["rooms"][2]["token"].as_str().unwrap().to_string();
    assert_eq!(
        guest.status(Method::POST, &format!("{base}/breakouts/choose"), Some(json!({"room": t3}))).await,
        StatusCode::FORBIDDEN,
        "not running yet"
    );
    alice.post(&format!("{base}/breakouts/start"), json!({"minutes": 1})).await;
    let chosen = guest.post(&format!("{base}/breakouts/choose"), json!({"room": t3})).await["breakouts"].clone();
    assert_eq!(chosen["rooms"][2]["members"][0]["id"], gid.as_str());
    guest.get(&format!("/api/talk/rooms/{t3}/messages")).await;
    sqlx::query("UPDATE rooms SET breakout_ends_at = ? WHERE token = ?")
        .bind(chrono::Utc::now() - chrono::Duration::seconds(1))
        .bind(&room)
        .execute(&srv.core.db)
        .await
        .unwrap();
    srv.core.run_talk_timers().await;
    let ended = next_where(&mut gws, "talk.breakouts", |e| e["breakouts"]["status"] == "stopped").await;
    assert!(ended["breakouts"]["ends_at"].is_null());
    let sys = alice.get(&format!("{base}/messages")).await;
    assert!(sys["messages"].as_array().unwrap().iter().any(|m| m["body"] == "The breakout rooms ended"));

    // Removing them deletes their chats.
    alice.ok(Method::DELETE, &format!("{base}/breakouts"), None).await;
    assert_eq!(
        alice.status(Method::GET, &format!("/api/talk/rooms/{t3}/messages"), None).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn mcp_tools_manage_lobby_and_breakouts() {
    let srv = start().await;
    let (alice, bob, carol, _dave) = people(&srv.url).await;
    let room = alice.post("/api/talk/rooms", json!({"name": "Seminar", "invite": ["bob", "carol"]})).await["room"]["token"]
        .as_str()
        .unwrap()
        .to_string();
    let scoped = |c: &Client, scopes: Value| {
        let http = c.http.clone();
        let url = srv.url.clone();
        async move {
            http.post(format!("{url}/api/me/tokens"))
                .header("X-Ferrite-Request", "1")
                .json(&json!({"name": "mcp", "scopes": scopes}))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()["secret"]
                .as_str()
                .unwrap()
                .to_string()
        }
    };
    let atok = scoped(&alice, json!(["mcp", "talk"])).await;
    let btok = scoped(&bob, json!(["mcp", "talk"])).await;
    let files_only = scoped(&alice, json!(["mcp", "files"])).await;

    let r = mcp_tool(&srv.url, &files_only, "talk_lobby", json!({"room": room, "enabled": true})).await;
    assert_eq!(r["isError"], true, "the talk scope is required");
    let r = mcp_tool(&srv.url, &btok, "talk_lobby", json!({"room": room, "enabled": true})).await;
    assert_eq!(r["isError"], true, "participants cannot moderate: {r}");

    let r = mcp_tool(&srv.url, &atok, "talk_lobby", json!({"room": "Seminar", "enabled": true})).await;
    assert_ne!(r["isError"], true, "{r}");
    assert_eq!(
        bob.status(Method::GET, &format!("/api/talk/rooms/{room}/messages"), None).await,
        StatusCode::FORBIDDEN
    );
    mcp_tool(&srv.url, &atok, "talk_lobby", json!({"room": room, "enabled": false})).await;
    bob.get(&format!("/api/talk/rooms/{room}/messages")).await;

    let carol_id = carol.get("/api/me").await["user"]["id"].as_str().unwrap().to_string();
    let r = mcp_tool(
        &srv.url,
        &atok,
        "talk_breakouts_start",
        json!({"room": room, "count": 2, "mode": "manual", "assignments": {"bob": 1, "carol": 2}, "minutes": 15}),
    )
    .await;
    assert_ne!(r["isError"], true, "{r}");
    let v: Value = serde_json::from_str(&tool_text(&r)).unwrap();
    assert_eq!(v["status"], "running");
    assert_eq!(v["rooms"][1]["members"][0]["id"], carol_id.as_str());
    let r = mcp_tool(&srv.url, &btok, "talk_breakouts", json!({"room": room})).await;
    assert_eq!(serde_json::from_str::<Value>(&tool_text(&r)).unwrap()["mode"], "manual");
    let r = mcp_tool(&srv.url, &btok, "talk_breakouts_broadcast", json!({"room": room, "text": "hi"})).await;
    assert_eq!(r["isError"], true);
    let r = mcp_tool(&srv.url, &atok, "talk_breakouts_broadcast", json!({"room": room, "text": "Wrap up"})).await;
    assert!(tool_text(&r).contains("2 breakout rooms"), "{r}");
    let r = mcp_tool(&srv.url, &atok, "talk_breakouts_stop", json!({"room": room})).await;
    assert_ne!(r["isError"], true, "{r}");
    assert_eq!(alice.get(&format!("/api/talk/rooms/{room}/breakouts")).await["breakouts"]["status"], "stopped");
}
