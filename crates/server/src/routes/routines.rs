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
//! POST /api/routines/preview -> {"schedule": {...}, "scheduleText": "..."} (S5-F-02, F5)
//!
//! S5-F-02 (F3/F5, `reviews/S5-R.md`): `create`/`patch` used to hand
//! `store::create_routine`/`update_routine` the raw phrase Josh typed
//! (`body_data.schedule`) - the column ended up holding "every 15 minutes"
//! where a live TS Bullpen's `routines.ts:410` writes
//! `JSON.stringify(parsed.schedule)`. Both now store
//! `serde_json::to_string(&schedule_parsed)` instead (`routine_wire_json`'s
//! doc has the read-side half). `fire_due`/`resume_absence_paused`
//! (`crates/server/src/routines.rs`, owned by S5-F-01) and the `active`
//! handler just above (also S5-F-01) needed NO changes for this: they all
//! call `schedule::parse_schedule`, which now accepts the JSON shape first
//! and only falls back to the phrase grammar (see that function's doc).

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

/// Validates `tool` against the toolbox and `tool_args` as a JSON OBJECT,
/// never array or primitive. Empty or missing args becomes `{}`. Returns
/// normalized JSON string or error. Runs on both POST and PATCH, so this
/// one function is F1's save-time gate for both.
fn validate_tool_kind(tool: Option<&str>, tool_args: Option<&str>) -> Result<String, String> {
    let name = tool.unwrap_or("").trim();
    if name.is_empty() {
        return Err("Give the routine a tool to run.".to_string());
    }

    // F1: refuse a name the toolbox does not have AT SAVE TIME, not merely
    // at fire time hours later - `crate::tools::known_tool_names` is
    // `tools::build`'s own spec list, so a tool added there becomes valid
    // here automatically, with no second list to keep in sync.
    if !crate::tools::known_tool_names()
        .iter()
        .any(|known| known == name)
    {
        return Err(format!("Bullpen has no tool called {name}."));
    }

    let text = tool_args.unwrap_or("").trim();
    let parsed: serde_json::Value = if text.is_empty() {
        json!({})
    } else {
        match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => return Err("tool arguments must be a JSON object".to_string()),
        }
    };

    // Check that parsed is an object, not array or primitive
    if !parsed.is_object() {
        return Err("tool arguments must be a JSON object".to_string());
    }

    serde_json::to_string(&parsed).map_err(|_| "tool arguments must be a JSON object".to_string())
}

// F6: Validate hookMatch as a valid case-insensitive regex pattern
fn validate_hook_match(pattern: Option<&str>) -> Result<(), String> {
    if pattern.is_none_or(|p| p.trim().is_empty()) {
        return Ok(());
    }

    let p = pattern.unwrap();
    if p.len() > 200 {
        return Err("hook match must be under 200 characters".to_string());
    }

    match regex::RegexBuilder::new(p).case_insensitive(true).build() {
        Ok(_) => Ok(()),
        Err(_) => Err("hook match must be a valid regular expression".to_string()),
    }
}

// F7: Validate hookKind is one of the allowed values
fn validate_hook_kind(kind: Option<&str>) -> Result<String, String> {
    if kind.is_none() {
        return Ok("raw".to_string());
    }

    let k = kind.unwrap();
    match k {
        "raw" | "github" | "sentry" | "linear" | "pagerduty" | "slack" => Ok(k.to_string()),
        _ => Err("hook kind must be raw, github, sentry, linear, pagerduty or slack".to_string()),
    }
}

// F7: Validate hookEvents is a list of non-empty strings, max 20 items
fn validate_hook_events(events: Option<&Vec<String>>) -> Result<(), String> {
    if events.is_none() {
        return Ok(());
    }

    let e = events.unwrap();
    if e.is_empty() {
        return Ok(());
    }

    if e.len() > 20 {
        return Err("hook events must be a list of event names".to_string());
    }

    for event in e {
        if event.trim().is_empty() {
            return Err("hook events must be a list of event names".to_string());
        }
    }

    Ok(())
}

