//! AI service: provider connections, which model serves which role, consent, the assistant,
//! and the Talk bot (`@assistant` in any conversation that allows it).
//!
//! The assistant never sees data the user couldn't: it acts through the same MCP tools with
//! the user's own permissions, read-only unless the user allows changes for that request.

use crate::tools::{FerriteTools, UserCtx, UserToolBox};
use ferrite_ai::{AgentEvent, AgentOptions, AgentRun, Provider, ProviderConfig, ProviderKind, Transcript};
use ferrite_core::auth::User;
use ferrite_core::talk::{Actor, BOT_ACTOR_ID, Message, NewMessage};
use ferrite_core::util::{new_id, now};
use ferrite_core::{Core, Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::mpsc;

pub const ROLES: &[(&str, &str)] = &[
    ("chat", "The assistant (chat, tool use, the Talk bot)"),
    ("summarize", "Summaries and quick actions (falls back to chat)"),
    ("embed", "Embeddings for semantic search (OpenAI-compatible only)"),
];

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ProviderRow {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub base_url: String,
    #[serde(skip_serializing)]
    pub api_key: Option<String>,
    pub models: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewProvider {
    pub name: String,
    pub kind: String,
    pub base_url: String,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct RoleRow {
    pub role: String,
    pub provider_id: String,
    pub model: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatTurn {
    pub role: String,
    pub content: String,
}

pub struct AiService {
    core: Core,
    tools: Arc<FerriteTools>,
}

fn system_prompt(user: &User, allow_writes: bool, extra: &str) -> String {
    format!(
        "You are the Ferrite assistant, built into a self-hosted collaboration server (files, Talk chat and calls, \
         kanban boards, live documents, mind maps, canvases, and the ArtCraft Crafting Apps). You are helping {name} \
         (@{username}). Today is {date}.\n\n\
         Use the tools to look things up instead of guessing; cite file paths and conversation names you used. \
         {writes}\n\
         Keep answers concise and use Markdown.{extra}",
        name = user.display_name,
        username = user.username,
        date = now().format("%A, %Y-%m-%d"),
        writes = if allow_writes {
            "You may make changes the user asks for (create, edit, move, share, post); say exactly what you changed."
        } else {
            "You are in read-only mode: you can read and search but not change anything. If a change is needed, describe it and tell the user they can allow changes."
        },
        extra = if extra.is_empty() { String::new() } else { format!("\n\n{extra}") },
    )
}

impl AiService {
    pub fn new(core: Core, tools: Arc<FerriteTools>) -> Arc<Self> {
        Arc::new(AiService { core, tools })
    }

    // ---- configuration --------------------------------------------------------

    pub async fn providers(&self) -> Result<Vec<ProviderRow>> {
        Ok(
            sqlx::query_as("SELECT id, name, kind, base_url, api_key, models FROM ai_providers ORDER BY name")
                .fetch_all(&self.core.db)
                .await?,
        )
    }

    pub async fn add_provider(&self, p: NewProvider) -> Result<ProviderRow> {
        if ProviderKind::parse(&p.kind).is_none() {
            return Err(Error::bad("kind must be openai (any OpenAI-compatible server, e.g. Ollama) or anthropic"));
        }
        if !(p.base_url.starts_with("http://") || p.base_url.starts_with("https://")) {
            return Err(Error::bad("base_url must be an http(s) URL"));
        }
        let id = new_id();
        sqlx::query("INSERT INTO ai_providers (id, name, kind, base_url, api_key, created_at) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(&id)
            .bind(p.name.trim())
            .bind(&p.kind)
            .bind(p.base_url.trim_end_matches('/'))
            .bind(p.api_key.filter(|k| !k.is_empty()))
            .bind(now())
            .execute(&self.core.db)
            .await?;
        self.providers().await?.into_iter().find(|r| r.id == id).ok_or(Error::NotFound)
    }

    pub async fn delete_provider(&self, id: &str) -> Result<()> {
        sqlx::query("DELETE FROM ai_providers WHERE id = ?").bind(id).execute(&self.core.db).await?;
        Ok(())
    }

    fn provider_of(row: &ProviderRow) -> Provider {
        Provider::new(ProviderConfig {
            kind: ProviderKind::parse(&row.kind).unwrap_or(ProviderKind::Openai),
            base_url: row.base_url.clone(),
            api_key: row.api_key.clone(),
        })
    }

    /// Asks the provider which models it serves (and caches the list).
    pub async fn refresh_models(&self, id: &str) -> Result<Vec<String>> {
        let row = self.providers().await?.into_iter().find(|r| r.id == id).ok_or(Error::NotFound)?;
        let models = Self::provider_of(&row).list_models().await.map_err(|e| Error::Unavailable(e.to_string()))?;
        sqlx::query("UPDATE ai_providers SET models = ? WHERE id = ?")
            .bind(serde_json::to_string(&models).unwrap_or_default())
            .bind(id)
            .execute(&self.core.db)
            .await?;
        Ok(models)
    }

    pub async fn roles(&self) -> Result<Vec<RoleRow>> {
        Ok(sqlx::query_as("SELECT role, provider_id, model FROM ai_roles ORDER BY role")
            .fetch_all(&self.core.db)
            .await?)
    }

    pub async fn set_role(&self, role: &str, provider_id: &str, model: &str) -> Result<()> {
        if !ROLES.iter().any(|(r, _)| *r == role) {
            return Err(Error::bad("unknown role"));
        }
        if model.trim().is_empty() {
            return Err(Error::bad("model required"));
        }
        sqlx::query("INSERT INTO ai_roles (role, provider_id, model) VALUES (?, ?, ?) ON CONFLICT(role) DO UPDATE SET provider_id = excluded.provider_id, model = excluded.model")
            .bind(role)
            .bind(provider_id)
            .bind(model.trim())
            .execute(&self.core.db)
            .await?;
        Ok(())
    }

    /// The provider and model for a role (summarize falls back to chat).
    pub async fn for_role(&self, role: &str) -> Result<(Provider, String)> {
        let roles = self.roles().await?;
        let pick = roles
            .iter()
            .find(|r| r.role == role)
            .or_else(|| if role != "embed" { roles.iter().find(|r| r.role == "chat") } else { None });
        let r = pick.ok_or_else(|| Error::Unavailable("AI is not configured yet: an administrator can add a provider under Settings → AI".into()))?;
        let row = self
            .providers()
            .await?
            .into_iter()
            .find(|p| p.id == r.provider_id)
            .ok_or_else(|| Error::Unavailable("the configured AI provider was removed".into()))?;
        Ok((Self::provider_of(&row), r.model.clone()))
    }

    pub async fn status(&self) -> Result<Value> {
        let roles = self.roles().await?;
        Ok(json!({"configured": roles.iter().any(|r| r.role == "chat"), "roles": roles}))
    }

    // ---- assistant --------------------------------------------------------------

    async fn record_job(&self, user_id: &str, kind: &str, input: &Value) -> Result<String> {
        let id = new_id();
        sqlx::query("INSERT INTO ai_jobs (id, user_id, kind, status, input, created_at) VALUES (?, ?, ?, 'running', ?, ?)")
            .bind(&id)
            .bind(user_id)
            .bind(kind)
            .bind(input.to_string())
            .bind(now())
            .execute(&self.core.db)
            .await?;
        Ok(id)
    }

    async fn finish_job(&self, id: &str, result: std::result::Result<&AgentRun, String>) {
        let (status, output, error) = match result {
            Ok(run) => ("done", Some(serde_json::to_string(run).unwrap_or_default()), None),
            Err(e) => ("failed", None, Some(e)),
        };
        let _ = sqlx::query("UPDATE ai_jobs SET status = ?, output = ?, error = ?, finished_at = ? WHERE id = ?")
            .bind(status)
            .bind(output)
            .bind(error)
            .bind(now())
            .bind(id)
            .execute(&self.core.db)
            .await;
    }

    fn require_consent(user: &User) -> Result<()> {
        if user.ai_consent { Ok(()) } else { Err(Error::Forbidden) }
    }

    /// Runs the assistant for a user. Progress events are pushed to the user's live
    /// connections as `{"type": "ai.event", "run_id", "event"}`.
    pub async fn assistant(&self, user: &User, scopes: &str, history: Vec<ChatTurn>, allow_writes: bool, run_id: Option<String>) -> Result<(String, AgentRun)> {
        Self::require_consent(user).map_err(|_| Error::bad("Turn on the assistant in Settings → AI first (it reads your data only when you ask it to)."))?;
        let (provider, model) = self.for_role("chat").await?;
        let run_id = run_id.unwrap_or_else(new_id);
        let job = self
            .record_job(&user.id, "assistant", &json!({"turns": history.len(), "allow_writes": allow_writes}))
            .await?;
        let mut t = Transcript {
            system: system_prompt(user, allow_writes, ""),
            messages: Vec::new(),
        };
        // Earlier turns are replayed as plain text; only this request's turn uses tools.
        for turn in history.iter().take(history.len().saturating_sub(1)) {
            let role = if turn.role == "assistant" { "assistant" } else { "user" };
            t.messages.push(json!({"role": role, "content": turn.content}));
        }
        let last = history.last().map(|h| h.content.clone()).unwrap_or_default();
        if last.trim().is_empty() {
            return Err(Error::bad("message is empty"));
        }
        provider.push_user(&mut t, &last);
        let toolbox = UserToolBox {
            tools: self.tools.clone(),
            ctx: UserCtx {
                user: user.clone(),
                scopes: scopes.to_string(),
            },
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
        let core = self.core.clone();
        let uid = user.id.clone();
        let rid = run_id.clone();
        let forward = tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                core.hub.publish([uid.as_str()], &json!({"type": "ai.event", "run_id": rid, "event": ev}));
            }
        });
        let opts = AgentOptions {
            model,
            allow_writes,
            ..AgentOptions::default()
        };
        let result = ferrite_ai::run(&provider, &mut t, &toolbox, &opts, Some(tx)).await;
        let _ = forward.await;
        match result {
            Ok(run) => {
                self.finish_job(&job, Ok(&run)).await;
                Ok((run_id, run))
            }
            Err(e) => {
                self.finish_job(&job, Err(e.to_string())).await;
                Err(Error::Unavailable(format!("AI provider: {e}")))
            }
        }
    }

    /// One-shot summary of arbitrary text with the summarize role.
    pub async fn summarize(&self, user: &User, what: &str, text: &str) -> Result<String> {
        Self::require_consent(user).map_err(|_| Error::bad("Turn on the assistant in Settings → AI first."))?;
        let (provider, model) = self.for_role("summarize").await?;
        let prompt = format!(
            "Summarize the following {what}. Lead with the gist in one sentence, then key points, decisions and action items (with owners) as bullet lists.\n\n---\n{text}"
        );
        provider
            .complete(&model, "You write accurate, compact summaries in Markdown.", &prompt, 4000)
            .await
            .map_err(|e| Error::Unavailable(format!("AI provider: {e}")))
    }

    // ---- Talk bot ---------------------------------------------------------------

    /// Answers `@assistant` mentions in conversations that allow AI.
    pub fn spawn_talk_bot(self: &Arc<Self>) {
        let me = self.clone();
        let mut rx = self.core.message_events.subscribe();
        tokio::spawn(async move {
            loop {
                let msg = match rx.recv().await {
                    Ok(m) => m,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                };
                if msg.actor_kind != "user" || msg.kind != "text" || !msg.body.to_lowercase().contains(&format!("@{BOT_ACTOR_ID}")) {
                    continue;
                }
                let me = me.clone();
                tokio::spawn(async move {
                    if let Err(e) = me.answer_mention(&msg).await {
                        tracing::warn!(error = %e, "Talk bot failed");
                    }
                });
            }
        });
    }

    async fn answer_mention(&self, msg: &Message) -> Result<()> {
        let room = self.core.room(&msg.room_id).await?;
        if !room.ai_enabled {
            return Ok(());
        }
        let user = self.core.user(&msg.actor_id).await?;
        let post = |body: String| {
            let core = self.core.clone();
            let token = room.token.clone();
            let reply_to = msg.id;
            async move {
                core.post_message(
                    &Actor::bot(),
                    &token,
                    NewMessage {
                        body,
                        reply_to: Some(reply_to),
                        ..Default::default()
                    },
                )
                .await
            }
        };
        if !user.ai_consent {
            post(format!("@{} I can help once you turn on the assistant in Settings → AI.", user.username)).await?;
            return Ok(());
        }
        let _ = self.core.typing(&Actor::bot(), &room.token).await;
        // Context: the recent conversation, as the asking user sees it.
        let actor = Actor::user(&user.id, &user.display_name);
        let recent = self.core.list_messages(&actor, &room.token, Some(msg.id), None, 30).await?;
        let transcript = recent
            .iter()
            .filter(|m| !m.message.deleted && m.message.kind != "system")
            .map(|m| format!("{}: {}", m.message.actor_name, m.message.body))
            .collect::<Vec<_>>()
            .join("\n");
        let room_name = self.core.room_display_name(&room, &user.id).await?;
        let extra = format!(
            "You were mentioned in the conversation \"{room_name}\" (token {}). Recent messages before the mention:\n{transcript}\n\n\
             Reply to the mention directly; your answer is posted in the conversation. Read-only tools only.",
            room.token
        );
        let (provider, model) = match self.for_role("chat").await {
            Ok(p) => p,
            Err(e) => {
                post(e.to_string()).await?;
                return Ok(());
            }
        };
        let mut t = Transcript {
            system: system_prompt(&user, false, &extra),
            messages: Vec::new(),
        };
        provider.push_user(&mut t, &format!("{}: {}", user.display_name, msg.body));
        let toolbox = UserToolBox {
            tools: self.tools.clone(),
            ctx: UserCtx {
                user: user.clone(),
                scopes: "*".into(),
            },
        };
        let job = self.record_job(&user.id, "talk_bot", &json!({"room": room.token, "message": msg.id})).await?;
        let opts = AgentOptions {
            model,
            allow_writes: false,
            ..AgentOptions::default()
        };
        match ferrite_ai::run(&provider, &mut t, &toolbox, &opts, None).await {
            Ok(run) => {
                self.finish_job(&job, Ok(&run)).await;
                post(run.answer).await?;
            }
            Err(e) => {
                self.finish_job(&job, Err(e.to_string())).await;
                post(format!("Sorry, I couldn't answer: {e}")).await?;
            }
        }
        Ok(())
    }
}
