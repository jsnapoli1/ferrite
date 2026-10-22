//! The web app, embedded in the binary (no separate web server or build step needed).

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "web/"]
struct Assets;

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; \
                   media-src 'self' blob: mediastream:; connect-src 'self' ws: wss:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

fn asset(path: &str) -> Option<Response> {
    let file = Assets::get(path)?;
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let mut r = Response::new(Body::from(file.data.into_owned()));
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime.as_ref()).unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    let etag = format!("\"{}\"", hex::encode(&file.metadata.sha256_hash()[..12]));
    h.insert(header::ETAG, HeaderValue::from_str(&etag).unwrap_or_else(|_| HeaderValue::from_static("\"\"")));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    Some(r)
}

/// Serves static files; any other path gets the app shell (client-side routing).
pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if !path.is_empty() && !path.ends_with('/') {
        if let Some(r) = asset(path) {
            return r;
        }
        // Missing static files are real 404s, not app routes.
        if path.contains('.') && !path.starts_with("s/") {
            return StatusCode::NOT_FOUND.into_response();
        }
    }
    match asset("index.html") {
        Some(mut r) => {
            let h = r.headers_mut();
            h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
            h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
            h.insert(header::REFERRER_POLICY, HeaderValue::from_static("same-origin"));
            h.insert(
                "Permissions-Policy",
                HeaderValue::from_static("camera=(self), microphone=(self), display-capture=(self), geolocation=()"),
            );
            r
        }
        None => (StatusCode::NOT_FOUND, "web assets missing").into_response(),
    }
}
