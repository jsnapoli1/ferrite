//! Model providers. Two wire formats cover nearly everything people run:
//!
//! * `openai` — any OpenAI-compatible Chat Completions server: Ollama, llama.cpp's
//!   `llama-server`, vLLM, LM Studio, LocalAI, or OpenAI itself. This is how Ferrite runs
//!   entirely on local hardware.
//! * `anthropic` — the Claude Messages API (raw HTTP, API version 2026-10-21).
//!
//! Each provider keeps the conversation in its own native message format and only ever
//! appends to it. That keeps provider-specific content (such as Claude's thinking blocks,
//! which must be sent back unchanged) intact across tool-use turns.

use ferrite_mcp::Tool;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

pub const ANTHROPIC_VERSION: &str = "2026-10-21";
pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-opus-5-5";
const ANTHROPIC_FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Openai,
    Anthropic,
}

impl ProviderKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "openai" => Some(Self::Openai),
            "anthropic" => Some(Self::Anthropic),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    /// e.g. `http://127.0.0.1:11434/v1` (Ollama) or `https://api.anthropic.com`.
    pub base_url: String,
    pub api_key: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AiError {
    #[error("provider request failed: {0}")]
    Http(String),
    #[error("provider returned an error ({status}): {body}")]
    Api { status: u16, body: String },
    #[error("unexpected provider response: {0}")]
    Format(String),
    #[error("the model declined this request")]
    Refused,
}

impl From<reqwest::Error> for AiError {
    fn from(e: reqwest::Error) -> Self {
        AiError::Http(e.to_string())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// Outcome of one model turn.
#[derive(Debug, Clone)]
pub enum Step {
    /// The model is done; its final text.
    Final(String),
    /// The model wants tools run. `text` is any commentary it gave alongside.
    Tools { text: String, calls: Vec<ToolCall> },
    /// The turn was cut off by the output limit before finishing.
    Truncated(String),
}

/// A conversation in the provider's native message format.
#[derive(Debug, Clone, Default)]
pub struct Transcript {
    pub system: String,
    pub messages: Vec<Value>,
}

pub struct Provider {
    cfg: ProviderConfig,
    http: reqwest::Client,
}

impl Provider {
    pub fn new(cfg: ProviderConfig) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(600)).build().unwrap_or_default();
        Provider { cfg, http }
    }

    pub fn kind(&self) -> &ProviderKind {
        &self.cfg.kind
    }

    fn base(&self) -> &str {
        self.cfg.base_url.trim_end_matches('/')
    }

    pub fn push_user(&self, t: &mut Transcript, text: &str) {
        t.messages.push(json!({"role": "user", "content": text}));
    }

    /// Runs one model turn on the transcript, appending the assistant message to it.
    pub async fn step(&self, model: &str, t: &mut Transcript, tools: &[Tool], max_tokens: u32) -> Result<Step, AiError> {
        match self.cfg.kind {
            ProviderKind::Openai => self.step_openai(model, t, tools, max_tokens).await,
            ProviderKind::Anthropic => self.step_anthropic(model, t, tools, max_tokens).await,
        }
    }

    /// Appends tool results (in the order of the calls) to the transcript.
    pub fn push_tool_results(&self, t: &mut Transcript, results: &[(ToolCall, String, bool)]) {
        match self.cfg.kind {
            ProviderKind::Openai => {
                for (call, text, _is_error) in results {
                    t.messages.push(json!({"role": "tool", "tool_call_id": call.id, "content": text}));
                }
            }
            ProviderKind::Anthropic => {
                // All results go back in a single user message.
                let blocks: Vec<Value> = results
                    .iter()
                    .map(|(call, text, is_error)| {
                        let mut b = json!({"type": "tool_result", "tool_use_id": call.id, "content": text});
                        if *is_error {
                            b["is_error"] = json!(true);
                        }
                        b
                    })
                    .collect();
                t.messages.push(json!({"role": "user", "content": blocks}));
            }
        }
    }

