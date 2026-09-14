//! S2-09b: `PATCH /api/bots/:id` for the model pin and reasoning effort a
//! bot's calls carry - port of `projects/bullpen-night/src/server/app.ts:1190-1240`
//! for the `model` and `effort` fields ONLY. `name`/`purpose`/`instructions`/
//! `voice` are not ported; nothing in this ticket touches a bot's identity.
//!
//! The TS route also runs a pinned model through `judgePin` (catalog lookup,
//! `:batch`/`:free` suffix checks, provider-redundancy warnings) before
//! accepting it. That is out of scope here - the ticket asks for only the
//! premium refusal, reusing `routes/settings.rs::refuse_if_premium` so this
//! gives the exact same text `/api/default-model` does rather than a second,
//! driftable copy.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::patch;
use axum::{Json, Router};
use serde_json::json;

use super::settings::refuse_if_premium;
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/bots/{id}", patch(patch_bot))
}

fn no_such_bot() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"error": "no such bot"}))).into_response()
}

/// Body is read as a raw JSON object rather than a typed struct: the ticket
/// (matching the TS `"model" in body`) needs to tell "key absent" apart from
/// "key present and null" (clear the pin) apart from "key present and a
/// string" (set it) - a plain `Option<String>` field collapses the first two.
async fn patch_bot(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, crate::AppError> {
    let db = state.db();
    if store::get_bot(&db, &id)?.is_none() {
        return Ok(no_such_bot());
    }

    let parsed: serde_json::Value = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&body)
            .map_err(|_| crate::AppError::bad_request("invalid JSON body"))?
    };
    let Some(obj) = parsed.as_object() else {
        return Err(crate::AppError::bad_request("invalid JSON body"));
    };

    if let Some(raw) = obj.get("model") {
        let model = match raw {
            serde_json::Value::Null => None,
            // Matches the TS `typeof raw === "string" && raw !== "" ? raw : null`:
            // an empty string is treated as "clear the pin", not an error.
            serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
            serde_json::Value::String(_) => None,
            _ => {
                return Err(crate::AppError::bad_request(
                    "model must be a string or null",
                ));
            }
        };
        if let Some(ref m) = model {
            // The pin is judged HERE, while Josh is looking at the screen -
            // same posture the TS `judgePin` call takes, narrowed to the one
            // check this ticket ports.
            refuse_if_premium(&db, m)?;
        }
        db.conn().execute(
            "UPDATE bots SET model = ?1 WHERE id = ?2",
            rusqlite::params![model, id],
        )?;
    }

    // M3: the effort a bot's calls carry. Only ever one of the three - a
    // typo or a stale client sending something else is refused here rather
    // than landing in the column and reading back as garbage later.
    if let Some(raw) = obj.get("effort") {
        let effort = raw.as_str().unwrap_or("");
        if !matches!(effort, "low" | "medium" | "high") {
            return Err(crate::AppError::bad_request(
                "effort must be low, medium or high",
            ));
        }
        db.conn().execute(
            "UPDATE bots SET effort = ?1 WHERE id = ?2",
            rusqlite::params![effort, id],
        )?;
    }

    let bot = store::get_bot(&db, &id)?
        .ok_or_else(|| crate::AppError::bad_request("bot vanished mid-request"))?;
    Ok(Json(json!({ "bot": bot })).into_response())
}
