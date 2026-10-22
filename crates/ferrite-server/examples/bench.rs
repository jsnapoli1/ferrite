//! Load benchmark against a running Ferrite server.
//!
//!   cargo run --release --example bench -- --url http://127.0.0.1:8900 --pid <server pid> \
//!       [--users 50] [--conns 20] [--messages 200] [--http-secs 5]
//!
//! It creates `users` accounts in one Talk room, opens `conns` event sockets per user, posts
//! `messages` chat messages and measures how long each takes to reach every socket. It then
//! measures HTTP request rates and WebDAV upload/download throughput. When `--pid` is given,
//! the server's resident memory is read from /proc at each stage. Output is JSON.

use futures::StreamExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;

struct Args {
    url: String,
    pid: Option<u32>,
    users: usize,
    conns: usize,
    messages: usize,
    http_secs: u64,
}

fn args() -> Args {
    let mut a = Args {
        url: "http://127.0.0.1:8900".into(),
        pid: None,
        users: 50,
        conns: 20,
        messages: 200,
        http_secs: 5,
    };
    let v: Vec<String> = std::env::args().skip(1).collect();
    for pair in v.chunks(2) {
        let val = pair.get(1).cloned().unwrap_or_default();
        match pair[0].as_str() {
            "--url" => a.url = val,
            "--pid" => a.pid = val.parse().ok(),
            "--users" => a.users = val.parse().expect("--users"),
            "--conns" => a.conns = val.parse().expect("--conns"),
            "--messages" => a.messages = val.parse().expect("--messages"),
            "--http-secs" => a.http_secs = val.parse().expect("--http-secs"),
            other => panic!("unknown argument {other}"),
        }
    }
    a
}

/// Resident set size of `pid` in bytes (VmRSS).
fn rss(pid: Option<u32>) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{}/status", pid?)).ok()?;
    let kb: u64 = status.lines().find(|l| l.starts_with("VmRSS:"))?.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

fn mib(b: Option<u64>) -> Value {
    b.map(|b| json!((b as f64 / 1048576.0 * 10.0).round() / 10.0)).unwrap_or(Value::Null)
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    (sorted[i] * 100.0).round() / 100.0
}

