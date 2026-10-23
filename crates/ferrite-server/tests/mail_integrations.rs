//! Mail integrations against a REAL Dovecot with Sieve. Skipped (passes trivially) unless
//! configured:
//!
//!   MAIL_TEST_DIR=/tmp/ferrite-mail-integrations IMAP_PORT=3341 LMTP_PORT=3342 AUTH_PORT=3343 SIEVE_PORT=3344 \
//!     POSTFIX=0 SIEVE=1 EXTRA_USERS="carol" bash e2e/mail-server.sh start /tmp/ferrite-mail-integrations
//!   FERRITE_TEST_IMAP=127.0.0.1:3341:alice@example.test:alice-mail-pw FERRITE_TEST_SIEVE=127.0.0.1:3344 \
//!   FERRITE_TEST_MAIL_DIR=/tmp/ferrite-mail-integrations cargo test -p ferrite-server --test mail_integrations -- --nocapture
//!
//! The mailbox and its Sieve scripts are EMPTIED. Mail is delivered with `mail-server.sh
//! deliver` (Dovecot's delivery agent, so Sieve really runs) or appended over IMAP; what
//! Sieve sends (vacation replies) is read from `$FERRITE_TEST_MAIL_DIR/outgoing`. Mail Ferrite
//! sends goes to an in-process SMTP sink; the AI is a mock OpenAI-compatible server.
//!
//! Covered: ManageSieve settings (tested on save), the raw script with server-side errors,
//! visual filters that really file and tag delivered mail while hand-written rules survive,
//! the autoresponder (not before its start, replies once per sender), the user's absence
//! driving it, invitations from Thunderbird/Outlook/Google fixtures (card with conflicts,
//! accept with the REPLY mailed through the account, automatic update and cancellation,
//! tentative entries), a Ferrite-organized event answered by an external guest's mail client
//! (PARTSTAT updated through scheduling, stale and spoofed replies ignored), a proposed new
//! time accepted, .ics import, reservations, contacts, events and tasks from messages,
//! one-click and mailto unsubscribe, AI summary/replies/translation/event/follow-ups with
//! caching, and the MCP tools.

use axum::Json;
use axum::routing::post;
use ferrite_core::mail::ServerSettings;
use ferrite_core::mail::imap::ImapConn;
use ferrite_core::mail::managesieve::SieveConn;
use ferrite_core::mailer::{SmtpSecurity, SmtpSettings};
use ferrite_core::{Config, Core};
use ferrite_server::AppState;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

struct Target {
    host: String,
    port: u16,
    user: String,
    pass: String,
    sieve_port: u16,
    dir: PathBuf,
}

fn target() -> Option<Target> {
    let v = std::env::var("FERRITE_TEST_IMAP").ok()?;
    let mut parts = v.splitn(4, ':');
    let host = parts.next()?.to_string();
    let port = parts.next()?.parse().ok()?;
    let user = parts.next()?.to_string();
    let pass = parts.next()?.to_string();
    let sieve_port = std::env::var("FERRITE_TEST_SIEVE").ok()?.rsplit_once(':')?.1.parse().ok()?;
    let dir = PathBuf::from(std::env::var("FERRITE_TEST_MAIL_DIR").ok()?);
    Some(Target {
        host,
        port,
        user,
        pass,
        sieve_port,
        dir,
    })
}

impl Target {
    fn settings(&self, port: u16) -> ServerSettings {
        ServerSettings {
            host: self.host.clone(),
            port,
            security: SmtpSecurity::None,
            username: self.user.clone(),
            password: self.pass.clone(),
        }
    }
}

struct Browser {
    http: reqwest::Client,
    url: String,
}

impl Browser {
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
    async fn send(&self, method: Method, path: &str, body: Value) -> Value {
        let (s, v) = self.call(method.clone(), path, Some(body)).await;
        assert!(s.is_success(), "{method} {path}: {s} {v}");
        v
    }
    async fn post(&self, path: &str, body: Value) -> Value {
        self.send(Method::POST, path, body).await
    }
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/imip").join(name)).unwrap()
}

/// Fills `{KEY}` placeholders and turns the text into a CRLF message.
fn fill(template: &str, vars: &[(&str, &str)]) -> Vec<u8> {
    let mut s = template.to_string();
    for (k, v) in vars {
        s = s.replace(&format!("{{{k}}}"), v);
    }
    assert!(!s.contains("{UID}") && !s.contains("{SEQ}"), "unfilled placeholder in {s}");
    s.replace("\r\n", "\n").replace('\n', "\r\n").into_bytes()
}

fn plain(from: &str, subject: &str, id: &str, extra: &str, body: &str) -> Vec<u8> {
    format!(
        "From: {from}\r\nTo: Alice <alice@example.test>\r\nSubject: {subject}\r\nDate: Thu, 22 Oct 2026 09:00:00 +0000\r\nMessage-ID: <{id}>\r\nMIME-Version: 1.0\r\n{extra}Content-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n"
    )
    .into_bytes()
}

async fn wait_for<F: std::future::Future<Output = bool>>(what: &str, mut f: impl FnMut() -> F) {
    for _ in 0..150 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Empties the mailbox, removes folders a previous run made and every Sieve script.
async fn reset(t: &Target) {
    let mut c = ImapConn::connect(&t.settings(t.port)).await.expect("IMAP login with FERRITE_TEST_IMAP");
    for f in c.list().await.unwrap() {
        if ["Reports", "Archive"].contains(&f.name.as_str()) {
            c.delete(&f.name).await.unwrap();
            continue;
        }
        if !f.selectable {
            continue;
        }
        let sel = c.select(&f.name).await.unwrap();
        if sel.exists > 0 {
            c.uid_store("1:*", "+FLAGS.SILENT (\\Deleted)").await.unwrap();
            c.expunge("1:*").await.unwrap();
        }
    }
    c.create("Reports").await.unwrap();
    c.logout().await;
    let mut s = SieveConn::connect(&t.settings(t.sieve_port)).await.expect("ManageSieve login");
    s.set_active("").await.unwrap();
    for script in s.list_scripts().await.unwrap() {
        s.delete_script(&script.name).await.unwrap();
    }
    s.logout().await;
    // Sieve remembers whom vacation answered; start from nobody.
    let _ = std::fs::remove_file(t.dir.join("mail/example.test/alice/.dovecot.lda-dupes"));
    let out = t.dir.join("outgoing");
    for e in std::fs::read_dir(&out).unwrap().flatten() {
        let _ = std::fs::remove_file(e.path());
    }
}

/// Delivers a message as mail arriving from outside (Dovecot's LDA: Sieve runs).
fn deliver(t: &Target, raw: &[u8]) {
    let file = t.dir.join("incoming.eml");
    std::fs::write(&file, raw).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../e2e/mail-server.sh");
    let out = std::process::Command::new("bash")
        .arg(script)
        .args(["deliver", "alice"])
        .arg(&file)
        .arg(&t.dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "deliver: {}", String::from_utf8_lossy(&out.stderr));
}

/// Vacation replies Sieve sent (captured by mail-server.sh).
fn outgoing(t: &Target) -> Vec<String> {
    let mut v: Vec<(PathBuf, String)> = std::fs::read_dir(t.dir.join("outgoing"))
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "eml"))
        .map(|e| (e.path(), std::fs::read_to_string(e.path()).unwrap()))
        .collect();
    v.sort();
    v.into_iter().map(|(_, s)| s).collect()
}

