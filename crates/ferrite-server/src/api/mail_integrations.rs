//! Mail integrations API: Sieve (script, filters, autoresponder) and the user's absence,
//! meeting invitations, travel reservations, events and tasks from messages, contacts,
//! one-click unsubscribe, and AI (summaries, replies, translation, follow-ups). Mail
//! endpoints need the `mail` scope; those that change calendars or contacts also `groupware`.

use crate::AppState;
use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::mail_ai;
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use ferrite_core::Error;
use ferrite_core::absence::AbsenceInput;
use ferrite_core::mail::agenda::TaskInput;
use ferrite_core::mail::filters::{Filter, Vacation};
use ferrite_core::mail::integrations::IntegrationsInput;
use serde::Deserialize;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::time::Duration;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/mail/accounts/{id}/integrations", get(integrations).patch(set_integrations))
        .route("/api/mail/accounts/{id}/sieve", get(sieve_script).put(set_sieve_script))
        .route("/api/mail/accounts/{id}/filters", get(filters).put(set_filters))
        .route("/api/mail/accounts/{id}/vacation", get(vacation).put(set_vacation))
        .route("/api/me/absence", get(my_absence).put(set_absence).delete(clear_absence))
        .route("/api/users/{username}/absence", get(user_absence))
        .route("/api/mail/messages/{id}/integrations", get(message_integrations))
        .route("/api/mail/messages/{id}/imip/respond", post(imip_respond))
        .route("/api/mail/messages/{id}/imip/counter", post(imip_counter))
        .route("/api/mail/messages/{id}/imip/counter-decision", post(imip_counter_decision))
        .route("/api/mail/messages/{id}/imip/apply", post(imip_apply))
        .route("/api/mail/messages/{id}/attachments/{index}/import", post(import_ics))
        .route("/api/mail/messages/{id}/agenda-draft", get(agenda_draft))
        .route("/api/mail/messages/{id}/unsubscribe", post(unsubscribe))
        .route("/api/mail/tasks", post(create_task))
        .route("/api/mail/contact-info", get(contact_info))
        .route("/api/mail/contacts", post(add_to_contacts))
        .route("/api/mail/ai/status", get(ai_status))
        .route("/api/mail/messages/{id}/ai/summary", post(ai_summary))
        .route("/api/mail/messages/{id}/ai/replies", post(ai_replies))
        .route("/api/mail/messages/{id}/ai/translate", post(ai_translate))
        .route("/api/mail/messages/{id}/ai/event", post(ai_event))
        .route("/api/mail/follow-ups", get(follow_ups))
        .route("/api/mail/follow-ups/disable", post(disable_follow_up))
        .route("/api/mail/follow-ups/check", post(check_follow_ups))
}

fn mail(auth: &Auth) -> Result<(), ApiError> {
    auth.require_scope("mail")
}

fn mail_and_groupware(auth: &Auth) -> Result<(), ApiError> {
    auth.require_scope("mail")?;
    auth.require_scope("groupware")
}

// ---- Sieve ---------------------------------------------------------------------------------

async fn integrations(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    mail(&auth)?;
    Ok(Json(json!({"integrations": s.core.mail_integrations(&auth.user.id, &id).await?})))
}

async fn set_integrations(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<IntegrationsInput>) -> ApiResult<Json<Value>> {
    mail(&auth)?;
    Ok(Json(json!({"integrations": s.core.set_mail_integrations(&auth.user.id, &id, &b).await?})))
}

async fn sieve_script(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    mail(&auth)?;
    Ok(Json(json!(s.core.sieve_script(&auth.user.id, &id).await?)))
}

#[derive(Deserialize)]
struct ScriptBody {
    script: String,
}

async fn set_sieve_script(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<ScriptBody>) -> ApiResult<Json<Value>> {
    mail(&auth)?;
    Ok(Json(json!(s.core.set_sieve_script(&auth.user.id, &id, &b.script).await?)))
}

async fn filters(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    mail(&auth)?;
    Ok(Json(json!({"filters": s.core.mail_filters(&auth.user.id, &id).await?})))
}

#[derive(Deserialize)]
struct FiltersBody {
    filters: Vec<Filter>,
}

async fn set_filters(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<FiltersBody>) -> ApiResult<Json<Value>> {
    mail(&auth)?;
    Ok(Json(json!({"filters": s.core.set_mail_filters(&auth.user.id, &id, &b.filters).await?})))
}

