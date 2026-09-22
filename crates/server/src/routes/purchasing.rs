//! S12-07: Stripe Issuing purchasing routes.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::purchasing::{
    ReqwestStripeHttp, STRIPE_REPLAY_TOLERANCE_SECONDS, create_issuing_card, get_stripe_config,
    handle_stripe_webhook, purchasing_status, purchasing_summary, save_stripe_keys, set_allow_live,
    set_bot_purchasing, unix_now, verify_stripe_signature,
};
use crate::{ApiResult, AppState};

const HOOK_BODY_MAX_BYTES: usize = 64 * 1024;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/purchasing", get(get_purchasing_status))
        .route("/api/purchasing/keys", put(put_purchasing_keys))
        .route("/api/purchasing/allow-live", put(put_allow_live))
        .route("/api/bots/{id}/purchasing", get(get_bot_purchasing))
        .route("/api/bots/{id}/purchasing", put(put_bot_purchasing))
        .route(
            "/api/bots/{id}/purchasing/card",
            post(post_bot_purchasing_card),
        )
        .route("/api/stripe/webhook", post(post_stripe_webhook))
}

async fn get_purchasing_status(
    State(state): State<AppState>,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    Ok(Json(json!(purchasing_status(&db))))
}

#[derive(Deserialize)]
struct KeysBody {
    #[serde(rename = "secretKey")]
    secret_key: Option<String>,
    #[serde(rename = "webhookSecret")]
    webhook_secret: Option<String>,
    #[serde(rename = "cardholderId")]
    cardholder_id: Option<String>,
}

async fn put_purchasing_keys(
    State(state): State<AppState>,
    Json(body): Json<KeysBody>,
) -> ApiResult<Response> {
    let db = state.db();
    let result = save_stripe_keys(
        &db,
        body.secret_key.as_deref(),
        body.webhook_secret.as_deref(),
        body.cardholder_id.as_deref(),
    );
    if result.ok {
        Ok((StatusCode::OK, Json(json!(result.status))).into_response())
    } else {
        Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": result.error.unwrap_or_else(|| "That key was refused.".to_string()) })),
        )
            .into_response())
    }
}

#[derive(Deserialize)]
struct AllowLiveBody {
    allow: Option<bool>,
}

async fn put_allow_live(
    State(state): State<AppState>,
    Json(body): Json<AllowLiveBody>,
) -> ApiResult<Response> {
    let db = state.db();
    set_allow_live(&db, body.allow == Some(true)).map_err(crate::AppError::from)?;
    Ok(Json(json!(purchasing_status(&db))).into_response())
}

fn no_such_bot() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "no such bot" })),
    )
        .into_response()
}

async fn get_bot_purchasing(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let db = state.db();
    if store::get_bot(&db, &id)?.is_none() {
        return Ok(no_such_bot());
    }
    Ok(Json(json!(purchasing_summary(&db, &id))).into_response())
}

async fn put_bot_purchasing(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Json<serde_json::Value>,
) -> ApiResult<Response> {
    let db = state.db();
    if store::get_bot(&db, &id)?.is_none() {
        return Ok(no_such_bot());
    }
    let can_buy = body.get("canBuy").and_then(|v| v.as_bool());
    let monthly = match body.get("monthlyLimitUsd") {
        None => None,
        Some(v) if v.is_null() => Some(None),
        Some(v) => Some(v.as_f64().filter(|n| n.is_finite() && *n >= 0.0)),
    };
    set_bot_purchasing(&db, &id, can_buy, monthly).map_err(crate::AppError::from)?;
    Ok(Json(json!(purchasing_summary(&db, &id))).into_response())
}

async fn post_bot_purchasing_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    {
        let db = state.db();
        if store::get_bot(&db, &id).ok().flatten().is_none() {
            return no_such_bot();
        }
    }
    let stripe = ReqwestStripeHttp::new();
    let result = create_issuing_card(&state.db_handle(), &id, &stripe).await;
    if !result.ok {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": result.error.unwrap_or_else(|| "Could not create a card.".to_string()) })),
        )
            .into_response();
    }
    let db = state.db();
    Json(json!(purchasing_summary(&db, &id))).into_response()
}

async fn post_stripe_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let config = {
        let db = state.db();
        get_stripe_config(&db)
    };
    let Some(webhook_secret) = config.webhook_secret else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Stripe is not configured" })),
        )
            .into_response();
    };

    if body.len() > HOOK_BODY_MAX_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": "body too large" })),
        )
            .into_response();
    }

    let raw = std::str::from_utf8(&body).unwrap_or("");
    let sig = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok());
    if !verify_stripe_signature(
        &webhook_secret,
        raw,
        sig,
        STRIPE_REPLAY_TOLERANCE_SECONDS,
        unix_now(),
    ) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "not authorized" })),
        )
            .into_response();
    }

    let payload: serde_json::Value = if raw.is_empty() {
        json!({})
    } else {
        match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "bad json" })),
                )
                    .into_response();
            }
        }
    };

    let stripe = ReqwestStripeHttp::new();
    let outcome = handle_stripe_webhook(
        &state.db_handle(),
        config.secret_key.as_deref(),
        &payload,
        &stripe,
    )
    .await;

    Json(json!({
        "received": true,
        "handled": outcome.handled,
        "approved": outcome.approved,
    }))
    .into_response()
}
