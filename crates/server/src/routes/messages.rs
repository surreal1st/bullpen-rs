//! `POST /api/bots/:id/messages`: send a message, start a run (or hand the
//! turn to a room's round), and stream the run back. Port of
//! `src/server/app.ts:1463-1656`.
//!
//! 🔴 Scope, named here rather than silently: no attachments (per the
//! ticket) - that still exists in the TS source inside this same line
//! range. S2-04 adds the interject branch: an already-`running` run in
//! this thread gets a new message handed to it directly instead of racing
//! it with a second run.

use std::convert::Infallible;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures::Stream;
use model::ladder::{Trigger, default_model};
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::json;
use store::Db;

use super::parse_body;
use crate::AppError;
use crate::AppState;
use crate::prompt::{self, HistoryTurn};
use crate::runs::{RunEvent, StartOptions};
use crate::spend;

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

/// Resolves "jason", "Jason", a bare bot id, a purpose ("Chief of Staff"),
/// or an unambiguous name prefix to a bot. Port of the TS `findBot`
/// (`delegate.ts:46-67`; the multi-user `scope` half of that function does
/// not apply here - S1 has no scope concept yet).
///
/// F16: the id arm compares case-sensitively against the RAW (trimmed but
/// not lowercased) input - TS lowercases the needle before every arm,
/// including the id one, which makes any bot whose id itself has an
/// uppercase letter unreachable by id no matter what a caller types; the
/// other three arms stay case-insensitive, same as TS.
fn find_bot(db: &Db, name_or_id: &str) -> Option<shared::Bot> {
    let raw = name_or_id.trim();
    if raw.is_empty() {
        return None;
    }
    let needle = raw.to_lowercase();
    let all = store::list_bots(db, false).ok()?;

    if let Some(direct) = all.iter().find(|b| b.id == raw)
        && !direct.archived
    {
        return Some(direct.clone());
    }
    if let Some(by_name) = all.iter().find(|b| b.name.to_lowercase() == needle) {
        return Some(by_name.clone());
    }
    if let Some(by_purpose) = all.iter().find(|b| b.purpose.to_lowercase() == needle) {
        return Some(by_purpose.clone());
    }
    // A unique prefix match only - two bots both starting with "j" must
    // still be named exactly, same as TS's `partial.length === 1` guard.
    let mut prefix_matches = all
        .into_iter()
        .filter(|b| b.name.to_lowercase().starts_with(&needle));
    let first = prefix_matches.next()?;
    if prefix_matches.next().is_none() {
        Some(first)
    } else {
        None
    }
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
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "text is required"})),
        )
            .into_response());
    }

    // S2-05/S2-F-04: ceiling gate, reading the account's real spend now
    // (F3/D6: this used to hardcode `account_usage = None`, whose arm
    // always allows - the 402 branch below was dead code). Checked BEFORE
    // the run starts and never during: stopping an answer halfway wastes
    // what was already spent and loses the reply. The gate only refuses
    // to START. `db` is never held across the credits `.await` - see
    // `AppState::db`'s doc on why a guard held across an await would not
    // compile inside a spawned task, and the same reasoning applies here
    // to any future caller of this handler under a runtime that cares.
    let ceiling = {
        let db = state.db();
        spend::get_ceiling(&db)
    };
    let account_usage = state.credits.total_usage().await.ok();
    let gate_check = {
        let db = state.db();
        spend::gate_run(&db, ceiling, account_usage)
    };
    // Allowed carries a warning (near the ceiling, or the balance could
    // not be read) that has to reach the run as its first event - see
    // `RunManager::start_with_notice`'s doc.
    let starting_notice = match gate_check {
        spend::GateResult::Denied { reason } => {
            return Ok((
                StatusCode::PAYMENT_REQUIRED,
                Json(json!({"error": reason, "kind": "spend-ceiling"})),
            )
                .into_response());
        }
        spend::GateResult::Allowed { warning } => warning,
    };

    // 🔴 `@someone` sends this turn to that bot instead, in the same
    // thread; `@everyone` inside a room wakes every member and forbids
    // silence. See `shared::mentions` and `prompt::room_instruction`'s doc.
    let mentions = shared::mentions::find_mentions(&text);
    let everyone = mentions.names.iter().any(|name| name == "everyone");

    let (conversation_id, speaker, round, model, messages, active_run) = {
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

        // B14: `threadId` came straight off the request body - unchecked, a
        // caller could POST to bot A with bot B's thread id and write a
        // user message (plus a run) into B's conversation. A thread belongs
        // to a bot when that bot owns it outright, or when it is a room the
        // bot is a member of; anything else is a 404 - the same answer a
        // thread that does not exist at all gets, so a probe cannot
        // distinguish "wrong owner" from "no such thread". Loaded once here
        // and reused below for the round check, rather than queried twice.
        let conversation = store::get_conversation(&db, &conversation_id)?;
        let owns_thread = matches!(
            &conversation,
            Some(c) if c.bot_id == bot.id || c.members.iter().any(|m| m == &bot.id)
        );
        if !owns_thread {
            return Ok((
                axum::http::StatusCode::NOT_FOUND,
                Json(json!({"error": "no such thread"})),
            )
                .into_response());
        }

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

        // S2-04: an already-`running` run in this thread gets this message
        // handed to it directly, instead of racing it with a second run -
        // port of the TS `activeRun`/`interject` check (`app.ts:1535-1551`).
        // Only a `running` row qualifies: `waiting` is parked on an
        // approval and must not be touched here, so it falls through to
        // starting an ordinary new run below and the approval stays
        // exactly where it was. No attachments here (see this file's
        // doc), so there is no TS suffix to add to the text. Read out of
        // this block (rather than acted on here) so `state.runs.interject`
        // - which takes its own lock on this same `Db` - never runs while
        // this `db` guard is still held.
        let active_run: Option<String> = db
            .conn()
            .query_row(
                "SELECT id FROM runs WHERE conversation_id = ?1 AND status = 'running' \
                 ORDER BY created_at DESC LIMIT 1",
                rusqlite::params![conversation_id],
                |row| row.get(0),
            )
            .optional()?;

        // H12: a room's round. A `room` thread with no resolvable
        // `@mention` hands the turn to every member in order; `@everyone`
        // overrides the narrowing the same way no mention at all does.
        // `conversation` is the same row the B14 check above already loaded.
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

        (
            conversation_id,
            speaker,
            round,
            pinned,
            messages,
            active_run,
        )
    };

    // S2-04: hand off to the run already in flight, if any, rather than
    // starting a second one - see the doc above. `interject` re-checks the
    // run is still `running` itself (the race the TS doc names: a run that
    // finished between the query above and this call), so a `false` here
    // falls straight through to starting an ordinary new run instead of
    // vanishing.
    if let Some(run_id) = active_run
        && state.runs.interject(&run_id, &text)
    {
        return Ok((
            StatusCode::OK,
            Json(json!({"interjected": true, "runId": run_id})),
        )
            .into_response());
    }

    // B9: registered (keyed by `conversation_id` - see `RoomEngine::register`'s
    // doc) BEFORE `runs.start`, so a run that settles before `start` even
    // returns (an instant `ModelEvent::Error` - no key configured, for one)
    // cannot fire `on_run_done` before this entry exists to chain past the
    // owner's leg.
    let is_room_round = !round.is_empty();
    if is_room_round {
        state
            .room_engine
            .register(&conversation_id, round, everyone);
    }

    let run_id = state.runs.start_with_notice(
        StartOptions {
            bot_id: speaker.id.clone(),
            conversation_id: conversation_id.clone(),
            model,
            messages,
            trigger: Trigger::Chat,
            // Only a real ROUND (more than the owner alone) needs to chain
            // and costs N times one answer - a narrowed `@mention` inside a
            // room leaves `round` empty and is priced like any other chat
            // turn.
            room: is_room_round,
        },
        starting_notice,
    );

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
        RunEvent::ApprovalNeeded {
            approval_id,
            name,
            args,
        } => {
            json!({"type": "approval_needed", "approvalId": approval_id, "name": name, "args": args})
        }
        RunEvent::Notice { message } => json!({"type": "notice", "message": message}),
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
