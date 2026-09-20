//! S2-09b: `PATCH /api/bots/:id` for the model pin and reasoning effort a
//! bot's calls carry - port of `projects/bullpen-night/src/server/app.ts:1190-1240`
//! for the `model` and `effort` fields ONLY. `name`/`purpose`/`instructions`/
//! `voice` are not ported; nothing in this ticket touches a bot's identity.
//! **EDIT-01 below ports the `name`/`purpose`/`instructions` third of that -
//! `voice` is the one field that stays out**, so this sentence is now only
//! historically accurate for `voice`.
//!
//! The TS route also runs a pinned model through `judgePin` (catalog lookup,
//! `:batch`/`:free` suffix checks, provider-redundancy warnings) before
//! accepting it. That is out of scope here - the ticket asks for only the
//! premium refusal, reusing `routes/settings.rs::refuse_if_premium` so this
//! gives the exact same text `/api/default-model` does rather than a second,
//! driftable copy.
//!
//! F7b-01 adds `POST /api/bots` - the only way to make a bot at all; a fresh
//! database showed an empty roster forever without it. Port of
//! `projects/bullpen-night/src/server/app.ts:1162-1188`. Unlike `PATCH`
//! above, the TS create route runs `judgePin` alone (no separate premium
//! check), so this does the same rather than reusing `refuse_if_premium`.
//!
//! ARCH-01 adds `POST /api/bots/:id/archive` (port of `app.ts:1246-1251`)
//! and `GET /api/bots/archived`, which has no TS equivalent - the TS client
//! reaches `listAllBots(db, true)` through a different route this ticket's
//! own contract does not name, so the path is this ticket's own pick. It
//! sits beside `POST /api/bots` (create) and `PATCH /api/bots/:id` (this
//! same file) as a third view onto the `bots` collection, distinguished from
//! both by HTTP method (`GET`, where neither of the others is) rather than
//! by colliding with `PATCH /api/bots/:id`'s own `{id}` segment - there is
//! no bot literally named `archived`, and even if there were, `GET
//! /api/bots/:id` does not exist for it to collide with (see `routes/mod.rs`'s
//! own callout on that gap). Without this route an archived bot is
//! unreachable and unrestorable - the roster query at `crates/store/src/
//! roster.rs` deliberately hides it - so archiving would otherwise be a
//! one-way door.
//!
//! RAIL-01 adds `PATCH /api/bots/:id/rail` (port of `app.ts:2340-2362`) and
//! `GET /api/bots/hidden` (no TS equivalent, same reasoning `GET /api/bots/
//! archived` above already gives for inventing its own path). Scope is pin
//! and hide ONLY - the TS route also carries `sectionId` (moving a bot
//! between sections), `avatar` and `shape`, and none of those three are
//! ported here; sections need their own CRUD first and are a separate
//! ticket. `patch_rail` below still reads the body as a raw JSON object and
//! checks each key independently (`"pinned" => ...`, `"hidden" => ...`),
//! the same shape the TS route itself uses for all four keys - adding
//! `sectionId`/`avatar`/`shape` later is one more `if let Some(raw) =
//! obj.get("...")` block beside these two, not a rewrite of how the body is
//! read or how the route answers.
//!
//! RAIL-02 adds that `sectionId` block - sections now have their own CRUD
//! (`routes/sections.rs`), so the room RAIL-01 left is filled in. Checked
//! and applied FIRST, before `pinned`/`hidden`, matching the TS route's own
//! ordering: an unknown section id fails the WHOLE request (400, nothing
//! else applied) rather than moving on to pin/hide with the bot left in its
//! old section - a partial write here (wrong section, but now also pinned)
//! would be worse than a flat refusal. `avatar`/`shape` remain out of scope.
//!
//! RAIL-03 adds `avatar`/`shape`, checked and applied LAST - after
//! `sectionId`/`pinned`/`hidden`, matching the TS route's own order
//! (`app.ts:2340-2362`). Neither is refused: `"avatar" in body`/
//! `"shape" in body` mean "the key is present at all", and everything
//! present under either key that is not a non-empty string (`null`, `""`,
//! a number, an object) collapses to `None`, clearing the column - the
//! same `typeof chosen === "string" && chosen !== "" ? chosen : null` shape
//! `sectionId` above already takes, not the boolean-guard shape `pinned`/
//! `hidden` use. Truncation to two code points (`avatar`) and the
//! `shared::faces::SHAPES` membership check (`shape`) both live in
//! `store::set_avatar`/`store::set_shape`, not here - this handler only
//! decides "did the client send a string to set, or anything else to
//! clear."
//!
//! EXPORT-01 adds `GET /api/bots/:id/export` (port of `app.ts:1254-1278`),
//! the first route in this crate whose success body is not JSON: `text/
//! markdown`, with a `Content-Disposition` filename so a plain browser
//! fetch offers a save-as. Checked, not assumed, that this survives the
//! session gate: `auth::require_session` (wired over every `/api/*` route
//! in `lib.rs::build_app`) only ever inspects the incoming `Request` (a
//! header/cookie check) and returns `next.run(req).await` untouched on
//! success - it never re-wraps, re-encodes, or content-type-sniffs the
//! handler's own `Response`, so a non-JSON body here rides through exactly
//! as this handler builds it. The id embedded in the filename is the bot's
//! own row id, always a `slug_for`/`slug_base` output (`store/src/
//! bots.rs`) - that alphabet is `[a-z0-9-]` only (every other character
//! collapsed to a single `-`, leading/trailing `-` trimmed, capped at 40
//! chars), so it is already safe inside a filename and inside a `Content-
//! Disposition` header value with no escaping needed, unlike `name`/
//! `description`/the instructions body, which is why only those three are
//! JSON-encoded per the ticket's contract.
//!
//! EDIT-01 adds `name`/`purpose`/`instructions` to `patch_bot`'s own PATCH -
//! port of the third of `app.ts:1190-1240` that S2-09b deliberately left
//! out (that ticket's own doc, above, named exactly this gap). Before this,
//! nothing anywhere in this port ever wrote a bot's instructions after
//! creation (`grep "SET instructions" crates/store` returned nothing) - a
//! bot made in the app had empty instructions forever, and `new_bot.rs`'s
//! own copy ("Give it instructions and a model next") had no "next" to send
//! Josh to. Import (`routes/import.rs`) was the only way in.
//!
//! The three guards are NOT the same shape as each other, matching the TS
//! exactly rather than picking one shared rule:
//! - `name` applies only when it is a string AND non-empty after trimming,
//!   and the TRIMMED value is what is stored. An empty or whitespace-only
//!   `name` is silently ignored, not a 400 - the TS never refuses this, it
//!   just treats it as "no change requested."
//! - `purpose`/`instructions` apply for ANY string, empty included -
//!   clearing either is legitimate (Josh emptying a bot's purpose, or
//!   replacing its instructions with a shorter draft, is not an error).
//!
//! `voice` stays out on purpose: it is S11's device-voice field, and
//! nothing in this port writes it anywhere except `store::duplicate_bot`
//! (which only ever carries an existing value ACROSS to a copy, never sets
//! one from scratch). Adding a writer here for a field with no feature
//! behind it yet would be a column edit pretending to be a feature.
//!
//! All three are collected as plain, side-effect-free locals BEFORE the
//! `model`/`effort` guards run - the same posture `model_update`/
//! `effort_update` already have below - so the model verdict's early
//! `return Err(...)` (a refused pin) exits the whole handler before any of
//! the three name/purpose/instructions `UPDATE`s in the final locked scope
//! ever run. Nothing about the model-pin path itself (the locked scope
//! dropped before `judge_pin`'s `.await`) is restructured - the new writes
//! land in the SAME final scope the existing `model`/`effort` writes
//! already use, after that `.await` has resolved.

