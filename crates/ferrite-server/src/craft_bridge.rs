//! Drives ArtCraft Crafting Apps headlessly through their built-in MCP servers.
//!
//! Opening a Craft document stages a private copy in a work folder, locks the original in
//! Ferrite (so nobody overwrites it meanwhile), and starts `<app>-cli mcp` confined to that
//! folder. The assistant (or any MCP client of Ferrite) then uses the app's own tools. On
//! close, every changed or new file in the folder is written back next to the original as a
//! new version, and the lock is released.

use dashmap::DashMap;
use ferrite_core::craft::{self, CraftApp};
use ferrite_core::files::{WriteOpts, perm};
use ferrite_core::{Core, Error, Result};
use ferrite_mcp::{Client, Tool, ToolResult};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const LOCK_SECS: i64 = 2 * 3600;

pub struct CraftSession {
    pub id: String,
    pub user_id: String,
    pub node_id: String,
    pub parent_id: String,
    pub app: &'static CraftApp,
    pub dir: PathBuf,
    pub file_name: String,
    client: Mutex<Option<Client>>,
    original: HashMap<String, String>,
    lock_token: String,
    last_used: std::sync::Mutex<Instant>,
}

impl CraftSession {
    fn touch(&self) {
        if let Ok(mut t) = self.last_used.lock() {
            *t = Instant::now();
        }
    }

