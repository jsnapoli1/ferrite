//! Mail security and privacy against a REAL IMAP server, plus always-on API checks.
//!
//! The real-server test is skipped (passes trivially) unless configured, like `tests/mail.rs`:
//!
//!   MAIL_TEST_DIR=/tmp/ferrite-mail-security IMAP_PORT=3331 LMTP_PORT=3332 AUTH_PORT=3333 \
//!     SIEVE_PORT=3334 POSTFIX=0 bash e2e/mail-server.sh start /tmp/ferrite-mail-security
//!   FERRITE_TEST_IMAP=127.0.0.1:3331:alice@example.test:alice-mail-pw \
//!   FERRITE_TEST_MAIL_DIR=/tmp/ferrite-mail-security \
//!   cargo test -p ferrite-server --test mail_security -- --nocapture
//!
//! With `FERRITE_TEST_MAIL_DIR`, messages arrive through Dovecot's delivery agent
//! (`mail-server.sh deliver`, as from outside); without it they are APPENDed. SMTP goes to an
//! in-process sink. DKIM keys are generated here and served by a small DNS server in the test
//! (the server's `[mail_security] dns_server`); images come from a local HTTP server through
//! the proxy (`proxy_allow_private`, which only this test sets).
//!
//! Covered: DKIM pass/fail/none with Ed25519 and RSA keys over real DNS lookups (and the
//! stored result), phishing findings (contact mismatch, address in the name, future date,
//! Reply-To domain, misleading links, `$Phishing`), trusted senders and internal addresses
//! (API, reader report, composer check), the image proxy (signed per user, expiry, tampering,
//! no cookies or referrer upstream, CSP of the frame, non-images refused), avatars (contact
//! photo, cached misses, admin-only Gravatar switch), view source, attachments as a ZIP,
//! S/MIME (PKCS#12 import with the key sealed at rest, decrypting OpenSSL mail in the reader
//! and in attachments, verifying signatures and the chain with an imported CA, collecting
//! the signer's certificate, signing and encrypting outgoing mail that OpenSSL decrypts and
//! verifies, refusing recipients without certificates, the decrypted-search opt-in, the
//! assistant not reading decrypted mail unless allowed), PGP detection and PGP/MIME sending.

use base64::Engine;
use ferrite_core::mail::ServerSettings;
use ferrite_core::mail::imap::ImapConn;
use ferrite_core::mailer::SmtpSecurity;
use ferrite_core::{Config, Core};
use ferrite_server::AppState;
use mail_auth::common::crypto::{Ed25519Key, RsaKey, Sha256};
use mail_auth::common::headers::HeaderWriter;
use mail_auth::dkim::DkimSigner;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../ferrite-core/src/mail/testdata/smime/");

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{FIXTURES}{name}")).unwrap()
}

struct Target {
    host: String,
    port: u16,
    user: String,
    pass: String,
    mail_dir: Option<String>,
}

fn target() -> Option<Target> {
    let v = std::env::var("FERRITE_TEST_IMAP").ok()?;
    let mut parts = v.splitn(4, ':');
    Some(Target {
        host: parts.next()?.to_string(),
        port: parts.next()?.parse().ok()?,
        user: parts.next()?.to_string(),
        pass: parts.next()?.to_string(),
        mail_dir: std::env::var("FERRITE_TEST_MAIL_DIR").ok(),
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

    /// Delivers a message as mail arriving from outside (or APPENDs it).
    async fn deliver(&self, raw: &[u8]) {
        let raw = String::from_utf8_lossy(raw).replace("\r\n", "\n").replace('\n', "\r\n");
        match &self.mail_dir {
            Some(dir) => {
                let f = tempfile::NamedTempFile::new().unwrap();
                std::fs::write(f.path(), raw.as_bytes()).unwrap();
                let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../e2e/mail-server.sh");
                let out = std::process::Command::new("bash")
                    .arg(script)
                    .args(["deliver", "alice"])
                    .arg(f.path())
                    .arg(dir)
                    .output()
                    .unwrap();
                assert!(out.status.success(), "deliver: {}", String::from_utf8_lossy(&out.stderr));
            }
            None => {
                let mut c = ImapConn::connect(&self.settings()).await.unwrap();
                c.append("INBOX", "()", raw.as_bytes()).await.unwrap();
                c.logout().await;
            }
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
    async fn raw(&self, path: &str) -> reqwest::Response {
        self.http.get(format!("{}{path}", self.url)).send().await.unwrap()
    }
}

/// A DNS server for TXT records only (enough for DKIM key lookups).
async fn dns_server(records: Arc<Mutex<HashMap<String, String>>>) -> std::net::SocketAddr {
    let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = sock.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        loop {
            let Ok((n, peer)) = sock.recv_from(&mut buf).await else { return };
            let q = &buf[..n];
            if n < 17 {
                continue;
            }
            // Question name.
            let mut i = 12;
            let mut labels = Vec::new();
            while i < n && q[i] != 0 {
                let l = q[i] as usize;
                labels.push(String::from_utf8_lossy(&q[i + 1..i + 1 + l]).to_ascii_lowercase());
                i += 1 + l;
            }
            let qend = i + 5;
            let name = labels.join(".");
            let qtype = u16::from_be_bytes([q[i + 1], q[i + 2]]);
            let answer = records.lock().unwrap().get(&name).cloned().filter(|_| qtype == 16);
            let mut r = Vec::new();
            r.extend_from_slice(&q[..2]);
            r.extend_from_slice(if answer.is_some() { &[0x81, 0x80] } else { &[0x81, 0x83] });
            r.extend_from_slice(&[0, 1, 0, if answer.is_some() { 1 } else { 0 }, 0, 0, 0, 0]);
            r.extend_from_slice(&q[12..qend]);
            if let Some(txt) = answer {
                let mut rdata = Vec::new();
                for chunk in txt.as_bytes().chunks(255) {
                    rdata.push(chunk.len() as u8);
                    rdata.extend_from_slice(chunk);
                }
                r.extend_from_slice(&[0xc0, 0x0c, 0, 16, 0, 1, 0, 0, 0, 60]);
                r.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
                r.extend_from_slice(&rdata);
            }
            let _ = sock.send_to(&r, peer).await;
        }
    });
    addr
}

/// A tiny HTTP server: `/pixel.png` (a PNG), `/page.html` (HTML), `/redirect` (to a private
/// address); records the request headers it saw.
async fn image_server(seen: Arc<Mutex<Vec<String>>>) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { return };
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                seen.lock().unwrap().push(req.clone());
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                let png = base64::engine::general_purpose::STANDARD
                    .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")
                    .unwrap();
                let (status, ctype, body): (&str, &str, Vec<u8>) = match path.as_str() {
                    "/pixel.png" => ("200 OK", "image/png", png),
                    "/lying.png" => ("200 OK", "image/png", b"<html><script>alert(1)</script></html>".to_vec()),
                    "/huge.png" => ("200 OK", "image/png", vec![0x89; 9 * 1024 * 1024]),
                    _ => ("404 Not Found", "text/plain", b"no".to_vec()),
                };
                let head = format!("HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nSet-Cookie: tracker=1\r\nConnection: close\r\n\r\n", body.len());
                let _ = s.write_all(head.as_bytes()).await;
                let _ = s.write_all(&body).await;
            });
        }
    });
    port
}

