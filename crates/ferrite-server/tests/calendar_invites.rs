//! End-to-end tests of calendar sharing and scheduling: invitations to Ferrite users and
//! by email (iMIP, checked through an SMTP sink), answers from the web app, CalDAV clients
//! and the public response page, cancellations, CalDAV privileges of shared calendars, the
//! public ICS feed, and that no attendee ever receives an invitation twice.

use base64::Engine;
use ferrite_core::mailer::sink::{Received, sink};
use ferrite_core::mailer::{SmtpSecurity, SmtpSettings};
use ferrite_core::{Config, Core};
use futures::StreamExt;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_tungstenite::tungstenite::Message as WsMessage;

struct Server {
    url: String,
    _core: Core,
    mail: Option<UnboundedReceiver<Received>>,
    _dir: tempfile::TempDir,
}

async fn start(with_mail: bool) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let mut cfg = Config {
        data_dir: dir.path().to_path_buf(),
        public_url: url.clone(),
        ..Config::default()
    };
    cfg.sfu.enabled = false;
    let mut mail = None;
    if with_mail {
        let (port, rx) = sink().await;
        cfg.smtp = Some(SmtpSettings {
            host: "127.0.0.1".into(),
            port,
            security: SmtpSecurity::None,
            username: None,
            password: None,
            from: "Ferrite <noreply@example.org>".into(),
        });
        mail = Some(rx);
    }
    let core = Core::open(cfg).await.unwrap();
    let state = ferrite_server::AppState::new(core.clone());
    let app = ferrite_server::router(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server {
        url,
        _core: core,
        mail,
        _dir: dir,
    }
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
    async fn patch(&self, path: &str, body: Value) -> Value {
        let (s, v) = self.call(Method::PATCH, path, Some(body)).await;
        assert!(s.is_success(), "PATCH {path}: {s} {v}");
        v
    }
    async fn login(url: &str, username: &str, password: &str) -> Self {
        let b = Browser::new(url);
        b.post("/api/auth/login", json!({"username": username, "password": password})).await;
        b
    }
    /// Events between Nov 2026 and Feb 2027.
    async fn events(&self) -> Vec<Value> {
        self.get("/api/calendar/events?from=2026-11-01T00:00:00Z&to=2027-02-01T00:00:00Z").await["events"]
            .as_array()
            .unwrap()
            .clone()
    }
    async fn event(&self, summary: &str) -> Value {
        self.events()
            .await
            .into_iter()
            .find(|e| e["summary"] == summary)
            .unwrap_or_else(|| panic!("no event {summary}"))
    }
    async fn notifications(&self) -> Vec<String> {
        self.get("/api/notifications").await["notifications"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["title"].as_str().unwrap().to_string())
            .collect()
    }
}

/// alice (admin, alice@example.org), bob (bob@example.org) and dave (no email).
async fn people(url: &str) -> (Browser, Browser, Browser) {
    let alice = Browser::new(url);
    alice
        .post(
            "/api/setup",
            json!({"username": "alice", "password": "alice-password", "display_name": "Alice", "email": "alice@example.org"}),
        )
        .await;
    alice
        .post(
            "/api/admin/users",
            json!({"username": "bob", "password": "bob-password", "display_name": "Bob", "email": "Bob@Example.org"}),
        )
        .await;
    alice
        .post(
            "/api/admin/users",
            json!({"username": "dave", "password": "dave-password", "display_name": "Dave"}),
        )
        .await;
    let bob = Browser::login(url, "bob", "bob-password").await;
    let dave = Browser::login(url, "dave", "dave-password").await;
    (alice, bob, dave)
}

/// One mail from the sink (within 10 s).
async fn mail(rx: &mut UnboundedReceiver<Received>) -> Mail {
    let m = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("a mail within 10s")
        .unwrap();
    Mail::parse(m)
}

/// Asserts that no mail arrives (scheduling sends mail right after the write returns).
async fn no_mail(rx: &mut UnboundedReceiver<Received>, why: &str) {
    if let Ok(Some(m)) = tokio::time::timeout(Duration::from_millis(700), rx.recv()).await {
        panic!("unexpected mail ({why}) to {:?}:\n{}", m.to, m.data);
    }
}

/// A received message with its MIME parts decoded.
struct Mail {
    to: Vec<String>,
    headers: String,
    text: String,
    calendar: String,
    calendar_type: String,
}

fn header_value<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

