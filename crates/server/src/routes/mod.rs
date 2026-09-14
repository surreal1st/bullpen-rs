//! S0-04's first four routes: health, version, auth/status + auth/check,
//! roster. Same JSON shapes as the TS server where the ticket says so; see
//! `projects/bullpen-night/src/server/app.ts:1074-1112,2306-2312,3205-3240`.
//!
//! S1-06 adds the message/conversation/room routes, the round engine's HTTP
//! surface, and the two SSE streams - one submodule per route family, same
//! split as `tools/`.

mod auth;
mod conversations;
mod events;
mod messages;
mod rooms;
mod runs;

use crate::auth::presented_token;
use crate::{ApiResult, AppError, AppState};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::OptionalExtension;
use serde::Serialize;
use serde_json::json;

pub fn router() -> Router<AppState> {
    let router = Router::new()
        .route("/api/health", get(health))
        .route("/api/version", get(version))
        .route("/api/auth/status", get(auth_status))
        .route("/api/auth/check", get(auth_check))
        .route("/api/roster", get(roster))
        .route("/api/bots/{id}/seen", post(mark_bot_seen))
        .route("/api/bots/{id}/unseen", post(mark_bot_unseen))
        .merge(auth::router())
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

fn no_such_bot() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"error": "no such bot"}))).into_response()
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// One millisecond before `iso`, same format - the timestamp `mark_bot_unseen`
/// backdates `last_seen_at` to. Falls back to `iso` itself on a parse
/// failure (a hand-seeded test row, say) rather than erroring the whole
/// request over a cosmetic one-millisecond miss.
fn backdate_1ms(iso: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(iso) {
        Ok(dt) => (dt.with_timezone(&chrono::Utc) - chrono::Duration::milliseconds(1))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        Err(_) => iso.to_string(),
    }
}

/// F14 (S1-F-11): `POST /api/bots/:id/seen`, ported from `app.ts:2366-2374`'s
/// `markSeen`. Opening a bot's conversation is what makes it read - stamped
/// with "now" rather than the newest message's time, so a reply that lands
/// while it is open counts as seen instead of reappearing as unread the
/// moment the pane closes. Store has no query API for the `bots` table's
/// write half yet (`crates/store/src/bots.rs` is read-only), so this writes
/// the one column directly - same posture `crate::rooms`/`runs.rs` already
/// take with `db.conn()` for writes their own crate has no query for.
async fn mark_bot_seen(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let db = state.db();
    if store::get_bot(&db, &id)?.is_none() {
        return Ok(no_such_bot());
    }
    db.conn().execute(
        "UPDATE bots SET last_seen_at = ?1 WHERE id = ?2",
        rusqlite::params![now_iso(), id],
    )?;
    let bots = store::list_roster(&db)?;
    Ok(Json(json!({ "bots": bots })).into_response())
}

/// The menu's "Mark as Unread" - the deliberate opposite of `mark_bot_seen`.
/// Ported from `app.ts:2377-2382`'s `markUnread`. Unread is a TIMESTAMP
/// comparison (`created_at > last_seen_at`, `store::roster::list_roster`),
/// not a flag, so there is no boolean to clear - this backs `last_seen_at`
/// up to one millisecond before the bot's own last assistant message, which
/// makes that one message (and nothing further back) count as unread again.
/// "The bot's own" deliberately excludes room conversations (`kind = 'room'`),
/// matching `list_roster`'s own unread count, which already excludes them
/// for the same bot id (a room's owner is a real bot row too) - backdating
/// off a room reply would move this bot's clock without the roster's own
/// count ever agreeing unread went up. This is a documented narrowing of
/// the TS query, which does not filter by kind, to match the Rust roster's
/// own rule. A bot with no assistant message yet is a harmless no-op, same
/// as the TS.
async fn mark_bot_unseen(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let db = state.db();
    if store::get_bot(&db, &id)?.is_none() {
        return Ok(no_such_bot());
    }
    let last_at: Option<String> = db
        .conn()
        .query_row(
            "SELECT m.created_at
               FROM messages m
               JOIN conversations c ON c.id = m.conversation_id
              WHERE c.bot_id = ?1 AND c.kind != 'room' AND m.role = 'assistant'
              ORDER BY m.created_at DESC, m.seq DESC LIMIT 1",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(last_at) = last_at {
        db.conn().execute(
            "UPDATE bots SET last_seen_at = ?1 WHERE id = ?2",
            rusqlite::params![backdate_1ms(&last_at), id],
        )?;
    }
    let bots = store::list_roster(&db)?;
    Ok(Json(json!({ "bots": bots })).into_response())
}