async fn append(t: &Target, folder: &str, raw: &[u8]) {
    let mut c = ImapConn::connect(&t.settings(t.port)).await.unwrap();
    c.append(folder, "()", raw).await.unwrap();
    c.logout().await;
}

/// The mock model: answers by what the prompt asks; counts calls.
async fn mock_llm(calls: Arc<AtomicUsize>) -> String {
    let handler = move |Json(body): Json<Value>| {
        let calls = calls.clone();
        async move {
            calls.fetch_add(1, Ordering::SeqCst);
            let prompt = body["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let answer = if prompt.contains("Summarize the email conversation") {
                "The team agreed to ship on Friday; Bob prepares the release notes.".to_string()
            } else if prompt.contains("Suggest three very short replies") {
                r#"{"reply1": "Sounds good!", "reply2": "Can we talk tomorrow?", "reply3": "Thanks, ___"}"#.to_string()
            } else if prompt.contains("Translate the e-mail") {
                r#"{"subject": "Freigabe am Freitag", "text": "Wir veröffentlichen am Freitag."}"#.to_string()
            } else if prompt.contains("I am scheduling an event") {
                r#"Here you go: {"title": "Release planning", "agenda": "* Scope\n* Dates", "start": "2027-03-15T14:00:00+01:00", "end": "2027-03-15T15:00:00+01:00"}"#.to_string()
            } else if prompt.contains("expect a reply") {
                if prompt.contains("Could you") {
                    r#"{"expectsReply": true}"#
                } else {
                    r#"{"expectsReply": false}"#
                }
                .to_string()
            } else {
                "?".to_string()
            };
            Json(json!({"choices": [{"finish_reason": "stop", "message": {"role": "assistant", "content": answer}}]}))
        }
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, axum::Router::new().route("/v1/chat/completions", post(handler)))
            .await
            .unwrap()
    });
    url
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mail_integrations_against_a_real_server() {
    let Some(t) = target() else {
        eprintln!("FERRITE_TEST_IMAP/FERRITE_TEST_SIEVE/FERRITE_TEST_MAIL_DIR are not set; skipping (see the top of tests/mail_integrations.rs)");
        return;
    };
    let _ = tracing_subscriber::fmt().with_env_filter("warn,ferrite=info").with_test_writer().try_init();
    // One-click unsubscribe may reach the local mock list server in this test.
    unsafe { std::env::set_var("FERRITE_MAIL_ALLOW_PRIVATE_FETCH", "1") };
    reset(&t).await;
    append(
        &t,
        "INBOX",
        &plain("Bob <bob@example.org>", "Welcome", "welcome@example.org", "", "Hello Alice."),
    )
    .await;

    // Ferrite, with the server's own mail and the account's SMTP going to one sink.
    let (sink_port, mut sink) = ferrite_core::mailer::sink::sink().await;
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let mut cfg = Config {
        data_dir: dir.path().to_path_buf(),
        public_url: url.clone(),
        ..Config::default()
    };
    cfg.sfu.enabled = false;
    cfg.smtp = Some(SmtpSettings {
        host: "127.0.0.1".into(),
        port: sink_port,
        security: SmtpSecurity::None,
        username: None,
        password: None,
        from: "Ferrite <noreply@ferrite.test>".into(),
    });
    let core = Core::open(cfg).await.unwrap();
    let state = AppState::new(core.clone());
    state.start_background();
    tokio::spawn(axum::serve(listener, ferrite_server::router(state.clone())).into_future());
    let b = Browser {
        http: reqwest::Client::builder().cookie_store(true).build().unwrap(),
        url: url.clone(),
    };
    let setup = b
        .post(
            "/api/setup",
            json!({"username": "alice", "password": "alice-password", "display_name": "Alice"}),
        )
        .await;
    let user_id = setup["user"]["id"].as_str().unwrap().to_string();
    b.send(Method::PATCH, "/api/me", json!({"email": "alice@example.test"})).await;
    let acc = b
        .post(
            "/api/mail/accounts",
            json!({"email": t.user, "name": "Alice", "password": t.pass, "imap_host": t.host, "imap_port": t.port, "imap_security": "none",
                   "smtp_host": "127.0.0.1", "smtp_port": sink_port, "smtp_security": "none", "smtp_auth": false}),
        )
        .await["account"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let folders = b.get(&format!("/api/mail/accounts/{acc}/folders")).await["folders"].clone();
    let folder = |name: &str| {
        folders
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == name)
            .unwrap_or_else(|| panic!("no folder {name}"))["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let inbox = folder("INBOX");
    assert_eq!(
        b.get(&format!("/api/mail/folders/{inbox}/messages")).await["messages"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let sync_inbox = || {
        let b = &b;
        let (acc, inbox) = (acc.clone(), inbox.clone());
        async move {
            b.post(&format!("/api/mail/accounts/{acc}/sync"), json!({"folder_id": inbox})).await;
        }
    };
    let find = |subject: &'static str| {
        let b = &b;
        let inbox = inbox.clone();
        async move {
            b.get(&format!("/api/mail/folders/{inbox}/messages")).await["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["subject"] == subject)
                .map(|m| m["id"].as_str().unwrap().to_string())
        }
    };

    // ---- Sieve settings ------------------------------------------------------------------
    let (s, v) = b.call(Method::GET, &format!("/api/mail/accounts/{acc}/filters"), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "filters need Sieve turned on: {v}");
    let i = b.get(&format!("/api/mail/accounts/{acc}/integrations")).await["integrations"].clone();
    assert_eq!((i["sieve_enabled"].as_bool(), i["sieve_security"].as_str()), (Some(false), Some("none")));
    let (s, v) = b
        .call(
            Method::PATCH,
            &format!("/api/mail/accounts/{acc}/integrations"),
            Some(json!({"sieve_enabled": true, "sieve_port": t.sieve_port, "sieve_password": "wrong"})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a wrong password is caught when saving: {v}");
    assert!(v["error"].as_str().unwrap().contains("ManageSieve login failed"), "{v}");
    let i = b
        .send(
            Method::PATCH,
            &format!("/api/mail/accounts/{acc}/integrations"),
            json!({"sieve_enabled": true, "sieve_port": t.sieve_port, "sieve_password": ""}),
        )
        .await["integrations"]
        .clone();
    assert_eq!((i["sieve_enabled"].as_bool(), i["sieve_custom_password"].as_bool()), (Some(true), Some(false)));
    assert!(!i.to_string().contains(&t.pass));

    // ---- raw script ------------------------------------------------------------------------
    let raw = b.get(&format!("/api/mail/accounts/{acc}/sieve")).await;
    assert_eq!((raw["name"].clone(), raw["script"].as_str()), (Value::Null, Some("")));
    assert!(raw["extensions"].as_array().unwrap().iter().any(|e| e == "vacation"));
    let (s, v) = b
        .call(
            Method::PUT,
            &format!("/api/mail/accounts/{acc}/sieve"),
            Some(json!({"script": "require [\"fileinto\"];\nfileintoo \"Junk\";\n"})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("line 2"),
        "the server's CHECKSCRIPT error names the line: {v}"
    );
    let handwritten = "require [\"fileinto\"];\r\n# my own rule\r\nif header :contains \"X-Test\" \"hand\" {\r\n\tfileinto \"Junk\";\r\n}\r\n";
    let saved = b
        .send(Method::PUT, &format!("/api/mail/accounts/{acc}/sieve"), json!({"script": handwritten}))
        .await;
    assert_eq!(saved["name"], "ferrite", "a script is created and activated");
    let mut sc = SieveConn::connect(&t.settings(t.sieve_port)).await.unwrap();
    assert_eq!(sc.active_script().await.unwrap().as_deref(), Some("ferrite"));
    sc.logout().await;

    // ---- visual filters --------------------------------------------------------------------
    let filters = json!([
        {"name": "Reports", "operator": "allof", "tests": [{"field": "from", "operator": "contains", "values": ["reports@"]}],
         "actions": [{"type": "fileinto", "mailbox": "Reports"}]},
        {"name": "Tag urgent", "operator": "anyof", "tests": [{"field": "subject", "operator": "contains", "values": ["Urgent"]}, {"field": "to", "operator": "is", "values": ["boss@example.org"]}],
         "actions": [{"type": "addflag", "flag": "$label1"}, {"type": "stop"}]}
    ]);
    let (s, v) = b
        .call(
            Method::PUT,
            &format!("/api/mail/accounts/{acc}/filters"),
            Some(json!({"filters": [{"name": "x", "tests": [{"field": "from", "operator": "is", "values": ["a"]}], "actions": [{"type": "fileinto", "mailbox": "Nope"}]}]})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "unknown folders are refused: {v}");
    let saved = b
        .send(Method::PUT, &format!("/api/mail/accounts/{acc}/filters"), json!({"filters": filters}))
        .await;
    assert_eq!(saved["filters"][1]["priority"], 2);
    let listed = b.get(&format!("/api/mail/accounts/{acc}/filters")).await["filters"].clone();
    assert_eq!(listed.as_array().unwrap().len(), 2);
    assert_eq!(listed[0]["name"], "Reports");
    let script = b.get(&format!("/api/mail/accounts/{acc}/sieve")).await["script"].as_str().unwrap().to_string();
    assert!(
        script.contains("# my own rule") && script.contains("### Nextcloud Mail: Filters ### DON'T EDIT ###"),
        "{script}"
    );
    assert!(script.contains("fileinto \"Reports\";") && script.contains("addflag \"$label1\";"));
    // Mail really is filed and tagged on delivery, and the hand-written rule still works.
    deliver(
        &t,
        &plain("Report Bot <reports@example.org>", "Weekly numbers", "f1@example.org", "", "numbers"),
    );
    deliver(&t, &plain("Carol <carol@example.org>", "Urgent: call me", "f2@example.org", "", "please"));
    deliver(
        &t,
        &plain("Dave <dave@example.org>", "Handwritten rule", "f3@example.org", "X-Test: hand\r\n", "junk me"),
    );
    deliver(&t, &plain("Erin <erin@example.org>", "Plain mail", "f4@example.org", "", "stays"));
    let mut check = ImapConn::connect(&t.settings(t.port)).await.unwrap();
    check.select("Reports").await.unwrap();
    assert_eq!(check.uid_search("FROM reports@example.org").await.unwrap().len(), 1, "filed into Reports");
    check.select("Junk").await.unwrap();
    assert_eq!(check.uid_search("HEADER X-Test hand").await.unwrap().len(), 1, "hand-written rule kept");
    check.select("INBOX").await.unwrap();
    assert_eq!(check.uid_search("KEYWORD $label1").await.unwrap().len(), 1, "tagged");
    assert_eq!(check.uid_search("SUBJECT \"Plain mail\"").await.unwrap().len(), 1);
    assert!(check.uid_search("FROM reports@example.org").await.unwrap().is_empty());

    // ---- autoresponder ---------------------------------------------------------------------
    let v = b.get(&format!("/api/mail/accounts/{acc}/vacation")).await["vacation"].clone();
    assert_eq!((v["enabled"].as_bool(), v["subject"].as_str()), (Some(false), Some("Re: ${subject}")));
    let now = chrono::Utc::now();
    let later = |h: i64| (now + chrono::Duration::hours(h)).to_rfc3339();
    b.send(
        Method::PUT,
        &format!("/api/mail/accounts/{acc}/vacation"),
        json!({"enabled": true, "start": later(24), "end": later(72), "subject": "Away: ${subject}", "message": "I am away this week.", "days": 2}),
    )
    .await;
    deliver(&t, &plain("Sender <sender@example.org>", "Before the holidays", "v1@example.org", "", "hi"));
    assert!(outgoing(&t).is_empty(), "no reply before the start");
    let set = b
        .send(
            Method::PUT,
            &format!("/api/mail/accounts/{acc}/vacation"),
            json!({"enabled": true, "start": later(-1), "end": later(48), "subject": "Away: ${subject}", "message": "I am away this week.", "days": 2}),
        )
        .await["vacation"]
        .clone();
    assert_eq!(set["addresses"][0], "alice@example.test");
    assert_eq!(set["days"], 2);
    deliver(&t, &plain("Sender <sender@example.org>", "Lunch?", "v2@example.org", "", "hi"));
    let sent = outgoing(&t);
    assert_eq!(sent.len(), 1, "one vacation reply");
    assert!(
        sent[0].contains("I am away this week.") && sent[0].contains("Subject: Away: Lunch?") && sent[0].contains("To: Sender <sender@example.org>"),
        "{}",
        sent[0]
    );
    deliver(&t, &plain("Sender <sender@example.org>", "Lunch again?", "v3@example.org", "", "hi"));
    assert_eq!(outgoing(&t).len(), 1, "each sender is answered once per interval");
    deliver(
        &t,
        &plain(
            "News <news@example.org>",
            "Newsletter",
            "v4@example.org",
            "List-Id: <news.example.org>\r\n",
            "news",
        ),
    );
    assert_eq!(outgoing(&t).len(), 1, "mailing lists are not answered");
    // Filters and the autoresponder live side by side.
    assert_eq!(
        b.get(&format!("/api/mail/accounts/{acc}/filters")).await["filters"].as_array().unwrap().len(),
        2
    );

    // ---- absence ---------------------------------------------------------------------------
    b.send(
        Method::PATCH,
        &format!("/api/mail/accounts/{acc}/integrations"),
        json!({"ooo_follows_absence": true}),
    )
    .await;
    let today = now.format("%Y-%m-%d").to_string();
    let in3 = (now + chrono::Duration::days(3)).format("%Y-%m-%d").to_string();
    let ab = b
        .send(
            Method::PUT,
            "/api/me/absence",
            json!({"first_day": today, "last_day": in3, "timezone": "Europe/Berlin", "status": "On vacation", "message": "Back on Monday. Ask Carol."}),
        )
        .await;
    assert_eq!(ab["active"], true);
    assert!(ab["autoresponder_errors"].as_array().unwrap().is_empty(), "{ab}");
    let v = b.get(&format!("/api/mail/accounts/{acc}/vacation")).await["vacation"].clone();
    assert_eq!((v["enabled"].as_bool(), v["follows_absence"].as_bool()), (Some(true), Some(true)));
    assert_eq!(v["message"], "Back on Monday. Ask Carol.");
    let (s, _) = b
        .call(Method::PUT, &format!("/api/mail/accounts/{acc}/vacation"), Some(json!({"enabled": false})))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "manual changes are refused while following the absence");
    deliver(&t, &plain("Frank <frank@example.org>", "Status?", "v5@example.org", "", "?"));
    let sent = outgoing(&t);
    assert_eq!(sent.len(), 2);
    assert!(
        sent[1].contains("Back on Monday. Ask Carol.") && sent[1].contains("Subject: Re: Status?"),
        "{}",
        sent[1]
    );
    let others = b.get("/api/users/alice/absence").await;
    assert_eq!(others["absence"]["status"], "On vacation");
    b.call(Method::DELETE, "/api/me/absence", None).await;
    let v = b.get(&format!("/api/mail/accounts/{acc}/vacation")).await["vacation"].clone();
    assert_eq!(v["enabled"], false, "clearing the absence turns the autoresponder off");
    b.send(
        Method::PATCH,
        &format!("/api/mail/accounts/{acc}/integrations"),
        json!({"ooo_follows_absence": false}),
    )
    .await;
    assert!(b.get("/api/users/alice/absence").await["absence"].is_null());

    // ---- an invitation from an external organizer ------------------------------------------
    let conflict = b
        .post(
            "/api/calendar/events",
            json!({"summary": "Dentist", "start": "2027-03-10T09:30:00Z", "end": "2027-03-10T10:30:00Z"}),
        )
        .await["event"]
        .clone();
    let request = fixture("thunderbird-request.eml");
    let req_vars = |seq: &'static str, start: &'static str, end: &'static str, stamp: &'static str, id: &'static str| {
        vec![
            ("UID", "design-review-42@example.org"),
            ("SUMMARY", "Design review"),
            ("SEQ", seq),
            ("START", start),
            ("END", end),
            ("DTSTAMP", stamp),
            ("MSGID", id),
            ("DAY", "Wed Mar 10, 2027"),
            ("DATE", "Wed, 21 Oct 2026 10:00:00 +0000"),
        ]
    };
    append(
        &t,
        "INBOX",
        &fill(
            &request,
            &req_vars("0", "20270310T100000", "20270310T110000", "20261021T100000Z", "req-1@example.org"),
        ),
    )
    .await;
    sync_inbox().await;
    let req_id = find("Invitation: Design review @ Wed Mar 10, 2027").await.expect("invitation synced");
    let info = b.get(&format!("/api/mail/messages/{req_id}/integrations")).await;
    let card = info["imip"].clone();
    assert_eq!((card["method"].as_str(), card["summary"].as_str()), (Some("REQUEST"), Some("Design review")));
    assert_eq!(card["starts_at"], "2027-03-10T09:00:00Z", "10:00 in Berlin");
    assert_eq!(
        (card["can_respond"].as_bool(), card["sender_ok"].as_bool(), card["me"].as_str()),
        (Some(true), Some(true), Some("alice@example.test"))
    );
    assert_eq!(card["organizer"]["email"], "org@example.org");
    assert_eq!(card["conflicts"][0]["summary"], "Dentist", "{card}");
    assert!(card["existing"].is_null(), "not added automatically by default");
    assert!(card["processed"].is_null() || card["processed"]["status"] == "ignored", "{card}");
    assert_eq!(info["sender"]["email"], "org@example.org");
    let r = b
        .post(
            &format!("/api/mail/messages/{req_id}/imip/respond"),
            json!({"response": "accepted", "comment": "See you there"}),
        )
        .await;
    assert_eq!(r["outcome"]["status"], "answered");
    let event_id = r["outcome"]["event_id"].as_str().unwrap().to_string();
    assert_eq!(r["imip"]["my_partstat"], "ACCEPTED");
    assert_eq!(r["imip"]["existing"]["id"], event_id.as_str());
    let mail = tokio::time::timeout(Duration::from_secs(10), sink.recv()).await.unwrap().unwrap();
    assert_eq!(mail.to, vec!["org@example.org"]);
    assert_eq!(mail.from, "alice@example.test", "sent through the user's account");
    assert!(
        mail.data.contains("method=REPLY") && mail.data.contains("Subject: Accepted: Design review"),
        "{}",
        mail.data
    );
    let parts = ferrite_core::mail::imip::itip_parts(mail.data.as_bytes());
    assert_eq!(parts[0].method, "REPLY");
    let unfolded = parts[0].cal.to_ics().replace("\r\n ", "");
    assert!(unfolded.contains("PARTSTAT=ACCEPTED") && unfolded.contains("COMMENT:See you there") && unfolded.contains("UID:design-review-42@example.org"));
    let ev = b.get("/api/calendar/events?from=2027-03-09T00:00:00Z&to=2027-03-11T00:00:00Z").await["events"].clone();
    let review = ev.as_array().unwrap().iter().find(|e| e["id"] == event_id.as_str()).unwrap().clone();
    assert_eq!(review["my_partstat"], "ACCEPTED");
    // The organizer moves it (SEQUENCE 1): the copy follows automatically.
    append(
        &t,
        "INBOX",
        &fill(
            &request,
            &req_vars("1", "20270310T140000", "20270310T150000", "20261022T100000Z", "req-2@example.org"),
        ),
    )
    .await;
    sync_inbox().await;
    wait_for("the update to apply", || {
        let core = core.clone();
        let (user_id, event_id) = (user_id.clone(), event_id.clone());
        async move { core.event_view(&user_id, &event_id).await.unwrap().starts_at.map(|t| t.to_rfc3339()) == Some("2027-03-10T13:00:00+00:00".into()) }
    })
    .await;
    // The first invitation is now outdated: it cannot change the newer copy.
    let card = b.get(&format!("/api/mail/messages/{req_id}/integrations")).await["imip"].clone();
    assert_eq!((card["outdated"].as_bool(), card["existing"]["sequence"].as_i64()), (Some(true), Some(1)));
    let (s, v) = b
        .call(
            Method::POST,
            &format!("/api/mail/messages/{req_id}/imip/respond"),
            Some(json!({"response": "declined"})),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    // The organizer cancels (Outlook): the copy is marked cancelled.
    let cancel_ics = fixture("outlook-cancel.ics");
    let cancel_cal = String::from_utf8(fill(
        &cancel_ics,
        &[
            ("UID", "design-review-42@example.org"),
            ("SUMMARY", "Design review"),
            ("SEQ", "2"),
            ("START", "20270310T140000"),
            ("END", "20270310T150000"),
            ("DTSTAMP", "20261023T080000Z"),
        ],
    ))
    .unwrap();
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(cancel_cal.as_bytes());
    let b64: Vec<String> = b64.as_bytes().chunks(76).map(|c| String::from_utf8(c.to_vec()).unwrap()).collect();
    append(
        &t,
        "INBOX",
        &fill(
            &fixture("outlook-cancel.eml"),
            &[
                ("SUMMARY", "Design review"),
                ("DATE", "Fri, 23 Oct 2026 08:00:00 +0000"),
                ("MSGID", "cancel-1@example.org"),
                ("CALENDAR_BASE64", &b64.join("\n")),
            ],
        ),
    )
    .await;
    sync_inbox().await;
    wait_for("the cancellation to apply", || {
        let core = core.clone();
        let (user_id, event_id) = (user_id.clone(), event_id.clone());
        async move { core.event_view(&user_id, &event_id).await.unwrap().cancelled }
    })
    .await;
    let cancel_id = find("Canceled: Design review").await.unwrap();
    let card = b.get(&format!("/api/mail/messages/{cancel_id}/integrations")).await["imip"].clone();
    assert_eq!(
        (card["method"].as_str(), card["processed"]["status"].as_str()),
        (Some("CANCEL"), Some("applied"))
    );
    assert_eq!(card["existing"]["cancelled"], true);

    // New invitations as tentative entries, when the account asks for it.
    b.send(
        Method::PATCH,
        &format!("/api/mail/accounts/{acc}/integrations"),
        json!({"imip_auto_create": true}),
    )
    .await;
    let mut vars = req_vars("0", "20270311T100000", "20270311T110000", "20261021T110000Z", "req-3@example.org");
    vars[0] = ("UID", "retro-7@example.org");
    vars[1] = ("SUMMARY", "Retro");
    append(&t, "INBOX", &fill(&request, &vars)).await;
    sync_inbox().await;
    wait_for("a tentative entry", || {
        let core = core.clone();
        let user_id = user_id.clone();
        async move {
            let from = "2027-03-11T00:00:00Z".parse().unwrap();
            let to = "2027-03-12T00:00:00Z".parse().unwrap();
            core.events_between(&user_id, from, to).await.unwrap().iter().any(|e| e.summary == "Retro")
        }
    })
    .await;
    let retro_obj: String = sqlx::query_scalar("SELECT data FROM dav_objects WHERE uid = 'retro-7@example.org'")
        .fetch_one(&core.db)
        .await
        .unwrap();
    assert!(
        retro_obj.contains("STATUS:TENTATIVE") && retro_obj.contains("PARTSTAT=NEEDS-ACTION") && !retro_obj.contains("METHOD"),
        "{retro_obj}"
    );

    // ---- a Ferrite event answered by an external guest from their own mail client -----------
    let planning = b
        .post(
            "/api/calendar/events",
            json!({"summary": "Planning", "start": "2027-03-12T09:00:00Z", "end": "2027-03-12T10:00:00Z", "attendees": ["Guest Person <guest@example.org>"]}),
        )
        .await["event"]
        .clone();
    let planning_id = planning["id"].as_str().unwrap().to_string();
    let planning_uid = planning["uid"].as_str().unwrap().to_string();
    let invite = tokio::time::timeout(Duration::from_secs(10), sink.recv()).await.unwrap().unwrap();
    assert_eq!(invite.to, vec!["guest@example.org"]);
    assert!(
        invite.data.contains("method=REQUEST") && invite.data.contains("Reply-To: Alice <alice@example.test>"),
        "{}",
        invite.data
    );
    let reply_template = fixture("google-reply.eml");
    let reply = |partstat: &str, stamp: &str, from: &str, id: &str| {
        fill(
            &reply_template,
            &[
                ("UID", &planning_uid),
                ("SUMMARY", "Planning"),
                ("SEQ", "0"),
                ("START_UTC", "20270312T090000Z"),
                ("END_UTC", "20270312T100000Z"),
                ("DTSTAMP", stamp),
                ("PARTSTAT", partstat),
                ("FROM", from),
                ("ATTENDEE", "guest@example.org"),
                ("MSGID", id),
                ("DAY", "Fri Mar 12, 2027"),
                ("DATE", "Fri, 23 Oct 2026 09:00:00 +0000"),
            ],
        )
    };
    let guest_status = || {
        let core = core.clone();
        let (user_id, planning_id) = (user_id.clone(), planning_id.clone());
        async move {
            core.event_view(&user_id, &planning_id)
                .await
                .unwrap()
                .attendees
                .iter()
                .find(|a| a.email == "guest@example.org")
                .map(|a| a.partstat.clone())
                .unwrap()
        }
    };
    assert_eq!(guest_status().await, "NEEDS-ACTION");
    append(&t, "INBOX", &reply("ACCEPTED", "20261023T090000Z", "guest@example.org", "reply-1@example.org")).await;
    sync_inbox().await;
    wait_for("the guest's answer", || {
        let f = guest_status();
        async move { f.await == "ACCEPTED" }
    })
    .await;
    let notes = b.get("/api/notifications").await["notifications"].clone();
    assert!(
        notes.as_array().unwrap().iter().any(|n| n["title"] == "Guest Person accepted “Planning”"),
        "{notes}"
    );
    // A late copy of an older answer and a forged one change nothing.
    append(
        &t,
        "INBOX",
        &reply("DECLINED", "20261023T080000Z", "guest@example.org", "reply-old@example.org"),
    )
    .await;
    append(
        &t,
        "INBOX",
        &reply("DECLINED", "20261023T100000Z", "mallory@example.org", "reply-forged@example.org"),
    )
    .await;
    sync_inbox().await;
    let log = || {
        let core = core.clone();
        async move {
            sqlx::query_as::<_, (String, String)>("SELECT message_id, status FROM mail_imip_log ORDER BY message_id")
                .fetch_all(&core.db)
                .await
                .unwrap()
        }
    };
    wait_for("both replies to be processed", || {
        let f = log();
        async move { f.await.iter().filter(|(m, _)| m.starts_with("reply-")).count() == 3 }
    })
    .await;
    let entries = log().await;
    assert!(entries.contains(&("reply-old@example.org".into(), "outdated".into())), "{entries:?}");
    assert!(entries.contains(&("reply-forged@example.org".into(), "rejected".into())), "{entries:?}");
    assert_eq!(guest_status().await, "ACCEPTED");
    // The forged one can still be applied on purpose (the user checked it), which wins.
    let forged: String = sqlx::query_scalar("SELECT id FROM mail_messages WHERE message_id = 'reply-forged@example.org'")
        .fetch_one(&core.db)
        .await
        .unwrap();
    let card = b.get(&format!("/api/mail/messages/{forged}/integrations")).await["imip"].clone();
    assert_eq!(
        (card["method"].as_str(), card["sender_ok"].as_bool(), card["from_attendee"]["email"].as_str()),
        (Some("REPLY"), Some(false), Some("guest@example.org"))
    );
    let forced = b.post(&format!("/api/mail/messages/{forged}/imip/apply"), json!({"force": true})).await;
    assert_eq!(forced["outcome"]["status"], "applied");
    assert_eq!(guest_status().await, "DECLINED");
    append(&t, "INBOX", &reply("ACCEPTED", "20261023T120000Z", "guest@example.org", "reply-2@example.org")).await;
    sync_inbox().await;
    wait_for("the guest's newer answer", || {
        let f = guest_status();
        async move { f.await == "ACCEPTED" }
    })
    .await;
    // The guest proposes a new time (Outlook COUNTER); accepting moves the event.
    append(
        &t,
        "INBOX",
        &fill(
            &fixture("outlook-counter.eml"),
            &[
                ("UID", &planning_uid),
                ("SUMMARY", "Planning"),
                ("SEQ", "0"),
                ("START_UTC", "20270312T130000Z"),
                ("END_UTC", "20270312T140000Z"),
                ("DTSTAMP", "20261023T110000Z"),
                ("MSGID", "counter-1@example.org"),
                ("DATE", "Fri, 23 Oct 2026 11:00:00 +0000"),
            ],
        ),
    )
    .await;
    sync_inbox().await;
    let counter_id = find("New Time Proposed: Planning").await.unwrap();
    let card = b.get(&format!("/api/mail/messages/{counter_id}/integrations")).await["imip"].clone();
    assert_eq!((card["method"].as_str(), card["organizer_is_me"].as_bool()), (Some("COUNTER"), Some(true)));
    assert_eq!(
        (card["proposed_start"].as_str(), card["sender_ok"].as_bool()),
        (Some("2027-03-12T13:00:00Z"), Some(true))
    );
    assert_eq!(card["comment"], "Could we move this a bit later?\n");
    let d = b
        .post(&format!("/api/mail/messages/{counter_id}/imip/counter-decision"), json!({"accept": true}))
        .await;
    assert_eq!(d["outcome"]["status"], "applied");
    assert_eq!(
        core.event_view(&user_id, &planning_id).await.unwrap().starts_at.unwrap().to_rfc3339(),
        "2027-03-12T13:00:00+00:00"
    );
    let update = tokio::time::timeout(Duration::from_secs(10), sink.recv()).await.unwrap().unwrap();
    assert!(
        update.to == vec!["guest@example.org"] && update.data.contains("method=REQUEST"),
        "the guest is sent the new time"
    );

    // ---- .ics import, reservations, contacts, events and tasks ------------------------------
    let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Example//EN\r\nBEGIN:VEVENT\r\nUID:conf-2027@example.org\r\nDTSTART:20270401T080000Z\r\nDTEND:20270401T160000Z\r\nSUMMARY:RustConf\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let itinerary_html = r#"<html><head><script type="application/ld+json">{"@context":"http://schema.org","@type":"FlightReservation","reservationNumber":"RXJ34P","underName":{"@type":"Person","name":"Alice"},"reservationFor":{"@type":"Flight","flightNumber":"110","airline":{"@type":"Airline","name":"United","iataCode":"UA"},"departureAirport":{"@type":"Airport","name":"San Francisco Airport","iataCode":"SFO"},"departureTime":"2027-03-04T20:15:00-08:00","arrivalAirport":{"@type":"Airport","name":"John F. Kennedy International Airport","iataCode":"JFK"},"arrivalTime":"2027-03-05T06:30:00-05:00"}}</script></head><body><p>Your flight is confirmed.</p></body></html>"#;
    let booking = format!(
        "From: Travel <travel@airline.example>\r\nTo: alice@example.test\r\nSubject: Your flight\r\nDate: Thu, 22 Oct 2026 08:00:00 +0000\r\nMessage-ID: <trip@example.org>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"T\"\r\n\r\n--T\r\nContent-Type: text/html; charset=utf-8\r\n\r\n{itinerary_html}\r\n--T\r\nContent-Type: text/calendar; name=\"conf.ics\"\r\nContent-Disposition: attachment; filename=\"conf.ics\"\r\n\r\n{ics}--T--\r\n"
    );
    append(&t, "INBOX", booking.as_bytes()).await;
    sync_inbox().await;
    let trip_id = find("Your flight").await.unwrap();
    let info = b.get(&format!("/api/mail/messages/{trip_id}/integrations")).await;
    assert_eq!(info["itineraries"][0]["title"], "Flight UA 110 SFO → JFK");
    assert_eq!(info["itineraries"][0]["start"]["utc"], "2027-03-05T04:15:00Z");
    assert_eq!(info["imip"]["method"], "PUBLISH");
    let idx = info["ics_attachments"][0].as_u64().unwrap();
    let imported = b
        .post(
            &format!("/api/mail/messages/{trip_id}/attachments/{idx}/import"),
            json!({"calendar": "personal"}),
        )
        .await;
    assert_eq!(imported["imported"].as_array().unwrap().len(), 1);
    let imported_id = imported["imported"][0].as_str().unwrap();
    assert_eq!(core.event_view(&user_id, imported_id).await.unwrap().summary, "RustConf");
    // Importing again replaces instead of duplicating.
    b.post(&format!("/api/mail/messages/{trip_id}/attachments/{idx}/import"), json!({})).await;
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dav_objects WHERE uid = 'conf-2027@example.org'")
        .fetch_one(&core.db)
        .await
        .unwrap();
    assert_eq!(n, 1);
    // Contacts.
    let ci = b.get("/api/mail/contact-info?email=travel@airline.example").await["info"].clone();
    assert!(ci["contact"].is_null());
    assert_eq!((ci["name"].as_str(), ci["received"].as_i64()), (Some("Travel"), Some(1)));
    let c = b.post("/api/mail/contacts", json!({"email": "Travel <travel@airline.example>"})).await["contact"].clone();
    assert_eq!(c["name"], "Travel");
    let c2 = b
        .post("/api/mail/contacts", json!({"email": "bookings@airline.example", "contact_id": c["id"]}))
        .await["contact"]
        .clone();
    assert_eq!(c2["emails"].as_array().unwrap().len(), 2);
    let ci = b.get("/api/mail/contact-info?email=bookings@airline.example").await["info"].clone();
    assert_eq!(ci["contact"]["id"], c["id"]);
    let ci = b.get("/api/mail/contact-info?email=alice@example.test").await["info"].clone();
    assert_eq!(ci["user"]["username"], "alice");
    // Event and task drafts.
    let draft = b.get(&format!("/api/mail/messages/{trip_id}/agenda-draft")).await["draft"].clone();
    assert_eq!(draft["summary"], "Your flight");
    assert!(draft["description"].as_str().unwrap().contains("Your flight is confirmed."));
    assert_eq!(draft["attendees"][0]["address"], "travel@airline.example");
    let task = b
        .post(
            "/api/mail/tasks",
            json!({"summary": draft["summary"], "description": draft["description"], "due": "2027-03-01T17:00:00Z"}),
        )
        .await["task"]
        .clone();
    assert_eq!((task["component"].as_str(), task["summary"].as_str()), (Some("VTODO"), Some("Your flight")));

    // ---- unsubscribe ---------------------------------------------------------------------
    let hits = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let h2 = hits.clone();
    let list_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let list_url = format!("http://{}/unsub/42", list_listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(
            list_listener,
            axum::Router::new().route(
                "/unsub/42",
                post(move |body: String| {
                    let h2 = h2.clone();
                    async move {
                        h2.lock().unwrap().push(body);
                        "ok"
                    }
                }),
            ),
        )
        .await
        .unwrap()
    });
    append(
        &t,
        "INBOX",
        &plain(
            "Weekly <weekly@lists.example>",
            "Weekly digest",
            "list-1@example.org",
            &format!("List-Id: Weekly digest <weekly.lists.example>\r\nList-Unsubscribe: <mailto:unsub@lists.example?subject=stop>, <{list_url}>\r\nList-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n"),
            "digest",
        ),
    )
    .await;
    sync_inbox().await;
    let digest = find("Weekly digest").await.unwrap();
    let info = b.get(&format!("/api/mail/messages/{digest}/integrations")).await;
    assert_eq!(info["unsubscribe"]["one_click"], list_url.as_str());
    assert_eq!(info["unsubscribe"]["list"], "Weekly digest");
    let u = b.post(&format!("/api/mail/messages/{digest}/unsubscribe"), json!({})).await;
    assert_eq!(u["method"], "one_click");
    assert_eq!(hits.lock().unwrap().as_slice(), ["List-Unsubscribe=One-Click"]);
    let u = b.post(&format!("/api/mail/messages/{digest}/unsubscribe"), json!({"method": "mail"})).await;
    assert_eq!(u["to"], "unsub@lists.example");
    let m = tokio::time::timeout(Duration::from_secs(10), sink.recv()).await.unwrap().unwrap();
    assert!(m.to == vec!["unsub@lists.example"] && m.data.contains("Subject: stop"));
    // Without the test switch, plain http (and private addresses) are never used.
    unsafe { std::env::remove_var("FERRITE_MAIL_ALLOW_PRIVATE_FETCH") };
    let (s, v) = b
        .call(
            Method::POST,
            &format!("/api/mail/messages/{digest}/unsubscribe"),
            Some(json!({"method": "one_click"})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("no one-click unsubscribe"),
        "plain http is never posted to: {v}"
    );

    // ---- AI ------------------------------------------------------------------------------
    let calls = Arc::new(AtomicUsize::new(0));
    let llm = mock_llm(calls.clone()).await;
    let st = b.get("/api/mail/ai/status").await;
    assert_eq!((st["enabled"].as_bool(), st["llm_processing"].as_bool()), (Some(false), Some(false)));
    let p = b.post("/api/ai/providers", json!({"name": "Local", "kind": "openai", "base_url": llm})).await;
    b.send(Method::PUT, "/api/ai/roles", json!({"role": "chat", "provider_id": p["id"], "model": "mock"}))
        .await;
    b.send(Method::PATCH, "/api/me", json!({"ai_consent": true})).await;
    // A conversation of three messages.
    for (i, (from, body)) in [
        ("Bob <bob@example.org>", "Shall we release on Friday?"),
        ("Carol <carol@example.org>", "Yes, Friday works."),
        ("Bob <bob@example.org>", "I will write the release notes."),
    ]
    .iter()
    .enumerate()
    {
        let extra = if i == 0 {
            String::new()
        } else {
            "In-Reply-To: <rel-0@example.org>\r\nReferences: <rel-0@example.org>\r\n".into()
        };
        append(
            &t,
            "INBOX",
            &plain(
                from,
                if i == 0 { "Release" } else { "Re: Release" },
                &format!("rel-{i}@example.org"),
                &extra,
                body,
            ),
        )
        .await;
    }
    sync_inbox().await;
    let rel = find("Release").await.unwrap();
    let (s, v) = b.call(Method::POST, &format!("/api/mail/messages/{rel}/ai/summary"), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "the administrator has not allowed AI for mail yet: {v}");
    b.send(Method::PUT, "/api/admin/mail/ai", json!({"llm_processing": true})).await;
    assert_eq!(b.get("/api/mail/ai/status").await["enabled"], true);
    let sum = b.post(&format!("/api/mail/messages/{rel}/ai/summary"), json!({})).await;
    assert_eq!(
        (sum["summary"].as_str(), sum["cached"].as_bool(), sum["messages"].as_u64()),
        (Some("The team agreed to ship on Friday; Bob prepares the release notes."), Some(false), Some(3))
    );
    let n = calls.load(Ordering::SeqCst);
    assert_eq!(b.post(&format!("/api/mail/messages/{rel}/ai/summary"), json!({})).await["cached"], true);
    assert_eq!(calls.load(Ordering::SeqCst), n, "the cached summary costs no model call");
    let (s, _) = b.call(Method::POST, &format!("/api/mail/messages/{trip_id}/ai/summary"), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "single messages are not summarized");
    let replies = b.post(&format!("/api/mail/messages/{rel}/ai/replies"), json!({})).await;
    assert_eq!(replies["replies"], json!(["Sounds good!", "Can we talk tomorrow?", "Thanks, ___"]));
    let tr = b.post(&format!("/api/mail/messages/{rel}/ai/translate"), json!({"lang": "de"})).await;
    assert_eq!(
        (tr["subject"].as_str(), tr["text"].as_str()),
        (Some("Freigabe am Freitag"), Some("Wir veröffentlichen am Freitag."))
    );
    let ev = b.post(&format!("/api/mail/messages/{rel}/ai/event"), json!({})).await["suggestion"].clone();
    assert_eq!(
        (ev["title"].as_str(), ev["start"].as_str()),
        (Some("Release planning"), Some("2027-03-15T13:00:00Z"))
    );
    // Follow-ups: sent five days ago, a question, no answer yet.
    let sent_folder = folder("Sent");
    let five_days = (chrono::Utc::now() - chrono::Duration::days(5)).to_rfc2822();
    let q = format!(
        "From: Alice <alice@example.test>\r\nTo: Dan <dan@example.org>\r\nSubject: Budget\r\nDate: {five_days}\r\nMessage-ID: <budget@example.test>\r\nContent-Type: text/plain\r\n\r\nCould you send me the budget by Monday?\r\n"
    );
    let fyi = format!(
        "From: Alice <alice@example.test>\r\nTo: Dan <dan@example.org>\r\nSubject: FYI\r\nDate: {five_days}\r\nMessage-ID: <fyi@example.test>\r\nContent-Type: text/plain\r\n\r\nJust so you know, the office is closed.\r\n"
    );
    append(&t, "Sent", q.as_bytes()).await;
    append(&t, "Sent", fyi.as_bytes()).await;
    b.post(&format!("/api/mail/accounts/{acc}/sync"), json!({"folder_id": sent_folder})).await;
    assert_eq!(ferrite_server::mail_ai::run_follow_ups(&core).await.unwrap(), 2);
    let fu = b.get("/api/mail/follow-ups").await["follow_ups"].clone();
    assert_eq!(fu.as_array().unwrap().len(), 1, "{fu}");
    assert_eq!(
        (fu[0]["subject"].as_str(), fu[0]["recipients"][0]["address"].as_str()),
        (Some("Budget"), Some("dan@example.org"))
    );
    assert!(fu[0]["id"].is_string(), "the Sent copy can be opened");
    // An answer settles it.
    append(
        &t,
        "INBOX",
        &plain(
            "Dan <dan@example.org>",
            "Re: Budget",
            "budget-re@example.org",
            "In-Reply-To: <budget@example.test>\r\nReferences: <budget@example.test>\r\n",
            "Attached.",
        ),
    )
    .await;
    sync_inbox().await;
    assert!(b.get("/api/mail/follow-ups").await["follow_ups"].as_array().unwrap().is_empty());
    let (s, _) = b
        .call(
            Method::POST,
            "/api/mail/follow-ups/disable",
            Some(json!({"account_id": acc, "message_id": "fyi@example.test"})),
        )
        .await;
    assert!(s.is_success());

    // ---- MCP tools -----------------------------------------------------------------------
    let ctx = ferrite_server::tools::UserCtx {
        user: core.user(&user_id).await.unwrap(),
        scopes: "mail,groupware,ai".into(),
    };
    let call = |name: &'static str, args: Value| {
        let (state, ctx) = (state.clone(), ctx.clone());
        async move { state.tools.call(&ctx, name, args).await.unwrap().text_content() }
    };
    let names: Vec<String> = state.tools.list(&ctx).iter().map(|t| t.name.clone()).collect();
    for n in [
        "mail_filters",
        "mail_filter_add",
        "mail_filter_remove",
        "mail_out_of_office",
        "mail_out_of_office_set",
        "mail_invitation_respond",
        "mail_summarize_thread",
        "mail_unsubscribe",
    ] {
        assert!(names.contains(&n.to_string()), "{n} listed");
    }
    assert!(call("mail_filters", json!({})).await.contains("Tag urgent"));
    let added = call(
        "mail_filter_add",
        json!({"name": "From Erin", "field": "from", "values": ["erin@example.org"], "folder": "Reports", "stop": true}),
    )
    .await;
    assert!(added.contains("From Erin"), "{added}");
    deliver(&t, &plain("Erin <erin@example.org>", "Filtered by the assistant", "f5@example.org", "", "x"));
    check.select("Reports").await.unwrap();
    assert_eq!(check.uid_search("FROM erin@example.org").await.unwrap().len(), 1);
    assert!(call("mail_filter_remove", json!({"name": "From Erin"})).await.contains("Removed"));
    assert!(!call("mail_filters", json!({})).await.contains("From Erin"));
    let ooo = call("mail_out_of_office_set", json!({"enabled": true, "message": "Out today.", "end": later(10)})).await;
    assert!(ooo.contains("Out today."), "{ooo}");
    assert!(call("mail_out_of_office", json!({})).await.contains("\"enabled\": true"));
    call("mail_out_of_office_set", json!({"enabled": false})).await;
    b.send(
        Method::PATCH,
        &format!("/api/mail/accounts/{acc}/integrations"),
        json!({"imip_auto_create": false}),
    )
    .await;
    let mut vars = req_vars("0", "20270320T100000", "20270320T110000", "20261021T120000Z", "req-4@example.org");
    vars[0] = ("UID", "lunch-9@example.org");
    vars[1] = ("SUMMARY", "Team lunch");
    vars[7] = ("DAY", "Sat Mar 20, 2027");
    append(&t, "INBOX", &fill(&request, &vars)).await;
    sync_inbox().await;
    let lunch = find("Invitation: Design review @ Sat Mar 20, 2027").await.unwrap();
    let r = call("mail_invitation_respond", json!({"message_id": lunch, "response": "declined"})).await;
    assert!(r.contains("answered"), "{r}");
    let decline = tokio::time::timeout(Duration::from_secs(10), sink.recv()).await.unwrap().unwrap();
    assert!(decline.data.contains("Subject: Declined: Team lunch"));
    assert!(call("mail_summarize_thread", json!({"message_id": rel})).await.contains("ship on Friday"));
    hits.lock().unwrap().clear();
    unsafe { std::env::set_var("FERRITE_MAIL_ALLOW_PRIVATE_FETCH", "1") };
    assert!(call("mail_unsubscribe", json!({"message_id": digest})).await.contains("one-click"));
    assert_eq!(hits.lock().unwrap().len(), 1);
    let _ = conflict;
    check.logout().await;
}