    /// Single prompt → text helper (summaries, titles, extraction).
    pub async fn complete(&self, model: &str, system: &str, prompt: &str, max_tokens: u32) -> Result<String, AiError> {
        let mut t = Transcript {
            system: system.to_string(),
            messages: Vec::new(),
        };
        self.push_user(&mut t, prompt);
        match self.step(model, &mut t, &[], max_tokens).await? {
            Step::Final(s) | Step::Truncated(s) => Ok(s),
            Step::Tools { text, .. } => Ok(text),
        }
    }

    /// Embeddings (OpenAI-compatible servers only).
    pub async fn embed(&self, model: &str, inputs: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
        if self.cfg.kind != ProviderKind::Openai {
            return Err(AiError::Format("embeddings need an OpenAI-compatible provider".into()));
        }
        let mut req = self
            .http
            .post(format!("{}/embeddings", self.base()))
            .json(&json!({"model": model, "input": inputs}));
        if let Some(k) = &self.cfg.api_key {
            req = req.bearer_auth(k);
        }
        let v = check(req.send().await?).await?;
        let data = v["data"].as_array().ok_or_else(|| AiError::Format("missing data".into()))?;
        let mut out = vec![Vec::new(); inputs.len()];
        for (i, d) in data.iter().enumerate() {
            let idx = d["index"].as_u64().map(|x| x as usize).unwrap_or(i);
            let vec: Vec<f32> = d["embedding"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_f64)
                .map(|f| f as f32)
                .collect();
            if idx < out.len() {
                out[idx] = vec;
            }
        }
        Ok(out)
    }

    /// Lists model ids the provider offers.
    pub async fn list_models(&self) -> Result<Vec<String>, AiError> {
        let mut req = self.http.get(format!(
            "{}/{}",
            self.base(),
            if self.cfg.kind == ProviderKind::Anthropic { "v1/models" } else { "models" }
        ));
        req = match (&self.cfg.kind, &self.cfg.api_key) {
            (ProviderKind::Anthropic, Some(k)) => req.header("x-api-key", k).header("anthropic-version", ANTHROPIC_VERSION),
            (ProviderKind::Openai, Some(k)) => req.bearer_auth(k),
            _ => req,
        };
        let v = check(req.send().await?).await?;
        Ok(v["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["id"].as_str().map(String::from))
            .collect())
    }

    async fn step_openai(&self, model: &str, t: &mut Transcript, tools: &[Tool], max_tokens: u32) -> Result<Step, AiError> {
        let mut messages = Vec::with_capacity(t.messages.len() + 1);
        if !t.system.is_empty() {
            messages.push(json!({"role": "system", "content": t.system}));
        }
        messages.extend(t.messages.iter().cloned());
        let mut body = json!({"model": model, "messages": messages, "max_tokens": max_tokens});
        if !tools.is_empty() {
            body["tools"] = Value::Array(
                tools
                    .iter()
                    .map(|tool| json!({"type": "function", "function": {"name": tool.name, "description": tool.description, "parameters": tool.input_schema}}))
                    .collect(),
            );
        }
        let mut req = self.http.post(format!("{}/chat/completions", self.base())).json(&body);
        if let Some(k) = &self.cfg.api_key {
            req = req.bearer_auth(k);
        }
        let v = check(req.send().await?).await?;
        let choice = &v["choices"][0];
        let msg = choice["message"].clone();
        if msg.is_null() {
            return Err(AiError::Format(format!("no choices in response: {v}")));
        }
        let text = msg["content"].as_str().unwrap_or("").to_string();
        let mut calls = Vec::new();
        for c in msg["tool_calls"].as_array().into_iter().flatten() {
            let raw = &c["function"]["arguments"];
            // Arguments arrive as a JSON string (some local servers send an object).
            let arguments = match raw {
                Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| json!({"_raw": s})),
                Value::Object(_) => raw.clone(),
                _ => json!({}),
            };
            calls.push(ToolCall {
                id: c["id"].as_str().map(String::from).unwrap_or_else(|| format!("call_{}", calls.len())),
                name: c["function"]["name"].as_str().unwrap_or("").to_string(),
                arguments,
            });
        }
        // Store the assistant message as returned (minus provider extras).
        let mut stored = json!({"role": "assistant", "content": msg["content"].clone()});
        if let Some(tc) = msg.get("tool_calls").filter(|v| !v.is_null()) {
            let mut tc = tc.clone();
            // Ensure every call has the id we use for its result.
            if let Some(arr) = tc.as_array_mut() {
                for (i, c) in arr.iter_mut().enumerate() {
                    if c.get("id").and_then(Value::as_str).is_none() {
                        c["id"] = json!(calls[i].id);
                    }
                }
            }
            stored["tool_calls"] = tc;
        }
        t.messages.push(stored);
        let finish = choice["finish_reason"].as_str().unwrap_or("stop");
        if !calls.is_empty() && finish != "length" {
            return Ok(Step::Tools { text, calls });
        }
        if finish == "length" {
            return Ok(Step::Truncated(text));
        }
        Ok(Step::Final(text))
    }

