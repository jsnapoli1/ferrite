use crate::AppState;
use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use ferrite_core::Error;
use ferrite_core::collab::DocKind;
use ferrite_core::files::{WriteOpts, perm};
use ferrite_core::shares::{NewShare, ShareUpdate};
use futures::TryStreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tower::ServiceExt;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/files/resolve", get(resolve))
        .route("/api/files/recent", get(recent))
        .route("/api/files/favorites", get(favorites))
        .route("/api/files/quota", get(quota))
        .route("/api/files/{id}", get(info).patch(update).delete(remove))
        .route("/api/files/{id}/children", get(children))
        .route("/api/files/{id}/mkdir", post(mkdir))
        .route("/api/files/{id}/upload", put(upload))
        .route("/api/files/{id}/download", get(download))
        .route("/api/files/{id}/content", get(content))
        .route("/api/files/{id}/copy", post(copy))
        .route("/api/files/{id}/favorite", post(favorite))
        .route("/api/files/{id}/versions", get(versions))
        .route("/api/files/{id}/versions/{vid}", get(version_download))
        .route("/api/files/{id}/versions/{vid}/restore", post(version_restore))
        .route("/api/files/{id}/lock", post(lock).delete(unlock))
        .route("/api/files/{id}/documents", post(create_document))
        .route("/api/files/{id}/shares", get(shares).post(create_share))
        .route("/api/files/{id}/open-with", get(open_with))
        .route("/api/shares/outgoing", get(outgoing))
        .route("/api/shares/incoming", get(incoming))
        .route("/api/shares/{id}", patch(update_share).delete(delete_share))
        .route("/api/trash", get(trash).delete(empty_trash))
        .route("/api/trash/{id}", delete(purge))
        .route("/api/trash/{id}/restore", post(restore))
}

fn ok() -> Json<Value> {
    Json(json!({"ok": true}))
}

fn files_scope(auth: &Auth) -> ApiResult<()> {
    auth.require_scope("files")
}

#[derive(Deserialize)]
struct PathQ {
    path: Option<String>,
}

async fn resolve(State(s): State<AppState>, auth: Auth, Query(q): Query<PathQ>) -> ApiResult<Json<Value>> {
    files_scope(&auth)?;
    let n = s.core.resolve_path(&auth.user.id, q.path.as_deref().unwrap_or("/")).await?;
    let p = s.core.permissions(&auth.user.id, &n).await?;
    let path = s.core.display_path(&auth.user.id, &n).await?;
    Ok(Json(json!({"entry": s.core.entry(&auth.user.id, n, p).await?, "path": path})))
}

async fn info(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    files_scope(&auth)?;
    let (n, p) = s.core.node_for(&auth.user.id, &id, perm::READ).await?;
    let path = s.core.display_path(&auth.user.id, &n).await?;
    // Breadcrumbs from the viewer's root.
    let mut crumbs = Vec::new();
    let mut cur = Some(n.clone());
    while let Some(c) = cur {
        if c.parent_id.is_none() || s.core.permissions(&auth.user.id, &c).await? & perm::READ == 0 {
            break;
        }
        crumbs.push(json!({"id": c.id, "name": c.name}));
        if c.owner_id != auth.user.id {
            // Stop at the share mount point.
            let parent = match &c.parent_id {
                Some(pid) => s.core.node(pid).await.ok(),
                None => None,
            };
            match parent {
                Some(p) if s.core.permissions(&auth.user.id, &p).await? & perm::READ != 0 => {
                    cur = Some(p);
                    continue;
                }
                _ => break,
            }
        }
        cur = match &c.parent_id {
            Some(pid) => s.core.node(pid).await.ok(),
            None => None,
        };
    }
    crumbs.reverse();
    Ok(Json(
        json!({"entry": s.core.entry(&auth.user.id, n, p).await?, "path": path, "breadcrumbs": crumbs}),
    ))
}

async fn children(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    files_scope(&auth)?;
    Ok(Json(json!({"entries": s.core.list_dir(&auth.user.id, &id).await?})))
}

#[derive(Deserialize)]
struct NameBody {
    name: String,
}

async fn mkdir(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<NameBody>) -> ApiResult<Json<Value>> {
    files_scope(&auth)?;
    Ok(Json(json!({"node": s.core.mkdir(&auth.user.id, &id, b.name.trim()).await?})))
}

#[derive(Deserialize)]
struct UploadQ {
    name: String,
    /// When true, fail if the file exists instead of adding a version.
    #[serde(default)]
    create_only: bool,
}

