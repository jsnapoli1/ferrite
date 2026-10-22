//! Public link shares (`/s/<token>`): browse, download and — when allowed — upload ("file
//! drop"), optionally behind a password. The page itself is the web app; this is its API.

use crate::AppState;
use crate::auth::cookie;
use crate::error::{ApiError, ApiResult};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use ferrite_core::Error;
use ferrite_core::files::{WriteOpts, perm};
use ferrite_core::shares::Share;
use futures::TryStreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/public/{token}", get(info))
        .route("/api/public/{token}/unlock", post(unlock))
        .route("/api/public/{token}/download", get(download))
        .route("/api/public/{token}/upload", put(upload))
}

fn cookie_name(token: &str) -> String {
    format!("ferrite_link_{token}")
}

async fn unlocked_share(s: &AppState, token: &str, headers: &HeaderMap) -> ApiResult<Share> {
    let share = s.core.link_share(token).await?;
    if share.has_password() && cookie(headers, &cookie_name(token)).as_deref() != Some(s.core.link_unlock_proof(&share).as_str()) {
        return Err(ApiError(Error::Unauthorized));
    }
    Ok(share)
}

#[derive(Deserialize)]
struct PathQ {
    #[serde(default)]
    path: String,
    /// `?download=1` asks for an attachment instead of inline display.
    #[serde(default)]
    download: Option<String>,
}

async fn info(State(s): State<AppState>, Path(token): Path<String>, Query(q): Query<PathQ>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let share = s.core.link_share(&token).await?;
    let owner = s.core.user(&share.owner_id).await?;
    let root = s.core.node(&share.node_id).await?;
    let base = json!({
        "name": root.name, "kind": root.kind, "owner": owner.display_name,
        "label": share.label, "expires_at": share.expires_at,
        "can_upload": share.permissions & perm::CREATE != 0 && root.is_dir(),
        "password_required": share.has_password(),
    });
    let share = match unlocked_share(&s, &token, &headers).await {
        Ok(sh) => sh,
        Err(ApiError(Error::Unauthorized)) => {
            return Ok(Json(json!({"share": base, "locked": true})));
        }
        Err(e) => return Err(e),
    };
    let node = s.core.link_node(&share, &q.path).await?;
    let entries: Vec<Value> = if node.is_dir() {
        s.core
            .link_children(&node)
            .await?
            .into_iter()
            .map(|n| json!({"name": n.name, "kind": n.kind, "size": n.size, "mime": n.mime, "modified": n.modified_at}))
            .collect()
    } else {
        Vec::new()
    };
    Ok(Json(
        json!({"share": base, "locked": false, "entry": {"name": node.name, "kind": node.kind, "size": node.size, "mime": node.mime, "modified": node.modified_at}, "entries": entries}),
    ))
}

#[derive(Deserialize)]
struct PasswordBody {
    password: String,
}

async fn unlock(State(s): State<AppState>, Path(token): Path<String>, Json(b): Json<PasswordBody>) -> ApiResult<Response> {
    let key = format!("link:{token}");
    if s.throttle.blocked(&key) {
        return Err(ApiError(Error::Unavailable("too many attempts; try again later".into())));
    }
    let share = s.core.link_share(&token).await?;
    if !s.core.check_link_password(&share, &b.password) {
        s.throttle.fail(&key);
        return Err(ApiError(Error::Unauthorized));
    }
    s.throttle.succeed(&key);
    let c = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=86400{}",
        cookie_name(&token),
        s.core.link_unlock_proof(&share),
        if s.core.config.secure_cookies { "; Secure" } else { "" }
    );
    Ok(([(header::SET_COOKIE, c)], Json(json!({"ok": true}))).into_response())
}

async fn download(State(s): State<AppState>, Path(token): Path<String>, Query(q): Query<PathQ>, req: Request) -> ApiResult<Response> {
    let share = unlocked_share(&s, &token, req.headers()).await?;
    let node = s.core.link_node(&share, &q.path).await?;
    let hash = node.blob_hash.clone().ok_or(ApiError(Error::bad("folders cannot be downloaded directly")))?;
    let inline = q.download.is_none();
    crate::api::files::serve_blob(s.core.blobs.path(&hash), &node.name, &node.mime, &node.etag, inline, req).await
}

#[derive(Deserialize)]
struct UploadQ {
    #[serde(default)]
    path: String,
    name: String,
}

/// Upload into a shared folder. Files are written as the share's owner; without edit rights
/// existing files are never replaced.
async fn upload(State(s): State<AppState>, Path(token): Path<String>, Query(q): Query<UploadQ>, req: Request) -> ApiResult<Json<Value>> {
    let share = unlocked_share(&s, &token, req.headers()).await?;
    if share.permissions & perm::CREATE == 0 {
        return Err(ApiError(Error::Forbidden));
    }
    let dir = s.core.link_node(&share, &q.path).await?;
    if !dir.is_dir() {
        return Err(ApiError(Error::bad("not a folder")));
    }
    let stream = req.into_body().into_data_stream().map_err(std::io::Error::other);
    let reader = tokio_util::io::StreamReader::new(stream);
    let opts = WriteOpts {
        create_only: share.permissions & perm::UPDATE == 0,
        ..WriteOpts::user()
    };
    let n = s.core.write_file(&share.owner_id, &dir.id, q.name.trim(), reader, opts).await?;
    Ok(Json(json!({"name": n.name, "size": n.size})))
}
