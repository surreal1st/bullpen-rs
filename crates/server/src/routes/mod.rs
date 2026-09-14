//! S0-04's first four routes: health, version, auth/status + auth/check,
//! roster. Same JSON shapes as the TS server where the ticket says so; see
//! `projects/bullpen-night/src/server/app.ts:1074-1112,2306-2312,3205-3240`.
//!
//! S1-06 adds the message/conversation/room routes, the round engine's HTTP
//! surface, and the two SSE streams - one submodule per route family, same
//! split as `tools/`.

mod conversations;
mod events;
mod messages;
mod rooms;
mod runs;

use crate::AppState;
use crate::auth::presented_token;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/version", get(version))
        .route("/api/auth/status", get(auth_status))
        .route("/api/auth/check", get(auth_check))
        .route("/api/roster", get(roster))
        .merge(conversations::router())
        .merge(rooms::router())
        .merge(messages::router())
        .merge(events::router())
        .merge(runs::router())
}

/// Build metadata baked at compile time. Not wired to the real git commit
/// yet (no build script in this slice) - set `BULLPEN_COMMIT` /
/// `BULLPEN_BUILT_AT` at build time once one exists.
const COMMIT: &str = match option_env!("BULLPEN_COMMIT") {
    Some(c) => c,
    None => "dev",
};
const BUILT_AT: &str = match option_env!("BULLPEN_BUILT_AT") {
    Some(b) => b,
    None => "unknown",
};

async fn health() -> impl IntoResponse {
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "commit": COMMIT,
    }))
}

async fn version() -> impl IntoResponse {
    Json(json!({
        "server": { "commit": COMMIT, "builtAt": BUILT_AT },
        "newestKnown": COMMIT,
    }))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthStatus {
    configured: bool,
    signed_in: bool,
    role: Option<&'static str>,
}

async fn auth_status(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let token = presented_token(&headers);
    let db = state.db.lock().expect("db mutex poisoned");
    let configured = store::is_configured(&db).expect("auth_settings query");
    let signed_in = store::session_valid(&db, &token).expect("sessions query");
    Json(AuthStatus {
        configured,
        signed_in,
        role: if signed_in { Some("owner") } else { None },
    })
}

async fn auth_check(State(state): State<AppState>, headers: HeaderMap) -> StatusCode {
    let token = presented_token(&headers);
    let db = state.db.lock().expect("db mutex poisoned");
    let signed_in = store::session_valid(&db, &token).expect("sessions query");
    if signed_in {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::UNAUTHORIZED
    }
}

#[derive(Serialize)]
struct RosterResponse {
    sections: Vec<shared::Section>,
    bots: Vec<shared::RosterEntry>,
}

async fn roster(State(state): State<AppState>) -> impl IntoResponse {
    let db = state.db.lock().expect("db mutex poisoned");
    let sections = store::list_sections(&db).expect("list_sections");
    let bots = store::list_roster(&db).expect("list_roster");
    Json(RosterResponse { sections, bots })
}
