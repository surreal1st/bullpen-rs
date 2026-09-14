//! `POST /api/bots/:id/messages`: send a message, start a run (or hand the
//! turn to a room's round), and stream the run back. Port of
//! `src/server/app.ts:1463-1656`.
//!
//! 🔴 Scope, named here rather than silently: no money gate (S1 has no
//! `credits`/spend-ceiling), no interject-into-a-still-running-turn branch
//! (`RunManager` has no `interject` - S2+), no attachments (per the
//! ticket). All three exist in the TS source inside this same line range.

use std::convert::Infallible;

use axum::extract::{Path, State};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures::Stream;
use model::ladder::{Trigger, default_model};
use serde::Deserialize;
use serde_json::json;
use store::Db;

use super::parse_body;
use crate::AppError;
use crate::AppState;
use crate::prompt::{self, HistoryTurn};
use crate::runs::{RunEvent, StartOptions};

pub fn router() -> Router<AppState> {
    Router::new().route("/api/bots/{id}/messages", post(post_message))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct MessageBody {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
}

/// Resolves "jason", "Jason", or a bare bot id to a bot. Port of the TS
/// `findBot`'s name/id matching (the multi-user `scope` half of that
/// function does not apply here - S1 has no scope concept yet).
fn find_bot(db: &Db, name_or_id: &str) -> Option<shared::Bot> {
    let needle = name_or_id.trim().to_lowercase();
    if needle.is_empty() {
        return None;
    }
    let all = store::list_bots(db).ok()?;
    if let Some(direct) = all.iter().find(|b| b.id == needle)
        && !direct.archived
    {
        return Some(direct.clone());
    }
    all.into_iter().find(|b| b.name.to_lowercase() == needle)
}

fn history_turns(db: &Db, conversation_id: &str) -> rusqlite::Result<Vec<HistoryTurn>> {
    Ok(store::list_messages(db, conversation_id)?
        .into_iter()
        .map(|m| HistoryTurn {
            role: m.role,
            content: m.content,
        })
        .collect())
}

async fn post_message(
    State(state): State<AppState>,
    Path(bot_id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, AppError> {
    let parsed: MessageBody = parse_body(&body)?;
    let text = parsed.text.unwrap_or_default().trim().to_string();

    let bot = {
        let db = state.db();
        store::get_bot(&db, &bot_id)?
    };
    let Some(bot) = bot else {
        return Ok((
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({"error": "no such bot"})),
        )
            .into_response());
    };

    if text.is_empty() {
        return Ok((
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error": "text is required"})),
        )
            .into_response());
    }

    // 🔴 `@someone` sends this turn to that bot instead, in the same
    // thread; `@everyone` inside a room wakes every member and forbids
    // silence. See `shared::mentions` and `prompt::room_instruction`'s doc.
    let mentions = shared::mentions::find_mentions(&text);
    let everyone = mentions.names.iter().any(|name| name == "everyone");

    let (conversation_id, speaker, round, model, messages) = {
        let db = state.db();

        let conversation_id = match parsed.thread_id.filter(|t| !t.is_empty()) {
            Some(id) => id,
            None => {
                let threads = store::list_threads(&db, &bot.id)?;
                match threads.first() {
                    Some(t) => t.id.clone(),
                    None => store::get_or_create_conversation(&db, &bot.id)?,
                }
            }
        };

        // A name that does not resolve is IGNORED, deliberately, and the
        // owner answers as usual - see the TS doc on `mentioned`.
        let mentioned: Vec<shared::Bot> = mentions
            .names
            .iter()
            .filter_map(|name| find_bot(&db, name))
            .filter(|found| found.id != bot.id)
            .collect();
        let speaker = mentioned.first().cloned().unwrap_or_else(|| bot.clone());

        store::append_message(
            &db,
            &conversation_id,
            "user",
            &text,
            store::NewMessage::default(),
        )?;

        // H12: a room's round. A `room` thread with no resolvable
        // `@mention` hands the turn to every member in order; `@everyone`
        // overrides the narrowing the same way no mention at all does.
        let conversation = store::get_conversation(&db, &conversation_id)?;
        let round: Vec<String> = match &conversation {
            Some(c) if c.kind == "room" && (mentioned.is_empty() || everyone) => {
                let mut ids = vec![c.bot_id.clone()];
                ids.extend(c.members.clone());
                ids
            }
            _ => Vec::new(),
        };

        store::touch_thread(&db, &conversation_id)?;
        store::title_from_first_message(&db, &conversation_id)?;

        let history = history_turns(&db, &conversation_id)?;
        let pinned = speaker.model.clone().unwrap_or_else(|| default_model(&db));

        let messages = if round.is_empty() {
            prompt::build_prompt(&db, &speaker, &history)
        } else {
            let instruction = prompt::room_instruction(&db, &round, &speaker.id, everyone);
            prompt::with_room_instruction(
                prompt::build_prompt(&db, &speaker, &history),
                &instruction,
            )
        };

        (conversation_id, speaker, round, pinned, messages)
    };

    let run_id = state.runs.start(StartOptions {
        bot_id: speaker.id.clone(),
        conversation_id: conversation_id.clone(),
        model,
        messages,
        trigger: Trigger::Chat,
        // Only a real ROUND (more than the owner alone) needs to chain and
        // costs N times one answer - a narrowed `@mention` inside a room
        // leaves `round` empty and is priced like any other chat turn.
        room: !round.is_empty(),
    });

    if !round.is_empty() {
        state.room_engine.register(&run_id, round, everyone);
    }

    // The stream SUBSCRIBES to the run. It does not drive it, so closing
    // the tab costs nothing.
    let rx = state.runs.subscribe(&run_id);
    Ok(Sse::new(run_stream(run_id, rx)).into_response())
}

fn run_event_json(event: &RunEvent) -> serde_json::Value {
    match event {
        RunEvent::Delta { text } => json!({"type": "delta", "text": text}),
        RunEvent::ToolCall { name, args } => {
            json!({"type": "tool_call", "name": name, "args": args})
        }
        RunEvent::ToolResult { name, result } => {
            json!({"type": "tool_result", "name": name, "result": result})
        }
        RunEvent::Done { model, message_id } => {
            json!({"type": "done", "model": model, "messageId": message_id})
        }
        RunEvent::Error { message, status } => {
            let mut value = json!({"type": "error", "message": message});
            if let Some(status) = status {
                value["status"] = json!(status);
            }
            value
        }
    }
}

fn run_stream(
    run_id: String,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<RunEvent>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    async_stream::stream! {
        yield Ok(Event::default().data(json!({"type": "run", "runId": run_id}).to_string()));
        while let Some(event) = rx.recv().await {
            let done = matches!(event, RunEvent::Done { .. } | RunEvent::Error { .. });
            yield Ok(Event::default().data(run_event_json(&event).to_string()));
            if done {
                break;
            }
        }
    }
}
