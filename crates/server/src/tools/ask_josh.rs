//! `ask_josh`: post a question (with optional multiple-choice options) into
//! your own 1:1 thread. Port of `src/server/conversing.ts`'s `askAsync` -
//! the non-waiting form. S2: writes a `questions` row and posts the message.

use std::sync::Arc;

use model::ToolSpec;
use serde_json::json;
use shared::ask_josh::{MAX_QUESTION_CHARS, parse_ask_josh};
use store::{Db, NewMessage};

use super::lock_db;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "ask_josh".to_string(),
        description: "Ask Josh a question in your own 1:1 thread, with options if that helps. \
Does not wait for an answer - carry on with other work if you have any, or stop here if you \
need his answer to continue."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "question": { "type": "string" },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "optional multiple-choice options"
                },
                "wait": {
                    "type": "boolean",
                    "description": "if true, park the run and wait for Josh to answer"
                }
            },
            "required": ["question"]
        }),
    }
}

pub fn run(
    db: &Arc<std::sync::Mutex<Db>>,
    bot_id: &str,
    args: &str,
    changes: crate::changes::ChangeBus,
) -> String {
    let parsed = parse_ask_josh(args);
    let question = parsed.question.trim();
    if question.is_empty() {
        return "Nothing was asked: the question was empty.".to_string();
    }

    let mut lines = vec![
        question
            .chars()
            .take(MAX_QUESTION_CHARS)
            .collect::<String>(),
    ];
    if !parsed.options.is_empty() {
        lines.push(String::new());
        for option in &parsed.options {
            lines.push(format!("- {option}"));
        }
    }
    let content = lines.join("\n");

    let db = lock_db(db);
    let conversation_id =
        store::get_or_create_conversation(&db, bot_id).expect("get_or_create_conversation");

    let message = store::append_message(
        &db,
        &conversation_id,
        "assistant",
        &content,
        NewMessage {
            bot_id: Some(bot_id.to_string()),
            ..Default::default()
        },
    )
    .expect("append ask_josh message");

    // Write the questions row (S2-08).
    if let Err(e) = store::insert_question(
        &db,
        bot_id,
        &conversation_id,
        Some(&message.id),
        question,
        &parsed.options,
    ) {
        eprintln!("Failed to insert question row: {e}");
    } else {
        // Touch the change bus after successfully writing the row
        changes.touch(crate::changes::ChangeKind::Questions);
        changes.touch(crate::changes::ChangeKind::Roster);
    }

    "Asked. Josh has the question and has NOT answered it yet. Do not wait and do not guess the answer. \
Carry on with everything that does not depend on it, and say plainly which part is waiting on his answer."
        .to_string()
}
