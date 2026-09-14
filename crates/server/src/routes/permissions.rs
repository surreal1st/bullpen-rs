//! S2-02: GET/PUT /api/bots/:id/permissions. Port of
//! `projects/bullpen-night/src/server/app.ts:4176-4190`.

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::parse_body;
use crate::{AppState, permissions::Decision};

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

// F10: a typed `HashMap<String, Decision>` here meant serde rejected the
// WHOLE body the moment any one value was not `allow`/`ask`/`deny` - a
// stale tab or an old iOS build sending one legacy value ("always",
// "never", null) lost every other change in the same PUT. TS's own
// `setPermissions` (`permissions.ts:512-521`) keeps the valid entries and
// drops the rest, so the body is untyped here and filtered below the same
// way.
#[derive(Deserialize, Default)]
struct SetPermissionsBody {
    permissions: Option<HashMap<String, Value>>,
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
    let raw = parsed.permissions.unwrap_or_default();
    let input = raw
        .into_iter()
        .filter_map(|(k, v)| {
            let dec = v.as_str().and_then(Decision::parse_decision)?;
            Some((k, dec))
        })
        .collect();
    let perms = crate::permissions::set_permissions(&db, &bot_id, &input)?;

    Ok(Json(json!({ "permissions": perms })).into_response())
}
