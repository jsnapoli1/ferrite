use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
use ferrite_ai::*;
use ferrite_mcp::{Tool, ToolResult};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Mock {
    requests: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
    replies: Arc<Mutex<Vec<Value>>>,
}

async fn handler(State(m): State<Mock>, headers: HeaderMap, Json(body): Json<Value>) -> Json<Value> {
    m.requests.lock().unwrap().push((headers, body));
    Json(m.replies.lock().unwrap().remove(0))
}

async fn serve(path: &str, replies: Vec<Value>) -> (String, Mock) {
    let mock = Mock {
        replies: Arc::new(Mutex::new(replies)),
        ..Default::default()
    };
    let app = Router::new().route(path, post(handler)).with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), mock)
}

struct Files;
impl ToolBox for Files {
    async fn tools(&self) -> Vec<Tool> {
        vec![
            Tool::new("files.read", "Read", "Read a file", &[("path", "string", "path", true)]).read_only(),
            Tool::new("files_delete", "Delete", "Delete a file", &[("path", "string", "path", true)]).destructive(),
        ]
    }
    async fn call(&self, name: &str, args: Value) -> ToolResult {
        match name {
            "files.read" if args["path"] == "/notes.md" => ToolResult::text("Ship on Friday."),
            "files.read" => ToolResult::error("no such file"),
            _ => ToolResult::error("unexpected"),
        }
    }
}

#[tokio::test]
async fn openai_compatible_agent_loop() {
    let (base, mock) = serve(
        "/v1/chat/completions",
        vec![
            json!({"choices": [{"finish_reason": "tool_calls", "message": {"role": "assistant", "content": null, "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "files_read", "arguments": "{\"path\":\"/notes.md\"}"}},
                {"id": "c2", "type": "function", "function": {"name": "files_read", "arguments": "{\"path\":\"/missing\"}"}}
            ]}}]}),
            json!({"choices": [{"finish_reason": "stop", "message": {"role": "assistant", "content": "You ship on Friday."}}]}),
        ],
    )
    .await;
    let p = Provider::new(ProviderConfig {
        kind: ProviderKind::Openai,
        base_url: format!("{base}/v1"),
        api_key: Some("k".into()),
    });
    let mut t = Transcript {
        system: "Be brief.".into(),
        messages: vec![],
    };
    p.push_user(&mut t, "When do we ship?");
    let opts = AgentOptions {
        model: "llama3.2".into(),
        ..Default::default()
    };
    let run = run(&p, &mut t, &Files, &opts, None).await.unwrap();
    assert_eq!(run.answer, "You ship on Friday.");
    assert_eq!(run.steps, 2);
    let reqs = mock.requests.lock().unwrap();
    // Read-only by default: the destructive tool is not offered; dotted names are sanitized.
    let names: Vec<_> = reqs[0].1["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["files_read"]);
    assert_eq!(reqs[0].1["messages"][0], json!({"role": "system", "content": "Be brief."}));
    assert_eq!(reqs[0].0["authorization"], "Bearer k");
    let second = reqs[1].1["messages"].as_array().unwrap();
    assert_eq!(second[2]["tool_calls"][0]["id"], "c1");
    assert_eq!(second[3], json!({"role": "tool", "tool_call_id": "c1", "content": "Ship on Friday."}));
    assert_eq!(second[4]["content"], "no such file");
    assert!(run.trace.iter().any(|e| matches!(e, AgentEvent::ToolResult { is_error: true, .. })));
}

#[tokio::test]
async fn anthropic_agent_loop_preserves_blocks() {
    let thinking = json!({"type": "thinking", "thinking": "", "signature": "sig123"});
    let (base, mock) = serve(
        "/v1/messages",
        vec![
            json!({"stop_reason": "tool_use", "content": [thinking.clone(), {"type": "text", "text": "Checking."},
                   {"type": "tool_use", "id": "toolu_1", "name": "files_read", "input": {"path": "/notes.md"}}]}),
            json!({"stop_reason": "end_turn", "content": [{"type": "text", "text": "Friday."}]}),
        ],
    )
    .await;
    let p = Provider::new(ProviderConfig {
        kind: ProviderKind::Anthropic,
        base_url: base,
        api_key: Some("sk".into()),
    });
    let mut t = Transcript {
        system: "sys".into(),
        messages: vec![],
    };
    p.push_user(&mut t, "When?");
    let run = run(&p, &mut t, &Files, &AgentOptions::default(), None).await.unwrap();
    assert_eq!(run.answer, "Friday.");
    let reqs = mock.requests.lock().unwrap();
    let (h, b) = &reqs[0];
    assert_eq!(h["anthropic-version"], "2026-10-21");
    assert_eq!(h["x-api-key"], "sk");
    assert_eq!(b["model"], "claude-opus-5-5");
    for banned in ["temperature", "top_p", "top_k", "tool_choice", "thinking"] {
        assert!(b.get(banned).is_none(), "{banned} must not be sent");
    }
    assert!(b.get("fallbacks").is_none(), "fallbacks only on the first-party endpoint");
    assert_eq!(b["system"], "sys");
    assert_eq!(b["tools"][0]["name"], "files_read");
    assert!(b["tools"][0]["input_schema"].is_object());
    // Second request: assistant content replayed verbatim, then one user message of results.
    let msgs = reqs[1].1["messages"].as_array().unwrap();
    assert_eq!(msgs[1]["content"][0], thinking);
    assert_eq!(msgs[2]["role"], "user");
    assert_eq!(
        msgs[2]["content"][0],
        json!({"type": "tool_result", "tool_use_id": "toolu_1", "content": "Ship on Friday."})
    );
}

#[tokio::test]
async fn anthropic_refusal_and_truncation() {
    let (base, _m) = serve(
        "/v1/messages",
        vec![
            json!({"stop_reason": "refusal", "content": []}),
            json!({"stop_reason": "max_tokens", "content": [{"type": "text", "text": "partial"}, {"type": "tool_use", "id": "t", "name": "files_read", "input": {}}]}),
        ],
    )
    .await;
    let p = Provider::new(ProviderConfig {
        kind: ProviderKind::Anthropic,
        base_url: base,
        api_key: None,
    });
    let mut t = Transcript::default();
    p.push_user(&mut t, "x");
    assert!(matches!(run(&p, &mut t, &Files, &AgentOptions::default(), None).await, Err(AiError::Refused)));
    let mut t = Transcript::default();
    p.push_user(&mut t, "y");
    let r = run(&p, &mut t, &Files, &AgentOptions::default(), None).await.unwrap();
    assert_eq!(r.answer, "partial", "a truncated tool_use is never executed");
}

#[tokio::test]
async fn writes_are_offered_only_when_allowed() {
    let (base, mock) = serve(
        "/v1/chat/completions",
        vec![json!({"choices": [{"finish_reason": "stop", "message": {"content": "ok"}}]})],
    )
    .await;
    let p = Provider::new(ProviderConfig {
        kind: ProviderKind::Openai,
        base_url: format!("{base}/v1"),
        api_key: None,
    });
    let mut t = Transcript::default();
    p.push_user(&mut t, "delete it");
    let opts = AgentOptions {
        model: "m".into(),
        allow_writes: true,
        ..Default::default()
    };
    run(&p, &mut t, &Files, &opts, None).await.unwrap();
    assert_eq!(mock.requests.lock().unwrap()[0].1["tools"].as_array().unwrap().len(), 2);
}
