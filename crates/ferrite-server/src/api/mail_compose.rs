//! Webmail composing API: composer context (identities, signatures, settings), aliases, text
//! blocks, the outbox (queue, schedule, undo, edit, send now), recipient details, large
//! attachments as share links, read receipts, attached messages and printing. Everything
//! requires the `mail` scope and only touches the caller's own accounts; the attachment size
//! limit comes from the admins' mail settings.

use super::mail::file_response;
use crate::AppState;
use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, header};
use axum::response::Response;
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use ferrite_core::Error;
use ferrite_core::mail::aliases::{AliasInput, ComposePrefsInput};
use ferrite_core::mail::compose::{Compose, LINK_DAYS, MAX_ATTACHMENTS_BYTES};
use ferrite_core::mail::textblocks::TextBlockInput;
use futures::TryStreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/mail/compose/context", get(context))
        .route("/api/mail/compose/recipient-info", get(recipient_info))
        .route("/api/mail/compose/large-attachment", put(large_attachment))
        .route("/api/mail/compose/share-link", post(share_link))
        .route("/api/mail/accounts/{id}/compose-settings", put(set_compose_settings))
        .route("/api/mail/accounts/{id}/aliases", get(aliases).post(create_alias))
        .route("/api/mail/aliases/{id}", patch(update_alias).delete(delete_alias))
        .route("/api/mail/send-settings", put(set_send_settings))
        .route("/api/mail/text-blocks", get(text_blocks).post(create_text_block))
        .route("/api/mail/text-blocks/{id}", patch(update_text_block).delete(delete_text_block))
        .route("/api/mail/text-blocks/{id}/shares", post(share_text_block))
        .route("/api/mail/text-blocks/{id}/shares/{kind}/{target}", delete(unshare_text_block))
        .route("/api/mail/uploads/{id}/content", get(upload_content))
        .route("/api/mail/outbox", get(outbox).post(enqueue))
        .route("/api/mail/outbox/{id}", get(outbox_message).put(update_outbox).delete(delete_outbox))
        .route("/api/mail/outbox/{id}/send", post(send_now))
        .route("/api/mail/outbox/{id}/stop", post(stop))
        .route("/api/mail/outbox/{id}/files/{file}", get(outbox_file))
        .route("/api/mail/messages/{id}/mdn", get(mdn_status).post(mdn_respond))
        .route("/api/mail/messages/{id}/print", get(print))
        .route("/api/mail/messages/{id}/attachments/{index}/message", get(nested))
        .route("/api/mail/messages/{id}/attachments/{index}/message/html", get(nested_html))
        .route("/api/mail/messages/{id}/attachments/{index}/parts/{part}", get(nested_part))
        .route("/api/mail/messages/{id}/attachments/{index}/cid/{cid}", get(nested_cid))
}

fn scope(auth: &Auth) -> Result<(), ApiError> {
    auth.require_scope("mail")
}

fn ok() -> Json<Value> {
    Json(json!({"ok": true}))
}

/// Everything the composer needs when it opens: each account with its composer settings
/// and aliases, the user's sending preferences and the attachment limits.
async fn context(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    let uid = &auth.user.id;
    let mut accounts = Vec::new();
    for a in s.core.mail_accounts(uid).await? {
        let prefs = s.core.mail_compose_prefs(uid, &a.id).await?;
        let mut aliases = Vec::new();
        for al in s.core.mail_aliases(uid, &a.id).await? {
            let ident = s.core.mail_identity(&a, Some(&al.id)).await?;
            aliases.push(json!({
                "id": al.id, "name": al.name, "email": al.email,
                "own_signature": al.signature.is_some(), "signature_html": ident.signature_html, "signature_text": ident.signature_text,
            }));
        }
        accounts.push(json!({"id": a.id, "name": a.name, "email": a.email, "prefs": prefs, "aliases": aliases}));
    }
    Ok(Json(json!({
        "accounts": accounts,
        "send": s.core.mail_send_prefs(uid).await?,
        "undo_choices": ferrite_core::mail::aliases::UNDO_SEND_CHOICES,
        "attachment_limit": s.core.mail_attachment_limit().await?,
        "max_attachments": MAX_ATTACHMENTS_BYTES,
        "link_days": LINK_DAYS,
    })))
}