async fn vacation(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    mail(&auth)?;
    Ok(Json(json!({"vacation": s.core.mail_vacation(&auth.user.id, &id).await?})))
}

async fn set_vacation(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(v): Json<Vacation>) -> ApiResult<Json<Value>> {
    mail(&auth)?;
    Ok(Json(json!({"vacation": s.core.set_mail_vacation(&auth.user.id, &id, &v).await?})))
}

// ---- absence -------------------------------------------------------------------------------

fn failures(failed: &[(String, String)]) -> Vec<Value> {
    failed.iter().map(|(a, e)| json!({"account": a, "error": e})).collect()
}

async fn my_absence(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    let a = s.core.absence(&auth.user.id).await?;
    let active = a.as_ref().is_some_and(|a| a.active_at(ferrite_core::util::now()));
    Ok(Json(json!({"absence": a, "active": active})))
}

async fn set_absence(State(s): State<AppState>, auth: Auth, Json(b): Json<AbsenceInput>) -> ApiResult<Json<Value>> {
    let (a, failed) = s.core.set_absence(&auth.user.id, &b).await?;
    let active = a.active_at(ferrite_core::util::now());
    Ok(Json(json!({"absence": a, "active": active, "autoresponder_errors": failures(&failed)})))
}

async fn clear_absence(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    let failed = s.core.clear_absence(&auth.user.id).await?;
    Ok(Json(json!({"absence": null, "autoresponder_errors": failures(&failed)})))
}

/// Another user's absence while it runs (what Nextcloud shows as their out-of-office status).
async fn user_absence(State(s): State<AppState>, _auth: Auth, Path(username): Path<String>) -> ApiResult<Json<Value>> {
    let u = s.core.user_by_name(&username).await?;
    if u.disabled {
        return Err(ApiError(Error::NotFound));
    }
    let a = s.core.current_absence(&u.id).await?;
    Ok(Json(json!({
        "absence": a.map(|a| json!({"status": a.status, "message": a.message, "first_day": a.first_day, "last_day": a.last_day, "replacement": a.replacement})),
    })))
}

// ---- reading -------------------------------------------------------------------------------

/// Everything the reader shows around a message: invitation, reservations, unsubscribe
/// options, importable calendar files and who the sender is.
async fn message_integrations(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    mail(&auth)?;
    let uid = &auth.user.id;
    let (_, m) = s.core.message_row(uid, &id).await?;
    let (_, raw) = s.core.mail_raw(uid, &id).await?;
    let imip = if auth.token.allows("groupware") {
        s.core.mail_imip(uid, &id).await?
    } else {
        None
    };
    let html = ferrite_core::mail::parse::parse_body(&raw).html;
    let itineraries = html.as_deref().map(ferrite_core::mail::itinerary::extract).unwrap_or_default();
    let sender = if m.from_addr.is_empty() {
        None
    } else {
        Some(s.core.mail_contact_info(uid, &m.from_addr).await?)
    };
    Ok(Json(json!({
        "imip": imip,
        "itineraries": itineraries,
        "unsubscribe": ferrite_core::mail::unsubscribe::from_raw(&raw),
        "ics_attachments": ferrite_core::mail::imip::ics_attachments(&raw),
        "sender": sender,
    })))
}

#[derive(Deserialize)]
struct RespondBody {
    response: String,
    calendar: Option<String>,
    comment: Option<String>,
}

async fn imip_respond(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<RespondBody>) -> ApiResult<Json<Value>> {
    mail_and_groupware(&auth)?;
    let ps = ferrite_core::scheduling::parse_response(&b.response)?;
    let out = s
        .core
        .mail_imip_respond(&auth.user.id, &id, ps, b.calendar.as_deref(), b.comment.as_deref())
        .await?;
    Ok(Json(json!({"outcome": out, "imip": s.core.mail_imip(&auth.user.id, &id).await?})))
}

#[derive(Deserialize)]
struct CounterBody {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    comment: Option<String>,
}

async fn imip_counter(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<CounterBody>) -> ApiResult<Json<Value>> {
    mail_and_groupware(&auth)?;
    let out = s.core.mail_imip_counter(&auth.user.id, &id, b.start, b.end, b.comment.as_deref()).await?;
    Ok(Json(json!({"outcome": out, "imip": s.core.mail_imip(&auth.user.id, &id).await?})))
}

