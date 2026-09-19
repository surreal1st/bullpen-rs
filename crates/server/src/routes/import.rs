//! IMPORT-01: `POST /api/import/open/preview` and `POST /api/import/open` -
//! ports of `projects/bullpen-night/src/server/app.ts:3155-3170`. The actual
//! parsing (`parse_open_bot`, `detect_format`, `read_frontmatter`) lives in
//! `crate::import_open`, which is pure and has no `.await` - this file owns
//! everything that touches the database or judges a model pin.
//!
//! Both routes read the body the same forgiving way `routes::bots::
//! create_bot` does: `serde_json::from_slice(...).unwrap_or_else(|_|
//! json!({}))`, then `.as_object()` - matching the TS `c.req.json().catch(()
//! => ({}))`, which swallows a malformed body instead of refusing it
//! outright. `fileName` absent or non-string falls back to `"bot.md"`;
//! `text` absent, non-string, or blank after trimming is a flat 400 on
//! EITHER route, checked before anything else runs.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use model::judge_pin;
use serde_json::{Value, json};

use crate::AppState;
use crate::import_open::parse_open_bot;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/import/open/preview", post(preview_open))
        .route("/api/import/open", post(import_open_route))
}

/// Reads `fileName`/`text` out of a raw JSON body the forgiving way this
/// file's own top doc comment describes. Shared by both routes below so
/// neither one drifts from the other on what counts as "no content".
fn read_import_body(body: &Bytes) -> (String, String) {
    let parsed: Value = serde_json::from_slice(body).unwrap_or_else(|_| json!({}));
    let obj = parsed.as_object().cloned().unwrap_or_default();
    let file_name = obj
        .get("fileName")
        .and_then(|v| v.as_str())
        .unwrap_or("bot.md")
        .to_string();
    let text = obj
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    (file_name, text)
}

fn no_file_content() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": "no file content was sent"})),
    )
        .into_response()
}

/// Previews what an open-format file would become, without creating
/// anything or judging a model pin - "a preview must not be able to fail on
/// a catalogue round trip" (the ticket's own wording). No `AppState` needed
/// at all: parsing is pure.
async fn preview_open(body: Bytes) -> Response {
    let (file_name, text) = read_import_body(&body);
    if text.trim().is_empty() {
        return no_file_content();
    }
    let preview = parse_open_bot(&file_name, &text);
    (StatusCode::OK, Json(json!({ "preview": preview }))).into_response()
}

/// Creates a bot from an open-format file. In order: parse, refuse empty
/// instructions, refuse a duplicate name (`store::create_bot`'s own
/// `slug_for` does NOT refuse a duplicate - it silently appends `-2` - so
/// this guard is the only thing standing between importing the same file
/// twice and quietly getting `arthur` and `arthur-2`), judge a pinned model
/// if the file named one, then create.
async fn import_open_route(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, crate::AppError> {
    let (file_name, text) = read_import_body(&body);
    if text.trim().is_empty() {
        return Ok(no_file_content());
    }

    let parsed = parse_open_bot(&file_name, &text);
    if parsed.instructions.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "There are no instructions in that file.",
                "warnings": parsed.warnings,
            })),
        )
            .into_response());
    }

    // Locked scope: the duplicate-name check needs the db, but the
    // `judge_pin` `.await` below must not hold that guard across it - a
    // `MutexGuard` cannot cross an `.await` and stay `Send`, same pattern
    // `routes/bots.rs::patch_bot` already uses for its own premium check
    // before its own `judge_pin` call.
    let duplicate = {
        let db = state.db();
        store::get_bot(&db, &store::slug_base(&parsed.name))?
    };
    if duplicate.is_some() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("There is already a bot called {}.", parsed.name),
                "warnings": parsed.warnings,
            })),
        )
            .into_response());
    }

    // Divergence (c) (see `import_open`'s own doc): a `model:` our own
    // export wrote is honoured, and judged with the same posture
    // `routes/bots.rs::create_bot` uses for its own `model` field - `false`
    // (not a routine) since an imported bot has none yet. A refusal here
    // fails the WHOLE request; nothing is created.
    if let Some(model) = &parsed.model {
        let verdict = judge_pin(state.catalog.as_ref(), model, false).await;
        if !verdict.ok {
            return Err(crate::AppError::bad_request(
                verdict
                    .refusal
                    .unwrap_or_else(|| "that model cannot be pinned".to_string()),
            ));
        }
    }

    let draft = store::BotDraft {
        name: parsed.name.clone(),
        purpose: parsed.purpose.clone(),
        instructions: parsed.instructions.clone(),
        model: parsed.model.clone(),
    };
    let db = state.db();
    let bot = store::create_bot(&db, draft)?;

    // The TS's own trailing warning becomes conditional here: divergence
    // (c) means a model CAN survive the trip, so a bot that got one no
    // longer starts on "platform defaults" for its model, only its
    // permissions.
    let mut warnings = parsed.warnings.clone();
    warnings.push(
        if parsed.model.is_some() {
            "permissions were not in the file, so it starts on the platform defaults"
        } else {
            "model and permissions were not in the file, so it starts on the platform defaults"
        }
        .to_string(),
    );

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "result": {
                "ok": true,
                "botId": bot.id,
                "name": bot.name,
                "format": parsed.format,
                "warnings": warnings,
            }
        })),
    )
        .into_response())
}
