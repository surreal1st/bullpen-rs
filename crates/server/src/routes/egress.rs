//! S6-W-02: Per-bot egress policy routes (mode + allow list).
//!
//! `GET /api/bots/:id/egress` reads a bot's current egress configuration.
//! `PATCH /api/bots/:id/egress` sets a new configuration, validating it with
//! `sanitize_bot_egress` before storage.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch};
use axum::{Json, Router};
use serde_json::json;

use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/bots/{id}/egress", get(get_bot_egress))
        .route("/api/bots/{id}/egress", patch(patch_bot_egress))
}

fn no_such_bot() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"error": "no such bot"}))).into_response()
}

/// Get a bot's current egress configuration.
async fn get_bot_egress(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, crate::AppError> {
    let db = state.db();

    // Check that the bot exists
    if store::get_bot(&db, &id)?.is_none() {
        return Ok(no_such_bot());
    }

    // Get the raw egress JSON
    let egress_json = store::get_bot_egress(&db, &id)?;

    // Parse and return
    let config = match egress_json {
        Some(raw) => crate::egress::parse_bot_egress(Some(&raw)),
        None => crate::egress::DEFAULT_BOT_EGRESS,
    };

    Ok((
        StatusCode::OK,
        Json(json!({
            "mode": config.mode.as_str(),
            "allow": config.allow,
        })),
    )
        .into_response())
}

/// Set a bot's egress configuration.
async fn patch_bot_egress(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, crate::AppError> {
    let db = state.db();

    // Check that the bot exists
    if store::get_bot(&db, &id)?.is_none() {
        return Ok(no_such_bot());
    }

    // Parse the incoming JSON
    let parsed: serde_json::Value = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&body)
            .map_err(|_| crate::AppError::bad_request("invalid JSON body"))?
    };

    // Validate and sanitize the input
    let config = crate::egress::sanitize_bot_egress(&parsed);

    // Serialize back to JSON for storage
    let egress_json = serde_json::to_string(&json!({
        "mode": config.mode.as_str(),
        "allow": config.allow,
    }))?;

    // Store it
    store::set_bot_egress(&db, &id, &egress_json)?;

    // Return the canonical form
    Ok((
        StatusCode::OK,
        Json(json!({
            "mode": config.mode.as_str(),
            "allow": config.allow,
        })),
    )
        .into_response())
}
