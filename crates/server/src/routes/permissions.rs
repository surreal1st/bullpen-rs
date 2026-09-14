//! S2-02: GET/PUT /api/bots/:id/permissions. Port of
//! `projects/bullpen-night/src/server/app.ts:4176-4190`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::parse_body;
use crate::{AppState, permissions::Permissions};

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/api/bots/{id}/permissions",
        get(get_permissions).put(set_permissions),
    )
}

fn no_such_bot() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"error": "no such bot"}))).into_response()
}

async fn get_permissions(
    State(state): State<AppState>,
    Path(bot_id): Path<String>,
) -> Result<Response, crate::AppError> {
    let db = state.db();
    let Some(_bot) = store::get_bot(&db, &bot_id)? else {
        return Ok(no_such_bot());
    };

    let perms = crate::permissions::get_permissions(&db, &bot_id)?;
    Ok(Json(json!({ "permissions": perms })).into_response())
}

#[derive(Deserialize, Default)]
struct SetPermissionsBody {
    permissions: Option<Permissions>,
}

async fn set_permissions(
    State(state): State<AppState>,
    Path(bot_id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, crate::AppError> {
    let db = state.db();
    let Some(_bot) = store::get_bot(&db, &bot_id)? else {
        return Ok(no_such_bot());
    };

    let parsed: SetPermissionsBody = parse_body(&body)?;
    let input = parsed.permissions.unwrap_or_default();
    let perms = crate::permissions::set_permissions(&db, &bot_id, &input)?;

    Ok(Json(json!({ "permissions": perms })).into_response())
}
