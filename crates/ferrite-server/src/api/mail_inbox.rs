//! Webmail inbox management API: preferences (and the admin defaults), tags and important
//! marking, snoozing, quick actions, folder management, unified views, advanced search,
//! Home widgets, deep-link resolution and `mailto:` parsing. Everything requires the `mail`
//! scope and only touches the caller's own accounts.

use crate::AppState;
use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use axum::extract::{Path, Query, State};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use ferrite_core::Error;
use ferrite_core::mail::prefs::{AccountInboxInput, Sort};
use ferrite_core::mail::quick_actions::QuickActionInput;
use ferrite_core::mail::views::{SearchFilter, View};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/mail/preferences", get(prefs).put(set_prefs))
        .route("/api/mail/admin/defaults", get(defaults).put(set_defaults))
        .route("/api/mail/tags", get(tags).post(create_tag))
        .route("/api/mail/tags/{id}", patch(update_tag).delete(delete_tag))
        .route("/api/mail/keywords", post(set_keyword))
        .route("/api/mail/snooze", post(snooze))
        .route("/api/mail/unsnooze", post(unsnooze))
        .route("/api/mail/snoozed", get(snoozed))
        .route("/api/mail/snooze/presets", get(snooze_presets))
        .route("/api/mail/quick-actions", get(quick_actions))
        .route("/api/mail/accounts/{id}/quick-actions", post(create_quick_action))
        .route("/api/mail/quick-actions/{id}", patch(update_quick_action).delete(delete_quick_action))
        .route("/api/mail/quick-actions/{id}/run", post(run_quick_action))
        .route("/api/mail/accounts/{id}/mailboxes", post(create_folder))
        .route("/api/mail/accounts/{id}/inbox-settings", get(inbox_settings).patch(set_inbox_settings))
        .route("/api/mail/accounts/{id}/classifier", post(train_classifier))
        .route("/api/mail/folders/{id}", patch(update_folder).delete(delete_folder))
        .route("/api/mail/folders/{id}/read-all", post(read_all))
        .route("/api/mail/folders/{id}/empty", post(empty_folder))
        .route("/api/mail/folders/{id}/stats", get(folder_stats))
        .route("/api/mail/folders/{id}/repair", post(repair_folder))
        .route("/api/mail/views/{view}/messages", get(view_messages))
        .route("/api/mail/search/advanced", get(search_advanced))
        .route("/api/mail/widgets", get(widgets))
        .route("/api/mail/resolve", get(resolve))
        .route("/api/mail/mailto", get(mailto))
}

fn ok() -> Json<Value> {
    Json(json!({"ok": true}))
}

fn scope(auth: &Auth) -> Result<(), ApiError> {
    auth.require_scope("mail")
}

fn some_ids(ids: &[String]) -> Result<(), ApiError> {
    if ids.len() > 1000 {
        return Err(ApiError(Error::bad("at most 1000 messages at once")));
    }
    Ok(())
}

async fn prefs(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"prefs": s.core.mail_prefs(&auth.user.id).await?})))
}

async fn set_prefs(State(s): State<AppState>, auth: Auth, Json(patch): Json<Value>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"prefs": s.core.set_mail_prefs(&auth.user.id, &patch).await?})))
}

fn admin(auth: &Auth) -> Result<(), ApiError> {
    scope(auth)?;
    if !auth.user.is_admin {
        return Err(ApiError(Error::Forbidden));
    }
    Ok(())
}

async fn defaults(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    admin(&auth)?;
    Ok(Json(json!({"defaults": s.core.mail_defaults().await?})))
}

async fn set_defaults(State(s): State<AppState>, auth: Auth, Json(patch): Json<Value>) -> ApiResult<Json<Value>> {
    admin(&auth)?;
    Ok(Json(json!({"defaults": s.core.set_mail_defaults(&patch).await?})))
}

async fn tags(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"tags": s.core.mail_tags(&auth.user.id).await?})))
}

#[derive(Deserialize)]
struct TagBody {
    name: Option<String>,
    color: Option<String>,
}

async fn create_tag(State(s): State<AppState>, auth: Auth, Json(b): Json<TagBody>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    let tag = s
        .core
        .create_mail_tag(&auth.user.id, b.name.as_deref().unwrap_or(""), b.color.as_deref().unwrap_or("#317ccc"))
        .await?;
    Ok(Json(json!({"tag": tag})))
}

async fn update_tag(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<TagBody>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    let tag = s.core.update_mail_tag(&auth.user.id, &id, b.name.as_deref(), b.color.as_deref()).await?;
    Ok(Json(json!({"tag": tag})))
}

async fn delete_tag(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    s.core.delete_mail_tag(&auth.user.id, &id).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct KeywordBody {
    ids: Vec<String>,
    keyword: String,
    on: bool,
}

/// Adds or removes a tag (an IMAP keyword) on messages; `$label1` is Important.
async fn set_keyword(State(s): State<AppState>, auth: Auth, Json(b): Json<KeywordBody>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    some_ids(&b.ids)?;
    s.core.mail_set_keyword(&auth.user.id, &b.ids, &b.keyword, b.on).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct SnoozeBody {
    ids: Vec<String>,
    until: DateTime<Utc>,
}

async fn snooze(State(s): State<AppState>, auth: Auth, Json(b): Json<SnoozeBody>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    some_ids(&b.ids)?;
    s.core.mail_snooze(&auth.user.id, &b.ids, b.until).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct IdsBody {
    ids: Vec<String>,
}

async fn unsnooze(State(s): State<AppState>, auth: Auth, Json(b): Json<IdsBody>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    some_ids(&b.ids)?;
    s.core.mail_unsnooze(&auth.user.id, &b.ids).await?;
    Ok(ok())
}

async fn snoozed(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"snoozed": s.core.mail_snoozed(&auth.user.id).await?})))
}

