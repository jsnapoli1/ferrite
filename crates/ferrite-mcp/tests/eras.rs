use ferrite_mcp::*;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct Echo;

impl Handler for Echo {
    type Ctx = String;
    fn server_info(&self) -> Implementation {
        Implementation { name: "echo".into(), version: "1".into(), title: None }
    }
    fn instructions(&self) -> Option<String> {
        Some("Echoes things.".into())
    }
    async fn list_tools(&self, _ctx: &String) -> Vec<Tool> {
        vec![Tool::new("echo", "Echo", "Echo text back", &[("text", "string", "what to echo", true)]).read_only()]
    }
    async fn call_tool(&self, ctx: &String, name: &str, args: Value) -> Result<ToolResult, RpcError> {
        match name {
            "echo" => Ok(ToolResult::json(json!({"user": ctx, "text": args["text"]}))),
            _ => Err(RpcError::invalid_params(format!("Unknown tool: {name}"))),
        }
    }
}

fn modern(method: &str, params: Value) -> Value {
    let mut p = params;
    p["_meta"] = json!({META_PROTOCOL_VERSION: MODERN_VERSION, META_CLIENT_CAPABILITIES: {}});
    json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": p})
}

#[tokio::test]
async fn modern_requests_are_stateless() {
    let s = Server::new(Echo);
    let ctx = "alice".to_string();
    let r = s.handle(&ctx, &modern("server/discover", json!({})), None).await;
    let b = r.body.unwrap();
    assert_eq!(b["result"]["resultType"], "complete");
    assert_eq!(b["result"]["supportedVersions"][0], MODERN_VERSION);
    assert_eq!(b["result"]["_meta"][META_SERVER_INFO]["name"], "echo");
    assert!(b["result"]["ttlMs"].is_u64());

    let r = s.handle(&ctx, &modern("tools/call", json!({"name": "echo", "arguments": {"text": "hi"}})), None).await;
    let b = r.body.unwrap();
    assert_eq!(b["result"]["structuredContent"], json!({"user": "alice", "text": "hi"}));
    assert_eq!(b["result"]["isError"], false);

    // Unsupported version -> 400 + -32022 listing what we support.
    let mut bad = modern("tools/list", json!({}));
    bad["params"]["_meta"][META_PROTOCOL_VERSION] = json!("1900-01-01");
    let r = s.handle(&ctx, &bad, None).await;
    assert_eq!(r.status, 400);
    let b = r.body.unwrap();
    assert_eq!(b["error"]["code"], UNSUPPORTED_PROTOCOL_VERSION);
    assert_eq!(b["error"]["data"]["requested"], "1900-01-01");

    // Missing capabilities -> -32602 / 400. Unknown method -> 404.
    let mut nocap = modern("tools/list", json!({}));
    nocap["params"]["_meta"].as_object_mut().unwrap().remove(META_CLIENT_CAPABILITIES);
    assert_eq!(s.handle(&ctx, &nocap, None).await.status, 400);
    let r = s.handle(&ctx, &modern("subscriptions/listen", json!({})), None).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.body.unwrap()["error"]["code"], METHOD_NOT_FOUND);

    // No metadata and no legacy session: rejected with guidance.
    let r = s.handle(&ctx, &json!({"jsonrpc": "2.0", "id": 9, "method": "tools/list"}), None).await;
    assert_eq!(r.status, 400);
    // Notifications are accepted without a body.
    let r = s.handle(&ctx, &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}), None).await;
    assert_eq!((r.status, r.body.is_none()), (202, true));
}

#[tokio::test]
async fn header_validation() {
    let body = modern("tools/call", json!({"name": "echo", "arguments": {}}));
    let ok = |h: &str| match h {
        "mcp-protocol-version" => Some(MODERN_VERSION.to_string()),
        "mcp-method" => Some("tools/call".to_string()),
        "mcp-name" => Some("=?base64?ZWNobw==?=".to_string()),
        _ => None,
    };
    assert!(server::validate_http_headers(&body, ok).is_ok());
    let wrong = |h: &str| match h {
        "mcp-protocol-version" => Some(MODERN_VERSION.to_string()),
        "mcp-method" => Some("tools/call".to_string()),
        "mcp-name" => Some("other".to_string()),
        _ => None,
    };
    assert_eq!(server::validate_http_headers(&body, wrong).unwrap_err().code, HEADER_MISMATCH);
}

