//! W5: `GET /api/bot-tools`, `DELETE /api/bot-tools/:name`, `GET /api/tools`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;

use crate::{ApiResult, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/bot-tools", get(list_bot_tools))
        .route("/api/bot-tools/{name}", delete(revoke))
        .route("/api/tools", get(list_tool_specs))
}

async fn list_bot_tools(State(state): State<AppState>) -> ApiResult<Response> {
    let tools = crate::bot_tools::list_tools(&state.db_handle());
    Ok(Json(json!({ "tools": tools })).into_response())
}

async fn revoke(State(state): State<AppState>, Path(name): Path<String>) -> ApiResult<Response> {
    let gone = crate::bot_tools::revoke_tool(&state.db_handle(), &name);
    Ok(if gone {
        Json(json!({ "ok": true })).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such tool" })),
        )
            .into_response()
    })
}

#[derive(Serialize)]
struct ToolNameDesc {
    name: String,
    description: String,
}

/// Same shape as TS `GET /api/tools` — built off the seed bot's toolbox.
async fn list_tool_specs(State(state): State<AppState>) -> ApiResult<Response> {
    let toolbox = state.runs.toolbox_for(
        "arthur",
        model::ladder::Trigger::Chat,
        false,
        "test/model",
        None,
    );
    let tools: Vec<ToolNameDesc> = toolbox
        .specs
        .iter()
        .map(|spec| ToolNameDesc {
            name: spec.name.clone(),
            description: spec.description.clone(),
        })
        .collect();
    Ok(Json(json!({ "tools": tools })).into_response())
}
