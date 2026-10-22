//! AI for Ferrite: model providers (local OpenAI-compatible servers or Claude) and the
//! assistant's tool-use loop over MCP tools.

pub mod agent;
pub mod provider;

pub use agent::{AgentEvent, AgentOptions, AgentRun, ToolBox, run};
pub use provider::{AiError, Provider, ProviderConfig, ProviderKind, Step, ToolCall, Transcript};