#[derive(Deserialize)]
struct DecisionBody {
    accept: bool,
}

async fn imip_counter_decision(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<DecisionBody>) -> ApiResult<Json<Value>> {
    mail_and_groupware(&auth)?;
    let out = s.core.mail_imip_counter_decision(&auth.user.id, &id, b.accept).await?;
    Ok(Json(json!({"outcome": out, "imip": s.core.mail_imip(&auth.user.id, &id).await?})))
}

#[derive(Deserialize, Default)]
struct ApplyBody {
    #[serde(default)]
    force: bool,
}

async fn imip_apply(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, body: Option<Json<ApplyBody>>) -> ApiResult<Json<Value>> {
    mail_and_groupware(&auth)?;
    let force = body.map(|b| b.0.force).unwrap_or(false);
    let out = s.core.mail_imip_apply(&auth.user.id, &id, force).await?;
    Ok(Json(json!({"outcome": out, "imip": s.core.mail_imip(&auth.user.id, &id).await?})))
}

#[derive(Deserialize, Default)]
struct ImportBody {
    calendar: Option<String>,
}

async fn import_ics(State(s): State<AppState>, auth: Auth, Path((id, index)): Path<(String, usize)>, body: Option<Json<ImportBody>>) -> ApiResult<Json<Value>> {
    mail_and_groupware(&auth)?;
    let cal = body.and_then(|b| b.0.calendar);
    let ids = s.core.mail_import_ics(&auth.user.id, &id, index, cal.as_deref()).await?;
    Ok(Json(json!({"imported": ids})))
}

async fn agenda_draft(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    mail(&auth)?;
    Ok(Json(json!({"draft": s.core.mail_agenda_draft(&auth.user.id, &id).await?})))
}

async fn create_task(State(s): State<AppState>, auth: Auth, Json(t): Json<TaskInput>) -> ApiResult<Json<Value>> {
    mail_and_groupware(&auth)?;
    let o = s.core.create_task(&auth.user.id, &t).await?;
    Ok(Json(json!({"task": s.core.event_view(&auth.user.id, &o.id).await?})))
}

#[derive(Deserialize)]
struct EmailQ {
    email: String,
}

/// The composer's recipient info and the reader's sender card.
async fn contact_info(State(s): State<AppState>, auth: Auth, Query(q): Query<EmailQ>) -> ApiResult<Json<Value>> {
    mail(&auth)?;
    Ok(Json(json!({"info": s.core.mail_contact_info(&auth.user.id, &q.email).await?})))
}

#[derive(Deserialize)]
struct AddContactBody {
    email: String,
    name: Option<String>,
    contact_id: Option<String>,
}

async fn add_to_contacts(State(s): State<AppState>, auth: Auth, Json(b): Json<AddContactBody>) -> ApiResult<Json<Value>> {
    mail_and_groupware(&auth)?;
    let c = s
        .core
        .mail_add_to_contacts(&auth.user.id, &b.email, b.name.as_deref(), b.contact_id.as_deref())
        .await?;
    Ok(Json(json!({"contact": c})))
}

// ---- unsubscribe ---------------------------------------------------------------------------

use ferrite_core::mail::unsubscribe::allow_private_fetch;

/// DNS that only answers with public addresses, so a list's URL cannot point the server at
/// an internal service (also not by re-resolving between check and connect).
struct PublicResolver;

