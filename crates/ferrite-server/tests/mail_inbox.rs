//! Inbox management against a REAL IMAP server. Skipped (passes trivially) unless configured:
//!
//!   MAIL_TEST_DIR=/tmp/ferrite-mail-inbox IMAP_PORT=3311 LMTP_PORT=3312 AUTH_PORT=3313 SIEVE_PORT=3314 \
//!     POSTFIX=0 SHARED=1 EXTRA_USERS=carol bash e2e/mail-server.sh start /tmp/ferrite-mail-inbox
//!   # optional, for the shared-folder check: carol shares her inbox with alice
//!   bash e2e/mail-server.sh deliver carol SOME.eml /tmp/ferrite-mail-inbox
//!   doveadm -c /tmp/ferrite-mail-inbox/dovecot.conf acl set -u carol@example.test INBOX user=alice@example.test lookup read
//!   FERRITE_TEST_IMAP=127.0.0.1:3311:alice@example.test:alice-mail-pw \
//!   FERRITE_TEST_SHARED=shared/carol@example.test \
//!   cargo test -p ferrite-server --test mail_inbox -- --nocapture
//!
//! `FERRITE_TEST_IMAP` is host:port:user:password of a mailbox the test may EMPTY (plain
//! IMAP). `FERRITE_TEST_SHARED` names a folder of another user's namespace that the mailbox
//! can see. Mail is sent to an in-process SMTP sink.
//!
//! Covered, each checked on the IMAP server: tags as keywords (defaults, custom, rename,
//! recolour, delete, sync from another client), important and the priority inbox views, all
//! inboxes, starred view, preferences and admin defaults, folders (create, nest, rename,
//! move, subscribe, mark all read, empty, statistics, repair, delete), shared folders,
//! default-folder choices, snooze and wake-up (and unsnooze), quick actions, junk moves,
//! trash retention, the importance classifier (rules, then a trained model), advanced search
//! with body search on and off, oldest-first pages, Home widgets, deep links after a move,
//! mailto parsing and the MCP tools.

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

/// Empties every own mailbox and removes folders a previous run created.
async fn reset_mailbox(t: &Target) {
    let mut c = ImapConn::connect(&t.settings()).await.expect("IMAP login with FERRITE_TEST_IMAP");
    let mut extra: Vec<String> = Vec::new();
    for f in c.list().await.unwrap() {
        if f.name.starts_with("shared") {
            continue;
        }
        if !["INBOX", "Drafts", "Sent", "Trash", "Junk"].contains(&f.name.as_str()) {
            extra.push(f.name.clone());
            continue;
        }
        let sel = c.select(&f.name).await.unwrap();
        if sel.exists > 0 {
            c.uid_store("1:*", "+FLAGS.SILENT (\\Deleted)").await.unwrap();
            c.expunge("1:*").await.unwrap();
        }
    }
    extra.sort_by_key(|n| std::cmp::Reverse(n.len()));
    for n in extra {
        let _ = c.delete(&n).await;
    }
    for n in c.lsub().await.unwrap() {
        let _ = c.subscribe(&n, false).await;
    }
    for n in ["INBOX", "Drafts", "Sent", "Trash", "Junk"] {
        let _ = c.subscribe(n, true).await;
    }
    c.logout().await;
}

fn msg(from: &str, to: &str, subject: &str, id: &str, date: &str, extra: &str, body: &str) -> Vec<u8> {
    format!("From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\nDate: {date}\r\nMessage-ID: <{id}>\r\nMIME-Version: 1.0\r\n{extra}Content-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n")
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

fn ids_of(v: &Value) -> Vec<String> {
    v.as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap().to_string()).collect()
}