use axum::extract::{Path, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use model::judge_pin;
use serde_json::json;

use super::settings::refuse_if_premium;
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/bots", post(create_bot))
        .route("/api/bots/archived", get(list_archived_bots))
        .route("/api/bots/hidden", get(list_hidden_bots))
        .route("/api/bots/{id}", patch(patch_bot).delete(hard_delete_bot))
        .route("/api/bots/{id}/archive", post(archive_bot))
        .route("/api/bots/{id}/duplicate", post(duplicate_bot))
        .route("/api/bots/{id}/rail", patch(patch_rail))
        .route("/api/bots/{id}/export", get(export_bot))
}

/// F7b-01: `POST /api/bots`. The body is read as a raw JSON object, same
/// posture as `patch_bot` below, but a body that fails to parse AT ALL
/// becomes `{}` rather than a 400 - matching the TS route's own
/// `c.req.json().catch(() => ({}))`, which swallows a malformed body
/// instead of refusing it outright. `{}` has no `name`, so that still ends
/// in the same 400 a genuinely empty body gets.
async fn create_bot(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Response, crate::AppError> {
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let obj = parsed.as_object().cloned().unwrap_or_default();

    let name = obj
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if name.is_empty() {
        return Err(crate::AppError::bad_request("name is required"));
    }

    let purpose = obj
        .get("purpose")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let instructions = obj
        .get("instructions")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // Matches the TS `typeof body["model"] === "string" && body["model"] !==
    // "" ? body["model"] : null`: absent, non-string, and empty-string all
    // collapse to "no pin".
    let model = match obj.get("model") {
        Some(serde_json::Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    };

    // Judged HERE, before the row exists - held to no db lock across this
    // `.await` (same reasoning as `patch_bot`'s own locked-scope comment
    // below), so a refused pin can never land a row first and get judged
    // after.
    if let Some(ref m) = model {
        let verdict = judge_pin(state.catalog.as_ref(), m, false).await;
        if !verdict.ok {
            return Err(crate::AppError::bad_request(
                verdict
                    .refusal
                    .unwrap_or_else(|| "that model cannot be pinned".to_string()),
            ));
        }
    }

    let draft = store::BotDraft {
        name,
        purpose,
        instructions,
        model,
    };
    let db = state.db();
    let bot = store::create_bot(&db, draft)?;
    Ok((StatusCode::CREATED, Json(json!({ "bot": bot }))).into_response())
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
    // Locked scope: `existing` (and its `has_routine`) is read here and the
    // guard dropped before the `judge_pin` await below - a `MutexGuard`
    // cannot cross an `.await` and stay `Send`, which is what an axum
    // handler future must be.
    let existing = {
        let db = state.db();
        store::get_bot(&db, &id)?
    };
    let Some(existing) = existing else {
        return Ok(no_such_bot());
    };

    let parsed: serde_json::Value = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&body)
            .map_err(|_| crate::AppError::bad_request("invalid JSON body"))?
    };
    let Some(obj) = parsed.as_object() else {
        return Err(crate::AppError::bad_request("invalid JSON body"));
    };

    // EDIT-01: `name`/`purpose`/`instructions`, collected as plain
    // side-effect-free locals - same posture as `model_update`/
    // `effort_update` below - so a refused model pin's early `return
    // Err(...)` further down exits before any of these are ever written.
    //
    // `name` matches the TS `typeof === "string" && trim() !== ""`
    // exactly: only a non-empty-after-trim string applies, and the TRIMMED
    // value is what gets stored. A blank/whitespace-only name is silently
    // ignored - not a 400 - matching the TS's own "no change requested"
    // treatment rather than inventing a refusal for it.
    let mut name_update: Option<String> = None;
    if let Some(serde_json::Value::String(raw)) = obj.get("name") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            name_update = Some(trimmed.to_string());
        }
    }

    // `purpose`/`instructions` both apply for ANY string, empty included -
    // clearing either is legitimate, unlike `name` above. A non-string
    // value present under either key is silently ignored, the same "wrong
    // shape, not an error" posture `name` takes for a non-string.
    let mut purpose_update: Option<String> = None;
    if let Some(serde_json::Value::String(raw)) = obj.get("purpose") {
        purpose_update = Some(raw.clone());
    }
    let mut instructions_update: Option<String> = None;
    if let Some(serde_json::Value::String(raw)) = obj.get("instructions") {
        instructions_update = Some(raw.clone());
    }

    // `Some(Some(id))` = set the pin, `Some(None)` = clear it, `None` = the
    // field was absent from the body at all - collected here and written
    // once the db lock is retaken below.
    let mut model_update: Option<Option<String>> = None;
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
            // same posture the TS `judgePin` call takes (`app.ts:1207`).
            // `existing.has_routine` tightens the free-tier and
            // provider-redundancy rules the same way an unattended run
            // needs; `refuse_if_premium` runs first since it needs no
            // catalogue round trip.
            {
                let db = state.db();
                refuse_if_premium(&db, m)?;
            }
            let verdict = judge_pin(state.catalog.as_ref(), m, existing.has_routine).await;
            if !verdict.ok {
                return Err(crate::AppError::bad_request(
                    verdict
                        .refusal
                        .unwrap_or_else(|| "that model cannot be pinned".to_string()),
                ));
            }
        }
        model_update = Some(model);
    }

    // M3: the effort a bot's calls carry. Only ever one of the three - a
    // typo or a stale client sending something else is refused here rather
    // than landing in the column and reading back as garbage later.
    let mut effort_update: Option<&str> = None;
    if let Some(raw) = obj.get("effort") {
        let effort = raw.as_str().unwrap_or("");
        if !matches!(effort, "low" | "medium" | "high") {
            return Err(crate::AppError::bad_request(
                "effort must be low, medium or high",
            ));
        }
        effort_update = Some(effort);
    }

    let db = state.db();
    if let Some(model) = model_update {
        db.conn().execute(
            "UPDATE bots SET model = ?1 WHERE id = ?2",
            rusqlite::params![model, id],
        )?;
    }
    if let Some(effort) = effort_update {
        db.conn().execute(
            "UPDATE bots SET effort = ?1 WHERE id = ?2",
            rusqlite::params![effort, id],
        )?;
    }
    // EDIT-01: only called when at least one of the three was actually
    // present in the body - same "only write if there is something to
    // write" posture the `model`/`effort` blocks above already take. Its
    // own `Option<Bot>` return is unused here (the freshening `get_bot`
    // call below already covers it) - the route already proved the bot
    // exists at the top of this handler, before any `.await`.
    if name_update.is_some() || purpose_update.is_some() || instructions_update.is_some() {
        store::set_identity(
            &db,
            &id,
            name_update.as_deref(),
            purpose_update.as_deref(),
            instructions_update.as_deref(),
        )?;
    }

    let bot = store::get_bot(&db, &id)?
        .ok_or_else(|| crate::AppError::bad_request("bot vanished mid-request"))?;
    Ok(Json(json!({ "bot": bot })).into_response())
}

