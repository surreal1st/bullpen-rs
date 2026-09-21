//! S11-06: APNs device registry + status. Port of `app.ts` push routes.

use crate::{ApiResult, AppState};
use axum::extract::{Path, State};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::json;
use store::push::{self, PushEnvironment};

use super::parse_body;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/push", get(describe))
        .route("/api/push/devices", post(register))
        .route("/api/push/devices/{token}", delete(forget))
}

async fn describe(State(state): State<AppState>) -> ApiResult<impl axum::response::IntoResponse> {
    let db = state.db();
    Ok(Json(crate::push::describe_push(&db)))
}

#[derive(serde::Deserialize, Default)]
struct RegisterBody {
    token: Option<String>,
    environment: Option<String>,
}

async fn register(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> ApiResult<impl axum::response::IntoResponse> {
    let parsed: RegisterBody = parse_body(&body)?;
    let token = parsed.token.unwrap_or_default();
    let environment = parsed
        .environment
        .as_deref()
        .and_then(PushEnvironment::parse)
        .unwrap_or(PushEnvironment::Production);

    let db = state.db();
    if push::register_device(&db, &token, environment) {
        Ok(Json(
            json!({ "ok": true, "environment": environment.as_str() }),
        ))
    } else {
        Err(crate::AppError::bad_request(
            "that is not an APNs device token",
        ))
    }
}

async fn forget(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> ApiResult<impl axum::response::IntoResponse> {
    let db = state.db();
    push::forget_device(&db, &token);
    Ok(Json(json!({ "ok": true })))
}
