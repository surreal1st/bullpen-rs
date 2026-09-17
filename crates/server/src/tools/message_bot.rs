//! `message_bot`: ask a colleague a question, or post into a group chat by
//! its title. Port of `src/server/delegate.ts`'s `askBot` (the bot branch)
//! and `app.ts:6060-6073`'s room-title match (the room branch).
//!
//! 🔴 S1 simplification, named here rather than silently: the bot branch is
//! ONE model call with no tools and no further nested `message_bot` - not
//! `askBot`'s full `runTurn` with a 3-step tool loop and its own permission
//! gate. Approvals, escalation and per-depth toolboxes are S2+ (per the
//! ticket's scope note), and a nested tool loop here would let
//! `message_bot` call itself recursively with nothing bounding the depth.
//! Asking a colleague a quick question - the common case - only ever needed
//! one model call anyway.

use std::sync::Arc;

use futures::StreamExt;
use model::ladder::{Trigger, default_model, model_for_run};
use model::{ModelEvent, ModelPort, ModelRequest, ModelUsage, ToolSpec};
use serde::Deserialize;
use serde_json::json;
use store::{Db, NewMessage};

use crate::prompt::{self, HistoryTurn};
use crate::tools::{RoomHook, fence_tool_output, lock_db};

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "message_bot".to_string(),
        description: "Ask another bot on the roster something and use its answer, or name a \
group chat instead of a bot to post there so every member sees it and can weigh in. Use it \
when the question is squarely someone else's area, not to avoid thinking."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "bot": { "type": "string", "description": "The bot's name (such as Jason) or a group chat's title." },
                "question": { "type": "string", "description": "What to ask, in full. It has no idea what you are working on." }
            },
            "required": ["bot", "question"]
        }),
    }
}

#[derive(Deserialize)]
struct Args {
    bot: String,
    question: String,
}

