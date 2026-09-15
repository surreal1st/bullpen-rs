//! S5b-06: routes for webhook management and delivery.
//!
//! Routes:
//! POST /api/routines/:id/hook -> {"secret": "...", "url": "..."} (201)
//! DELETE /api/routines/:id/hook -> {"ok": true}
//! POST /api/hooks/:routineId -> trigger routine from webhook (202 or 204)
//!
//! The webhook secret is returned ONCE by the mint route and never again.
//! `GET /api/routines` only ever says `hasHook: true`. The secret is never
//! logged or echoed back in error bodies.
//!
//! The POST /api/hooks route is unauthenticated (open via `auth.rs`). Signature
//! verification per `hook_kind` happens BEFORE parsing the payload. A reduced
//! payload is external data - it reaches the routine's prompt inside the "what
//! arrived" block, never as instructions.
//!
//! S5b-06b (finishing S5b-06/`eb7ffaa`): once a delivery is authenticated it
//! is reduced by `crate::hooks`' reducers, narrowed by `hook_events`
//! (github only) and filtered by `hook_match`, then either fires the
//! routine directly (`## What arrived`) or, for a `conditions` routine,
//! records a `hook_arrivals` row and fires only once every condition in the
//! AND-group has a recent-enough arrival (`## All conditions met`). Port of
//! `app.ts:3606-3844` (read whole, past the ticket's cited 3606-3700 - the
//! conditions/firing tail runs to 3844).

use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use chrono::Utc;
use model::ladder::Trigger;
use regex::RegexBuilder;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

use crate::ApiResult;
use crate::AppError;
use crate::AppState;
use crate::hooks;
use crate::prompt::{self, HistoryTurn};
use crate::runs::StartOptions;
use store::{RoutineRow, clear_routine_hook, mint_routine_hook, routine_by_id, routine_row_by_id};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/routines/{id}/hook",
            post(post_routine_hook).delete(delete_routine_hook),
        )
        .route("/api/hooks/{routine_id}", post(post_webhook))
}

/// POST /api/routines/:id/hook - mint a fresh webhook secret. Returns 201
/// with {secret, url} if the routine exists. The secret is never returned
/// again. Returns 404 if the routine doesn't exist.
async fn post_routine_hook(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let db = state.db();

    // Check if routine exists first
    let routine = routine_by_id(&db, &id)?;
    if routine.is_none() {
        return Err(AppError::not_found("no such routine"));
    }

    match mint_routine_hook(&db, &id)? {
        Some(secret) => {
            let public_url =
                std::env::var("PUBLIC_URL").unwrap_or_else(|_| "http://localhost:4380".to_string());
            Ok((
                StatusCode::CREATED,
                Json(json!({
                    "secret": secret,
                    "url": format!("{}/api/hooks/{}", public_url, id)
                })),
            ))
        }
        None => Err(AppError::not_found("no such routine")),
    }
}

/// DELETE /api/routines/:id/hook - clear the webhook secret. Returns 200 with
/// {ok: true} if the routine exists and was updated. Returns 404 if the
/// routine doesn't exist.
async fn delete_routine_hook(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    match clear_routine_hook(&db, &id)? {
        true => Ok(Json(json!({"ok": true}))),
        false => Err(AppError::not_found("no such routine")),
    }
}