fn find<'a>(list: &'a Value, subject: &str) -> &'a Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|m| m["subject"] == subject)
        .unwrap_or_else(|| panic!("no {subject:?} in {list}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inbox_management_against_a_real_imap_server() {
    let Some(t) = target() else {
        eprintln!("FERRITE_TEST_IMAP is not set; skipping the real-server inbox test (see the top of tests/mail_inbox.rs)");
        return;
    };
    let _ = tracing_subscriber::fmt().with_env_filter("warn,ferrite=info").with_test_writer().try_init();
    reset_mailbox(&t).await;
    let me = t.user.clone();

    // Seed: a conversation partner the user reads and answers, a newsletter, a receipt with an
    // attachment, and an old message for oldest-first paging.
    let mut seed = ImapConn::connect(&t.settings()).await.unwrap();
    let dates = [
        "Mon, 05 Oct 2026 09:00:00 +0000",
        "Tue, 06 Oct 2026 09:00:00 +0000",
        "Wed, 07 Oct 2026 09:00:00 +0000",
    ];
    for (i, d) in dates.iter().enumerate() {
        seed.append(
            "INBOX",
            "(\\Seen \\Answered)",
            &msg(
                "Grace Hopper <grace@example.org>",
                &me,
                &format!("Compiler notes {i}"),
                &format!("grace{i}@example.org"),
                d,
                "",
                "Notes.",
            ),
        )
        .await
        .unwrap();
    }
    seed.append(
        "INBOX",
        "()",
        &msg(
            "Shop <deals@shop.example>",
            &me,
            "Big sale this week",
            "sale1@shop.example",
            "Thu, 08 Oct 2026 09:00:00 +0000",
            "",
            "Buy things.",
        ),
    )
    .await
    .unwrap();
    seed.append(
        "INBOX",
        "(\\Flagged)",
        &msg(
            "Billing <billing@example.net>",
            &me,
            "Your receipt",
            "receipt1@example.net",
            "Fri, 09 Oct 2026 09:00:00 +0000",
            "",
            "Paid: quasar subscription.",
        ),
    )
    .await
    .unwrap();
    seed.append(
        "INBOX",
        "(\\Seen)",
        &msg(
            "Old Friend <old@example.org>",
            &me,
            "Ancient history",
            "old1@example.org",
            "Mon, 05 Jan 2015 09:00:00 +0000",
            "",
            "Remember?",
        ),
    )
    .await
    .unwrap();
    let html = "Content-Type: multipart/mixed; boundary=\"B\"\r\n";
    let body = "--B\r\nContent-Type: text/plain\r\n\r\nSee attached.\r\n--B\r\nContent-Type: text/csv; name=\"q3.csv\"\r\nContent-Disposition: attachment; filename=\"q3.csv\"\r\n\r\na,b\r\n--B--";
    seed.append(
        "INBOX",
        "()",
        format!("From: Ada <ada@example.org>\r\nTo: {me}\r\nCc: Charles <charles@example.org>\r\nSubject: Quarterly figures\r\nDate: Sat, 10 Oct 2026 09:00:00 +0000\r\nMessage-ID: <ada1@example.org>\r\nMIME-Version: 1.0\r\n{html}\r\n{body}\r\n").as_bytes(),
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
    let (smtp_port, _sink) = ferrite_core::mailer::sink::sink().await;
    let account = b
        .post(
            "/api/mail/accounts",
            json!({"email": me, "name": "Alice", "password": t.pass, "imap_host": t.host, "imap_port": t.port, "imap_security": "none",
                   "smtp_host": "127.0.0.1", "smtp_port": smtp_port, "smtp_security": "none", "smtp_auth": false}),
        )
        .await["account"]
        .clone();
    let acc = account["id"].as_str().unwrap().to_string();
    let folders = || async { b.get(&format!("/api/mail/accounts/{acc}/folders")).await["folders"].clone() };
    let role = |fs: &Value, r: &str| {
        fs.as_array()
            .unwrap()
            .iter()
            .find(|f| f["role"] == r)
            .map(|f| f["id"].as_str().unwrap().to_string())
    };
    let named = |fs: &Value, n: &str| {
        fs.as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == n)
            .map(|f| f["id"].as_str().unwrap().to_string())
    };
    let fs = folders().await;
    let inbox = role(&fs, "inbox").unwrap();
    let trash = role(&fs, "trash").unwrap();
    let list = |folder: String| {
        let b = &b;
        async move { b.get(&format!("/api/mail/folders/{folder}/messages?limit=100")).await["messages"].clone() }
    };
    let msgs = list(inbox.clone()).await;
    assert_eq!(msgs.as_array().unwrap().len(), 7, "{msgs}");
    let mut check = ImapConn::connect(&t.settings()).await.unwrap();

    // ---- tags ----------------------------------------------------------------------------
    let tags = b.get("/api/mail/tags").await["tags"].clone();
    let names: Vec<&str> = tags.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Important", "Work", "Personal", "To Do", "Later"]);
    assert_eq!(tags[0]["keyword"], "$label1");
    let custom = b.post("/api/mail/tags", json!({"name": "Project X", "color": "#123456"})).await["tag"].clone();
    assert_eq!(custom["keyword"], "$project_x");
    let (s, _) = b
        .call(Method::POST, "/api/mail/tags", Some(json!({"name": "project x", "color": "#123456"})))
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "names are unique");
    let receipt_id = find(&msgs, "Your receipt")["id"].as_str().unwrap().to_string();
    b.post("/api/mail/keywords", json!({"ids": [receipt_id], "keyword": "$project_x", "on": true}))
        .await;
    check.select("INBOX").await.unwrap();
    assert_eq!(check.uid_search("KEYWORD $project_x").await.unwrap().len(), 1, "the tag is an IMAP keyword");
    // Another client tags a message: picked up by the next sync.
    let sale_uid = check.uid_search("SUBJECT \"Big sale\"").await.unwrap()[0];
    check.uid_store(&sale_uid.to_string(), "+FLAGS.SILENT ($label2)").await.unwrap();
    b.post(&format!("/api/mail/accounts/{acc}/sync"), json!({"folder_id": inbox})).await;
    let msgs = list(inbox.clone()).await;
    assert_eq!(find(&msgs, "Big sale this week")["keywords"], json!(["$label2"]));
    assert_eq!(find(&msgs, "Your receipt")["keywords"], json!(["$project_x"]));
    // Tag view (cache and server).
    let tagged = b.get("/api/mail/views/tag:$label2/messages").await["messages"].clone();
    assert_eq!(ids_of(&tagged), vec![find(&msgs, "Big sale this week")["id"].as_str().unwrap().to_string()]);
    // Rename and recolour keep the keyword; defaults can't be deleted; deleting a custom tag
    // takes it off the server's messages.
    let custom_id = custom["id"].as_str().unwrap();
    let renamed = b
        .send(
            Method::PATCH,
            &format!("/api/mail/tags/{custom_id}"),
            json!({"name": "Project Y", "color": "#abcdef"}),
        )
        .await["tag"]
        .clone();
    assert_eq!((renamed["name"].as_str(), renamed["keyword"].as_str()), (Some("Project Y"), Some("$project_x")));
    let work_id = tags[1]["id"].as_str().unwrap();
    b.send(Method::PATCH, &format!("/api/mail/tags/{work_id}"), json!({"name": "Job"})).await;
    let (s, _) = b.call(Method::DELETE, &format!("/api/mail/tags/{work_id}"), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = b.call(Method::DELETE, &format!("/api/mail/tags/{custom_id}"), None).await;
    assert!(s.is_success());
    check.select("INBOX").await.unwrap();
    assert!(
        check.uid_search("KEYWORD $project_x").await.unwrap().is_empty(),
        "deleted tag removed on the server"
    );

    // ---- important, priority inbox, all inboxes, starred ---------------------------------
    let ada_id = find(&msgs, "Quarterly figures")["id"].as_str().unwrap().to_string();
    b.post("/api/mail/keywords", json!({"ids": [ada_id], "keyword": "$label1", "on": true})).await;
    check.select("INBOX").await.unwrap();
    assert_eq!(check.uid_search("KEYWORD $label1").await.unwrap().len(), 1);
    let important = b.get("/api/mail/views/important/messages").await["messages"].clone();
    assert_eq!(ids_of(&important), vec![ada_id.clone()]);
    let others = b.get("/api/mail/views/others/messages").await["messages"].clone();
    assert_eq!(others.as_array().unwrap().len(), 6);
    assert!(!ids_of(&others).contains(&ada_id));
    let all = b.get("/api/mail/views/all/messages?limit=3").await;
    assert_eq!(all["messages"].as_array().unwrap().len(), 3);
    assert_eq!(all["has_more"], true);
    let last = all["messages"][2]["id"].as_str().unwrap();
    let rest = b.get(&format!("/api/mail/views/all/messages?before={last}")).await["messages"].clone();
    assert_eq!(rest.as_array().unwrap().len(), 4, "the cursor continues where the page ended");
    let oldest_all = b.get("/api/mail/views/all/messages?sort=oldest&limit=1").await["messages"].clone();
    assert_eq!(oldest_all[0]["subject"], "Ancient history");
    let starred = b.get("/api/mail/views/starred/messages").await["messages"].clone();
    assert_eq!(ids_of(&starred), vec![receipt_id.clone()]);
    let unread_important = b.get("/api/mail/views/important/messages?unread=true").await["messages"].clone();
    assert_eq!(unread_important.as_array().unwrap().len(), 1);

    // ---- preferences and admin defaults ---------------------------------------------------
    let p = b.get("/api/mail/preferences").await["prefs"].clone();
    assert_eq!(
        (p["layout"].as_str(), p["threaded"].as_bool(), p["sort"].as_str()),
        (Some("vertical"), Some(false), Some("newest"))
    );
    let p = b
        .send(Method::PUT, "/api/mail/preferences", json!({"layout": "horizontal", "compact": true}))
        .await["prefs"]
        .clone();
    assert_eq!((p["layout"].as_str(), p["compact"].as_bool()), (Some("horizontal"), Some(true)));
    let (s, _) = b.call(Method::PUT, "/api/mail/preferences", Some(json!({"layout": "diagonal"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    b.send(Method::PUT, "/api/mail/admin/defaults", json!({"sort": "oldest", "classify": true}))
        .await;
    let p = b.get("/api/mail/preferences").await["prefs"].clone();
    assert_eq!(
        (p["sort"].as_str(), p["layout"].as_str()),
        (Some("oldest"), Some("horizontal")),
        "admin default, own choice kept"
    );
    b.send(Method::PUT, "/api/mail/admin/defaults", json!({"sort": "newest"})).await;

    // ---- folders ---------------------------------------------------------------------------
    let projects = b.post(&format!("/api/mail/accounts/{acc}/mailboxes"), json!({"name": "Projects"})).await["folder"].clone();
    let projects_id = projects["id"].as_str().unwrap().to_string();
    let sub = b
        .post(
            &format!("/api/mail/accounts/{acc}/mailboxes"),
            json!({"name": "Entwürfe 2026", "parent_id": projects_id}),
        )
        .await["folder"]
        .clone();
    assert_eq!(sub["name"], "Projects/Entw&APw-rfe 2026");
    assert_eq!(sub["display_name"], "Entwürfe 2026");
    let sub_id = sub["id"].as_str().unwrap().to_string();
    let names = check.list().await.unwrap().into_iter().map(|f| f.name).collect::<Vec<_>>();
    assert!(names.contains(&"Projects/Entw&APw-rfe 2026".to_string()), "{names:?}");
    // Move a message into the subfolder, then rename the parent: ids and messages stay.
    b.post(
        "/api/mail/actions",
        json!({"ids": [find(&msgs, "Big sale this week")["id"]], "action": "move", "folder_id": sub_id}),
    )
    .await;
    wait_for("the moved message", || {
        let (b, sub_id) = (&b, sub_id.clone());
        async move {
            b.get(&format!("/api/mail/folders/{sub_id}/messages")).await["messages"]
                .as_array()
                .unwrap()
                .len()
                == 1
        }
    })
    .await;
    let renamed = b
        .send(Method::PATCH, &format!("/api/mail/folders/{projects_id}"), json!({"name": "Work Projects"}))
        .await["folder"]
        .clone();
    assert_eq!(
        (renamed["id"].as_str(), renamed["name"].as_str()),
        (Some(projects_id.as_str()), Some("Work Projects"))
    );
    let fs = folders().await;
    assert_eq!(
        named(&fs, "Work Projects/Entw&APw-rfe 2026").as_deref(),
        Some(sub_id.as_str()),
        "the subfolder moved along"
    );
    assert_eq!(list(sub_id.clone()).await.as_array().unwrap().len(), 1, "cached messages kept");
    // Move the subfolder to the top level, and back under the parent.
    let moved = b.send(Method::PATCH, &format!("/api/mail/folders/{sub_id}"), json!({"parent_id": ""})).await["folder"].clone();
    assert_eq!(moved["name"], "Entw&APw-rfe 2026");
    let (s, _) = b
        .call(
            Method::PATCH,
            &format!("/api/mail/folders/{projects_id}"),
            Some(json!({"parent_id": projects_id})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "not into itself");
    b.send(Method::PATCH, &format!("/api/mail/folders/{sub_id}"), json!({"parent_id": projects_id}))
        .await;
    // Subscriptions.
    b.send(Method::PATCH, &format!("/api/mail/folders/{sub_id}"), json!({"subscribed": false}))
        .await;
    let subs = check.lsub().await.unwrap();
    assert!(
        !subs.contains(&"Work Projects/Entw&APw-rfe 2026".to_string()) && subs.contains(&"Work Projects".to_string()),
        "{subs:?}"
    );
    b.post(&format!("/api/mail/accounts/{acc}/sync"), json!({})).await;
    let fs = folders().await;
    let sub_row = fs.as_array().unwrap().iter().find(|f| f["id"] == sub_id.as_str()).unwrap();
    assert_eq!(sub_row["subscribed"], false);
    b.send(Method::PATCH, &format!("/api/mail/folders/{sub_id}"), json!({"subscribed": true})).await;
    // Statistics, mark all read, repair, empty.
    let stats = b.get(&format!("/api/mail/folders/{sub_id}/stats")).await["stats"].clone();
    assert_eq!(
        (stats["total"].as_i64(), stats["unread"].as_i64(), stats["cached"].as_i64()),
        (Some(1), Some(1), Some(1))
    );
    b.post(&format!("/api/mail/folders/{sub_id}/read-all"), json!({})).await;
    check.select("Work Projects/Entw&APw-rfe 2026").await.unwrap();
    assert!(check.uid_search("UNSEEN").await.unwrap().is_empty());
    b.post(&format!("/api/mail/folders/{sub_id}/repair"), json!({})).await;
    let after = list(sub_id.clone()).await;
    assert_eq!(after.as_array().unwrap().len(), 1, "refetched after repair");
    assert_eq!(after[0]["keywords"], json!(["$label2"]), "keywords come back with the envelope");
    let emptied = b.post(&format!("/api/mail/folders/{sub_id}/empty"), json!({})).await;
    assert_eq!(emptied["deleted"], 1);
    check.select("Work Projects/Entw&APw-rfe 2026").await.unwrap();
    assert!(check.uid_search("ALL").await.unwrap().is_empty());
    // Delete with subfolders; the inbox can't be. (Servers end sessions that have a deleted
    // mailbox selected, so this client steps out of it first.)
    check.select("INBOX").await.unwrap();
    let (s, _) = b.call(Method::DELETE, &format!("/api/mail/folders/{inbox}"), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, v) = b.call(Method::DELETE, &format!("/api/mail/folders/{projects_id}"), None).await;
    assert!(s.is_success(), "{v}");
    let names = check.list().await.unwrap().into_iter().map(|f| f.name).collect::<Vec<_>>();
    assert!(!names.iter().any(|n| n.starts_with("Work Projects")), "{names:?}");

    // ---- shared folders ---------------------------------------------------------------------
    if let Ok(shared_name) = std::env::var("FERRITE_TEST_SHARED") {
        wait_for("namespaces", || {
            let core = core.clone();
            let acc = acc.clone();
            async move {
                sqlx::query_scalar::<_, Option<String>>("SELECT namespaces FROM mail_account_prefs WHERE account_id = ?")
                    .bind(&acc)
                    .fetch_optional(&core.db)
                    .await
                    .unwrap()
                    .flatten()
                    .is_some()
            }
        })
        .await;
        b.post(&format!("/api/mail/accounts/{acc}/sync"), json!({})).await;
        let fs = folders().await;
        let shared = fs
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == shared_name.as_str())
            .unwrap_or_else(|| panic!("no {shared_name} in {fs}"));
        assert_eq!(shared["shared"], true);
        assert!(
            fs.as_array()
                .unwrap()
                .iter()
                .filter(|f| f["shared"] == true)
                .all(|f| f["name"].as_str().unwrap().starts_with("shared"))
        );
        assert_eq!(fs[0]["shared"], false);
        let shared_msgs = list(shared["id"].as_str().unwrap().to_string()).await;
        assert!(!shared_msgs.as_array().unwrap().is_empty(), "the shared inbox can be read");
    }

    // ---- default folders ------------------------------------------------------------------
    let arch = b.post(&format!("/api/mail/accounts/{acc}/mailboxes"), json!({"name": "Old Mail"})).await["folder"].clone();
    let arch_id = arch["id"].as_str().unwrap().to_string();
    let st = b
        .send(
            Method::PATCH,
            &format!("/api/mail/accounts/{acc}/inbox-settings"),
            json!({"roles": {"archive": arch_id}}),
        )
        .await;
    assert_eq!(st["settings"]["roles"]["archive"], arch_id.as_str());
    b.post(&format!("/api/mail/accounts/{acc}/sync"), json!({})).await;
    assert_eq!(
        role(&folders().await, "archive").as_deref(),
        Some(arch_id.as_str()),
        "the choice survives a folder sync"
    );
    let grace0 = find(&list(inbox.clone()).await, "Compiler notes 0")["id"].as_str().unwrap().to_string();
    b.post("/api/mail/actions", json!({"ids": [grace0], "action": "archive"})).await;
    check.select("Old Mail").await.unwrap();
    assert_eq!(check.uid_search("ALL").await.unwrap().len(), 1, "archived into the chosen folder");

    // ---- deep links survive a move ---------------------------------------------------------
    let resolved = b.get(&format!("/api/mail/resolve?m={grace0}&mid=grace0@example.org")).await;
    let new_id = resolved["id"].as_str().unwrap();
    assert_ne!(new_id, grace0);
    let (_, m) = core.message_row(&user_id, new_id).await.unwrap();
    assert_eq!(m.folder_id, arch_id);
    let (s, _) = b.call(Method::GET, "/api/mail/resolve?m=nope&mid=nobody@nowhere", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // ---- snooze ----------------------------------------------------------------------------
    let receipt = find(&list(inbox.clone()).await, "Your receipt")["id"].as_str().unwrap().to_string();
    b.post("/api/mail/actions", json!({"ids": [receipt], "action": "read"})).await;
    let until = (chrono::Utc::now() + chrono::Duration::hours(3)).to_rfc3339();
    b.post("/api/mail/snooze", json!({"ids": [receipt], "until": until})).await;
    let snoozed_folder = role(&folders().await, "snoozed").expect("a Snoozed folder was created");
    check.select("Snoozed").await.unwrap();
    assert_eq!(check.uid_search("ALL").await.unwrap().len(), 1, "moved to Snoozed on the server");
    let listed = b.get("/api/mail/snoozed").await["snoozed"].clone();
    assert_eq!(
        (listed[0]["message_id"].as_str(), listed[0]["src_folder"].as_str()),
        (Some("receipt1@example.net"), Some("INBOX"))
    );
    // Nothing is due yet; then the time comes (as after a restart: the schedule is stored).
    assert_eq!(core.wake_snoozed().await.unwrap(), 0);
    sqlx::query("UPDATE mail_snoozed SET until = ?")
        .bind(chrono::Utc::now() - chrono::Duration::minutes(1))
        .execute(&core.db)
        .await
        .unwrap();
    assert_eq!(core.wake_snoozed().await.unwrap(), 1);
    check.select("INBOX").await.unwrap();
    let back = check.uid_search("HEADER Message-ID \"<receipt1@example.net>\" UNSEEN").await.unwrap();
    assert_eq!(back.len(), 1, "back in the inbox, unread");
    check.select("Snoozed").await.unwrap();
    assert!(check.uid_search("ALL").await.unwrap().is_empty());
    assert!(b.get("/api/mail/snoozed").await["snoozed"].as_array().unwrap().is_empty());
    // Unsnooze by hand.
    b.post(&format!("/api/mail/accounts/{acc}/sync"), json!({"folder_id": inbox})).await;
    let receipt = find(&list(inbox.clone()).await, "Your receipt")["id"].as_str().unwrap().to_string();
    b.post("/api/mail/snooze", json!({"ids": [receipt], "until": until})).await;
    b.post(&format!("/api/mail/accounts/{acc}/sync"), json!({"folder_id": snoozed_folder})).await;
    let in_snoozed = list(snoozed_folder.clone()).await;
    b.post("/api/mail/unsnooze", json!({"ids": ids_of(&in_snoozed)})).await;
    check.select("INBOX").await.unwrap();
    assert_eq!(check.uid_search("HEADER Message-ID \"<receipt1@example.net>\"").await.unwrap().len(), 1);
    let presets = b.get("/api/mail/snooze/presets?tz=60").await["presets"].clone();
    assert!(presets.as_array().unwrap().iter().any(|p| p["key"] == "tomorrow"));

    // ---- quick actions ---------------------------------------------------------------------
    let todo_dest = b.post(&format!("/api/mail/accounts/{acc}/mailboxes"), json!({"name": "Follow up"})).await["folder"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (s, _) = b
        .call(
            Method::POST,
            &format!("/api/mail/accounts/{acc}/quick-actions"),
            Some(json!({"name": "Bad", "steps": [{"type": "delete"}, {"type": "read"}]})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let qa = b
        .post(
            &format!("/api/mail/accounts/{acc}/quick-actions"),
            json!({"name": "To do later", "steps": [{"type": "unread"}, {"type": "flag"}, {"type": "tag", "keyword": "$label4"}, {"type": "move", "folder_id": todo_dest}]}),
        )
        .await["quick_action"]
        .clone();
    let listed = b.get(&format!("/api/mail/quick-actions?account_id={acc}")).await["quick_actions"].clone();
    assert_eq!(listed[0]["name"], "To do later");
    let grace1 = find(&list(inbox.clone()).await, "Compiler notes 1")["id"].as_str().unwrap().to_string();
    b.post(&format!("/api/mail/quick-actions/{}/run", qa["id"].as_str().unwrap()), json!({"ids": [grace1]}))
        .await;
    check.select("Follow up").await.unwrap();
    assert_eq!(
        check.uid_search("UNSEEN FLAGGED KEYWORD $label4").await.unwrap().len(),
        1,
        "every step ran on the server"
    );

    // ---- junk ------------------------------------------------------------------------------
    let ancient = find(&list(inbox.clone()).await, "Ancient history")["id"].as_str().unwrap().to_string();
    b.post("/api/mail/junk", json!({"ids": [ancient], "junk": true})).await;
    check.select("Junk").await.unwrap();
    assert_eq!(check.uid_search("KEYWORD $Junk").await.unwrap().len(), 1, "moved to Junk with $Junk");
    let junk_id = role(&folders().await, "junk").unwrap();
    b.post(&format!("/api/mail/accounts/{acc}/sync"), json!({"folder_id": junk_id})).await;
    let in_junk = list(junk_id.clone()).await;
    b.post("/api/mail/junk", json!({"ids": ids_of(&in_junk), "junk": false})).await;
    check.select("INBOX").await.unwrap();
    assert_eq!(check.uid_search("KEYWORD $NotJunk").await.unwrap().len(), 1, "back in the inbox as not junk");

    // ---- trash retention -------------------------------------------------------------------
    b.send(
        Method::PATCH,
        &format!("/api/mail/accounts/{acc}/inbox-settings"),
        json!({"trash_retention_days": 7}),
    )
    .await;
    b.post(&format!("/api/mail/accounts/{acc}/sync"), json!({"folder_id": inbox})).await;
    let grace2 = find(&list(inbox.clone()).await, "Compiler notes 2")["id"].as_str().unwrap().to_string();
    b.post("/api/mail/actions", json!({"ids": [grace2], "action": "delete"})).await;
    let a = core.mail_account(&user_id, &acc).await.unwrap();
    assert_eq!(core.run_trash_retention(&a).await.unwrap(), 0, "just arrived in Trash");
    sqlx::query("UPDATE mail_trash_seen SET seen_at = ?")
        .bind(chrono::Utc::now() - chrono::Duration::days(8))
        .execute(&core.db)
        .await
        .unwrap();
    assert_eq!(core.run_trash_retention(&a).await.unwrap(), 1);
    check.select("Trash").await.unwrap();
    assert!(check.uid_search("ALL").await.unwrap().is_empty(), "expired from Trash");
    assert!(list(trash.clone()).await.as_array().unwrap().is_empty());

    // ---- importance classifier --------------------------------------------------------------
    // Rules first: Grace's mail is read and answered, so her next message is important; the
    // shop's is not.
    let st = b.get(&format!("/api/mail/accounts/{acc}/inbox-settings")).await;
    assert_eq!(
        (st["settings"]["classify"].as_bool(), st["classifier"]["mode"].as_str()),
        (Some(true), Some("rules"))
    );
    check
        .append(
            "INBOX",
            "()",
            &msg(
                "Grace Hopper <grace@example.org>",
                &me,
                "Compiler notes 3",
                "grace3@example.org",
                "Sun, 11 Oct 2026 09:00:00 +0000",
                "",
                "More notes.",
            ),
        )
        .await
        .unwrap();
    check
        .append(
            "INBOX",
            "()",
            &msg(
                "Shop <deals@shop.example>",
                &me,
                "Another sale",
                "sale2@shop.example",
                "Sun, 11 Oct 2026 10:00:00 +0000",
                "",
                "Buy more.",
            ),
        )
        .await
        .unwrap();
    b.post(&format!("/api/mail/accounts/{acc}/sync"), json!({"folder_id": inbox})).await;
    wait_for("the classifier to tag new mail", || {
        let t = &t;
        async move {
            let mut c = ImapConn::connect(&t.settings()).await.unwrap();
            c.select("INBOX").await.unwrap();
            let found = c.uid_search("KEYWORD $label1 HEADER Message-ID \"<grace3@example.org>\"").await.unwrap().len() == 1;
            c.logout().await;
            found
        }
    })
    .await;
    check.select("INBOX").await.unwrap();
    assert!(
        check
            .uid_search("KEYWORD $label1 HEADER Message-ID \"<sale2@shop.example>\"")
            .await
            .unwrap()
            .is_empty(),
        "the shop's mail is not important"
    );
    // Then a learned model: 24 important messages from a boss, 30 newsletters left alone.
    let mut batch = ImapConn::connect(&t.settings()).await.unwrap();
    for i in 0..24 {
        batch
            .append(
                "INBOX",
                "(\\Seen $label1)",
                &msg(
                    "Boss <boss@corp.example>",
                    &me,
                    &format!("Budget review {i}"),
                    &format!("boss{i}@corp.example"),
                    "Mon, 12 Oct 2026 09:00:00 +0000",
                    "",
                    "Numbers.",
                ),
            )
            .await
            .unwrap();
    }
    for i in 0..30 {
        batch
            .append(
                "INBOX",
                "(\\Seen)",
                &msg(
                    "Digest <digest@news.example>",
                    &me,
                    &format!("Weekly digest {i}"),
                    &format!("digest{i}@news.example"),
                    "Mon, 12 Oct 2026 10:00:00 +0000",
                    "",
                    "News.",
                ),
            )
            .await
            .unwrap();
    }
    batch.logout().await;
    b.post(&format!("/api/mail/accounts/{acc}/sync"), json!({"folder_id": inbox})).await;
    let trained = b.post(&format!("/api/mail/accounts/{acc}/classifier"), json!({})).await["classifier"].clone();
    assert_eq!(trained["mode"], "model", "{trained}");
    assert!(trained["important"].as_i64().unwrap() >= 24);
    assert!(trained["accuracy"].as_f64().unwrap() > 0.8, "{trained}");
    let new_ids: Vec<String> = {
        check
            .append(
                "INBOX",
                "()",
                &msg(
                    "Boss <boss@corp.example>",
                    &me,
                    "Budget review final",
                    "boss-final@corp.example",
                    "Tue, 13 Oct 2026 09:00:00 +0000",
                    "",
                    "Final.",
                ),
            )
            .await
            .unwrap();
        check
            .append(
                "INBOX",
                "()",
                &msg(
                    "Digest <digest@news.example>",
                    &me,
                    "Weekly digest final",
                    "digest-final@news.example",
                    "Tue, 13 Oct 2026 10:00:00 +0000",
                    "",
                    "News.",
                ),
            )
            .await
            .unwrap();
        b.post(&format!("/api/mail/accounts/{acc}/sync"), json!({"folder_id": inbox})).await;
        let l = list(inbox.clone()).await;
        vec![
            find(&l, "Budget review final")["id"].as_str().unwrap().to_string(),
            find(&l, "Weekly digest final")["id"].as_str().unwrap().to_string(),
        ]
    };
    let tagged = core.classify_messages(&a, &new_ids).await.unwrap();
    assert!(tagged.is_empty() || tagged == vec![new_ids[0].clone()], "{tagged:?}");
    wait_for("the boss's mail to be important", || {
        let (core, uid, id) = (core.clone(), user_id.clone(), new_ids[0].clone());
        async move { core.message_row(&uid, &id).await.unwrap().1.keywords.contains("$label1") }
    })
    .await;
    assert!(!core.message_row(&user_id, &new_ids[1]).await.unwrap().1.keywords.contains("$label1"));
    // Marking something important teaches it (retraining is scheduled), and it can be off.
    b.post("/api/mail/keywords", json!({"ids": [new_ids[1]], "keyword": "$label1", "on": true}))
        .await;
    let dirty: bool = sqlx::query_scalar("SELECT dirty FROM mail_classifiers WHERE account_id = ?")
        .bind(&acc)
        .fetch_one(&core.db)
        .await
        .unwrap();
    assert!(dirty);
    b.send(Method::PATCH, &format!("/api/mail/accounts/{acc}/inbox-settings"), json!({"classify": false}))
        .await;
    assert!(core.classify_messages(&a, &new_ids).await.unwrap().is_empty(), "switched off");

    // ---- advanced search --------------------------------------------------------------------
    let found = b.get("/api/mail/search/advanced?from=ada&attachments=true").await["messages"].clone();
    assert_eq!(ids_of(&found), vec![ada_id.clone()]);
    let found = b.get("/api/mail/search/advanced?cc=charles&since=2026-10-10&until=2026-10-10").await["messages"].clone();
    assert_eq!(ids_of(&found), vec![ada_id.clone()]);
    let found = b.get("/api/mail/search/advanced?subject=quarterly&tags=$label1,$label3").await["messages"].clone();
    assert_eq!(ids_of(&found), vec![ada_id.clone()]);
    let found = b.get("/api/mail/search/advanced?q=quasar").await["messages"].clone();
    assert_eq!(found[0]["subject"], "Your receipt", "body text found with body search on");
    let found = b.get("/api/mail/search/advanced?q=quasar&unified=true&flagged=true").await["messages"].clone();
    assert_eq!(found.as_array().unwrap().len(), 1);
    b.send(Method::PUT, "/api/mail/preferences", json!({"body_search": false})).await;
    let found = b.get("/api/mail/search/advanced?q=quasar").await["messages"].clone();
    assert!(found.as_array().unwrap().is_empty(), "body text not searched with body search off: {found}");
    let found = b.get("/api/mail/search/advanced?q=receipt").await["messages"].clone();
    assert_eq!(found[0]["subject"], "Your receipt");
    let (s, _) = b.call(Method::GET, "/api/mail/search/advanced?body=quasar", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    b.send(Method::PUT, "/api/mail/preferences", json!({"body_search": true})).await;

    // ---- oldest first -----------------------------------------------------------------------
    let p1 = b.get(&format!("/api/mail/folders/{inbox}/messages?sort=oldest&limit=2")).await;
    assert_eq!(p1["messages"][0]["subject"], "Ancient history");
    assert_eq!(p1["has_more"], true);
    let p2 = b.get(&format!("/api/mail/folders/{inbox}/messages?sort=oldest&limit=2&offset=2")).await;
    assert!(!ids_of(&p2["messages"]).iter().any(|id| ids_of(&p1["messages"]).contains(id)));

    // ---- widgets and mailto -----------------------------------------------------------------
    let w = b.get("/api/mail/widgets").await;
    assert_eq!(w["accounts"], 1);
    assert!(w["unread_count"].as_i64().unwrap() >= 1);
    assert!(!w["important"].as_array().unwrap().is_empty());
    let m = b
        .get("/api/mail/mailto?uri=mailto%3Abob%40example.org%3Fcc%3Dcarol%40example.org%26subject%3DHi%2520there%26body%3DLine%2520one")
        .await["mailto"]
        .clone();
    assert_eq!(
        m,
        json!({"to": ["bob@example.org"], "cc": ["carol@example.org"], "bcc": [], "subject": "Hi there", "body": "Line one"})
    );

    // ---- MCP tools --------------------------------------------------------------------------
    let ctx = ferrite_server::tools::UserCtx {
        user: core.user(&user_id).await.unwrap(),
        scopes: "mail".into(),
    };
    let tools = state.tools.list(&ctx);
    let tool = |n: &str| tools.iter().find(|t| t.name == n).unwrap_or_else(|| panic!("no tool {n}")).clone();
    assert_eq!(tool("mail_important").annotations.unwrap()["readOnlyHint"], true);
    assert_eq!(tool("mail_tags").annotations.unwrap()["readOnlyHint"], true);
    assert_eq!(tool("mail_folder_delete").annotations.unwrap()["destructiveHint"], true);
    assert!(tool("mail_tag").annotations.is_none_or(|a| a["readOnlyHint"] != true));
    let call = |name: &'static str, args: Value| {
        let (state, ctx) = (state.clone(), ctx.clone());
        async move { state.tools.call(&ctx, name, args).await.unwrap().text_content() }
    };
    let out = call("mail_important", json!({"limit": 100})).await;
    assert!(out.contains("Quarterly figures"), "{out}");
    let out = call("mail_tag", json!({"message_ids": [ada_id], "tag": "To Do"})).await;
    assert!(out.contains("Added “To Do”"), "{out}");
    check.select("INBOX").await.unwrap();
    assert_eq!(
        check
            .uid_search("KEYWORD $label4 HEADER Message-ID \"<ada1@example.org>\"")
            .await
            .unwrap()
            .len(),
        1
    );
    call("mail_untag", json!({"message_ids": [ada_id], "tag": "$label4"})).await;
    check.select("INBOX").await.unwrap();
    assert!(
        check
            .uid_search("KEYWORD $label4 HEADER Message-ID \"<ada1@example.org>\"")
            .await
            .unwrap()
            .is_empty()
    );
    let out = call("mail_folder_create", json!({"name": "Assistant"})).await;
    assert!(out.contains("Assistant"), "{out}");
    let out = call("mail_folder_rename", json!({"folder": "Assistant", "new_name": "Assistant Notes"})).await;
    assert!(out.contains("Assistant Notes"), "{out}");
    let out = call("mail_mark_all_read", json!({"folder": "inbox"})).await;
    assert!(out.contains("Marked every message"), "{out}");
    check.select("INBOX").await.unwrap();
    assert!(check.uid_search("UNSEEN").await.unwrap().is_empty());
    let out = call(
        "mail_snooze",
        json!({"message_ids": [ada_id], "until": (chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339()}),
    )
    .await;
    assert!(out.contains("Snoozed 1"), "{out}");
    check.select("Snoozed").await.unwrap();
    assert_eq!(check.uid_search("ALL").await.unwrap().len(), 1);
    let out = call("mail_folder_delete", json!({"folder": "Assistant Notes"})).await;
    assert!(out.contains("Deleted"), "{out}");
    let names = check.list().await.unwrap().into_iter().map(|f| f.name).collect::<Vec<_>>();
    assert!(!names.iter().any(|n| n.starts_with("Assistant")), "{names:?}");
    let out = call("mail_tags", json!({})).await;
    assert!(out.contains("Job") && out.contains("$label2"), "{out}");

    // Bounded connections.
    assert!(core.mail.open_idle_connections() <= ferrite_core::mail::imap::POOL_SIZE + 1);
    check.logout().await;
}
