//! Composing and sending against a REAL IMAP server. Skipped (passes trivially) unless
//! `FERRITE_TEST_IMAP` is set, like `tests/mail.rs`:
//!
//!   MAIL_TEST_DIR=/tmp/ferrite-mail-compose IMAP_PORT=3321 LMTP_PORT=3322 AUTH_PORT=3323 SIEVE_PORT=3324 \
//!     POSTFIX=0 bash e2e/mail-server.sh start /tmp/ferrite-mail-compose
//!   FERRITE_TEST_IMAP=127.0.0.1:3321:alice@example.test:alice-mail-pw \
//!     cargo test -p ferrite-server --test mail_compose -- --nocapture
//!
//! The mailbox is EMPTIED. Outgoing mail goes to an in-process SMTP sink; what it receives is
//! appended to the IMAP inbox to see how it reads, and checked with Python's email package
//! when `python3` is installed (independently of Ferrite's parser).
//!
//! Covered: composer settings and rich signatures, aliases (From and envelope sender), group
//! recipients, HTML mail with an inline image and an attachment (multipart/mixed >
//! alternative > related, format=flowed text, cid parts), the outbox (undo delay, schedule,
//! hold, send now), the admin attachment limit, reading the sent message back (sanitized
//! HTML with the cid image), read receipts (RFC 8098 report, $MDNSent on the server),
//! drafts with HTML and inline images reopened and saved again, reply templates quoting
//! HTML, forward as attachment (message/rfc822 as it is) and the attached message view,
//! the print view, and the MCP tools (html, from, send_at, mail_outbox).

use ferrite_core::mail::ServerSettings;
use ferrite_core::mail::imap::ImapConn;
use ferrite_core::mailer::SmtpSecurity;
use ferrite_core::{Config, Core};
use ferrite_server::AppState;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use std::time::Duration;

struct Target {
    host: String,
    port: u16,
    user: String,
    pass: String,
}

fn target() -> Option<Target> {
    let v = std::env::var("FERRITE_TEST_IMAP").ok()?;
    let mut parts = v.splitn(4, ':');
    Some(Target {
        host: parts.next()?.to_string(),
        port: parts.next()?.parse().ok()?,
        user: parts.next()?.to_string(),
        pass: parts.next()?.to_string(),
    })
}

