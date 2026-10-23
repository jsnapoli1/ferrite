//! Mail security and privacy API: the reader's security report, trusted senders, internal
//! addresses, settings, the remote image proxy, sender avatars, view source, attachments as
//! a ZIP and the S/MIME certificate store. Everything requires the `mail` scope and only
//! touches the caller's own data (messages through the central access check; Gravatar is the
//! admin's `gravatar` mail setting).

use super::files::content_disposition;
use crate::AppState;
use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::mail_fetch;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::Engine;
use ferrite_core::Error;
use ferrite_core::mail::security::ZipWriter;
use ferrite_core::mail::smime_store::IdentityInput;
use ferrite_core::mail::trust::{EntryKind, List, SettingsPatch};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;

/// Cached favicons are fetched again after this long; misses after a day.
const ICON_TTL: chrono::Duration = chrono::Duration::days(7);
const MISS_TTL: chrono::Duration = chrono::Duration::days(1);
/// Avatars resolved per request.
const MAX_AVATARS: usize = 40;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/mail/security/settings", get(settings).patch(update_settings))
        .route("/api/mail/security/check-recipients", post(check_recipients))
        .route("/api/mail/trusted-senders", get(trusted).post(add_trusted))
        .route("/api/mail/trusted-senders/{id}", delete(remove_trusted))
        .route("/api/mail/internal-addresses", get(internal).post(add_internal))
        .route("/api/mail/internal-addresses/{id}", delete(remove_internal))
        .route("/api/mail/messages/{id}/security", get(report))
        .route("/api/mail/messages/{id}/source", get(source))
        .route("/api/mail/messages/{id}/attachments.zip", get(zip))
        .route("/api/mail/proxy", get(proxy))
        .route("/api/mail/avatars", get(avatars))
        .route("/api/mail/smime/certificates", get(certificates).post(import_certificate))
        .route("/api/mail/smime/certificates/{id}", delete(delete_certificate))
        .route("/api/mail/smime/certificates/{id}/pem", get(certificate_pem))
        .route("/api/mail/smime/identities", get(identities).put(set_identity))
}

fn scope(auth: &Auth) -> Result<(), ApiError> {
    auth.require_scope("mail")
}

fn ok() -> Json<Value> {
    Json(json!({"ok": true}))
}

async fn settings(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"settings": s.core.mail_security_settings(&auth.user.id).await?})))
}

async fn update_settings(State(s): State<AppState>, auth: Auth, Json(p): Json<SettingsPatch>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"settings": s.core.mail_update_security_settings(&auth.user.id, &p).await?})))
}

#[derive(Deserialize)]
struct RecipientsBody {
    recipients: Vec<String>,
}

/// For the composer's before-send check: recipients outside the internal addresses, and
/// which recipients S/MIME can encrypt to.
async fn check_recipients(State(s): State<AppState>, auth: Auth, Json(b): Json<RecipientsBody>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    if b.recipients.len() > 500 {
        return Err(ApiError(Error::bad("too many recipients")));
    }
    let uid = &auth.user.id;
    Ok(Json(json!({
        "external": s.core.mail_external_recipients(uid, &b.recipients).await?,
        "smime": s.core.smime_recipients_status(uid, &b.recipients).await?,
    })))
}

#[derive(Deserialize)]
struct EntryBody {
    value: String,
    kind: Option<EntryKind>,
}

async fn trusted(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"entries": s.core.mail_list_entries(&auth.user.id, List::Trusted).await?})))
}

async fn add_trusted(State(s): State<AppState>, auth: Auth, Json(b): Json<EntryBody>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(
        json!({"entry": s.core.mail_add_entry(&auth.user.id, List::Trusted, b.kind, &b.value).await?}),
    ))
}

async fn remove_trusted(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    s.core.mail_remove_entry(&auth.user.id, List::Trusted, &id).await?;
    Ok(ok())
}

async fn internal(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"entries": s.core.mail_list_entries(&auth.user.id, List::Internal).await?})))
}

async fn add_internal(State(s): State<AppState>, auth: Auth, Json(b): Json<EntryBody>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(
        json!({"entry": s.core.mail_add_entry(&auth.user.id, List::Internal, b.kind, &b.value).await?}),
    ))
}

async fn remove_internal(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    s.core.mail_remove_entry(&auth.user.id, List::Internal, &id).await?;
    Ok(ok())
}

async fn report(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"security": s.core.mail_security_report(&auth.user.id, &id).await?})))
}

/// The raw message as JSON text (the UI shows it in a text node; nothing is interpreted).
async fn source(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    let (source, truncated) = s.core.mail_source(&auth.user.id, &id).await?;
    Ok(Json(json!({"source": source, "truncated": truncated})))
}