/// Logs in with a cookie session and mints an API token.
async fn token_for(url: &str, user: &str, password: &str) -> String {
    let http = reqwest::Client::builder().cookie_store(true).build().unwrap();
    let r = http
        .post(format!("{url}/api/auth/login"))
        .header("X-Ferrite-Request", "1")
        .json(&json!({"username": user, "password": password}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "login {user}: {}", r.status());
    let t: Value = http
        .post(format!("{url}/api/me/tokens"))
        .header("X-Ferrite-Request", "1")
        .json(&json!({"name": "bench"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    t["secret"].as_str().expect("token secret").to_string()
}

#[tokio::main]
async fn main() {
    let a = args();
    let url = a.url.trim_end_matches('/').to_string();
    let http = reqwest::Client::new();
    let mut report = json!({"users": a.users, "connections": a.users * a.conns, "messages": a.messages});
    report["rss_mib_start"] = mib(rss(a.pid));

    // ---- accounts and room ------------------------------------------------------------------
    let status: Value = http.get(format!("{url}/api/status")).send().await.unwrap().json().await.unwrap();
    if status["setup_needed"].as_bool() == Some(true) {
        let r = http
            .post(format!("{url}/api/setup"))
            .header("X-Ferrite-Request", "1")
            .json(&json!({"username": "admin", "display_name": "Admin", "password": "bench-password"}))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "setup: {}", r.status());
    }
    let admin = token_for(&url, "admin", "bench-password").await;
    let auth = |t: &str| format!("Bearer {t}");
    let names: Vec<String> = (0..a.users).map(|i| format!("bench{i:04}")).collect();
    for n in &names {
        let _ = http
            .post(format!("{url}/api/admin/users"))
            .header("Authorization", auth(&admin))
            .json(&json!({"username": n, "display_name": n, "password": "bench-password"}))
            .send()
            .await
            .unwrap();
    }
    let mut tokens = Vec::new();
    for n in &names {
        tokens.push(token_for(&url, n, "bench-password").await);
    }
    let room: Value = http
        .post(format!("{url}/api/talk/rooms"))
        .header("Authorization", auth(&admin))
        .json(&json!({"kind": "group", "name": "Bench", "invite": names}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let room_token = room["room"]["token"].as_str().expect("room token").to_string();
    let rss_after_setup = rss(a.pid);
    report["rss_mib_after_setup"] = mib(rss_after_setup);

    // ---- event sockets ----------------------------------------------------------------------
    let ws_base = url.replace("http://", "ws://");
    let total = a.users * a.conns;
    let start = Instant::now();
    let latencies: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::with_capacity(total * a.messages)));
    let received = Arc::new(AtomicU64::new(0));
    let sent_at: Arc<Mutex<HashMap<u64, Instant>>> = Arc::new(Mutex::new(HashMap::new()));
    let connect_started = Instant::now();
    let mut readers = Vec::with_capacity(total);
    for t in &tokens {
        for _ in 0..a.conns {
            let (mut ws, _) = tokio_tungstenite::connect_async(format!("{ws_base}/api/ws?token={t}"))
                .await
                .expect("ws connect");
            // Wait for the server's hello so the socket is registered before we post.
            loop {
                if let Some(Ok(Message::Text(m))) = ws.next().await {
                    if m.contains("\"hello\"") {
                        break;
                    }
                }
            }
            let (lat, rec, sent) = (latencies.clone(), received.clone(), sent_at.clone());
            readers.push(tokio::spawn(async move {
                let (_tx, mut rx) = ws.split();
                while let Some(Ok(m)) = rx.next().await {
                    let Message::Text(m) = m else { continue };
                    let now = Instant::now();
                    if !m.contains("\"talk.message\"") {
                        continue;
                    }
                    let v: Value = serde_json::from_str(&m).unwrap();
                    let Some(seq) = v["message"]["body"]
                        .as_str()
                        .and_then(|b| b.strip_prefix("bench "))
                        .and_then(|s| s.parse::<u64>().ok())
                    else {
                        continue;
                    };
                    if let Some(t0) = sent.lock().await.get(&seq).copied() {
                        lat.lock().await.push(now.duration_since(t0).as_secs_f64() * 1000.0);
                    }
                    rec.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
    }
    report["connect_seconds"] = json!((connect_started.elapsed().as_secs_f64() * 100.0).round() / 100.0);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let rss_with_connections = rss(a.pid);
    report["rss_mib_with_connections"] = mib(rss_with_connections);
    if let (Some(with), Some(before)) = (rss_with_connections, rss_after_setup) {
        let per_conn = with.saturating_sub(before) as f64 / total as f64;
        report["rss_kib_per_connection"] = json!((per_conn / 1024.0 * 10.0).round() / 10.0);
    }

    // ---- message fan-out --------------------------------------------------------------------
    let poster = tokens[0].clone();
    let post_started = Instant::now();
    for seq in 0..a.messages as u64 {
        sent_at.lock().await.insert(seq, Instant::now());
        let r = http
            .post(format!("{url}/api/talk/rooms/{room_token}/messages"))
            .header("Authorization", auth(&poster))
            .json(&json!({"body": format!("bench {seq}")}))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "post: {}", r.status());
        // ~50 messages/second: a very busy room, but not a flood.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let expected = (total * a.messages) as u64;
    let deadline = Instant::now() + Duration::from_secs(30);
    while received.load(Ordering::Relaxed) < expected && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let elapsed = post_started.elapsed().as_secs_f64();
    let mut lat = latencies.lock().await.clone();
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let got = received.load(Ordering::Relaxed);
    report["fanout"] = json!({
        "deliveries_expected": expected,
        "deliveries_received": got,
        "deliveries_per_second": (got as f64 / elapsed).round(),
        "latency_ms_p50": percentile(&lat, 0.50),
        "latency_ms_p95": percentile(&lat, 0.95),
        "latency_ms_p99": percentile(&lat, 0.99),
        "latency_ms_max": percentile(&lat, 1.0),
    });
    report["rss_mib_after_fanout"] = mib(rss(a.pid));
    for r in readers {
        r.abort();
    }

    // ---- HTTP request rate ------------------------------------------------------------------
    let concurrency = 32;
    let reqs = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));
    let until = Instant::now() + Duration::from_secs(a.http_secs);
    let mut workers = Vec::new();
    for i in 0..concurrency {
        let (http, url, t, room_token, reqs, errors) = (
            http.clone(),
            url.clone(),
            tokens[i % tokens.len()].clone(),
            room_token.clone(),
            reqs.clone(),
            errors.clone(),
        );
        workers.push(tokio::spawn(async move {
            while Instant::now() < until {
                let r = http
                    .get(format!("{url}/api/talk/rooms/{room_token}/messages?limit=50"))
                    .header("Authorization", format!("Bearer {t}"))
                    .send()
                    .await;
                match r {
                    Ok(r) if r.status().is_success() => {
                        let _ = r.bytes().await;
                        reqs.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }
    for w in workers {
        w.await.unwrap();
    }
    report["http_messages_page"] = json!({
        "concurrency": concurrency,
        "requests_per_second": (reqs.load(Ordering::Relaxed) as f64 / a.http_secs as f64).round(),
        "errors": errors.load(Ordering::Relaxed),
    });

    // ---- WebDAV throughput -------------------------------------------------------------------
    let size = 64 * 1024 * 1024;
    let body: Vec<u8> = (0..size).map(|i| (i * 7 % 251) as u8).collect();
    let dav = format!("{url}/remote.php/dav/files/admin/bench.bin");
    let t0 = Instant::now();
    let r = http.put(&dav).header("Authorization", auth(&admin)).body(body.clone()).send().await.unwrap();
    assert!(r.status().is_success(), "PUT: {}", r.status());
    let up = t0.elapsed().as_secs_f64();
    let t0 = Instant::now();
    let got = http
        .get(&dav)
        .header("Authorization", auth(&admin))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let down = t0.elapsed().as_secs_f64();
    assert_eq!(got.len(), size, "download size");
    // Same content again: deduplicated by content hash, so no new blob is stored.
    let t0 = Instant::now();
    let r = http
        .put(format!("{dav}.copy"))
        .header("Authorization", auth(&admin))
        .body(body)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let dedup = t0.elapsed().as_secs_f64();
    let mbs = |secs: f64| ((size as f64 / 1048576.0) / secs).round();
    report["webdav_64mib"] = json!({"upload_mib_s": mbs(up), "download_mib_s": mbs(down), "duplicate_upload_mib_s": mbs(dedup)});
    report["rss_mib_end"] = mib(rss(a.pid));
    report["total_seconds"] = json!((start.elapsed().as_secs_f64() * 10.0).round() / 10.0);
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}
