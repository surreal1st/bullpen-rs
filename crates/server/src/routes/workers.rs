//! Workers registry + per-bot worker assignment.
//! Port of `projects/bullpen-night/src/server/app.ts:4226-4239,4456-4486`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::{Value, json};

use super::parse_body;
use crate::workers::{
    get_bot_worker_id, list_workers, parse_worker_input, remove_worker, set_bot_worker_id,
    test_worker_without_docker, upsert_worker, worker_public_to_value,
};
use crate::{ApiResult, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/workers", get(list_workers_route).post(create_worker))
        .route(
            "/api/workers/{id}",
            put(update_worker).delete(delete_worker),
        )
        .route("/api/workers/{id}/test", post(test_worker_route))
        .route(
            "/api/bots/{id}/worker",
            get(get_bot_worker).put(put_bot_worker),
        )
}

async fn list_workers_route(State(state): State<AppState>) -> impl IntoResponse {
    let db = state.db();
    let workers: Vec<Value> = list_workers(&db)
        .iter()
        .map(worker_public_to_value)
        .collect();
    Json(json!({ "workers": workers }))
}

async fn create_worker(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> ApiResult<Response> {
    let raw: Value = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&body)
            .map_err(|_| crate::AppError::bad_request("invalid JSON body"))?
    };
    let db = state.db();
    let result = upsert_worker(&db, &parse_worker_input(&raw));
    if !result.ok {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": result.error })),
        )
            .into_response());
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({ "worker": worker_public_to_value(result.worker.as_ref().unwrap()) })),
    )
        .into_response())
}

async fn update_worker(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Response> {
    let db = state.db();
    if !list_workers(&db).iter().any(|w| w.id == id) {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such worker" })),
        )
            .into_response());
    }
    let raw: Value = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&body)
            .map_err(|_| crate::AppError::bad_request("invalid JSON body"))?
    };
    let mut input = parse_worker_input(&raw);
    input.id = Some(id);
    let result = upsert_worker(&db, &input);
    if !result.ok {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": result.error })),
        )
            .into_response());
    }
    Ok(
        Json(json!({ "worker": worker_public_to_value(result.worker.as_ref().unwrap()) }))
            .into_response(),
    )
}

async fn delete_worker(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let db = state.db();
    let workers: Vec<Value> = remove_worker(&db, &id)
        .iter()
        .map(worker_public_to_value)
        .collect();
    Json(json!({ "workers": workers }))
}

async fn test_worker_route(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let db = state.db();
    let checked_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let worker = test_worker_without_docker(&db, &id, checked_at);
    if worker.is_none() {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such worker" })),
        )
            .into_response());
    }
    Ok(Json(json!({ "worker": worker_public_to_value(&worker.unwrap()) })).into_response())
}

fn no_such_bot() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "no such bot" })),
    )
        .into_response()
}

async fn get_bot_worker(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let db = state.db();
    if store::get_bot(&db, &id)?.is_none() {
        return Ok(no_such_bot());
    }
    let worker_id = get_bot_worker_id(&db, &id)?;
    Ok(Json(json!({ "workerId": worker_id })).into_response())
}

async fn put_bot_worker(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Response> {
    let db = state.db();
    if store::get_bot(&db, &id)?.is_none() {
        return Ok(no_such_bot());
    }
    #[derive(serde::Deserialize, Default)]
    struct Body {
        #[serde(default, rename = "workerId")]
        worker_id: Option<String>,
    }
    let parsed: Body = parse_body(&body)?;
    let worker_id = parsed
        .worker_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let worker_id = set_bot_worker_id(&db, &id, worker_id)?;
    Ok(Json(json!({ "workerId": worker_id })).into_response())
}
