//! S1-F-03: errors are responses, not panics. `AppError` is what a request
//! handler's `?` collects a failed store call into - never the raw rusqlite
//! text (which can carry a file path), always a JSON body a client can
//! parse. Two constructors: `From<rusqlite::Error>` for anything that
//! reaches all the way to a genuine 500, and `bad_request` for a handler
//! that already knows the client sent something malformed (B20).

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// A response-shaped error: a status and a message a client can show.
/// `IntoResponse` renders it as `{"error": "..."}` at that status.
#[derive(Debug)]
pub struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    /// A 400 for a request the client sent wrong (bad JSON, a missing
    /// field) - the message is safe to show verbatim because the caller
    /// wrote it, not the database.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    /// A 404 for a resource that doesn't exist.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    /// A 409 when the request is valid but the resource is in the wrong state.
    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

/// Every store call on a request path lands here via `?`. The rusqlite
/// error (which can quote a file path or a column name) is logged
/// server-side only; the client gets a flat, generic 500.
impl From<String> for AppError {
    fn from(e: String) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: e,
        }
    }
}

impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: e.to_string(),
        }
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(err: rusqlite::Error) -> Self {
        tracing::error!("store error: {err}");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "internal error".to_string(),
        }
    }
}

/// What a fallible request handler returns instead of a bare value.
pub type ApiResult<T> = Result<T, AppError>;