/// F3: the second element is the delegated model's own spend, when the bot
/// branch actually reached a model - `None` for the room branch and for
/// every early return, so `runs.rs`'s tool loop has nothing to add for
/// those. F2: `trigger`/`room` are the CALLER's, threaded down from
/// `tools::build` so the colleague's call is floored the same way the
/// caller's own would be.
pub async fn run(
    db: &Arc<std::sync::Mutex<Db>>,
    port: &Arc<dyn ModelPort>,
    caller_bot_id: &str,
    room_hook: &RoomHook,
    trigger: Trigger,
    room: bool,
    args: &str,
) -> (String, Option<ModelUsage>) {
    let Ok(parsed) = serde_json::from_str::<Args>(args) else {
        return ("Could not read `bot`/`question`.".to_string(), None);
    };
    let to = parsed.bot.trim();
    let question = parsed.question.trim();
    if question.is_empty() {
        return (
            "Nothing was asked: the question was empty.".to_string(),
            None,
        );
    }

    // 1. A room, matched by title.
    let room_match = {
        let db = lock_db(db);
        store::list_rooms(&db)
            .expect("list_rooms")
            .into_iter()
            .find(|r| r.title.eq_ignore_ascii_case(to))
    };
    if let Some(room_summary) = room_match {
        let caller_name = {
            let db = lock_db(db);
            store::get_bot(&db, caller_bot_id)
                .expect("get_bot")
                .map(|b| b.name)
        };
        {
            let db = lock_db(db);
            // Guest-attributed unless the caller is the room's own database
            // owner - the same rule an @mention reply uses.
            let owner_is_caller =
                room_summary.member_ids.first().map(String::as_str) == Some(caller_bot_id);
            store::append_message(
                &db,
                &room_summary.id,
                "assistant",
                question,
                NewMessage {
                    bot_id: if owner_is_caller {
                        None
                    } else {
                        Some(caller_bot_id.to_string())
                    },
                    ..Default::default()
                },
            )
            .expect("append message_bot room message");
        }
        // B6: the hook itself refuses (and starts nothing) when this room
        // already has a round in flight - see `RoomEngine::start_room_turn`'s
        // doc. Without this, a member woken by its own room's round could
        // `message_bot` right back into that same room and start a second,
        // overlapping round every lap - two bots naming each other's room
        // would then page each other forever, N model calls a lap, against
        // Josh's $60/month ceiling with nothing to stop it. The message
        // itself still posts either way; only waking a NEW round is refused.
        let awakened = room_hook
            .lock()
            .expect("room hook mutex poisoned")
            .as_ref()
            .is_some_and(|hook| hook(&room_summary.id, false));
        let said_by = caller_name.unwrap_or_else(|| "You".to_string());
        if !awakened {
            return (
                format!(
                    "The {} room already has a round in progress.",
                    room_summary.title
                ),
                None,
            );
        }
        // S8b-06 Decision: not fenced. `question` here is an ECHO of what
        // the CALLING bot itself just typed as this same tool call's own
        // argument, not a second party's output - the model already holds
        // every byte of it in its own context before this call returns.
        // Confirming what you just said back to you introduces nothing new
        // to launder, unlike the bot-reply branch below where a DIFFERENT
        // model's text arrives for the first time.
        return (
            format!(
                "Posted to the {} room. {said_by} said: {question}",
                room_summary.title
            ),
            None,
        );
    }

    // 2. A bot, matched by id or name.
    let bots = {
        let db = lock_db(db);
        store::list_bots(&db).expect("list_bots")
    };
    let target = bots
        .iter()
        .find(|b| b.id == to || b.name.eq_ignore_ascii_case(to))
        .cloned();
    // F10: an unresolvable name hands back the whole roster so the model can
    // retry with a real one, instead of a dead end it can only apologise for.
    let Some(bot) = target else {
        let roster = bots
            .iter()
            .map(|b| b.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return (
            format!("There is no bot called that. The roster is: {roster}"),
            None,
        );
    };
    // F10: a bot asking itself is a paid model call for nothing - it already
    // has the answer.
    if bot.id == caller_bot_id {
        return ("That is you. Answer it yourself.".to_string(), None);
    }

    let framed = format!(
        "A colleague is asking you this, on Josh's behalf:\n\n{question}\n\nAnswer as yourself, \
briefly. If it is not your area, say whose it is rather than guessing."
    );
    // F2: the colleague's OWN pin does not get to opt out of the caller's
    // floor - `model_for_run` is the only place a run's model is settled,
    // same as the TS `askBot` (`delegate.ts:105`).
    let request_model = {
        let db_guard = lock_db(db);
        let raw = bot
            .model
            .clone()
            .unwrap_or_else(|| default_model(&db_guard));
        model_for_run(&db_guard, trigger, &raw, room)
    };
    let messages = {
        let db = lock_db(db);
        prompt::build_prompt(&db, &bot, &[HistoryTurn::user(framed)])
    };

    let mut stream = port.stream(ModelRequest {
        model: request_model,
        messages,
        ..Default::default()
    });
    let mut text = String::new();
    let mut usage: Option<ModelUsage> = None;
    while let Some(event) = stream.next().await {
        match event {
            ModelEvent::Delta { text: chunk } => text.push_str(&chunk),
            ModelEvent::Done { usage: u, .. } => {
                usage = u;
                break;
            }
            // No nested tool loop here - see the module doc.
            ModelEvent::ToolCalls { usage: u, .. } => {
                usage = u;
                break;
            }
            ModelEvent::Error { message, .. } => {
                return (format!("{} could not answer: {message}", bot.name), usage);
            }
        }
    }

    if text.trim().is_empty() {
        (format!("{} had nothing to say.", bot.name), usage)
    } else {
        // F5(b): `text` is a SECOND model's own generated output - the
        // colleague answered from its own context, which may itself hold a
        // page it just browsed (fenced to IT, not to the caller). Nothing
        // marks that answer as data once it lands here, so it would read to
        // the calling model as ordinary trusted narration - the exact relay
        // this ticket exists to close. `bot.name` stays OUTSIDE the fence:
        // it is server-generated (looked up by id/name against the roster,
        // never chosen by either model) and is how the caller knows who
        // answered; only the reply body the colleague actually wrote is
        // untrusted.
        (
            format!("{}: {}", bot.name, fence_tool_output(text.trim())),
            usage,
        )
    }
}
