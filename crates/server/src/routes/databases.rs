//! S12-08: database target settings routes.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::databases::{
    DbTargetInput, list_db_targets, parse_target_input, remove_db_target, test_db_target,
    upsert_db_target,
};
use crate::{ApiResult, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/databases", get(get_databases).post(post_database))
        .route(
            "/api/databases/{id}",
            put(put_database).delete(delete_database),
        )
        .route("/api/databases/{id}/test", post(post_database_test))
}

async fn get_databases(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let db = state.db();
    Ok(Json(json!({ "targets": list_db_targets(&db) })))
}

async fn post_database(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> ApiResult<Response> {
    let db = state.db();
    let input = parse_target_input(&body);
    let result = upsert_db_target(&db, input);
    if result.ok {
        Ok((
            StatusCode::CREATED,
            Json(json!({ "target": result.target })),
        )
            .into_response())
    } else {
        Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": result.error.unwrap_or_else(|| "Refused.".to_string()) })),
        )
            .into_response())
    }
}

async fn put_database(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Response> {
    let db = state.db();
    if !list_db_targets(&db).iter().any(|t| t.id == id) {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such target" })),
        )
            .into_response());
    }
    let parsed = parse_target_input(&body);
    let input = DbTargetInput {
        id: Some(id),
        label: parsed.label,
        kind: parsed.kind,
        path: parsed.path,
        dsn: parsed.dsn,
        tables: parsed.tables,
    };
    let result = upsert_db_target(&db, input);
    if result.ok {
        Ok(Json(json!({ "target": result.target })).into_response())
    } else {
        Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": result.error.unwrap_or_else(|| "Refused.".to_string()) })),
        )
            .into_response())
    }
}

async fn delete_database(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let db = state.db();
    Ok(Json(json!({ "targets": remove_db_target(&db, &id) })))
}

async fn post_database_test(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let outcome = test_db_target(state.db_handle(), &id).await;
    Json(serde_json::to_value(outcome).unwrap_or(json!({ "ok": false }))).into_response()
}
