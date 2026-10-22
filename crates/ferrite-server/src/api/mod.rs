//! JSON API used by the web UI (and scripts with app tokens).

pub mod boards;
pub mod calendar_sharing;
pub mod files;
pub mod groupware;
pub mod mail;
pub mod push;
pub mod security;
pub mod talk;

use crate::AppState;
use crate::ai::{ChatTurn, NewProvider};
use crate::auth::{Auth, MaybeAuth, session_cookie};
use crate::error::{ApiError, ApiResult};
use crate::external::NewMcpServer;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use ferrite_core::Error;
use ferrite_core::auth::{NewUser, SESSION_TTL_DAYS, UserUpdate};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/status", get(status))
        .route("/api/setup", post(setup))
        .route("/api/auth/login", post(login))
        .route("/api/auth/register", post(register))
        .route("/api/auth/logout", post(logout))
        .route("/api/me", get(me).patch(update_me))
        .route("/api/me/tokens", get(list_tokens).post(create_token))
        .route("/api/me/tokens/{id}", delete(revoke_token))
        .route("/api/users", get(directory))
        .route("/api/admin/users", get(admin_users).post(admin_create_user))
        .route("/api/admin/users/{id}", patch(admin_update_user).delete(admin_delete_user))
        .route("/api/admin/groups", get(list_groups).post(create_group))
        .route("/api/admin/groups/{id}", delete(delete_group))
        .route("/api/admin/groups/{id}/members", get(group_members).post(add_member))
        .route("/api/admin/groups/{id}/members/{user}", delete(remove_member))
        .route("/api/admin/stats", get(stats))
        .route("/api/search", get(search))
        .route("/api/notifications", get(notifications))
        .route("/api/notifications/read", post(read_notifications))
        .route("/api/activity", get(activity))
        .route("/api/craft/apps", get(craft_apps))
        .route("/api/ai/status", get(ai_status))
        .route("/api/ai/providers", get(ai_providers).post(ai_add_provider))
        .route("/api/ai/providers/{id}", delete(ai_delete_provider))
        .route("/api/ai/providers/{id}/models", post(ai_models))
        .route("/api/ai/roles", get(ai_roles).put(ai_set_role))
        .route("/api/ai/chat", post(ai_chat))
        .route("/api/ai/summarize", post(ai_summarize))
        .route("/api/ai/jobs", get(ai_jobs))
        .route("/api/mcp/servers", get(mcp_servers).post(mcp_add_server))
        .route("/api/mcp/servers/{id}", delete(mcp_delete_server))
        .route("/api/mcp/servers/{id}/probe", post(mcp_probe_server))
        .route("/api/mcp/tools", get(mcp_tools))
        .route("/api/config", get(client_config))
        .merge(files::router())
        .merge(talk::router())
        .merge(boards::router())
        .merge(groupware::router())
        .merge(security::router())
        .merge(push::router())
        .merge(calendar_sharing::router())
        .merge(mail::router())
        .route("/api/{*rest}", get(not_found).post(not_found).put(not_found).patch(not_found).delete(not_found))
}

async fn not_found() -> ApiError {
    ApiError(Error::NotFound)
}

fn ok() -> Json<Value> {
    Json(json!({"ok": true}))
}

// ---- session ------------------------------------------------------------------

async fn status(State(s): State<AppState>, MaybeAuth(auth): MaybeAuth) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "signed_in": auth.is_some(),
        "product": "Ferrite",
        "version": env!("CARGO_PKG_VERSION"),
        "setup_needed": s.core.user_count().await? == 0,
        "open_registration": s.core.config.open_registration,
        "sso": crate::oidc::public_providers(&s),
    })))
}

#[derive(Deserialize)]
struct Credentials {
    username: String,
    password: String,
    display_name: Option<String>,
    email: Option<String>,
}

pub(crate) async fn start_session(s: &AppState, user_id: &str, headers: &HeaderMap) -> ApiResult<(String, String)> {
    let agent = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("browser");
    let name = agent.chars().take(120).collect::<String>();
    let (_, secret) = s
        .core
        .create_token(user_id, "session", &name, "*", Some(chrono::Duration::days(SESSION_TTL_DAYS)))
        .await?;
    let cookie = session_cookie(&secret, s.core.config.secure_cookies, SESSION_TTL_DAYS * 86400);
    Ok((secret, cookie))
}

