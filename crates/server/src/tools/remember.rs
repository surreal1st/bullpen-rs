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
        description: "Write one fact or event to your own memory log, so a future run of yours \
can recall it. Use for things worth knowing later, not for what you already said in this reply."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "content": { "type": "string" }
            },
            "required": ["content"]
        }),
    }
}

#[derive(Deserialize)]
struct Args {
    content: String,
}

pub fn run(db: &Arc<Mutex<Db>>, bot_id: &str, args: &str) -> String {
    let Ok(parsed) = serde_json::from_str::<Args>(args) else {
        return "Could not read `content`.".to_string();
    };
    let content = parsed.content.trim();
    if content.is_empty() {
        return "Nothing to remember: the content was empty.".to_string();
    }

    let db = db.lock().expect("db mutex poisoned");
    store::remember(&db, bot_id, content, "bot").expect("remember");

    "Remembered.".to_string()
}