#[tokio::test]
async fn dual_client_talks_to_modern_server() {
    let (c_side, s_side) = tokio::io::duplex(64 * 1024);
    let (sr, sw) = tokio::io::split(s_side);
    tokio::spawn(async move {
        let s = Server::new(Echo);
        s.serve_stream(&"bob".to_string(), sr, sw).await.unwrap();
    });
    let (cr, cw) = tokio::io::split(c_side);
    let client = Client::from_stream(cr, cw, None).await.unwrap();
    assert_eq!(client.era(), &Era::Modern(MODERN_VERSION.into()));
    assert_eq!(client.server_info.as_ref().unwrap().name, "echo");
    assert_eq!(client.instructions.as_deref(), Some("Echoes things."));
    let tools = client.list_tools().await.unwrap();
    assert!(tools[0].is_read_only());
    let r = client.call_tool("echo", json!({"text": "yo"})).await.unwrap();
    assert_eq!(r.structured_content.unwrap()["user"], "bob");
    let e = client.call_tool("nope", json!({})).await.unwrap_err();
    assert!(matches!(e, ClientError::Rpc(RpcError { code: INVALID_PARAMS, .. })));
}

/// A legacy-only server like the Craft apps' `<app>-cli mcp` (protocol 2025-06-18): rejects
/// anything before `initialize`.
async fn legacy_server(stream: tokio::io::DuplexStream) {
    let (r, mut w) = tokio::io::split(stream);
    let mut lines = BufReader::new(r).lines();
    let mut initialized = false;
    while let Ok(Some(line)) = lines.next_line().await {
        let msg: Value = serde_json::from_str(&line).unwrap();
        let Some(id) = msg.get("id").cloned() else { continue };
        let method = msg["method"].as_str().unwrap_or("");
        let reply = match method {
            "initialize" => {
                initialized = true;
                json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "wordcraft", "version": "0.1"}}})
            }
            _ if !initialized => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32002, "message": "server not initialized"}}),
            "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [{"name": "open_document", "description": "Open", "inputSchema": {"type": "object"}}]}}),
            "tools/call" => {
                assert!(msg["params"].get("_meta").is_none(), "legacy requests carry no modern _meta");
                json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text", "text": "opened"}]}})
            }
            _ => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "unknown"}}),
        };
        let mut out = serde_json::to_vec(&reply).unwrap();
        out.push(b'\n');
        w.write_all(&out).await.unwrap();
    }
}

#[tokio::test]
async fn dual_client_falls_back_for_legacy_servers() {
    let (c_side, s_side) = tokio::io::duplex(64 * 1024);
    tokio::spawn(legacy_server(s_side));
    let (cr, cw) = tokio::io::split(c_side);
    let client = Client::from_stream(cr, cw, None).await.unwrap();
    assert_eq!(client.era(), &Era::Legacy("2025-06-18".into()));
    assert_eq!(client.server_info.as_ref().unwrap().name, "wordcraft");
    assert_eq!(client.list_tools().await.unwrap()[0].name, "open_document");
    assert_eq!(client.call_tool("open_document", json!({"path": "/x.docx"})).await.unwrap().text_content(), "opened");
}

#[tokio::test]
async fn legacy_client_is_served() {
    let (c_side, s_side) = tokio::io::duplex(64 * 1024);
    let (sr, sw) = tokio::io::split(s_side);
    tokio::spawn(async move { Server::new(Echo).serve_stream(&"carol".to_string(), sr, sw).await.unwrap() });
    let (cr, mut cw) = tokio::io::split(c_side);
    let mut lines = BufReader::new(cr).lines();
    let send = |v: Value| {
        let mut b = serde_json::to_vec(&v).unwrap();
        b.push(b'\n');
        b
    };
    cw.write_all(&send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "old", "version": "1"}}}))).await.unwrap();
    let init: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "echo");
    cw.write_all(&send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))).await.unwrap();
    cw.write_all(&send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "echo", "arguments": {"text": "old"}}}))).await.unwrap();
    let r: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(r["result"]["structuredContent"]["text"], "old");
    assert!(r["result"].get("resultType").is_none(), "legacy results keep the legacy shape");

    // An unknown legacy version negotiates down to our newest legacy version.
    cw.write_all(&send(json!({"jsonrpc": "2.0", "id": 3, "method": "initialize", "params": {"protocolVersion": "2099-01-01", "capabilities": {}}}))).await.unwrap();
    let r: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(r["result"]["protocolVersion"], LEGACY_VERSIONS[0]);
}