/// Every attachment in one ZIP, streamed entry by entry.
async fn zip(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Response> {
    scope(&auth)?;
    let (m, files) = s.core.mail_attachments_for_zip(&auth.user.id, &id).await?;
    let modified = m.date;
    let base: String = m.subject.chars().filter(|c| !"/\\\0\"".contains(*c) && !c.is_control()).take(80).collect();
    let name = if base.trim().is_empty() {
        "attachments.zip".to_string()
    } else {
        format!("{} - attachments.zip", base.trim())
    };
    // Entries are produced one at a time as the body is read; the iterator stays exhausted
    // once the central directory went out (the body may be polled again after its end).
    let mut writer = Some(ZipWriter::default());
    let mut files = files.into_iter();
    let chunks = std::iter::from_fn(move || {
        let w = writer.as_mut()?;
        let chunk = match files.next() {
            Some((n, data)) => w.entry(&n, &data, modified),
            None => writer.take()?.finish(),
        };
        Some(Ok::<_, std::io::Error>(bytes::Bytes::from(chunk)))
    });
    let stream = futures::stream::iter(chunks);
    let mut resp = Response::new(Body::from_stream(stream));
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/zip"));
    h.insert(header::CONTENT_DISPOSITION, content_disposition(&name, false));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    Ok(resp)
}

#[derive(Deserialize)]
struct ProxyQ {
    u: String,
    e: i64,
    s: String,
}

/// The image proxy: only URLs the server signed for this user (in a message it rendered),
/// fetched without cookies or referrer, through the SSRF guard, size and time limits, and
/// served only when the bytes are a raster image.
async fn proxy(State(s): State<AppState>, auth: Auth, Query(q): Query<ProxyQ>) -> ApiResult<Response> {
    scope(&auth)?;
    let url = s.core.mail_proxy_check(&auth.user.id, &q.u, q.e, &q.s)?;
    let allow_private = local_allowed(&s).await;
    let (mime, data) = match mail_fetch::proxied_image(&url, mail_fetch::MAX_IMAGE, allow_private).await {
        Ok(x) => x,
        Err(e) => {
            tracing::debug!(error = %e, "image proxy fetch failed");
            let status = match e {
                Error::Forbidden => StatusCode::FORBIDDEN,
                Error::BadRequest(_) => StatusCode::UNPROCESSABLE_ENTITY,
                _ => StatusCode::BAD_GATEWAY,
            };
            let mut r = Response::new(Body::empty());
            *r.status_mut() = status;
            r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=300"));
            return Ok(r);
        }
    };
    let mut resp = Response::new(Body::from(data));
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    // An SVG opened directly is a document: no scripts, no requests, no same origin.
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; sandbox"),
    );
    h.insert(header::CONTENT_DISPOSITION, HeaderValue::from_static("inline; filename=\"image\""));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=3600"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert("cross-origin-resource-policy", HeaderValue::from_static("same-origin"));
    Ok(resp)
}

#[derive(Deserialize)]
struct AvatarsQ {
    emails: String,
}

fn data_uri(mime: &str, data: &[u8]) -> String {
    format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(data))
}

/// One cached network picture: fresh hit, fresh miss, or not cached (or stale).
async fn cached(s: &AppState, key: &str) -> Option<Option<String>> {
    let c = s.core.mail_avatar_cache_get(key).await.ok().flatten()?;
    let age = ferrite_core::util::now() - c.fetched_at;
    match (c.mime, c.data) {
        (Some(m), Some(d)) if age < ICON_TTL => Some(Some(data_uri(&m, &d))),
        (_, None) if age < MISS_TTL => Some(None),
        _ => None,
    }
}

async fn network_avatar(s: &AppState, email: &str, gravatar: bool) -> Option<String> {
    let allow_private = local_allowed(s).await;
    if gravatar {
        let hash = ferrite_core::util::sha256_hex(email.trim().to_lowercase().as_bytes());
        let key = format!("gravatar:{hash}");
        match cached(s, &key).await {
            Some(Some(uri)) => return Some(uri),
            Some(None) => {}
            None => {
                let url = format!("https://www.gravatar.com/avatar/{hash}?d=404&s=96");
                let found = mail_fetch::image(&url, mail_fetch::MAX_ICON, allow_private).await.ok();
                let _ = s.core.mail_avatar_cache_put(&key, found.as_ref().map(|(m, d)| (*m, d.as_slice()))).await;
                if let Some((m, d)) = found {
                    return Some(data_uri(m, &d));
                }
            }
        }
    }
    let domain = ferrite_core::mail::trust::domain_of(email)?;
    if !ferrite_core::mail::trust::valid_domain(&domain) {
        return None;
    }
    let key = format!("favicon:{domain}");
    if let Some(hit) = cached(s, &key).await {
        return hit;
    }
    let found = tokio::time::timeout(Duration::from_secs(8), mail_fetch::favicon(&domain, allow_private))
        .await
        .ok()
        .flatten();
    let _ = s.core.mail_avatar_cache_put(&key, found.as_ref().map(|(m, d)| (*m, d.as_slice()))).await;
    found.map(|(m, d)| data_uri(m, &d))
}

