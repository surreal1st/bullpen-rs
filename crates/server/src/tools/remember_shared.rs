//! `remember_shared`: save one fact into memory every bot can read, not
//! just this one. Port of the Grok-shaped tiered-memory tools (S3-03).

use std::sync::Arc;

use model::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use store::{Db, Scope};

use super::lock_db;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "remember_shared".to_string(),
        description: "Save one durable fact into SHARED memory, visible to every bot on the \
roster, not just you. Use it for something the whole roster should know, never for something \
private to your own job."
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

pub fn run(db: &Arc<std::sync::Mutex<Db>>, bot_id: &str, args: &str) -> String {
    let Ok(parsed) = serde_json::from_str::<Args>(args) else {
        return "Could not read `fact`.".to_string();
    };
    let fact = parsed.fact.trim();
    if fact.is_empty() {
        return "Nothing to remember: the fact was empty.".to_string();
    }

    let db = lock_db(db);
    store::remember_scoped(&db, bot_id, fact, Scope::Shared, None).expect("remember_scoped");

    "Remembered, shared with every bot.".to_string()
}