fn decode_qp(s: &str) -> String {
    let joined = s.replace("=\r\n", "");
    let bytes = joined.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'=' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&joined[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Leaf MIME parts as (headers, decoded body).
fn parts(raw: &str) -> Vec<(String, String)> {
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw, ""));
    let head = head.replace("\r\n ", " ").replace("\r\n\t", " ");
    let ct = header_value(&head, "content-type").unwrap_or("text/plain").to_string();
    if ct.to_ascii_lowercase().starts_with("multipart/") {
        let boundary = ct
            .split(';')
            .find_map(|p| p.trim().strip_prefix("boundary="))
            .unwrap()
            .trim_matches('"')
            .to_string();
        let delim = format!("--{boundary}");
        return body
            .split(&delim)
            .skip(1)
            .filter(|p| !p.starts_with("--"))
            .flat_map(|p| parts(p.trim_start_matches("\r\n")))
            .collect();
    }
    let decoded = match header_value(&head, "content-transfer-encoding").map(str::to_ascii_lowercase).as_deref() {
        Some("base64") => {
            let clean: String = body.chars().filter(|c| !c.is_whitespace()).collect();
            String::from_utf8(base64::engine::general_purpose::STANDARD.decode(clean).unwrap()).unwrap()
        }
        Some("quoted-printable") => decode_qp(body),
        _ => body.to_string(),
    };
    vec![(head, decoded)]
}

impl Mail {
    fn parse(m: Received) -> Mail {
        let headers = m.data.split_once("\r\n\r\n").map(|(h, _)| h).unwrap_or("").replace("\r\n ", " ");
        let leaves = parts(&m.data);
        let find = |t: &str| leaves.iter().find(|(h, _)| header_value(h, "content-type").is_some_and(|c| c.starts_with(t)));
        let (cal_head, calendar) = find("text/calendar").cloned().expect("a text/calendar part");
        Mail {
            to: m.to,
            headers,
            text: find("text/plain").map(|(_, b)| b.clone()).unwrap_or_default(),
            calendar: calendar.replace("\r\n ", ""),
            calendar_type: header_value(&cal_head, "content-type").unwrap().to_string(),
        }
    }
    fn header(&self, name: &str) -> &str {
        header_value(&self.headers, name).unwrap_or("")
    }
    /// The response-page token from the "Accept" link.
    fn token(&self) -> String {
        let at = self.text.find("/invitation/").expect("invitation link") + "/invitation/".len();
        self.text[at..].split(['?', '\r', '\n']).next().unwrap().to_string()
    }
}

fn attendee<'a>(ev: &'a Value, email: &str) -> &'a Value {
    ev["attendees"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["email"] == email)
        .unwrap_or_else(|| panic!("{email} not among {}", ev["attendees"]))
}

