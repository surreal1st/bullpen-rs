//! One bot's own conversation view, and its thread strip. Port of
//! `src/server/app.ts:1445-1465` (conversation view) and the threads half
//! of `app.ts:2447-2530` (`GET`/`POST /api/bots/:id/threads`,
//! `PATCH`/`DELETE /api/threads/:id`).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/bots/{id}/conversation", get(conversation_view))
        .route(
            "/api/bots/{id}/threads",
            get(list_threads).post(create_thread),
        )
        .route(
            "/api/threads/{id}",
            axum::routing::patch(rename_thread).delete(archive_thread),
        )
}

#[derive(Deserialize, Default)]
struct ConversationQuery {
    thread: Option<String>,
}

fn no_such_bot() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"error": "no such bot"}))).into_response()
}

async fn conversation_view(
    State(state): State<AppState>,
    Path(bot_id): Path<String>,
    Query(query): Query<ConversationQuery>,
) -> Response {
    let db = state.db.lock().expect("db mutex poisoned");
    let Some(bot) = store::get_bot(&db, &bot_id).expect("get_bot") else {
        return no_such_bot();
    };

    let conversation_id = match query.thread.filter(|t| !t.is_empty()) {
        Some(id) => id,
        None => {
            let threads = store::list_threads(&db, &bot.id).expect("list_threads");
            match threads.first() {
                Some(t) => t.id.clone(),
                None => store::get_or_create_conversation(&db, &bot.id)
                    .expect("get_or_create_conversation"),
            }
        }
    };

    let effective_model = bot
        .model
        .clone()
        .unwrap_or_else(|| model::ladder::default_model(&db));
    let messages = store::list_messages(&db, &conversation_id).expect("list_messages");

    Json(json!({
        "conversationId": conversation_id,
        "bot": bot,
        "effectiveModel": effective_model,
        "messages": messages,
    }))
    .into_response()
}

async fn list_threads(State(state): State<AppState>, Path(bot_id): Path<String>) -> Response {
    let db = state.db.lock().expect("db mutex poisoned");
    let Some(bot) = store::get_bot(&db, &bot_id).expect("get_bot") else {
        return no_such_bot();
    };
    // Make sure there is always one to talk in.
    if store::list_threads(&db, &bot.id)
        .expect("list_threads")
        .is_empty()
    {
        store::get_or_create_conversation(&db, &bot.id).expect("get_or_create_conversation");
    }
    let threads = store::list_threads(&db, &bot.id).expect("list_threads");
    Json(json!({ "threads": threads })).into_response()
}

#[derive(Deserialize, Default)]
struct CreateThreadBody {
    #[serde(default)]
    members: Vec<String>,
}

async fn create_thread(
    State(state): State<AppState>,
    Path(bot_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let db = state.db.lock().expect("db mutex poisoned");
    let Some(bot) = store::get_bot(&db, &bot_id).expect("get_bot") else {
        return no_such_bot();
    };
    let parsed: CreateThreadBody = serde_json::from_slice(&body).unwrap_or_default();
    match store::create_thread(&db, &bot.id, &parsed.members) {
        Ok(thread) => (StatusCode::CREATED, Json(json!({ "thread": thread }))).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response(),
    }
}

#[derive(Deserialize, Default)]
struct RenameThreadBody {
    #[serde(default)]
    title: Option<String>,
}

async fn rename_thread(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let parsed: RenameThreadBody = serde_json::from_slice(&body).unwrap_or_default();
    let title = parsed.title.unwrap_or_default();
    let db = state.db.lock().expect("db mutex poisoned");
    match store::rename_thread(&db, &id, &title).expect("rename_thread") {
        true => Json(json!({ "ok": true })).into_response(),
        false => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no such thread"})),
        )
            .into_response(),
    }
}

async fn archive_thread(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let db = state.db.lock().expect("db mutex poisoned");
    match store::archive_thread(&db, &id).expect("archive_thread") {
        true => Json(json!({ "ok": true })).into_response(),
        false => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no such thread"})),
        )
            .into_response(),
    }
}
