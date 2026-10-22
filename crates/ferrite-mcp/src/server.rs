//! Transport-independent MCP server dispatch.
//!
//! Serves modern clients (2026-07-28: per-request `_meta`, stateless) and legacy clients
//! (`initialize` handshake, 2024-11-05 … 2025-11-25) from the same handler, as the spec's
//! "dual-era server". Transports (HTTP in `ferrite-server`, stdio here) only move bytes and
//! report which legacy version, if any, the connection negotiated.

use crate::protocol::*;
use serde_json::{Value, json};
use std::future::Future;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

/// Application side of an MCP server. `Ctx` is whatever identifies the caller (for Ferrite,
/// the authenticated user), resolved by the transport from per-request credentials.
pub trait Handler: Send + Sync + 'static {
    type Ctx: Send + Sync;

    fn server_info(&self) -> Implementation;
    fn instructions(&self) -> Option<String> {
        None
    }
    fn list_tools(&self, ctx: &Self::Ctx) -> impl Future<Output = Vec<Tool>> + Send;
    fn call_tool(&self, ctx: &Self::Ctx, name: &str, args: Value) -> impl Future<Output = Result<ToolResult, RpcError>> + Send;
    fn list_resources(&self, _ctx: &Self::Ctx) -> impl Future<Output = Vec<Resource>> + Send {
        async { Vec::new() }
    }
    fn resource_templates(&self) -> Vec<ResourceTemplate> {
        Vec::new()
    }
    fn read_resource(&self, _ctx: &Self::Ctx, uri: &str) -> impl Future<Output = Result<Vec<ResourceContents>, RpcError>> + Send {
        let uri = uri.to_string();
        async move { Err(RpcError::invalid_params("Resource not found").with_data(json!({"uri": uri}))) }
    }
    fn list_prompts(&self) -> Vec<Prompt> {
        Vec::new()
    }
    fn get_prompt(&self, _ctx: &Self::Ctx, name: &str, _args: Value) -> impl Future<Output = Result<Value, RpcError>> + Send {
        let name = name.to_string();
        async move { Err(RpcError::invalid_params(format!("Unknown prompt: {name}"))) }
    }
}

/// What a transport should send back.
#[derive(Debug, Clone)]
pub struct Reply {
    /// JSON-RPC response body (None for accepted notifications).
    pub body: Option<Value>,
    /// HTTP status for the Streamable HTTP transport.
    pub status: u16,
    /// Set when this message was an `initialize` that negotiated a legacy version.
    pub negotiated_legacy: Option<String>,
}

impl Reply {
    fn ok(body: Value) -> Self {
        Reply { body: Some(body), status: 200, negotiated_legacy: None }
    }
    fn err(id: Option<&Value>, status: u16, e: RpcError) -> Self {
        Reply { body: Some(error_response(id, &e)), status, negotiated_legacy: None }
    }
}

enum Era {
    Modern,
    Legacy,
}

fn capabilities() -> Value {
    json!({"tools": {"listChanged": false}, "resources": {}, "prompts": {"listChanged": false}})
}

pub struct Server<H: Handler> {
    pub handler: H,
}

impl<H: Handler> Server<H> {
    pub fn new(handler: H) -> Self {
        Server { handler }
    }

    /// Handles one JSON-RPC message. `legacy` is the legacy version negotiated earlier on this
    /// transport (stdio process), or declared by the HTTP `MCP-Protocol-Version` header.
    pub async fn handle(&self, ctx: &H::Ctx, msg: &Value, legacy: Option<&str>) -> Reply {
        let Some(obj) = msg.as_object() else {
            return Reply::err(None, 400, RpcError::new(INVALID_REQUEST, "expected a JSON-RPC object"));
        };
        if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Reply::err(obj.get("id"), 400, RpcError::new(INVALID_REQUEST, "jsonrpc must be \"2.0\""));
        }
        let Some(method) = obj.get("method").and_then(Value::as_str) else {
            // Responses from clients are not part of this server's protocol.
            return Reply::err(obj.get("id"), 400, RpcError::new(INVALID_REQUEST, "missing method"));
        };
        let params = obj.get("params").cloned().unwrap_or_else(|| json!({}));
        let Some(id) = obj.get("id").filter(|i| !i.is_null()) else {
            // Notifications (initialized, cancelled, ...) are accepted and need no reply.
            return Reply { body: None, status: 202, negotiated_legacy: None };
        };