// F7: Validate conditions is a list of up to 3 condition objects with valid match patterns
fn validate_conditions(conditions: Option<&Vec<store::Condition>>) -> Result<(), String> {
    if conditions.is_none() {
        return Ok(());
    }

    let c = conditions.unwrap();
    if c.is_empty() {
        return Ok(());
    }

    if c.len() > 3 {
        return Err("conditions must be a list of up to 3 condition objects".to_string());
    }

    for cond in c {
        let valid_kind = matches!(
            cond.kind.as_str(),
            "github" | "sentry" | "linear" | "pagerduty" | "raw"
        );
        if !valid_kind {
            return Err("conditions must be a list of up to 3 condition objects".to_string());
        }

        if let Some(ref m) = cond.match_ {
            if m.len() > 200 {
                return Err("hook match must be under 200 characters".to_string());
            }
            // Validate the condition's match pattern as a regex
            match regex::RegexBuilder::new(m).case_insensitive(true).build() {
                Ok(_) => {}
                Err(_) => return Err("hook match must be a valid regular expression".to_string()),
            }
        }
    }

    Ok(())
}

// F7: Validate tools is a list of non-empty strings, max 40 items
fn validate_tools(tools: Option<&Vec<String>>) -> Result<(), String> {
    if tools.is_none() {
        return Ok(());
    }

    let t = tools.unwrap();
    if t.is_empty() {
        return Ok(());
    }

    if t.len() > 40 {
        return Err("tools must be a list of tool names".to_string());
    }

    for tool in t {
        if tool.trim().is_empty() {
            return Err("tools must be a list of tool names".to_string());
        }
    }

    Ok(())
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/routines", get(get_routines).post(post_routines))
        .route("/api/routines/preview", post(post_routines_preview))
        .route(
            "/api/routines/{id}",
            patch(patch_routine).delete(delete_routine_handler),
        )
        .route("/api/routines/{id}/active", post(post_routine_active))
        .route("/api/routines/{id}/runs", get(get_routine_runs))
        .route("/api/routines/tick", post(post_routines_tick))
        .route("/api/routines/{id}/run", post(post_routine_run))
}

