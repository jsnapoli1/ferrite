//! Webmail against a REAL IMAP server. Skipped (passes trivially) unless configured:
//!
//!   sudo e2e/mail-server.sh start          # Dovecot + Postfix on localhost (see the script)
//!   FERRITE_TEST_IMAP=127.0.0.1:3143:alice@example.test:alice-mail-pw \
//!   FERRITE_TEST_SMTP=127.0.0.1:3587 \
//!   cargo test -p ferrite-server --test mail -- --nocapture
//!
//! `FERRITE_TEST_IMAP` is host:port:user:password of a mailbox the test may EMPTY (plain
//! IMAP, no TLS). `FERRITE_TEST_SMTP` (host:port, plain, same credentials) is that server's
//! SMTP; without it, mail is sent to an in-process SMTP sink and only the Sent copy is
//! checked on the IMAP server.
//!
//! Covered: account setup (connection test, password sealed at rest and never returned),
//! special-use folders, envelope sync and incremental sync, UIDVALIDITY reset, threads,
//! bodies on demand, HTML sanitizing and remote-image blocking, attachments (download and
//! save to Files), flags/move/archive/delete/expunge mirrored on the server, full-text and
//! server-side search, IDLE delivering new mail live (hub event + notification) while the
//! owner is online, drafts, sending (reply threading, attachments, Sent copy, arrival),
//! MCP tools, and bounded connections.

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
    smtp: Option<(String, u16)>,
}

fn target() -> Option<Target> {
    let v = std::env::var("FERRITE_TEST_IMAP").ok()?;
    let mut parts = v.splitn(4, ':');
    let host = parts.next()?.to_string();
    let port = parts.next()?.parse().ok()?;
    let user = parts.next()?.to_string();
    let pass = parts.next()?.to_string();
    let smtp = std::env::var("FERRITE_TEST_SMTP").ok().and_then(|s| {
        let (h, p) = s.rsplit_once(':')?;
        Some((h.to_string(), p.parse().ok()?))
    });
    Some(Target { host, port, user, pass, smtp })
}

