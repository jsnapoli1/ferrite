//! WebDAV (RFC 4918 class 1 and 2) over the Ferrite file tree, at the same paths Nextcloud
//! uses (`/remote.php/dav/files/<user>/…` and `/remote.php/webdav/…`) plus `/dav/files/<user>/…`.
//! Works with rclone, macOS Finder, Windows Explorer, GNOME Files, davfs2, Cyberduck and
//! Nextcloud's own path conventions. Authenticate with Basic auth (username + app token or
//! password) or a bearer token.

use crate::AppState;
use crate::auth::{Auth, authenticate};
use crate::error::{ApiError, public_message, status_of};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ferrite_core::files::{Node, WriteOpts, perm};
use ferrite_core::{Error, Result};
use futures::TryStreamExt;
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};

const PATH_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'[')
    .add(b']')
    .add(b'^')
    .add(b'|')
    .add(b'\\');

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Splits a request path into (url prefix up to and including the user root, path inside).
fn split_dav_path(path: &str, username: &str) -> Option<(String, String)> {
    for base in ["/remote.php/dav/files/", "/dav/files/"] {
        if let Some(rest) = path.strip_prefix(base) {
            let (user, inner) = rest.split_once('/').unwrap_or((rest, ""));
            let user = percent_decode_str(user).decode_utf8_lossy();
            if !user.eq_ignore_ascii_case(username) {
                return None;
            }
            return Some((format!("{base}{}", utf8_percent_encode(&user, PATH_SEGMENT)), format!("/{inner}")));
        }
    }
    for base in ["/remote.php/webdav", "/webdav"] {
        if let Some(rest) = path.strip_prefix(base) {
            return Some((base.to_string(), if rest.is_empty() { "/".into() } else { rest.to_string() }));
        }
    }
    None
}

fn decode(inner: &str) -> String {
    percent_decode_str(inner).decode_utf8_lossy().into_owned()
}

fn error_response(e: &Error) -> Response {
    (status_of(e), public_message(e)).into_response()
}

fn dav_unauthorized() -> Response {
    let mut r = (StatusCode::UNAUTHORIZED, "authentication required").into_response();
    r.headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"Ferrite\", charset=\"UTF-8\""));
    r
}

/// Nextcloud-style permission string, understood by Nextcloud clients.
fn oc_permissions(n: &Node, p: i64, shared: bool) -> String {
    let mut s = String::new();
    if shared {
        s.push('S');
    }
    if p & perm::SHARE != 0 {
        s.push('R');
    }
    if p & perm::DELETE != 0 {
        s.push('D');
    }
    if p & perm::UPDATE != 0 {
        s.push_str("NV");
        if !n.is_dir() {
            s.push('W');
        }
    }
    if n.is_dir() && p & perm::CREATE != 0 {
        s.push_str("CK");
    }
    s
}

pub async fn handle(State(s): State<AppState>, req: Request) -> Response {
    if req.method() == Method::OPTIONS {
        return options();
    }
    let auth = match authenticate(&s, req.headers(), req.method(), true).await {
        Ok(a) if a.token.allows("files") => a,
        Ok(_) => return (StatusCode::FORBIDDEN, "token lacks the files scope").into_response(),
        Err(ApiError(Error::Unavailable(m))) => {
            return (StatusCode::TOO_MANY_REQUESTS, m).into_response();
        }
        Err(ApiError(Error::BadRequest(m))) => return crate::auth::basic_challenge(&m),
        Err(_) => return dav_unauthorized(),
    };
    let path = req.uri().path().to_string();
    let Some((prefix, inner)) = split_dav_path(&path, &auth.user.username) else {
        return (StatusCode::FORBIDDEN, "this path belongs to another user").into_response();
    };
    let inner = decode(&inner);
    let method = req.method().as_str().to_string();
    let result = match method.as_str() {
        "PROPFIND" => propfind(&s, &auth, &prefix, &inner, req.headers()).await,
        "PROPPATCH" => proppatch(&s, &auth, &prefix, &inner).await,
        "GET" | "HEAD" => get(&s, &auth, &inner, req).await,
        "PUT" => put(&s, &auth, &inner, req).await,
        "MKCOL" => mkcol(&s, &auth, &inner).await,
        "DELETE" => delete(&s, &auth, &inner).await,
        "MOVE" | "COPY" => move_copy(&s, &auth, &inner, req.headers(), method == "MOVE").await,
        "LOCK" => lock(&s, &auth, &prefix, &inner).await,
        "UNLOCK" => unlock(&s, &auth, &inner).await,
        _ => Ok(StatusCode::METHOD_NOT_ALLOWED.into_response()),
    };
    result.unwrap_or_else(|e| error_response(&e))
}

