//! S7-01: connector registry and per-bot enablement — port of
//! `projects/bullpen-night/src/server/app.ts:2384-2445`.

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::routing::{delete, get, put};
use axum::{Json, Router};
use serde_json::json;

use crate::{ApiResult, AppError, AppState};
use store::{connectors, get_bot};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/connectors", get(list).post(create))
        .route("/api/connectors/{id}/tools", get(connector_tools))
        .route("/api/connectors/{id}", delete(remove))
        .route("/api/bots/{id}/connectors", get(bot_connectors))
        .route(
            "/api/bots/{id}/connectors/{connector_id}",
            put(set_bot_connector_handler),
        )
}

async fn list(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    let list = connectors::list_connectors(&db)?;
    Ok(Json(json!({ "connectors": list })))
}

async fn create(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> ApiResult<impl IntoResponse> {
    let (connector, connector_id) = {
        let db = state.db();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
        let name = value.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let url = value.get("url").and_then(|v| v.as_str()).unwrap_or("");
        let auth_header = value
            .get("authHeader")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());

        let result = connectors::add_connector(&db, name, url, auth_header)?;
        if !result.ok {
            return Err(AppError::bad_request(
                result
                    .error
                    .unwrap_or_else(|| "could not add connector".to_string()),
            ));
        }
        let connector = result.connector.clone();
        let connector_id = connector.as_ref().map(|c| c.id.clone()).unwrap_or_default();
        (connector, connector_id)
    };
    let reached = state.refresh_connector_tools(&connector_id).await;
    Ok((
        axum::http::StatusCode::CREATED,
        Json(json!({
            "connector": connector,
            "reachable": reached.ok,
            "error": reached.error,
        })),
    ))
}

async fn connector_tools(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<axum::response::Response> {
    let listed = state.refresh_connector_tools(&id).await;
    if listed.error.as_deref() == Some("no such connector") {
        return Err(AppError::not_found("no such connector"));
    }
    if listed.ok {
        Ok((
            axum::http::StatusCode::OK,
            Json(json!({ "tools": listed.tools })),
        )
            .into_response())
    } else {
        Ok((
            axum::http::StatusCode::BAD_GATEWAY,
            Json(json!({ "error": listed.error, "tools": [] })),
        )
            .into_response())
    }
}

async fn remove(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    if connectors::remove_connector(&db, &id)? {
        Ok(Json(json!({ "ok": true })))
    } else {
        Err(AppError::not_found("no such connector"))
    }
}

async fn bot_connectors(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    if get_bot(&db, &id)?.is_none() {
        return Err(AppError::not_found("no such bot"));
    }
    let enabled: std::collections::HashSet<String> = connectors::connectors_for_bot(&db, &id)?
        .into_iter()
        .map(|c| c.id)
        .collect();
    let list = connectors::list_connectors(&db)?;
    let out: Vec<serde_json::Value> = list
        .into_iter()
        .map(|c| {
            json!({
                "id": c.id,
                "name": c.name,
                "url": c.url,
                "createdAt": c.created_at,
                "enabled": enabled.contains(&c.id),
            })
        })
        .collect();
    Ok(Json(json!({ "connectors": out })))
}

async fn set_bot_connector_handler(
    State(state): State<AppState>,
    Path((bot_id, connector_id)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    if get_bot(&db, &bot_id)?.is_none() {
        return Err(AppError::not_found("no such bot"));
    }
    if connectors::get_connector(&db, &connector_id)?.is_none() {
        return Err(AppError::not_found("no such connector"));
    }
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let enabled = value
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    connectors::set_bot_connector(&db, &bot_id, &connector_id, enabled)?;
    Ok(Json(json!({ "ok": true })))
}
