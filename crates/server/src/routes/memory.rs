//! Memory routes: GET/PUT core, POST log entries, DELETE entries, shared core,
//! projects. Port of `app.ts:4490-4530` (memory), `app.ts:3147-3160` (shared-core).

use crate::{ApiResult, AppState};
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post, put};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;

const CORE_TOKEN_BUDGET: i64 = 500;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/bots/{id}/memory", get(get_bot_memory))
        .route("/api/bots/{id}/memory/core", put(put_bot_memory_core))
        .route("/api/bots/{id}/memory", post(post_bot_memory))
        .route("/api/bots/{id}/memory/notes", post(post_bot_memory_note))
        .route(
            "/api/bots/{id}/memory/{entryId}",
            delete(delete_bot_memory_entry),
        )
        .route("/api/shared-core", get(get_shared_core))
        .route("/api/shared-core", put(put_shared_core))
        .route("/api/projects", get(get_projects))
        .route("/api/projects", post(post_project))
        .route("/api/projects/{id}/members", post(post_project_member))
        .route("/api/memory/shared", get(get_shared_memory))
        .route("/api/memory/shared", post(post_shared_memory))
        .route(
            "/api/memory/shared/{entryId}",
            delete(delete_shared_memory_entry),
        )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CoreStatus {
    core: String,
    tokens: i64,
    budget: i64,
    over_budget: bool,
    entries: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LogEntryResponse {
    id: String,
    content: String,
    source: String,
    #[serde(rename = "createdAt")]
    created_at: String,
    kind: String,
    expires_at: Option<String>,
    scope: String,
    project_id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectResponse {
    id: String,
    name: String,
    created_at: String,
}

fn approx_tokens(text: &str) -> i64 {
    ((text.len() as i64) + 3) / 4
}

fn core_status(db: &store::Db, bot_id: &str) -> ApiResult<CoreStatus> {
    let core = store::memory::get_core(db, bot_id)?;
    let tokens = approx_tokens(&core);
    let count: i64 = db.conn().query_row(
        "SELECT COUNT(*) FROM memory_log WHERE bot_id = ?1",
        rusqlite::params![bot_id],
        |row| row.get(0),
    )?;
    Ok(CoreStatus {
        core,
        tokens,
        budget: CORE_TOKEN_BUDGET,
        over_budget: tokens > CORE_TOKEN_BUDGET,
        entries: count,
    })
}

/// GET /api/bots/:id/memory
async fn get_bot_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<impl IntoResponse> {
    let db = state.db();
    if store::get_bot(&db, &id)?.is_none() {
        return Ok((
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": "no such bot" })),
        )
            .into_response());
    }

    let query = params.get("q").map(|s| s.as_str()).unwrap_or("");

    // Sweep expired notes before returning memory (F3)
    store::memory::sweep_expired(&db)?;

    let log: Vec<LogEntryResponse> = if query.is_empty() {
        store::memory::recent_log(&db, &id, 50)?
            .into_iter()
            .map(|e| LogEntryResponse {
                id: e.id,
                content: e.content,
                source: e.source,
                created_at: e.created_at,
                kind: e.kind,
                expires_at: e.expires_at,
                scope: e.scope,
                project_id: e.project_id,
            })
            .collect()
    } else {
        store::memory::search_log(&db, &id, query, &[store::memory::Scope::Own], 50)?
            .into_iter()
            .map(|e| LogEntryResponse {
                id: e.id,
                content: e.content,
                source: e.source,
                created_at: e.created_at,
                kind: e.kind,
                expires_at: e.expires_at,
                scope: e.scope,
                project_id: e.project_id,
            })
            .collect()
    };

    let status = core_status(&db, &id)?;
    Ok((
        StatusCode::OK,
        axum::Json(json!({
            "core": status.core,
            "tokens": status.tokens,
            "budget": status.budget,
            "overBudget": status.over_budget,
            "entries": status.entries,
            "searched": !query.is_empty(),
            "log": log,
        })),
    )
        .into_response())
}

#[derive(Deserialize, Default)]
struct SetCoreRequest {
    core: Option<String>,
}

/// PUT /api/bots/:id/memory/core
async fn put_bot_memory_core(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<impl IntoResponse> {
    let db = state.db();
    if store::get_bot(&db, &id)?.is_none() {
        return Ok((
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": "no such bot" })),
        )
            .into_response());
    }

    let req = super::parse_body::<SetCoreRequest>(&body)?;
    let core = req.core.unwrap_or_default();

    store::memory::set_core(&db, &id, &core)?;
    state.runs.changes.touch(crate::changes::ChangeKind::Memory);

    let status = core_status(&db, &id)?;
    Ok((
        StatusCode::OK,
        axum::Json(json!({
            "core": status.core,
            "tokens": status.tokens,
            "budget": status.budget,
            "overBudget": status.over_budget,
            "entries": status.entries,
        })),
    )
        .into_response())
}

#[derive(Deserialize, Default)]
struct RememberRequest {
    content: Option<String>,
}

/// POST /api/bots/:id/memory
async fn post_bot_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<impl IntoResponse> {
    let db = state.db();
    if store::get_bot(&db, &id)?.is_none() {
        return Ok((
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": "no such bot" })),
        )
            .into_response());
    }

    let req = super::parse_body::<RememberRequest>(&body)?;
    let content = req.content.unwrap_or_default().trim().to_string();

    if content.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            axum::Json(json!({ "error": "content is required" })),
        )
            .into_response());
    }

    let entry = store::memory::remember(&db, &id, &content, "josh")?;
    state.runs.changes.touch(crate::changes::ChangeKind::Memory);

    Ok((
        StatusCode::CREATED,
        axum::Json(json!({
            "entry": {
                "id": entry.id,
                "source": entry.source,
                "content": entry.content,
                "createdAt": entry.created_at,
            }
        })),
    )
        .into_response())
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct NoteRequest {
    content: Option<String>,
    ttl_seconds: Option<u64>,
}

/// POST /api/bots/:id/memory/notes
async fn post_bot_memory_note(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<impl IntoResponse> {
    let db = state.db();
    if store::get_bot(&db, &id)?.is_none() {
        return Ok((
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": "no such bot" })),
        )
            .into_response());
    }

    let req = super::parse_body::<NoteRequest>(&body)?;
    let content = req.content.unwrap_or_default().trim().to_string();

    if content.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            axum::Json(json!({ "error": "content is required" })),
        )
            .into_response());
    }

    // Default to 86400 (1 day), reject 0 or negative (F2)
    let ttl = match req.ttl_seconds {
        None => 86400,
        Some(0) => {
            return Ok((
                StatusCode::BAD_REQUEST,
                axum::Json(json!({ "error": "ttlSeconds must be greater than 0" })),
            )
                .into_response());
        }
        Some(t) => t,
    };
    let entry = store::memory::note(&db, &id, &content, ttl)?;
    state.runs.changes.touch(crate::changes::ChangeKind::Memory);

    Ok((
        StatusCode::CREATED,
        axum::Json(json!({
            "entry": {
                "id": entry.id,
                "source": entry.source,
                "content": entry.content,
                "createdAt": entry.created_at,
            }
        })),
    )
        .into_response())
}

