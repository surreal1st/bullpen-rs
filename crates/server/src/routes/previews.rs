//! S12-06: link previews — `POST /api/previews`, `GET /api/preview/image`.

use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::link_preview::{
    IMAGE_TYPES, MAX_IMAGE_BYTES, PreviewOptions, image_allowed, links_in, preview_for,
};
use crate::web_fetch::{FetchOptions, ReqwestWebFetch, WebFetchPolicy, fetch_for_bot};
use crate::{ApiResult, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/previews", post(post_previews))
        .route("/api/preview/image", get(get_preview_image))
}

#[derive(Deserialize)]
struct PreviewsBody {
    text: Option<String>,
}

async fn post_previews(
    State(state): State<AppState>,
    Json(body): Json<PreviewsBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let text = body.text.unwrap_or_default();
    if text.is_empty() {
        return Ok(Json(json!({ "previews": [] })));
    }

    let db = state.db_handle();
    let fetch = ReqwestWebFetch::new();
    let resolver = state.mcp_resolver_handle();
    let urls = links_in(&text, 3);
    let mut previews = Vec::new();
    for url in urls {
        if let Some(p) = preview_for(
            &db,
            &url,
            PreviewOptions {
                fetch: &fetch,
                resolver: resolver.as_ref(),
                now: None,
            },
        )
        .await
        {
            previews.push(p);
        }
    }
    Ok(Json(json!({ "previews": previews })))
}

#[derive(Deserialize)]
struct ImageQuery {
    url: Option<String>,
}

async fn get_preview_image(
    State(state): State<AppState>,
    Query(query): Query<ImageQuery>,
) -> ApiResult<Response> {
    let url = query.url.unwrap_or_default();
    if !image_allowed(&url) {
        return Ok((StatusCode::BAD_REQUEST, "not an https image url").into_response());
    }

    let policy = {
        let db = state.db();
        WebFetchPolicy::from_db(&db)
    };
    let fetch = ReqwestWebFetch::new();
    let outcome = fetch_for_bot(
        &policy,
        &url,
        state.mcp_resolver_handle().as_ref(),
        &fetch,
        FetchOptions {
            max_bytes: Some(MAX_IMAGE_BYTES),
            timeout_ms: Some(15_000),
            binary: true,
            _marker: std::marker::PhantomData,
        },
    )
    .await;

    if outcome.error.is_some() || outcome.bytes.is_none() {
        return Ok((StatusCode::BAD_GATEWAY, "could not fetch that image").into_response());
    }

    let content_type = outcome.content_type.unwrap_or_default();
    let type_ = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if !IMAGE_TYPES.contains(&type_.as_str()) {
        return Ok((StatusCode::UNSUPPORTED_MEDIA_TYPE, "not an image").into_response());
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_str(&type_)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=300"),
    );

    Ok((StatusCode::OK, headers, outcome.bytes.unwrap()).into_response())
}
