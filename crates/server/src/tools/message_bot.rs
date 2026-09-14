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

use std::sync::{Arc, Mutex};

use futures::StreamExt;
use model::{CHEAP_DEFAULT_MODEL, ModelEvent, ModelPort, ModelRequest, ToolSpec};
use serde::Deserialize;
use serde_json::json;
use store::{Db, NewMessage};

use crate::prompt::{self, HistoryTurn};
use crate::tools::RoomHook;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "message_bot".to_string(),
        description: "Ask another bot a question, or post to a group chat by its title. \
Given a bot's id or name: runs a short nested turn and returns its reply. Given a room's \
title: posts your message there for the room to see."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "to": { "type": "string", "description": "a bot's id/name, or a room's title" },
                "message": { "type": "string" }
            },
            "required": ["to", "message"]
        }),
    }
}

#[derive(Deserialize)]
struct Args {
    to: String,
    message: String,
}

pub async fn run(
    db: &Arc<Mutex<Db>>,
    port: &Arc<dyn ModelPort>,
    caller_bot_id: &str,
    room_hook: &RoomHook,
    args: &str,
) -> String {
    let Ok(parsed) = serde_json::from_str::<Args>(args) else {
        return "Could not read `to`/`message`.".to_string();
    };
    let to = parsed.to.trim();
    let message = parsed.message.trim();
    if message.is_empty() {
        return "Nothing was said: the message was empty.".to_string();
    }

    // 1. A room, matched by title.
    let room_match = {
        let db = db.lock().expect("db mutex poisoned");
        store::list_rooms(&db)
            .expect("list_rooms")
            .into_iter()
            .find(|r| r.title.eq_ignore_ascii_case(to))
    };
    if let Some(room) = room_match {
        let caller_name = {
            let db = db.lock().expect("db mutex poisoned");
            store::get_bot(&db, caller_bot_id)
                .expect("get_bot")
                .map(|b| b.name)
        };
        {
            let db = db.lock().expect("db mutex poisoned");
            // Guest-attributed unless the caller is the room's own database
            // owner - the same rule an @mention reply uses.
            let owner_is_caller =
                room.member_ids.first().map(String::as_str) == Some(caller_bot_id);
            store::append_message(
                &db,
                &room.id,
                "assistant",
                message,
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
        if let Some(hook) = room_hook.lock().expect("room hook mutex poisoned").as_ref() {
            hook(&room.id, false);
        }
        let said_by = caller_name.unwrap_or_else(|| "You".to_string());
        return format!(
            "Posted to the {} room. {said_by} said: {message}",
            room.title
        );
    }

    // 2. A bot, matched by id or name.
    let target = {
        let db = db.lock().expect("db mutex poisoned");
        store::list_bots(&db)
            .expect("list_bots")
            .into_iter()
            .find(|b| b.id == to || b.name.eq_ignore_ascii_case(to))
    };
    let Some(bot) = target else {
        return format!("No such bot or room: {to}");
    };

    let framed = format!(
        "A colleague is asking you this, on Josh's behalf:\n\n{message}\n\nAnswer as yourself, \
briefly. If it is not your area, say whose it is rather than guessing."
    );
    let request_model = bot
        .model
        .clone()
        .unwrap_or_else(|| CHEAP_DEFAULT_MODEL.to_string());
    let messages = {
        let db = db.lock().expect("db mutex poisoned");
        prompt::build_prompt(&db, &bot, &[HistoryTurn::user(framed)])
    };

    let mut stream = port.stream(ModelRequest {
        model: request_model,
        messages,
        ..Default::default()
    });
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event {
            ModelEvent::Delta { text: chunk } => text.push_str(&chunk),
            ModelEvent::Done { .. } => break,
            // No nested tool loop here - see the module doc.
            ModelEvent::ToolCalls { .. } => break,
            ModelEvent::Error { message, .. } => {
                return format!("{} could not answer: {message}", bot.name);
            }
        }
    }

    if text.trim().is_empty() {
        format!("{} had nothing to say.", bot.name)
    } else {
        format!("{}: {}", bot.name, text.trim())
    }
}
