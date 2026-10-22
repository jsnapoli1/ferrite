//! Dual-era MCP client over stdio (child processes, e.g. the Craft apps' `<app>-cli mcp`)
//! or Streamable HTTP. It probes with `server/discover` and falls back to the legacy
//! `initialize` handshake exactly as the 2026-07-28 spec prescribes.

use crate::protocol::*;
use base64::Engine;
use dashmap::DashMap;
use serde_json::{Value, json};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, oneshot};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("timed out waiting for the server")]
    Timeout,
}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        ClientError::Transport(e.to_string())
    }
}

impl From<reqwest::Error> for ClientError {
    fn from(e: reqwest::Error) -> Self {
        ClientError::Transport(e.to_string())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Era {
    Modern(String),
    Legacy(String),
}

type Pending = Arc<DashMap<String, oneshot::Sender<Value>>>;

enum Transport {
    Stream {
        writer: Mutex<Box<dyn AsyncWrite + Send + Unpin>>,
        pending: Pending,
        child: Mutex<Option<tokio::process::Child>>,
    },
    Http {
        http: reqwest::Client,
        url: String,
        headers: Vec<(String, String)>,
        session: Mutex<Option<String>>,
    },
}

pub struct Client {
    transport: Transport,
    era: Era,
    next_id: AtomicU64,
    client_info: Implementation,
    pub server_info: Option<Implementation>,
    pub instructions: Option<String>,
    timeout: Duration,
    tools: Mutex<Vec<Tool>>,
}

fn client_info() -> Implementation {
    Implementation {
        name: "ferrite".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        title: Some("Ferrite".into()),
    }
}

/// Encodes a value for an MCP header, using the base64 sentinel when it isn't plain ASCII.
pub fn encode_header_value(v: &str) -> String {
    let plain = !v.is_empty()
        && v.bytes().all(|b| (0x20..=0x7e).contains(&b))
        && !v.starts_with(' ')
        && !v.ends_with(' ')
        && !(v.starts_with("=?base64?") && v.ends_with("?="));
    if plain {
        v.to_string()
    } else {
        format!("=?base64?{}?=", base64::engine::general_purpose::STANDARD.encode(v.as_bytes()))
    }
}

fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// Collects `x-mcp-header` annotations reachable through `properties` chains.
/// Returns None when an annotation is invalid (the tool must then be rejected).
fn header_params(schema: &Value) -> Option<Vec<(Vec<String>, String)>> {
    fn walk(schema: &Value, path: &mut Vec<String>, out: &mut Vec<(Vec<String>, String)>) -> bool {
        let Some(props) = schema.get("properties").and_then(Value::as_object) else {
            return true;
        };
        for (k, v) in props {
            path.push(k.clone());
            if let Some(h) = v.get("x-mcp-header") {
                let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
                match h.as_str() {
                    Some(name) if is_token(name) && matches!(ty, "string" | "integer" | "boolean") => {
                        if out.iter().any(|(_, n)| n.eq_ignore_ascii_case(name)) {
                            return false;
                        }
                        out.push((path.clone(), name.to_string()));
                    }
                    _ => return false,
                }
            }
            if !walk(v, path, out) {
                return false;
            }
            path.pop();
        }
        true
    }
    let mut out = Vec::new();
    if walk(schema, &mut Vec::new(), &mut out) { Some(out) } else { None }
}

impl Client {
    /// Starts `command` and connects to its stdio MCP server.
    pub async fn stdio(command: &str, args: &[String], cwd: Option<&std::path::Path>, env: &[(String, String)]) -> Result<Self, ClientError> {
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(d) = cwd {
            cmd.current_dir(d);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().map_err(|e| ClientError::Transport(format!("cannot start {command}: {e}")))?;
        let stdout = child.stdout.take().ok_or_else(|| ClientError::Transport("no stdout".into()))?;
        let stdin = child.stdin.take().ok_or_else(|| ClientError::Transport("no stdin".into()))?;
        if let Some(stderr) = child.stderr.take() {
            let name = command.to_string();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    tracing::debug!(server = %name, "{l}");
                }
            });
        }
        Self::from_stream(stdout, stdin, Some(child)).await
    }