/// ARCH-01: `POST /api/bots/:id/archive` - port of `app.ts:1246-1251`'s
/// route (which itself just calls `setArchived`, this file's `store::
/// set_archived`). The body is read the same forgiving way `create_bot`
/// above does (`c.req.json().catch(() => ({}))` in the TS): anything that
/// fails to parse, or is empty, becomes `{}`.
///
/// The archived flag itself is **`body.archived !== false`** - matching the
/// TS exactly, including its surprising shape: a missing body, a missing
/// key, `null`, a number, a string, all mean "archive". The ONLY value that
/// means "restore" is the JSON literal `false`. This is surprising enough to
/// spell out because the iOS client and existing scripts already rely on
/// it - tightening this to "only `true` archives" would flip every one of
/// those callers' missing-body archive calls into silent no-ops.
/// SEC5-09: `DELETE /api/bots/:id` - permanent removal. No TS equivalent
/// (live Bullpen and iOS only archive). Requires the bot to already be
/// archived; returns 409 otherwise so archive stays the reversible step.
async fn hard_delete_bot(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, crate::AppError> {
    let db = state.db();
    match store::hard_delete_bot(&db, &id)? {
        store::HardDeleteBotOutcome::Deleted => Ok(StatusCode::NO_CONTENT.into_response()),
        store::HardDeleteBotOutcome::NotFound => Ok(no_such_bot()),
        store::HardDeleteBotOutcome::NotArchived => Err(crate::AppError::conflict(
            "archive the bot before deleting it permanently",
        )),
    }
}

async fn archive_bot(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, crate::AppError> {
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let archived = parsed.get("archived") != Some(&serde_json::Value::Bool(false));

    let db = state.db();
    match store::set_archived(&db, &id, archived)? {
        Some(bot) => Ok(Json(json!({ "bot": bot })).into_response()),
        None => Ok(no_such_bot()),
    }
}

/// DUP-01: `POST /api/bots/:id/duplicate` - port of `app.ts:1241-1244`. No
/// request body at all - the route takes only the id in the path, same as
/// the TS. `201 {"bot": ...}` on success (the same envelope `create_bot`/
/// `archive_bot` above already answer with); `404 {"error": "no such bot"}`
/// through the shared `no_such_bot()` helper, not a second copy of that
/// body. All the actual copying decisions (which fields carry, which do
/// not, the numbered name, the routine copy and its dropped `hook_secret`)
/// live in `store::duplicate_bot`'s own doc comment - this handler is only
/// the 404/201 shape around it.
async fn duplicate_bot(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, crate::AppError> {
    let db = state.db();
    match store::duplicate_bot(&db, &id)? {
        Some(bot) => Ok((StatusCode::CREATED, Json(json!({ "bot": bot }))).into_response()),
        None => Ok(no_such_bot()),
    }
}

/// EXPORT-01: `GET /api/bots/:id/export` - port of `app.ts:1254-1278`.
/// Renders a bot as open-format markdown: `---`-delimited frontmatter
/// (`name`, `description` always; `model` only when `bot.model` is
/// `Some`, so an unpinned bot's file never grows a `model:` line claiming
/// a pin it does not have), each value run through `serde_json::to_string`,
/// which gives the same escaping `JSON.stringify` gives the TS route, so a
/// quote or an embedded newline in a name cannot break the frontmatter's
/// own `---` delimiters. Then a blank line, then `bot.instructions`
/// verbatim (never JSON-encoded - it is the body of the file, not a
/// frontmatter value).
///
/// See this file's top doc comment for why the body can safely be
/// `text/markdown` rather than JSON (the session gate never touches a
/// handler's response) and why `bot.id` needs no escaping in the
/// `Content-Disposition` filename.
async fn export_bot(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, crate::AppError> {
    let db = state.db();
    let Some(bot) = store::get_bot(&db, &id)? else {
        return Ok(no_such_bot());
    };

    let mut markdown = String::from("---\n");
    markdown.push_str(&format!("name: {}\n", serde_json::to_string(&bot.name)?));
    markdown.push_str(&format!(
        "description: {}\n",
        serde_json::to_string(&bot.purpose)?
    ));
    if let Some(model) = &bot.model {
        markdown.push_str(&format!("model: {}\n", serde_json::to_string(model)?));
    }
    markdown.push_str("---\n\n");
    markdown.push_str(&crate::export_scrub::scrub_export_text(&bot.instructions));

    let mut response = (StatusCode::OK, markdown).into_response();
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/markdown; charset=utf-8"),
    );
    // `bot.id` is always a `slug_for`/`slug_base` output - `[a-z0-9-]`
    // only, see this file's top doc comment - so this can never fail; the
    // `expect` documents that invariant instead of silently swallowing a
    // header this route promises the client.
    response.headers_mut().insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{}.md\"", bot.id))
            .expect("a slug-shaped bot id is always a valid header value"),
    );
    Ok(response)
}