/// Sender pictures: the user's contact photo, else (when the admin allows it) Gravatar, else
/// the sender domain's favicon; `null` means initials. Network pictures are fetched by the
/// server (never by the browser) and cached, misses too.
async fn avatars(State(s): State<AppState>, auth: Auth, Query(q): Query<AvatarsQ>) -> ApiResult<Response> {
    scope(&auth)?;
    let mut emails: Vec<String> = q
        .emails
        .split(',')
        .map(|e| e.trim().to_lowercase())
        .filter(|e| ferrite_core::mail::parse::Addr::parse(e).is_some())
        .collect();
    emails.sort();
    emails.dedup();
    emails.truncate(MAX_AVATARS);
    let gravatar = s.core.mail_gravatar().await;
    let uid = auth.user.id.clone();
    let results: Vec<(String, Option<String>)> = futures::stream::iter(emails)
        .map(|email| {
            let s = s.clone();
            let uid = uid.clone();
            async move {
                if let Ok(Some((mime, data))) = s.core.mail_contact_photo(&uid, &email).await {
                    return (email, Some(data_uri(&mime, &data)));
                }
                let found = network_avatar(&s, &email, gravatar).await;
                (email, found)
            }
        })
        .buffer_unordered(6)
        .collect()
        .await;
    let map: HashMap<String, Option<String>> = results.into_iter().collect();
    let mut resp = Response::new(Body::from(serde_json::to_vec(&json!({"avatars": map})).map_err(Error::internal)?));
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=600"));
    Ok(resp)
}

async fn certificates(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"certificates": s.core.smime_certificates(&auth.user.id).await?})))
}

/// Import body: `pkcs12` (base64) with `password`, or `certificate` (PEM text or base64 DER)
/// with an optional `private_key` (PEM) and its `password`.
#[derive(Deserialize)]
struct ImportBody {
    pkcs12: Option<String>,
    certificate: Option<String>,
    private_key: Option<String>,
    #[serde(default)]
    password: String,
}

fn text_or_b64(v: &str) -> Vec<u8> {
    if v.contains("-----BEGIN") {
        v.as_bytes().to_vec()
    } else {
        let clean: String = v.split_whitespace().collect();
        base64::engine::general_purpose::STANDARD
            .decode(clean)
            .unwrap_or_else(|_| v.as_bytes().to_vec())
    }
}

async fn import_certificate(State(s): State<AppState>, auth: Auth, Json(b): Json<ImportBody>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    let uid = &auth.user.id;
    let cert = match (&b.pkcs12, &b.certificate) {
        (Some(p), _) => {
            let data = base64::engine::general_purpose::STANDARD
                .decode(p.split_whitespace().collect::<String>())
                .map_err(|_| ApiError(Error::bad("the PKCS#12 file is not base64")))?;
            s.core.smime_import_pkcs12(uid, &data, &b.password).await?
        }
        (None, Some(c)) => {
            let key = b.private_key.as_deref().filter(|k| !k.trim().is_empty()).map(text_or_b64);
            let password = Some(b.password.as_str()).filter(|p| !p.is_empty());
            s.core.smime_import_pem(uid, &text_or_b64(c), key.as_deref(), password).await?
        }
        (None, None) => return Err(ApiError(Error::bad("send a PKCS#12 file or a certificate"))),
    };
    Ok(Json(json!({"certificate": cert})))
}

async fn delete_certificate(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    s.core.smime_delete(&auth.user.id, &id).await?;
    Ok(ok())
}

/// The certificate (never the key) as PEM.
async fn certificate_pem(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Response> {
    scope(&auth)?;
    let (view, der) = s.core.smime_certificate(&auth.user.id, &id).await?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&der);
    let mut pem = String::from("-----BEGIN CERTIFICATE-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        pem.push('\n');
    }
    pem.push_str("-----END CERTIFICATE-----\n");
    let name = format!("{}.pem", view.info.emails.first().cloned().unwrap_or_else(|| "certificate".into()));
    let mut resp = Response::new(Body::from(pem));
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/x-pem-file"));
    h.insert(header::CONTENT_DISPOSITION, content_disposition(&name, false));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    Ok(resp)
}

async fn identities(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"identities": s.core.smime_identities(&auth.user.id).await?})))
}

async fn set_identity(State(s): State<AppState>, auth: Auth, Json(b): Json<IdentityInput>) -> ApiResult<Json<Value>> {
    scope(&auth)?;
    Ok(Json(json!({"identities": s.core.smime_set_identity(&auth.user.id, &b).await?})))
}

/// Whether the image proxy and favicon fetcher may reach local and private addresses: when
/// the admin allows mail servers there (`allow_local_servers`, for a LAN), or in tests
/// (`[mail_security] proxy_allow_private`). Otherwise such addresses are refused.
async fn local_allowed(s: &AppState) -> bool {
    s.core.config.mail_security.proxy_allow_private || s.core.mail_settings().await.allow_local_servers
}
