//! MCP wire types and constants (spec revision 2026-07-28, plus the legacy
//! `initialize`-based revisions it is backward compatible with).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The current ("modern") protocol revision: per-request `_meta`, no handshake.
pub const MODERN_VERSION: &str = "2026-07-28";
/// Handshake-based ("legacy") revisions, newest first.
pub const LEGACY_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

pub const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
pub const META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
pub const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
pub const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
pub const HEADER_MISMATCH: i64 = -32020;
pub const MISSING_REQUIRED_CLIENT_CAPABILITY: i64 = -32021;
pub const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

/// Error codes that identify a modern server during era detection.
pub fn is_modern_error(code: i64) -> bool {
    matches!(code, HEADER_MISMATCH | MISSING_REQUIRED_CLIENT_CAPABILITY | UNSUPPORTED_PROTOCOL_VERSION)
}

pub fn supported_versions() -> Vec<&'static str> {
    std::iter::once(MODERN_VERSION).chain(LEGACY_VERSIONS.iter().copied()).collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Implementation {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub description: String,
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
}

impl Tool {
    /// A tool with an object input schema built from `(name, type, description, required)`.
    pub fn new(name: &str, title: &str, description: &str, params: &[(&str, &str, &str, bool)]) -> Self {
        let mut props = serde_json::Map::new();
        let mut required = Vec::new();
        for (pname, ptype, pdesc, req) in params {
            let schema = match *ptype {
                "string[]" => json!({"type": "array", "items": {"type": "string"}, "description": pdesc}),
                "integer[]" => json!({"type": "array", "items": {"type": "integer"}, "description": pdesc}),
                "object" => json!({"type": "object", "description": pdesc}),
                "array" => json!({"type": "array", "items": {"type": "object"}, "description": pdesc}),
                t => json!({"type": t, "description": pdesc}),
            };
            props.insert(pname.to_string(), schema);
            if *req {
                required.push(pname.to_string());
            }
        }
        let input_schema = if props.is_empty() {
            json!({"type": "object", "additionalProperties": false})
        } else {
            json!({"type": "object", "properties": props, "required": required})
        };
        Tool { name: name.into(), title: Some(title.into()), description: description.into(), input_schema, output_schema: None, annotations: None }
    }

    pub fn read_only(mut self) -> Self {
        self.annotations = Some(json!({"readOnlyHint": true, "openWorldHint": false}));
        self
    }

    pub fn destructive(mut self) -> Self {
        self.annotations = Some(json!({"readOnlyHint": false, "destructiveHint": true, "openWorldHint": false}));
        self
    }

    pub fn is_read_only(&self) -> bool {
        self.annotations.as_ref().and_then(|a| a["readOnlyHint"].as_bool()).unwrap_or(false)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resource {
    pub uri: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceTemplate {
    pub uri_template: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceContents {
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Base64 encoded bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptArgument {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prompt {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub arguments: Vec<PromptArgument>,
}

/// Result of `tools/call`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub content: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    #[serde(default)]
    pub is_error: bool,
}

impl ToolResult {
    pub fn text(s: impl Into<String>) -> Self {
        ToolResult { content: vec![json!({"type": "text", "text": s.into()})], structured_content: None, is_error: false }
    }

    /// Structured result; also serialized as text for clients without structured support.
    pub fn json(v: Value) -> Self {
        let text = serde_json::to_string_pretty(&v).unwrap_or_default();
        ToolResult { content: vec![json!({"type": "text", "text": text})], structured_content: Some(v), is_error: false }
    }

    pub fn error(s: impl Into<String>) -> Self {
        ToolResult { content: vec![json!({"type": "text", "text": s.into()})], structured_content: None, is_error: true }
    }

    /// Concatenated text content (for feeding results to a model).
    pub fn text_content(&self) -> String {
        self.content.iter().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n")
    }
}

#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize, PartialEq)]
#[error("MCP error {code}: {message}")]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        RpcError { code, message: message.into(), data: None }
    }
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(INTERNAL_ERROR, message)
    }
}

pub fn response(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub fn error_response(id: Option<&Value>, err: &RpcError) -> Value {
    let mut v = json!({"jsonrpc": "2.0", "error": err});
    if let Some(id) = id {
        v["id"] = id.clone();
    }
    v
}