#[derive(Deserialize)]
struct AddressQ {
    address: String,
}

async fn recipient_info(State(s): State<AppState>, auth: Auth, Query(q): Query<AddressQ>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"info": s.core.mail_recipient_info(&auth.user.id, &q.address).await?})))
}

#[derive(Deserialize)]
struct LargeQ {
    name: String,
    days: Option<i64>,
}

/// Streams a large attachment into Files (`Mail attachments`) and shares it as a link.
async fn large_attachment(State(s): State<AppState>, auth: Auth, Query(q): Query<LargeQ>, req: Request) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    auth.require_scope("files")?;
    let stream = req.into_body().into_data_stream().map_err(std::io::Error::other);
    let reader = tokio_util::io::StreamReader::new(stream);
    let link = s.core.mail_store_large_attachment(&auth.user.id, &q.name, reader, q.days).await?;
    Ok(Json(json!({"link": link})))
}

#[derive(Deserialize)]
struct ShareLinkBody {
    file_id: Option<String>,
    upload_id: Option<String>,
    days: Option<i64>,
}

/// Shares a file from Files, or moves a composer upload there and shares it, as a link.
async fn share_link(State(s): State<AppState>, auth: Auth, Json(b): Json<ShareLinkBody>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    auth.require_scope("files")?;
    let link = match (b.file_id, b.upload_id) {
        (Some(f), _) => s.core.mail_share_link(&auth.user.id, &f, b.days).await?,
        (None, Some(u)) => s.core.mail_upload_to_link(&auth.user.id, &u, b.days).await?,
        (None, None) => return Err(ApiError(Error::bad("file_id or upload_id is required"))),
    };
    Ok(Json(json!({"link": link})))
}

async fn set_compose_settings(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<ComposePrefsInput>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"prefs": s.core.set_mail_compose_prefs(&auth.user.id, &id, &b).await?})))
}

async fn aliases(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"aliases": s.core.mail_aliases(&auth.user.id, &id).await?})))
}

async fn create_alias(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<AliasInput>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"alias": s.core.create_mail_alias(&auth.user.id, &id, &b).await?})))
}

async fn update_alias(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<AliasInput>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"alias": s.core.update_mail_alias(&auth.user.id, &id, &b).await?})))
}

async fn delete_alias(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    s.core.delete_mail_alias(&auth.user.id, &id).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct SendSettings {
    undo_send_seconds: i64,
}

async fn set_send_settings(State(s): State<AppState>, auth: Auth, Json(b): Json<SendSettings>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"send": s.core.set_mail_send_prefs(&auth.user.id, b.undo_send_seconds).await?})))
}

async fn text_blocks(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"blocks": s.core.mail_text_blocks(&auth.user.id).await?})))
}

async fn create_text_block(State(s): State<AppState>, auth: Auth, Json(b): Json<TextBlockInput>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"block": s.core.create_mail_text_block(&auth.user.id, &b).await?})))
}

async fn update_text_block(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<TextBlockInput>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"block": s.core.update_mail_text_block(&auth.user.id, &id, &b).await?})))
}

async fn delete_text_block(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    s.core.delete_mail_text_block(&auth.user.id, &id).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct ShareBody {
    kind: String,
    target: String,
}

async fn share_text_block(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<ShareBody>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(
        json!({"shares": s.core.share_mail_text_block(&auth.user.id, &id, &b.kind, &b.target).await?}),
    ))
}

async fn unshare_text_block(State(s): State<AppState>, auth: Auth, Path((id, kind, target)): Path<(String, String, String)>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(
        json!({"shares": s.core.unshare_mail_text_block(&auth.user.id, &id, &kind, &target).await?}),
    ))
}

/// A composer upload (images shown in the editor).
async fn upload_content(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Response> {
    scope(&auth)?;
    let (name, mime, data) = s.core.mail_upload_content(&auth.user.id, &id).await?;
    file_response(&name, &mime, data, true)
}

async fn outbox(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"messages": s.core.mail_outbox(&auth.user.id).await?})))
}

#[derive(Deserialize)]
struct EnqueueBody {
    account_id: String,
    compose: Compose,
    /// Send later at this time; without it the message goes out after the undo delay.
    send_at: Option<DateTime<Utc>>,
    /// Keep the (edited) message set aside instead of scheduling it.
    #[serde(default)]
    hold: bool,
}

