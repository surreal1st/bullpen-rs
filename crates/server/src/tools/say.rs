//! `say`: talk in your own 1:1 thread with Josh without ending your turn.
//! Port of `src/server/conversing.ts`'s `sayToJosh`.

use std::sync::{Arc, Mutex};

use model::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use store::{Db, NewMessage};

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "say".to_string(),
        description: "Say something in your own 1:1 thread with Josh, without ending your turn. \
Use it to report progress mid-run and keep working."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "What to say." }
            },
            "required": ["text"]
        }),
    }
}

#[derive(Deserialize)]
struct Args {
    text: String,
}

pub fn run(db: &Arc<Mutex<Db>>, bot_id: &str, args: &str) -> String {
    let Ok(parsed) = serde_json::from_str::<Args>(args) else {
        return "Could not read `text`.".to_string();
    };
    let trimmed = parsed.text.trim();
    if trimmed.is_empty() {
        return "Nothing was said: the text was empty.".to_string();
    }

    let db = db.lock().expect("db mutex poisoned");
    let conversation_id =
        store::get_or_create_conversation(&db, bot_id).expect("get_or_create_conversation");
    store::append_message(
        &db,
        &conversation_id,
        "assistant",
        trimmed,
        NewMessage {
            bot_id: Some(bot_id.to_string()),
            ..Default::default()
        },
    )
    .expect("append say message");

    "Said. Josh can see that now. Carry on - this did not end your turn.".to_string()
}
