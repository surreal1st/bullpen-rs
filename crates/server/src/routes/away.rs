//! W6: GET /api/away, POST /api/away/dismiss. Port of `app.ts` away routes.

use crate::{AppError, AppState};
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/away", get(get_away))
        .route("/api/away/dismiss", post(dismiss))
}

async fn get_away(
    State(state): State<AppState>,
) -> Result<Json<crate::away::AwayPayload>, AppError> {
    let db = state.db_handle();
    let port = state.runs.model_port();
    let payload = crate::away::compute_away(db, port, None, chrono::Utc::now()).await?;
    Ok(Json(payload))
}

async fn dismiss(State(state): State<AppState>) -> Result<Json<serde_json::Value>, AppError> {
    let db = state.db();
    crate::away::dismiss_away(&db)?;
    Ok(Json(json!({ "ok": true })))
}