async fn signed_in(s: &AppState, user_id: &str, headers: &HeaderMap) -> ApiResult<Response> {
    let (_, cookie) = start_session(s, user_id, headers).await?;
    let user = s.core.user(user_id).await?;
    Ok(([(header::SET_COOKIE, cookie)], Json(json!({"user": user}))).into_response())
}

async fn setup(State(s): State<AppState>, headers: HeaderMap, Json(c): Json<Credentials>) -> ApiResult<Response> {
    if s.core.user_count().await? > 0 {
        return Err(ApiError(Error::Forbidden));
    }
    let u = s
        .core
        .create_user(NewUser {
            username: c.username,
            display_name: c.display_name,
            email: c.email,
            password: c.password,
            is_admin: true,
            quota_bytes: None,
        })
        .await?;
    security::password_signed_in(&s, &u, &headers).await
}

async fn register(State(s): State<AppState>, headers: HeaderMap, Json(c): Json<Credentials>) -> ApiResult<Response> {
    if !s.core.config.open_registration {
        return Err(ApiError(Error::Forbidden));
    }
    let u = s
        .core
        .create_user(NewUser {
            username: c.username,
            display_name: c.display_name,
            email: c.email,
            password: c.password,
            is_admin: false,
            quota_bytes: None,
        })
        .await?;
    security::password_signed_in(&s, &u, &headers).await
}

async fn login(State(s): State<AppState>, headers: HeaderMap, Json(c): Json<Credentials>) -> ApiResult<Response> {
    let key = format!("login:{}", c.username.to_lowercase());
    if s.throttle.blocked(&key) {
        return Err(ApiError(Error::Unavailable("too many failed attempts; try again in a few minutes".into())));
    }
    match s.core.authenticate(&c.username, &c.password).await {
        Ok(u) => {
            s.throttle.succeed(&key);
            security::password_signed_in(&s, &u, &headers).await
        }
        Err(e) => {
            s.throttle.fail(&key);
            Err(e.into())
        }
    }
}

async fn logout(State(s): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(secret) = crate::auth::cookie(&headers, crate::auth::SESSION_COOKIE) {
        s.core.revoke_secret(&secret).await?;
    }
    Ok(([(header::SET_COOKIE, session_cookie("", s.core.config.secure_cookies, 0))], ok()).into_response())
}

async fn me(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    let quota = s.core.quota(&auth.user.id).await?;
    let groups = s.core.user_groups(&auth.user.id).await?;
    let root = s.core.ensure_root(&auth.user.id).await?;
    Ok(Json(json!({"user": auth.user, "quota": quota, "groups": groups, "root_id": root})))
}

#[derive(Deserialize)]
struct MeUpdate {
    display_name: Option<String>,
    email: Option<String>,
    password: Option<String>,
    current_password: Option<String>,
    ai_consent: Option<bool>,
}

async fn update_me(State(s): State<AppState>, auth: Auth, Json(u): Json<MeUpdate>) -> ApiResult<Json<Value>> {
    if u.password.is_some() {
        let cur = u.current_password.as_deref().unwrap_or("");
        s.core
            .authenticate(&auth.user.username, cur)
            .await
            .map_err(|_| ApiError(Error::bad("current password is wrong")))?;
    }
    let user = s
        .core
        .update_user(
            &auth.user.id,
            UserUpdate {
                display_name: u.display_name,
                email: u.email,
                password: u.password,
                ai_consent: u.ai_consent,
                ..Default::default()
            },
        )
        .await?;
    Ok(Json(json!({"user": user})))
}

async fn list_tokens(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"tokens": s.core.list_tokens(&auth.user.id).await?})))
}

#[derive(Deserialize)]
struct NewToken {
    name: String,
    #[serde(default)]
    scopes: Vec<String>,
    expires_in_days: Option<i64>,
}