#[tokio::test]
async fn invitations_internal_and_by_email_with_answers_updates_and_cancellation() {
    let mut srv = start(true).await;
    let mut rx = srv.mail.take().unwrap();
    let (alice, bob, dave) = people(&srv.url).await;
    assert_eq!(alice.get("/api/calendar/calendars").await["can_email"], true);

    // Without an email address you cannot organize.
    let (st, err) = dave
        .call(
            Method::POST,
            "/api/calendar/events",
            Some(json!({"summary": "x", "start": "2026-11-05T09:00:00Z", "attendees": ["bob@example.org"]})),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{err}");

    // Alice invites Bob (internal) and Carol (external).
    let created = alice
        .post(
            "/api/calendar/events",
            json!({"summary": "Planning", "start": "2026-11-05T09:00:00Z", "end": "2026-11-05T10:00:00Z", "location": "Room 1",
                   "attendees": ["Bob <bob@example.org>", "Carol Chen <carol@example.net>", "alice@example.org"]}),
        )
        .await;
    let ev = &created["event"];
    let id = ev["id"].as_str().unwrap().to_string();
    assert_eq!(ev["organizer"]["email"], "alice@example.org");
    assert_eq!(ev["attendees"].as_array().unwrap().len(), 2, "the organizer is not her own attendee: {ev}");
    assert_eq!(attendee(ev, "carol@example.net")["partstat"], "NEEDS-ACTION");

    let m = mail(&mut rx).await;
    assert_eq!(m.to, vec!["carol@example.net"]);
    assert!(m.header("Reply-To").contains("alice@example.org"), "{}", m.headers);
    assert!(m.header("From").contains("Alice via Ferrite") && m.header("From").contains("noreply@example.org"));
    assert!(m.calendar_type.contains("method=REQUEST"), "{}", m.calendar_type);
    for needle in [
        "METHOD:REQUEST",
        "ORGANIZER;CN=Alice:mailto:alice@example.org",
        "mailto:carol@example.net",
        "SEQUENCE:0",
        "LOCATION:Room 1",
    ] {
        assert!(m.calendar.contains(needle), "{needle} in\n{}", m.calendar);
    }
    assert!(!m.calendar.contains("SCHEDULE-STATUS"), "server parameters stay out of iTIP messages");
    let token = m.token();
    for r in ["accepted", "tentative", "declined"] {
        assert!(m.text.contains(&format!("/invitation/{token}?response={r}")), "{}", m.text);
    }
    no_mail(&mut rx, "Bob has an account, so he is invited inside Ferrite").await;

    // Bob finds it in his calendar, with a notification, and accepts.
    let bev = bob.event("Planning").await;
    assert_eq!(bev["my_partstat"], "NEEDS-ACTION");
    assert_eq!(bev["can_edit"], false);
    assert!(bob.notifications().await.iter().any(|n| n == "Alice invited you to “Planning”"));
    let (st, _) = bob
        .call(
            Method::PATCH,
            &format!("/api/calendar/events/{}", bev["id"].as_str().unwrap()),
            Some(json!({"summary": "Mine", "start": "2026-11-05T09:00:00Z"})),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "attendees cannot reschedule the organizer's event");
    let answered = bob
        .post(
            &format!("/api/calendar/events/{}/respond", bev["id"].as_str().unwrap()),
            json!({"response": "accepted"}),
        )
        .await;
    assert_eq!(answered["event"]["my_partstat"], "ACCEPTED");
    assert_eq!(attendee(&alice.event("Planning").await, "bob@example.org")["partstat"], "ACCEPTED");
    assert!(alice.notifications().await.iter().any(|n| n == "Bob accepted “Planning”"));
    no_mail(&mut rx, "internal answers send no mail").await;

    // Carol answers on the public page, without an account.
    let anon = reqwest::Client::new();
    let page: Value = anon
        .get(format!("{}/api/invitation/{token}", srv.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["invitation"]["summary"], "Planning");
    assert_eq!(page["invitation"]["attendee"]["email"], "carol@example.net");
    assert_eq!(page["invitation"]["organizer"]["name"], "Alice");
    let r = anon
        .post(format!("{}/api/invitation/{token}", srv.url))
        .json(&json!({"response": "tentative"}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    assert_eq!(attendee(&alice.event("Planning").await, "carol@example.net")["partstat"], "TENTATIVE");
    assert!(alice.notifications().await.iter().any(|n| n == "Carol Chen tentatively accepted “Planning”"));
    let forged = format!("{}x", &token[..token.len() - 1]);
    let r = anon.get(format!("{}/api/invitation/{forged}", srv.url)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND, "tokens are signed");
    no_mail(&mut rx, "answers are recorded, not mailed").await;

    // A notes-only change: Bob's copy follows silently, nobody is mailed.
    let attendees = json!(["bob@example.org", "Carol Chen <carol@example.net>"]);
    alice
        .patch(
            &format!("/api/calendar/events/{id}"),
            json!({"summary": "Planning", "start": "2026-11-05T09:00:00Z", "end": "2026-11-05T10:00:00Z", "location": "Room 1", "description": "Agenda attached", "attendees": attendees}),
        )
        .await;
    no_mail(&mut rx, "a description change is not significant").await;
    let bev = bob.event("Planning").await;
    assert_eq!(bev["description"], "Agenda attached");
    assert_eq!(bev["my_partstat"], "ACCEPTED", "not a reschedule: answers stand");

    // Rescheduling: a new SEQUENCE, answers reset, everyone told once.
    alice
        .patch(
            &format!("/api/calendar/events/{id}"),
            json!({"summary": "Planning", "start": "2026-11-06T13:00:00Z", "end": "2026-11-06T14:00:00Z", "location": "Room 1", "description": "Agenda attached", "attendees": attendees}),
        )
        .await;
    let m = mail(&mut rx).await;
    assert_eq!(m.to, vec!["carol@example.net"]);
    assert!(m.calendar.contains("METHOD:REQUEST") && m.calendar.contains("DTSTART:20261106T130000Z"));
    let seq = m.calendar.lines().find(|l| l.starts_with("SEQUENCE:")).unwrap().to_string();
    assert_ne!(seq, "SEQUENCE:0", "the sequence is raised on a reschedule");
    assert!(m.text.contains("updated the event"));
    no_mail(&mut rx, "one update per attendee").await;
    let bev = bob.event("Planning").await;
    assert_eq!(bev["starts_at"], "2026-11-06T13:00:00Z");
    assert_eq!(bev["my_partstat"], "NEEDS-ACTION", "a reschedule asks again");
    assert!(bob.notifications().await.iter().any(|n| n == "Alice updated “Planning”"));

    // Removing Carol cancels for her only; her link stops working.
    alice
        .patch(
            &format!("/api/calendar/events/{id}"),
            json!({"summary": "Planning", "start": "2026-11-06T13:00:00Z", "end": "2026-11-06T14:00:00Z", "location": "Room 1", "attendees": ["bob@example.org"]}),
        )
        .await;
    let m = mail(&mut rx).await;
    assert_eq!(m.to, vec!["carol@example.net"]);
    assert!(m.calendar_type.contains("method=CANCEL"));
    assert!(m.calendar.contains("METHOD:CANCEL") && m.calendar.contains("mailto:carol@example.net"));
    assert!(
        !m.calendar.contains("STATUS:CANCELLED") && !m.calendar.contains("bob@example.org"),
        "{}",
        m.calendar
    );
    no_mail(&mut rx, "Bob is not mailed").await;
    let r = anon.get(format!("{}/api/invitation/{token}", srv.url)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);

    // Deleting the event cancels it for Bob: his copy says so and he is notified.
    let (st, _) = alice.call(Method::DELETE, &format!("/api/calendar/events/{id}"), None).await;
    assert!(st.is_success());
    let bev = bob.event("Planning").await;
    assert_eq!(bev["cancelled"], true);
    assert!(bob.notifications().await.iter().any(|n| n == "Alice cancelled “Planning”"));
    no_mail(&mut rx, "Carol was already removed").await;
    // Removing the cancelled copy sends no answer back.
    let (st, _) = bob
        .call(Method::DELETE, &format!("/api/calendar/events/{}", bev["id"].as_str().unwrap()), None)
        .await;
    assert!(st.is_success());
    assert!(!alice.notifications().await.iter().any(|n| n.contains("declined")));

    // MCP: invite by tool, answer by tool.
    let tok = alice.post("/api/me/tokens", json!({"name": "ai", "scopes": ["groupware", "mcp"]})).await["secret"]
        .as_str()
        .unwrap()
        .to_string();
    let bob_tok = bob.post("/api/me/tokens", json!({"name": "ai", "scopes": ["groupware", "mcp"]})).await["secret"]
        .as_str()
        .unwrap()
        .to_string();
    let c = mcp_call(
        &srv.url,
        &tok,
        json!({"name": "calendar_create_event", "arguments": {"summary": "Retro", "start": "2026-11-12T15:00:00Z", "attendees": ["bob@example.org", "erin@example.net"]}}),
    )
    .await;
    assert_eq!(c["result"]["structuredContent"]["event"]["attendees"].as_array().unwrap().len(), 2, "{c}");
    let m = mail(&mut rx).await;
    assert_eq!(m.to, vec!["erin@example.net"]);
    no_mail(&mut rx, "MCP-created events are scheduled once").await;
    let retro = bob.event("Retro").await;
    let r = mcp_call(
        &srv.url,
        &bob_tok,
        json!({"name": "calendar_respond", "arguments": {"event_id": retro["id"], "response": "decline"}}),
    )
    .await;
    assert_eq!(r["result"]["structuredContent"]["event"]["my_partstat"], "DECLINED", "{r}");
    assert_eq!(attendee(&alice.event("Retro").await, "bob@example.org")["partstat"], "DECLINED");
    assert!(alice.notifications().await.iter().any(|n| n == "Bob declined “Retro”"));
}

async fn mcp_call(url: &str, token: &str, params: Value) -> Value {
    let mut p = params;
    p["_meta"] = json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientCapabilities": {}});
    let name = p["name"].as_str().unwrap().to_string();
    reqwest::Client::new()
        .post(format!("{url}/mcp"))
        .bearer_auth(token)
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "tools/call")
        .header("Mcp-Name", ferrite_mcp::client::encode_header_value(&name))
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": p}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

struct Dav {
    http: reqwest::Client,
    url: String,
    user: String,
    password: String,
}

impl Dav {
    fn new(url: &str, user: &str, password: &str) -> Dav {
        Dav {
            http: reqwest::Client::new(),
            url: url.to_string(),
            user: user.to_string(),
            password: password.to_string(),
        }
    }
    fn req(&self, method: &str, path: &str) -> reqwest::RequestBuilder {
        let auth = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", self.user, self.password));
        self.http
            .request(Method::from_bytes(method.as_bytes()).unwrap(), format!("{}{path}", self.url))
            .header("Authorization", format!("Basic {auth}"))
    }
    async fn put(&self, path: &str, ics: &str) -> reqwest::Response {
        self.req("PUT", path)
            .header("Content-Type", "text/calendar")
            .body(ics.to_string())
            .send()
            .await
            .unwrap()
    }
    async fn text(&self, method: &str, path: &str, depth: &str, body: &str) -> (StatusCode, String) {
        let r = self.req(method, path).header("Depth", depth).body(body.to_string()).send().await.unwrap();
        (r.status(), r.text().await.unwrap())
    }
}

fn ics(lines: &[&str]) -> String {
    let mut s = lines.join("\r\n");
    s.push_str("\r\n");
    s
}

const SERIES: &[&str] = &[
    "BEGIN:VCALENDAR",
    "VERSION:2.0",
    "PRODID:-//Test Client//EN",
    "BEGIN:VEVENT",
    "UID:standup-1",
    "DTSTAMP:20261101T080000Z",
    "SEQUENCE:0",
    "DTSTART;TZID=Europe/Berlin:20261102T090000",
    "DTEND;TZID=Europe/Berlin:20261102T091500",
    "RRULE:FREQ=WEEKLY;COUNT=10",
    "SUMMARY:Standup",
    "ORGANIZER;CN=Alice:mailto:alice@example.org",
    "ATTENDEE;CN=Bob;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:bob@example.org",
    "ATTENDEE;CN=Carol;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:carol@example.net",
    "ATTENDEE;CN=Dan;SCHEDULE-AGENT=CLIENT:mailto:dan@example.net",
    "END:VEVENT",
    "END:VCALENDAR",
];

#[tokio::test]
async fn caldav_clients_are_scheduled_once_and_attendees_reply_through_caldav() {
    let mut srv = start(true).await;
    let mut rx = srv.mail.take().unwrap();
    let (alice, bob, _dave) = people(&srv.url).await;
    let adav = Dav::new(&srv.url, "alice", "alice-password");
    let bdav = Dav::new(&srv.url, "bob", "bob-password");

    // Discovery says the server schedules.
    let r = adav.req("OPTIONS", "/remote.php/dav/calendars/alice/personal/").send().await.unwrap();
    assert!(r.headers()["dav"].to_str().unwrap().contains("calendar-auto-schedule"));
    let (_, principal) = adav
        .text(
            "PROPFIND",
            "/remote.php/dav/principals/users/alice/",
            "0",
            "<d:propfind xmlns:d=\"DAV:\"><d:allprop/></d:propfind>",
        )
        .await;
    assert!(
        principal.contains("<cal:schedule-inbox-URL><d:href>/remote.php/dav/calendars/alice/inbox/</d:href>"),
        "{principal}"
    );
    assert!(principal.contains("<cal:schedule-outbox-URL>") && principal.contains("mailto:alice@example.org"));
    let (_, home) = adav.text("PROPFIND", "/remote.php/dav/calendars/alice/", "1", "").await;
    assert!(
        home.contains("<cal:schedule-inbox/>")
            && home.contains("<cal:schedule-default-calendar-URL><d:href>/remote.php/dav/calendars/alice/personal/</d:href>")
    );

    // A client (Apple Calendar, Thunderbird) saves a recurring meeting with attendees.
    let path = "/remote.php/dav/calendars/alice/personal/standup-1.ics";
    let r = adav.put(path, &ics(SERIES)).await;
    assert_eq!(r.status(), StatusCode::CREATED);
    assert!(r.headers().get("etag").is_none(), "the server added scheduling status, so no ETag");
    let m = mail(&mut rx).await;
    assert_eq!(m.to, vec!["carol@example.net"]);
    assert!(m.calendar.contains("RRULE:FREQ=WEEKLY;COUNT=10") && m.calendar.contains("DTSTART;TZID=Europe/Berlin:20261102T090000"));
    assert!(m.calendar.contains("SEQUENCE:0") && !m.calendar.contains("SCHEDULE-AGENT"));
    no_mail(&mut rx, "Dan is scheduled by the client (SCHEDULE-AGENT=CLIENT), Bob internally").await;
    let r = adav.req("GET", path).send().await.unwrap();
    let etag = r.headers()["etag"].to_str().unwrap().to_string();
    let stored = r.text().await.unwrap().replace("\r\n ", "");
    assert!(
        stored.contains("SCHEDULE-STATUS=1.2:mailto:bob@example.org") && stored.contains("SCHEDULE-STATUS=1.1:mailto:carol@example.net"),
        "{stored}"
    );
    assert!(stored.contains("ATTENDEE;CN=Dan;SCHEDULE-AGENT=CLIENT:mailto:dan@example.net"));

    // Clients write the event back unchanged, or with a new alarm: nobody hears about it.
    let r = adav.req("PUT", path).header("If-Match", &etag).body(stored.clone()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    let with_alarm = stored.replace("END:VEVENT", "BEGIN:VALARM\r\nTRIGGER:-PT5M\r\nACTION:DISPLAY\r\nEND:VALARM\r\nEND:VEVENT");
    assert_eq!(adav.put(path, &with_alarm).await.status(), StatusCode::NO_CONTENT);
    no_mail(&mut rx, "re-saving is not a change").await;
    assert_eq!(
        bob.notifications().await.iter().filter(|n| n.contains("Standup")).count(),
        1,
        "Bob was invited once"
    );

    // Bob's copy is in his calendar (as the series), and he accepts from his CalDAV client.
    let (_, list) = bdav.text("PROPFIND", "/remote.php/dav/calendars/bob/personal/", "1", "").await;
    assert!(list.contains("/remote.php/dav/calendars/bob/personal/standup-1.ics"), "{list}");
    let bpath = "/remote.php/dav/calendars/bob/personal/standup-1.ics";
    let copy = bdav.req("GET", bpath).send().await.unwrap().text().await.unwrap().replace("\r\n ", "");
    assert!(copy.contains("RRULE:FREQ=WEEKLY;COUNT=10") && copy.contains("PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:bob@example.org"));
    assert!(!copy.contains("TRIGGER:-PT5M"), "the organizer's alarms stay hers");
    let accepted = copy.replace(
        "PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:bob@example.org",
        "PARTSTAT=ACCEPTED:mailto:bob@example.org",
    );
    assert_eq!(bdav.put(bpath, &accepted).await.status(), StatusCode::NO_CONTENT);
    let standup = alice.event("Standup").await;
    assert_eq!(attendee(&standup, "bob@example.org")["partstat"], "ACCEPTED");
    assert_eq!(standup["recurring"], true);
    assert_eq!(
        alice.events().await.iter().filter(|e| e["summary"] == "Standup").count(),
        10,
        "the answer covers the series"
    );
    assert!(alice.notifications().await.iter().any(|n| n == "Bob accepted “Standup”"));
    no_mail(&mut rx, "an internal reply").await;

    // Moving the event to another calendar (PUT there, then DELETE here) schedules nothing.
    let r = adav
        .req("MKCALENDAR", "/remote.php/dav/calendars/alice/work/")
        .body("<c:mkcalendar xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\"><d:set><d:prop><d:displayname>Work</d:displayname></d:prop></d:set></c:mkcalendar>")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    let current = adav.req("GET", path).send().await.unwrap().text().await.unwrap();
    assert!(
        adav.put("/remote.php/dav/calendars/alice/work/standup-1.ics", &current)
            .await
            .status()
            .is_success()
    );
    assert_eq!(adav.req("DELETE", path).send().await.unwrap().status(), StatusCode::NO_CONTENT);
    no_mail(&mut rx, "a move is not a new invitation or a cancellation").await;
    assert_eq!(bob.event("Standup").await["cancelled"], false);

    // Bob deletes his copy in his client: that declines.
    assert_eq!(bdav.req("DELETE", bpath).send().await.unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(attendee(&alice.event("Standup").await, "bob@example.org")["partstat"], "DECLINED");
    assert!(alice.notifications().await.iter().any(|n| n == "Bob declined “Standup”"));

    // An organizer's client cancelling one attendee by removing them, then the whole event.
    let wpath = "/remote.php/dav/calendars/alice/work/standup-1.ics";
    let cur = adav.req("GET", wpath).send().await.unwrap().text().await.unwrap().replace("\r\n ", "");
    let without_carol: String = cur.lines().filter(|l| !l.contains("carol@")).map(|l| format!("{l}\r\n")).collect();
    assert_eq!(adav.put(wpath, &without_carol).await.status(), StatusCode::NO_CONTENT);
    let m = mail(&mut rx).await;
    assert!(m.calendar.contains("METHOD:CANCEL") && m.calendar.contains("SEQUENCE:1"), "{}", m.calendar);
    assert_eq!(adav.req("DELETE", wpath).send().await.unwrap().status(), StatusCode::NO_CONTENT);
    no_mail(&mut rx, "nobody external is left").await;

    // Free/busy through the outbox.
    bob.post(
        "/api/calendar/events",
        json!({"summary": "Dentist", "start": "2026-11-20T10:00:00Z", "end": "2026-11-20T11:00:00Z"}),
    )
    .await;
    let fb = ics(&[
        "BEGIN:VCALENDAR",
        "VERSION:2.0",
        "PRODID:-//Test Client//EN",
        "METHOD:REQUEST",
        "BEGIN:VFREEBUSY",
        "UID:fb-1",
        "DTSTAMP:20261101T080000Z",
        "DTSTART:20261120T000000Z",
        "DTEND:20261121T000000Z",
        "ORGANIZER:mailto:alice@example.org",
        "ATTENDEE:mailto:bob@example.org",
        "ATTENDEE:mailto:nobody@example.net",
        "END:VFREEBUSY",
        "END:VCALENDAR",
    ]);
    let r = adav
        .req("POST", "/remote.php/dav/calendars/alice/outbox/")
        .header("Content-Type", "text/calendar")
        .body(fb)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let xml = r.text().await.unwrap();
    assert!(xml.contains("FREEBUSY;FBTYPE=BUSY:20261120T100000Z/20261120T110000Z"), "{xml}");
    assert!(xml.contains("3.7;Invalid calendar user"));
}

async fn ws_events(url: &str, user: &str, password: &str) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let b = Browser::login(url, user, password).await;
    let tok = b.post("/api/me/tokens", json!({"name": "ws", "scopes": ["*"]})).await["secret"]
        .as_str()
        .unwrap()
        .to_string();
    let ws_url = format!("{}/api/ws?token={tok}", url.replace("http://", "ws://"));
    let (ws, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();
    ws
}

async fn next_of_type<S: StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin>(ws: &mut S, ty: &str) -> Value {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("ws message within 5s")
            .unwrap()
            .unwrap();
        if let WsMessage::Text(t) = m {
            let v: Value = serde_json::from_str(&t).unwrap();
            if v["type"] == ty {
                return v;
            }
        }
    }
}

#[tokio::test]
async fn shared_calendars_over_the_api_and_caldav_and_public_feeds() {
    let srv = start(false).await;
    let (alice, bob, dave) = people(&srv.url).await;
    assert_eq!(alice.get("/api/calendar/calendars").await["can_email"], false);
    let team = alice.post("/api/calendar/calendars", json!({"name": "Team", "color": "#0f766e"})).await["calendar"]["uri"]
        .as_str()
        .unwrap()
        .to_string();
    let launch = alice
        .post(
            "/api/calendar/events",
            json!({"summary": "Launch", "start": "2026-11-09T10:00:00Z", "calendar": team, "description": "Go live"}),
        )
        .await["event"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    // Only Ferrite users here and no SMTP: inviting by email is impossible, internal delivery works.
    alice
        .post(
            "/api/calendar/events",
            json!({"summary": "Lunch", "start": "2026-11-10T12:00:00Z", "calendar": team, "attendees": ["bob@example.org", "x@example.net"]}),
        )
        .await;
    assert_eq!(bob.event("Lunch").await["my_partstat"], "NEEDS-ACTION");

    // Read-only share with Bob.
    let share = alice
        .post(
            &format!("/api/calendar/calendars/{team}/shares"),
            json!({"kind": "user", "target": "bob", "writable": false}),
        )
        .await["share"]
        .clone();
    assert_eq!(share["target_name"], "Bob");
    assert!(bob.notifications().await.iter().any(|n| n == "Alice shared the calendar “Team” with you"));
    let shared_uri = format!("{team}_shared_by_alice");
    let cals = bob.get("/api/calendar/calendars").await["calendars"].as_array().unwrap().clone();
    let sc = cals.iter().find(|c| c["uri"] == shared_uri.as_str()).unwrap_or_else(|| panic!("{cals:?}"));
    assert_eq!(
        (sc["shared_by"]["display_name"].as_str(), sc["writable"].as_bool(), sc["access"].as_str()),
        (Some("Alice"), Some(false), Some("read"))
    );
    let ev = bob.events().await.into_iter().find(|e| e["summary"] == "Launch").unwrap();
    assert_eq!(
        (ev["calendar"].as_str(), ev["shared_by"].as_str(), ev["writable"].as_bool()),
        (Some(shared_uri.as_str()), Some("Alice"), Some(false))
    );
    // Bob's own colour for it; Alice's stays.
    bob.patch(&format!("/api/calendar/calendars/{shared_uri}"), json!({"color": "#7c3aed"})).await;
    let mine = bob.get("/api/calendar/calendars").await["calendars"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["uri"] == shared_uri.as_str())
        .unwrap()["color"]
        .clone();
    assert_eq!(mine, "#7c3aed");
    let theirs = alice.get("/api/calendar/calendars").await["calendars"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["uri"] == team.as_str())
        .unwrap()
        .clone();
    assert_eq!((theirs["color"].as_str(), theirs["shares"].as_i64()), (Some("#0f766e"), Some(1)));
    let (st, _) = bob
        .call(
            Method::PATCH,
            &format!("/api/calendar/events/{launch}"),
            Some(json!({"summary": "Hijack", "start": "2026-11-09T10:00:00Z"})),
        )
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, _) = bob
        .call(
            Method::POST,
            "/api/calendar/events",
            Some(json!({"summary": "No", "start": "2026-11-09T10:00:00Z", "calendar": shared_uri})),
        )
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    // CalDAV: the shared calendar is in Bob's home with read privileges only, and writes fail.
    let bdav = Dav::new(&srv.url, "bob", "bob-password");
    let (_, home) = bdav.text("PROPFIND", "/remote.php/dav/calendars/bob/", "1", "").await;
    let href = format!("/remote.php/dav/calendars/bob/{shared_uri}/");
    let entry = home
        .split("<d:response>")
        .find(|r| r.contains(&format!("<d:href>{href}</d:href>")))
        .unwrap_or_else(|| panic!("{home}"));
    assert!(entry.contains("<d:privilege><d:read/></d:privilege>") && !entry.contains("<d:write"), "{entry}");
    assert!(entry.contains("<d:owner><d:href>/remote.php/dav/principals/users/alice/</d:href>"));
    let (_, objects) = bdav.text("PROPFIND", &href, "1", "").await;
    let obj = objects
        .split("<d:href>")
        .filter_map(|p| p.split("</d:href>").next())
        .find(|h| h.ends_with(".ics"))
        .unwrap()
        .to_string();
    let r = bdav.req("GET", &obj).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let data = r.text().await.unwrap();
    let r = bdav.put(&obj, &data.replace("SUMMARY:", "SUMMARY:Changed ")).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert!(r.text().await.unwrap().contains("need-privileges"));
    assert_eq!(bdav.req("DELETE", &obj).send().await.unwrap().status(), StatusCode::FORBIDDEN);
    let new_event = ics(&[
        "BEGIN:VCALENDAR",
        "VERSION:2.0",
        "BEGIN:VEVENT",
        "UID:from-bob",
        "DTSTART:20261111T090000Z",
        "DTEND:20261111T100000Z",
        "SUMMARY:Bob's slot",
        "END:VEVENT",
        "END:VCALENDAR",
    ]);
    assert_eq!(bdav.put(&format!("{href}from-bob.ics"), &new_event).await.status(), StatusCode::FORBIDDEN);

    // Read-write: Bob adds an event through CalDAV; Alice sees it live and attributed to Bob.
    alice
        .patch(
            &format!("/api/calendar/calendars/{team}/shares/{}", share["id"].as_str().unwrap()),
            json!({"writable": true}),
        )
        .await;
    let (_, home) = bdav.text("PROPFIND", "/remote.php/dav/calendars/bob/", "1", "").await;
    let entry = home.split("<d:response>").find(|r| r.contains(&format!("<d:href>{href}</d:href>"))).unwrap();
    assert!(
        entry.contains("<d:privilege><d:write-content/></d:privilege>") && !entry.contains("<d:all/>"),
        "{entry}"
    );
    let mut alice_ws = ws_events(&srv.url, "alice", "alice-password").await;
    assert_eq!(bdav.put(&format!("{href}from-bob.ics"), &new_event).await.status(), StatusCode::CREATED);
    assert_eq!(next_of_type(&mut alice_ws, "groupware.changed").await["kind"], "calendar");
    let slot = alice.event("Bob's slot").await;
    assert_eq!((slot["calendar"].as_str(), slot["created_by"].as_str()), (Some(team.as_str()), Some("Bob")));
    assert!(
        alice.get("/api/activity").await["activity"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["summary"].as_str().unwrap().contains("Bob added “Bob's slot”"))
    );
    // …and edits Alice's event from the web app.
    let edited = bob
        .patch(
            &format!("/api/calendar/events/{launch}"),
            json!({"summary": "Launch (moved)", "start": "2026-11-09T11:00:00Z"}),
        )
        .await;
    assert_eq!(edited["event"]["calendar"], shared_uri.as_str());
    assert_eq!(alice.event("Launch (moved)").await["starts_at"], "2026-11-09T11:00:00Z");

    // Group shares reach every member.
    let group = alice.post("/api/admin/groups", json!({"name": "Ops"})).await;
    alice
        .post(
            &format!("/api/admin/groups/{}/members", group["group"]["id"].as_str().unwrap()),
            json!({"username": "dave"}),
        )
        .await;
    let targets = dave.get("/api/calendar/share-targets?q=op").await;
    assert_eq!(targets["groups"][0]["name"], "Ops");
    let mcp_tok = alice.post("/api/me/tokens", json!({"name": "ai", "scopes": ["groupware", "mcp"]})).await["secret"]
        .as_str()
        .unwrap()
        .to_string();
    let r = mcp_call(
        &srv.url,
        &mcp_tok,
        json!({"name": "calendar_share", "arguments": {"calendar": "Team", "with": "Ops", "kind": "group"}}),
    )
    .await;
    assert_eq!(r["result"]["structuredContent"]["share"]["kind"], "group", "{r}");
    assert!(dave.events().await.iter().any(|e| e["summary"] == "Launch (moved)"));
    let r = mcp_call(
        &srv.url,
        &mcp_tok,
        json!({"name": "calendar_unshare", "arguments": {"calendar": "Team", "with": "Ops", "kind": "group"}}),
    )
    .await;
    assert!(r["result"]["isError"] != true, "{r}");
    assert!(!dave.events().await.iter().any(|e| e["summary"] == "Launch (moved)"));

    // Bob removes the calendar from his list.
    let (st, _) = bob.call(Method::DELETE, &format!("/api/calendar/calendars/{shared_uri}"), None).await;
    assert!(st.is_success());
    assert!(!bob.events().await.iter().any(|e| e["summary"] == "Launch (moved)"));
    assert!(
        alice.events().await.iter().any(|e| e["summary"] == "Launch (moved)"),
        "Alice's calendar is untouched"
    );

    // Public read-only feed, then revoked.
    let url = alice.post(&format!("/api/calendar/calendars/{team}/publish"), json!({})).await["public_url"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(url.starts_with(&format!("{}/remote.php/dav/public-calendars/", srv.url)) && url.len() > 60);
    let anon = reqwest::Client::new();
    let r = anon.get(&url).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert!(r.headers()["content-type"].to_str().unwrap().starts_with("text/calendar"));
    let feed = r.text().await.unwrap();
    assert_eq!(feed.matches("BEGIN:VCALENDAR").count(), 1);
    assert!(feed.contains("SUMMARY:Launch (moved)") && feed.contains("SUMMARY:Bob's slot") && feed.contains("X-WR-CALNAME:Team"));
    assert!(!feed.contains("mailto:"), "no addresses in public feeds");
    alice.call(Method::DELETE, &format!("/api/calendar/calendars/{team}/publish"), None).await;
    assert_eq!(anon.get(&url).send().await.unwrap().status(), StatusCode::NOT_FOUND);
}
