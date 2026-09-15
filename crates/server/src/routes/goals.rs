//! S5b-04: routes for goal scheduling and management. Port of `app.ts:3496-
//! 3572` (`GET/POST /api/goals`, `PATCH/DELETE /api/goals/:id`, `GET /api/
//! goals/:id/runs`, `POST /api/goals/tick`, `POST /api/goals/:id/run`),
//! verbatim paths and response shapes.
//!
//! `list_goals` (`store::goals`) deliberately omits the room-visibility
//! `MaybeScope` TS's `listGoals` also takes - S5b-03's own Results block
//! (`.scratch/bullpen-rs/tickets/S5b-tickets.md`) already made this call,
//! matching `list_routines`'s existing convention in this crate; `GET /api/
//! goals` here just passes `bot` straight through, same as `GET /api/
//! routines`.
//!
//! `PATCH /api/goals/:id` reads the body as a raw `serde_json::Value`
//! instead of a typed struct for `budgetTokens`/`budgetUntil`: those two
//! fields need to tell "key omitted" (leave unchanged) apart from "key is
//! JSON `null`" (clear it) apart from "key is a value" (set it), and a plain
//! `Option<Option<T>>` struct field cannot make that distinction through
//! serde's derive - `Option<T>::deserialize` answers `None` for BOTH a
//! missing key and an explicit `null`, collapsing exactly the two cases that
//! must stay apart. Reading the value out of a `serde_json::Value` with
//! `.get()` (present/absent) and `.as_f64()`/`.as_str()` (value/null) is the
//! only way to reproduce the TS route's own `"budgetTokens" in body` /
//! `raw === null` checks (`app.ts:3542-3551`).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::{ApiResult, AppError, AppState};
use store::goals;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/goals", get(get_goals).post(post_goals))
        .route(
            "/api/goals/{id}",
            patch(patch_goal).delete(delete_goal_handler),
        )
        .route("/api/goals/{id}/runs", get(get_goal_runs))
        .route("/api/goals/tick", post(post_goals_tick))
        .route("/api/goals/{id}/run", post(post_goal_run))
}

#[derive(Deserialize, Default)]
struct ListQuery {
    bot: Option<String>,
}

/// GET /api/goals - list goals for a bot or all.
async fn get_goals(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    let list = goals::list_goals(&db, query.bot.as_deref())?;
    Ok(Json(json!({ "goals": list })))
}

/// POST /api/goals - create a new goal. Port of `app.ts:3501-3514`: always a
/// 400 on failure (no 404 branch on create, unlike the routes below - `no
/// such bot` included), matching `createGoal`'s own `{ok, goal, error}`
/// shape collapsed to a single status. Parses body as raw JSON like PATCH
/// does, coercing types like TS (missing budgetTokens/budgetUntil treated as
/// absent, non-matching types coerced away).
async fn post_goals(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let db = state.db();
    let value: serde_json::Value = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&body).map_err(|_| AppError::bad_request("invalid JSON body"))?
    };

    // Extract string fields with type coercion, matching TS's `text` helper.
    let text = |key: &str| -> String {
        value
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };

    let objective = text("objective");
    let done_when = text("doneWhen");

    if objective.is_empty() {
        return Err(AppError::bad_request("Say what you are working toward."));
    }
    if done_when.is_empty() {
        return Err(AppError::bad_request("Say what done looks like."));
    }

    // Extract numeric/string fields only if they match the type.
    let budget_tokens = value.get("budgetTokens").and_then(|v| v.as_f64());
    let budget_until = value
        .get("budgetUntil")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let input = goals::CreateGoalInput {
        bot_id: text("botId"),
        objective,
        done_when,
        budget_tokens,
        budget_until,
    };
    match goals::create_goal(&db, input, chrono::Utc::now()) {
        Ok(goal) => Ok((StatusCode::CREATED, Json(json!({ "goal": goal })))),
        Err(e) => Err(AppError::bad_request(e)),
    }
}

