//! S5b-06: routes for webhook management and delivery.
//!
//! Routes:
//! POST /api/routines/:id/hook -> {"secret": "...", "url": "..."} (201)
//! DELETE /api/routines/:id/hook -> {"ok": true}
//! POST /api/hooks/:routineId -> trigger routine from webhook (202 or 204)
//!
//! The webhook secret is returned ONCE by the mint route and never again.
//! `GET /api/routines` only ever says `hasHook: true`. The secret is never
//! logged or echoed back in error bodies.
//!
//! The POST /api/hooks route is unauthenticated (open via `auth.rs`). Signature
//! verification per `hook_kind` happens BEFORE parsing the payload. A reduced
//! payload is external data - it reaches the routine's prompt inside the "what
//! arrived" block, never as instructions.

use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;
use subtle::ConstantTimeEq;

use crate::ApiResult;
use crate::AppError;
use crate::AppState;
use crate::hooks;
use store::{clear_routine_hook, mint_routine_hook, routine_by_id, routine_row_by_id};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/routines/{id}/hook",
            post(post_routine_hook).delete(delete_routine_hook),
        )
        .route("/api/hooks/{routine_id}", post(post_webhook))
}

/// POST /api/routines/:id/hook - mint a fresh webhook secret. Returns 201
/// with {secret, url} if the routine exists. The secret is never returned
/// again. Returns 404 if the routine doesn't exist.
async fn post_routine_hook(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let db = state.db();

    // Check if routine exists first
    let routine = routine_by_id(&db, &id)?;
    if routine.is_none() {
        return Err(AppError::not_found("no such routine"));
    }

    match mint_routine_hook(&db, &id)? {
        Some(secret) => {
            let public_url =
                std::env::var("PUBLIC_URL").unwrap_or_else(|_| "http://localhost:4380".to_string());
            Ok((
                StatusCode::CREATED,
                Json(json!({
                    "secret": secret,
                    "url": format!("{}/api/hooks/{}", public_url, id)
                })),
            ))
        }
        None => Err(AppError::not_found("no such routine")),
    }
}

/// DELETE /api/routines/:id/hook - clear the webhook secret. Returns 200 with
/// {ok: true} if the routine exists and was updated. Returns 404 if the
/// routine doesn't exist.
async fn delete_routine_hook(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    match clear_routine_hook(&db, &id)? {
        true => Ok(Json(json!({"ok": true}))),
        false => Err(AppError::not_found("no such routine")),
    }
}

/// POST /api/hooks/:routineId - receive a webhook delivery and trigger the
/// routine. This is the only unauthenticated route that reads a body.
async fn post_webhook(
    State(_state): State<AppState>,
    Path(routine_id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // Get routine row (includes hook_secret)
    let db = _state.db();
    let row = match routine_row_by_id(&db, &routine_id) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "no such webhook"})),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal error"})),
            )
                .into_response();
        }
    };

    // Check that secret exists
    if row.hook_secret.is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no such webhook"})),
        )
            .into_response();
    }

    // Check body size cap
    const HOOK_BODY_MAX_BYTES: usize = 64 * 1024;
    if body.len() > HOOK_BODY_MAX_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({"error": "body too large"})),
        )
            .into_response();
    }

    let raw_body = String::from_utf8_lossy(&body).to_string();

    // Determine which signature kind to use based on headers (S6b conditions logic)
    let has_conditions = row.conditions.is_some();
    let configured_kind = match row.hook_kind.as_str() {
        "github" => "github",
        "sentry" => "sentry",
        "linear" => "linear",
        "pagerduty" => "pagerduty",
        _ => "raw",
    };

    let mut hook_kind = configured_kind.to_string();
    if has_conditions {
        if headers.get("x-hub-signature-256").is_some() {
            hook_kind = "github".to_string();
        } else if headers.get("linear-signature").is_some() {
            hook_kind = "linear".to_string();
        } else if headers.get("x-pagerduty-signature").is_some() {
            hook_kind = "pagerduty".to_string();
        } else {
            hook_kind = "raw".to_string();
        }
    }

    // Verify signature based on kind
    let secret = row.hook_secret.as_ref().unwrap();
    match hook_kind.as_str() {
        "github" => {
            let header = headers
                .get("x-hub-signature-256")
                .and_then(|v| v.to_str().ok());
            if !hooks::verify_github_signature(secret, &raw_body, header) {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": "not authorized"})),
                )
                    .into_response();
            }
        }
        "linear" => {
            let header = headers
                .get("linear-signature")
                .and_then(|v| v.to_str().ok());
            if !hooks::verify_linear_signature(secret, &raw_body, header) {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": "not authorized"})),
                )
                    .into_response();
            }
        }
        "pagerduty" => {
            let header = headers
                .get("x-pagerduty-signature")
                .and_then(|v| v.to_str().ok());
            if !hooks::verify_pager_duty_signature(secret, &raw_body, header) {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": "not authorized"})),
                )
                    .into_response();
            }
        }
        _ => {
            // "raw" or "sentry" - check bearer token
            let auth_header = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            let bearer = if let Some(token) = auth_header.strip_prefix("Bearer ") {
                token
            } else {
                ""
            };
            if !verify_hook_secret(secret, bearer) {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": "not authorized"})),
                )
                    .into_response();
            }
        }
    }

    // TODO: Continue with payload reduction, matching, and routine firing

    (StatusCode::NO_CONTENT, Json(json!({}))).into_response()
}

/// Verify a webhook bearer token in constant time.
fn verify_hook_secret(secret: &str, presented: &str) -> bool {
    if secret.is_empty() || presented.is_empty() {
        return false;
    }
    let expected_bytes = secret.as_bytes();
    let presented_bytes = presented.as_bytes();
    if expected_bytes.len() != presented_bytes.len() {
        return false;
    }
    // Constant-time comparison using subtle crate
    expected_bytes.ct_eq(presented_bytes).into()
}