/// Streams the request body straight to the blob store (no buffering in memory).
async fn upload(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Query(q): Query<UploadQ>, req: Request) -> ApiResult<Json<Value>> {
    files_scope(&auth)?;
    let if_match = req.headers().get(header::IF_MATCH).and_then(|v| v.to_str().ok()).map(String::from);
    let stream = req.into_body().into_data_stream().map_err(std::io::Error::other);
    let reader = tokio_util::io::StreamReader::new(stream);
    let opts = WriteOpts {
        if_match,
        create_only: q.create_only,
        ..WriteOpts::user()
    };
    let node = s.core.write_file(&auth.user.id, &id, q.name.trim(), reader, opts).await?;
    Ok(Json(json!({"node": node})))
}

fn content_disposition(name: &str, inline: bool) -> HeaderValue {
    let ascii: String = name
        .chars()
        .map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' })
        .collect();
    let encoded = percent_encoding::utf8_percent_encode(name, percent_encoding::NON_ALPHANUMERIC).to_string();
    let kind = if inline { "inline" } else { "attachment" };
    HeaderValue::from_str(&format!("{kind}; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")).unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

/// Types safe to render inline from our origin; everything else downloads (prevents stored XSS
/// through uploaded HTML/SVG).
fn inline_safe(mime: &str) -> bool {
    matches!(
        mime,
        "image/png"
            | "image/jpeg"
            | "image/gif"
            | "image/webp"
            | "image/avif"
            | "application/pdf"
            | "text/plain"
            | "video/mp4"
            | "video/webm"
            | "audio/mpeg"
            | "audio/ogg"
            | "audio/wav"
            | "audio/webm"
    )
}

pub async fn serve_blob(path: std::path::PathBuf, name: &str, mime: &str, etag: &str, inline: bool, req: Request) -> ApiResult<Response> {
    let inline = inline && inline_safe(mime);
    let mime_v: mime_guess::mime::Mime = if inline {
        mime.parse().unwrap_or(mime_guess::mime::APPLICATION_OCTET_STREAM)
    } else {
        mime_guess::mime::APPLICATION_OCTET_STREAM
    };
    let svc = tower_http::services::ServeFile::new_with_mime(&path, &mime_v);
    let mut resp = svc.oneshot(req).await.map_err(|e| ApiError(Error::internal(e)))?.map(Body::new);
    let h = resp.headers_mut();
    h.insert(header::CONTENT_DISPOSITION, content_disposition(name, inline));
    h.insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{etag}\"")).unwrap_or_else(|_| HeaderValue::from_static("\"\"")),
    );
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; img-src 'self'; media-src 'self'; style-src 'unsafe-inline'; sandbox"),
    );
    Ok(resp)
}

#[derive(Deserialize)]
struct DownloadQ {
    #[serde(default)]
    inline: bool,
}

async fn download(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Query(q): Query<DownloadQ>, req: Request) -> ApiResult<Response> {
    files_scope(&auth)?;
    let (node, path) = s.core.open_file(&auth.user.id, &id).await?;
    serve_blob(path, &node.name, &node.mime, &node.etag, q.inline, req).await
}

/// Text content of a file (live documents include unsaved live edits).
async fn content(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Response> {
    files_scope(&auth)?;
    let (node, _) = s.core.node_for(&auth.user.id, &id, perm::READ).await?;
    let data = if DocKind::for_name(&node.name).is_some() {
        s.core.collab_content(&auth.user.id, &id).await?
    } else {
        s.core.read_file(&auth.user.id, &id).await?.1
    };
    if data.len() > 8 * 1024 * 1024 {
        return Err(ApiError(Error::bad("file too large to preview")));
    }
    Ok((
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        String::from_utf8_lossy(&data).into_owned(),
    )
        .into_response())
}

#[derive(Deserialize)]
struct UpdateBody {
    name: Option<String>,
    parent_id: Option<String>,
}

async fn update(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<UpdateBody>) -> ApiResult<Json<Value>> {
    files_scope(&auth)?;
    let node = s.core.node(&id).await?;
    let parent = b.parent_id.or(node.parent_id.clone()).ok_or(ApiError(Error::bad("cannot move the root")))?;
    let name = b.name.map(|n| n.trim().to_string()).unwrap_or(node.name);
    Ok(Json(json!({"node": s.core.move_node(&auth.user.id, &id, &parent, &name, false).await?})))
}

#[derive(Deserialize)]
struct CopyBody {
    parent_id: String,
    name: Option<String>,
}

async fn copy(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<CopyBody>) -> ApiResult<Json<Value>> {
    files_scope(&auth)?;
    let node = s.core.node(&id).await?;
    let name = b.name.unwrap_or_else(|| {
        if node.parent_id.as_deref() == Some(b.parent_id.as_str()) {
            match node.name.rsplit_once('.') {
                Some((stem, ext)) if !stem.is_empty() => format!("{stem} (copy).{ext}"),
                _ => format!("{} (copy)", node.name),
            }
        } else {
            node.name.clone()
        }
    });
    Ok(Json(json!({"node": s.core.copy_node(&auth.user.id, &id, &b.parent_id, &name).await?})))
}

async fn remove(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    files_scope(&auth)?;
    s.core.delete(&auth.user.id, &id).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct FavBody {
    favorite: bool,
}

async fn favorite(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<FavBody>) -> ApiResult<Json<Value>> {
    s.core.set_favorite(&auth.user.id, &id, b.favorite).await?;
    Ok(ok())
}

async fn recent(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    files_scope(&auth)?;
    let mut out = Vec::new();
    for n in s.core.recent_files(&auth.user.id, 50).await? {
        let path = s.core.display_path(&auth.user.id, &n).await?;
        out.push(json!({"node": n, "path": path}));
    }
    Ok(Json(json!({"files": out})))
}

async fn favorites(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    files_scope(&auth)?;
    Ok(Json(json!({"files": s.core.favorites(&auth.user.id).await?})))
}

async fn quota(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"quota": s.core.quota(&auth.user.id).await?})))
}