fn options() -> Response {
    let mut r = StatusCode::OK.into_response();
    let h = r.headers_mut();
    h.insert("DAV", HeaderValue::from_static("1, 2"));
    h.insert("MS-Author-Via", HeaderValue::from_static("DAV"));
    h.insert(
        header::ALLOW,
        HeaderValue::from_static("OPTIONS, GET, HEAD, PUT, DELETE, MKCOL, MOVE, COPY, PROPFIND, PROPPATCH, LOCK, UNLOCK"),
    );
    r
}

async fn prop_xml(s: &AppState, auth: &Auth, href: &str, n: &Node) -> Result<String> {
    let p = s.core.permissions(&auth.user.id, n).await?;
    let shared = n.owner_id != auth.user.id;
    let mut x = String::new();
    x.push_str("<d:response><d:href>");
    x.push_str(&xml_escape(href));
    x.push_str("</d:href><d:propstat><d:prop>");
    let name = if n.parent_id.is_none() { auth.user.username.clone() } else { n.name.clone() };
    x.push_str(&format!("<d:displayname>{}</d:displayname>", xml_escape(&name)));
    x.push_str(&format!(
        "<d:getlastmodified>{}</d:getlastmodified>",
        n.modified_at.format("%a, %d %b %Y %H:%M:%S GMT")
    ));
    x.push_str(&format!("<d:creationdate>{}</d:creationdate>", n.created_at.format("%Y-%m-%dT%H:%M:%SZ")));
    x.push_str(&format!("<d:getetag>&quot;{}&quot;</d:getetag>", xml_escape(&n.etag)));
    x.push_str(&format!("<oc:fileid>{}</oc:fileid><oc:id>{}</oc:id>", n.id, n.id));
    x.push_str(&format!("<oc:permissions>{}</oc:permissions>", oc_permissions(n, p, shared)));
    x.push_str(&format!("<oc:favorite>{}</oc:favorite>", i32::from(n.favorite)));
    if shared {
        let owner = s.core.user(&n.owner_id).await?;
        x.push_str(&format!(
            "<oc:owner-id>{}</oc:owner-id><oc:owner-display-name>{}</oc:owner-display-name>",
            xml_escape(&owner.username),
            xml_escape(&owner.display_name)
        ));
    }
    if n.is_dir() {
        x.push_str("<d:resourcetype><d:collection/></d:resourcetype>");
        let q = s.core.quota(&n.owner_id).await?;
        let avail = q.limit.map(|l| (l - q.used).max(0)).unwrap_or(-3);
        x.push_str(&format!(
            "<d:quota-used-bytes>{}</d:quota-used-bytes><d:quota-available-bytes>{avail}</d:quota-available-bytes>",
            q.used
        ));
        x.push_str(&format!("<oc:size>{}</oc:size>", if n.parent_id.is_none() { q.used } else { n.size }));
    } else {
        x.push_str("<d:resourcetype/>");
        x.push_str(&format!("<d:getcontentlength>{}</d:getcontentlength>", n.size));
        x.push_str(&format!("<d:getcontenttype>{}</d:getcontenttype>", xml_escape(&n.mime)));
        x.push_str(&format!("<oc:size>{}</oc:size>", n.size));
    }
    x.push_str("</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>");
    Ok(x)
}

fn href_for(prefix: &str, inner_path: &str, is_dir: bool) -> String {
    let mut h = prefix.to_string();
    for seg in inner_path.split('/').filter(|s| !s.is_empty()) {
        h.push('/');
        h.push_str(&utf8_percent_encode(seg, PATH_SEGMENT).to_string());
    }
    if is_dir || h == prefix {
        h.push('/');
    }
    h
}

fn multistatus(body: String) -> Response {
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<d:multistatus xmlns:d=\"DAV:\" xmlns:oc=\"http://owncloud.org/ns\" xmlns:nc=\"http://nextcloud.org/ns\">{body}</d:multistatus>"
    );
    let mut r = (StatusCode::MULTI_STATUS, xml).into_response();
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml; charset=utf-8"));
    r
}

