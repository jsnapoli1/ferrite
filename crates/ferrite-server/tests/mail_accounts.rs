//! Mail accounts, delegation, provisioning and administration against REAL IMAP servers.
//! Skipped (passes trivially) unless configured:
//!
//!   # Dovecot with four mailboxes and a 10 MiB quota each:
//!   MAIL_TEST_DIR=/tmp/fm-acc IMAP_PORT=3351 LMTP_PORT=3352 AUTH_PORT=3353 SIEVE_PORT=3354 POSTFIX=0 QUOTA=1 \
//!     EXTRA_USERS="carol dave" bash e2e/mail-server.sh start /tmp/fm-acc
//!   # A second Dovecot that also accepts OAuth bearer tokens, validated by the mock provider
//!   # this test runs on 127.0.0.1:3365:
//!   MAIL_TEST_DIR=/tmp/fm-oauth IMAP_PORT=3361 LMTP_PORT=3362 AUTH_PORT=3363 SIEVE_PORT=3364 POSTFIX=0 \
//!     EXTRA_USERS="carol dave" OAUTH2_INTROSPECTION_URL=http://127.0.0.1:3365/introspect bash e2e/mail-server.sh start /tmp/fm-oauth
//!   FERRITE_TEST_IMAP=127.0.0.1:3351:alice@example.test:alice-mail-pw \
//!   FERRITE_TEST_OAUTH_IMAP=127.0.0.1:3361 FERRITE_TEST_OAUTH_PROVIDER=127.0.0.1:3365 \
//!   cargo test -p ferrite-server --test mail_accounts -- --nocapture
//!
//! The mailboxes alice, bob, carol and dave @example.test (password `<name>-mail-pw`) are
//! EMPTIED. SMTP goes to in-process sinks that check AUTH PLAIN / XOAUTH2 credentials.
//!
//! Covered: delegation (rights matrix over HTTP and MCP, abuse cases: re-authenticating,
//! removing, re-sharing, ids of the owner's other accounts riding along, strangers; live
//! events, search, revocation), account order, quota, report spam / not spam, anti-abuse
//! alerts, admin settings (allow local servers, allow user accounts), connectivity test,
//! provisioning (login password and master password modes, group and domain matching,
//! deprovisioning, what users may change), background sync with notifications while
//! offline; and OAuth 2.0: the consent flow with PKCE against a mock provider, state bound
//! to the session and used once, XOAUTH2 to Dovecot (bearer tokens checked by token
//! introspection) and SMTP, tokens sealed at rest, refresh before expiry.

use axum::extract::{Form, Query, State};
use axum::response::{IntoResponse, Redirect};
use ferrite_core::mail::ServerSettings;
use ferrite_core::mail::imap::ImapConn;
use ferrite_core::mailer::SmtpSecurity;
use ferrite_core::mailer::sink::{Check, Received, sink_with_auth};
use ferrite_core::{Config, Core};
use ferrite_server::AppState;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

fn imap_target() -> Option<(String, u16)> {
    let v = std::env::var("FERRITE_TEST_IMAP").ok()?;
    let mut p = v.split(':');
    Some((p.next()?.to_string(), p.next()?.parse().ok()?))
}

fn settings(host: &str, port: u16, user: &str) -> ServerSettings {
    let name = user.split('@').next().unwrap_or(user);
    ServerSettings {
        host: host.into(),
        port,
        security: SmtpSecurity::None,
        username: user.into(),
        password: format!("{name}-mail-pw"),
        xoauth2: false,
    }
}