/// ARCH-01: `GET /api/bots/archived` - the archived-bot listing an
/// otherwise-unreachable archived bot needs to ever be restored. See this
/// file's top doc comment for why this path was picked. Same envelope shape
/// (`{"bots": [...]}`) `mark_bot_seen`/`mark_bot_unseen` in `routes/mod.rs`
/// already answer with, rather than inventing a third shape for "a list of
/// bots".
async fn list_archived_bots(State(state): State<AppState>) -> Result<Response, crate::AppError> {
    let db = state.db();
    let bots = store::list_bots(&db, true)?;
    Ok(Json(json!({ "bots": bots })).into_response())
}

/// RAIL-01/RAIL-02/RAIL-03: `PATCH /api/bots/:id/rail` - port of
/// `app.ts:2340-2362`'s route, now carrying all four fields the TS route
/// does: `sectionId`/`pinned`/`hidden`/`avatar`/`shape`.
///
/// The 404 check runs FIRST, before the body is even read - matching the
/// TS's own `const bot = getBot(...); if (!bot) return ...404` ahead of its
/// `c.req.json()` call, and the ticket's own "checked first."
///
/// `sectionId` is checked and applied next, BEFORE `pinned`/`hidden` -
/// again matching the TS route's own order. `"sectionId" in body` in the TS
/// means "the key is present at all", not "present and a string": any
/// non-empty string moves the bot there, and EVERYTHING else present under
/// that key (`null`, `""`, a number, an object) collapses to Unassigned,
/// same as the TS `typeof section === "string" && section !== "" ? section
/// : null`. An unknown target - checked by `store::move_bot`'s own existence
/// query - fails the WHOLE request with `400 {"error": "no such section"}`
/// and returns immediately, before `pinned`/`hidden` are even looked at: a
/// partial write (wrong section, but now also pinned) would be worse than a
/// flat refusal.
///
/// `pinned`/`hidden` are each applied only when present AND a JSON boolean,
/// matching the TS `typeof body["pinned"] === "boolean"` guard exactly: a
/// string `"false"`, a number, or `null` for either key is silently ignored
/// rather than coerced or refused - the same "ignore the wrong shape"
/// posture as a missing key, not an error. A body with none of the three
/// keys (including `{}`, or a body that fails to parse at all - read the
/// same forgiving way `create_bot`/`archive_bot` above do) is therefore a
/// no-op 200, not a 400.
async fn patch_rail(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, crate::AppError> {
    let db = state.db();
    if store::get_bot(&db, &id)?.is_none() {
        return Ok(no_such_bot());
    }

    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let obj = parsed.as_object().cloned().unwrap_or_default();

    if let Some(raw) = obj.get("sectionId") {
        let target: Option<&str> = match raw {
            serde_json::Value::String(s) if !s.is_empty() => Some(s.as_str()),
            _ => None,
        };
        if !store::move_bot(&db, &id, target)? {
            return Err(crate::AppError::bad_request("no such section"));
        }
    }

    if let Some(serde_json::Value::Bool(pinned)) = obj.get("pinned") {
        store::set_pinned(&db, &id, *pinned)?;
    }
    if let Some(serde_json::Value::Bool(hidden)) = obj.get("hidden") {
        store::set_hidden(&db, &id, *hidden)?;
    }

    // RAIL-03: `store::set_avatar`/`store::set_shape` return `false` only
    // for an unknown bot id, which this handler already ruled out above -
    // the existence check ran before the body was even read - so the
    // return value has nothing left to report here, same as `set_pinned`/
    // `set_hidden`'s `Option<Bot>` above being ignored for the same reason.
    if let Some(raw) = obj.get("avatar") {
        let chosen: Option<&str> = match raw {
            serde_json::Value::String(s) if !s.is_empty() => Some(s.as_str()),
            _ => None,
        };
        store::set_avatar(&db, &id, chosen)?;
    }
    if let Some(raw) = obj.get("shape") {
        let chosen: Option<&str> = match raw {
            serde_json::Value::String(s) if !s.is_empty() => Some(s.as_str()),
            _ => None,
        };
        store::set_shape(&db, &id, chosen)?;
    }

    // RAIL-01: one call refreshes the rail - same contract the TS route's
    // own `c.json({ bots: rosterView(...) })` gives, and the same envelope
    // `mark_bot_seen`/`mark_bot_unseen` in `routes/mod.rs` already answer
    // with for the same reason (a client that wants the new order/flags
    // does not need a second round trip to get them).
    let bots = store::list_roster(&db)?;
    Ok(Json(json!({ "bots": bots })).into_response())
}

/// RAIL-01: `GET /api/bots/hidden` - the hidden-bot listing a hidden bot
/// needs to ever be unhidden again from somewhere other than the rail
/// itself. See this file's top doc comment for why the path (not a TS
/// route) was picked, same reasoning `GET /api/bots/archived` above
/// already gives. Unlike the archived listing, a hidden bot is NOT excluded
/// from `GET /api/roster` (see `crate::store::roster::list_roster`'s own
/// doc) - this route exists anyway because Settings' hidden-bots section
/// (this ticket's own choice of where to put it, not the rail itself) reads
/// from here rather than filtering the full roster fetch a second time.
async fn list_hidden_bots(State(state): State<AppState>) -> Result<Response, crate::AppError> {
    let db = state.db();
    let bots = store::list_hidden(&db)?;
    Ok(Json(json!({ "bots": bots })).into_response())
}
