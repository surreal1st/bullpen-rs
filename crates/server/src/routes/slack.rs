//! S5c-03: `/api/slack*` (session-gated status/connect/disconnect/answer-bot)
//! plus the unauthenticated `POST /api/slack/events` Events API delivery
//! route. Port of `app.ts:2570-2724` (bullpen-night) and the DM/mention
//! chat-surface half of that route.
//!
//! `POST /api/slack/events` is open via `auth::OPEN_PATHS` - Slack's own
//! delivery carries no session cookie, so it is authenticated a different
//! way: `verify_slack_signature` over the RAW body, checked before anything
//! is parsed, same discipline `routes/hooks.rs::post_webhook` uses for
//! per-routine webhooks (raw-body capture, a 64 KB cap, signature-before-
//! parse). Unlike that route, Slack signs ONE app-wide endpoint rather than
//! a URL per routine, so this reads `get_slack_config` first to find the
//! secret to verify against, and a Slack event can fan out to BOTH the chat
//! surface (a DM/mention starts a run and replies in-thread) and any number
//! of `hook_kind: "slack"` routines in one delivery.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::Utc;
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, PoisonError};
use store::Db;

use super::parse_body;
use crate::hooks;
use crate::prompt::{self, HistoryTurn};
use crate::runs::StartOptions;
use crate::slack::{self, SlackConnectInput};
use crate::{ApiResult, AppError, AppState, PendingSlackReply};
use model::ladder::{Trigger, default_model};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/slack",
            get(get_slack).put(put_slack).delete(delete_slack),
        )
        .route("/api/slack/answer-bot", put(put_slack_answer_bot))
        .route("/api/slack/events", post(post_slack_events))
}

