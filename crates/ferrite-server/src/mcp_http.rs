//! MCP over Streamable HTTP at `/mcp`. Authenticate with an app token
//! (`Authorization: Bearer ft_…`, scope `mcp`). Serves modern (2026-07-28) clients statelessly
//! and legacy (`initialize`) clients from the same endpoint.

use crate::AppState;
use crate::tools::UserCtx;
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use ferrite_mcp::{INVALID_REQUEST, LEGACY_VERSIONS, PARSE_ERROR, RpcError, error_response, server::validate_http_headers};
use serde_json::Value;

fn rpc_error(status: StatusCode, id: Option<&Value>, e: RpcError) -> Response {
    (status, Json(error_response(id, &e))).into_response()
}

pub async fn post(State(s): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    // DNS rebinding protection: browsers always send Origin; it must be ours.
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
        if origin.trim_end_matches('/') != s.core.config.public_url && origin.split("://").nth(1) != Some(host) {
            return rpc_error(StatusCode::FORBIDDEN, None, RpcError::new(INVALID_REQUEST, "origin not allowed"));
        }
    }
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return rpc_error(StatusCode::BAD_REQUEST, None, RpcError::new(PARSE_ERROR, format!("parse error: {e}")));
        }
    };
    if msg.is_array() {
        return rpc_error(StatusCode::BAD_REQUEST, None, RpcError::new(INVALID_REQUEST, "batches are not supported"));
    }
    let id = msg.get("id").cloned();
    let auth = match crate::auth::authenticate(&s, &headers, &axum::http::Method::POST, false).await {
        Ok(a) if a.token.allows("mcp") => a,
        Ok(_) => {
            return rpc_error(
                StatusCode::FORBIDDEN,
                id.as_ref(),
                RpcError::new(INVALID_REQUEST, "this token lacks the `mcp` scope"),
            );
        }
        Err(_) => {
            let mut r = rpc_error(
                StatusCode::UNAUTHORIZED,
                id.as_ref(),
                RpcError::new(
                    INVALID_REQUEST,
                    "authentication required: create an app token in Ferrite (Settings → Tokens) and send it as `Authorization: Bearer <token>`",
                ),
            );
            r.headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer realm=\"ferrite\""));
            return r;
        }
    };
    let has_meta = msg.pointer("/params/_meta/io.modelcontextprotocol~1protocolVersion").is_some();
    let header_version = headers.get("mcp-protocol-version").and_then(|v| v.to_str().ok());
    // Legacy clients: a legacy version header, or (pre-2025-06-18) no header and no _meta.
    let legacy = match header_version {
        Some(v) if LEGACY_VERSIONS.contains(&v) && !has_meta => Some(v.to_string()),
        None if !has_meta => Some("2025-03-26".to_string()),
        _ => None,
    };
    if has_meta {
        let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(String::from);
        if let Err(e) = validate_http_headers(&msg, get) {
            return rpc_error(StatusCode::BAD_REQUEST, id.as_ref(), e);
        }
    }
    let ctx = UserCtx {
        user: auth.user,
        scopes: auth.token.scopes,
    };
    let reply = s.mcp.handle(&ctx, &msg, legacy.as_deref()).await;
    let status = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::OK);
    let mut resp = match reply.body {
        Some(b) => (status, Json(b)).into_response(),
        None => status.into_response(),
    };
    if reply.negotiated_legacy.is_some() {
        // Legacy Streamable HTTP clients expect a session id; the protocol state is per request
        // here, so any id is accepted back.
        let sid = ferrite_core::util::random_token(16);
        if let Ok(v) = HeaderValue::from_str(&sid) {
            resp.headers_mut().insert("mcp-session-id", v);
        }
    }
    resp
}

/// GET (legacy standalone SSE stream) and DELETE (legacy session end) are not offered.
pub async fn other() -> Response {
    let mut r = StatusCode::METHOD_NOT_ALLOWED.into_response();
    r.headers_mut().insert(header::ALLOW, HeaderValue::from_static("POST"));
    r
}