        if method == "initialize" {
            let requested = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(LEGACY_VERSIONS[0]);
            let version = LEGACY_VERSIONS.iter().find(|v| **v == requested).copied().unwrap_or(LEGACY_VERSIONS[0]);
            let mut result = json!({
                "protocolVersion": version,
                "capabilities": capabilities(),
                "serverInfo": self.handler.server_info(),
            });
            if let Some(i) = self.handler.instructions() {
                result["instructions"] = json!(i);
            }
            let mut r = Reply::ok(response(id, result));
            r.negotiated_legacy = Some(version.to_string());
            return r;
        }

        let meta = params.get("_meta");
        let era = match meta.and_then(|m| m.get(META_PROTOCOL_VERSION)).and_then(Value::as_str) {
            Some(v) if v == MODERN_VERSION => {
                if meta.and_then(|m| m.get(META_CLIENT_CAPABILITIES)).is_none() {
                    return Reply::err(Some(id), 400, RpcError::invalid_params(format!("missing _meta[\"{META_CLIENT_CAPABILITIES}\"]")));
                }
                Era::Modern
            }
            Some(v) => {
                return Reply::err(
                    Some(id),
                    400,
                    RpcError::new(UNSUPPORTED_PROTOCOL_VERSION, "Unsupported protocol version").with_data(json!({"supported": supported_versions(), "requested": v})),
                );
            }
            None if legacy.is_some() => Era::Legacy,
            None => {
                return Reply::err(
                    Some(id),
                    400,
                    RpcError::invalid_params(format!(
                        "missing _meta[\"{META_PROTOCOL_VERSION}\"]; this server speaks {MODERN_VERSION} (per-request metadata) and the initialize handshake for {}",
                        LEGACY_VERSIONS.join(", ")
                    )),
                );
            }
        };

        let result = self.dispatch(ctx, method, &params, &era).await;
        match result {
            Ok(mut v) => {
                if let Era::Modern = era {
                    v["resultType"] = json!("complete");
                    let m = v.as_object_mut().and_then(|o| {
                        o.entry("_meta").or_insert_with(|| json!({}));
                        o.get_mut("_meta")
                    });
                    if let Some(m) = m {
                        m[META_SERVER_INFO] = json!(self.handler.server_info());
                    }
                }
                Reply::ok(response(id, v))
            }
            Err(e) => {
                let status = match (&era, e.code) {
                    (Era::Modern, METHOD_NOT_FOUND) => 404,
                    _ => 200,
                };
                Reply::err(Some(id), status, e)
            }
        }
    }

    async fn dispatch(&self, ctx: &H::Ctx, method: &str, params: &Value, era: &Era) -> Result<Value, RpcError> {
        let modern = matches!(era, Era::Modern);
        // Caching hints are part of the modern result shapes only.
        let cache = |mut v: Value, ttl: u64, scope: &str| {
            if modern {
                v["ttlMs"] = json!(ttl);
                v["cacheScope"] = json!(scope);
            }
            v
        };
        match method {
            "server/discover" if modern => {
                let mut v = json!({"supportedVersions": supported_versions(), "capabilities": capabilities()});
                if let Some(i) = self.handler.instructions() {
                    v["instructions"] = json!(i);
                }
                Ok(cache(v, 300_000, "public"))
            }
            "ping" => Ok(json!({})),
            "logging/setLevel" => Ok(json!({})),
            "completion/complete" => Ok(json!({"completion": {"values": [], "hasMore": false}})),
            "tools/list" => {
                let tools = self.handler.list_tools(ctx).await;
                Ok(cache(json!({"tools": tools}), 300_000, "public"))
            }
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).ok_or_else(|| RpcError::invalid_params("missing tool name"))?;
                let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                if !args.is_object() {
                    return Err(RpcError::invalid_params("arguments must be an object"));
                }
                let r = self.handler.call_tool(ctx, name, args).await?;
                serde_json::to_value(r).map_err(|e| RpcError::internal(e.to_string()))
            }
            "resources/list" => {
                let resources = self.handler.list_resources(ctx).await;
                Ok(cache(json!({"resources": resources}), 0, "private"))
            }
            "resources/templates/list" => Ok(cache(json!({"resourceTemplates": self.handler.resource_templates()}), 300_000, "public")),
            "resources/read" => {
                let uri = params.get("uri").and_then(Value::as_str).ok_or_else(|| RpcError::invalid_params("missing uri"))?;
                let contents = self.handler.read_resource(ctx, uri).await?;
                Ok(cache(json!({"contents": contents}), 0, "private"))
            }
            "prompts/list" => Ok(cache(json!({"prompts": self.handler.list_prompts()}), 300_000, "public")),
            "prompts/get" => {
                let name = params.get("name").and_then(Value::as_str).ok_or_else(|| RpcError::invalid_params("missing prompt name"))?;
                let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                self.handler.get_prompt(ctx, name, args).await
            }
            other => Err(RpcError::new(METHOD_NOT_FOUND, format!("Method not found: {other}"))),
        }
    }

    /// Serves newline-delimited JSON-RPC over a byte stream (the stdio transport).
    pub async fn serve_stream<R, W>(&self, ctx: &H::Ctx, reader: R, mut writer: W) -> std::io::Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut lines = BufReader::new(reader).lines();
        let mut legacy: Option<String> = None;
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let reply = match serde_json::from_str::<Value>(&line) {
                Ok(msg) => self.handle(ctx, &msg, legacy.as_deref()).await,
                Err(e) => Reply::err(None, 400, RpcError::new(PARSE_ERROR, format!("parse error: {e}"))),
            };
            if let Some(v) = reply.negotiated_legacy {
                legacy = Some(v);
            }
            if let Some(body) = reply.body {
                let mut out = serde_json::to_vec(&body).unwrap_or_default();
                out.push(b'\n');
                writer.write_all(&out).await?;
                writer.flush().await?;
            }
        }
        Ok(())
    }
}

