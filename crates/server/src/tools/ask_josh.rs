//! `ask_josh`: post a question (with optional multiple-choice options) into
//! your own 1:1 thread. Port of `src/server/conversing.ts`'s `askAsync` -
//! the non-waiting form only. S1 has no `questions` table and no approval
//! queue (S2+), so this does not park the run or write a row anywhere else;
//! it posts the question as a message, same as `say`, and returns.

use std::sync::{Arc, Mutex};

use model::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use store::{Db, NewMessage};

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
                }
            },
            "required": ["question"]
        }),
    }
}

#[derive(Deserialize)]
struct Args {
    question: String,
    #[serde(default)]
    options: Vec<String>,
}

pub fn run(db: &Arc<Mutex<Db>>, bot_id: &str, args: &str) -> String {
    let Ok(parsed) = serde_json::from_str::<Args>(args) else {
        return "Could not read `question`.".to_string();
    };
    let question = parsed.question.trim();
    if question.is_empty() {
        return "Nothing was asked: the question was empty.".to_string();
    }

    let mut lines = vec![question.to_string()];
    if !parsed.options.is_empty() {
        lines.push(String::new());
        for option in &parsed.options {
            lines.push(format!("- {option}"));
        }
    }
    let content = lines.join("\n");

    let db = db.lock().expect("db mutex poisoned");
    let conversation_id =
        store::get_or_create_conversation(&db, bot_id).expect("get_or_create_conversation");
    store::append_message(
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

    "Asked. Carry on with other work if you have it; Josh will answer when he can.".to_string()
}