async fn create_token(State(s): State<AppState>, auth: Auth, Json(t): Json<NewToken>) -> ApiResult<Json<Value>> {
    if !auth.via_cookie {
        // Tokens mint tokens only from an interactive session.
        return Err(ApiError(Error::Forbidden));
    }
    const KNOWN: &[&str] = &["*", "files", "talk", "boards", "docs", "groupware", "mail", "craft", "ai", "mcp", "admin"];
    if let Some(bad) = t.scopes.iter().find(|x| !KNOWN.contains(&x.as_str())) {
        return Err(ApiError(Error::bad(format!("unknown scope {bad}"))));
    }
    if t.scopes.iter().any(|x| x == "admin") && !auth.user.is_admin {
        return Err(ApiError(Error::Forbidden));
    }
    let scopes = if t.scopes.is_empty() { "*".to_string() } else { t.scopes.join(",") };
    let ttl = t.expires_in_days.map(|d| chrono::Duration::days(d.clamp(1, 3650)));
    let (info, secret) = s.core.create_token(&auth.user.id, "app", t.name.trim(), &scopes, ttl).await?;
    Ok(Json(json!({"token": info, "secret": secret})))
}

async fn revoke_token(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    s.core.revoke_token(&auth.user.id, &id).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct Q {
    q: Option<String>,
    kinds: Option<String>,
    before: Option<i64>,
    unread: Option<bool>,
}

async fn directory(State(s): State<AppState>, _auth: Auth, Query(q): Query<Q>) -> ApiResult<Json<Value>> {
    let users: Vec<Value> = s
        .core
        .list_users(q.q.as_deref(), 50)
        .await?
        .into_iter()
        .filter(|u| !u.disabled)
        .map(|u| json!({"id": u.id, "username": u.username, "display_name": u.display_name, "online": s.core.hub.is_online(&u.id)}))
        .collect();
    Ok(Json(json!({"users": users})))
}

// ---- administration -------------------------------------------------------------

async fn admin_users(State(s): State<AppState>, auth: Auth, Query(q): Query<Q>) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    let mut out = Vec::new();
    let two_factor = s.core.two_factor_users().await?;
    for u in s.core.list_users(q.q.as_deref(), 1000).await? {
        let quota = s.core.quota(&u.id).await?;
        out.push(json!({"user": u, "quota": quota, "two_factor": two_factor.contains(&u.id)}));
    }
    Ok(Json(json!({"users": out})))
}

async fn admin_create_user(State(s): State<AppState>, auth: Auth, Json(n): Json<NewUser>) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    Ok(Json(json!({"user": s.core.create_user(n).await?})))
}

async fn admin_update_user(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(u): Json<UserUpdate>) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    if id == auth.user.id && (u.is_admin == Some(false) || u.disabled == Some(true)) {
        return Err(ApiError(Error::bad("you cannot remove your own admin rights or disable yourself")));
    }
    Ok(Json(json!({"user": s.core.update_user(&id, u).await?})))
}

async fn admin_delete_user(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    if id == auth.user.id {
        return Err(ApiError(Error::bad("you cannot delete yourself")));
    }
    s.core.delete_user(&id).await?;
    Ok(ok())
}

async fn list_groups(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    // Everyone may see group names (for sharing); only admins change them.
    let _ = auth;
    Ok(Json(json!({"groups": s.core.list_groups().await?})))
}

#[derive(Deserialize)]
struct NameBody {
    name: String,
}

async fn create_group(State(s): State<AppState>, auth: Auth, Json(b): Json<NameBody>) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    Ok(Json(json!({"group": s.core.create_group(&b.name).await?})))
}

async fn delete_group(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    s.core.delete_group(&id).await?;
    Ok(ok())
}

async fn group_members(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    Ok(Json(json!({"members": s.core.group_members(&id).await?})))
}

#[derive(Deserialize)]
struct UserRef {
    username: String,
}

async fn add_member(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<UserRef>) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    let u = s.core.user_by_name(&b.username).await?;
    s.core.add_to_group(&id, &u.id).await?;
    Ok(ok())
}