/// Decodes the `=?base64?…?=` sentinel used for header values (spec: Value Encoding).
pub fn decode_header_value(v: &str) -> Option<String> {
    use base64::Engine;
    match v.strip_prefix("=?base64?").and_then(|s| s.strip_suffix("?=")) {
        Some(b64) => base64::engine::general_purpose::STANDARD.decode(b64).ok().and_then(|b| String::from_utf8(b).ok()),
        None => Some(v.to_string()),
    }
}

/// Validates Streamable HTTP request headers against a modern request body.
/// `header` looks up a header case-insensitively. Returns the error to send, if any.
pub fn validate_http_headers(body: &Value, header: impl Fn(&str) -> Option<String>) -> Result<(), RpcError> {
    let mismatch = |m: String| Err(RpcError::new(HEADER_MISMATCH, m));
    let method = body.get("method").and_then(Value::as_str).unwrap_or("");
    let params = body.get("params").cloned().unwrap_or_else(|| json!({}));
    let meta_version = params.get("_meta").and_then(|m| m.get(META_PROTOCOL_VERSION)).and_then(Value::as_str);
    let Some(meta_version) = meta_version else { return Ok(()) };
    match header("mcp-protocol-version") {
        Some(h) if h == meta_version => {}
        Some(h) => return mismatch(format!("Header mismatch: MCP-Protocol-Version header value '{h}' does not match body value '{meta_version}'")),
        None => return mismatch("Header mismatch: missing MCP-Protocol-Version header".into()),
    }
    if body.get("id").is_some() {
        match header("mcp-method") {
            Some(h) if h == method => {}
            Some(h) => return mismatch(format!("Header mismatch: Mcp-Method header value '{h}' does not match body value '{method}'")),
            None => return mismatch("Header mismatch: missing Mcp-Method header".into()),
        }
        let name_src = match method {
            "tools/call" | "prompts/get" => params.get("name").and_then(Value::as_str),
            "resources/read" => params.get("uri").and_then(Value::as_str),
            _ => None,
        };
        if let Some(expected) = name_src {
            match header("mcp-name").map(|h| decode_header_value(&h)) {
                Some(Some(h)) if h == expected => {}
                Some(Some(h)) => return mismatch(format!("Header mismatch: Mcp-Name header value '{h}' does not match body value '{expected}'")),
                Some(None) => return mismatch("Header mismatch: Mcp-Name header is not valid base64".into()),
                None => return mismatch("Header mismatch: missing Mcp-Name header".into()),
            }
        }
    }
    Ok(())
}
