//! Questions routes: GET to list open questions, POST to answer one.
//! Port of `app.ts:4164-4175`.

use crate::{ApiResult, AppState};
use axum::Router;
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::json;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/questions", get(list_open_questions))
        .route("/api/questions/{id}", post(answer_question))
}

/// GET /api/questions: List all open questions for Josh.
async fn list_open_questions(State(state): State<AppState>) -> ApiResult<impl IntoResponse> {
    let db = state.db();
    let questions = store::list_all_open(&db)?;

    Ok(axum::Json(json!({
        "questions": questions
            .into_iter()
            .map(|q| json!({
                "id": q.id,
                "botId": q.bot_id,
                "conversationId": q.conversation_id,
                "question": q.question,
                "options": q.options,
                "askedAt": q.asked_at,
            }))
            .collect::<Vec<_>>()
    })))
}

#[derive(Deserialize, Default)]
struct AnswerRequest {
    answer: String,
}

/// POST /api/questions/:id: Record Josh's answer and post it to the conversation.
async fn answer_question(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<impl IntoResponse> {
    let db = state.db();
    let req = super::parse_body::<AnswerRequest>(&body)?;
    let answer = req.answer.trim();

    if answer.is_empty() {
        return Err(crate::AppError::bad_request("answer cannot be empty"));
    }

    // Record the answer in the questions table.
    let answered = store::answer_question(&db, &id, answer)?;

    if !answered {
        return Err(crate::AppError::not_found("no such open question"));
    }

    // Find the question to get bot_id and conversation_id.
    // Read directly from the questions table.
    let (_bot_id, conversation_id) = db
        .conn()
        .query_row(
            "SELECT bot_id, conversation_id FROM questions WHERE id = ?",
            rusqlite::params![&id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|_| crate::AppError::not_found("no such open question"))?;

    // Post Josh's answer as an ordinary user message into the conversation.
    store::append_message(
        &db,
        &conversation_id,
        "user",
        answer,
        store::NewMessage::default(),
    )?;

    // Emit a change event so clients re-fetch questions.
    state
        .runs
        .changes
        .touch(crate::changes::ChangeKind::Questions);

    Ok(axum::Json(json!({ "ok": true })))
}