async fn remove_member(State(s): State<AppState>, auth: Auth, Path((id, user)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    s.core.remove_from_group(&id, &user).await?;
    Ok(ok())
}

async fn stats(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    let count = |sql: &'static str| {
        let db = s.core.db.clone();
        async move { sqlx::query_scalar::<_, i64>(sql).fetch_one(&db).await.unwrap_or(0) }
    };
    Ok(Json(json!({
        "users": count("SELECT COUNT(*) FROM users").await,
        "files": count("SELECT COUNT(*) FROM nodes WHERE kind = 'file' AND trashed_at IS NULL").await,
        "stored_bytes": count("SELECT COALESCE(SUM(size), 0) FROM blobs").await,
        "logical_bytes": count("SELECT COALESCE(SUM(size), 0) FROM nodes WHERE kind = 'file'").await,
        "rooms": count("SELECT COUNT(*) FROM rooms").await,
        "messages": count("SELECT COUNT(*) FROM messages").await,
        "boards": count("SELECT COUNT(*) FROM boards").await,
        "online_users": s.core.hub.online_count(),
        "live_documents": s.core.collab.active().len(),
        "rss_bytes": crate::rss_bytes(),
        "sfu": s.sfu.as_ref().map(|x| x.stats()),
    })))
}

// ---- search, notifications, activity -------------------------------------------

async fn search(State(s): State<AppState>, auth: Auth, Query(q): Query<Q>) -> ApiResult<Json<Value>> {
    let kinds: Vec<String> = q.kinds.as_deref().map(|k| k.split(',').map(String::from).collect()).unwrap_or_default();
    let kinds_ref: Vec<&str> = kinds.iter().map(String::as_str).collect();
    let (hits, semantic) = s
        .tools
        .semantic
        .search(
            &auth.user.id,
            q.q.as_deref().unwrap_or(""),
            if kinds.is_empty() { None } else { Some(&kinds_ref) },
            50,
        )
        .await?;
    Ok(Json(json!({"results": hits, "semantic": semantic})))
}

async fn notifications(State(s): State<AppState>, auth: Auth, Query(q): Query<Q>) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({"notifications": s.core.notifications(&auth.user.id, q.unread.unwrap_or(false), 100).await?}),
    ))
}

#[derive(Deserialize)]
struct ReadBody {
    id: Option<i64>,
}

async fn read_notifications(State(s): State<AppState>, auth: Auth, Json(b): Json<ReadBody>) -> ApiResult<Json<Value>> {
    s.core.mark_notifications_read(&auth.user.id, b.id).await?;
    Ok(ok())
}

async fn activity(State(s): State<AppState>, auth: Auth, Query(q): Query<Q>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"activity": s.core.activity(&auth.user.id, q.before, 100).await?})))
}

async fn craft_apps(State(s): State<AppState>, _auth: Auth) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"apps": s.tools.craft.availability()})))
}

async fn client_config(State(s): State<AppState>, _auth: Auth) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "ice_servers": s.core.config.ice_servers,
        "public_url": s.core.config.public_url,
        "version": env!("CARGO_PKG_VERSION"),
        "mcp_url": format!("{}/mcp", s.core.config.public_url),
        "sfu": {"enabled": s.sfu.is_some()},
        "sso": crate::oidc::public_providers(&s),
    })))
}

// ---- AI -------------------------------------------------------------------------

async fn ai_status(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    let mut v = s.ai.status().await?;
    v["consent"] = json!(auth.user.ai_consent);
    v["roles_available"] = json!(crate::ai::ROLES.iter().map(|(r, d)| json!({"role": r, "description": d})).collect::<Vec<_>>());
    if !auth.user.is_admin {
        v.as_object_mut().map(|o| o.remove("roles"));
    }
    Ok(Json(v))
}

async fn ai_providers(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    let rows: Vec<Value> = s
        .ai
        .providers()
        .await?
        .into_iter()
        .map(|p| json!({"id": p.id, "name": p.name, "kind": p.kind, "base_url": p.base_url, "has_key": p.api_key.is_some(), "models": serde_json::from_str::<Value>(&p.models).unwrap_or(json!([]))}))
        .collect();
    Ok(Json(json!({"providers": rows})))
}

async fn ai_add_provider(State(s): State<AppState>, auth: Auth, Json(p): Json<NewProvider>) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    let row = s.ai.add_provider(p).await?;
    Ok(Json(json!({"id": row.id})))
}

async fn ai_delete_provider(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    s.ai.delete_provider(&id).await?;
    Ok(ok())
}

async fn ai_models(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    Ok(Json(json!({"models": s.ai.refresh_models(&id).await?})))
}

async fn ai_roles(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    Ok(Json(json!({"roles": s.ai.roles().await?})))
}