    async fn step_anthropic(&self, model: &str, t: &mut Transcript, tools: &[Tool], max_tokens: u32) -> Result<Step, AiError> {
        let first_party = self.base().contains("api.anthropic.com");
        // Thinking is left at the model default (adaptive). No sampling parameters: API
        // version 2026-10-21 rejects them.
        let mut body = json!({"model": model, "max_tokens": max_tokens, "messages": t.messages});
        if !t.system.is_empty() {
            body["system"] = json!(t.system);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(
                tools
                    .iter()
                    .map(|tool| json!({"name": tool.name, "description": tool.description, "input_schema": tool.input_schema}))
                    .collect(),
            );
        }
        if first_party {
            // Server-side fallback: a safety decline is retried on a suitable model in-call.
            body["fallbacks"] = json!("default");
        }
        let mut req = self
            .http
            .post(format!("{}/v1/messages", self.base()))
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body);
        if first_party {
            req = req.header("anthropic-beta", ANTHROPIC_FALLBACK_BETA);
        }
        if let Some(k) = &self.cfg.api_key {
            req = req.header("x-api-key", k);
        }
        let v = check(req.send().await?).await?;
        let content = v["content"].clone();
        let stop = v["stop_reason"].as_str().unwrap_or("end_turn").to_string();
        // Append the assistant content exactly as returned (thinking blocks included).
        t.messages.push(json!({"role": "assistant", "content": content}));
        let blocks = content.as_array().cloned().unwrap_or_default();
        let text: String = blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("");
        match stop.as_str() {
            "refusal" => Err(AiError::Refused),
            // A truncated turn may hold a partial tool_use; never run it.
            "max_tokens" => Ok(Step::Truncated(text)),
            "tool_use" => {
                let calls = blocks
                    .iter()
                    .filter(|b| b["type"] == "tool_use")
                    .map(|b| ToolCall {
                        id: b["id"].as_str().unwrap_or("").to_string(),
                        name: b["name"].as_str().unwrap_or("").to_string(),
                        arguments: b["input"].clone(),
                    })
                    .collect();
                Ok(Step::Tools { text, calls })
            }
            // pause_turn: the server paused its own tool loop; re-sending continues it.
            "pause_turn" => Ok(Step::Tools { text, calls: Vec::new() }),
            _ => Ok(Step::Final(text)),
        }
    }
}

async fn check(resp: reqwest::Response) -> Result<Value, AiError> {
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(AiError::Api {
            status: status.as_u16(),
            body: text.chars().take(2000).collect(),
        });
    }
    serde_json::from_str(&text).map_err(|e| AiError::Format(format!("{e}: {}", text.chars().take(300).collect::<String>())))
}
