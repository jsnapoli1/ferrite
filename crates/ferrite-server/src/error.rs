use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use ferrite_core::Error;
use serde_json::json;

/// API error: a core error rendered as `{"error": "..."}` with a fitting status code.
#[derive(Debug)]
pub struct ApiError(pub Error);

pub type ApiResult<T> = Result<T, ApiError>;

impl<E: Into<Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(e.into())
    }
}

pub fn status_of(e: &Error) -> StatusCode {
    match e {
        Error::NotFound => StatusCode::NOT_FOUND,
        Error::Unauthorized => StatusCode::UNAUTHORIZED,
        Error::Forbidden => StatusCode::FORBIDDEN,
        Error::BadRequest(_) => StatusCode::BAD_REQUEST,
        Error::Conflict(_) => StatusCode::CONFLICT,
        Error::Locked(_) => StatusCode::LOCKED,
        Error::QuotaExceeded => StatusCode::INSUFFICIENT_STORAGE,
        Error::PreconditionFailed => StatusCode::PRECONDITION_FAILED,
        Error::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        Error::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Message safe to show to clients (internal details are logged, not returned).
pub fn public_message(e: &Error) -> String {
    match e {
        Error::Internal(inner) => {
            tracing::error!(error = ?inner, "internal error");
            "internal server error".into()
        }
        other => other.to_string(),
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = status_of(&self.0);
        (status, Json(json!({"error": public_message(&self.0)}))).into_response()
    }
}
