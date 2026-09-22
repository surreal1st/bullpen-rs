//! S12-04: `POST /api/transcribe` — port of `app.ts` push-to-talk route.

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;

use crate::transcribe::{self, MAX_AUDIO_BYTES, TranscribeOptions};
use crate::{ApiResult, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/transcribe", post(post_transcribe))
        // Let the handler enforce MAX_AUDIO_BYTES and return JSON 413 (TS parity).
        .layer(DefaultBodyLimit::max(MAX_AUDIO_BYTES + 1))
}

async fn post_transcribe(
    State(_state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if body.len() > MAX_AUDIO_BYTES {
        let mb = format!("{:.1}", body.len() as f64 / 1024.0 / 1024.0);
        return Ok((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({
                "error": format!("That recording is {mb} MB. The limit is 4 MB."),
                "limitBytes": MAX_AUDIO_BYTES,
            })),
        )
            .into_response());
    }

    if transcribe::format_for(content_type).is_none() {
        let label = if content_type.trim().is_empty() {
            "unknown"
        } else {
            content_type.trim()
        };
        return Ok((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(json!({ "error": format!("Unsupported audio type: {label}.") })),
        )
            .into_response());
    }

    let result = transcribe::transcribe(
        &body,
        content_type,
        TranscribeOptions {
            model: None,
            api_key: None,
            timeout_ms: 120_000,
            http: None,
        },
    )
    .await;

    if result.ok {
        Ok((StatusCode::OK, Json(json!({ "text": result.text }))).into_response())
    } else {
        Ok((
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": result.detail })),
        )
            .into_response())
    }
}