async fn versions(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    files_scope(&auth)?;
    Ok(Json(json!({"versions": s.core.list_versions(&auth.user.id, &id).await?})))
}

async fn version_download(State(s): State<AppState>, auth: Auth, Path((id, vid)): Path<(String, String)>) -> ApiResult<Response> {
    files_scope(&auth)?;
    let (node, _) = s.core.node_for(&auth.user.id, &id, perm::READ).await?;
    let data = s.core.read_version(&auth.user.id, &id, &vid).await?;
    Ok((
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream")),
            (header::CONTENT_DISPOSITION, content_disposition(&node.name, false)),
        ],
        data,
    )
        .into_response())
}

async fn version_restore(State(s): State<AppState>, auth: Auth, Path((id, vid)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    files_scope(&auth)?;
    Ok(Json(json!({"node": s.core.restore_version(&auth.user.id, &id, &vid).await?})))
}

#[derive(Deserialize)]
struct LockBody {
    app: Option<String>,
    ttl_secs: Option<i64>,
}

async fn lock(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<LockBody>) -> ApiResult<Json<Value>> {
    files_scope(&auth)?;
    let l = s
        .core
        .lock(&auth.user.id, &id, b.app.as_deref().unwrap_or("manual"), b.ttl_secs.unwrap_or(3600))
        .await?;
    Ok(Json(json!({"lock": l, "token": l.token})))
}

async fn unlock(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    files_scope(&auth)?;
    s.core.unlock(&auth.user.id, &id).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct DocBody {
    title: String,
    kind: String,
}

async fn create_document(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<DocBody>) -> ApiResult<Json<Value>> {
    files_scope(&auth)?;
    let kind = DocKind::parse(&b.kind).ok_or(ApiError(Error::bad("kind must be text, sheet, mindmap or canvas")))?;
    Ok(Json(json!({"node": s.core.create_document(&auth.user.id, &id, &b.title, kind).await?})))
}

async fn open_with(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let (n, _) = s.core.node_for(&auth.user.id, &id, perm::READ).await?;
    let installed = s.tools.craft.availability();
    let apps: Vec<Value> = ferrite_core::craft::apps_for_name(&n.name)
        .into_iter()
        .map(|a| json!({"id": a.id, "name": a.name, "replaces": a.replaces, "default": ferrite_core::craft::app_for_name(&n.name).is_some_and(|d| d.id == a.id), "installed_on_server": installed.iter().any(|i| i.app.id == a.id && i.installed), "repo": a.repo}))
        .collect();
    Ok(Json(json!({"apps": apps, "live_editor": DocKind::for_name(&n.name).map(|k| k.as_str())})))
}

// ---- shares -----------------------------------------------------------------------

async fn shares(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    files_scope(&auth)?;
    Ok(Json(json!({"shares": s.core.shares_of_node(&auth.user.id, &id).await?})))
}

async fn create_share(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<NewShare>) -> ApiResult<Json<Value>> {
    files_scope(&auth)?;
    Ok(Json(json!({"share": s.core.create_share(&auth.user.id, &id, b).await?})))
}

async fn outgoing(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"shares": s.core.outgoing_shares(&auth.user.id).await?})))
}

async fn incoming(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"shares": s.core.incoming_shares(&auth.user.id).await?})))
}

async fn update_share(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<ShareUpdate>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"share": s.core.update_share(&auth.user.id, &id, b).await?})))
}

async fn delete_share(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    s.core.delete_share(&auth.user.id, &id).await?;
    Ok(ok())
}

// ---- trash ------------------------------------------------------------------------

async fn trash(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"items": s.core.list_trash(&auth.user.id).await?})))
}

async fn empty_trash(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"purged": s.core.empty_trash(&auth.user.id).await?})))
}

async fn purge(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    s.core.purge(&auth.user.id, &id).await?;
    Ok(ok())
}

async fn restore(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"node": s.core.restore(&auth.user.id, &id).await?})))
}
