//! Webmail API: accounts, folders, messages, actions, search and composing. Everything
//! requires the `mail` scope and only ever touches the caller's own accounts.

use super::files::{content_disposition, inline_safe};
use crate::AppState;
use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::Response;
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use ferrite_core::Error;
use ferrite_core::mail::AccountInput;
use ferrite_core::mail::compose::{Compose, ReplyMode};
use ferrite_core::mail::store::FlagAction;
use serde::Deserialize;
use serde_json::{Value, json};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/mail/accounts", get(accounts).post(create_account))
        .route("/api/mail/accounts/{id}", patch(update_account).delete(delete_account))
        .route("/api/mail/accounts/{id}/folders", get(folders))
        .route("/api/mail/accounts/{id}/sync", post(sync))
        .route("/api/mail/accounts/{id}/send", post(send))
        .route("/api/mail/accounts/{id}/drafts", post(save_draft))
        .route("/api/mail/autoconfig", get(autoconfig))
        .route("/api/mail/folders/{id}/messages", get(messages))
        .route("/api/mail/messages/{id}", get(message))
        .route("/api/mail/messages/{id}/html", get(message_html))
        .route("/api/mail/messages/{id}/thread", get(thread))
        .route("/api/mail/messages/{id}/compose", get(compose_template))
        .route("/api/mail/messages/{id}/raw", get(raw))
        .route("/api/mail/messages/{id}/attachments/{index}", get(attachment))
        .route("/api/mail/messages/{id}/attachments/{index}/save", post(save_attachment))
        .route("/api/mail/messages/{id}/cid/{cid}", get(inline_part))
        .route("/api/mail/actions", post(actions))
        .route("/api/mail/search", get(search))
        .route("/api/mail/recipients", get(recipients))
        .route("/api/mail/uploads", post(upload))
        .route("/api/mail/uploads/{id}", delete(discard_upload))
}

fn ok() -> Json<Value> {
    Json(json!({"ok": true}))
}

fn scope(auth: &Auth) -> Result<(), ApiError> {
    auth.require_scope("mail")
}

async fn accounts(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    let accounts = s.core.mail_accounts(&auth.user.id).await?;
    // Opening mail is the moment to start watching for new mail.
    s.core.ensure_mail_idle(&auth.user.id).await;
    Ok(Json(json!({"accounts": accounts})))
}

async fn create_account(State(s): State<AppState>, auth: Auth, Json(input): Json<AccountInput>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"account": s.core.create_mail_account(&auth.user.id, &input).await?})))
}

async fn update_account(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(input): Json<AccountInput>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"account": s.core.update_mail_account(&auth.user.id, &id, &input).await?})))
}

async fn delete_account(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    s.core.delete_mail_account(&auth.user.id, &id).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct EmailQ {
    email: String,
}

async fn autoconfig(auth: Auth, Query(q): Query<EmailQ>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"config": ferrite_core::mail::autoconfig::lookup(&q.email).await})))
}

async fn folders(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"folders": s.core.mail_folders(&auth.user.id, &id).await?})))
}

#[derive(Deserialize, Default)]
struct SyncBody {
    folder_id: Option<String>,
}

/// Refreshes the folder list and (optionally) one folder now.
async fn sync(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, body: Option<Json<SyncBody>>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    let a = s.core.mail_account(&auth.user.id, &id).await?;
    s.core.sync_mail_folders(&a).await?;
    if let Some(folder) = body.and_then(|b| b.0.folder_id) {
        s.core.sync_mail_folder(&a, &folder).await?;
    }
    Ok(Json(json!({"folders": s.core.mail_folders(&auth.user.id, &id).await?})))
}

#[derive(Deserialize)]
struct ListQ {
    before: Option<String>,
    #[serde(default)]
    unread: bool,
    limit: Option<i64>,
}

async fn messages(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Query(q): Query<ListQ>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    let page = s
        .core
        .mail_messages(&auth.user.id, &id, q.before.as_deref(), q.unread, q.limit.unwrap_or(50))
        .await?;
    Ok(Json(json!(page)))
}

async fn message(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"message": s.core.mail_message(&auth.user.id, &id).await?})))
}

#[derive(Deserialize)]
struct HtmlQ {
    #[serde(default)]
    remote: bool,
}

/// The sanitized HTML of a message as a document for a sandboxed iframe. Its own CSP allows
/// no scripts, forms or plugins, and remote images only when asked for.
async fn message_html(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Query(q): Query<HtmlQ>) -> ApiResult<Response> {
    scope(&auth)?;
    let doc = s.core.mail_message_html(&auth.user.id, &id, q.remote).await?;
    let img = if q.remote { "'self' data: https: http:" } else { "'self' data:" };
    let csp = format!(
        "default-src 'none'; img-src {img}; style-src 'unsafe-inline'; font-src data:; frame-ancestors 'self'; base-uri 'none'; form-action 'none'; \
         sandbox allow-same-origin allow-popups allow-popups-to-escape-sandbox"
    );
    let mut resp = Response::new(Body::from(doc));
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_str(&csp).map_err(Error::internal)?);
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    Ok(resp)
}

async fn thread(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"messages": s.core.mail_thread(&auth.user.id, &id).await?})))
}

#[derive(Deserialize)]
struct ComposeQ {
    mode: ReplyMode,
}

async fn compose_template(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Query(q): Query<ComposeQ>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"compose": s.core.mail_compose_template(&auth.user.id, &id, q.mode).await?})))
}

