//! The working indicator and stopping a run. Port of
//! `src/server/app.ts:1688` (`GET /api/conversations/:id/working`) and
//! `app.ts:2536` (`POST /api/runs/:id/stop`).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::{ApiResult, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/conversations/{id}/working", get(working))
        .route("/api/runs/{id}/stop", axum::routing::post(stop))
}

/// 🔴 A route rather than something assembled from the client's own SSE
/// stream, because the client only ever holds a stream for a run IT
/// started. A room round chains members two, three and four from
/// `on_run_done` with no HTTP response to write to, and a routine (once one
/// exists) fires with no tab open at all - so an indicator built from the
/// tab's own events would show one face out of four. This reads the run
/// rows, so it sees every one of them.
async fn working(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    let working = state.runs.working(&id)?;
    Ok(Json(json!({ "working": working })).into_response())
}

async fn stop(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    // F17: TS `stop` (`runs.ts:791-795`) refuses only `done`/`failed` - a run
    // parked `waiting` (S2's approvals) is still stoppable. Matching only
    // `status = 'running'` answered 404 for a waiting run even though
    // `RunManager::working` already reports it as working.
    let stoppable = {
        let db = state.db();
        db.conn()
            .query_row(
                "SELECT 1 FROM runs WHERE id = ?1 AND status NOT IN ('done', 'failed')",
                rusqlite::params![id],
                |_| Ok(()),
            )
            .is_ok()
    };
    if !stoppable {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "that run is not running"})),
        )
            .into_response();
    }
    state.runs.stop(&id);
    Json(json!({ "ok": true })).into_response()
}
