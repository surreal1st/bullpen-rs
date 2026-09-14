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

use crate::auth::presented_token;
use crate::{ApiResult, AppError, AppState};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;

pub fn router() -> Router<AppState> {
    let router = Router::new()
        .route("/api/health", get(health))
        .route("/api/version", get(version))
        .route("/api/auth/status", get(auth_status))
        .route("/api/auth/check", get(auth_check))
        .route("/api/roster", get(roster))
        .merge(conversations::router())
        .merge(rooms::router())
        .merge(messages::router())
        .merge(events::router())
        .merge(runs::router());

    // B2: exists only so `tests/errors.rs` can prove the db mutex recovers
    // from a poison instead of panicking every request after the first -
    // stripped from release builds, same `#[cfg(debug_assertions)]` pattern
    // `main.rs` uses for the fake port.
    #[cfg(debug_assertions)]
    let router = router.route(
        "/api/__test/poison",
        axum::routing::post(poison_db_for_test),
    );

    router
}

/// Takes the db lock and panics while holding it - exactly how a panic
/// under the lock used to poison it before B1/B11/B12 removed every such
/// panic from a real request handler.
#[cfg(debug_assertions)]
async fn poison_db_for_test(State(state): State<AppState>) -> StatusCode {
    let _db = state.db();
    panic!("S1-F-03 test: intentionally poisoning the db mutex");
}

/// B20: a body that fails to parse gets a 400 naming the real fault, rather
/// than silently becoming the type's default via `unwrap_or_default()` -
/// which let a client's serialisation bug produce a `201 Created` on an
/// empty-titled room (or a "text is required" 400 that misnamed the real
/// fault) instead of a 400 that says the body did not parse. An empty body
/// still means "no fields given" and gets the type's default.
///
/// Returns `AppError` rather than a full `Response` so callers can just `?`
/// it - `clippy::result_large_err` rightly objects to an axum `Response`
/// (which carries a whole `http::Response<Body>`) living in a `Result`'s
/// `Err` variant.
pub(super) fn parse_body<T: Default + serde::de::DeserializeOwned>(
    body: &axum::body::Bytes,
) -> Result<T, AppError> {
    if body.is_empty() {
        return Ok(T::default());
    }
    serde_json::from_slice(body).map_err(|_| AppError::bad_request("invalid JSON body"))
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

async fn auth_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<impl IntoResponse> {
    let token = presented_token(&headers);
    let db = state.db();
    let configured = store::is_configured(&db)?;
    let signed_in = store::session_valid(&db, &token)?;
    Ok(Json(AuthStatus {
        configured,
        signed_in,
        role: if signed_in { Some("owner") } else { None },
    }))
}

async fn auth_check(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<StatusCode> {
    let token = presented_token(&headers);
    let db = state.db();
    let signed_in = store::session_valid(&db, &token)?;
    Ok(if signed_in {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::UNAUTHORIZED
    })
}

#[derive(Serialize)]
struct RosterResponse {
    sections: Vec<shared::Section>,
    bots: Vec<shared::RosterEntry>,
}

async fn roster(State(state): State<AppState>) -> ApiResult<impl IntoResponse> {
    let db = state.db();
    let sections = store::list_sections(&db)?;
    let bots = store::list_roster(&db)?;
    Ok(Json(RosterResponse { sections, bots }))
}
