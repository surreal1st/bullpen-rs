//! S2-05: GET/PUT /api/spend and /api/spend/ceiling. Port of
//! `projects/bullpen-night/src/server/app.ts:1345-1380`.

use axum::extract::{Query, State};
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
    /// Dollars spent, all time, across everything using this key -
    /// `CreditsPort::total_usage`'s own number. TS's fuller `AccountSpend`
    /// (`totalCredits`/`remaining` too) needs `total_credits`, which
    /// nothing on `AppState` reads yet - no client panel consumes this
    /// route today (`grep -rn /api/spend crates/client` is empty), so
    /// there is nothing this would silently break.
    total_usage: f64,
}

#[derive(Deserialize)]
struct SpendQuery {
    #[serde(default)]
    month: Option<String>,
}

async fn get_spend(
    State(state): State<AppState>,
    Query(query): Query<SpendQuery>,
) -> Result<impl IntoResponse, AppError> {
    // F11: `?month=` (`app.ts:1346`) was previously ignored - the panel
    // could only ever see the current month.
    let month = query
        .month
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| spend::current_month(chrono::Utc::now()));
    let ceiling = {
        let db = state.db();
        spend::get_ceiling(&db)
    };

    let bots = {
        let db = state.db();
        spend::spend_by_bot(&db, &month)?
    };

    // F3/D6: reads the real balance now instead of hardcoding
    // `account: None, accountReadable: false` - a read failure answers
    // readable=false with the (already-redacted, by `CreditsPort`) error
    // rather than a panic or a 500.
    match state.credits.total_usage().await {
        Ok(total_usage) => Ok(Json(GetSpendResponse {
            month,
            ceiling,
            account: Some(AccountSpend { total_usage }),
            bots,
            account_readable: true,
            error: None,
        })),
        Err(err) => Ok(Json(GetSpendResponse {
            month,
            ceiling,
            account: None,
            bots,
            account_readable: false,
            error: Some(err),
        })),
    }
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
    // F-SPEND-01a: an absent `ceiling` key (a missing body included - an
    // empty body decodes to `PutCeilingBody::default()`, same `None`) used
    // to fall back to `unwrap_or(0.0)`, a value `is_finite() && >= 0.0`
    // happily accepts - so a body-less PUT silently zeroed the ceiling and
    // stopped every run instead of being refused like every other bad
    // value. `Some(v)` is now required same as a genuine number is.
    let Some(value) = parsed.ceiling else {
        return Err(AppError::bad_request(
            "ceiling must be a number of dollars, zero or more",
        ));
    };

    if !value.is_finite() || value < 0.0 {
        return Err(AppError::bad_request(
            "ceiling must be a number of dollars, zero or more",
        ));
    }

    let db = state.db();
    let ceiling = spend::set_ceiling(&db, value)?;
    Ok(Json(json!({ "ceiling": ceiling })))
}
