//! S10-01: `/api/skills*` (the library) and `/api/bots/:id/skills*` (per-bot
//! enable/disable). Port of `app.ts:3918-3965`, same JSON shapes and status
//! codes.
//!
//! `DELETE /api/skills/:name` is wired from Settings → Skills (SEC5-03).

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::{ApiResult, AppError, AppState};
use store::skills;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/skills", get(get_skills))
        .route(
            "/api/skills/{name}",
            get(get_skill).put(put_skill).delete(delete_skill_handler),
        )
        .route("/api/bots/{id}/skills", get(get_bot_skills))
        .route(
            "/api/bots/{id}/skills/{name}",
            axum::routing::put(put_bot_skill),
        )
}

/// GET /api/skills - list, body STRIPPED, each carrying `bytes` = body
/// length. Port of `app.ts:3918-3925`'s `listSkills(db).map(({ body,
/// ...rest }) => ({ ...rest, bytes: body.length }))`.
async fn get_skills(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    let list = skills::list_skills(&db)?;
    let out: Vec<serde_json::Value> = list
        .into_iter()
        .map(|s| {
            json!({
                "id": s.id,
                "name": s.name,
                "description": s.description,
                "bytes": s.body.len(),
                "source": s.source,
                "createdAt": s.created_at,
                "updatedAt": s.updated_at,
            })
        })
        .collect();
    Ok(Json(json!({ "skills": out })))
}

/// GET /api/skills/:name - the full skill (body included), 404 "no such
/// skill". Port of `app.ts:3927-3930`.
async fn get_skill(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    match skills::get_skill(&db, &name)? {
        Some(skill) => Ok(Json(json!({ "skill": skill }))),
        None => Err(AppError::not_found("no such skill")),
    }
}

/// PUT /api/skills/:name - upsert; 400 "that name is not usable" when the
/// normalised name is empty. Port of `app.ts:3932-3944`: a malformed body
/// becomes `{}` (TS's `.json().catch(() => ({}))`), never a 400 for a bad
/// body, and each field is coerced to its type or dropped to a default
/// rather than failing the whole request.
async fn put_skill(
    State(state): State<AppState>,
    Path(name): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));

    let description = value
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let body_text = value
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // TS: `...(body.source === "claude-code" ? { source: "claude-code" } : {})`
    // - anything else leaves `source` unset, so `save_skill`'s own
    // `unwrap_or("bullpen")` default applies.
    let source = match value.get("source").and_then(|v| v.as_str()) {
        Some("claude-code") => Some("claude-code".to_string()),
        _ => None,
    };

    let input = skills::SkillInput {
        name,
        description,
        body: body_text,
        source,
    };
    match skills::save_skill(&db, input, chrono::Utc::now())? {
        Some(skill) => Ok(Json(json!({ "skill": skill }))),
        None => Err(AppError::bad_request("that name is not usable")),
    }
}

/// DELETE /api/skills/:name - port of `app.ts:3946-3950`. Built for parity;
/// not wired into the client this slice (see this module's own doc).
async fn delete_skill_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    if !skills::delete_skill(&db, &name)? {
        return Err(AppError::not_found("no such skill"));
    }
    Ok(Json(json!({ "ok": true })))
}

/// GET /api/bots/:id/skills - the bot's enabled skill NAMES, 404 "no such
/// bot". Port of `app.ts:3952-3956`.
async fn get_bot_skills(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    if store::get_bot(&db, &id)?.is_none() {
        return Err(AppError::not_found("no such bot"));
    }
    let names: Vec<String> = skills::skills_for(&db, &id)?
        .into_iter()
        .map(|s| s.name)
        .collect();
    Ok(Json(json!({ "skills": names })))
}

/// PUT /api/bots/:id/skills/:name - `{"on":true|false}`, returns the new
/// name list, 404 for an unknown bot or skill. Port of `app.ts:3958-3964`:
/// `on` is true only for a literal JSON `true` (`body.on === true` in TS),
/// so `1`, `"true"`, or anything else coerces to false, same as a malformed
/// body (which becomes `{}` here too).
async fn put_bot_skill(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    if store::get_bot(&db, &id)?.is_none() {
        return Err(AppError::not_found("no such bot"));
    }

    let value: serde_json::Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let on = value.get("on").and_then(|v| v.as_bool()) == Some(true);

    if !skills::set_bot_skill(&db, &id, &name, on)? {
        return Err(AppError::not_found("no such skill"));
    }
    let names: Vec<String> = skills::skills_for(&db, &id)?
        .into_iter()
        .map(|s| s.name)
        .collect();
    Ok(Json(json!({ "skills": names })))
}
