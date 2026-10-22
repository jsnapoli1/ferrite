//! Ferrite server: HTTP API, WebSockets, WebDAV, public links, MCP endpoint and web app.

pub mod ai;
pub mod api;
pub mod auth;
pub mod craft_bridge;
pub mod error;
pub mod external;
pub mod groupdav;
pub mod mcp_http;
pub mod public;
pub mod tools;
pub mod web;
pub mod webdav;
pub mod ws;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{any, get, post};
use ferrite_core::Core;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct AppState {
    pub core: Core,
    pub tools: Arc<tools::FerriteTools>,
    pub ai: Arc<ai::AiService>,
    pub mcp: Arc<ferrite_mcp::Server<tools::McpHandler>>,
    pub throttle: Arc<auth::Throttle>,
    /// Built-in SFU for calls; None when disabled or its UDP port is unavailable (calls then
    /// fall back to peer-to-peer).
    pub sfu: Option<Arc<ferrite_sfu::Sfu>>,
}

impl AppState {
    pub fn new(core: Core) -> Self {
        let craft = craft_bridge::CraftBridge::new(core.clone());
        let external = external::ExternalMcp::new(core.clone());
        let tools = Arc::new(tools::FerriteTools {
            core: core.clone(),
            craft,
            external,
        });
        let ai = ai::AiService::new(core.clone(), tools.clone());
        let mcp = Arc::new(ferrite_mcp::Server::new(tools::McpHandler { tools: tools.clone() }));
        let sfu = start_sfu(&core.config.sfu);
        AppState {
            core,
            tools,
            ai,
            mcp,
            throttle: Arc::new(auth::Throttle::default()),
            sfu,
        }
    }

    /// Starts background work: the Talk bot and periodic maintenance.
    pub fn start_background(&self) {
        self.ai.spawn_talk_bot();
        let core = self.core.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3600));
            loop {
                tick.tick().await;
                match core.expire_trash().await {
                    Ok(n) if n > 0 => tracing::info!(purged = n, "expired trash"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "trash expiry failed"),
                }
                let _ = sqlx::query("DELETE FROM tokens WHERE expires_at IS NOT NULL AND expires_at < ?")
                    .bind(ferrite_core::util::now())
                    .execute(&core.db)
                    .await;
            }
        });
    }
}

pub fn router(state: AppState) -> Router {
    let webdav = any(webdav::handle);
    Router::new()
        .merge(api::router())
        .merge(public::router())
        .route("/api/ws", get(ws::events))
        .route("/api/collab/{id}", get(ws::collab))
        .route("/mcp", post(mcp_http::post).get(mcp_http::other).delete(mcp_http::other))
        .route("/status.php", get(webdav::status_php))
        .route("/remote.php/dav/files/{user}", webdav.clone())
        .route("/remote.php/dav/files/{user}/", webdav.clone())
        .route("/remote.php/dav/files/{user}/{*path}", webdav.clone())
        .route("/remote.php/webdav", webdav.clone())
        .route("/remote.php/webdav/", webdav.clone())
        .route("/remote.php/webdav/{*path}", webdav.clone())
        .route("/dav/files/{user}", webdav.clone())
        .route("/dav/files/{user}/", webdav.clone())
        .route("/dav/files/{user}/{*path}", webdav)
        .route("/.well-known/caldav", any(groupdav::well_known))
        .route("/.well-known/carddav", any(groupdav::well_known))
        .route("/remote.php/dav", any(groupdav::handle))
        .route("/remote.php/dav/", any(groupdav::handle))
        .route("/remote.php/dav/principals/{*rest}", any(groupdav::handle))
        .route("/remote.php/dav/calendars/{*rest}", any(groupdav::handle))
        .route("/remote.php/dav/addressbooks/{*rest}", any(groupdav::handle))
        .fallback(web::serve)
        .layer(DefaultBodyLimit::disable())
        .layer(tower_http::compression::CompressionLayer::new())
        .with_state(state)
}

fn start_sfu(cfg: &ferrite_core::config::SfuSettings) -> Option<Arc<ferrite_sfu::Sfu>> {
    if !cfg.enabled {
        return None;
    }
    let bind = match cfg.udp_bind.parse() {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(addr = %cfg.udp_bind, error = %e, "invalid sfu.udp_bind; calls will be peer-to-peer");
            return None;
        }
    };
    let advertise = cfg.advertise.iter().filter_map(|a| a.parse().ok()).collect();
    match ferrite_sfu::Sfu::start(ferrite_sfu::SfuConfig { bind, advertise }) {
        Ok(s) => Some(Arc::new(s)),
        Err(e) => {
            tracing::warn!(addr = %cfg.udp_bind, error = %e, "SFU could not start; calls will be peer-to-peer");
            None
        }
    }
}

/// Resident memory of this process (Linux), for the admin dashboard and benchmarks.
pub fn rss_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1).and_then(|p| p.parse::<u64>().ok()))
        .map(|pages| pages * 4096)
        .unwrap_or(0)
}