    /// Connects over any newline-delimited JSON-RPC byte stream.
    pub async fn from_stream<R, W>(reader: R, writer: W, child: Option<tokio::process::Child>) -> Result<Self, ClientError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let pending: Pending = Arc::new(DashMap::new());
        {
            let pending = pending.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(reader).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if msg.get("method").is_some() {
                        // Server notifications/requests: nothing to do for a tool client.
                        continue;
                    }
                    if let Some(id) = msg.get("id") {
                        if let Some((_, tx)) = pending.remove(&id.to_string()) {
                            let _ = tx.send(msg);
                        }
                    }
                }
                // Stream closed: fail everything still waiting.
                pending.clear();
            });
        }
        let transport = Transport::Stream {
            writer: Mutex::new(Box::new(writer)),
            pending,
            child: Mutex::new(child),
        };
        let mut client = Client {
            transport,
            era: Era::Modern(MODERN_VERSION.into()),
            next_id: AtomicU64::new(1),
            client_info: client_info(),
            server_info: None,
            instructions: None,
            timeout: Duration::from_secs(60),
            tools: Mutex::new(Vec::new()),
        };
        client.negotiate(Duration::from_secs(5)).await?;
        Ok(client)
    }

    /// Connects to a Streamable HTTP MCP endpoint. `headers` are sent on every request
    /// (e.g. `Authorization`).
    pub async fn http(url: &str, headers: Vec<(String, String)>) -> Result<Self, ClientError> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(120)).build()?;
        let transport = Transport::Http {
            http,
            url: url.to_string(),
            headers,
            session: Mutex::new(None),
        };
        let mut client = Client {
            transport,
            era: Era::Modern(MODERN_VERSION.into()),
            next_id: AtomicU64::new(1),
            client_info: client_info(),
            server_info: None,
            instructions: None,
            timeout: Duration::from_secs(120),
            tools: Mutex::new(Vec::new()),
        };
        client.negotiate(Duration::from_secs(15)).await?;
        Ok(client)
    }

    pub fn era(&self) -> &Era {
        &self.era
    }

    async fn negotiate(&mut self, probe_timeout: Duration) -> Result<(), ClientError> {
        let saved = self.timeout;
        self.timeout = probe_timeout;
        let probe = self.request("server/discover", json!({})).await;
        self.timeout = saved;
        match probe {
            Ok(result) => {
                self.server_info = result
                    .get("_meta")
                    .and_then(|m| m.get(META_SERVER_INFO))
                    .and_then(|v| serde_json::from_value(v.clone()).ok());
                self.instructions = result.get("instructions").and_then(Value::as_str).map(String::from);
                return Ok(());
            }
            Err(ClientError::Rpc(e)) if is_modern_error(e.code) => {
                let supported: Vec<String> = e
                    .data
                    .as_ref()
                    .and_then(|d| d.get("supported"))
                    .and_then(|s| serde_json::from_value(s.clone()).ok())
                    .unwrap_or_default();
                if supported.iter().any(|v| v == MODERN_VERSION) {
                    return Ok(());
                }
                if !supported.iter().any(|v| LEGACY_VERSIONS.contains(&v.as_str())) {
                    return Err(ClientError::Rpc(e));
                }
            }
            Err(ClientError::Transport(t)) if matches!(self.transport, Transport::Stream { .. }) && t.contains("closed") => {
                return Err(ClientError::Transport(t));
            }
            Err(_) => {}
        }
        // Legacy server: initialize handshake.
        self.era = Era::Legacy(LEGACY_VERSIONS[0].into());
        let init = self
            .request(
                "initialize",
                json!({"protocolVersion": LEGACY_VERSIONS[0], "capabilities": {}, "clientInfo": self.client_info}),
            )
            .await?;
        let version = init.get("protocolVersion").and_then(Value::as_str).unwrap_or(LEGACY_VERSIONS[1]).to_string();
        self.era = Era::Legacy(version);
        self.server_info = init.get("serverInfo").and_then(|v| serde_json::from_value(v.clone()).ok());
        self.instructions = init.get("instructions").and_then(Value::as_str).map(String::from);
        self.notify("notifications/initialized", json!({})).await?;
        Ok(())
    }

    fn with_meta(&self, method: &str, mut params: Value) -> Value {
        if let (Era::Modern(v), false) = (&self.era, method == "initialize") {
            if !params.is_object() {
                params = json!({});
            }
            let meta = params.as_object_mut().map(|o| o.entry("_meta").or_insert_with(|| json!({})));
            if let Some(m) = meta {
                m[META_PROTOCOL_VERSION] = json!(v);
                m[META_CLIENT_INFO] = json!(self.client_info);
                m[META_CLIENT_CAPABILITIES] = json!({});
            }
        }
        params
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), ClientError> {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        match &self.transport {
            Transport::Stream { writer, .. } => {
                let mut line = serde_json::to_vec(&msg).map_err(|e| ClientError::Transport(e.to_string()))?;
                line.push(b'\n');
                let mut w = writer.lock().await;
                w.write_all(&line).await?;
                w.flush().await?;
                Ok(())
            }
            Transport::Http { http, url, headers, session } => {
                let mut req = http.post(url).header("Accept", "application/json, text/event-stream").json(&msg);
                for (k, v) in headers {
                    req = req.header(k, v);
                }
                if let Era::Legacy(v) = &self.era {
                    req = req.header("MCP-Protocol-Version", v);
                }
                if let Some(s) = session.lock().await.as_ref() {
                    req = req.header("Mcp-Session-Id", s);
                }
                let resp = req.send().await?;
                if !resp.status().is_success() {
                    return Err(ClientError::Transport(format!("notification rejected: HTTP {}", resp.status())));
                }
                Ok(())
            }
        }
    }

    /// Sends a request and returns its `result`, or the JSON-RPC error.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, ClientError> {
        self.request_with_headers(method, params, Vec::new()).await
    }

    async fn request_with_headers(&self, method: &str, params: Value, extra_headers: Vec<(String, String)>) -> Result<Value, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let params = self.with_meta(method, params);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let reply = match &self.transport {
            Transport::Stream { writer, pending, .. } => {
                let (tx, rx) = oneshot::channel();
                pending.insert(json!(id).to_string(), tx);
                let mut line = serde_json::to_vec(&msg).map_err(|e| ClientError::Transport(e.to_string()))?;
                line.push(b'\n');
                {
                    let mut w = writer.lock().await;
                    w.write_all(&line).await?;
                    w.flush().await?;
                }
                match tokio::time::timeout(self.timeout, rx).await {
                    Ok(Ok(v)) => v,
                    Ok(Err(_)) => {
                        return Err(ClientError::Transport("server closed the connection".into()));
                    }
                    Err(_) => {
                        pending.remove(&json!(id).to_string());
                        return Err(ClientError::Timeout);
                    }
                }
            }
            Transport::Http { http, url, headers, session } => {
                let mut req = http
                    .post(url)
                    .header("Accept", "application/json, text/event-stream")
                    .header("Content-Type", "application/json");
                for (k, v) in headers.iter().chain(extra_headers.iter()) {
                    req = req.header(k, v);
                }
                match &self.era {
                    Era::Modern(v) => {
                        req = req.header("MCP-Protocol-Version", v).header("Mcp-Method", method);
                        let name = match method {
                            "tools/call" | "prompts/get" => params.get("name").and_then(Value::as_str),
                            "resources/read" => params.get("uri").and_then(Value::as_str),
                            _ => None,
                        };
                        if let Some(n) = name {
                            req = req.header("Mcp-Name", encode_header_value(n));
                        }
                    }
                    Era::Legacy(v) if method != "initialize" => {
                        req = req.header("MCP-Protocol-Version", v);
                    }
                    Era::Legacy(_) => {}
                }
                if let Some(s) = session.lock().await.as_ref() {
                    req = req.header("Mcp-Session-Id", s);
                }
                let resp = tokio::time::timeout(self.timeout, req.json(&msg).send())
                    .await
                    .map_err(|_| ClientError::Timeout)??;
                if let Some(s) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
                    *session.lock().await = Some(s.to_string());
                }
                let status = resp.status();
                let is_sse = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|c| c.starts_with("text/event-stream"));
                let text = resp.text().await?;
                let parsed = if is_sse {
                    parse_sse_response(&text, id)
                } else {
                    serde_json::from_str::<Value>(&text).ok()
                };
                match parsed {
                    Some(v) => v,
                    None => {
                        return Err(ClientError::Transport(format!("HTTP {status}: unexpected response body")));
                    }
                }
            }
        };
        if let Some(err) = reply.get("error") {
            let e: RpcError = serde_json::from_value(err.clone()).unwrap_or_else(|_| RpcError::internal(err.to_string()));
            return Err(ClientError::Rpc(e));
        }
        let result = reply.get("result").cloned().unwrap_or(Value::Null);
        match result.get("resultType").and_then(Value::as_str) {
            None | Some("complete") => Ok(result),
            Some(other) => Err(ClientError::Transport(format!(
                "server requires interaction this client does not support (resultType {other})"
            ))),
        }
    }

    /// All tools (follows pagination). Tools with invalid `x-mcp-header` annotations are
    /// dropped, as the spec requires of HTTP clients.
    pub async fn list_tools(&self) -> Result<Vec<Tool>, ClientError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..100 {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let r = self.request("tools/list", params).await?;
            for t in r.get("tools").and_then(Value::as_array).cloned().unwrap_or_default() {
                match serde_json::from_value::<Tool>(t) {
                    Ok(tool) if header_params(&tool.input_schema).is_some() => out.push(tool),
                    Ok(tool) => {
                        tracing::warn!(tool = %tool.name, "rejecting tool with invalid x-mcp-header annotation")
                    }
                    Err(e) => tracing::warn!("skipping malformed tool definition: {e}"),
                }
            }
            cursor = r.get("nextCursor").and_then(Value::as_str).map(String::from);
            if cursor.is_none() {
                break;
            }
        }
        *self.tools.lock().await = out.clone();
        Ok(out)
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolResult, ClientError> {
        let mut extra = Vec::new();
        if let (Transport::Http { .. }, Era::Modern(_)) = (&self.transport, &self.era) {
            let tools = self.tools.lock().await;
            if let Some(t) = tools.iter().find(|t| t.name == name) {
                for (path, header) in header_params(&t.input_schema).unwrap_or_default() {
                    let mut v = &arguments;
                    for p in &path {
                        v = &v[p.as_str()];
                    }
                    let s = match v {
                        Value::String(s) => Some(s.clone()),
                        Value::Bool(b) => Some(b.to_string()),
                        Value::Number(n) if n.is_i64() || n.is_u64() => Some(n.to_string()),
                        _ => None,
                    };
                    if let Some(s) = s {
                        extra.push((format!("Mcp-Param-{header}"), encode_header_value(&s)));
                    }
                }
            }
        }
        let r = self
            .request_with_headers("tools/call", json!({"name": name, "arguments": arguments}), extra)
            .await?;
        serde_json::from_value(r).map_err(|e| ClientError::Transport(format!("bad tool result: {e}")))
    }

    pub async fn list_resources(&self) -> Result<Vec<Resource>, ClientError> {
        let r = self.request("resources/list", json!({})).await?;
        Ok(serde_json::from_value(r.get("resources").cloned().unwrap_or(json!([]))).unwrap_or_default())
    }

    pub async fn read_resource(&self, uri: &str) -> Result<Vec<ResourceContents>, ClientError> {
        let r = self.request("resources/read", json!({"uri": uri})).await?;
        Ok(serde_json::from_value(r.get("contents").cloned().unwrap_or(json!([]))).unwrap_or_default())
    }

    /// Closes the connection (stdio: closes stdin and waits briefly for the process to exit).
    pub async fn close(self) {
        if let Transport::Stream { writer, child, .. } = self.transport {
            let mut w = writer.into_inner();
            let _ = w.shutdown().await;
            drop(w);
            if let Some(mut c) = child.into_inner() {
                if tokio::time::timeout(Duration::from_secs(3), c.wait()).await.is_err() {
                    let _ = c.kill().await;
                }
            }
        }
    }
}