/// POST /api/hooks/:routineId - receive a webhook delivery and trigger the
/// routine. This is the only unauthenticated route that reads a body.
async fn post_webhook(
    State(_state): State<AppState>,
    Path(routine_id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // Get routine row (includes hook_secret). Scoped so the guard is
    // DROPPED here: `_state.db()` locks a plain (non-reentrant)
    // `std::sync::Mutex`, and this handler locks it again further down (the
    // in-flight check, arrival recording) - holding this guard across those
    // would deadlock the request on itself, same discipline as
    // `fire_webhook_routine` below.
    let row = {
        let db = _state.db();
        match routine_row_by_id(&db, &routine_id) {
            Ok(Some(r)) => r,
            Ok(None) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "no such webhook"})),
                )
                    .into_response();
            }
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "internal error"})),
                )
                    .into_response();
            }
        }
    };

    // Check that secret exists
    if row.hook_secret.is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no such webhook"})),
        )
            .into_response();
    }

    // Check body size cap
    const HOOK_BODY_MAX_BYTES: usize = 64 * 1024;
    if body.len() > HOOK_BODY_MAX_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({"error": "body too large"})),
        )
            .into_response();
    }

    let raw_body = String::from_utf8_lossy(&body).to_string();

    // S5b-06b (item 6): KEPT, not deleted. The TS itself sniffs the header
    // rather than dispatching on the configured `hook_kind` alone, exactly
    // here - `app.ts:3629-3645`'s own comment: a routine holding
    // `conditions` (an AND-group) shares ONE secret and ONE URL across
    // several external services, each posting in its own native shape
    // ("GitHub PR opened AND a Sentry alert" needs both to reach one
    // endpoint), so a conditions-mode delivery self-identifies by header
    // rather than being pinned to the routine's single configured
    // `hook_kind` - which is what an ORDINARY single-kind routine still
    // uses for both auth and reduction (the `else` below, unchanged).
    let has_conditions = row.conditions.is_some();
    let configured_kind = match row.hook_kind.as_str() {
        "github" => "github",
        "sentry" => "sentry",
        "linear" => "linear",
        "pagerduty" => "pagerduty",
        _ => "raw",
    };

    let mut hook_kind = configured_kind.to_string();
    if has_conditions {
        if headers.get("x-hub-signature-256").is_some() {
            hook_kind = "github".to_string();
        } else if headers.get("linear-signature").is_some() {
            hook_kind = "linear".to_string();
        } else if headers.get("x-pagerduty-signature").is_some() {
            hook_kind = "pagerduty".to_string();
        } else {
            hook_kind = "raw".to_string();
        }
    }

    // Verify signature based on kind
    let secret = row.hook_secret.as_ref().unwrap();
    match hook_kind.as_str() {
        "github" => {
            let header = headers
                .get("x-hub-signature-256")
                .and_then(|v| v.to_str().ok());
            if !hooks::verify_github_signature(secret, &raw_body, header) {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": "not authorized"})),
                )
                    .into_response();
            }
        }
        "linear" => {
            let header = headers
                .get("linear-signature")
                .and_then(|v| v.to_str().ok());
            if !hooks::verify_linear_signature(secret, &raw_body, header) {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": "not authorized"})),
                )
                    .into_response();
            }
        }
        "pagerduty" => {
            let header = headers
                .get("x-pagerduty-signature")
                .and_then(|v| v.to_str().ok());
            if !hooks::verify_pager_duty_signature(secret, &raw_body, header) {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": "not authorized"})),
                )
                    .into_response();
            }
        }
        _ => {
            // "raw" or "sentry" - check bearer token
            let auth_header = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            let bearer = auth_header.strip_prefix("Bearer ").unwrap_or_default();
            if !verify_hook_secret(secret, bearer) {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": "not authorized"})),
                )
                    .into_response();
            }
        }
    }

    // A bearer-authenticated delivery to a conditions-mode routine could be
    // either Sentry or raw - both use the same auth, so once it has passed,
    // the payload's own shape breaks the tie. Port of `app.ts:3669-3680`.
    if has_conditions && hook_kind == "raw" {
        let maybe_json: Value = serde_json::from_str(&raw_body).unwrap_or_else(|_| json!({}));
        if hooks::reduce_sentry(&maybe_json).is_some() {
            hook_kind = "sentry".to_string();
        }
    }

    // A run already in flight for this routine means a second trigger
    // arrived before the first finished - refusing it rather than queuing
    // keeps a fast upstream retry from stacking runs nobody asked for. Port
    // of `app.ts:3682-3688`.
    let in_flight = {
        let db = _state.db();
        db.conn()
            .query_row(
                "SELECT 1 FROM runs WHERE routine_id = ?1 AND status IN ('running', 'waiting') LIMIT 1",
                rusqlite::params![routine_id],
                |_| Ok(()),
            )
            .is_ok()
    };
    if in_flight {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error": "a run for this routine is already in progress"})),
        )
            .into_response();
    }

    // Reduce the delivery to one line of text, by kind. Port of
    // `app.ts:3690-3756`.
    let text: String = match hook_kind.as_str() {
        "github" => {
            let event = headers
                .get("x-github-event")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            // GitHub's own connectivity check when a webhook is first saved -
            // not worth a run, and firing one every re-save would be noise.
            if event == "ping" {
                return (StatusCode::NO_CONTENT, Json(json!({}))).into_response();
            }
            let events = store::routines::parse_hook_events(row.hook_events.as_deref());
            if let Some(events) = &events
                && !events.iter().any(|e| e == &event)
            {
                return (StatusCode::NO_CONTENT, Json(json!({}))).into_response();
            }
            let payload: Value = serde_json::from_str(&raw_body).unwrap_or_else(|_| json!({}));
            match hooks::reduce_github(&event, &payload) {
                Some(t) => t,
                None => return (StatusCode::NO_CONTENT, Json(json!({}))).into_response(),
            }
        }
        "sentry" => {
            let payload: Value = serde_json::from_str(&raw_body).unwrap_or_else(|_| json!({}));
            match hooks::reduce_sentry(&payload) {
                Some(t) => t,
                None => return (StatusCode::NO_CONTENT, Json(json!({}))).into_response(),
            }
        }
        "linear" => {
            let event = headers
                .get("x-linear-webhook-event")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            let payload: Value = serde_json::from_str(&raw_body).unwrap_or_else(|_| json!({}));
            match hooks::reduce_linear(event, &payload) {
                Some(t) => t,
                None => return (StatusCode::NO_CONTENT, Json(json!({}))).into_response(),
            }
        }
        "pagerduty" => {
            let event = headers
                .get("x-pagerduty-event-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            let payload: Value = serde_json::from_str(&raw_body).unwrap_or_else(|_| json!({}));
            match hooks::reduce_pager_duty(event, &payload) {
                Some(t) => t,
                None => return (StatusCode::NO_CONTENT, Json(json!({}))).into_response(),
            }
        }
        _ => {
            // "raw": a JSON body's `text` field if present, else the raw body.
            let content_type = headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            let extracted = if content_type.contains("application/json") {
                serde_json::from_str::<Value>(&raw_body)
                    .ok()
                    .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(str::to_string))
                    .unwrap_or_default()
            } else {
                raw_body.clone()
            };
            extracted.chars().take(20_000).collect()
        }
    };

    // Applies to every kind: a routine that only cares about SOME
    // deliveries (a specific repo, a keyword) drops the rest here rather
    // than firing and making the model say "nothing relevant" every time.
    // Port of `app.ts:3758-3772`.
    if let Some(pattern) = row.hook_match.as_deref()
        && !pattern.is_empty()
        && !regex_matches_or_fails_open(pattern, &text)
    {
        return (StatusCode::NO_CONTENT, Json(json!({}))).into_response();
    }

    // S6b: a routine may hold up to three trigger conditions that must ALL
    // arrive within a 60-minute window before it fires. Port of
    // `app.ts:3774-3839`.
    let conditions =
        store::routines::parse_conditions(row.conditions.as_deref()).filter(|c| !c.is_empty());

    if let Some(conditions) = conditions {
        {
            let db = _state.db();
            if store::routines::record_hook_arrival(&db, &routine_id, &hook_kind, &text).is_err() {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "internal error"})),
                )
                    .into_response();
            }
        }

        let since = (Utc::now() - chrono::Duration::minutes(60)).to_rfc3339();
        let mut arrivals: Vec<String> = Vec::with_capacity(conditions.len());
        let mut all_conditions_met = true;
        {
            let db = _state.db();
            for condition in &conditions {
                let arrival =
                    store::routines::latest_hook_arrival(&db, &routine_id, &condition.kind, &since)
                        .unwrap_or(None);
                let Some(arrival_text) = arrival else {
                    all_conditions_met = false;
                    break;
                };
                if let Some(pattern) = condition.match_.as_deref()
                    && !pattern.is_empty()
                    && !regex_matches_or_fails_open(pattern, &arrival_text)
                {
                    all_conditions_met = false;
                    break;
                }
                arrivals.push(arrival_text);
            }
        }

        if !all_conditions_met {
            return (StatusCode::NO_CONTENT, Json(json!({}))).into_response();
        }

        {
            let db = _state.db();
            let _ = store::routines::clear_hook_arrivals(&db, &routine_id);
        }

        let arrivals_text = arrivals
            .iter()
            .enumerate()
            .map(|(i, a)| format!("{}. {}", i + 1, a))
            .collect::<Vec<_>>()
            .join("\n");
        let extra = format!("\n\n## All conditions met\n\n{arrivals_text}");
        return match fire_webhook_routine(&_state, &row, &extra) {
            Some(run_id) => (
                StatusCode::ACCEPTED,
                Json(json!({"ok": true, "runId": run_id})),
            )
                .into_response(),
            None => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "that routine's bot no longer exists"})),
            )
                .into_response(),
        };
    }

    // Fenced as data: `text` is an external delivery, never instructions -
    // it rides inside "## What arrived", the same fence `fire_webhook_routine`
    // uses for the conditions-met case above. Port of `app.ts:3841-3843`.
    let extra = format!("\n\n## What arrived\n\n{text}");
    match fire_webhook_routine(&_state, &row, &extra) {
        Some(run_id) => (
            StatusCode::ACCEPTED,
            Json(json!({"ok": true, "runId": run_id})),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "that routine's bot no longer exists"})),
        )
            .into_response(),
    }
}