/// Turns a stored `store::Routine` into the wire shape: `schedule`
/// overwritten with the PARSED JSON object (not the raw stored string) and
/// `scheduleText` added as `schedule::describe_schedule` of it (F5). A row
/// whose `schedule` column fails to parse (a hand-edited db, or the
/// pre-F3 window where a route briefly wrote the raw phrase) falls back to
/// leaving the raw stored text under `schedule` and an empty
/// `scheduleText` rather than 500ing the whole list over one bad row -
/// this is the "courtesy" F3 names for a legacy phrase-written row.
fn routine_wire_json(routine: &store::Routine) -> serde_json::Value {
    let mut value = serde_json::to_value(routine).unwrap_or_else(|_| json!({}));
    if let Some(map) = value.as_object_mut() {
        match crate::schedule::parse_schedule(&routine.schedule) {
            Ok(parsed) => {
                map.insert(
                    "schedule".to_string(),
                    serde_json::to_value(&parsed).unwrap_or(serde_json::Value::Null),
                );
                map.insert(
                    "scheduleText".to_string(),
                    json!(crate::schedule::describe_schedule(&parsed)),
                );
            }
            Err(_) => {
                map.insert("scheduleText".to_string(), json!(""));
            }
        }
    }
    value
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
    let routines: Vec<serde_json::Value> = routines.iter().map(routine_wire_json).collect();
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

    // F6: Validate bot exists
    if store::get_bot(&db, &body_data.bot_id)?.is_none() {
        return Err(AppError::bad_request("no such bot"));
    }

    // F6: Validate name is not empty
    if body_data.name.trim().is_empty() {
        return Err(AppError::bad_request("Give the routine a name."));
    }

    // F6: Validate prompt is not empty (only for "prompt" kind)
    let kind = body_data.kind.as_deref().unwrap_or("prompt");
    if kind == "prompt" && body_data.prompt.trim().is_empty() {
        return Err(AppError::bad_request("Give the routine something to do."));
    }

    // F6: Validate hookMatch as a valid regex pattern
    validate_hook_match(body_data.hook_match.as_deref()).map_err(AppError::bad_request)?;

    // F7: Validate hook_kind
    let validated_hook_kind =
        validate_hook_kind(body_data.hook_kind.as_deref()).map_err(AppError::bad_request)?;

    // F7: Validate hook_events
    validate_hook_events(body_data.hook_events.as_ref()).map_err(AppError::bad_request)?;

    // F7: Validate conditions and their match patterns
    validate_conditions(body_data.conditions.as_ref()).map_err(AppError::bad_request)?;

    // F7: Validate tools
    validate_tools(body_data.tools.as_ref()).map_err(AppError::bad_request)?;

    // S5b: Validate tool and tool_args (only for "tool" kind), capture normalized args
    let normalized_tool_args = if kind == "tool" {
        Some(
            validate_tool_kind(body_data.tool.as_deref(), body_data.tool_args.as_deref())
                .map_err(AppError::bad_request)?,
        )
    } else {
        None
    };

    // Parse the schedule to compute next_run_at and validate it
    let schedule_parsed =
        crate::schedule::parse_schedule(&body_data.schedule).map_err(AppError::bad_request)?;

    let now = chrono::Utc::now();
    let next_run_at = crate::schedule::next_run(&schedule_parsed, now).to_rfc3339();
    // F3: the column holds the TS JSON shape, not the phrase Josh typed -
    // `routine_wire_json` (the read side) and `fire_due`/
    // `resume_absence_paused`/`POST /:id/active` (via `parse_schedule`'s
    // JSON-first branch) all expect this.
    let schedule_json = serde_json::to_string(&schedule_parsed)
        .map_err(|e| AppError::bad_request(format!("could not encode schedule: {e}")))?;

    // 🔴 Refused here rather than discovered at 06:00. A routine naming a
    // path it cannot reach does not fail loudly - it stops and asks Josh to paste
    // the files into the chat, hours later, with nobody reading.
    // Only a path NOTHING can reach refuses a save. A Windows path or a meridian
    // path is a dependency on a tool, not a broken routine - Josh: "code happens
    // here on Windows, storage/hosting happens on Meridian."
    let blocking = crate::routine_paths::blocking_problems(
        &crate::routine_paths::check_routine_paths(&body_data.prompt),
    );
    if !blocking.is_empty() {
        return Err(AppError::bad_request(
            crate::routine_paths::describe_path_problems(&blocking),
        ));
    }

    // F7: Convert empty lists to None (they will become NULL in the database)
    let tools_for_store = if body_data.tools.as_ref().is_some_and(|t| t.is_empty()) {
        None
    } else {
        body_data.tools
    };

    let hook_events_for_store = if body_data.hook_events.as_ref().is_some_and(|e| e.is_empty()) {
        None
    } else {
        body_data.hook_events
    };

    let conditions_for_store = if body_data.conditions.as_ref().is_some_and(|c| c.is_empty()) {
        None
    } else {
        body_data.conditions
    };

    // F7: Trim hook_match and convert empty to None
    let hook_match_for_store = body_data.hook_match.as_deref().and_then(|h| {
        let trimmed = h.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    });

    // Call the store to create the routine - it will check the 50-cap
    let result = create_routine(
        &db,
        &body_data.bot_id,
        &body_data.name,
        &body_data.prompt,
        schedule_json,
        Some(next_run_at),
        tools_for_store,
        body_data.kind.as_deref(),
        body_data.tool.as_deref(),
        normalized_tool_args.as_deref(),
        Some(&validated_hook_kind),
        hook_events_for_store,
        hook_match_for_store,
        conditions_for_store,
    );

    match result {
        Ok(id) => {
            // Retrieve the created routine to return it
            let routine = store::routine_by_id(&db, &id)?
                .ok_or_else(|| AppError::not_found("routine not found"))?;
            Ok((
                StatusCode::CREATED,
                Json(json!({ "routine": routine_wire_json(&routine) })),
            ))
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

    // Get the existing routine to compute effective values
    let existing =
        store::routine_by_id(&db, &id)?.ok_or_else(|| AppError::not_found("no such routine"))?;

    // F8: Compute effective values from body ?? existing row
    let effective_name = body_data
        .name
        .as_ref()
        .map(|n| n.trim())
        .unwrap_or(&existing.name);
    let effective_prompt = body_data
        .prompt
        .as_ref()
        .map(|p| p.trim())
        .unwrap_or(&existing.prompt);
    let effective_kind = match &body_data.kind {
        Some(k) => k.as_str(),
        None => &existing.kind,
    };

    // F8: Re-check name/prompt emptiness against effective values
    if effective_name.is_empty() {
        return Err(AppError::bad_request("Give the routine a name."));
    }
    if effective_kind == "prompt" && effective_prompt.is_empty() {
        return Err(AppError::bad_request("Give the routine something to do."));
    }

    // F6: Validate hookMatch as a valid regex pattern
    if let Some(hm) = &body_data.hook_match {
        validate_hook_match(hm.as_deref()).map_err(AppError::bad_request)?;
    }

    // F7: Validate hook_kind
    if let Some(ref hk) = body_data.hook_kind {
        validate_hook_kind(Some(hk)).map_err(AppError::bad_request)?;
    }

    // F7: Validate hook_events (extract from Option<Option<Vec>>)
    if let Some(ref he) = body_data.hook_events {
        validate_hook_events(he.as_ref()).map_err(AppError::bad_request)?;
    }

    // F7: Validate conditions (extract from Option<Option<Vec>>)
    if let Some(ref c) = body_data.conditions {
        validate_conditions(c.as_ref()).map_err(AppError::bad_request)?;
    }

    // F7: Validate tools (extract from Option<Option<Vec>>)
    if let Some(ref t) = body_data.tools {
        validate_tools(t.as_ref()).map_err(AppError::bad_request)?;
    }

    // F8: Validate tool and tool_args when effective kind is "tool"
    let mut normalized_tool_args: Option<String> = None;
    let mut tool_value: Option<Option<String>> = None;

    if effective_kind == "tool" {
        // Compute effective tool and tool_args
        let effective_tool = body_data.tool.as_deref().or(existing.tool.as_deref());
        let effective_tool_args = body_data
            .tool_args
            .as_deref()
            .or(existing.tool_args.as_deref());

        // Validate and normalize
        normalized_tool_args = Some(
            validate_tool_kind(effective_tool, effective_tool_args)
                .map_err(AppError::bad_request)?,
        );

        // F8: Store the trimmed tool name
        tool_value = Some(effective_tool.map(|t| t.trim().to_string()));
    } else if body_data.kind.as_ref().is_some_and(|k| k == "prompt") {
        // F8: Switching back to prompt NULLs tool and tool_args
        tool_value = Some(None);
        normalized_tool_args = Some(String::new()); // Empty string signals NULL for tool_args
    }

    // F8: Run path guard on effective prompt (whether it's new or existing)
    let blocking = crate::routine_paths::blocking_problems(
        &crate::routine_paths::check_routine_paths(effective_prompt),
    );
    if !blocking.is_empty() {
        return Err(AppError::bad_request(
            crate::routine_paths::describe_path_problems(&blocking),
        ));
    }

    // If schedule is being updated, validate and compute new next_run_at
    // and re-encode as the TS JSON shape (F3 - see this module's doc).
    let mut next_run_at = None;
    let mut schedule_json = None;
    if let Some(ref schedule_text) = body_data.schedule {
        let schedule_parsed =
            crate::schedule::parse_schedule(schedule_text).map_err(AppError::bad_request)?;
        let now = chrono::Utc::now();
        next_run_at = Some(crate::schedule::next_run(&schedule_parsed, now).to_rfc3339());
        schedule_json = Some(
            serde_json::to_string(&schedule_parsed)
                .map_err(|e| AppError::bad_request(format!("could not encode schedule: {e}")))?,
        );
    }

    // Build the update fields
    let mut updates = store::UpdateRoutineFields::default();

    if let Some(name) = body_data.name {
        updates.name = Some(name);
    }
    if let Some(prompt) = body_data.prompt {
        updates.prompt = Some(prompt);
    }
    if let Some(schedule) = schedule_json {
        updates.schedule = Some(schedule);
    }
    if let Some(nra) = next_run_at {
        updates.next_run_at = Some(Some(nra));
    }

    // F7: Handle tools - convert empty lists to None
    if let Some(tools) = body_data.tools {
        let tools_for_store = if tools.as_ref().is_some_and(|t| t.is_empty()) {
            None
        } else {
            tools
        };
        updates.tools = Some(tools_for_store);
    }

    if let Some(kind) = body_data.kind {
        updates.kind = Some(kind);
    }

    // F8: Handle tool value (might be None if switching to prompt)
    if let Some(tool) = tool_value {
        updates.tool = Some(tool);
    }

    // F8: Handle tool_args
    if let Some(ta) = normalized_tool_args {
        if ta.is_empty() {
            // Empty string means NULL for tool_args (switching to prompt)
            updates.tool_args = Some(None);
        } else {
            updates.tool_args = Some(Some(ta));
        }
    }

    // F7: Set hook_kind (already validated above)
    if let Some(hk) = &body_data.hook_kind {
        updates.hook_kind = Some(hk.clone());
    }

    // F7: Handle hook_events - convert empty lists to None
    if let Some(hook_events) = body_data.hook_events {
        let he_for_store = if hook_events.as_ref().is_some_and(|e| e.is_empty()) {
            None
        } else {
            hook_events
        };
        updates.hook_events = Some(he_for_store);
    }

    // F7: Handle hook_match - trim and convert empty to None
    if let Some(hook_match) = body_data.hook_match {
        let hm_for_store = if let Some(hm) = hook_match {
            let trimmed = hm.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        } else {
            None
        };
        updates.hook_match = Some(hm_for_store);
    }

    // F7: Handle conditions - convert empty lists to None
    if let Some(conditions) = body_data.conditions {
        let c_for_store = if conditions.as_ref().is_some_and(|c| c.is_empty()) {
            None
        } else {
            conditions
        };
        updates.conditions = Some(c_for_store);
    }

    if let Some(second_opinion) = body_data.second_opinion {
        updates.second_opinion = Some(second_opinion);
    }

    update_routine(&db, &id, &updates)?;

    let routine =
        store::routine_by_id(&db, &id)?.ok_or_else(|| AppError::not_found("no such routine"))?;

    Ok(Json(json!({ "routine": routine_wire_json(&routine) })))
}

#[derive(Deserialize, Default)]
struct ActiveBody {
    active: Option<bool>,
}

/// POST /api/routines/:id/active - toggle routine active state
///
/// F2: `active: true` recomputes `next_run_at` from the routine's own
/// stored schedule and passes it, never `None` - port of the TS
/// `setRoutineActive` (`routines.ts:602-617`), which does exactly that on
/// the way up and keeps the STORED value on the way down. Passing `None`
/// unconditionally (the bug this replaces) wrote `next_run_at = NULL` on
/// every Start, and `due_routines`/`fire_due` require `next_run_at IS NOT
/// NULL`, so a routine started this way could never fire again - dead
/// through its own "Start" button.
async fn post_routine_active(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    let body_data: ActiveBody = super::parse_body(&body)?;
    let active = body_data.active.unwrap_or(true);

    let existing =
        store::routine_by_id(&db, &id)?.ok_or_else(|| AppError::not_found("no such routine"))?;

    // If activating, resume the routine (clear pause reason)
    if active {
        store::resume_routine(&db, &id)?;
    }

    let next_run_at = if active {
        let parsed =
            crate::schedule::parse_schedule(&existing.schedule).map_err(AppError::bad_request)?;
        Some(crate::schedule::next_run(&parsed, chrono::Utc::now()).to_rfc3339())
    } else {
        existing.next_run_at.clone()
    };

    set_routine_active(&db, &id, active, next_run_at)?;

    let routine =
        store::routine_by_id(&db, &id)?.ok_or_else(|| AppError::not_found("no such routine"))?;

    Ok(Json(json!({ "routine": routine })))
}

/// DELETE /api/routines/:id - delete a routine
///
/// F7: Returns 404 if the routine doesn't exist, matching TS's
/// `app.ts:3441-3445` behaviour.
async fn delete_routine_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    let found = delete_routine(&db, &id)?;
    if !found {
        return Err(AppError::not_found("no such routine"));
    }
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

/// POST /api/routines/tick - fire all due routines. Port of the TS `app.
/// post("/api/routines/tick", ...)` (`app.ts:3450`): the scheduler
/// (`server::routines::start_scheduler`) calls the SAME `fire_due` on its
/// own 30s timer, so this route and the timer can never drift apart. The
/// `now` here is the real clock - only tests reach `fire_due` with an
/// explicit one, calling `server::routines::fire_due` directly (see
/// `crates/server/tests/routines_fire.rs`'s doc for why).
async fn post_routines_tick(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let started = crate::routines::fire_due(&state, chrono::Utc::now()).await;
    let started: Vec<serde_json::Value> = started
        .into_iter()
        .map(|(routine_id, run_id)| json!({ "routineId": routine_id, "runId": run_id }))
        .collect();
    Ok(Json(json!({ "started": started })))
}

/// POST /api/routines/:id/run - "Run now": fires regardless of schedule or
/// active state. Port of the TS `app.post("/api/routines/:id/run", ...)`
/// (`app.ts:3454-3458`), same status-code split: "no such routine"/"no
/// such bot" is a 404, anything else (today, only "could not start") is a
/// 400.
async fn post_routine_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    match crate::routines::run_routine_now(&state, &id).await {
        Ok(run_id) => Ok((StatusCode::CREATED, Json(json!({ "runId": run_id })))),
        Err(error) if error == "no such routine" || error == "no such bot" => {
            Err(AppError::not_found(error))
        }
        Err(error) => Err(AppError::bad_request(error)),
    }
}

#[derive(Deserialize, Default)]
struct PreviewBody {
    schedule: String,
}

/// POST /api/routines/preview {"schedule": "<phrase>"} -> `{"schedule":
/// {...}, "scheduleText": "..."}` on success or 400 `{"error": "..."}"` on
/// a phrase that doesn't parse. S5-F-02 (F5): replaces the client's own
/// `preview_schedule` grammar (deleted from `routines_editor.rs`) - the
/// create/edit form's live "-> description" now debounces into this route,
/// so it can never show a green preview over a save the server would
/// actually reject (F5's own example: "every 200 hours" used to preview
/// fine client-side and then 400 on submit).
async fn post_routines_preview(body: axum::body::Bytes) -> ApiResult<Json<serde_json::Value>> {
    let body_data: PreviewBody = super::parse_body(&body)?;
    let parsed =
        crate::schedule::parse_schedule(&body_data.schedule).map_err(AppError::bad_request)?;
    let schedule_value = serde_json::to_value(&parsed)
        .map_err(|e| AppError::bad_request(format!("could not encode schedule: {e}")))?;
    let description = crate::schedule::describe_schedule(&parsed);
    Ok(Json(
        json!({ "schedule": schedule_value, "scheduleText": description }),
    ))
}