fn message(from: &str, subject: &str, id: &str, headers: &str, body: &str) -> String {
    format!(
        "From: {from}\r\nTo: Alice <alice@example.test>\r\nSubject: {subject}\r\nDate: Mon, 19 Oct 2026 09:00:00 +0000\r\nMessage-ID: <{id}>\r\nMIME-Version: 1.0\r\n{headers}\r\n{body}"
    )
}

async fn reset_mailbox(t: &Target) {
    let mut c = ImapConn::connect(&t.settings()).await.expect("IMAP login with FERRITE_TEST_IMAP");
    for f in c.list().await.unwrap() {
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

async fn server(cfg_mut: impl FnOnce(&mut Config)) -> (tempfile::TempDir, Core, AppState, Browser, String) {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let mut cfg = Config {
        data_dir: dir.path().to_path_buf(),
        public_url: url.clone(),
        ..Config::default()
    };
    cfg.sfu.enabled = false;
    cfg_mut(&mut cfg);
    let core = Core::open(cfg).await.unwrap();
    let state = AppState::new(core.clone());
    state.start_background();
    tokio::spawn(axum::serve(listener, ferrite_server::router(state.clone())).into_future());
    let b = Browser {
        http: reqwest::Client::builder().cookie_store(true).build().unwrap(),
        url,
    };
    let setup = b
        .post("/api/setup", json!({"username": "alice", "password": "alice-password", "display_name": "Alice"}))
        .await;
    let user_id = setup["user"]["id"].as_str().unwrap().to_string();
    (dir, core, state, b, user_id)
}

/// Security APIs that need no mail server: settings, lists, S/MIME import and its storage,
/// the proxy's URL signing, the admin switch and the MCP tools.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn security_apis_without_a_mail_server() {
    let (_dir, core, state, b, user_id) = server(|_| {}).await;

    // Settings default off; patch them.
    let s = b.get("/api/mail/security/settings").await;
    assert_eq!(s["settings"], json!({"internal_enabled": false, "index_decrypted": false, "assistant_decrypted": false}));
    let (st, v) = b.call(Method::PATCH, "/api/mail/security/settings", Some(json!({"internal_enabled": true}))).await;
    assert!(st.is_success() && v["settings"]["internal_enabled"] == true, "{v}");

    // Trusted senders: addresses and domains, normalized, de-duplicated, removable.
    let e = b.post("/api/mail/trusted-senders", json!({"value": " News@Example.ORG "})).await;
    assert_eq!((e["entry"]["kind"].as_str(), e["entry"]["value"].as_str()), (Some("address"), Some("news@example.org")));
    b.post("/api/mail/trusted-senders", json!({"value": "news@example.org"})).await;
    b.post("/api/mail/trusted-senders", json!({"value": "@shop.example"})).await;
    let list = b.get("/api/mail/trusted-senders").await["entries"].clone();
    assert_eq!(list.as_array().unwrap().len(), 2, "{list}");
    for bad in ["", "not valid", "a@b@c.example", "<x@y.example>z", "localhost"] {
        let (st, _) = b.call(Method::POST, "/api/mail/trusted-senders", Some(json!({"value": bad}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{bad:?} must be refused");
    }
    let (st, _) = b.call(Method::DELETE, "/api/mail/trusted-senders/shop.example", None).await;
    assert!(st.is_success());
    let (st, _) = b.call(Method::DELETE, "/api/mail/trusted-senders/nothing.example", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Internal addresses and the composer's check (own addresses are not special here).
    b.post("/api/mail/internal-addresses", json!({"value": "example.test"})).await;
    let c = b
        .post("/api/mail/security/check-recipients", json!({"recipients": ["Bob <bob@example.test>", "eve@attacker.example", "x@sub.example.test"]}))
        .await;
    assert_eq!(c["external"], json!(["eve@attacker.example", "x@sub.example.test"]), "{c}");
    assert_eq!(c["smime"].as_array().unwrap().len(), 3);
    assert!(c["smime"].as_array().unwrap().iter().all(|r| r["has_certificate"] == false));

    // S/MIME: a PKCS#12 import; the key is sealed at rest and never returned.
    let p12 = base64::engine::general_purpose::STANDARD.encode(fixture("alice.p12"));
    let (st, v) = b.call(Method::POST, "/api/mail/smime/certificates", Some(json!({"pkcs12": p12, "password": "wrong"}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"].as_str().unwrap().contains("password"), "{v}");
    let cert = b.post("/api/mail/smime/certificates", json!({"pkcs12": p12, "password": "alice-p12"})).await["certificate"].clone();
    assert_eq!(cert["kind"], "own");
    assert_eq!(cert["has_key"], true);
    assert_eq!(cert["emails"], json!(["alice@example.test"]));
    assert!(cert.get("key_sealed").is_none() && !cert.to_string().contains("PRIVATE"));
    let sealed: String = sqlx::query_scalar("SELECT key_sealed FROM mail_smime_certs WHERE id = ?")
        .bind(cert["id"].as_str().unwrap())
        .fetch_one(&core.db)
        .await
        .unwrap();
    let alice_key_der = ferrite_core::mail::smime::read_private_key(&fixture("alice.key"), None).unwrap();
    let key_b64 = base64::engine::general_purpose::STANDARD.encode(alice_key_der.pkcs8.as_slice());
    assert!(!sealed.contains(&key_b64[..40]), "the private key is encrypted at rest");
    // The sealed key is bound to its row: copied to another row it does not open.
    let other = b
        .post("/api/mail/smime/certificates", json!({"certificate": String::from_utf8(fixture("bob.pem")).unwrap()}))
        .await["certificate"]
        .clone();
    assert_eq!((other["kind"].as_str(), other["has_key"].as_bool()), (Some("contact"), Some(false)));
    let sb = ferrite_core::mail::crypto::SecretBox::new(&core.secret);
    let opened = sb.open(&format!("smime-key:{}", cert["id"].as_str().unwrap()), &sealed).unwrap();
    assert_eq!(opened, key_b64, "it is Alice's key, sealed");
    assert!(sb.open(&format!("smime-key:{}", other["id"].as_str().unwrap()), &sealed).is_err());
    assert!(ferrite_core::mail::crypto::SecretBox::new("another-server-secret").open(&format!("smime-key:{}", cert["id"].as_str().unwrap()), &sealed).is_err());
    // PEM certificate + PEM key import; a key that does not match is refused.
    let (st, v) = b
        .call(
            Method::POST,
            "/api/mail/smime/certificates",
            Some(json!({"certificate": String::from_utf8(fixture("bob.pem")).unwrap(), "private_key": String::from_utf8(fixture("alice.key")).unwrap()})),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(v["error"].as_str().unwrap().contains("does not belong"), "{v}");
    // A CA becomes a trusted issuer; the PEM download never includes a key.
    let ca = b
        .post("/api/mail/smime/certificates", json!({"certificate": String::from_utf8(fixture("ca.pem")).unwrap()}))
        .await["certificate"]
        .clone();
    assert_eq!(ca["kind"], "authority");
    let pem = b.raw(&format!("/api/mail/smime/certificates/{}/pem", cert["id"].as_str().unwrap())).await.text().await.unwrap();
    assert!(pem.starts_with("-----BEGIN CERTIFICATE-----") && !pem.contains("PRIVATE"));
    // An expired certificate is flagged.
    let old = b
        .post("/api/mail/smime/certificates", json!({"certificate": String::from_utf8(fixture("expired.pem")).unwrap()}))
        .await["certificate"]
        .clone();
    assert_eq!(old["expired"], true);

    // The image proxy only takes URLs the server signed for this user, until they expire.
    let url = core.mail_proxy_url(&user_id, "https://img.example/a.png?x=1").unwrap();
    let q: HashMap<String, String> = reqwest::Url::parse(&format!("http://x{url}")).unwrap().query_pairs().into_owned().collect();
    let e: i64 = q["e"].parse().unwrap();
    assert_eq!(core.mail_proxy_check(&user_id, &q["u"], e, &q["s"]).unwrap(), "https://img.example/a.png?x=1");
    assert!(core.mail_proxy_check("someone-else", &q["u"], e, &q["s"]).is_err(), "bound to the user");
    assert!(core.mail_proxy_check(&user_id, &q["u"], e + 1, &q["s"]).is_err(), "expiry is signed");
    let other_u = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("http://169.254.169.254/latest/meta-data/");
    assert!(core.mail_proxy_check(&user_id, &other_u, e, &q["s"]).is_err(), "not an open proxy");
    let past = chrono::Utc::now().timestamp() - 10;
    assert!(core.mail_proxy_check(&user_id, &q["u"], past, &q["s"]).is_err(), "expired");
    assert!(core.mail_proxy_url(&user_id, "javascript:alert(1)").is_none());
    assert!(core.mail_proxy_url(&user_id, "file:///etc/passwd").is_none());
    let (st, _) = b.call(Method::GET, &url.replace(&format!("s={}", q["s"]), &format!("s={}", "0".repeat(64))), None).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    // Signed for this user but pointing at a private address: the SSRF guard refuses.
    let ssrf = core.mail_proxy_url(&user_id, "http://127.0.0.1:9/x.png").unwrap();
    let r = b.raw(&ssrf).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert!(matches!(
        ferrite_server::mail_fetch::image("http://[::1]/x.png", 1000, false).await,
        Err(ferrite_core::Error::Forbidden)
    ));

    // Gravatar is off unless the admin allows it; avatars without a source are null (initials).
    assert!(!core.mail_gravatar().await);
    let av = b.get("/api/mail/avatars?emails=nobody@nonexistent.invalid,not-an-address").await;
    assert_eq!(av["avatars"], json!({"nobody@nonexistent.invalid": null}), "{av}");
    let miss: Option<Option<Vec<u8>>> = sqlx::query_scalar("SELECT data FROM mail_avatar_cache WHERE key = 'favicon:nonexistent.invalid'")
        .fetch_optional(&core.db)
        .await
        .unwrap();
    assert_eq!(miss, Some(None), "misses are cached");

    // MCP: list and edit the lists; editing is not read-only.
    let ctx = ferrite_server::tools::UserCtx {
        user: core.user(&user_id).await.unwrap(),
        scopes: "mail".into(),
    };
    let tools = state.tools.list(&ctx);
    let def = |n: &str| tools.iter().find(|t| t.name == n).unwrap_or_else(|| panic!("tool {n}")).clone();
    assert!(def("mail_trusted_senders").annotations.is_some_and(|a| a["readOnlyHint"] == true));
    assert!(def("mail_trusted_senders_edit").annotations.is_none_or(|a| a["readOnlyHint"] != true));
    let r = state
        .tools
        .call(&ctx, "mail_trusted_senders_edit", json!({"add": ["deals.example"], "remove": ["news@example.org"]}))
        .await
        .unwrap();
    assert!(r.text_content().contains("deals.example") && !r.text_content().contains("news@example.org"), "{}", r.text_content());
    let r = state
        .tools
        .call(&ctx, "mail_internal_addresses_edit", json!({"add": ["corp.example"], "warn": false}))
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&r.text_content()).unwrap();
    assert!(v["warn"] == false && v["internal_addresses"].to_string().contains("corp.example"), "{v}");
    // Another user sees none of it.
    let (st, _) = b
        .call(Method::POST, "/api/admin/users", Some(json!({"username": "bob", "password": "bob-password-1", "display_name": "Bob"})))
        .await;
    assert!(st.is_success());
    let bob = Browser {
        http: reqwest::Client::builder().cookie_store(true).build().unwrap(),
        url: b.url.clone(),
    };
    bob.post("/api/auth/login", json!({"username": "bob", "password": "bob-password-1"})).await;
    assert_eq!(bob.get("/api/mail/trusted-senders").await["entries"], json!([]));
    assert_eq!(bob.get("/api/mail/smime/certificates").await["certificates"], json!([]));
    let (st, _) = bob.call(Method::DELETE, &format!("/api/mail/smime/certificates/{}", cert["id"].as_str().unwrap()), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = bob.call(Method::GET, &url, None).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "alice's proxy URL does not work for bob");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mail_security_against_a_real_imap_server() {
    let Some(t) = target() else {
        eprintln!("FERRITE_TEST_IMAP is not set; skipping the real-server mail security test (see the top of tests/mail_security.rs)");
        return;
    };
    let _ = tracing_subscriber::fmt().with_env_filter("warn,ferrite=info").with_test_writer().try_init();
    reset_mailbox(&t).await;

    // DNS with DKIM keys generated here.
    let records = Arc::new(Mutex::new(HashMap::new()));
    let dns = dns_server(records.clone()).await;
    let ed_pkcs8 = Ed25519Key::generate_pkcs8().unwrap();
    let ed = Ed25519Key::from_pkcs8_der(&ed_pkcs8).unwrap();
    records.lock().unwrap().insert(
        "ed._domainkey.signed.example".into(),
        format!("v=DKIM1; k=ed25519; p={}", base64::engine::general_purpose::STANDARD.encode(ed.public_key())),
    );
    let rsa_kp = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap();
    let rsa_pkcs8 = aws_lc_rs::encoding::AsDer::<aws_lc_rs::encoding::Pkcs8V1Der>::as_der(&rsa_kp).unwrap();
    let rsa_spki = aws_lc_rs::encoding::AsDer::<aws_lc_rs::encoding::PublicKeyX509Der>::as_der(aws_lc_rs::signature::KeyPair::public_key(&rsa_kp)).unwrap();
    records.lock().unwrap().insert(
        "rsa._domainkey.signed.example".into(),
        format!("v=DKIM1; k=rsa; p={}", base64::engine::general_purpose::STANDARD.encode(rsa_spki.as_ref())),
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let img_port = image_server(seen.clone()).await;
    let (_dir, core, state, b, user_id) = server(|c| {
        c.mail_security.dns_server = Some(dns.to_string());
        c.mail_security.proxy_allow_private = true;
    })
    .await;
    let (smtp_port, mut sink) = ferrite_core::mailer::sink::sink().await;
    let (st, v) = b.call(Method::PATCH, "/api/admin/mail/settings", Some(json!({"allow_local_servers": true}))).await;
    assert!(st.is_success(), "{v}");
    let account = b
        .post(
            "/api/mail/accounts",
            json!({"email": t.user, "name": "Alice", "password": t.pass, "imap_host": t.host, "imap_port": t.port, "imap_security": "none",
                   "smtp_host": "127.0.0.1", "smtp_port": smtp_port, "smtp_security": "none", "smtp_auth": false}),
        )
        .await["account"]
        .clone();
    let acc = account["id"].as_str().unwrap().to_string();

    // A contact named like a phisher's display name, with a photo.
    let book = core.dav_collections(&user_id, "addressbook").await.unwrap().remove(0);
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    let vcard = format!("BEGIN:VCARD\r\nVERSION:3.0\r\nUID:ada\r\nFN:Ada Lovelace\r\nEMAIL:ada@example.test\r\nPHOTO;ENCODING=b;TYPE=PNG:{png}\r\nEND:VCARD\r\n");
    core.put_dav_object(&book, "ada.vcf", &vcard, None, true).await.unwrap();

    // Mail arrives.
    let sign = |raw: String, key: &str| -> String {
        let sig = match key {
            "ed" => DkimSigner::from_key(Ed25519Key::from_pkcs8_der(&ed_pkcs8).unwrap())
                .domain("signed.example")
                .selector("ed")
                .headers(["From", "To", "Subject", "Date", "Message-ID"])
                .sign(raw.as_bytes())
                .unwrap()
                .to_header(),
            _ => DkimSigner::from_key(
                RsaKey::<Sha256>::from_key_der(rustls_pki_types::PrivateKeyDer::Pkcs8(rsa_pkcs8.as_ref().to_vec().into())).unwrap(),
            )
                .domain("signed.example")
                .selector("rsa")
                .headers(["From", "To", "Subject", "Date", "Message-ID"])
                .sign(raw.as_bytes())
                .unwrap()
                .to_header(),
        };
        format!("{sig}{raw}")
    };
    let plain = "Content-Type: text/plain; charset=utf-8\r\n";
    t.deliver(sign(message("Signer <news@signed.example>", "DKIM ed25519", "dkim-ed@signed.example", plain, "Signed with Ed25519.\r\n"), "ed").as_bytes())
        .await;
    t.deliver(sign(message("Signer <news@signed.example>", "DKIM rsa", "dkim-rsa@signed.example", plain, "Signed with RSA.\r\n"), "rsa").as_bytes())
        .await;
    let tampered = sign(message("Signer <news@signed.example>", "DKIM tampered", "dkim-bad@signed.example", plain, "Pay 10 EUR.\r\n"), "ed").replace("Pay 10 EUR.", "Pay 10000 EUR.");
    t.deliver(tampered.as_bytes()).await;
    let phish_html = "Content-Type: text/html; charset=utf-8\r\nReply-To: collect@attacker.example\r\n";
    let phish = message(
        "\"Ada Lovelace\" <ada.lovelace@freemail.example>",
        "Urgent invoice",
        "phish@freemail.example",
        phish_html,
        "<p>Pay at <a href=\"https://attacker.example/pay\">https://bank.example/pay</a></p>\r\n",
    )
    .replace("Date: Mon, 19 Oct 2026 09:00:00 +0000", "Date: Fri, 1 Jan 2038 09:00:00 +0000");
    t.deliver(phish.as_bytes()).await;
    let fake_name = message("\"support@bank.example\" <x@attacker.example>", "Account locked", "fakename@attacker.example", plain, "Click.\r\n");
    t.deliver(fake_name.as_bytes()).await;
    let images = message(
        "Newsletter <news@shop.example>",
        "Remote images",
        "images@shop.example",
        "Content-Type: text/html; charset=utf-8\r\n",
        &format!(
            "<p>Hello</p><img src=\"http://127.0.0.1:{img_port}/pixel.png\"><img src=\"http://127.0.0.1:{img_port}/lying.png\"><div style=\"background:url(http://127.0.0.1:{img_port}/pixel.png?bg)\">x</div>\r\n"
        ),
    );
    t.deliver(images.as_bytes()).await;
    let att = message(
        "Ada Lovelace <ada@example.test>",
        "Two files",
        "files@example.test",
        "Content-Type: multipart/mixed; boundary=\"B\"\r\n",
        "--B\r\nContent-Type: text/plain\r\n\r\nSee attached.\r\n--B\r\nContent-Type: text/plain; name=\"a.txt\"\r\nContent-Disposition: attachment; filename=\"a.txt\"\r\n\r\nfirst file\r\n--B\r\nContent-Type: text/plain; name=\"a.txt\"\r\nContent-Disposition: attachment; filename=\"a.txt\"\r\n\r\nsecond file\r\n--B\r\nContent-Type: application/octet-stream; name=\"../../evil.sh\"\r\nContent-Disposition: attachment; filename=\"../../evil.sh\"\r\nContent-Transfer-Encoding: base64\r\n\r\nZWNobyBoaQo=\r\n--B--\r\n",
    );
    t.deliver(att.as_bytes()).await;
    let pgp = message(
        "Bob <bob@example.test>",
        "PGP inline",
        "pgp@example.test",
        plain,
        "-----BEGIN PGP MESSAGE-----\r\n\r\nhQEMA1x3P5A+pgptestarmor\r\n=abcd\r\n-----END PGP MESSAGE-----\r\n",
    );
    t.deliver(pgp.as_bytes()).await;
    for f in ["encrypted-aes.eml", "signed-detached.eml", "encrypted-ber.eml", "signed-encrypted.eml"] {
        t.deliver(&fixture(f)).await;
    }

    // The server marks one as phishing (as a spam filter would).
    {
        let mut c = ImapConn::connect(&t.settings()).await.unwrap();
        c.select("INBOX").await.unwrap();
        let uids = c.uid_search("HEADER Message-ID \"fakename@attacker.example\"").await.unwrap();
        c.uid_store(&uids[0].to_string(), "+FLAGS ($Phishing $Junk)").await.unwrap();
        c.logout().await;
    }

    let folders = b.get(&format!("/api/mail/accounts/{acc}/folders")).await["folders"].clone();
    let inbox = folders.as_array().unwrap().iter().find(|f| f["role"] == "inbox").unwrap()["id"].as_str().unwrap().to_string();
    let mut list = Value::Null;
    for _ in 0..100 {
        b.call(Method::POST, &format!("/api/mail/accounts/{acc}/sync"), Some(json!({"folder_id": inbox}))).await;
        list = b.get(&format!("/api/mail/folders/{inbox}/messages?limit=100")).await;
        if list["messages"].as_array().unwrap().len() >= 12 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(list["messages"].as_array().unwrap().len(), 12, "{list}");
    let id_of = |subject: &str| -> String {
        list["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["subject"] == subject)
            .unwrap_or_else(|| panic!("no message {subject:?}"))["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let report = |id: String| {
        let b = &b;
        async move { b.get(&format!("/api/mail/messages/{id}/security")).await["security"].clone() }
    };

    // DKIM over real DNS lookups, stored once verified.
    let r = report(id_of("DKIM ed25519")).await;
    assert_eq!((r["dkim"]["status"].as_str(), r["dkim"]["domain"].as_str()), (Some("pass"), Some("signed.example")), "{r}");
    let r = report(id_of("DKIM rsa")).await;
    assert_eq!(r["dkim"]["status"], "pass", "{r}");
    let r = report(id_of("DKIM tampered")).await;
    assert_eq!(r["dkim"]["status"], "fail", "{r}");
    let r = report(id_of("Two files")).await;
    assert_eq!(r["dkim"]["status"], "none");
    // Stored: with the key gone from DNS, the result stays.
    records.lock().unwrap().clear();
    assert_eq!(report(id_of("DKIM ed25519")).await["dkim"]["status"], "pass");
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mail_dkim_results WHERE status = 'pass'").fetch_one(&core.db).await.unwrap();
    assert_eq!(stored, 2);

    // Phishing hints.
    let r = report(id_of("Urgent invoice")).await;
    let kinds: Vec<&str> = r["phishing"].as_array().unwrap().iter().map(|f| f["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, vec!["contact_mismatch", "reply_to", "future_date", "misleading_link"], "{r}");
    assert!(r["phishing"][3]["links"][0][1] == "https://attacker.example/pay");
    let r = report(id_of("Account locked")).await;
    let kinds: Vec<&str> = r["phishing"].as_array().unwrap().iter().map(|f| f["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, vec!["custom_address", "server_flag"], "{r}");
    assert!(r["keywords"].as_array().unwrap().iter().any(|k| k == "$Phishing"));
    assert_eq!(report(id_of("Two files")).await["phishing"], json!([]));

    // Internal addresses: senders outside the list are external (the switch is on).
    b.call(Method::PATCH, "/api/mail/security/settings", Some(json!({"internal_enabled": true}))).await;
    b.post("/api/mail/internal-addresses", json!({"value": "example.test"})).await;
    assert_eq!(report(id_of("Urgent invoice")).await["external"], true);
    assert_eq!(report(id_of("Two files")).await["external"], false);

    // Remote images: blocked by default; through the proxy when shown; trusted senders.
    let id = id_of("Remote images");
    let m = b.get(&format!("/api/mail/messages/{id}")).await["message"].clone();
    assert_eq!(m["remote_content"], true);
    let blocked = b.raw(&format!("/api/mail/messages/{id}/html")).await;
    let csp = blocked.headers()["content-security-policy"].to_str().unwrap().to_string();
    assert!(csp.contains("img-src 'self' data:;") && !csp.contains("http:"), "{csp}");
    let doc = blocked.text().await.unwrap();
    assert!(!doc.contains("127.0.0.1"), "{doc}");
    let shown = b.raw(&format!("/api/mail/messages/{id}/html?remote=true")).await;
    assert_eq!(shown.headers()["content-security-policy"].to_str().unwrap(), csp, "the same CSP: images only from Ferrite");
    let doc = shown.text().await.unwrap();
    assert!(!doc.contains(&format!("127.0.0.1:{img_port}")), "no direct remote URL left: {doc}");
    let proxied: Vec<String> = doc
        .split("/api/mail/proxy?")
        .skip(1)
        .map(|s| {
            let s = s.split("&quot;").next().unwrap();
            format!("/api/mail/proxy?{}", s.split(['"', ')']).next().unwrap().replace("&amp;", "&"))
        })
        .collect();
    assert_eq!(proxied.len(), 3, "{doc}");
    let before = seen.lock().unwrap().len();
    let r = b.raw(&proxied[0]).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["content-type"], "image/png");
    assert_eq!(r.headers()["x-content-type-options"], "nosniff");
    assert!(r.headers().get("set-cookie").is_none(), "the remote's cookies are not passed on");
    assert!(r.bytes().await.unwrap().starts_with(b"\x89PNG"));
    let upstream = seen.lock().unwrap()[before..].join("\n").to_ascii_lowercase();
    assert!(upstream.contains("get /pixel.png"), "{upstream}");
    assert!(!upstream.contains("cookie:") && !upstream.contains("referer:"), "no cookies or referrer upstream: {upstream}");
    // A "PNG" that is HTML is not served.
    let r = b.raw(&proxied[1]).await;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    // Over the size limit.
    let huge = core.mail_proxy_url(&user_id, &format!("http://127.0.0.1:{img_port}/huge.png")).unwrap();
    assert_eq!(b.raw(&huge).await.status(), StatusCode::UNPROCESSABLE_ENTITY);
    // Trusting the sender's domain is reported (the UI then loads images right away).
    assert!(report(id.clone()).await["trusted_sender"].is_null());
    b.post("/api/mail/trusted-senders", json!({"value": "shop.example", "kind": "domain"})).await;
    let r = report(id.clone()).await;
    assert_eq!((r["trusted_sender"]["kind"].as_str(), r["trusted_sender"]["value"].as_str()), (Some("domain"), Some("shop.example")));

    // Avatars: the contact's photo.
    let av = b.get("/api/mail/avatars?emails=ada@example.test").await;
    assert!(av["avatars"]["ada@example.test"].as_str().unwrap().starts_with("data:image/png;base64,iVBOR"), "{av}");

    // View source and the ZIP of attachments.
    let src = b.get(&format!("/api/mail/messages/{}/source", id_of("Two files"))).await;
    assert!(src["source"].as_str().unwrap().contains("Message-ID: <files@example.test>") && src["truncated"] == false);
    let z = b.raw(&format!("/api/mail/messages/{}/attachments.zip", id_of("Two files"))).await;
    assert_eq!(z.headers()["content-type"], "application/zip");
    assert!(z.headers()["content-disposition"].to_str().unwrap().contains("Two files - attachments.zip"));
    let zip = z.bytes().await.unwrap();
    let names: Vec<String> = {
        let eocd = &zip[zip.len() - 22..];
        let count = u16::from_le_bytes([eocd[10], eocd[11]]) as usize;
        let mut off = u32::from_le_bytes(eocd[16..20].try_into().unwrap()) as usize;
        (0..count)
            .map(|_| {
                let n = u16::from_le_bytes([zip[off + 28], zip[off + 29]]) as usize;
                let extra = u16::from_le_bytes([zip[off + 30], zip[off + 31]]) as usize;
                let comment = u16::from_le_bytes([zip[off + 32], zip[off + 33]]) as usize;
                let name = String::from_utf8(zip[off + 46..off + 46 + n].to_vec()).unwrap();
                off += 46 + n + extra + comment;
                name
            })
            .collect()
    };
    assert_eq!(names, vec!["a.txt", "a (2).txt", "_.._evil.sh"], "unique names, no paths");
    let (st, _) = b.call(Method::GET, &format!("/api/mail/messages/{}/attachments.zip", id_of("DKIM rsa")), None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "no attachments");

    // PGP inline is detected and handed over armored (Mailvelope decrypts in the browser).
    let r = report(id_of("PGP inline")).await;
    assert_eq!(r["pgp"]["kind"], "inline");
    assert!(r["pgp"]["armored"].as_str().unwrap().starts_with("-----BEGIN PGP MESSAGE-----\n"));

    // S/MIME: without a key the message cannot be read.
    let enc_id = id_of("Encrypted AES");
    let m = b.get(&format!("/api/mail/messages/{enc_id}")).await["message"].clone();
    assert_eq!(m["encrypted"], true);
    assert!(!m["text"].as_str().unwrap().contains("launch code"));
    let r = report(enc_id.clone()).await;
    assert_eq!((r["smime"]["encrypted"].as_bool(), r["smime"]["decrypted"].as_bool()), (Some(true), Some(false)));
    assert!(r["smime"]["decrypt_error"].as_str().unwrap().contains("import your certificate"), "{r}");
    // With Alice's PKCS#12 it is decrypted (cached bodies of encrypted mail are re-read).
    let p12 = base64::engine::general_purpose::STANDARD.encode(fixture("alice.p12"));
    let alice_cert = b.post("/api/mail/smime/certificates", json!({"pkcs12": p12, "password": "alice-p12"})).await["certificate"].clone();
    for (subject, code) in [("Encrypted AES", "3456"), ("Encrypted BER", "8901"), ("Signed and encrypted", "6789")] {
        let m = b.get(&format!("/api/mail/messages/{}", id_of(subject))).await["message"].clone();
        assert!(m["text"].as_str().unwrap().contains(&format!("The launch code is {code}.")), "{subject}: {m}");
        assert_eq!(m["preview"], "", "decrypted text stays out of previews");
        assert!(m["attachments"].as_array().unwrap().iter().all(|a| a["name"] != "smime.p7m" && a["name"] != "smime.p7s"), "{m}");
    }
    let r = report(enc_id.clone()).await;
    assert_eq!(r["smime"]["decrypted"], true, "{r}");
    let fts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fts WHERE body LIKE '%3456%' OR body LIKE '%8901%' OR body LIKE '%6789%'").fetch_one(&core.db).await.unwrap();
    assert_eq!(fts, 0, "decrypted text is not in the shared index");
    // Signatures: valid, untrusted until the CA is trusted; the signer's certificate is kept.
    let signed_id = id_of("Signed detached");
    let r = report(signed_id.clone()).await;
    let sig = &r["smime"]["signatures"][0];
    assert_eq!((sig["valid"].as_bool(), sig["matches_sender"].as_bool()), (Some(true), Some(true)), "{r}");
    // The PKCS#12 brought the CA along as Alice's chain, but that does not make it trusted.
    assert_eq!(sig["chain"]["status"], "untrusted", "{r}");
    b.post("/api/mail/smime/certificates", json!({"certificate": String::from_utf8(fixture("ca.pem")).unwrap()})).await;
    let r = report(signed_id.clone()).await;
    assert_eq!(r["smime"]["signatures"][0]["chain"]["status"], "trusted", "{r}");
    assert_eq!(r["smime"]["signatures"][0]["chain"]["path"].as_array().unwrap().len(), 2, "{r}");
    let certs = b.get("/api/mail/smime/certificates").await["certificates"].clone();
    assert!(certs.as_array().unwrap().iter().any(|c| c["kind"] == "contact" && c["source"] == "collected" && c["emails"][0] == "bob@example.test"), "{certs}");
    let m = b.get(&format!("/api/mail/messages/{signed_id}")).await["message"].clone();
    assert!(m["text"].as_str().unwrap().contains("1234") && m["attachments"].as_array().unwrap().is_empty(), "{m}");

    // Search: decrypted text only with the opt-in, and only in Mail search.
    let found = b.get("/api/mail/search?q=launch").await["messages"].clone();
    assert!(found.as_array().unwrap().iter().all(|m| m["subject"] != "Encrypted AES"), "{found}");
    b.call(Method::PATCH, "/api/mail/security/settings", Some(json!({"index_decrypted": true}))).await;
    let found = b.get("/api/mail/search?q=launch").await["messages"].clone();
    assert!(found.as_array().unwrap().iter().any(|m| m["subject"] == "Encrypted AES"), "{found}");
    let global = b.get("/api/search?q=launch").await;
    assert!(!global.to_string().contains("launch code"), "global search never shows decrypted text: {global}");

    // The assistant does not see decrypted mail unless allowed.
    let ctx = ferrite_server::tools::UserCtx {
        user: core.user(&user_id).await.unwrap(),
        scopes: "mail".into(),
    };
    let read = state.tools.call(&ctx, "mail_read", json!({"message_id": enc_id})).await.unwrap();
    assert!(!read.text_content().contains("launch code") && read.text_content().contains("encrypted"), "{}", read.text_content());
    let s = state.tools.call(&ctx, "mail_search", json!({"query": "launch"})).await.unwrap();
    assert!(!s.text_content().contains("Encrypted AES"), "{}", s.text_content());
    b.call(Method::PATCH, "/api/mail/security/settings", Some(json!({"assistant_decrypted": true}))).await;
    let read = state.tools.call(&ctx, "mail_read", json!({"message_id": enc_id})).await.unwrap();
    assert!(read.text_content().contains("launch code is 3456"), "{}", read.text_content());

    // Sending signed and encrypted: Bob's collected certificate; Carol has none.
    let (st, v) = b
        .call(
            Method::PUT,
            "/api/mail/smime/identities",
            Some(json!({"account_id": acc, "cert_id": alice_cert["id"], "sign_default": true, "encrypt_default": false})),
        )
        .await;
    assert!(st.is_success(), "{v}");
    let (st, v) = b
        .call(
            Method::POST,
            &format!("/api/mail/accounts/{acc}/send"),
            Some(json!({"to": ["carol@example.test"], "subject": "Secret", "text": "x", "security": {"smime_encrypt": true}})),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(v["error"].as_str().unwrap().contains("carol@example.test"), "{v}");
    assert!(tokio::time::timeout(Duration::from_millis(300), sink.recv()).await.is_err(), "nothing was sent");
    b.post(
        &format!("/api/mail/accounts/{acc}/send"),
        json!({"to": ["Bob <bob@example.test>"], "subject": "Signed and sealed", "text": "Meet at the observatory. ü", "security": {"smime_sign": true, "smime_encrypt": true}}),
    )
    .await;
    let got = tokio::time::timeout(Duration::from_secs(10), sink.recv()).await.unwrap().unwrap();
    assert!(got.data.contains("application/pkcs7-mime; smime-type=enveloped-data") && !got.data.contains("observatory"), "{}", got.data);
    assert!(got.data.contains("Subject: Signed and sealed"));
    if std::process::Command::new("openssl").arg("version").output().is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("wire.eml");
        std::fs::write(&p, got.data.as_bytes()).unwrap();
        let dec = std::process::Command::new("openssl")
            .args(["cms", "-decrypt", "-in"])
            .arg(&p)
            .args(["-recip", &format!("{FIXTURES}bob.pem"), "-inkey", &format!("{FIXTURES}bob.key")])
            .output()
            .unwrap();
        assert!(dec.status.success(), "openssl decrypt: {}", String::from_utf8_lossy(&dec.stderr));
        let inner = dir.path().join("inner.eml");
        std::fs::write(&inner, &dec.stdout).unwrap();
        let ver = std::process::Command::new("openssl")
            .args(["cms", "-verify", "-in"])
            .arg(&inner)
            .args(["-CAfile", &format!("{FIXTURES}ca.pem")])
            .output()
            .unwrap();
        assert!(ver.status.success(), "openssl verify: {}", String::from_utf8_lossy(&ver.stderr));
        assert!(String::from_utf8_lossy(&ver.stdout).contains("Meet at the observatory."));
    }
    // The Sent copy is encrypted to Alice too, so she can read it.
    let sent = folders.as_array().unwrap().iter().find(|f| f["role"] == "sent").unwrap()["id"].as_str().unwrap().to_string();
    let sent_list = b.get(&format!("/api/mail/folders/{sent}/messages")).await;
    let sent_id = sent_list["messages"].as_array().unwrap().iter().find(|m| m["subject"] == "Signed and sealed").unwrap()["id"].as_str().unwrap().to_string();
    let m = b.get(&format!("/api/mail/messages/{sent_id}")).await["message"].clone();
    assert!(m["text"].as_str().unwrap().contains("Meet at the observatory. ü"), "{m}");
    let r = report(sent_id).await;
    assert_eq!(r["smime"]["signatures"][0]["valid"], true, "{r}");

    // PGP/MIME from Mailvelope's armored text; attachments are refused.
    b.post(
        &format!("/api/mail/accounts/{acc}/send"),
        json!({"to": ["bob@example.test"], "subject": "PGP out", "text": "-----BEGIN PGP MESSAGE-----\n\nwcBMAxyz\n=abcd\n-----END PGP MESSAGE-----\n", "security": {"pgp_mime": true}}),
    )
    .await;
    let got = tokio::time::timeout(Duration::from_secs(10), sink.recv()).await.unwrap().unwrap();
    assert!(got.data.contains("multipart/encrypted; protocol=\"application/pgp-encrypted\"") && got.data.contains("Version: 1"), "{}", got.data);
    let (st, _) = b
        .call(
            Method::POST,
            &format!("/api/mail/accounts/{acc}/send"),
            Some(json!({"to": ["bob@example.test"], "subject": "PGP plain", "text": "not encrypted", "security": {"pgp_mime": true}})),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "no PGP block: not sent as PGP/MIME");

    // Deleting the key forgets decrypted mail.
    b.call(Method::DELETE, &format!("/api/mail/smime/certificates/{}", alice_cert["id"].as_str().unwrap()), None).await;
    let m = b.get(&format!("/api/mail/messages/{enc_id}")).await["message"].clone();
    assert!(!m["text"].as_str().unwrap().contains("launch code"), "{m}");
    let idx: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mail_decrypted_fts").fetch_one(&core.db).await.unwrap();
    assert_eq!(idx, 0);
    reset_mailbox(&t).await;
}