async fn propfind(s: &AppState, auth: &Auth, prefix: &str, inner: &str, headers: &HeaderMap) -> Result<Response> {
    let node = s.core.resolve_path(&auth.user.id, inner).await?;
    let depth = headers.get("depth").and_then(|v| v.to_str().ok()).unwrap_or("1");
    let mut body = prop_xml(s, auth, &href_for(prefix, inner, node.is_dir()), &node).await?;
    if node.is_dir() && depth != "0" {
        for e in s.core.list_dir(&auth.user.id, &node.id).await? {
            let child_path = format!("{}/{}", inner.trim_end_matches('/'), e.node.name);
            body.push_str(&prop_xml(s, auth, &href_for(prefix, &child_path, e.node.is_dir()), &e.node).await?);
        }
    }
    Ok(multistatus(body))
}

/// Clients set properties such as modification times; we accept them without storing.
async fn proppatch(s: &AppState, auth: &Auth, prefix: &str, inner: &str) -> Result<Response> {
    let node = s.core.resolve_path(&auth.user.id, inner).await?;
    let body = format!(
        "<d:response><d:href>{}</d:href><d:propstat><d:prop/><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
        xml_escape(&href_for(prefix, inner, node.is_dir()))
    );
    Ok(multistatus(body))
}

async fn get(s: &AppState, auth: &Auth, inner: &str, req: Request) -> Result<Response> {
    let node = s.core.resolve_path(&auth.user.id, inner).await?;
    if node.is_dir() {
        let names: Vec<String> = s
            .core
            .list_dir(&auth.user.id, &node.id)
            .await?
            .into_iter()
            .map(|e| format!("{}{}", e.node.name, if e.node.is_dir() { "/" } else { "" }))
            .collect();
        return Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], names.join("\n")).into_response());
    }
    let (n, path) = s.core.open_file(&auth.user.id, &node.id).await?;
    let mut resp = crate::api::files::serve_blob(path, &n.name, &n.mime, &n.etag, false, req)
        .await
        .map_err(|e| e.0)?;
    resp.headers_mut()
        .insert("OC-FileId", HeaderValue::from_str(&n.id).unwrap_or_else(|_| HeaderValue::from_static("")));
    Ok(resp)
}

async fn put(s: &AppState, auth: &Auth, inner: &str, req: Request) -> Result<Response> {
    let (parent, name) = s.core.resolve_parent(&auth.user.id, inner).await.map_err(|e| match e {
        Error::NotFound => Error::Conflict("parent folder does not exist".into()),
        other => other,
    })?;
    let existed = s.core.resolve_path(&auth.user.id, inner).await.is_ok();
    let if_match = req.headers().get(header::IF_MATCH).and_then(|v| v.to_str().ok()).map(String::from);
    let if_none_match = req.headers().get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some("*");
    let lock_token = req
        .headers()
        .get("if")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split('<').nth(1))
        .and_then(|v| v.split('>').next())
        .map(String::from);
    let stream = req.into_body().into_data_stream().map_err(std::io::Error::other);
    let reader = tokio_util::io::StreamReader::new(stream);
    let opts = WriteOpts {
        if_match,
        create_only: if_none_match,
        lock_token,
        ..WriteOpts::user()
    };
    let node = match s.core.write_file(&auth.user.id, &parent.id, &name, reader, opts).await {
        Ok(n) => n,
        Err(Error::Conflict(_)) if if_none_match => {
            return Ok(StatusCode::PRECONDITION_FAILED.into_response());
        }
        Err(e) => return Err(e),
    };
    let mut r = if existed { StatusCode::NO_CONTENT } else { StatusCode::CREATED }.into_response();
    let h = r.headers_mut();
    h.insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{}\"", node.etag)).unwrap_or_else(|_| HeaderValue::from_static("\"\"")),
    );
    h.insert(
        "OC-ETag",
        HeaderValue::from_str(&format!("\"{}\"", node.etag)).unwrap_or_else(|_| HeaderValue::from_static("\"\"")),
    );
    h.insert("OC-FileId", HeaderValue::from_str(&node.id).unwrap_or_else(|_| HeaderValue::from_static("")));
    Ok(r)
}

async fn mkcol(s: &AppState, auth: &Auth, inner: &str) -> Result<Response> {
    if s.core.resolve_path(&auth.user.id, inner).await.is_ok() {
        return Ok(StatusCode::METHOD_NOT_ALLOWED.into_response());
    }
    let (parent, name) = s.core.resolve_parent(&auth.user.id, inner).await.map_err(|e| match e {
        Error::NotFound => Error::Conflict("parent folder does not exist".into()),
        other => other,
    })?;
    match s.core.mkdir(&auth.user.id, &parent.id, &name).await {
        Ok(_) => Ok(StatusCode::CREATED.into_response()),
        Err(Error::Conflict(_)) => Ok(StatusCode::METHOD_NOT_ALLOWED.into_response()),
        Err(e) => Err(e),
    }
}