/// Extracts the JSON-RPC response with `id` from an SSE body.
fn parse_sse_response(body: &str, id: u64) -> Option<Value> {
    let mut data = String::new();
    let mut found = None;
    let mut flush = |data: &mut String| {
        if !data.is_empty() {
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                if v.get("id") == Some(&json!(id)) && (v.get("result").is_some() || v.get("error").is_some()) {
                    found = Some(v);
                }
            }
            data.clear();
        }
    };
    for line in body.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            flush(&mut data);
        } else if let Some(d) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(d.strip_prefix(' ').unwrap_or(d));
        }
    }
    flush(&mut data);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_values_are_encoded() {
        assert_eq!(encode_header_value("us-west1"), "us-west1");
        assert_eq!(encode_header_value("Hello, 世界"), "=?base64?SGVsbG8sIOS4lueVjA==?=");
        assert_eq!(encode_header_value(" padded "), "=?base64?IHBhZGRlZCA=?=");
        assert_eq!(encode_header_value("=?base64?literal?="), "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?=");
        assert_eq!(
            crate::server::decode_header_value("=?base64?SGVsbG8sIOS4lueVjA==?=").as_deref(),
            Some("Hello, 世界")
        );
    }

    #[test]
    fn x_mcp_header_validation() {
        let ok = json!({"type": "object", "properties": {"region": {"type": "string", "x-mcp-header": "Region"}, "nested": {"type": "object", "properties": {"n": {"type": "integer", "x-mcp-header": "N"}}}}});
        assert_eq!(header_params(&ok).unwrap().len(), 2);
        let dup = json!({"properties": {"a": {"type": "string", "x-mcp-header": "X"}, "b": {"type": "string", "x-mcp-header": "x"}}});
        assert!(header_params(&dup).is_none());
        let number = json!({"properties": {"a": {"type": "number", "x-mcp-header": "A"}}});
        assert!(header_params(&number).is_none());
    }

    #[test]
    fn sse_parsing() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n: keepalive\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":3,\n";
        assert!(parse_sse_response(body, 3).is_none(), "incomplete JSON is not a response");
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{}}\n\n";
        assert!(parse_sse_response(body, 3).is_some());
    }
}
