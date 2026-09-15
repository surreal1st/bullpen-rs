//! S5-04: Routes for routine scheduling and management.
//!
//! Routes:
//! GET /api/routines -> {"routines": [...]}
//! POST /api/routines -> {"routine": {...}} (201)
//! PATCH /api/routines/:id -> {"routine": {...}}
//! POST /api/routines/:id/active -> {"routine": {...}}
//! DELETE /api/routines/:id -> {"ok": true}
//! GET /api/routines/:id/runs -> {"runs": [...]}
//! POST /api/routines/tick -> {"started": [...]} (S5-03)
//! POST /api/routines/:id/run -> {"runId": "..."} (201) (S5-03)

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::{ApiResult, AppError, AppState};
use store::{
    create_routine, delete_routine, list_routines, routine_runs, set_routine_active, update_routine,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/routines", get(get_routines).post(post_routines))
        .route(
            "/api/routines/{id}",
            patch(patch_routine).delete(delete_routine_handler),
        )
        .route("/api/routines/{id}/active", post(post_routine_active))
        .route("/api/routines/{id}/runs", get(get_routine_runs))
        .route("/api/routines/tick", post(post_routines_tick))
        .route("/api/routines/{id}/run", post(post_routine_run))
}

#[derive(Deserialize, Default)]
struct ListQuery {
    bot: Option<String>,
}

/// GET /api/routines - list routines for a bot or all
async fn get_routines(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    let routines = list_routines(&db, query.bot.as_deref())?;
    Ok(Json(json!({ "routines": routines })))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct CreateRoutineBody {
    bot_id: String,
    name: String,
    prompt: String,
    schedule: String,
    #[serde(default)]
    tools: Option<Vec<String>>,
    kind: Option<String>,
    tool: Option<String>,
    tool_args: Option<String>,
    hook_kind: Option<String>,
    hook_events: Option<Vec<String>>,
    hook_match: Option<String>,
    conditions: Option<Vec<store::Condition>>,
}

/// POST /api/routines - create a new routine
async fn post_routines(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let db = state.db();
    let body_data: CreateRoutineBody = super::parse_body(&body)?;

    // Parse the schedule to compute next_run_at and validate it
    let schedule_parsed =
        crate::schedule::parse_schedule(&body_data.schedule).map_err(AppError::bad_request)?;

    let now = chrono::Utc::now();
    let next_run_at = crate::schedule::next_run(&schedule_parsed, now).to_rfc3339();

    // Call the store to create the routine - it will check the 50-cap
    let result = create_routine(
        &db,
        &body_data.bot_id,
        &body_data.name,
        &body_data.prompt,
        body_data.schedule,
        Some(next_run_at),
        body_data.tools,
        body_data.kind.as_deref(),
        body_data.tool.as_deref(),
        body_data.tool_args.as_deref(),
        body_data.hook_kind.as_deref(),
        body_data.hook_events,
        body_data.hook_match.as_deref(),
        body_data.conditions,
    );

    match result {
        Ok(id) => {
            // Retrieve the created routine to return it
            let routine = store::routine_by_id(&db, &id)?
                .ok_or_else(|| AppError::not_found("routine not found"))?;
            Ok((StatusCode::CREATED, Json(json!({ "routine": routine }))))
        }
        Err(e) => Err(AppError::bad_request(e)),
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct UpdateRoutineBody {
    name: Option<String>,
    prompt: Option<String>,
    schedule: Option<String>,
    tools: Option<Option<Vec<String>>>,
    kind: Option<String>,
    tool: Option<String>,
    tool_args: Option<String>,
    hook_kind: Option<String>,
    hook_events: Option<Option<Vec<String>>>,
    hook_match: Option<Option<String>>,
    conditions: Option<Option<Vec<store::Condition>>>,
    second_opinion: Option<bool>,
}

/// PATCH /api/routines/:id - update a routine
async fn patch_routine(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    let body_data: UpdateRoutineBody = super::parse_body(&body)?;

    // If schedule is being updated, validate and compute new next_run_at
    let mut next_run_at = None;
    if let Some(ref schedule_text) = body_data.schedule {
        let schedule_parsed =
            crate::schedule::parse_schedule(schedule_text).map_err(AppError::bad_request)?;
        let now = chrono::Utc::now();
        next_run_at = Some(crate::schedule::next_run(&schedule_parsed, now).to_rfc3339());
    }

    // Build the update fields
    let mut updates = store::UpdateRoutineFields::default();
    if let Some(name) = body_data.name {
        updates.name = Some(name);
    }
    if let Some(prompt) = body_data.prompt {
        updates.prompt = Some(prompt);
    }
    if let Some(schedule) = body_data.schedule {
        updates.schedule = Some(schedule);
    }
    if let Some(nra) = next_run_at {
        updates.next_run_at = Some(Some(nra));
    }
    if let Some(tools) = body_data.tools {
        updates.tools = Some(tools);
    }
    if let Some(kind) = body_data.kind {
        updates.kind = Some(kind);
    }
    if let Some(tool) = body_data.tool {
        updates.tool = Some(Some(tool));
    }
    if let Some(tool_args) = body_data.tool_args {
        updates.tool_args = Some(Some(tool_args));
    }
    if let Some(hook_kind) = body_data.hook_kind {
        updates.hook_kind = Some(hook_kind);
    }
    if let Some(hook_events) = body_data.hook_events {
        updates.hook_events = Some(hook_events);
    }
    if let Some(hook_match) = body_data.hook_match {
        updates.hook_match = Some(hook_match);
    }
    if let Some(conditions) = body_data.conditions {
        updates.conditions = Some(conditions);
    }
    if let Some(second_opinion) = body_data.second_opinion {
        updates.second_opinion = Some(second_opinion);
    }

    update_routine(&db, &id, &updates)?;

    let routine =
        store::routine_by_id(&db, &id)?.ok_or_else(|| AppError::not_found("no such routine"))?;

    Ok(Json(json!({ "routine": routine })))
}

#[derive(Deserialize, Default)]
struct ActiveBody {
    active: Option<bool>,
}

/// POST /api/routines/:id/active - toggle routine active state
async fn post_routine_active(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    let body_data: ActiveBody = super::parse_body(&body)?;
    let active = body_data.active.unwrap_or(true);

    // If activating, resume the routine (clear pause reason)
    if active {
        store::resume_routine(&db, &id)?;
    }

    set_routine_active(&db, &id, active, None)?;

    let routine =
        store::routine_by_id(&db, &id)?.ok_or_else(|| AppError::not_found("no such routine"))?;

    Ok(Json(json!({ "routine": routine })))
}

/// DELETE /api/routines/:id - delete a routine
async fn delete_routine_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    delete_routine(&db, &id)?;
    Ok(Json(json!({ "ok": true })))
}

/// GET /api/routines/:id/runs - get routine run history (max 20)
async fn get_routine_runs(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    let runs = routine_runs(&db, &id, 20)?;
    Ok(Json(json!({ "runs": runs })))
}

/// POST /api/routines/tick - fire all due routines (S5-03)
async fn post_routines_tick(State(_state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    // S5-03 not yet implemented - stub with error
    Err(AppError::bad_request(
        "TODO: S5-03 fireDue not implemented".to_string(),
    ))
}

/// POST /api/routines/:id/run - run a routine immediately (S5-03)
async fn post_routine_run(
    State(_state): State<AppState>,
    Path(_id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    // S5-03 not yet implemented - stub with error
    Err(AppError::bad_request(
        "TODO: S5-03 runRoutineNow not implemented".to_string(),
    ))
}
