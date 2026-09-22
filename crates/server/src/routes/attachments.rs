//! S12-01: `/api/attachments*` and `/api/library` — port of `attachments.ts` + `library.ts`.

use axum::body::{Body, Bytes};
use axum::extract::{Extension, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::scope::Scope;
use crate::{ApiResult, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/attachments", post(post_attachment))
        .route(
            "/api/attachments/{id}",
            get(get_attachment).delete(delete_attachment_route),
        )
        .route("/api/library", get(get_library))
}

async fn post_attachment(
    State(state): State<AppState>,
    Extension(scope): Extension<Scope>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let name = headers
        .get("x-file-name")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("file");
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream");

    let db = state.db();
    let data_dir = state.data_dir();
    match store::store_attachment(
        &db,
        &data_dir,
        store::StoreAttachmentInput {
            name,
            content_type,
            data: &body,
            bot_id: None,
            kind: None,
            user_id: Some(scope.user_id.as_str()),
        },
    ) {
        Ok(attachment) => Ok((
            StatusCode::CREATED,
            Json(json!({ "attachment": attachment })),
        )
            .into_response()),
        Err(store::StoreAttachmentError::Empty) => Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "That file is empty." })),
        )
            .into_response()),
        Err(store::StoreAttachmentError::TooLarge { bytes }) => {
            let mb = format!("{:.1}", bytes as f64 / 1024.0 / 1024.0);
            Ok((
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({
                    "error": format!("That file is {mb} MB. The limit is 25 MB."),
                    "limitBytes": store::MAX_ATTACHMENT_BYTES,
                })),
            )
                .into_response())
        }
        Err(store::StoreAttachmentError::Store(err)) => Err(err.into()),
    }
}

async fn get_attachment(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<Response> {
    let db = state.db();
    let data_dir = state.data_dir();
    let Some(attachment) = store::get_attachment(&db, &id)? else {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such attachment" })),
        )
            .into_response());
    };
    if !store::attachment_exists(&data_dir, &id) {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such attachment" })),
        )
            .into_response());
    }
    let bytes =
        store::read_attachment(&data_dir, &id).map_err(|e| format!("read attachment: {e}"))?;
    let safe_name = attachment.name.replace('"', "");
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        attachment
            .content_type
            .parse()
            .unwrap_or_else(|_| header::HeaderValue::from_static("application/octet-stream")),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_str(&format!("inline; filename=\"{safe_name}\""))
            .unwrap_or_else(|_| header::HeaderValue::from_static("inline")),
    );
    Ok(response)
}

async fn delete_attachment_route(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<Response> {
    let db = state.db();
    let data_dir = state.data_dir();
    if store::delete_attachment(&db, &data_dir, &id)? {
        Ok((StatusCode::OK, Json(json!({ "ok": true }))).into_response())
    } else {
        Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such attachment" })),
        )
            .into_response())
    }
}

#[derive(Debug, Deserialize, Default)]
struct LibraryParams {
    q: Option<String>,
    bot: Option<String>,
    kind: Option<String>,
}

async fn get_library(
    State(state): State<AppState>,
    Extension(scope): Extension<Scope>,
    Query(params): Query<LibraryParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    let items = store::list_library(
        &db,
        store::LibraryQuery {
            q: params.q.as_deref(),
            bot: params.bot.as_deref(),
            kind: params.kind.as_deref(),
            limit: None,
            scope: Some(scope.list_filter()),
        },
    )?;
    Ok(Json(json!({ "items": items })))
}