/// Case-insensitive regex test, failing OPEN (treats a non-compiling
/// pattern as a match) rather than silently dropping every delivery for a
/// routine because of a pattern nobody rejected - `hook_match`/
/// `Condition.match` are validated as compilable at save time, so a compile
/// error here should not happen. Port of the TS `try { new RegExp(pattern,
/// "i").test(text) } catch { true }` (`app.ts:3762-3770`, `:3810-3818`).
fn regex_matches_or_fails_open(pattern: &str, text: &str) -> bool {
    RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .map(|re| re.is_match(text))
        .unwrap_or(true)
}

/// Fires a routine from a webhook delivery. A near-duplicate of the private
/// `fire_routine` in `crate::routines` (`routines.rs:202`) rather than a
/// call into it: that module belongs to S5b-05 while this ticket is in
/// flight (shared working tree), `fire_routine` is not `pub`, and it
/// hardcodes `Trigger::Routine` with no way to pass a different one in. The
/// one real difference from `fire_routine` is `Trigger::Webhook`, matching
/// the TS `fireRoutine(db, runs, row, extra, "webhook")` calls at
/// `app.ts:3830`/`:3841`. Flagged in this ticket's Results for the
/// orchestrator to fold back into one function once `routines.rs` is free.
fn fire_webhook_routine(state: &AppState, row: &RoutineRow, extra: &str) -> Option<String> {
    // Same B2-style discipline as `fire_routine`'s own doc: the db guard is
    // locked, read, and DROPPED before `state.runs.start_routine` below,
    // which locks the same `Arc<Mutex<Db>>` internally - holding this guard
    // across that call would deadlock the calling thread.
    let (bot_id, conversation_id, model, messages) = {
        let db = state.db();
        let bot = store::get_bot(&db, &row.bot_id).ok().flatten()?;
        let conversation_id = store::get_or_create_conversation(&db, &bot.id).ok()?;
        let model = model::ladder::safe_fallback(&db);
        let _ = store::append_message(
            &db,
            &conversation_id,
            "user",
            &format!("[{}] ran", row.name),
            store::NewMessage::default(),
        );
        let prompt_text = format!("{}{}{}", row.prompt, extra, prompt::STOP_RATHER_THAN_INVENT);
        let messages = prompt::build_prompt(&db, &bot, &[HistoryTurn::user(prompt_text)]);
        (bot.id, conversation_id, model, messages)
    };

    Some(state.runs.start_routine(
        StartOptions {
            bot_id,
            conversation_id,
            model,
            messages,
            trigger: Trigger::Webhook,
            room: false,
        },
        row.id.clone(),
    ))
}

/// Verify a webhook bearer token in constant time.
fn verify_hook_secret(secret: &str, presented: &str) -> bool {
    if secret.is_empty() || presented.is_empty() {
        return false;
    }
    let expected_bytes = secret.as_bytes();
    let presented_bytes = presented.as_bytes();
    if expected_bytes.len() != presented_bytes.len() {
        return false;
    }
    // Constant-time comparison using subtle crate
    expected_bytes.ct_eq(presented_bytes).into()
}
