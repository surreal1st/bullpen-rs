//! S2-05: GET/PUT /api/spend and /api/spend/ceiling. Port of
//! `projects/bullpen-night/src/server/app.ts:1345-1380`.

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{AppError, AppState, spend};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/spend", get(get_spend))
        .route("/api/spend/ceiling", put(put_ceiling))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GetSpendResponse {
    month: String,
    ceiling: f64,
    account: Option<AccountSpend>,
    bots: Vec<spend::BotSpend>,
    #[serde(rename = "accountReadable")]
    account_readable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountSpend {
    /// Dollars purchased on the account.
    total_credits: f64,
    /// Dollars spent, all time, across everything using this key.
    total_usage: f64,
    /// Dollars left.
    remaining: f64,
}

async fn get_spend(State(state): State<AppState>) -> Result<impl IntoResponse, AppError> {
    let month = spend::current_month(chrono::Utc::now());
    let ceiling = {
        let db = state.db();
        spend::get_ceiling(&db)
    };

    let bots = {
        let db = state.db();
        spend::spend_by_bot(&db, &month)?
    };

    // Note: S2-05 does not implement OpenRouter credits reading yet.
    // That would require a CreditsPort (network dependency).
    // For now, return the database info only.
    Ok(Json(GetSpendResponse {
        month,
        ceiling,
        account: None,
        bots,
        account_readable: false,
        error: None,
    }))
}

#[derive(Deserialize, Default)]
struct PutCeilingBody {
    ceiling: Option<f64>,
}

async fn put_ceiling(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, AppError> {
    let parsed: PutCeilingBody = super::parse_body(&body)?;
    let value = parsed.ceiling.unwrap_or(0.0);

    if !value.is_finite() || value < 0.0 {
        return Err(AppError::bad_request(
            "ceiling must be a number of dollars, zero or more",
        ));
    }

    let db = state.db();
    let ceiling = spend::set_ceiling(&db, value)?;
    Ok(Json(json!({ "ceiling": ceiling })))
}