    fn idle_for(&self) -> Duration {
        self.last_used.lock().map(|t| t.elapsed()).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AppAvailability {
    #[serde(flatten)]
    pub app: CraftApp,
    pub installed: bool,
    pub command: Option<String>,
}

pub struct CraftBridge {
    core: Core,
    sessions: DashMap<String, Arc<CraftSession>>,
}

fn hash_file(p: &Path) -> Option<String> {
    std::fs::read(p).ok().map(|b| blake3::hash(&b).to_hex().to_string())
}

fn on_path(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(bin)).find(|p| p.is_file())
}

impl CraftBridge {
    pub fn new(core: Core) -> Arc<Self> {
        let bridge = Arc::new(CraftBridge {
            core,
            sessions: DashMap::new(),
        });
        let weak = Arc::downgrade(&bridge);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                let Some(b) = weak.upgrade() else { break };
                let idle: Vec<(String, String)> = b
                    .sessions
                    .iter()
                    .filter(|s| s.idle_for() > IDLE_TIMEOUT)
                    .map(|s| (s.id.clone(), s.user_id.clone()))
                    .collect();
                for (id, user) in idle {
                    if let Err(e) = b.close(&user, &id, true).await {
                        tracing::warn!(session = %id, error = %e, "closing idle Craft session failed");
                    }
                }
            }
        });
        bridge
    }

    /// The command (and leading args) that runs an app's CLI on this server, if installed.
    fn command(&self, app: &CraftApp) -> Option<(String, Vec<String>)> {
        let cfg = &self.core.config.craft;
        if let Some(c) = cfg.commands.get(app.id) {
            return Some((c.command.clone(), c.args.clone()));
        }
        if let Some(dir) = &cfg.bin_dir {
            let p = dir.join(app.cli);
            if p.is_file() {
                return Some((p.to_string_lossy().into_owned(), Vec::new()));
            }
        }
        on_path(app.cli).map(|p| (p.to_string_lossy().into_owned(), Vec::new()))
    }

    pub fn availability(&self) -> Vec<AppAvailability> {
        craft::APPS
            .iter()
            .map(|a| {
                let cmd = self.command(a);
                AppAvailability {
                    app: a.clone(),
                    installed: cmd.is_some(),
                    command: cmd.map(|c| c.0),
                }
            })
            .collect()
    }

    pub async fn open(&self, user_id: &str, node_id: &str, app_id: Option<&str>) -> Result<(Arc<CraftSession>, Vec<Tool>)> {
        let (node, _) = self.core.node_for(user_id, node_id, perm::UPDATE).await?;
        let app = match app_id {
            Some(id) => craft::app(id).ok_or_else(|| Error::bad(format!("unknown Craft app {id}")))?,
            None => craft::app_for_name(&node.name).ok_or_else(|| Error::bad(format!("no Craft app opens {}", node.name)))?,
        };
        let (cmd, mut args) = self
            .command(app)
            .ok_or_else(|| Error::Unavailable(format!("{} is not installed on this server ({} not found)", app.name, app.cli)))?;
        let parent_id = node.parent_id.clone().ok_or(Error::NotFound)?;
        let lock = self.core.lock(user_id, &node.id, app.id, LOCK_SECS).await?;
        let id = ferrite_core::util::short_token(12);
        let dir = self.core.config.data_dir.join("craft-work").join(&id);
        tokio::fs::create_dir_all(&dir).await?;
        let (_, data) = self.core.read_file(user_id, &node.id).await?;
        let file_path = dir.join(&node.name);
        tokio::fs::write(&file_path, &data).await?;
        let mut original = HashMap::new();
        original.insert(node.name.clone(), blake3::hash(&data).to_hex().to_string());
        let dir_str = dir.to_string_lossy().into_owned();
        args.extend(app.mcp_args(&dir_str, &node.name));
        let client = match Client::stdio(&cmd, &args, Some(&dir), &[]).await {
            Ok(c) => c,
            Err(e) => {
                let _ = self.core.unlock(user_id, &node.id).await;
                let _ = tokio::fs::remove_dir_all(&dir).await;
                return Err(Error::Unavailable(format!("could not start {}: {e}", app.name)));
            }
        };
        let tools = client
            .list_tools()
            .await
            .map_err(|e| Error::Unavailable(format!("{} did not list its tools: {e}", app.name)))?;
        let session = Arc::new(CraftSession {
            id: id.clone(),
            user_id: user_id.to_string(),
            node_id: node.id.clone(),
            parent_id,
            app,
            dir,
            file_name: node.name.clone(),
            client: Mutex::new(Some(client)),
            original,
            lock_token: lock.token,
            last_used: std::sync::Mutex::new(Instant::now()),
        });
        self.sessions.insert(id, session.clone());
        Ok((session, tools))
    }

    fn session(&self, user_id: &str, id: &str) -> Result<Arc<CraftSession>> {
        let s = self
            .sessions
            .get(id)
            .map(|s| s.clone())
            .ok_or_else(|| Error::bad("unknown or expired Craft session; open the document again"))?;
        if s.user_id != user_id {
            return Err(Error::NotFound);
        }
        s.touch();
        Ok(s)
    }

    pub async fn call(&self, user_id: &str, id: &str, tool: &str, arguments: Value) -> Result<ToolResult> {
        let s = self.session(user_id, id)?;
        // Keep the lock alive while the session is in use.
        let _ = self.core.lock(user_id, &s.node_id, s.app.id, LOCK_SECS).await;
        let guard = s.client.lock().await;
        let client = guard.as_ref().ok_or_else(|| Error::bad("session is closed"))?;
        client.call_tool(tool, arguments).await.map_err(|e| Error::bad(format!("{}: {e}", s.app.name)))
    }

    /// Ends a session; with `save`, changed and new files are written back as new versions.
    pub async fn close(&self, user_id: &str, id: &str, save: bool) -> Result<Vec<String>> {
        let s = self.session(user_id, id)?;
        self.sessions.remove(id);
        if let Some(c) = s.client.lock().await.take() {
            c.close().await;
        }
        let mut written = Vec::new();
        if save {
            let mut entries = tokio::fs::read_dir(&s.dir).await?;
            while let Some(e) = entries.next_entry().await? {
                let p = e.path();
                if !p.is_file() {
                    continue;
                }
                let name = e.file_name().to_string_lossy().into_owned();
                let Some(h) = hash_file(&p) else { continue };
                if s.original.get(&name) == Some(&h) {
                    continue;
                }
                if ferrite_core::util::validate_name(&name).is_err() {
                    continue;
                }
                let data = tokio::fs::read(&p).await?;
                let opts = WriteOpts {
                    lock_token: Some(s.lock_token.clone()),
                    ..WriteOpts::user()
                };
                match self.core.write_bytes(user_id, &s.parent_id, &name, &data, opts).await {
                    Ok(_) => written.push(name),
                    Err(e) => tracing::warn!(file = %name, error = %e, "Craft write-back failed"),
                }
            }
        }
        let _ = self.core.unlock(user_id, &s.node_id).await;
        let _ = tokio::fs::remove_dir_all(&s.dir).await;
        Ok(written)
    }

    pub fn describe(s: &CraftSession, tools: &[Tool]) -> Value {
        json!({
            "session": s.id,
            "app": s.app.name,
            "document": s.file_name,
            "document_path": s.dir.join(&s.file_name).to_string_lossy(),
            "work_folder": s.dir.to_string_lossy(),
            "note": "Call craft_call with this session and one of the tools below. Files the app saves in the work folder are written back to Ferrite (as new versions) when you call craft_close.",
            "tools": tools.iter().map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.input_schema})).collect::<Vec<_>>(),
        })
    }
}