/// A downloaded part, always as an attachment unless it is a type safe to show inline.
fn file_response(name: &str, mime: &str, data: Vec<u8>, inline: bool) -> ApiResult<Response> {
    let inline = inline && inline_safe(mime);
    let ct = if inline { mime } else { "application/octet-stream" };
    let mut resp = Response::new(Body::from(data));
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(ct).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    h.insert(header::CONTENT_DISPOSITION, content_disposition(name, inline));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; img-src 'self'; sandbox"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=3600"));
    Ok(resp)
}

#[derive(Deserialize)]
struct InlineQ {
    #[serde(default)]
    inline: bool,
}

async fn attachment(State(s): State<AppState>, auth: Auth, Path((id, index)): Path<(String, usize)>, Query(q): Query<InlineQ>) -> ApiResult<Response> {
    scope(&auth)?;
    let (name, mime, data) = s.core.mail_attachment(&auth.user.id, &id, index).await?;
    file_response(&name, &mime, data, q.inline)
}

async fn inline_part(State(s): State<AppState>, auth: Auth, Path((id, cid)): Path<(String, String)>) -> ApiResult<Response> {
    scope(&auth)?;
    let (name, mime, data) = s.core.mail_inline_part(&auth.user.id, &id, &cid).await?;
    file_response(&name, &mime, data, true)
}

async fn raw(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Response> {
    scope(&auth)?;
    let (m, data) = s.core.mail_raw(&auth.user.id, &id).await?;
    let base: String = m.subject.chars().filter(|c| !"/\\\0".contains(*c)).take(80).collect();
    let name = if base.trim().is_empty() {
        "message.eml".to_string()
    } else {
        format!("{}.eml", base.trim())
    };
    file_response(&name, "message/rfc822", data, false)
}

#[derive(Deserialize)]
struct SaveBody {
    folder_id: String,
}

async fn save_attachment(State(s): State<AppState>, auth: Auth, Path((id, index)): Path<(String, usize)>, Json(b): Json<SaveBody>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    auth.require_scope("files")?;
    let node = s.core.mail_save_attachment(&auth.user.id, &id, index, &b.folder_id).await?;
    Ok(Json(json!({"node": node})))
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ActionKind {
    Read,
    Unread,
    Flag,
    Unflag,
    Move,
    Archive,
    Delete,
}

#[derive(Deserialize)]
struct ActionBody {
    ids: Vec<String>,
    action: ActionKind,
    folder_id: Option<String>,
}

async fn actions(State(s): State<AppState>, auth: Auth, Json(b): Json<ActionBody>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    if b.ids.is_empty() {
        return Ok(ok());
    }
    if b.ids.len() > 1000 {
        return Err(ApiError(Error::bad("at most 1000 messages at once")));
    }
    let uid = &auth.user.id;
    match b.action {
        ActionKind::Read => s.core.mail_set_flags(uid, &b.ids, FlagAction::Read).await?,
        ActionKind::Unread => s.core.mail_set_flags(uid, &b.ids, FlagAction::Unread).await?,
        ActionKind::Flag => s.core.mail_set_flags(uid, &b.ids, FlagAction::Flag).await?,
        ActionKind::Unflag => s.core.mail_set_flags(uid, &b.ids, FlagAction::Unflag).await?,
        ActionKind::Archive => s.core.mail_archive(uid, &b.ids).await?,
        ActionKind::Delete => s.core.mail_delete(uid, &b.ids).await?,
        ActionKind::Move => {
            let dest = b.folder_id.ok_or_else(|| ApiError(Error::bad("folder_id is required to move")))?;
            s.core.mail_move(uid, &b.ids, &dest).await?
        }
    }
    Ok(ok())
}

#[derive(Deserialize)]
struct SearchQ {
    q: String,
    account_id: Option<String>,
    folder_id: Option<String>,
}

async fn search(State(s): State<AppState>, auth: Auth, Query(q): Query<SearchQ>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    let results = s
        .core
        .mail_search(&auth.user.id, &q.q, q.account_id.as_deref(), q.folder_id.as_deref(), 100)
        .await?;
    Ok(Json(json!({"messages": results})))
}

#[derive(Deserialize)]
struct RecipientsQ {
    #[serde(default)]
    q: String,
}

async fn recipients(State(s): State<AppState>, auth: Auth, Query(q): Query<RecipientsQ>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"recipients": s.core.mail_recipients(&auth.user.id, &q.q).await?})))
}

#[derive(Deserialize)]
struct UploadQ {
    name: String,
}

async fn upload(State(s): State<AppState>, auth: Auth, Query(q): Query<UploadQ>, headers: HeaderMap, body: Bytes) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    let mime = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or("").trim().to_string())
        .unwrap_or_default();
    let up = s.core.mail_upload(&auth.user.id, &q.name, &mime, body.to_vec()).await?;
    Ok(Json(json!({"upload": up})))
}

async fn discard_upload(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    s.core.mail_discard_upload(&auth.user.id, &id).await?;
    Ok(ok())
}

async fn send(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(c): Json<Compose>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    let message_id = s.core.mail_send(&auth.user.id, &id, &c).await?;
    Ok(Json(json!({"message_id": message_id})))
}

async fn save_draft(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(c): Json<Compose>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"id": s.core.mail_save_draft(&auth.user.id, &id, &c).await?})))
}