impl reqwest::dns::Resolve for PublicResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| crate::push::is_public(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(format!("{host} has no public address").into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// The RFC 8058 one-click POST: `List-Unsubscribe=One-Click` to an HTTPS URL on a public
/// address, without cookies or redirects.
pub async fn one_click_post(url: &str) -> ferrite_core::Result<u16> {
    let parsed = reqwest::Url::parse(url).map_err(|_| Error::bad("the unsubscribe link is not a URL"))?;
    let private_ok = allow_private_fetch();
    if parsed.scheme() != "https" && !(private_ok && parsed.scheme() == "http") {
        return Err(Error::bad("one-click unsubscribe needs an https link"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(Error::bad("the unsubscribe link contains credentials"));
    }
    if !private_ok {
        if let Some(ip) = parsed.host_str().and_then(|h| h.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok()) {
            if !crate::push::is_public(ip) {
                return Err(Error::bad("the unsubscribe link points to a private address"));
            }
        }
    }
    let mut b = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("Ferrite mail (RFC 8058 one-click unsubscribe)");
    if !private_ok {
        b = b.dns_resolver(std::sync::Arc::new(PublicResolver));
    }
    let client = b.build().map_err(|e| Error::internal(format!("HTTP client: {e}")))?;
    let resp = client
        .post(parsed)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("List-Unsubscribe=One-Click")
        .send()
        .await
        .map_err(|e| Error::Unavailable(format!("the list's unsubscribe server did not answer: {e}")))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) && !(300..400).contains(&status) {
        return Err(Error::Unavailable(format!("the list's unsubscribe server answered {status}")));
    }
    Ok(status)
}

#[derive(Deserialize, Default)]
struct UnsubscribeBody {
    /// one_click (default when possible) or mail
    method: Option<String>,
}

async fn unsubscribe(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, body: Option<Json<UnsubscribeBody>>) -> ApiResult<Json<Value>> {
    mail(&auth)?;
    let info = s
        .core
        .mail_unsubscribe_info(&auth.user.id, &id)
        .await?
        .ok_or_else(|| Error::bad("this message is not from a mailing list"))?;
    let method = body
        .and_then(|b| b.0.method)
        .unwrap_or_else(|| if info.one_click.is_some() { "one_click".into() } else { "mail".into() });
    match method.as_str() {
        "one_click" => {
            let url = info.one_click.as_deref().ok_or_else(|| Error::bad("this list has no one-click unsubscribe"))?;
            let status = one_click_post(url).await?;
            Ok(Json(json!({"done": true, "method": "one_click", "status": status})))
        }
        "mail" => {
            let to = s.core.mail_unsubscribe_by_mail(&auth.user.id, &id).await?;
            Ok(Json(json!({"done": true, "method": "mail", "to": to})))
        }
        _ => Err(ApiError(Error::bad("method must be one_click or mail"))),
    }
}

// ---- AI ------------------------------------------------------------------------------------

async fn ai_status(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    mail(&auth)?;
    Ok(Json(json!(mail_ai::status(&s.core, &auth.user).await?)))
}

fn ai_scopes(auth: &Auth) -> Result<(), ApiError> {
    auth.require_scope("mail")?;
    auth.require_scope("ai")
}

async fn ai_summary(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    ai_scopes(&auth)?;
    Ok(Json(mail_ai::summarize_thread(&s.core, &auth.user, &id).await?))
}

async fn ai_replies(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    ai_scopes(&auth)?;
    Ok(Json(mail_ai::smart_replies(&s.core, &auth.user, &id).await?))
}

#[derive(Deserialize)]
struct TranslateBody {
    lang: String,
}

async fn ai_translate(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<TranslateBody>) -> ApiResult<Json<Value>> {
    ai_scopes(&auth)?;
    Ok(Json(mail_ai::translate(&s.core, &auth.user, &id, &b.lang).await?))
}

async fn ai_event(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    ai_scopes(&auth)?;
    Ok(Json(json!({"suggestion": mail_ai::event_suggestion(&s.core, &auth.user, &id).await?})))
}

async fn follow_ups(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    mail(&auth)?;
    Ok(Json(json!({"follow_ups": mail_ai::follow_ups(&s.core, &auth.user.id).await?})))
}

/// Checks the user's sent mail for follow-ups now (instead of at the next half-hourly pass).
async fn check_follow_ups(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    ai_scopes(&auth)?;
    let checked = mail_ai::check_follow_ups(&s.core, &auth.user).await?;
    Ok(Json(
        json!({"checked": checked, "follow_ups": mail_ai::follow_ups(&s.core, &auth.user.id).await?}),
    ))
}

#[derive(Deserialize)]
struct FollowUpRef {
    account_id: String,
    message_id: String,
}

async fn disable_follow_up(State(s): State<AppState>, auth: Auth, Json(b): Json<FollowUpRef>) -> ApiResult<Json<Value>> {
    mail(&auth)?;
    mail_ai::disable_follow_up(&s.core, &auth.user.id, &b.account_id, &b.message_id).await?;
    Ok(Json(json!({"ok": true})))
}