async fn get_slack(State(state): State<AppState>) -> ApiResult<Response> {
    let db = state.db();
    Ok(Json(slack::slack_status(&db)).into_response())
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ConnectBody {
    #[serde(default)]
    bot_token: String,
    #[serde(default)]
    signing_secret: String,
    #[serde(default)]
    app_token: Option<String>,
}

/// Settings > Computer's Slack card. Proves the bot token by calling
/// `auth.test` before anything is stored - see `slack::connect_slack` - so a
/// typo'd token never sits in Settings looking connected.
///
/// `connect_slack` holds its `&Db` argument live across its own internal
/// `.await` (the `auth.test` call happens before its `put_setting`s), which
/// makes its generated future `!Send` - `Db` wraps a `rusqlite::Connection`
/// (`Send` but not `Sync`), so a reference to it is never `Send` either, and
/// axum requires a handler's future to BE `Send`. `.await`ing it directly
/// here does not compile (proved against a standalone probe before this
/// route was written - see this ticket's Results). Instead: move the raw
/// `Arc<Mutex<Db>>` handle (`AppState::db_arc`, itself `Send + Sync`
/// regardless of `Db`'s own `Sync`-ness) into a `tokio::task::
/// spawn_blocking` closure, lock it THERE, and drive `connect_slack` to
/// completion with `futures::executor::block_on` - entirely on one
/// blocking-pool thread, so the `!Send` future never has to cross a thread
/// boundary as a value the way an `.await` on it directly would require.
async fn put_slack(State(state): State<AppState>, body: axum::body::Bytes) -> ApiResult<Response> {
    let parsed: ConnectBody = parse_body(&body)?;
    let db_arc = state.db_arc();
    let api = Arc::clone(&state.slack_api);

    let result = tokio::task::spawn_blocking(move || {
        let guard = db_arc.lock().unwrap_or_else(PoisonError::into_inner);
        futures::executor::block_on(slack::connect_slack(
            &guard,
            SlackConnectInput {
                bot_token: parsed.bot_token,
                signing_secret: parsed.signing_secret,
                app_token: parsed.app_token,
            },
            api.as_ref(),
        ))
    })
    .await
    .map_err(|_| AppError::from("slack connect task panicked".to_string()))?;

    Ok(match result {
        Ok(status) => Json(status).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response(),
    })
}

async fn delete_slack(State(state): State<AppState>) -> ApiResult<Response> {
    let db = state.db();
    slack::disconnect_slack(&db)?;
    Ok(Json(json!({"ok": true})).into_response())
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct AnswerBotBody {
    #[serde(default)]
    bot_id: String,
}

/// "Which bot answers in Slack" - the setting a DM or mention falls back to.
async fn put_slack_answer_bot(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> ApiResult<Response> {
    let parsed: AnswerBotBody = parse_body(&body)?;
    let db = state.db();
    Ok(if slack::set_slack_answer_bot_id(&db, &parsed.bot_id)? {
        Json(slack::slack_status(&db)).into_response()
    } else {
        (StatusCode::NOT_FOUND, Json(json!({"error": "no such bot"}))).into_response()
    })
}

const HOOK_BODY_MAX_BYTES: usize = 64 * 1024;

fn as_str(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

/// Case-insensitive regex test, failing OPEN - a near-duplicate of the
/// private `regex_matches_or_fails_open` in `routes/hooks.rs` rather than a
/// call into it: that function is not `pub(crate)` and this ticket's ONE
/// allowed visibility change in that file is `fire_webhook_routine`, not
/// this helper too. Same reasoning `fire_webhook_routine`'s own doc gives
/// for its own near-duplicate.
fn regex_matches_or_fails_open(pattern: &str, text: &str) -> bool {
    RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .map(|re| re.is_match(text))
        .unwrap_or(true)
}

/// A near-duplicate of the private `history_turns` in `routes/messages.rs`
/// (not reachable from here: private to that module) - the DM/mention chat
/// branch below needs the same "every stored message, oldest first, role +
/// content only" shape `buildPrompt(db, bot, listMessages(...))` reads in
/// the TS original (`app.ts:2685`).
fn history_turns(db: &Db, conversation_id: &str) -> rusqlite::Result<Vec<HistoryTurn>> {
    Ok(store::list_messages(db, conversation_id)?
        .into_iter()
        .map(|m| HistoryTurn {
            role: m.role,
            content: m.content,
        })
        .collect())
}

/// The Events API. One app-wide endpoint - unlike the per-routine webhooks
/// in `routes/hooks.rs`, Slack has no notion of "this URL is for routine X";
/// every event subscribed to arrives here and this handler decides what it
/// is for. Signature verified over the raw body BEFORE anything is parsed,
/// and (uniquely to Slack's scheme) the signature also carries a timestamp,
/// so a captured request cannot be replayed outside
/// `hooks::SLACK_REPLAY_WINDOW_SECONDS` even with a valid HMAC. Port of
/// `app.ts:2618-2724`.
async fn post_slack_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let config = {
        let db = state.db();
        slack::get_slack_config(&db)
    };
    let Some(config) = config else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Slack is not configured"})),
        )
            .into_response();
    };

    if body.len() > HOOK_BODY_MAX_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({"error": "body too large"})),
        )
            .into_response();
    }
    let raw_body = String::from_utf8_lossy(&body).to_string();

    let ts_header = headers
        .get("x-slack-request-timestamp")
        .and_then(|v| v.to_str().ok());
    let sig_header = headers
        .get("x-slack-signature")
        .and_then(|v| v.to_str().ok());
    let now = Utc::now().timestamp().max(0) as u64;
    if !hooks::verify_slack_signature(
        &config.signing_secret,
        &raw_body,
        ts_header,
        sig_header,
        now,
    ) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "not authorized"})),
        )
            .into_response();
    }

    let payload: Value = if raw_body.is_empty() {
        json!({})
    } else {
        match serde_json::from_str(&raw_body) {
            Ok(v) => v,
            Err(_) => {
                return (StatusCode::BAD_REQUEST, Json(json!({"error": "bad json"})))
                    .into_response();
            }
        }
    };

    // Slack's own handshake when the request URL is first saved: echo the
    // challenge back, verbatim, with no signature exemption - it was still
    // checked above like every other delivery.
    if payload.get("type").and_then(|v| v.as_str()) == Some("url_verification") {
        let challenge = payload
            .get("challenge")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return Json(json!({"challenge": challenge})).into_response();
    }

    let is_event_callback = payload.get("type").and_then(|v| v.as_str()) == Some("event_callback");
    let event = payload.get("event").cloned();
    let (Some(event), true) = (event, is_event_callback) else {
        return StatusCode::NO_CONTENT.into_response();
    };
    if !event.is_object() {
        return StatusCode::NO_CONTENT.into_response();
    }

    // Never react to Bullpen's own reply, or any other app's message - the
    // loop this would otherwise be is the first thing to get wrong here.
    if !as_str(&event["bot_id"]).is_empty()
        || event.get("subtype").and_then(|v| v.as_str()) == Some("bot_message")
    {
        return StatusCode::NO_CONTENT.into_response();
    }

    let event_type = as_str(&event["type"]);
    let text = as_str(&event["text"]);
    let bot_user_id = config.bot_user_id.clone().unwrap_or_default();
    let is_mention = hooks::mentions_slack_user(&text, &bot_user_id);
    let is_dm = event_type == "message" && as_str(&event["channel_type"]) == "im";
    let is_app_mention = event_type == "app_mention";

    // (2) The chat surface: a DM or an @mention runs the addressed bot and
    // replies in the same channel/thread - see `AppState::build`'s
    // `add_on_run_done` reply-posting hook for the reply half.
    if is_dm || is_app_mention {
        let answer_bot_id = {
            let db = state.db();
            slack::get_slack_answer_bot_id(&db)
        };
        let bot = {
            let db = state.db();
            answer_bot_id.and_then(|id| store::get_bot(&db, &id).ok().flatten())
        };
        let channel = as_str(&event["channel"]);
        let thread_ts = {
            let explicit = as_str(&event["thread_ts"]);
            if explicit.is_empty() {
                as_str(&event["ts"])
            } else {
                explicit
            }
        };
        let spoken_text = if bot_user_id.is_empty() {
            text.trim().to_string()
        } else {
            text.replace(&format!("<@{bot_user_id}>"), "")
                .trim()
                .to_string()
        };

        if let Some(bot) = bot
            && !channel.is_empty()
            && !thread_ts.is_empty()
            && !spoken_text.is_empty()
        {
            // B2 discipline (same as `routes/hooks.rs::fire_webhook_routine`'s
            // own doc): everything db-touching happens inside this block, and
            // the guard is DROPPED before `state.runs.start` below, which
            // locks this SAME `Arc<Mutex<Db>>` internally to insert the run
            // row - holding this guard across that call deadlocks the
            // request on itself (proved the hard way: the first draft of
            // this route hung every test until the guard scope was fixed to
            // end here, see this ticket's Results).
            let (conversation_id, model, messages) = {
                let db = state.db();
                let conversation_id = match store::slack::get_or_create_slack_conversation(
                    &db, &bot.id, &channel, &thread_ts,
                ) {
                    Ok(id) => id,
                    Err(err) => {
                        tracing::error!("failed to get/create slack conversation: {err}");
                        return StatusCode::NO_CONTENT.into_response();
                    }
                };
                if let Err(err) = store::append_message(
                    &db,
                    &conversation_id,
                    "user",
                    &spoken_text,
                    store::NewMessage::default(),
                ) {
                    tracing::error!("failed to append slack message: {err}");
                    return StatusCode::NO_CONTENT.into_response();
                }
                let _ = store::touch_thread(&db, &conversation_id);

                let model = bot.model.clone().unwrap_or_else(|| default_model(&db));
                let history = match history_turns(&db, &conversation_id) {
                    Ok(h) => h,
                    Err(err) => {
                        tracing::error!("failed to load slack conversation history: {err}");
                        return StatusCode::NO_CONTENT.into_response();
                    }
                };
                let messages = prompt::build_prompt(&db, &bot, &history);
                (conversation_id, model, messages)
            };

            let run_id = state.runs.start(StartOptions {
                bot_id: bot.id.clone(),
                conversation_id,
                model,
                messages,
                trigger: Trigger::Webhook,
                room: false,
            });
            state.register_pending_slack_reply(
                run_id,
                PendingSlackReply {
                    bot_token: config.bot_token.clone(),
                    channel,
                    thread_ts,
                },
            );
        }
    }

    // (3) Slack as a routine trigger. Every event is offered to every active
    // `hook_kind: "slack"` routine - see `store::slack::
    // slack_event_matches_trigger_kind` for what "mention" / "keyword" /
    // "message" / "reaction" each require, and `hook_match` (checked here
    // exactly as every other hook kind checks it) for the keyword itself.
    let routines = {
        let db = state.db();
        store::routines::active_routines_by_hook_kind(&db, "slack").unwrap_or_default()
    };
    for routine in routines {
        let trigger_kind = store::routines::parse_hook_events(routine.hook_events.as_deref())
            .and_then(|events| events.first().cloned())
            .unwrap_or_else(|| "message".to_string());
        if !store::slack::SLACK_TRIGGER_KINDS.contains(&trigger_kind.as_str()) {
            continue;
        }
        if !store::slack::slack_event_matches_trigger_kind(&trigger_kind, &event_type, is_mention) {
            continue;
        }

        let Some(reduced) = hooks::reduce_slack(&event) else {
            continue;
        };

        if let Some(pattern) = routine.hook_match.as_deref()
            && !pattern.is_empty()
            && !regex_matches_or_fails_open(pattern, &reduced)
        {
            continue;
        }

        let in_flight = {
            let db = state.db();
            db.conn()
                .query_row(
                    "SELECT 1 FROM runs WHERE routine_id = ?1 AND status IN ('running', 'waiting') LIMIT 1",
                    rusqlite::params![routine.id],
                    |_| Ok(()),
                )
                .is_ok()
        };
        if in_flight {
            continue;
        }

        let extra = format!("\n\n## What arrived\n\n{reduced}");
        let _ = super::hooks::fire_webhook_routine(&state, &routine, &extra);
    }

    StatusCode::NO_CONTENT.into_response()
}