async fn delete(s: &AppState, auth: &Auth, inner: &str) -> Result<Response> {
    let node = s.core.resolve_path(&auth.user.id, inner).await?;
    s.core.delete(&auth.user.id, &node.id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn move_copy(s: &AppState, auth: &Auth, inner: &str, headers: &HeaderMap, is_move: bool) -> Result<Response> {
    let node = s.core.resolve_path(&auth.user.id, inner).await?;
    let dest = headers
        .get("destination")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| Error::bad("Destination header required"))?;
    // Destination may be an absolute URL; keep only its path.
    let dest_path = match dest.find("://") {
        Some(i) => dest[i + 3..].find('/').map(|j| &dest[i + 3 + j..]).unwrap_or("/"),
        None => dest,
    };
    let (_, dest_inner) = split_dav_path(dest_path, &auth.user.username).ok_or(Error::Forbidden)?;
    let dest_inner = decode(&dest_inner);
    let overwrite = headers.get("overwrite").and_then(|v| v.to_str().ok()) != Some("F");
    let (parent, name) = s.core.resolve_parent(&auth.user.id, &dest_inner).await.map_err(|e| match e {
        Error::NotFound => Error::Conflict("destination folder does not exist".into()),
        other => other,
    })?;
    let existing = s.core.resolve_path(&auth.user.id, &dest_inner).await.ok();
    if let Some(ex) = &existing {
        if ex.id == node.id {
            return Ok(StatusCode::FORBIDDEN.into_response());
        }
        if !overwrite {
            return Ok(StatusCode::PRECONDITION_FAILED.into_response());
        }
        s.core.delete(&auth.user.id, &ex.id).await?;
    }
    if is_move {
        s.core.move_node(&auth.user.id, &node.id, &parent.id, &name, false).await?;
    } else {
        s.core.copy_node(&auth.user.id, &node.id, &parent.id, &name).await?;
    }
    Ok(if existing.is_some() { StatusCode::NO_CONTENT } else { StatusCode::CREATED }.into_response())
}

async fn lock(s: &AppState, auth: &Auth, prefix: &str, inner: &str) -> Result<Response> {
    let node = match s.core.resolve_path(&auth.user.id, inner).await {
        Ok(n) => n,
        Err(Error::NotFound) => {
            // LOCK on an unmapped URL creates an empty file (RFC 4918 §9.10.4).
            let (parent, name) = s.core.resolve_parent(&auth.user.id, inner).await?;
            s.core.write_bytes(&auth.user.id, &parent.id, &name, b"", WriteOpts::user()).await?
        }
        Err(e) => return Err(e),
    };
    if node.is_dir() {
        // Folder locks are not supported; report success with no lock so clients proceed.
        return Ok(StatusCode::OK.into_response());
    }
    let l = s.core.lock(&auth.user.id, &node.id, "webdav", 3600).await?;
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<d:prop xmlns:d=\"DAV:\"><d:lockdiscovery><d:activelock>\
         <d:locktype><d:write/></d:locktype><d:lockscope><d:exclusive/></d:lockscope><d:depth>0</d:depth>\
         <d:owner>{}</d:owner><d:timeout>Second-3600</d:timeout><d:locktoken><d:href>{}</d:href></d:locktoken>\
         <d:lockroot><d:href>{}</d:href></d:lockroot></d:activelock></d:lockdiscovery></d:prop>",
        xml_escape(&auth.user.username),
        xml_escape(&l.token),
        xml_escape(&href_for(prefix, inner, false))
    );
    let mut r = (StatusCode::OK, body).into_response();
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml; charset=utf-8"));
    r.headers_mut().insert(
        "Lock-Token",
        HeaderValue::from_str(&format!("<{}>", l.token)).unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    Ok(r)
}

async fn unlock(s: &AppState, auth: &Auth, inner: &str) -> Result<Response> {
    let node = s.core.resolve_path(&auth.user.id, inner).await?;
    s.core.unlock(&auth.user.id, &node.id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Nextcloud's `status.php`, probed by clients to detect a server.
pub async fn status_php() -> Response {
    let body = serde_json::json!({
        "installed": true, "maintenance": false, "needsDbUpgrade": false,
        "version": "30.0.0.0", "versionstring": "30.0.0", "edition": "",
        "productname": "Ferrite", "extendedSupport": false,
    });
    axum::Json(body).into_response()
}