#[derive(Deserialize)]
struct RoleBody {
    role: String,
    provider_id: String,
    model: String,
}

async fn ai_set_role(State(s): State<AppState>, auth: Auth, Json(b): Json<RoleBody>) -> ApiResult<Json<Value>> {
    auth.require_admin()?;
    s.ai.set_role(&b.role, &b.provider_id, &b.model).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct ChatBody {
    messages: Vec<ChatTurn>,
    #[serde(default)]
    allow_writes: bool,
    run_id: Option<String>,
}

async fn ai_chat(State(s): State<AppState>, auth: Auth, Json(b): Json<ChatBody>) -> ApiResult<Json<Value>> {
    auth.require_scope("ai")?;
    let (run_id, run) = s.ai.assistant(&auth.user, &auth.token.scopes, b.messages, b.allow_writes, b.run_id).await?;
    Ok(Json(json!({"run_id": run_id, "answer": run.answer, "trace": run.trace, "steps": run.steps})))
}

#[derive(Deserialize)]
struct SummarizeBody {
    /// `room` (token) or `file` (id)
    kind: String,
    target: String,
}

async fn ai_summarize(State(s): State<AppState>, auth: Auth, Json(b): Json<SummarizeBody>) -> ApiResult<Json<Value>> {
    auth.require_scope("ai")?;
    let (what, text) = match b.kind.as_str() {
        "room" => {
            let actor = ferrite_core::talk::Actor::user(&auth.user.id, &auth.user.display_name);
            let msgs = s.core.list_messages(&actor, &b.target, None, None, 200).await?;
            let t = msgs
                .iter()
                .filter(|m| !m.message.deleted && m.message.kind != "system")
                .map(|m| format!("{}: {}", m.message.actor_name, m.message.body))
                .collect::<Vec<_>>()
                .join("\n");
            ("chat conversation", t)
        }
        "file" => {
            let data = s
                .core
                .collab_content(&auth.user.id, &b.target)
                .await
                .map_err(|_| Error::bad("only text documents can be summarized"))?;
            (
                "document",
                String::from_utf8(data).map_err(|_| ApiError(Error::bad("only text documents can be summarized")))?,
            )
        }
        _ => return Err(ApiError(Error::bad("kind must be room or file"))),
    };
    if text.trim().is_empty() {
        return Err(ApiError(Error::bad("nothing to summarize yet")));
    }
    let max = 120_000;
    let text = if text.len() > max { text[text.len() - max..].to_string() } else { text };
    Ok(Json(json!({"summary": s.ai.summarize(&auth.user, what, &text).await?})))
}

#[derive(serde::Serialize, sqlx::FromRow)]
struct JobRow {
    id: String,
    kind: String,
    status: String,
    error: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    finished_at: Option<chrono::DateTime<chrono::Utc>>,
}

async fn ai_jobs(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    let jobs: Vec<JobRow> =
        sqlx::query_as("SELECT id, kind, status, error, created_at, finished_at FROM ai_jobs WHERE user_id = ? ORDER BY created_at DESC LIMIT 50")
            .bind(&auth.user.id)
            .fetch_all(&s.core.db)
            .await
            .map_err(Error::from)?;
    Ok(Json(json!({"jobs": jobs})))
}

// ---- external MCP servers ----------------------------------------------------------

async fn mcp_servers(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"servers": s.tools.external.list(&auth.user.id).await?})))
}

async fn mcp_add_server(State(s): State<AppState>, auth: Auth, Json(n): Json<NewMcpServer>) -> ApiResult<Json<Value>> {
    let row = s.tools.external.add(&auth.user.id, auth.user.is_admin, n).await?;
    Ok(Json(json!({"server": row})))
}

async fn mcp_delete_server(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    s.tools.external.remove(&auth.user.id, auth.user.is_admin, &id).await?;
    Ok(ok())
}

async fn mcp_probe_server(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let tools = s.tools.external.probe(&auth.user.id, &id).await?;
    Ok(Json(json!({"tools": tools})))
}

/// Ferrite's own MCP tools (for the settings page and for documentation).
async fn mcp_tools(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    let ctx = crate::tools::UserCtx {
        user: auth.user.clone(),
        scopes: auth.token.scopes.clone(),
    };
    Ok(Json(json!({"tools": s.tools.list(&ctx)})))
}
