//! S4-02: Routes for auto-review judge settings and log.
//!
//! GET /api/auto-review/judge -> {"enabled": bool}
//! PUT /api/auto-review/judge {"enabled": bool} -> same
//! GET /api/auto-review/log?limit=20 -> {"entries": [...]}

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::{ApiResult, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/auto-review/judge", get(get_judge).put(put_judge))
        .route("/api/auto-review/log", get(get_log))
}

/// GET /api/auto-review/judge - return whether auto-review judge is enabled
async fn get_judge(State(state): State<AppState>) -> ApiResult<Response> {
    let db = state.db();
    let enabled = crate::judge::judge_enabled(&db);
    Ok(Json(json!({ "enabled": enabled })).into_response())
}

#[derive(Deserialize, Default)]
struct JudgeBody {
    enabled: Option<bool>,
}

/// PUT /api/auto-review/judge - enable/disable auto-review judge
async fn put_judge(State(state): State<AppState>, body: axum::body::Bytes) -> ApiResult<Response> {
    let db = state.db();
    let parsed: JudgeBody = super::parse_body(&body)?;

    if let Some(enabled) = parsed.enabled {
        crate::judge::set_judge_enabled(&db, enabled)?;
    }

    let enabled = crate::judge::judge_enabled(&db);
    Ok(Json(json!({ "enabled": enabled })).into_response())
}

#[derive(Deserialize, Default)]
struct LogQuery {
    limit: Option<u32>,
}

/// GET /api/auto-review/log - list recent judgements
async fn get_log(
    State(state): State<AppState>,
    Query(query): Query<LogQuery>,
) -> ApiResult<Response> {
    let db = state.db();
    let limit = query.limit.unwrap_or(20);
    let entries = crate::judge::list_log(&db, limit)?;
    Ok(Json(json!({ "entries": entries })).into_response())
}