#[derive(Deserialize)]
struct TzQ {
    /// Minutes east of UTC (the negated `Date.getTimezoneOffset()`).
    #[serde(default)]
    tz: i32,
}

async fn snooze_presets(auth: Auth, Query(q): Query<TzQ>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    let presets: Vec<Value> = ferrite_core::mail::snooze::presets(ferrite_core::util::now(), q.tz.clamp(-14 * 60, 14 * 60))
        .into_iter()
        .map(|(k, t)| json!({"key": k, "until": t}))
        .collect();
    Ok(Json(json!({"presets": presets})))
}

#[derive(Deserialize)]
struct AccountQ {
    account_id: Option<String>,
}

async fn quick_actions(State(s): State<AppState>, auth: Auth, Query(q): Query<AccountQ>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(
        json!({"quick_actions": s.core.mail_quick_actions(&auth.user.id, q.account_id.as_deref()).await?}),
    ))
}

async fn create_quick_action(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<QuickActionInput>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"quick_action": s.core.create_mail_quick_action(&auth.user.id, &id, &b).await?})))
}

async fn update_quick_action(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<QuickActionInput>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"quick_action": s.core.update_mail_quick_action(&auth.user.id, &id, &b).await?})))
}

async fn delete_quick_action(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    s.core.delete_mail_quick_action(&auth.user.id, &id).await?;
    Ok(ok())
}

async fn run_quick_action(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<IdsBody>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    some_ids(&b.ids)?;
    s.core.run_mail_quick_action(&auth.user.id, &id, &b.ids).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct FolderBody {
    name: Option<String>,
    /// Parent folder id; "" for the top level.
    parent_id: Option<String>,
    subscribed: Option<bool>,
}

async fn create_folder(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<FolderBody>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    let f = s
        .core
        .create_mail_folder(&auth.user.id, &id, b.name.as_deref().unwrap_or(""), b.parent_id.as_deref())
        .await?;
    Ok(Json(json!({"folder": f})))
}

/// Renames or moves a folder, or changes its subscription.
async fn update_folder(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<FolderBody>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    let uid = &auth.user.id;
    let mut f = None;
    if b.name.is_some() || b.parent_id.is_some() {
        f = Some(s.core.rename_mail_folder(uid, &id, b.name.as_deref(), b.parent_id.as_deref()).await?);
    }
    if let Some(on) = b.subscribed {
        f = Some(s.core.subscribe_mail_folder(uid, &id, on).await?);
    }
    let f = match f {
        Some(f) => f,
        None => s.core.mail_folder(uid, &id).await?.1,
    };
    Ok(Json(json!({"folder": f})))
}

async fn delete_folder(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    s.core.delete_mail_folder(&auth.user.id, &id).await?;
    Ok(ok())
}

async fn read_all(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    s.core.mark_folder_read(&auth.user.id, &id).await?;
    Ok(ok())
}

async fn empty_folder(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    let n = s.core.empty_mail_folder(&auth.user.id, &id).await?;
    Ok(Json(json!({"deleted": n})))
}

async fn folder_stats(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"stats": s.core.mail_folder_stats(&auth.user.id, &id).await?})))
}

async fn repair_folder(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    s.core.repair_mail_folder(&auth.user.id, &id).await?;
    Ok(ok())
}

async fn inbox_settings(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    let settings = s.core.account_inbox_settings(&auth.user.id, &id).await?;
    let classifier = s.core.classifier_status(&id).await?;
    Ok(Json(json!({"settings": settings, "classifier": classifier})))
}

async fn set_inbox_settings(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<AccountInboxInput>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    let settings = s.core.set_account_inbox_settings(&auth.user.id, &id, &b).await?;
    let classifier = s.core.classifier_status(&id).await?;
    Ok(Json(json!({"settings": settings, "classifier": classifier})))
}

/// Retrains the account's importance classifier now.
async fn train_classifier(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    let a = s
        .core
        .mail_access(&auth.user.id, &id, ferrite_core::mail::delegation::MailRight::Modify)
        .await?;
    Ok(Json(json!({"classifier": s.core.train_classifier(&a).await?})))
}

#[derive(Deserialize)]
struct ViewQ {
    before: Option<String>,
    #[serde(default)]
    unread: bool,
    limit: Option<i64>,
    sort: Option<Sort>,
}

async fn view_messages(State(s): State<AppState>, auth: Auth, Path(view): Path<String>, Query(q): Query<ViewQ>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    let view = View::parse(&view)?;
    let page = s
        .core
        .mail_view(
            &auth.user.id,
            &view,
            q.before.as_deref(),
            q.unread,
            q.sort.unwrap_or(Sort::Newest),
            q.limit.unwrap_or(50),
        )
        .await?;
    Ok(Json(json!(page)))
}

async fn search_advanced(State(s): State<AppState>, auth: Auth, Query(f): Query<SearchFilter>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"messages": s.core.mail_search_advanced(&auth.user.id, &f, 200).await?})))
}

async fn widgets(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!(s.core.mail_widgets(&auth.user.id, 6).await?)))
}

#[derive(Deserialize)]
struct ResolveQ {
    m: Option<String>,
    mid: Option<String>,
}

async fn resolve(State(s): State<AppState>, auth: Auth, Query(q): Query<ResolveQ>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    let id = s.core.mail_resolve(&auth.user.id, q.m.as_deref(), q.mid.as_deref()).await?;
    Ok(Json(json!({"id": id})))
}

#[derive(Deserialize)]
struct MailtoQ {
    uri: String,
}

async fn mailto(auth: Auth, Query(q): Query<MailtoQ>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"mailto": ferrite_core::mail::mailto::parse(&q.uri)})))
}