impl Target {
    fn settings(&self) -> ServerSettings {
        ServerSettings {
            host: self.host.clone(),
            port: self.port,
            security: SmtpSecurity::None,
            username: self.user.clone(),
            password: self.pass.clone(),
            xoauth2: false,
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
    async fn text(&self, path: &str) -> (StatusCode, reqwest::header::HeaderMap, String) {
        let r = self.http.get(format!("{}{path}", self.url)).send().await.unwrap();
        (r.status(), r.headers().clone(), r.text().await.unwrap())
    }
    async fn upload(&self, name: &str, mime: &str, data: Vec<u8>) -> String {
        let up = self
            .http
            .post(format!("{}/api/mail/uploads?name={name}", self.url))
            .header("X-Ferrite-Request", "1")
            .header("Content-Type", mime)
            .body(data)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        up["upload"]["id"].as_str().unwrap().to_string()
    }
}

async fn reset_mailbox(t: &Target) {
    let mut c = ImapConn::connect(&t.settings()).await.expect("IMAP login with FERRITE_TEST_IMAP");
    for f in c.list().await.unwrap() {
        if f.name == "Archive" {
            c.delete(&f.name).await.unwrap();
            continue;
        }
        if !f.selectable {
            continue;
        }
        if c.select(&f.name).await.unwrap().exists > 0 {
            c.uid_store("1:*", "+FLAGS.SILENT (\\Deleted)").await.unwrap();
            c.expunge("1:*").await.unwrap();
        }
    }
    c.logout().await;
}

/// The MIME tree of a message as Python's email package sees it (None without python3).
fn python_tree(raw: &str) -> Option<Value> {
    const PY: &str = r#"
import email, json, sys
from email import policy
m = email.message_from_bytes(sys.stdin.buffer.read(), policy=policy.default)
def walk(p):
    kids = list(p.iter_parts()) if p.is_multipart() else []
    if p.get_content_type() == 'message/rfc822':
        kids = [p.get_payload(0)]
    return {'type': p.get_content_type(), 'params': {k: v for k, v in (p.get_params() or [])[1:]}, 'cid': p.get('Content-ID'),
            'cte': p.get('Content-Transfer-Encoding'), 'filename': p.get_filename(), 'parts': [walk(x) for x in kids]}
print(json.dumps({'tree': walk(m), 'from': str(m['from']), 'subject': str(m['subject']), 'defects': [repr(d) for d in m.defects]}))
"#;
    use std::io::Write;
    let mut child = std::process::Command::new("python3")
        .args(["-c", PY])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(raw.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    assert!(out.status.success(), "python3 could not parse the message");
    serde_json::from_slice(&out.stdout).ok()
}

fn kinds(t: &Value) -> Vec<String> {
    let mut v = vec![t["type"].as_str().unwrap().to_string()];
    for p in t["parts"].as_array().unwrap() {
        v.extend(kinds(p));
    }
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn composing_and_sending_against_a_real_imap_server() {
    let Some(t) = target() else {
        eprintln!("FERRITE_TEST_IMAP is not set; skipping the real-server composing test (see the top of tests/mail_compose.rs)");
        return;
    };
    let _ = tracing_subscriber::fmt().with_env_filter("warn,ferrite=info").with_test_writer().try_init();
    reset_mailbox(&t).await;
    let me = t.user.clone();
    let mut imap = ImapConn::connect(&t.settings()).await.unwrap();
    imap.append(
        "INBOX",
        "()",
        format!(
            "From: Ada Lovelace <ada@example.org>\r\nTo: {me}\r\nSubject: Engine notes\r\nMessage-ID: <engine@rust.test>\r\nDate: Mon, 19 Oct 2026 09:00:00 +0000\r\n\
             MIME-Version: 1.0\r\nContent-Type: multipart/related; boundary=\"R\"\r\n\r\n--R\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
             <p>The <b>analytical engine</b> needs a mill.</p><img src=\"cid:dot@rust\" alt=\"diagram\">\r\n--R\r\nContent-Type: image/png\r\n\
             Content-ID: <dot@rust>\r\nContent-Transfer-Encoding: base64\r\n\r\niVBORw0KGgo=\r\n--R--\r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let mut cfg = Config {
        data_dir: dir.path().to_path_buf(),
        public_url: url.clone(),
        ..Config::default()
    };
    cfg.sfu.enabled = false;
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
    for (u, n) in [("bob", "Bob B"), ("carol", "Carol C")] {
        b.post(
            "/api/admin/users",
            json!({"username": u, "display_name": n, "email": format!("{u}@example.test"), "password": "pw-pw-pw-pw"}),
        )
        .await;
    }
    let group = b.post("/api/admin/groups", json!({"name": "Team"})).await["group"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    for u in ["bob", "carol"] {
        b.post(&format!("/api/admin/groups/{group}/members"), json!({"username": u})).await;
    }
    // Local mail servers need the admin's consent where the admin mail settings exist.
    let _ = b
        .call(Method::PATCH, "/api/admin/mail/settings", Some(json!({"allow_local_servers": true})))
        .await;
    let (sink_port, mut sink) = ferrite_core::mailer::sink::sink().await;
    let acc = b
        .post(
            "/api/mail/accounts",
            json!({"email": me, "name": "Alice", "password": t.pass, "imap_host": t.host, "imap_port": t.port, "imap_security": "none",
                   "smtp_host": "127.0.0.1", "smtp_port": sink_port, "smtp_security": "none", "smtp_auth": false}),
        )
        .await["account"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let folders = b.get(&format!("/api/mail/accounts/{acc}/folders")).await["folders"].clone();
    let role = |r: &str| {
        folders.as_array().unwrap().iter().find(|f| f["role"] == r).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let (inbox, drafts) = (role("inbox"), role("drafts"));

    // Settings: rich text by default, a rich signature below the quote, no undo delay; an alias.
    let prefs = b
        .send(
            Method::PUT,
            &format!("/api/mail/accounts/{acc}/compose-settings"),
            json!({"editor_mode": "richtext", "signature_html": "<p>Alice <b>A.</b><script>x()</script></p>", "signature_above_quote": false}),
        )
        .await["prefs"]
        .clone();
    assert_eq!(prefs["signature_text"], "Alice *A.*");
    assert!(!prefs["signature_html"].as_str().unwrap().contains("script"));
    b.send(Method::PUT, "/api/mail/send-settings", json!({"undo_send_seconds": 0})).await;
    let (s, _) = b.call(Method::PUT, "/api/mail/send-settings", Some(json!({"undo_send_seconds": 7}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let alias = b
        .post(
            &format!("/api/mail/accounts/{acc}/aliases"),
            json!({"name": "Alice Support", "email": "support@example.test", "signature_html": "<p>Support team</p>"}),
        )
        .await["alias"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let ctx = b.get("/api/mail/compose/context").await;
    assert_eq!(ctx["accounts"][0]["aliases"][0]["email"], "support@example.test");
    assert!(ctx["accounts"][0]["aliases"][0]["signature_html"].as_str().unwrap().contains("Support team"));

    // HTML mail with an inline image and an attachment, from the alias, to a group, with a
    // read-receipt request, through the outbox.
    let png = vec![0x89, b'P', b'N', b'G', 13, 10, 26, 10, 1, 2, 3, 4];
    let img = b.upload("chart.png", "image/png", png.clone()).await;
    let file = b.upload("notes.txt", "text/plain", b"the notes".to_vec()).await;
    let html = format!(
        "<p>Hi <b>team</b>,</p><ul><li>first</li></ul><p><img src=\"/api/mail/uploads/{img}/content\" alt=\"chart\"></p>\
         <p onclick=\"x()\">See <a href=\"https://example.org/plan\">the plan</a>.</p><div class=\"ferrite-signature\"><p>Support team</p></div>"
    );
    let queued = b
        .post(
            "/api/mail/outbox",
            json!({"account_id": acc, "compose": {"to": [format!("group:{group}")], "cc": [me], "subject": "Grüße", "html": html,
                   "attachments": [{"kind": "upload", "id": file}], "alias_id": alias, "request_mdn": true}}),
        )
        .await["message"]
        .clone();
    assert_eq!(queued["status"], "queued");
    let got = tokio::time::timeout(Duration::from_secs(20), sink.recv())
        .await
        .expect("the outbox sends")
        .unwrap();
    assert_eq!(got.from, "support@example.test", "the alias is the envelope sender");
    let mut rcpt = got.to.clone();
    rcpt.sort();
    let mut want = vec!["bob@example.test".to_string(), "carol@example.test".to_string(), me.clone()];
    want.sort();
    assert_eq!(rcpt, want, "the group expanded");
    let dnt = got
        .data
        .lines()
        .find(|l| l.starts_with("Disposition-Notification-To:"))
        .expect("a read receipt is requested");
    assert!(dnt.contains("support@example.test"), "{dnt}");
    assert!(!got.data.contains("onclick") && !got.data.contains("/api/mail/uploads/"));
    let parsed = mail_parser::MessageParser::default().parse(got.data.as_bytes()).unwrap();
    assert_eq!(parsed.subject(), Some("Grüße"));
    assert!(parsed.body_text(0).unwrap().contains("Hi *team*,"));
    assert!(parsed.body_text(0).unwrap().contains("the plan <https://example.org/plan>"));
    assert!(parsed.body_text(0).unwrap().contains("-- \r\nSupport team") || parsed.body_text(0).unwrap().contains("-- \nSupport team"));
    if let Some(py) = python_tree(&got.data) {
        assert!(py["defects"].as_array().unwrap().is_empty(), "{py}");
        assert_eq!(py["from"], "Alice Support <support@example.test>");
        assert_eq!(
            kinds(&py["tree"]),
            [
                "multipart/mixed",
                "multipart/alternative",
                "text/plain",
                "multipart/related",
                "text/html",
                "image/png",
                "text/plain"
            ]
        );
        assert_eq!(py["tree"]["parts"][0]["parts"][0]["params"]["format"], "flowed");
        assert_eq!(py["tree"]["parts"][1]["filename"], "notes.txt");
    } else {
        eprintln!("python3 is not installed; the MIME structure was only checked with mail-parser");
    }
    assert!(b.get("/api/mail/outbox").await["messages"].as_array().unwrap().is_empty());

    // It arrives: read it back.
    imap.append("INBOX", "()", got.data.as_bytes()).await.unwrap();
    b.post(&format!("/api/mail/accounts/{acc}/sync"), json!({"folder_id": inbox})).await;
    let list = b.get(&format!("/api/mail/folders/{inbox}/messages")).await["messages"].clone();
    let mid = list.as_array().unwrap().iter().find(|m| m["subject"] == "Grüße").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, _, doc) = b.text(&format!("/api/mail/messages/{mid}/html")).await;
    assert!(doc.contains("<b>team</b>") && doc.contains(&format!("/api/mail/messages/{mid}/cid/")), "{doc}");
    let view = b.get(&format!("/api/mail/messages/{mid}")).await["message"].clone();
    let cid = view["attachments"].as_array().unwrap().iter().find(|a| a["inline"] == true).unwrap()["content_id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = b.http.get(format!("{url}/api/mail/messages/{mid}/cid/{cid}")).send().await.unwrap();
    assert_eq!(r.bytes().await.unwrap().to_vec(), png, "the inline image round-trips");

    // Read receipt: the sender is one of our own addresses (the alias), so no question...
    assert_eq!(b.get(&format!("/api/mail/messages/{mid}/mdn")).await["mdn"]["requested"], false);
    // ...but for a message from someone else there is one, and the answer is remembered.
    imap.append(
        "INBOX",
        "()",
        format!(
            "From: Ada <ada@example.org>\r\nTo: {me}\r\nSubject: Receipt please\r\nMessage-ID: <receipt@rust.test>\r\nDisposition-Notification-To: Ada <ada@example.org>\r\n\r\nHello\r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    b.post(&format!("/api/mail/accounts/{acc}/sync"), json!({"folder_id": inbox})).await;
    let list = b.get(&format!("/api/mail/folders/{inbox}/messages")).await["messages"].clone();
    let rid = list.as_array().unwrap().iter().find(|m| m["subject"] == "Receipt please").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let st = b.get(&format!("/api/mail/messages/{rid}/mdn")).await["mdn"].clone();
    assert_eq!((st["requested"].as_bool(), st["handled"].as_bool()), (Some(true), Some(false)), "{st}");
    b.post(&format!("/api/mail/messages/{rid}/mdn"), json!({"send": true})).await;
    let mdn = tokio::time::timeout(Duration::from_secs(10), sink.recv()).await.unwrap().unwrap();
    assert_eq!(mdn.to, vec!["ada@example.org"]);
    assert!(mdn.data.contains("report-type=disposition-notification"));
    assert!(mdn.data.contains("Original-Message-ID: <receipt@rust.test>"));
    assert!(mdn.data.contains("Disposition: manual-action/MDN-sent-manually; displayed"));
    if let Some(py) = python_tree(&mdn.data) {
        assert_eq!(kinds(&py["tree"])[..3], ["multipart/report", "text/plain", "message/disposition-notification"]);
    }
    assert_eq!(b.get(&format!("/api/mail/messages/{rid}/mdn")).await["mdn"]["handled"], true);
    let mut check = ImapConn::connect(&t.settings()).await.unwrap();
    check.select("INBOX").await.unwrap();
    assert_eq!(check.uid_search("KEYWORD $MDNSent").await.unwrap().len(), 1, "$MDNSent is on the server");

    // A reply quotes the HTML (with its image) and puts the signature below the quote.
    let list = b.get(&format!("/api/mail/folders/{inbox}/messages")).await["messages"].clone();
    let eid = list.as_array().unwrap().iter().find(|m| m["subject"] == "Engine notes").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let tpl = b.get(&format!("/api/mail/messages/{eid}/compose?mode=reply")).await["compose"].clone();
    let th = tpl["html"].as_str().unwrap();
    assert!(th.contains("<blockquote type=\"cite\"") && th.contains("<b>analytical engine</b>"), "{th}");
    assert!(th.contains(&format!("/api/mail/messages/{eid}/cid/dot@rust")), "{th}");
    assert!(th.find("ferrite-quote").unwrap() < th.find("ferrite-signature").unwrap());
    assert!(tpl["text"].as_str().unwrap().contains("> The analytical engine needs a mill."));
    assert!(tpl["text"].as_str().unwrap().trim_end().ends_with("-- \nAlice *A.*"));

    // Drafts keep their HTML and inline images; reopened, they can be saved again.
    let img2 = b.upload("pic.png", "image/png", png.clone()).await;
    let d1 = b
        .post(
            &format!("/api/mail/accounts/{acc}/drafts"),
            json!({"to": ["bob@example.test"], "subject": "Rich draft", "html": format!("<p>Draft <i>text</i></p><img src=\"/api/mail/uploads/{img2}/content\">"), "request_mdn": true}),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let edit = b.get(&format!("/api/mail/messages/{d1}/compose?mode=edit")).await["compose"].clone();
    let eh = edit["html"].as_str().unwrap().to_string();
    assert!(eh.contains("<i>text</i>") && eh.contains("/api/mail/uploads/"), "{eh}");
    assert_eq!(edit["request_mdn"], true);
    let d2 = b
        .post(
            &format!("/api/mail/accounts/{acc}/drafts"),
            json!({"to": ["bob@example.test"], "subject": "Rich draft", "html": eh, "draft_id": d1}),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let dl = b.get(&format!("/api/mail/folders/{drafts}/messages")).await["messages"].clone();
    assert_eq!(dl.as_array().unwrap().len(), 1);
    let (_, _, ddoc) = b.text(&format!("/api/mail/messages/{d2}/html")).await;
    assert!(ddoc.contains("/cid/"), "the image is still in the draft: {ddoc}");

    // Forward as attachment: the original travels as message/rfc822, as it is.
    let fwd = b.get(&format!("/api/mail/messages/{eid}/compose?mode=forward_as_attachment")).await["compose"].clone();
    assert_eq!(fwd["attachments"][0]["kind"], "eml");
    let mut c = fwd.clone();
    c["to"] = json!([me]);
    b.post("/api/mail/outbox", json!({"account_id": acc, "compose": c})).await;
    let got = tokio::time::timeout(Duration::from_secs(20), sink.recv()).await.unwrap().unwrap();
    assert!(
        got.data
            .contains("Content-Type: message/rfc822\r\nContent-Disposition: attachment; filename=\"Engine notes.eml\"\r\nContent-Transfer-Encoding: 7bit")
    );
    if let Some(py) = python_tree(&got.data) {
        assert!(kinds(&py["tree"]).contains(&"image/png".to_string()), "the attached original is readable: {py}");
    }
    imap.append("INBOX", "()", got.data.as_bytes()).await.unwrap();
    b.post(&format!("/api/mail/accounts/{acc}/sync"), json!({"folder_id": inbox})).await;
    let list = b.get(&format!("/api/mail/folders/{inbox}/messages")).await["messages"].clone();
    let fid = list.as_array().unwrap().iter().find(|m| m["subject"] == "Fwd: Engine notes").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let fv = b.get(&format!("/api/mail/messages/{fid}")).await["message"].clone();
    let idx = fv["attachments"].as_array().unwrap().iter().find(|a| a["mime"] == "message/rfc822").unwrap()["index"]
        .as_u64()
        .unwrap();
    let nested = b.get(&format!("/api/mail/messages/{fid}/attachments/{idx}/message")).await["message"].clone();
    assert_eq!(nested["subject"], "Engine notes");
    assert_eq!(nested["has_html"], true);
    let (_, headers, ndoc) = b.text(&format!("/api/mail/messages/{fid}/attachments/{idx}/message/html")).await;
    assert!(
        ndoc.contains("<b>analytical engine</b>") && ndoc.contains(&format!("/attachments/{idx}/cid/dot@rust")),
        "{ndoc}"
    );
    assert!(headers["content-security-policy"].to_str().unwrap().contains("sandbox"));

    // Print view: headers, body; the frame may print but runs no scripts.
    let (s, headers, pdoc) = b.text(&format!("/api/mail/messages/{eid}/print")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        pdoc.contains("Ada Lovelace") && pdoc.contains("analytical engine") && pdoc.contains("<h1>Engine notes</h1>"),
        "{pdoc}"
    );
    let csp = headers["content-security-policy"].to_str().unwrap();
    assert!(csp.contains("allow-modals") && !csp.contains("allow-scripts"), "{csp}");

    // Scheduling: later is not now; held messages wait; send now sends.
    let later = b
        .post(
            "/api/mail/outbox",
            json!({"account_id": acc, "compose": {"to": ["bob@example.test"], "subject": "Later", "text": "plain later"},
                   "send_at": (chrono::Utc::now() + chrono::Duration::hours(3)).to_rfc3339()}),
        )
        .await["message"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(sink.try_recv().is_err(), "a scheduled message waits");
    b.post(&format!("/api/mail/outbox/{later}/stop"), json!({})).await;
    let held = b.get(&format!("/api/mail/outbox/{later}")).await["message"].clone();
    assert!(held["send_at"].is_null());
    assert_eq!(held["compose"]["text"], "plain later");
    b.post(&format!("/api/mail/outbox/{later}/send"), json!({})).await;
    let got = tokio::time::timeout(Duration::from_secs(20), sink.recv()).await.unwrap().unwrap();
    assert!(got.data.contains("Subject: Later") && got.data.contains("format=flowed") && !got.data.contains("multipart"));

    // The admin attachment limit (the admins' mail settings) refuses larger attachments.
    let (s, v) = b
        .call(Method::PATCH, "/api/admin/mail/settings", Some(json!({"attachment_size_limit": 1024})))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let big = b.upload("big.bin", "application/octet-stream", vec![7u8; 4096]).await;
    let (s, v) = b
        .call(
            Method::POST,
            "/api/mail/outbox",
            Some(json!({"account_id": acc, "compose": {"to": ["bob@example.test"], "subject": "x", "text": "x", "attachments": [{"kind": "upload", "id": big}]}})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"].as_str().unwrap().contains("share it as a link"));
    assert_eq!(b.get("/api/mail/compose/context").await["attachment_limit"], 1024);
    let link = b.post("/api/mail/compose/share-link", json!({"upload_id": big})).await["link"].clone();
    assert!(link["url"].as_str().unwrap().contains("/s/") && link["name"] == "big.bin" && !link["expires_at"].is_null());
    let r = b.http.get(link["url"].as_str().unwrap()).send().await.unwrap();
    assert!(r.status().is_success());
    b.call(Method::PATCH, "/api/admin/mail/settings", Some(json!({"attachment_size_limit": null})))
        .await;

    // MCP: html, from (an alias), send_at, and the outbox.
    let tctx = ferrite_server::tools::UserCtx {
        user: core.user(&user_id).await.unwrap(),
        scopes: "mail".into(),
    };
    let r = state
        .tools
        .call(
            &tctx,
            "mail_send",
            json!({"to": ["bob@example.test"], "subject": "From the assistant", "html": "<p>Hello <b>Bob</b></p>", "from": "support@example.test",
                   "send_at": (chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339()}),
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&r.text_content()).unwrap()["scheduled"],
        true,
        "{}",
        r.text_content()
    );
    let listed = state.tools.call(&tctx, "mail_outbox", json!({})).await.unwrap();
    assert!(listed.text_content().contains("From the assistant"), "{}", listed.text_content());
    let r = state
        .tools
        .call(
            &tctx,
            "mail_send",
            json!({"to": ["bob@example.test"], "subject": "Now", "html": "<p>Now <i>then</i></p>", "from": "support@example.test"}),
        )
        .await
        .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&r.text_content()).unwrap()["sent"], true, "{}", r.text_content());
    let got = tokio::time::timeout(Duration::from_secs(20), sink.recv()).await.unwrap().unwrap();
    assert_eq!(got.from, "support@example.test");
    assert!(got.data.contains("multipart/alternative"));
    let refused = state
        .tools
        .call(
            &tctx,
            "mail_send",
            json!({"to": ["bob@example.test"], "text": "x", "from": "nobody@example.test"}),
        )
        .await
        .unwrap();
    assert!(
        refused.text_content().contains("none of your mail accounts sends as nobody@example.test"),
        "{}",
        refused.text_content()
    );
    check.logout().await;
    imap.logout().await;
}