impl Target {
    fn settings(&self) -> ServerSettings {
        ServerSettings {
            host: self.host.clone(),
            port: self.port,
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
    async fn post(&self, path: &str, body: Value) -> Value {
        let (s, v) = self.call(Method::POST, path, Some(body)).await;
        assert!(s.is_success(), "POST {path}: {s} {v}");
        v
    }
    async fn bytes(&self, path: &str) -> (StatusCode, reqwest::header::HeaderMap, Vec<u8>) {
        let r = self.http.get(format!("{}{path}", self.url)).send().await.unwrap();
        (r.status(), r.headers().clone(), r.bytes().await.unwrap().to_vec())
    }
}

/// Empties every mailbox and removes folders a previous run created.
async fn reset_mailbox(t: &Target) {
    let mut c = ImapConn::connect(&t.settings()).await.expect("IMAP login with FERRITE_TEST_IMAP");
    for f in c.list().await.unwrap() {
        if ["Archive", "Reset-Test"].contains(&f.name.as_str()) {
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
    c.logout().await;
}

fn msg(from: &str, to: &str, subject: &str, id: &str, extra_headers: &str, body: &str) -> Vec<u8> {
    format!(
        "From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\nDate: Mon, 19 Oct 2026 09:00:00 +0000\r\nMessage-ID: <{id}>\r\nMIME-Version: 1.0\r\n{extra_headers}\r\n{body}"
    )
    .replace("\r\n", "\n")
    .replace('\n', "\r\n")
    .into_bytes()
}

async fn wait_for<F: std::future::Future<Output = bool>>(what: &str, mut f: impl FnMut() -> F) {
    for _ in 0..100 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webmail_against_a_real_imap_server() {
    let Some(t) = target() else {
        eprintln!("FERRITE_TEST_IMAP is not set; skipping the real-server webmail test (see the top of tests/mail.rs)");
        return;
    };
    let _ = tracing_subscriber::fmt().with_env_filter("warn,ferrite=info").with_test_writer().try_init();
    reset_mailbox(&t).await;
    let me = t.user.clone();

    // Seed the mailbox: a conversation, an HTML newsletter with a tracker and an attachment.
    let mut seed = ImapConn::connect(&t.settings()).await.unwrap();
    seed.append(
        "INBOX",
        "(\\Seen)",
        &msg(
            "Ada Lovelace <ada@example.org>",
            &me,
            "Engine plans",
            "root@example.org",
            "Content-Type: text/plain; charset=utf-8\r\n",
            "Shall we build the analytical engine?\r\n",
        ),
    )
    .await
    .unwrap();
    seed.append(
        "INBOX",
        "()",
        &msg(
            "Charles Babbage <charles@example.org>",
            &me,
            "Re: Engine plans",
            "reply1@example.org",
            "In-Reply-To: <root@example.org>\r\nReferences: <root@example.org>\r\nContent-Type: text/plain; charset=utf-8\r\n",
            "Yes, with a zebracorn mill.\r\n",
        ),
    )
    .await
    .unwrap();
    let html = "Content-Type: multipart/mixed; boundary=\"B\"\r\n";
    let html_body = "--B\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p onclick=\"x()\">Weekly <b>news</b></p><img src=\"https://tracker.example/p.gif\"><script>alert(1)</script>\r\n\
--B\r\nContent-Type: text/plain; name=\"notes.txt\"\r\nContent-Disposition: attachment; filename=\"notes.txt\"\r\n\r\nquasar notes inside the attachment\r\n--B--\r\n";
    seed.append(
        "INBOX",
        "()",
        &msg("News <news@example.org>", &me, "Newsletter", "news1@example.org", html, html_body),
    )
    .await
    .unwrap();

    // Server.
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

    // SMTP: the real server's, or a sink.
    let (smtp_host, smtp_port, smtp_auth, mut sink) = match &t.smtp {
        Some((h, p)) => (h.clone(), *p, true, None),
        None => {
            let (port, rx) = ferrite_core::mailer::sink::sink().await;
            ("127.0.0.1".to_string(), port, false, Some(rx))
        }
    };

    // A wrong password is caught when saving.
    let bad = json!({"email": me, "name": "Alice", "password": "wrong", "imap_host": t.host, "imap_port": t.port, "imap_security": "none",
                     "smtp_host": smtp_host, "smtp_port": smtp_port, "smtp_security": "none"});
    let (s, v) = b.call(Method::POST, "/api/mail/accounts", Some(bad)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"].as_str().unwrap().contains("IMAP login failed"), "{v}");

    let account = b
        .post(
            "/api/mail/accounts",
            json!({"email": me, "name": "Alice", "password": t.pass, "imap_host": t.host, "imap_port": t.port, "imap_security": "none",
                   "smtp_host": smtp_host, "smtp_port": smtp_port, "smtp_security": "none", "smtp_auth": smtp_auth, "signature": "Alice A."}),
        )
        .await["account"]
        .clone();
    let acc = account["id"].as_str().unwrap().to_string();
    assert!(
        account.get("password").is_none() && !account.to_string().contains(&t.pass),
        "password never returned"
    );
    let sealed: String = sqlx::query_scalar("SELECT password FROM mail_accounts WHERE id = ?")
        .bind(&acc)
        .fetch_one(&core.db)
        .await
        .unwrap();
    assert!(!sealed.contains(&t.pass), "password is encrypted at rest");

    // Folders with special-use roles.
    let folders = b.get(&format!("/api/mail/accounts/{acc}/folders")).await["folders"].clone();
    let role = |r: &str| {
        folders
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["role"] == r)
            .unwrap_or_else(|| panic!("no {r} folder in {folders}"))["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let (inbox, sent, drafts, trash) = (role("inbox"), role("sent"), role("drafts"), role("trash"));
    role("junk");
    assert_eq!(folders[0]["role"], "inbox", "inbox first");

    // Envelopes.
    let page = b.get(&format!("/api/mail/folders/{inbox}/messages")).await;
    let list = page["messages"].as_array().unwrap();
    assert_eq!(list.len(), 3, "{page}");
    let by_subject = |subject: &str| list.iter().find(|m| m["subject"] == subject).unwrap().clone();
    let root = by_subject("Engine plans");
    let reply = by_subject("Re: Engine plans");
    let news = by_subject("Newsletter");
    assert_eq!(root["seen"], true);
    assert_eq!(reply["seen"], false);
    assert_eq!(reply["thread_size"], 2);
    assert_eq!(reply["thread_id"], root["thread_id"]);
    assert_eq!(reply["from_name"], "Charles Babbage");
    assert!(reply["preview"].as_str().unwrap().contains("zebracorn"));
    assert_eq!(news["has_attachments"], true);
    let inbox_row = b.get(&format!("/api/mail/accounts/{acc}/folders")).await["folders"][0].clone();
    assert_eq!((inbox_row["total"].as_i64(), inbox_row["unread"].as_i64()), (Some(3), Some(2)));

    // Conversation.
    let reply_id = reply["id"].as_str().unwrap().to_string();
    let thread = b.get(&format!("/api/mail/messages/{reply_id}/thread")).await["messages"].clone();
    assert_eq!(thread.as_array().unwrap().len(), 2);
    assert_eq!(thread[0]["subject"], "Engine plans");

    // Bodies and sanitized HTML.
    let news_id = news["id"].as_str().unwrap().to_string();
    let view = b.get(&format!("/api/mail/messages/{news_id}")).await["message"].clone();
    assert_eq!(view["has_html"], true);
    assert_eq!(view["remote_content"], true);
    let att = &view["attachments"][0];
    assert_eq!((att["name"].as_str(), att["inline"].as_bool()), (Some("notes.txt"), Some(false)));
    let (status, headers, html) = b.bytes(&format!("/api/mail/messages/{news_id}/html")).await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8(html).unwrap();
    assert!(
        html.contains("<b>news</b>") && !html.contains("script") && !html.contains("onclick") && !html.contains("tracker.example"),
        "{html}"
    );
    let csp = headers["content-security-policy"].to_str().unwrap();
    assert!(csp.contains("sandbox") && csp.contains("img-src 'self' data:;"), "{csp}");
    let (_, headers, html) = b.bytes(&format!("/api/mail/messages/{news_id}/html?remote=true")).await;
    assert!(String::from_utf8(html).unwrap().contains("https://tracker.example/p.gif"));
    assert!(headers["content-security-policy"].to_str().unwrap().contains("https:"));
    // Opening a message does not mark it read on the server (BODY.PEEK).
    let mut check = ImapConn::connect(&t.settings()).await.unwrap();
    check.select("INBOX").await.unwrap();
    assert_eq!(check.uid_search("SEEN").await.unwrap().len(), 1);

    // Attachments: download, and save to Files.
    let (status, headers, data) = b.bytes(&format!("/api/mail/messages/{news_id}/attachments/0")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers["content-disposition"].to_str().unwrap().contains("notes.txt"));
    assert_eq!(String::from_utf8(data).unwrap().trim(), "quasar notes inside the attachment");
    let me_info = b.get("/api/me").await;
    let root_id = me_info["root_id"].as_str().unwrap();
    let saved = b
        .post(&format!("/api/mail/messages/{news_id}/attachments/0/save"), json!({"folder_id": root_id}))
        .await;
    assert_eq!(saved["node"]["name"], "notes.txt");
    let again = b
        .post(&format!("/api/mail/messages/{news_id}/attachments/0/save"), json!({"folder_id": root_id}))
        .await;
    assert_eq!(again["node"]["name"], "notes (2).txt", "a free name is picked");

    // Search: the global index (kind mail) and server-side search for text only in the body
    // of a message whose body is not cached.
    let hits = b.get("/api/search?q=zebracorn&kinds=mail").await["results"].clone();
    assert_eq!(hits[0]["kind"], "mail");
    assert_eq!(hits[0]["id"], reply_id);
    let root_id_msg = root["id"].as_str().unwrap().to_string();
    let found = b.get("/api/mail/search?q=analytical").await["messages"].clone();
    assert!(found.as_array().unwrap().iter().any(|m| m["id"] == root_id_msg), "server-side search: {found}");

    // Flags mirror to the server.
    b.post("/api/mail/actions", json!({"ids": [reply_id], "action": "read"})).await;
    b.post("/api/mail/actions", json!({"ids": [reply_id], "action": "flag"})).await;
    check.select("INBOX").await.unwrap();
    assert_eq!(check.uid_search("SEEN").await.unwrap().len(), 2);
    assert_eq!(check.uid_search("FLAGGED").await.unwrap().len(), 1);
    let inbox_row = b.get(&format!("/api/mail/accounts/{acc}/folders")).await["folders"][0].clone();
    assert_eq!(inbox_row["unread"], 1);

    // A flag changed by another client is picked up by the next sync.
    check.uid_store("1:*", "-FLAGS.SILENT (\\Flagged)").await.unwrap();
    b.post(&format!("/api/mail/accounts/{acc}/sync"), json!({"folder_id": inbox})).await;
    let r = b.get(&format!("/api/mail/messages/{reply_id}")).await["message"].clone();
    assert_eq!(r["flagged"], false);

    // Archive creates the Archive folder; delete moves to Trash, delete in Trash expunges.
    b.post("/api/mail/actions", json!({"ids": [news_id], "action": "archive"})).await;
    let folders = b.get(&format!("/api/mail/accounts/{acc}/folders")).await["folders"].clone();
    let archive = folders.as_array().unwrap().iter().find(|f| f["role"] == "archive").expect("archive folder")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let archived = b.get(&format!("/api/mail/folders/{archive}/messages")).await["messages"].clone();
    assert_eq!(archived[0]["subject"], "Newsletter");
    let archived_id = archived[0]["id"].as_str().unwrap().to_string();
    b.post("/api/mail/actions", json!({"ids": [archived_id], "action": "delete"})).await;
    b.post(&format!("/api/mail/accounts/{acc}/sync"), json!({"folder_id": trash})).await;
    let trashed = b.get(&format!("/api/mail/folders/{trash}/messages")).await["messages"].clone();
    assert_eq!(trashed.as_array().unwrap().len(), 1, "{trashed}");
    let trashed_id = trashed[0]["id"].as_str().unwrap().to_string();
    b.post("/api/mail/actions", json!({"ids": [trashed_id], "action": "delete"})).await;
    check.select("Trash").await.unwrap();
    assert!(check.uid_search("ALL").await.unwrap().is_empty(), "expunged from Trash");
    check.select("INBOX").await.unwrap();
    assert_eq!(check.uid_search("ALL").await.unwrap().len(), 2);

    // UIDVALIDITY change: a folder deleted and recreated under the same name is refetched.
    check.create("Reset-Test").await.unwrap();
    check
        .append("Reset-Test", "()", &msg("x@example.org", &me, "Before reset", "r1@example.org", "", "one\r\n"))
        .await
        .unwrap();
    b.post(&format!("/api/mail/accounts/{acc}/sync"), json!({})).await;
    let folders = b.get(&format!("/api/mail/accounts/{acc}/folders")).await["folders"].clone();
    let reset_id = folders.as_array().unwrap().iter().find(|f| f["name"] == "Reset-Test").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let before = b.get(&format!("/api/mail/folders/{reset_id}/messages")).await["messages"].clone();
    assert_eq!(before[0]["subject"], "Before reset");
    check.delete("Reset-Test").await.unwrap();
    check.create("Reset-Test").await.unwrap();
    check
        .append("Reset-Test", "()", &msg("x@example.org", &me, "After reset", "r2@example.org", "", "two\r\n"))
        .await
        .unwrap();
    b.post(&format!("/api/mail/accounts/{acc}/sync"), json!({"folder_id": reset_id})).await;
    let after = b.get(&format!("/api/mail/folders/{reset_id}/messages")).await["messages"].clone();
    assert_eq!(after.as_array().unwrap().len(), 1, "{after}");
    assert_eq!(after[0]["subject"], "After reset");

    // IDLE: while the owner is online, new mail arrives live with a notification.
    let (conn_id, mut events) = core.hub.connect(&user_id);
    core.ensure_mail_idle(&user_id).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    check
        .append(
            "INBOX",
            "()",
            &msg(
                "Grace Hopper <grace@example.org>",
                &me,
                "Live delivery",
                "live@example.org",
                "",
                "Found a moth.\r\n",
            ),
        )
        .await
        .unwrap();
    let mut got_change = false;
    let mut got_note = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !(got_change && got_note) {
        let ev = tokio::time::timeout_at(deadline, events.recv()).await.expect("live mail events").unwrap();
        let v: Value = serde_json::from_str(&ev).unwrap();
        if v["type"] == "mail.changed" && v["folder_id"] == inbox.as_str() {
            got_change = true;
        }
        if v["type"] == "notification" && v["notification"]["kind"] == "mail" {
            assert_eq!(v["notification"]["title"], "New mail from Grace Hopper");
            assert_eq!(v["notification"]["body"], "Live delivery");
            got_note = true;
        }
    }

    // Bounded connections: at most the pool plus the IDLE watcher.
    assert!(core.mail.open_idle_connections() <= ferrite_core::mail::imap::POOL_SIZE + 1);

    // Drafts: saved to Drafts via APPEND, replaced on re-save.
    let d1 = b
        .post(
            &format!("/api/mail/accounts/{acc}/drafts"),
            json!({"to": [], "subject": "Unfinished", "text": "first version"}),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let d2 = b
        .post(
            &format!("/api/mail/accounts/{acc}/drafts"),
            json!({"to": ["Bob <bob@example.test>"], "bcc": ["carol@example.test"], "subject": "Unfinished", "text": "second version", "draft_id": d1}),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let dlist = b.get(&format!("/api/mail/folders/{drafts}/messages")).await["messages"].clone();
    assert_eq!(dlist.as_array().unwrap().len(), 1, "{dlist}");
    assert_eq!(dlist[0]["id"], d2.as_str());
    let edit = b.get(&format!("/api/mail/messages/{d2}/compose?mode=edit")).await["compose"].clone();
    assert_eq!(edit["bcc"][0], "carol@example.test");
    assert!(edit["text"].as_str().unwrap().contains("second version"));
    check.select("Drafts").await.unwrap();
    assert_eq!(check.uid_search("DRAFT").await.unwrap().len(), 1);

    // Reply with an attachment, sent from the draft; the draft goes away.
    let template = b.get(&format!("/api/mail/messages/{reply_id}/compose?mode=reply_all")).await["compose"].clone();
    assert_eq!(template["subject"], "Re: Engine plans");
    assert_eq!(template["to"][0], "Charles Babbage <charles@example.org>");
    assert!(template["text"].as_str().unwrap().contains("> Yes, with a zebracorn mill."));
    assert!(template["text"].as_str().unwrap().contains("-- \nAlice A."));
    let up = b
        .http
        .post(format!("{url}/api/mail/uploads?name=plan.txt"))
        .header("X-Ferrite-Request", "1")
        .header("Content-Type", "text/plain")
        .body("the plan")
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let upload_id = up["upload"]["id"].as_str().unwrap().to_string();
    let sent_resp = b
        .post(
            &format!("/api/mail/accounts/{acc}/send"),
            json!({"to": [me], "subject": "Re: Engine plans", "text": "Count me in.", "in_reply_to": reply_id,
                   "attachments": [{"kind": "upload", "id": upload_id}], "draft_id": d2}),
        )
        .await;
    let message_id = sent_resp["message_id"].as_str().unwrap().trim_matches(['<', '>']).to_string();
    if let Some(rx) = sink.as_mut() {
        let got = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap();
        assert!(got.data.contains("In-Reply-To: <reply1@example.org>") && got.data.contains("plan.txt"));
    }
    // The Sent copy is filed and cached, threaded with the conversation.
    let sent_list = b.get(&format!("/api/mail/folders/{sent}/messages")).await["messages"].clone();
    assert_eq!(sent_list[0]["subject"], "Re: Engine plans");
    assert_eq!(sent_list[0]["message_id"], message_id.as_str());
    assert_eq!(sent_list[0]["thread_id"], root["thread_id"]);
    check.select("Drafts").await.unwrap();
    assert!(check.uid_search("ALL").await.unwrap().is_empty(), "the draft was removed after sending");
    check.select("INBOX").await.unwrap();
    assert_eq!(check.uid_search("ANSWERED").await.unwrap().len(), 1, "the original is marked answered");
    if t.smtp.is_some() {
        // Delivered through the real SMTP server into the inbox.
        wait_for("the sent message to arrive", || {
            let b = &b;
            let inbox = inbox.clone();
            let message_id = message_id.clone();
            let acc = acc.clone();
            async move {
                let _ = b
                    .call(Method::POST, &format!("/api/mail/accounts/{acc}/sync"), Some(json!({"folder_id": inbox})))
                    .await;
                let l = b.get(&format!("/api/mail/folders/{inbox}/messages")).await["messages"].clone();
                l.as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["message_id"] == message_id.as_str() && m["has_attachments"] == true)
            }
        })
        .await;
        // The conversation shows the message once although it is in Sent and Inbox.
        let thread = b.get(&format!("/api/mail/messages/{reply_id}/thread")).await["messages"].clone();
        assert_eq!(thread.as_array().unwrap().len(), 3, "{thread}");
    }

    // MCP tools.
    let ctx = ferrite_server::tools::UserCtx {
        user: core.user(&user_id).await.unwrap(),
        scopes: "mail".into(),
    };
    let listed = state
        .tools
        .call(&ctx, "mail_list", json!({"folder": "inbox", "unread_only": false}))
        .await
        .unwrap();
    assert!(listed.text_content().contains("Live delivery"), "{}", listed.text_content());
    let read = state
        .tools
        .call(&ctx, "mail_read", json!({"message_id": reply_id, "thread": true}))
        .await
        .unwrap();
    assert!(read.text_content().contains("analytical engine"));
    let names: Vec<String> = state.tools.list(&ctx).iter().map(|t| t.name.clone()).collect();
    assert!(names.contains(&"mail_send".to_string()));
    let send_tool = state.tools.list(&ctx).into_iter().find(|t| t.name == "mail_send").unwrap();
    assert!(send_tool.annotations.is_none_or(|a| a["readOnlyHint"] != true), "sending is not read-only");
    let no_scope = ferrite_server::tools::UserCtx {
        user: ctx.user.clone(),
        scopes: "files".into(),
    };
    let refused = state.tools.call(&no_scope, "mail_list", json!({})).await.unwrap();
    assert!(refused.text_content().contains("`mail` scope"));
    let drafted = state
        .tools
        .call(&ctx, "mail_draft", json!({"reply_to": reply_id, "text": "Drafted by the assistant."}))
        .await
        .unwrap();
    assert!(drafted.text_content().contains("draft_id"), "{}", drafted.text_content());

    // Removing the account drops its cache and search entries.
    core.hub.disconnect(&user_id, conn_id);
    let (s, _) = b.call(Method::DELETE, &format!("/api/mail/accounts/{acc}"), None).await;
    assert!(s.is_success());
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fts WHERE object_kind = 'mail'")
        .fetch_one(&core.db)
        .await
        .unwrap();
    assert_eq!(left, 0);
    check.logout().await;
}
