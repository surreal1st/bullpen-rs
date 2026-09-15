//! S5c-03: `POST /api/teams/events`, the Teams stub route. Port of
//! `app.ts:2726-2736`. Open (no session) via `auth::OPEN_PATHS` - see that
//! module's doc for why this is safe: `crate::teams::teams_enabled` gates
//! everything behind `BULLPEN_TEAMS`, and even with it on this refuses
//! everything real (`verify_teams_client_state` always false).

use std::collections::HashMap;

use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;

use crate::AppState;
use crate::teams;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/teams/events", post(post_teams_events))
}

/// `BULLPEN_TEAMS` unset -> 404 "Teams is not enabled". With it on: a Graph
/// subscription-validation handshake (`?validationToken=`) is echoed back
/// verbatim as `text/plain`; anything else is 501 "not implemented" - there
/// is no real subscription/notification handling yet (see `crate::teams`'s
/// doc on what that would take).
async fn post_teams_events(Query(params): Query<HashMap<String, String>>) -> Response {
    if !teams::teams_enabled() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Teams is not enabled"})),
        )
            .into_response();
    }

    if let Some(token) = teams::teams_validation_token(&params) {
        return (StatusCode::OK, token).into_response();
    }

    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({"error": "not implemented"})),
    )
        .into_response()
}
