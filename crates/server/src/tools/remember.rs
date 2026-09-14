//! `remember`: write one fact to a bot's own memory log. Port of the
//! `remember` tool over `src/server/memory.ts`'s `remember`.

use std::sync::{Arc, Mutex};

use model::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use store::Db;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "remember".to_string(),
        description: "Save one durable fact so a future conversation has it. Use it for \
decisions, preferences and facts that outlive this conversation, never for chit-chat."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "fact": { "type": "string", "description": "One fact, stated plainly and in full." }
            },
            "required": ["fact"]
        }),
    }
}

#[derive(Deserialize)]
struct Args {
    fact: String,
}

pub fn run(db: &Arc<Mutex<Db>>, bot_id: &str, args: &str) -> String {
    let Ok(parsed) = serde_json::from_str::<Args>(args) else {
        return "Could not read `fact`.".to_string();
    };
    let fact = parsed.fact.trim();
    if fact.is_empty() {
        return "Nothing to remember: the fact was empty.".to_string();
    }

    let db = db.lock().expect("db mutex poisoned");
    store::remember(&db, bot_id, fact, "bot").expect("remember");

    "Remembered.".to_string()
}