/// Empties every folder of a mailbox and removes folders a previous run created.
async fn reset(host: &str, port: u16, user: &str) {
    let mut c = ImapConn::connect(&settings(host, port, user)).await.expect("IMAP login");
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

fn msg(from: &str, to: &str, subject: &str, id: &str, body: &str) -> Vec<u8> {
    format!("From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\nDate: Tue, 20 Oct 2026 09:00:00 +0000\r\nMessage-ID: <{id}>\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n").into_bytes()
}

async fn append(host: &str, port: u16, user: &str, folder: &str, raw: &[u8]) {
    let mut c = ImapConn::connect(&settings(host, port, user)).await.unwrap();
    c.append(folder, "()", raw).await.unwrap();
    c.logout().await;
}

struct Browser {
    http: reqwest::Client,
    url: String,
}

impl Browser {
    fn new(url: &str) -> Browser {
        Browser {
            http: reqwest::Client::builder()
                .cookie_store(true)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
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
    async fn send(&self, method: Method, path: &str, body: Value) -> Value {
        let (s, v) = self.call(method.clone(), path, Some(body)).await;
        assert!(s.is_success(), "{method} {path}: {s} {v}");
        v
    }
    async fn post(&self, path: &str, body: Value) -> Value {
        self.send(Method::POST, path, body).await
    }
    async fn status(&self, method: Method, path: &str, body: Option<Value>) -> StatusCode {
        self.call(method, path, body).await.0
    }
    async fn login(&self, user: &str, password: &str) {
        self.post("/api/auth/login", json!({"username": user, "password": password})).await;
    }
}

async fn start_server() -> (Core, AppState, String, tempfile::TempDir) {
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
    (core, state, url, dir)
}

async fn next_mail(rx: &mut UnboundedReceiver<Received>) -> Received {
    tokio::time::timeout(Duration::from_secs(15), rx.recv())
        .await
        .expect("a message reached the SMTP sink")
        .unwrap()
}

fn find<'a>(list: &'a Value, key: &str, value: &str) -> &'a Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|x| x[key] == value)
        .unwrap_or_else(|| panic!("no {key}={value} in {list}"))
}

async fn folder_id(b: &Browser, account: &str, role: &str) -> String {
    let folders = b.get(&format!("/api/mail/accounts/{account}/folders")).await["folders"].clone();
    find(&folders, "role", role)["id"].as_str().unwrap().to_string()
}

async fn message_id(b: &Browser, folder: &str, subject: &str) -> String {
    let list = b.get(&format!("/api/mail/folders/{folder}/messages")).await["messages"].clone();
    find(&list, "subject", subject)["id"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delegation_provisioning_and_administration() {
    let Some((host, port)) = imap_target() else {
        eprintln!("FERRITE_TEST_IMAP is not set; skipping (see the top of tests/mail_accounts.rs)");
        return;
    };
    let _ = tracing_subscriber::fmt().with_env_filter("warn,ferrite=info").with_test_writer().try_init();
    for u in ["alice", "bob", "carol", "dave"] {
        reset(&host, port, &format!("{u}@example.test")).await;
    }
    append(
        &host,
        port,
        "alice@example.test",
        "INBOX",
        &msg(
            "Ada <ada@example.org>",
            "alice@example.test",
            "Shared plans",
            "plans@example.org",
            "The quokka plan.",
        ),
    )
    .await;
    append(
        &host,
        port,
        "dave@example.test",
        "INBOX",
        &msg(
            "Eve <eve@example.org>",
            "dave@example.test",
            "Private note",
            "private@example.org",
            "Only for Dave's account.",
        ),
    )
    .await;

    let (core, state, url, _dir) = start_server().await;
    let check: Check = Arc::new(|mech: &str, user: &str, secret: &str| {
        mech == "PLAIN" && user.ends_with("@example.test") && secret == format!("{}-mail-pw", user.split('@').next().unwrap())
    });
    let (smtp_port, mut smtp) = sink_with_auth(check).await;

    // Administrator and three more users.
    let alice = Browser::new(&url);
    let alice_id = alice
        .post(
            "/api/setup",
            json!({"username": "alice", "password": "alice-password", "display_name": "Alice", "email": "alice@example.test"}),
        )
        .await["user"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut ids = HashMap::new();
    for (name, pw) in [
        ("bob", "bob-password"),
        ("carol", "carol-mail-pw"),
        ("mallory", "mallory-password"),
        ("dave", "dave-password"),
    ] {
        let u = alice
            .post(
                "/api/admin/users",
                json!({"username": name, "password": pw, "email": format!("{name}@example.test")}),
            )
            .await;
        ids.insert(name, u["user"]["id"].as_str().unwrap().to_string());
    }
    let bob = Browser::new(&url);
    bob.login("bob", "bob-password").await;
    let mallory = Browser::new(&url);
    mallory.login("mallory", "mallory-password").await;

    // Local servers are refused until the administrator allows them; only admins can.
    let account_body = |email: &str, pw: &str| {
        json!({"email": email, "name": "Alice", "password": pw, "imap_host": host, "imap_port": port, "imap_security": "none",
               "smtp_host": "127.0.0.1", "smtp_port": smtp_port, "smtp_security": "none"})
    };
    let (s, v) = alice
        .call(Method::POST, "/api/mail/accounts", Some(account_body("alice@example.test", "alice-mail-pw")))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"].as_str().unwrap().contains("local"), "{v}");
    assert_eq!(
        bob.status(Method::PATCH, "/api/admin/mail/settings", Some(json!({"allow_local_servers": true})))
            .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(bob.status(Method::GET, "/api/admin/mail/settings", None).await, StatusCode::FORBIDDEN);
    let v = alice
        .send(
            Method::PATCH,
            "/api/admin/mail/settings",
            json!({"allow_local_servers": true, "spam_report_address": "spam-reports@example.test"}),
        )
        .await;
    assert_eq!(v["settings"]["allow_local_servers"], true);
    assert_eq!(v["settings"]["verify_tls"], true, "TLS verification stays on by default");

    // Connectivity test: reachability without a password, the logins with one.
    let t = json!({"imap_host": host, "imap_port": port, "imap_security": "none", "smtp_host": "127.0.0.1", "smtp_port": smtp_port, "smtp_security": "none",
                   "imap_username": "alice@example.test"});
    let r = alice.post("/api/mail/autoconfig/test", t.clone()).await["result"].clone();
    assert_eq!(
        (r["imap"]["ok"].as_bool(), r["imap"]["login"].as_bool(), r["smtp"]["ok"].as_bool()),
        (Some(true), Some(false), Some(true)),
        "{r}"
    );
    let mut wrong = t.clone();
    wrong["password"] = json!("wrong");
    let r = alice.post("/api/mail/autoconfig/test", wrong).await["result"].clone();
    assert_eq!(r["imap"]["ok"], false, "{r}");
    assert!(r["imap"]["error"].as_str().unwrap().contains("login failed"), "{r}");
    let mut closed = t.clone();
    closed["imap_port"] = json!(1);
    assert_eq!(alice.post("/api/mail/autoconfig/test", closed).await["result"]["imap"]["ok"], false);

    // Alice's accounts: her own mailbox (shared below) and Dave's mailbox (not shared).
    let acc = alice.post("/api/mail/accounts", account_body("alice@example.test", "alice-mail-pw")).await["account"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut second = account_body("dave@example.test", "dave-mail-pw");
    second["name"] = json!("Alice (Dave's box)");
    let private = alice.post("/api/mail/accounts", second).await["account"]["id"].as_str().unwrap().to_string();
    let inbox = folder_id(&alice, &acc, "inbox").await;
    let private_inbox = folder_id(&alice, &private, "inbox").await;
    let plans = message_id(&alice, &inbox, "Shared plans").await;
    let private_msg = message_id(&alice, &private_inbox, "Private note").await;

    // Quota (RFC 9208) from the folder sync.
    let accounts = alice.get("/api/mail/accounts").await["accounts"].clone();
    let a = find(&accounts, "id", &acc);
    assert_eq!(a["quota_limit"], 10 * 1024 * 1024, "{a}");
    assert!(a["quota_used"].as_i64().unwrap() > 0);
    assert_eq!(a["role"], "owner");
    assert_eq!(a["permissions"]["authenticate"], true);

    // Account order.
    alice.send(Method::PUT, "/api/mail/accounts/order", json!({"ids": [private, acc]})).await;
    let accounts = alice.get("/api/mail/accounts").await["accounts"].clone();
    assert_eq!(accounts[0]["id"], private.as_str());
    assert_eq!(accounts[1]["id"], acc.as_str());
    assert_eq!(
        bob.status(Method::PUT, "/api/mail/accounts/order", Some(json!({"ids": [acc]}))).await,
        StatusCode::BAD_REQUEST,
        "an account that isn't visible can't be ordered"
    );

    // ---- delegation ---------------------------------------------------------------------
    // Strangers see nothing.
    for path in [
        format!("/api/mail/accounts/{acc}/folders"),
        format!("/api/mail/folders/{inbox}/messages"),
        format!("/api/mail/messages/{plans}"),
        format!("/api/mail/accounts/{acc}/delegations"),
    ] {
        assert_eq!(bob.status(Method::GET, &path, None).await, StatusCode::NOT_FOUND, "{path}");
    }
    assert_eq!(
        bob.status(Method::POST, "/api/mail/actions", Some(json!({"ids": [plans], "action": "read"})))
            .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        bob.status(Method::POST, &format!("/api/mail/accounts/{acc}/delegations"), Some(json!({"user": "bob"})))
            .await,
        StatusCode::NOT_FOUND,
        "nobody shares someone else's account with themselves"
    );
    // Sharing: not with the owner, not with nobody.
    assert_eq!(
        alice
            .status(Method::POST, &format!("/api/mail/accounts/{acc}/delegations"), Some(json!({"user": "alice"})))
            .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        alice
            .status(Method::POST, &format!("/api/mail/accounts/{acc}/delegations"), Some(json!({"user": "nobody"})))
            .await,
        StatusCode::BAD_REQUEST
    );
    let d = alice.post(&format!("/api/mail/accounts/{acc}/delegations"), json!({"user": "bob"})).await["delegation"].clone();
    assert_eq!(d["username"], "bob");
    let delegation = d["id"].as_str().unwrap().to_string();
    assert_eq!(
        alice
            .status(Method::POST, &format!("/api/mail/accounts/{acc}/delegations"), Some(json!({"user": "bob"})))
            .await,
        StatusCode::CONFLICT
    );
    let notes = bob.get("/api/notifications").await["notifications"].clone();
    assert!(notes.to_string().contains("shared a mail account with you"), "{notes}");

    // Bob sees the account, marked as delegated, and only that one.
    let accounts = bob.get("/api/mail/accounts").await["accounts"].clone();
    assert_eq!(accounts.as_array().unwrap().len(), 1, "{accounts}");
    let shared = &accounts[0];
    assert_eq!(
        (shared["id"].as_str(), shared["delegated"].as_bool(), shared["owner_name"].as_str()),
        (Some(acc.as_str()), Some(true), Some("Alice"))
    );
    assert_eq!(
        shared["permissions"],
        json!({"configure": true, "authenticate": false, "remove": false, "share": false})
    );
    assert!(shared.get("password").is_none() && !shared.to_string().contains("alice-mail-pw"));

    // Live events reach the delegate.
    let (conn, mut events) = core.hub.connect(&ids["bob"]);
    // Bob reads, flags and moves the shared mail.
    let view = bob.get(&format!("/api/mail/messages/{plans}")).await["message"].clone();
    assert!(view["text"].as_str().unwrap().contains("quokka"));
    bob.post("/api/mail/actions", json!({"ids": [plans], "action": "flag"})).await;
    let mut seen_event = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !seen_event {
        let ev = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("a live event for the delegate")
            .unwrap();
        let v: Value = serde_json::from_str(&ev).unwrap();
        seen_event = v["type"] == "mail.changed" && v["account_id"] == acc.as_str();
    }
    core.hub.disconnect(&ids["bob"], conn);
    let mut check = ImapConn::connect(&settings(&host, port, "alice@example.test")).await.unwrap();
    check.select("INBOX").await.unwrap();
    assert_eq!(check.uid_search("FLAGGED").await.unwrap().len(), 1, "the flag is on the server");
    // Search finds the shared mail for the delegate (webmail search and global search).
    let found = bob.get("/api/mail/search?q=quokka").await["messages"].clone();
    assert!(found.to_string().contains(&plans), "{found}");
    let hits = bob.get("/api/search?q=quokka&kinds=mail").await["results"].clone();
    assert!(hits.to_string().contains(&plans), "{hits}");
    assert!(mallory.get("/api/search?q=quokka&kinds=mail").await["results"].as_array().unwrap().is_empty());
    assert!(mallory.get("/api/mail/search?q=quokka").await["messages"].as_array().unwrap().is_empty());

    // Bob sends as the account; the copy goes to the account's Sent folder.
    bob.post(
        &format!("/api/mail/accounts/{acc}/send"),
        json!({"to": ["ada@example.org"], "subject": "Re: Shared plans", "text": "Sent by Bob for Alice.", "in_reply_to": plans}),
    )
    .await;
    let got = next_mail(&mut smtp).await;
    assert_eq!(got.from, "alice@example.test");
    assert_eq!(got.auth, Some(("PLAIN".to_string(), "alice@example.test".to_string())));
    let sent = folder_id(&bob, &acc, "sent").await;
    message_id(&bob, &sent, "Re: Shared plans").await;
    // Configure is allowed (signature), how it signs in is not; nor removing or re-sharing.
    bob.send(Method::PATCH, &format!("/api/mail/accounts/{acc}"), json!({"signature": "Bob, for Alice"}))
        .await;
    for body in [
        json!({"imap_host": "evil.example"}),
        json!({"password": "stolen"}),
        json!({"email": "bob@example.test"}),
        json!({"smtp_host": "127.0.0.2", "signature": "x"}),
    ] {
        assert_eq!(
            bob.status(Method::PATCH, &format!("/api/mail/accounts/{acc}"), Some(body.clone())).await,
            StatusCode::FORBIDDEN,
            "{body}"
        );
    }
    // The edit form sends unchanged server fields back: that is fine.
    let a = find(&bob.get("/api/mail/accounts").await["accounts"], "id", &acc).clone();
    bob.send(
        Method::PATCH,
        &format!("/api/mail/accounts/{acc}"),
        json!({"imap_host": a["imap_host"], "imap_port": a["imap_port"], "smtp_host": a["smtp_host"], "name": "Alice A."}),
    )
    .await;
    assert_eq!(
        bob.status(Method::DELETE, &format!("/api/mail/accounts/{acc}"), None).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        bob.status(Method::GET, &format!("/api/mail/accounts/{acc}/delegations"), None).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        bob.status(Method::POST, &format!("/api/mail/accounts/{acc}/delegations"), Some(json!({"user": "mallory"})))
            .await,
        StatusCode::FORBIDDEN
    );
    assert_ne!(
        bob.status(
            Method::POST,
            "/api/mail/oauth/start",
            Some(json!({"provider": "google", "email": "alice@example.test", "account_id": acc}))
        )
        .await,
        StatusCode::OK
    );
    // Ids of Alice's other account never ride along on the delegated one.
    assert_eq!(
        bob.status(
            Method::POST,
            "/api/mail/actions",
            Some(json!({"ids": [plans], "action": "move", "folder_id": private_inbox}))
        )
        .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        bob.status(Method::POST, "/api/mail/actions", Some(json!({"ids": [plans, private_msg], "action": "read"})))
            .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        bob.status(
            Method::POST,
            &format!("/api/mail/accounts/{acc}/send"),
            Some(json!({"to": ["x@example.org"], "text": "x", "in_reply_to": private_msg}))
        )
        .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        bob.status(
            Method::POST,
            &format!("/api/mail/accounts/{acc}/drafts"),
            Some(json!({"to": [], "text": "x", "attachments": [{"kind": "message", "message_id": private_msg, "index": 0}]}))
        )
        .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        bob.status(Method::GET, &format!("/api/mail/search?q=Private&account_id={private}"), None).await,
        StatusCode::NOT_FOUND
    );
    assert!(!bob.get("/api/mail/search?q=Private").await.to_string().contains(&private_msg));
    assert_eq!(
        bob.status(Method::POST, &format!("/api/mail/accounts/{private}/sync"), Some(json!({}))).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        bob.status(
            Method::POST,
            &format!("/api/mail/accounts/{private}/send"),
            Some(json!({"to": ["x@example.org"], "text": "x"}))
        )
        .await,
        StatusCode::NOT_FOUND
    );

    // MCP sees the same rights.
    let bob_ctx = ferrite_server::tools::UserCtx {
        user: core.user(&ids["bob"]).await.unwrap(),
        scopes: "mail".into(),
    };
    let listed = state.tools.call(&bob_ctx, "mail_accounts", json!({})).await.unwrap().text_content();
    assert!(listed.contains("\"delegated\": true") && listed.contains("\"shared_by\": \"Alice\""), "{listed}");
    let l = state
        .tools
        .call(&bob_ctx, "mail_list", json!({"account": "alice@example.test"}))
        .await
        .unwrap()
        .text_content();
    assert!(l.contains("Shared plans"), "{l}");
    let refused = state
        .tools
        .call(&bob_ctx, "mail_delegate", json!({"account": "alice@example.test", "user": "mallory"}))
        .await
        .unwrap()
        .text_content();
    assert!(refused.contains("forbidden"), "{refused}");
    assert!(
        state
            .tools
            .call(&bob_ctx, "mail_provision", json!({}))
            .await
            .unwrap()
            .text_content()
            .contains("forbidden")
    );
    let alice_ctx = ferrite_server::tools::UserCtx {
        user: core.user(&alice_id).await.unwrap(),
        scopes: "mail".into(),
    };
    let ds = state
        .tools
        .call(&alice_ctx, "mail_delegations", json!({"account": "alice@example.test"}))
        .await
        .unwrap()
        .text_content();
    assert!(ds.contains("\"username\": \"bob\""), "{ds}");
    let carol_share = state
        .tools
        .call(&alice_ctx, "mail_delegate", json!({"account": "alice@example.test", "user": "mallory"}))
        .await
        .unwrap()
        .text_content();
    assert!(carol_share.contains("delegation_id"), "{carol_share}");
    // An administrator can see and revoke every delegation.
    let all = alice.get("/api/admin/mail/delegations").await["delegations"].clone();
    assert_eq!(all.as_array().unwrap().len(), 2, "{all}");
    let mallory_delegation = find(&all, "username", "mallory")["id"].as_str().unwrap().to_string();
    assert_eq!(
        bob.status(Method::DELETE, &format!("/api/admin/mail/delegations/{mallory_delegation}"), None)
            .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        bob.status(Method::DELETE, &format!("/api/mail/delegations/{mallory_delegation}"), None).await,
        StatusCode::NOT_FOUND
    );
    alice
        .send(Method::DELETE, &format!("/api/admin/mail/delegations/{mallory_delegation}"), json!(null))
        .await;

    // Revoking takes effect at once.
    alice.send(Method::DELETE, &format!("/api/mail/delegations/{delegation}"), json!(null)).await;
    assert_eq!(
        bob.status(Method::GET, &format!("/api/mail/messages/{plans}"), None).await,
        StatusCode::NOT_FOUND
    );
    assert!(bob.get("/api/mail/accounts").await["accounts"].as_array().unwrap().is_empty());
    assert!(bob.get("/api/search?q=quokka&kinds=mail").await["results"].as_array().unwrap().is_empty());

    // ---- report spam / not spam -----------------------------------------------------------
    let r = alice.post("/api/mail/actions", json!({"ids": [plans], "action": "spam"})).await;
    assert_eq!((r["reported"].as_i64(), r["moved"].as_i64()), (Some(1), Some(1)), "{r}");
    let report = next_mail(&mut smtp).await;
    assert_eq!(report.to, vec!["spam-reports@example.test"]);
    assert!(report.data.contains("message/rfc822") && report.data.contains("The quokka plan.") && report.data.contains("Subject: Spam report: Shared plans"));
    check.select("Junk").await.unwrap();
    assert_eq!(check.uid_search("ALL").await.unwrap().len(), 1, "moved to Junk");
    assert_eq!(check.uid_search("KEYWORD $Junk").await.unwrap().len(), 1, "tagged $Junk");
    let junk = folder_id(&alice, &acc, "junk").await;
    alice.post(&format!("/api/mail/accounts/{acc}/sync"), json!({"folder_id": junk})).await;
    let in_junk = message_id(&alice, &junk, "Shared plans").await;
    // No address for "not spam": only moved back.
    let r = alice.post("/api/mail/actions", json!({"ids": [in_junk], "action": "not_spam"})).await;
    assert_eq!((r["reported"].as_i64(), r["moved"].as_i64()), (Some(0), Some(1)), "{r}");
    check.select("INBOX").await.unwrap();
    assert_eq!(check.uid_search("KEYWORD $NotJunk").await.unwrap().len(), 1);

    // ---- anti-abuse alerts ----------------------------------------------------------------
    alice
        .send(
            Method::PATCH,
            "/api/admin/mail/settings",
            json!({"abuse": {"enabled": true, "max_recipients": 2, "per_15m": 1}}),
        )
        .await;
    for n in 0..2 {
        alice
            .post(
                &format!("/api/mail/accounts/{acc}/send"),
                json!({"to": ["a@example.org", "b@example.org", "c@example.org"], "subject": format!("Blast {n}"), "text": "x"}),
            )
            .await;
        next_mail(&mut smtp).await;
    }
    let alerts = alice.get("/api/admin/mail/abuse").await["alerts"].clone();
    let rules: Vec<&str> = alerts.as_array().unwrap().iter().map(|a| a["rule"].as_str().unwrap()).collect();
    assert!(rules.contains(&"recipients") && rules.contains(&"15m"), "{alerts}");
    assert_eq!(rules.iter().filter(|r| **r == "recipients").count(), 1, "one alert per window, not per message");
    let notes = alice.get("/api/notifications").await["notifications"].clone();
    assert!(notes.to_string().contains("Suspicious sending by @alice"), "{notes}");
    assert_eq!(bob.status(Method::GET, "/api/admin/mail/abuse", None).await, StatusCode::FORBIDDEN);

    // ---- users may not add accounts --------------------------------------------------------
    alice
        .send(Method::PATCH, "/api/admin/mail/settings", json!({"allow_user_accounts": false}))
        .await;
    let (s, v) = bob
        .call(Method::POST, "/api/mail/accounts", Some(account_body("bob@example.test", "bob-mail-pw")))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"].as_str().unwrap().contains("turned off"), "{v}");
    assert_eq!(bob.get("/api/mail/config").await["allow_user_accounts"], false);
    alice
        .send(Method::PATCH, "/api/admin/mail/settings", json!({"allow_user_accounts": true}))
        .await;

    // ---- provisioning ----------------------------------------------------------------------
    let template = |extra: Value| {
        let mut t = json!({"name": "Example staff", "match_kind": "domain", "match_value": "example.test", "email_pattern": "%EMAIL%",
                           "imap_host": host, "imap_port": port, "imap_security": "none", "imap_username_pattern": "%EMAIL%",
                           "smtp_host": "127.0.0.1", "smtp_port": smtp_port, "smtp_security": "none", "smtp_username_pattern": "%EMAIL%",
                           "password_mode": "login"});
        for (k, v) in extra.as_object().unwrap() {
            t[k] = v.clone();
        }
        t
    };
    assert_eq!(
        bob.status(Method::POST, "/api/admin/mail/provisionings", Some(template(json!({})))).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        alice
            .status(
                Method::POST,
                "/api/admin/mail/provisionings",
                Some(template(json!({"password_mode": "master"})))
            )
            .await,
        StatusCode::BAD_REQUEST,
        "master mode needs the master password"
    );
    // Login-password mode, limited to Carol by a group.
    let staff = alice.post("/api/admin/groups", json!({"name": "mail-staff"})).await;
    let staff_id = staff["group"]["id"].as_str().or(staff["id"].as_str()).unwrap().to_string();
    alice.post(&format!("/api/admin/groups/{staff_id}/members"), json!({"username": "carol"})).await;
    let created = alice
        .post(
            "/api/admin/mail/provisionings",
            template(json!({"match_kind": "group", "match_value": staff_id})),
        )
        .await;
    let login_tpl = created["provisioning"]["id"].as_str().unwrap().to_string();
    assert_eq!(created["report"]["pending"], 1, "Carol has not signed in with her password yet: {created}");
    let list = alice.get("/api/admin/mail/provisionings").await["provisionings"].clone();
    assert_eq!(find(&list, "id", &login_tpl)["pending"], 1);
    // Signing in captures the password (sealed) and provisions the account.
    let carol = Browser::new(&url);
    carol.login("carol", "carol-mail-pw").await;
    let sealed: String = sqlx::query_scalar("SELECT password FROM mail_login_secrets WHERE user_id = ?")
        .bind(&ids["carol"])
        .fetch_one(&core.db)
        .await
        .unwrap();
    assert!(!sealed.contains("carol-mail-pw"));
    let none: Option<String> = sqlx::query_scalar("SELECT password FROM mail_login_secrets WHERE user_id = ?")
        .bind(&ids["mallory"])
        .fetch_optional(&core.db)
        .await
        .unwrap();
    assert!(none.is_none(), "passwords of users no login-mode template matches are never kept");
    let accounts = carol.get("/api/mail/accounts").await["accounts"].clone();
    assert_eq!(accounts.as_array().unwrap().len(), 1, "{accounts}");
    let carol_acc = accounts[0]["id"].as_str().unwrap().to_string();
    assert_eq!(accounts[0]["provisioning_id"], login_tpl.as_str());
    assert_eq!(accounts[0]["email"], "carol@example.test");
    assert_eq!(
        accounts[0]["permissions"],
        json!({"configure": true, "authenticate": false, "remove": false, "share": true})
    );
    carol.get(&format!("/api/mail/accounts/{carol_acc}/folders")).await;
    // Users change only the sender name, signature and Sent copy of provisioned accounts.
    carol
        .send(Method::PATCH, &format!("/api/mail/accounts/{carol_acc}"), json!({"signature": "Carol"}))
        .await;
    assert_eq!(
        carol
            .status(
                Method::PATCH,
                &format!("/api/mail/accounts/{carol_acc}"),
                Some(json!({"imap_host": "elsewhere.example"}))
            )
            .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        carol.status(Method::DELETE, &format!("/api/mail/accounts/{carol_acc}"), None).await,
        StatusCode::FORBIDDEN
    );
    // Master-password mode: Dave's mailbox password stands in for a Dovecot master user.
    let master = alice
        .post(
            "/api/admin/mail/provisionings",
            template(
                json!({"name": "Dave only", "match_kind": "domain", "match_value": "example.test", "email_pattern": "%USERID%@example.test",
                            "imap_username_pattern": "dave@example.test", "smtp_username_pattern": "dave@example.test",
                            "password_mode": "master", "master_password": "dave-mail-pw"}),
            ),
        )
        .await;
    let master_tpl = master["provisioning"]["id"].as_str().unwrap().to_string();
    let stored: String = sqlx::query_scalar("SELECT master_password FROM mail_provisionings WHERE id = ?")
        .bind(&master_tpl)
        .fetch_one(&core.db)
        .await
        .unwrap();
    assert!(!stored.contains("dave-mail-pw"));
    assert!(!master.to_string().contains("dave-mail-pw"));
    // Every example.test user got one without signing in (alice, bob, carol, mallory, dave).
    assert_eq!(master["report"]["created"], 5, "{master}");
    let davebox: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mail_accounts WHERE provisioning_id = ?")
        .bind(&master_tpl)
        .fetch_one(&core.db)
        .await
        .unwrap();
    assert_eq!(davebox, 5);
    let bob_accounts = bob.get("/api/mail/accounts").await["accounts"].clone();
    let bob_prov = find(&bob_accounts, "provisioning_id", &master_tpl)["id"].as_str().unwrap().to_string();
    assert!(!bob_prov.is_empty());
    let f = bob.get(&format!("/api/mail/accounts/{bob_prov}/folders")).await["folders"].clone();
    assert!(!f.as_array().unwrap().is_empty(), "the master password logs in");
    // Narrowing the template removes accounts that no longer match.
    let r = alice
        .send(
            Method::PATCH,
            &format!("/api/admin/mail/provisionings/{master_tpl}"),
            json!({"match_kind": "group", "match_value": staff_id}),
        )
        .await;
    assert_eq!(r["report"]["removed"], 4, "{r}");
    assert!(bob.get("/api/mail/accounts").await["accounts"].as_array().unwrap().is_empty());
    // Leaving the group deprovisions too (on the next reconcile).
    alice
        .send(Method::DELETE, &format!("/api/admin/groups/{staff_id}/members/{}", ids["carol"]), json!(null))
        .await;
    let r = alice.post("/api/admin/mail/provision", json!({})).await;
    assert_eq!(r["report"]["removed"], 2, "{r}");
    assert!(carol.get("/api/mail/accounts").await["accounts"].as_array().unwrap().is_empty());
    let gone: Option<String> = sqlx::query_scalar("SELECT password FROM mail_login_secrets WHERE user_id = ?")
        .bind(&ids["carol"])
        .fetch_optional(&core.db)
        .await
        .unwrap();
    assert!(gone.is_none(), "the captured password goes when no template needs it");
    // Deprovisioning a template removes its accounts and turns it off.
    alice.post(&format!("/api/admin/groups/{staff_id}/members"), json!({"username": "carol"})).await;
    let r = alice.post("/api/admin/mail/provision", json!({})).await;
    assert_eq!(r["report"]["created"], 1, "master mode needs no sign-in: {r}");
    let n = alice.post(&format!("/api/admin/mail/provisionings/{master_tpl}/deprovision"), json!({})).await;
    assert_eq!(n["removed"], 1);
    let list = alice.get("/api/admin/mail/provisionings").await["provisionings"].clone();
    assert_eq!(find(&list, "id", &master_tpl)["enabled"], false);
    assert_eq!(alice.post("/api/admin/mail/provision", json!({})).await["report"]["created"], 0);
    alice
        .send(Method::DELETE, &format!("/api/admin/mail/provisionings/{login_tpl}"), json!(null))
        .await;

    // ---- background sync while nobody is online ------------------------------------------
    assert!(!core.hub.is_online(&alice_id));
    append(
        &host,
        port,
        "alice@example.test",
        "INBOX",
        &msg(
            "Grace <grace@example.org>",
            "alice@example.test",
            "Offline delivery",
            "offline@example.org",
            "Synced in the background.",
        ),
    )
    .await;
    let r = core.mail_background_round().await;
    assert!(r.accounts >= 2 && r.new_messages >= 1, "{r:?}");
    message_id(&alice, &inbox, "Offline delivery").await;
    let notes = alice.get("/api/notifications").await["notifications"].clone();
    assert!(notes.to_string().contains("New mail from Grace"), "{notes}");
    assert_eq!(
        alice.post("/api/admin/mail/sync", json!({})).await["report"]["new_messages"],
        0,
        "nothing new: CONDSTORE skips"
    );
    check.logout().await;
}

// ---- OAuth ------------------------------------------------------------------------------------

#[derive(Default)]
struct Provider {
    /// code -> (PKCE challenge, redirect_uri, client id)
    codes: HashMap<String, (String, String)>,
    /// Valid access tokens; refresh tokens.
    access: Vec<String>,
    refresh: Vec<String>,
    refreshes: usize,
    issued: usize,
}

type Shared = Arc<Mutex<Provider>>;

fn pkce(verifier: &str) -> String {
    ferrite_core::mail::oauth::pkce_challenge(verifier)
}

async fn authorize(State(p): State<Shared>, Query(q): Query<HashMap<String, String>>) -> axum::response::Response {
    assert_eq!(q.get("client_id").map(String::as_str), Some("test-client"));
    assert_eq!(q.get("response_type").map(String::as_str), Some("code"));
    assert_eq!(q.get("code_challenge_method").map(String::as_str), Some("S256"));
    assert_eq!(q.get("scope").map(String::as_str), Some("https://mail.google.com/"));
    let code = format!("code-{}", ferrite_core::util::random_token(8));
    let redirect = q["redirect_uri"].clone();
    p.lock().unwrap().codes.insert(code.clone(), (q["code_challenge"].clone(), redirect.clone()));
    Redirect::to(&format!("{redirect}?code={code}&state={}", q["state"])).into_response()
}

async fn token(State(p): State<Shared>, Form(f): Form<HashMap<String, String>>) -> axum::response::Response {
    let mut p = p.lock().unwrap();
    if f.get("client_id").map(String::as_str) != Some("test-client") || f.get("client_secret").map(String::as_str) != Some("test-secret") {
        return (axum::http::StatusCode::UNAUTHORIZED, axum::Json(json!({"error": "invalid_client"}))).into_response();
    }
    let ok = match f.get("grant_type").map(String::as_str) {
        Some("authorization_code") => match p.codes.remove(f.get("code").map(String::as_str).unwrap_or("")) {
            Some((challenge, redirect)) => f.get("redirect_uri") == Some(&redirect) && f.get("code_verifier").is_some_and(|v| pkce(v) == challenge),
            None => false,
        },
        Some("refresh_token") => {
            let ok = p.refresh.contains(&f.get("refresh_token").cloned().unwrap_or_default());
            if ok {
                p.refreshes += 1;
                // The old access tokens stop working.
                p.access.clear();
            }
            ok
        }
        _ => false,
    };
    if !ok {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "invalid_grant", "error_description": "bad code, verifier or refresh token"})),
        )
            .into_response();
    }
    p.issued += 1;
    let access = format!("access-{}-{}", p.issued, ferrite_core::util::random_token(6));
    let refresh = format!("refresh-{}", ferrite_core::util::random_token(6));
    p.access.push(access.clone());
    p.refresh.push(refresh.clone());
    axum::Json(json!({"access_token": access, "refresh_token": refresh, "expires_in": 3600, "token_type": "Bearer"})).into_response()
}

/// RFC 7662 token introspection, as Dovecot's oauth2 passdb calls it.
async fn introspect(State(p): State<Shared>, Form(f): Form<HashMap<String, String>>) -> axum::Json<Value> {
    let active = p.lock().unwrap().access.contains(&f.get("token").cloned().unwrap_or_default());
    axum::Json(if active {
        json!({"active": true, "username": "carol@example.test"})
    } else {
        json!({"active": false})
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oauth_accounts_with_xoauth2() {
    let (Ok(imap), Ok(provider_addr)) = (std::env::var("FERRITE_TEST_OAUTH_IMAP"), std::env::var("FERRITE_TEST_OAUTH_PROVIDER")) else {
        eprintln!("FERRITE_TEST_OAUTH_IMAP / FERRITE_TEST_OAUTH_PROVIDER are not set; skipping the OAuth test (see the top of tests/mail_accounts.rs)");
        return;
    };
    let _ = tracing_subscriber::fmt().with_env_filter("warn,ferrite=info").with_test_writer().try_init();
    let (host, port) = imap.rsplit_once(':').map(|(h, p)| (h.to_string(), p.parse::<u16>().unwrap())).unwrap();
    reset(&host, port, "carol@example.test").await;
    append(
        &host,
        port,
        "carol@example.test",
        "INBOX",
        &msg(
            "Ada <ada@example.org>",
            "carol@example.test",
            "Token mail",
            "tok@example.org",
            "Read over XOAUTH2.",
        ),
    )
    .await;

    // The mock provider (authorization, token and introspection endpoints).
    let shared: Shared = Arc::default();
    let app = axum::Router::new()
        .route("/authorize", axum::routing::get(authorize))
        .route("/token", axum::routing::post(token))
        .route("/introspect", axum::routing::post(introspect))
        .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind(&provider_addr).await.expect("bind the mock provider");
    tokio::spawn(axum::serve(listener, app).into_future());
    let mock = format!("http://{provider_addr}");
    // SMTP accepts only live access tokens.
    let tokens = shared.clone();
    let check: Check = Arc::new(move |mech: &str, user: &str, secret: &str| {
        mech == "XOAUTH2" && user == "carol@example.test" && tokens.lock().unwrap().access.iter().any(|t| t == secret)
    });
    let (smtp_port, mut smtp) = sink_with_auth(check).await;

    let (core, _state, url, _dir) = start_server().await;
    let admin = Browser::new(&url);
    admin.post("/api/setup", json!({"username": "admin", "password": "admin-password"})).await;
    admin.post("/api/admin/users", json!({"username": "carol", "password": "carol-password"})).await;
    admin.post("/api/admin/users", json!({"username": "bob", "password": "bob-password"})).await;
    let carol = Browser::new(&url);
    carol.login("carol", "carol-password").await;
    let bob = Browser::new(&url);
    bob.login("bob", "bob-password").await;

    // Not configured yet.
    let (s, v) = carol
        .call(
            Method::POST,
            "/api/mail/oauth/start",
            Some(json!({"provider": "google", "email": "carol@example.test"})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let v = admin
        .send(
            Method::PATCH,
            "/api/admin/mail/settings",
            json!({"allow_local_servers": true, "google": {"client_id": "test-client", "client_secret": "test-secret",
                   "auth_url": format!("{mock}/authorize"), "token_url": format!("{mock}/token"),
                   "imap_host": host, "imap_port": port, "imap_security": "none", "smtp_host": "127.0.0.1", "smtp_port": smtp_port, "smtp_security": "none"}}),
        )
        .await;
    assert_eq!(v["settings"]["google_secret_set"], true);
    assert!(!v.to_string().contains("test-secret"), "the client secret is never returned");
    assert_eq!(v["redirect_uri"], format!("{url}/api/mail/oauth/callback"));
    let cfg = carol.get("/api/mail/config").await;
    assert_eq!(find(&cfg["oauth"], "id", "google")["configured"], true);

    // App tokens can't start the browser flow.
    let (_, tok) = core.create_token(&carol_id(&core).await, "app", "script", "mail", None).await.unwrap();
    let r = reqwest::Client::new()
        .post(format!("{url}/api/mail/oauth/start"))
        .bearer_auth(&tok)
        .json(&json!({"provider": "google", "email": "carol@example.test"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);

    async fn consent(b: &Browser, url: &str) -> String {
        let start = b
            .post(
                "/api/mail/oauth/start",
                json!({"provider": "google", "email": "carol@example.test", "name": "Carol"}),
            )
            .await;
        let consent_url = start["url"].as_str().unwrap();
        assert!(consent_url.contains("code_challenge=") && consent_url.contains("login_hint=carol%40example.test"));
        let r = b.http.get(consent_url).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        let callback = r.headers()["location"].to_str().unwrap().to_string();
        assert!(callback.starts_with(&format!("{url}/api/mail/oauth/callback?")), "{callback}");
        callback
    }
    let land = |r: &reqwest::Response| r.headers()["location"].to_str().unwrap().to_string();

    // The callback must come back to the session that started it.
    let callback = consent(&carol, &url).await;
    let r = bob.http.get(&callback).send().await.unwrap();
    assert!(land(&r).starts_with("/mail?oauth_error="), "{}", land(&r));
    // ...and the state was consumed by that attempt.
    let r = carol.http.get(&callback).send().await.unwrap();
    assert!(land(&r).contains("oauth_error=") && land(&r).contains("expired"), "{}", land(&r));
    // A forged code is refused by the provider.
    let callback = consent(&carol, &url).await;
    let forged = format!(
        "{}{}",
        callback.split("code=").next().unwrap(),
        callback.split('&').nth(1).map(|s| format!("code=forged&{s}")).unwrap()
    );
    let r = carol.http.get(&forged).send().await.unwrap();
    assert!(land(&r).contains("oauth_error="), "{}", land(&r));

    // The real thing: the account is created and signs in with XOAUTH2.
    let callback = consent(&carol, &url).await;
    let r = carol.http.get(&callback).send().await.unwrap();
    let to = land(&r);
    assert!(to.starts_with("/mail?account=") && to.ends_with("&oauth=connected"), "{to}");
    let accounts = carol.get("/api/mail/accounts").await["accounts"].clone();
    let a = &accounts[0];
    assert_eq!(
        (a["auth_method"].as_str(), a["oauth_provider"].as_str(), a["save_sent"].as_bool()),
        (Some("xoauth2"), Some("google"), Some(false))
    );
    let acc = a["id"].as_str().unwrap().to_string();
    let (sealed, refresh): (String, String) = sqlx::query_as("SELECT password, oauth_refresh FROM mail_accounts WHERE id = ?")
        .bind(&acc)
        .fetch_one(&core.db)
        .await
        .unwrap();
    {
        let p = shared.lock().unwrap();
        assert!(
            !p.access.iter().any(|t| sealed.contains(t.as_str())) && !p.refresh.iter().any(|t| refresh.contains(t.as_str())),
            "tokens are sealed"
        );
    }
    let inbox = folder_id(&carol, &acc, "inbox").await;
    let view = carol
        .get(&format!("/api/mail/messages/{}", message_id(&carol, &inbox, "Token mail").await))
        .await;
    assert!(view["message"]["text"].as_str().unwrap().contains("XOAUTH2"));
    let log = std::fs::read_to_string(format!(
        "{}/dovecot.log",
        std::env::var("FERRITE_TEST_OAUTH_DIR").unwrap_or_else(|_| "/tmp/ferrite-mail-accounts-oauth".into())
    ))
    .unwrap_or_default();
    if !log.is_empty() {
        assert!(log.contains("method=XOAUTH2"), "Dovecot saw an XOAUTH2 login");
    }
    carol
        .post(
            &format!("/api/mail/accounts/{acc}/send"),
            json!({"to": ["ada@example.org"], "subject": "Hi", "text": "Over XOAUTH2."}),
        )
        .await;
    let got = next_mail(&mut smtp).await;
    assert_eq!(got.auth, Some(("XOAUTH2".to_string(), "carol@example.test".to_string())));
    // The servers come from the provider: users can't point the token elsewhere.
    assert_eq!(
        carol
            .status(Method::PATCH, &format!("/api/mail/accounts/{acc}"), Some(json!({"imap_host": "evil.example"})))
            .await,
        StatusCode::BAD_REQUEST
    );

    // Near expiry, the token is refreshed before use (the old one stops working).
    sqlx::query("UPDATE mail_accounts SET oauth_expires_at = ? WHERE id = ?")
        .bind(ferrite_core::util::now() + chrono::Duration::seconds(30))
        .bind(&acc)
        .execute(&core.db)
        .await
        .unwrap();
    carol
        .post(
            &format!("/api/mail/accounts/{acc}/send"),
            json!({"to": ["ada@example.org"], "subject": "After refresh", "text": "x"}),
        )
        .await;
    assert_eq!(next_mail(&mut smtp).await.auth.unwrap().0, "XOAUTH2");
    assert_eq!(shared.lock().unwrap().refreshes, 1);
    let expires: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT oauth_expires_at FROM mail_accounts WHERE id = ?")
        .bind(&acc)
        .fetch_one(&core.db)
        .await
        .unwrap();
    assert!(expires > ferrite_core::util::now() + chrono::Duration::minutes(50));
    // A fresh IMAP connection with the new token: Dovecot introspects it.
    let fresh = core
        .mail_access(&carol_id(&core).await, &acc, ferrite_core::mail::MailRight::Read)
        .await
        .unwrap();
    let fresh = core.mail_fresh_credentials(&fresh).await.unwrap();
    core.test_imap(&core.imap_settings(&fresh).unwrap())
        .await
        .expect("XOAUTH2 login with the refreshed token");
    assert_eq!(shared.lock().unwrap().refreshes, 1, "no second refresh while the token is fresh");

    // Reconnecting (a revoked grant) renews the tokens of the same account.
    shared.lock().unwrap().refresh.clear();
    let start = carol
        .post(
            "/api/mail/oauth/start",
            json!({"provider": "google", "email": "carol@example.test", "account_id": acc}),
        )
        .await;
    let r = carol.http.get(start["url"].as_str().unwrap()).send().await.unwrap();
    let r = carol.http.get(land(&r)).send().await.unwrap();
    assert!(land(&r).contains(&format!("account={acc}")), "{}", land(&r));
    assert_eq!(
        carol.get("/api/mail/accounts").await["accounts"].as_array().unwrap().len(),
        1,
        "no second account"
    );
    // Bob can't reconnect Carol's account.
    assert_eq!(
        bob.status(
            Method::POST,
            "/api/mail/oauth/start",
            Some(json!({"provider": "google", "email": "carol@example.test", "account_id": acc}))
        )
        .await,
        StatusCode::NOT_FOUND
    );
}

async fn carol_id(core: &Core) -> String {
    core.user_by_name("carol").await.unwrap().id
}
