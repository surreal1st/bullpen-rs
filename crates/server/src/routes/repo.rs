//! S9-07: `GET`/`PUT /api/bots/:id/repo` — port of `app.ts:4213-4224`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use serde_json::json;

use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/bots/{id}/repo", get(get_bot_repo))
        .route("/api/bots/{id}/repo", put(put_bot_repo))
}

fn no_such_bot() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"error": "no such bot"}))).into_response()
}

async fn get_bot_repo(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, crate::AppError> {
    let db = state.db();
    if store::get_bot(&db, &id)?.is_none() {
        return Ok(no_such_bot());
    }
    let repo = crate::repo::get_bot_repo(&db, &id)?;
    Ok((StatusCode::OK, Json(json!({ "repo": repo }))).into_response())
}

async fn put_bot_repo(
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
    let repo_input = parsed
        .get("repo")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let repo = crate::repo::set_bot_repo(&db, &id, &repo_input)?;
    Ok((StatusCode::OK, Json(json!({ "repo": repo }))).into_response())
}
