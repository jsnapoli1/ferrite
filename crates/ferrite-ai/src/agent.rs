//! The assistant's tool-use loop. Tools come from any [`ToolBox`] — in Ferrite that is the
//! same MCP tool set external clients see, plus tools of connected MCP servers and Craft apps.

use crate::provider::{AiError, Provider, Step, ToolCall, Transcript};
use ferrite_mcp::{Tool, ToolResult};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use tokio::sync::mpsc;

/// Largest tool output fed back to the model; the rest is elided with a note.
const MAX_TOOL_OUTPUT: usize = 24_000;

pub trait ToolBox: Send + Sync {
    fn tools(&self) -> impl Future<Output = Vec<Tool>> + Send;
    fn call(&self, name: &str, args: Value) -> impl Future<Output = ToolResult> + Send;
}

#[derive(Debug, Clone)]
pub struct AgentOptions {
    pub model: String,
    pub max_steps: usize,
    pub max_tokens: u32,
    /// When false only tools annotated read-only are offered (human-in-the-loop default).
    pub allow_writes: bool,
}

impl Default for AgentOptions {
    fn default() -> Self {
        AgentOptions {
            model: crate::provider::DEFAULT_ANTHROPIC_MODEL.into(),
            max_steps: 12,
            max_tokens: 16_000,
            allow_writes: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    Commentary { text: String },
    ToolCall { name: String, arguments: Value },
    ToolResult { name: String, is_error: bool, preview: String },
    Final { text: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentRun {
    pub answer: String,
    pub trace: Vec<AgentEvent>,
    pub steps: usize,
}

/// Provider tool names must match `^[a-zA-Z0-9_-]{1,64}$`.
pub fn sanitize_tool_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    s.chars().take(64).collect()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…[{} more bytes omitted]", &s[..end], s.len() - end)
}

pub async fn run<T: ToolBox>(
    provider: &Provider,
    transcript: &mut Transcript,
    toolbox: &T,
    opts: &AgentOptions,
    events: Option<mpsc::UnboundedSender<AgentEvent>>,
) -> Result<AgentRun, AiError> {
    let all = toolbox.tools().await;
    let mut names: HashMap<String, String> = HashMap::new();
    let mut tools: Vec<Tool> = Vec::new();
    for mut t in all.into_iter().filter(|t| opts.allow_writes || t.is_read_only()) {
        let safe = sanitize_tool_name(&t.name);
        if names.contains_key(&safe) {
            continue;
        }
        names.insert(safe.clone(), t.name.clone());
        t.name = safe;
        tools.push(t);
    }
    let mut trace = Vec::new();
    let emit = |trace: &mut Vec<AgentEvent>, e: AgentEvent| {
        if let Some(tx) = &events {
            let _ = tx.send(e.clone());
        }
        trace.push(e);
    };
    for step in 1..=opts.max_steps {
        match provider.step(&opts.model, transcript, &tools, opts.max_tokens).await? {
            Step::Final(text) => {
                emit(&mut trace, AgentEvent::Final { text: text.clone() });
                return Ok(AgentRun {
                    answer: text,
                    trace,
                    steps: step,
                });
            }
            Step::Truncated(text) => {
                let answer = if text.is_empty() {
                    "(The answer was cut off by the output limit.)".to_string()
                } else {
                    text
                };
                emit(&mut trace, AgentEvent::Final { text: answer.clone() });
                return Ok(AgentRun { answer, trace, steps: step });
            }
            Step::Tools { text, calls } => {
                if !text.trim().is_empty() {
                    emit(&mut trace, AgentEvent::Commentary { text: text.clone() });
                }
                if calls.is_empty() {
                    // Paused server-side turn: send the transcript again to continue.
                    continue;
                }
                for c in &calls {
                    emit(
                        &mut trace,
                        AgentEvent::ToolCall {
                            name: names.get(&c.name).cloned().unwrap_or_else(|| c.name.clone()),
                            arguments: c.arguments.clone(),
                        },
                    );
                }
                let futures = calls.iter().map(|c| {
                    let real = names.get(&c.name).cloned();
                    async move {
                        match real {
                            Some(n) => toolbox.call(&n, c.arguments.clone()).await,
                            None => ToolResult::error(format!("Unknown tool: {}", c.name)),
                        }
                    }
                });
                let outputs = futures::future::join_all(futures).await;
                let mut results: Vec<(ToolCall, String, bool)> = Vec::new();
                for (c, r) in calls.into_iter().zip(outputs) {
                    let text = truncate(&r.text_content(), MAX_TOOL_OUTPUT);
                    emit(
                        &mut trace,
                        AgentEvent::ToolResult {
                            name: names.get(&c.name).cloned().unwrap_or_default(),
                            is_error: r.is_error,
                            preview: truncate(&text, 400),
                        },
                    );
                    results.push((c, text, r.is_error));
                }
                provider.push_tool_results(transcript, &results);
            }
        }
    }
    let answer = "I stopped after reaching the step limit before finishing.".to_string();
    emit(&mut trace, AgentEvent::Final { text: answer.clone() });
    Ok(AgentRun {
        answer,
        trace,
        steps: opts.max_steps,
    })
}
