//! External MCP servers the assistant may use. Admins can register servers for everyone
//! (including stdio servers, which run commands on this machine); users can add their own
//! Streamable HTTP servers. Connections are made lazily and reused.

use dashmap::DashMap;
use ferrite_core::util::{new_id, now};
use ferrite_core::{Core, Error, Result};
use ferrite_mcp::{Client, Tool, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct McpServerRow {
    pub id: String,
    pub owner_id: Option<String>,
    pub name: String,
    pub transport: String,
    pub url: Option<String>,
    pub command: Option<String>,
    pub args: String,
    #[serde(skip_serializing)]
    pub headers: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewMcpServer {
    pub name: String,
    pub transport: String,
    pub url: Option<String>,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Admin only: make the server available to everyone.
    #[serde(default)]
    pub global: bool,
}

pub struct ExternalMcp {
    core: Core,
    clients: DashMap<String, Arc<Client>>,
}

/// Prefix for external tools as the assistant sees them: `ext_<server>__<tool>`.
pub fn tool_prefix(server_name: &str) -> String {
    let s: String = server_name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("ext_{}__", s.trim_matches('_'))
}

impl ExternalMcp {
    pub fn new(core: Core) -> Arc<Self> {
        Arc::new(ExternalMcp { core, clients: DashMap::new() })
    }

    pub async fn list(&self, user_id: &str) -> Result<Vec<McpServerRow>> {
        Ok(sqlx::query_as(
            "SELECT id, owner_id, name, transport, url, command, args, headers, enabled FROM mcp_servers WHERE owner_id IS NULL OR owner_id = ? ORDER BY name",
        )
        .bind(user_id)
        .fetch_all(&self.core.db)
        .await?)
    }

    pub async fn add(&self, user_id: &str, is_admin: bool, n: NewMcpServer) -> Result<McpServerRow> {
        let name = n.name.trim();
        if name.is_empty() || name.len() > 64 {
            return Err(Error::bad("name must be 1-64 characters"));
        }
        match n.transport.as_str() {
            "http" => {
                let url = n.url.as_deref().unwrap_or("");
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return Err(Error::bad("an http MCP server needs an http(s) URL"));
                }
            }
            "stdio" => {
                // Stdio servers execute programs on this machine: administrators only.
                if !is_admin {
                    return Err(Error::Forbidden);
                }
                if n.command.as_deref().unwrap_or("").is_empty() {
                    return Err(Error::bad("a stdio MCP server needs a command"));
                }
            }
            _ => return Err(Error::bad("transport must be http or stdio")),
        }
        if n.global && !is_admin {
            return Err(Error::Forbidden);
        }
        let id = new_id();
        sqlx::query("INSERT INTO mcp_servers (id, owner_id, name, transport, url, command, args, headers, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(&id)
            .bind(if n.global || n.transport == "stdio" { None } else { Some(user_id) })
            .bind(name)
            .bind(&n.transport)
            .bind(&n.url)
            .bind(&n.command)
            .bind(serde_json::to_string(&n.args).unwrap_or_else(|_| "[]".into()))
            .bind(serde_json::to_string(&n.headers).unwrap_or_else(|_| "{}".into()))
            .bind(now())
            .execute(&self.core.db)
            .await?;
        self.list(user_id).await?.into_iter().find(|s| s.id == id).ok_or(Error::NotFound)
    }

    pub async fn remove(&self, user_id: &str, is_admin: bool, id: &str) -> Result<()> {
        let row = self.list(user_id).await?.into_iter().find(|s| s.id == id).ok_or(Error::NotFound)?;
        if row.owner_id.is_none() && !is_admin {
            return Err(Error::Forbidden);
        }
        sqlx::query("DELETE FROM mcp_servers WHERE id = ?").bind(id).execute(&self.core.db).await?;
        if let Some((_, c)) = self.clients.remove(id) {
            if let Ok(c) = Arc::try_unwrap(c) {
                c.close().await;
            }
        }
        Ok(())
    }

    async fn client(&self, row: &McpServerRow) -> Result<Arc<Client>> {
        if let Some(c) = self.clients.get(&row.id) {
            return Ok(c.clone());
        }
        let client = match row.transport.as_str() {
            "http" => {
                let headers: BTreeMap<String, String> = serde_json::from_str(&row.headers).unwrap_or_default();
                Client::http(row.url.as_deref().unwrap_or(""), headers.into_iter().collect()).await
            }
            _ => {
                let args: Vec<String> = serde_json::from_str(&row.args).unwrap_or_default();
                Client::stdio(row.command.as_deref().unwrap_or(""), &args, None, &[]).await
            }
        }
        .map_err(|e| Error::Unavailable(format!("MCP server {}: {e}", row.name)))?;
        let c = Arc::new(client);
        self.clients.insert(row.id.clone(), c.clone());
        Ok(c)
    }

    /// Tools of all enabled servers visible to the user, prefixed per server.
    pub async fn tools(&self, user_id: &str) -> Vec<Tool> {
        let Ok(rows) = self.list(user_id).await else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for row in rows.into_iter().filter(|r| r.enabled) {
            let tools = match self.client(&row).await {
                Ok(c) => match c.list_tools().await {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(server = %row.name, error = %e, "listing external MCP tools failed");
                        self.clients.remove(&row.id);
                        continue;
                    }
                },
                Err(e) => {
                    tracing::warn!(server = %row.name, error = %e, "connecting to external MCP server failed");
                    continue;
                }
            };
            let prefix = tool_prefix(&row.name);
            for mut t in tools {
                t.name = format!("{prefix}{}", t.name);
                t.description = format!("[{}] {}", row.name, t.description);
                out.push(t);
            }
        }
        out
    }

    pub async fn call(&self, user_id: &str, prefixed: &str, args: Value) -> Option<ToolResult> {
        let rows = self.list(user_id).await.ok()?;
        let row = rows.into_iter().find(|r| r.enabled && prefixed.starts_with(&tool_prefix(&r.name)))?;
        let tool = &prefixed[tool_prefix(&row.name).len()..];
        let result = match self.client(&row).await {
            Ok(c) => match c.call_tool(tool, args).await {
                Ok(r) => r,
                Err(e) => {
                    self.clients.remove(&row.id);
                    ToolResult::error(format!("{}: {e}", row.name))
                }
            },
            Err(e) => ToolResult::error(e.to_string()),
        };
        Some(result)
    }

    /// Connects and lists tools (for the settings page "test" button).
    pub async fn probe(&self, user_id: &str, id: &str) -> Result<Vec<Tool>> {
        let row = self.list(user_id).await?.into_iter().find(|s| s.id == id).ok_or(Error::NotFound)?;
        self.clients.remove(&row.id);
        let c = self.client(&row).await?;
        c.list_tools().await.map_err(|e| Error::Unavailable(e.to_string()))
    }
}