async fn enqueue(State(s): State<AppState>, auth: Auth, Json(b): Json<EnqueueBody>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    let entry = s.core.mail_outbox_enqueue(&auth.user.id, &b.account_id, &b.compose, b.send_at).await?;
    Ok(Json(json!({"message": entry})))
}

async fn outbox_message(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"message": s.core.mail_outbox_message(&auth.user.id, &id).await?})))
}

async fn update_outbox(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<EnqueueBody>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    let entry = if b.hold {
        s.core.mail_outbox_keep(&auth.user.id, &id, &b.account_id, &b.compose).await?
    } else {
        s.core.mail_outbox_update(&auth.user.id, &id, &b.account_id, &b.compose, b.send_at).await?
    };
    Ok(Json(json!({"message": entry})))
}

async fn delete_outbox(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    s.core.mail_outbox_delete(&auth.user.id, &id).await?;
    Ok(ok())
}

async fn send_now(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"message": s.core.mail_outbox_send_now(&auth.user.id, &id).await?})))
}

/// Undo: the message stays in the outbox without a send time.
async fn stop(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"message": s.core.mail_outbox_hold(&auth.user.id, &id).await?})))
}

async fn outbox_file(State(s): State<AppState>, auth: Auth, Path((id, file)): Path<(String, String)>) -> ApiResult<Response> {
    scope(&auth)?;
    let (name, mime, data) = s.core.mail_outbox_file(&auth.user.id, &id, &file).await?;
    file_response(&name, &mime, data, true)
}

async fn mdn_status(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"mdn": s.core.mail_mdn_status(&auth.user.id, &id).await?})))
}

#[derive(Deserialize)]
struct MdnBody {
    send: bool,
}

async fn mdn_respond(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<MdnBody>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"mdn": s.core.mail_mdn_respond(&auth.user.id, &id, b.send).await?})))
}

#[derive(Deserialize)]
struct RemoteQ {
    #[serde(default)]
    remote: bool,
}

/// A script-free HTML document for a sandboxed frame. `modals` lets the parent page call
/// print() on it.
fn document_response(doc: String, remote: bool, modals: bool) -> ApiResult<Response> {
    let img = if remote { "'self' data: https: http:" } else { "'self' data:" };
    let extra = if modals { " allow-modals" } else { "" };
    let csp = format!(
        "default-src 'none'; img-src {img}; style-src 'unsafe-inline'; font-src data:; frame-ancestors 'self'; base-uri 'none'; form-action 'none'; \
         sandbox allow-same-origin allow-popups allow-popups-to-escape-sandbox{extra}"
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

async fn print(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Query(q): Query<RemoteQ>) -> ApiResult<Response> {
    scope(&auth)?;
    let doc = s.core.mail_print_document(&auth.user.id, &id, q.remote).await?;
    document_response(doc, q.remote, true)
}

async fn nested(State(s): State<AppState>, auth: Auth, Path((id, index)): Path<(String, usize)>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"message": s.core.mail_nested_message(&auth.user.id, &id, index).await?})))
}

async fn nested_html(State(s): State<AppState>, auth: Auth, Path((id, index)): Path<(String, usize)>, Query(q): Query<RemoteQ>) -> ApiResult<Response> {
    scope(&auth)?;
    let doc = s.core.mail_nested_html(&auth.user.id, &id, index, q.remote).await?;
    document_response(doc, q.remote, false)
}

#[derive(Deserialize)]
struct InlineQ {
    #[serde(default)]
    inline: bool,
}

async fn nested_part(
    State(s): State<AppState>,
    auth: Auth,
    Path((id, index, part)): Path<(String, usize, usize)>,
    Query(q): Query<InlineQ>,
) -> ApiResult<Response> {
    scope(&auth)?;
    let (name, mime, data) = s.core.mail_nested_part(&auth.user.id, &id, index, part).await?;
    file_response(&name, &mime, data, q.inline)
}

async fn nested_cid(State(s): State<AppState>, auth: Auth, Path((id, index, cid)): Path<(String, usize, String)>) -> ApiResult<Response> {
    scope(&auth)?;
    let (name, mime, data) = s.core.mail_nested_cid(&auth.user.id, &id, index, &cid).await?;
    file_response(&name, &mime, data, true)
}
