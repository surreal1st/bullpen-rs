//! Group chats as their own roster entries. Port of the rooms half of
//! `src/server/app.ts:2447-2530` (`GET`/`POST /api/rooms`,
//! `PATCH`/`DELETE /api/rooms/:id`, `POST /api/rooms/:id/seen|unseen`). A
//! group chat is the same "room" the round engine (`crate::rooms`) already
//! runs underneath - these routes just list/create/update it as a roster
//! entry rather than a thread inside one bot's own strip.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/rooms", get(list_rooms).post(create_room))
        .route(
            "/api/rooms/{id}",
            axum::routing::patch(update_room).delete(archive_room),
        )
        .route("/api/rooms/{id}/seen", axum::routing::post(mark_seen))
        .route("/api/rooms/{id}/unseen", axum::routing::post(mark_unseen))
}

async fn list_rooms(State(state): State<AppState>) -> impl IntoResponse {
    let db = state.db.lock().expect("db mutex poisoned");
    let rooms = store::list_rooms(&db).expect("list_rooms");
    Json(json!({ "rooms": rooms }))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RoomBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    member_ids: Option<Vec<String>>,
}

async fn create_room(State(state): State<AppState>, body: axum::body::Bytes) -> Response {
    let parsed: RoomBody = serde_json::from_slice(&body).unwrap_or_default();
    let title = parsed.title.unwrap_or_default();
    let member_ids = parsed.member_ids.unwrap_or_default();
    let db = state.db.lock().expect("db mutex poisoned");
    match store::create_room(&db, &title, &member_ids) {
        Ok(room) => (StatusCode::CREATED, Json(json!({ "room": room }))).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response(),
    }
}

async fn update_room(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let parsed: RoomBody = serde_json::from_slice(&body).unwrap_or_default();
    let db = state.db.lock().expect("db mutex poisoned");
    match store::update_room(
        &db,
        &id,
        parsed.title.as_deref(),
        parsed.member_ids.as_deref(),
    ) {
        Ok(room) => Json(json!({ "room": room })).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response(),
    }
}

/// Same shape as an ordinary thread's delete: archives, never destroys.
async fn archive_room(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let db = state.db.lock().expect("db mutex poisoned");
    match store::archive_thread(&db, &id).expect("archive_thread") {
        true => Json(json!({ "ok": true })).into_response(),
        false => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no such group chat"})),
        )
            .into_response(),
    }
}

/// Opening a group chat is what makes it read - same rule as a bot.
async fn mark_seen(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let db = state.db.lock().expect("db mutex poisoned");
    match store::mark_room_seen(&db, &id).expect("mark_room_seen") {
        true => Json(json!({ "ok": true })).into_response(),
        false => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no such group chat"})),
        )
            .into_response(),
    }
}

/// The menu's "Mark as Unread" - the deliberate opposite of `/seen`.
async fn mark_unseen(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let db = state.db.lock().expect("db mutex poisoned");
    match store::mark_room_unread(&db, &id).expect("mark_room_unread") {
        true => Json(json!({ "ok": true })).into_response(),
        false => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no such group chat"})),
        )
            .into_response(),
    }
}