/// DELETE /api/bots/:id/memory/:entryId
async fn delete_bot_memory_entry(
    State(state): State<AppState>,
    Path((id, entry_id)): Path<(String, String)>,
) -> ApiResult<impl IntoResponse> {
    let db = state.db();
    if store::get_bot(&db, &id)?.is_none() {
        return Ok((
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": "no such bot" })),
        )
            .into_response());
    }

    let removed = store::memory::forget(&db, &id, &entry_id)?;
    if removed {
        state.runs.changes.touch(crate::changes::ChangeKind::Memory);
        Ok((StatusCode::OK, axum::Json(json!({ "ok": true }))).into_response())
    } else {
        Ok((
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": "no such entry" })),
        )
            .into_response())
    }
}

#[derive(Serialize)]
struct SharedCoreResponse {
    core: String,
}

/// GET /api/shared-core
async fn get_shared_core(State(state): State<AppState>) -> ApiResult<impl IntoResponse> {
    let db = state.db();
    let core = store::memory::get_shared_core(&db)?;
    Ok((StatusCode::OK, axum::Json(SharedCoreResponse { core })).into_response())
}

#[derive(Deserialize, Default)]
struct SetSharedCoreRequest {
    core: Option<String>,
}

/// PUT /api/shared-core
async fn put_shared_core(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> ApiResult<impl IntoResponse> {
    let db = state.db();
    let req = super::parse_body::<SetSharedCoreRequest>(&body)?;
    let core = req.core.unwrap_or_default();

    store::memory::set_shared_core(&db, &core)?;
    state.runs.changes.touch(crate::changes::ChangeKind::Memory);

    Ok((StatusCode::OK, axum::Json(SharedCoreResponse { core })).into_response())
}

/// GET /api/projects
async fn get_projects(State(state): State<AppState>) -> ApiResult<impl IntoResponse> {
    let db = state.db();
    let projects = db
        .conn()
        .prepare("SELECT id, name, created_at FROM projects ORDER BY created_at DESC")?
        .query_map([], |row| {
            Ok(ProjectResponse {
                id: row.get(0)?,
                name: row.get(1)?,
                created_at: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok((
        StatusCode::OK,
        axum::Json(json!({
            "projects": projects,
        })),
    )
        .into_response())
}

#[derive(Deserialize, Default)]
struct CreateProjectRequest {
    name: Option<String>,
}

/// POST /api/projects
async fn post_project(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> ApiResult<impl IntoResponse> {
    let db = state.db();
    let req = super::parse_body::<CreateProjectRequest>(&body)?;
    let name = req.name.unwrap_or_default().trim().to_string();

    if name.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            axum::Json(json!({ "error": "name is required" })),
        )
            .into_response());
    }

    // Check for duplicate name (case-insensitive) (F5)
    let exists = db
        .conn()
        .query_row(
            "SELECT 1 FROM projects WHERE lower(name) = lower(?1)",
            rusqlite::params![&name],
            |_| Ok(()),
        )
        .optional()?;

    if exists.is_some() {
        return Ok((
            StatusCode::CONFLICT,
            axum::Json(json!({ "error": "a project with that name already exists" })),
        )
            .into_response());
    }

    let project = store::memory::create_project(&db, &name)?;
    state.runs.changes.touch(crate::changes::ChangeKind::Memory);

    Ok((
        StatusCode::CREATED,
        axum::Json(json!({
            "project": ProjectResponse {
                id: project.id,
                name: project.name,
                created_at: project.created_at,
            }
        })),
    )
        .into_response())
}

#[derive(Deserialize, Default)]
struct AddMemberRequest {
    #[serde(rename = "botId")]
    bot_id: Option<String>,
}

/// POST /api/projects/:id/members
async fn post_project_member(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<impl IntoResponse> {
    let db = state.db();
    let req = super::parse_body::<AddMemberRequest>(&body)?;
    let bot_id = req.bot_id.unwrap_or_default().trim().to_string();

    if bot_id.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            axum::Json(json!({ "error": "botId is required" })),
        )
            .into_response());
    }

    // Validate project exists (F6)
    if db
        .conn()
        .query_row(
            "SELECT 1 FROM projects WHERE id = ?1",
            rusqlite::params![&id],
            |_| Ok(()),
        )
        .optional()?
        .is_none()
    {
        return Ok((
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": "no such project" })),
        )
            .into_response());
    }

    // Validate bot exists (F6)
    if store::get_bot(&db, &bot_id)?.is_none() {
        return Ok((
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": "no such bot" })),
        )
            .into_response());
    }

    store::memory::add_project_member(&db, &id, &bot_id)?;
    state.runs.changes.touch(crate::changes::ChangeKind::Memory);

    Ok((StatusCode::OK, axum::Json(json!({ "ok": true }))).into_response())
}

/// GET /api/memory/shared
async fn get_shared_memory(State(state): State<AppState>) -> ApiResult<impl IntoResponse> {
    let db = state.db();
    let entries = store::memory::recent_shared_log(&db, 50)?
        .into_iter()
        .map(|e| LogEntryResponse {
            id: e.id,
            content: e.content,
            source: e.source,
            created_at: e.created_at,
            kind: e.kind,
            expires_at: e.expires_at,
            scope: e.scope,
            project_id: e.project_id,
        })
        .collect::<Vec<_>>();

    Ok((
        StatusCode::OK,
        axum::Json(json!({
            "log": entries,
        })),
    )
        .into_response())
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct SharedMemoryRequest {
    content: Option<String>,
    bot_id: Option<String>,
}

/// POST /api/memory/shared
/// Creates a shared-scope memory entry. Accepts an optional `botId`; defaults to "josh".
async fn post_shared_memory(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> ApiResult<impl IntoResponse> {
    let db = state.db();
    let req = super::parse_body::<SharedMemoryRequest>(&body)?;
    let content = req.content.unwrap_or_default().trim().to_string();

    if content.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            axum::Json(json!({ "error": "content is required" })),
        )
            .into_response());
    }

    // Validate bot exists if an explicit botId was provided (F-00)
    if let Some(ref bot_id_ref) = req.bot_id
        && store::get_bot(&db, bot_id_ref)?.is_none()
    {
        return Ok((
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": "no such bot" })),
        )
            .into_response());
    }

    let bot_id = req.bot_id.unwrap_or_else(|| "josh".to_string());

    let entry = store::memory::remember_scoped_with_source(
        &db,
        &bot_id,
        &content,
        store::memory::Scope::Shared,
        None,
        &bot_id,
    )?;
    state.runs.changes.touch(crate::changes::ChangeKind::Memory);

    Ok((
        StatusCode::CREATED,
        axum::Json(json!({
            "entry": {
                "id": entry.id,
                "source": entry.source,
                "content": entry.content,
                "createdAt": entry.created_at,
                "kind": entry.kind,
                "expiresAt": entry.expires_at,
                "scope": entry.scope,
                "projectId": entry.project_id,
                "botId": bot_id,
            }
        })),
    )
        .into_response())
}

/// DELETE /api/memory/shared/:entryId
async fn delete_shared_memory_entry(
    State(state): State<AppState>,
    Path(entry_id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let db = state.db();
    let removed = db.conn().execute(
        "DELETE FROM memory_log WHERE id = ?1 AND scope = 'shared'",
        rusqlite::params![entry_id],
    )? > 0;

    if removed {
        state.runs.changes.touch(crate::changes::ChangeKind::Memory);
        Ok((StatusCode::OK, axum::Json(json!({ "ok": true }))).into_response())
    } else {
        Ok((
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": "no such entry" })),
        )
            .into_response())
    }
}