/// PATCH /api/goals/:id - update a goal. Port of `app.ts:3516-3556`. Called
/// with `bot_id: None` - the admin path (Josh's own goal page), same as
/// `store::goals::update_goal`'s own doc distinguishes from the tool-call
/// path (bot-scoped, not reachable over HTTP at all).
async fn patch_goal(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    let value: serde_json::Value = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&body).unwrap_or_else(|_| json!({}))
    };

    let mut patch = goals::UpdateGoalPatch::default();
    if let Some(s) = value.get("status").and_then(|v| v.as_str())
        && goals::is_goal_status(s)
    {
        patch.status = Some(s.to_string());
    }
    if let Some(s) = value.get("plan").and_then(|v| v.as_str()) {
        patch.plan = Some(s.to_string());
    }
    if let Some(s) = value.get("note").and_then(|v| v.as_str()) {
        patch.note = Some(s.to_string());
    }
    if let Some(s) = value.get("objective").and_then(|v| v.as_str()) {
        patch.objective = Some(s.to_string());
    }
    if let Some(s) = value.get("doneWhen").and_then(|v| v.as_str()) {
        patch.done_when = Some(s.to_string());
    }
    // present + null -> Some(None) (clear); present + number -> Some(Some(v))
    // (set); key absent -> `.get()` itself is None, `patch.budget_tokens`
    // stays the type default None (leave unchanged) - see this module's doc.
    // present + wrong type -> do not modify patch field at all.
    if let Some(raw) = value.get("budgetTokens") {
        if raw.is_null() {
            patch.budget_tokens = Some(None);
        } else if let Some(num) = raw.as_f64() {
            patch.budget_tokens = Some(Some(num));
        }
        // If wrong type (e.g., string), don't modify patch.budget_tokens at all
    }
    if let Some(raw) = value.get("budgetUntil") {
        if raw.is_null() {
            patch.budget_until = Some(None);
        } else if let Some(s) = raw.as_str() {
            patch.budget_until = Some(Some(s.to_string()));
        }
        // If wrong type, don't modify patch.budget_until at all
    }

    match goals::update_goal(&db, &id, &patch, None, chrono::Utc::now()) {
        Ok(goal) => Ok(Json(json!({ "goal": goal }))),
        Err(e) if e == "no such goal" => Err(AppError::not_found(e)),
        Err(e) => Err(AppError::bad_request(e)),
    }
}

/// DELETE /api/goals/:id - port of `app.ts:3558-3560`.
async fn delete_goal_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    if !goals::delete_goal(&db, &id)? {
        return Err(AppError::not_found("no such goal"));
    }
    Ok(Json(json!({ "ok": true })))
}

/// GET /api/goals/:id/runs - port of `app.ts:3562`, same 20-run cap
/// `GET /api/routines/:id/runs` uses.
async fn get_goal_runs(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    let runs = goals::goal_runs(&db, &id, 20)?;
    Ok(Json(json!({ "runs": runs })))
}

/// POST /api/goals/tick - fires every goal that is due. Port of `app.ts:
/// 3565`: the scheduler (`crate::goals::start_goal_scheduler`) calls the
/// SAME `fire_due_goals` on its own 30s timer, so this route and the timer
/// can never drift apart - same posture `POST /api/routines/tick` already
/// takes for `routines::fire_due`.
async fn post_goals_tick(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let started = crate::goals::fire_due_goals(&state, chrono::Utc::now());
    let started: Vec<serde_json::Value> = started
        .into_iter()
        .map(|(goal_id, run_id)| json!({ "goalId": goal_id, "runId": run_id }))
        .collect();
    Ok(Json(json!({ "started": started })))
}

/// POST /api/goals/:id/run - "Run now": fires regardless of schedule. Port
/// of `app.ts:3568-3572`, same status-code split `POST /api/routines/:id/
/// run` uses: "no such goal"/"no such bot" is a 404, anything else a 400.
async fn post_goal_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    match crate::goals::run_goal_now(&state, &id, chrono::Utc::now()) {
        Ok(run_id) => Ok((StatusCode::CREATED, Json(json!({ "runId": run_id })))),
        Err(e) if e == "no such goal" || e == "no such bot" => Err(AppError::not_found(e)),
        Err(e) => Err(AppError::bad_request(e)),
    }
}
