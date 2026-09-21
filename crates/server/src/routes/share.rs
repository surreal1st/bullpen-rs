//! S11-05: unauthenticated bot export via share token (`GET /api/share/:token`).

use axum::extract::{Path, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use super::bots::bot_export_markdown;
use crate::{ApiResult, AppState};

pub fn router() -> Router<AppState> {
    Router::new().route("/api/share/{token}", get(serve_share_export))
}

async fn serve_share_export(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> ApiResult<Response> {
    let db = state.db();
    let Some(bot_id) = crate::share::verify_share_token(&db, &token)? else {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "invalid or expired share link" })),
        )
            .into_response());
    };
    let Some(bot) = store::get_bot(&db, &bot_id)? else {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "bot not found" })),
        )
            .into_response());
    };

    let markdown = bot_export_markdown(&bot)?;
    let mut response = (StatusCode::OK, markdown).into_response();
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/markdown; charset=utf-8"),
    );
    response.headers_mut().insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{}.md\"", bot.id))
            .expect("slug-shaped bot id is always a valid header value"),
    );
    Ok(response)
}
