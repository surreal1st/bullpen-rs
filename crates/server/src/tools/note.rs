//! `note`: save a fact that matters only for a while, then drops out of
//! memory on its own. Port of the Grok-shaped tiered-memory tools (S3-03) -
//! `note` is the TTL cousin of `remember`.

use std::sync::Arc;

use model::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use store::Db;

use super::lock_db;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "note".to_string(),
        description: "Save one fact that matters only for a while - it expires on its own and \
drops out of memory. Use it for something true right now but not worth keeping forever (a \
status, something in progress, a reminder for later today)."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "fact": { "type": "string", "description": "One fact, stated plainly and in full." },
                "ttl": { "type": "string", "description": "How long it matters: \"1h\", \"1d\" (default), or \"1w\"." }
            },
            "required": ["fact"]
        }),
    }
}

#[derive(Deserialize)]
struct Args {
    fact: String,
    ttl: Option<String>,
}

/// Parses a TTL word into seconds. An unrecognised or absent word falls
/// back to a day - the ticket's stated default - rather than erroring, so a
/// model that guesses a slightly wrong word still gets a note instead of a
/// refusal.
fn ttl_secs(ttl: Option<&str>) -> u64 {
    match ttl {
        Some("1h") => 3600,
        Some("1w") => 7 * 24 * 3600,
        _ => 24 * 3600, // "1d" and anything else
    }
}

pub fn run(db: &Arc<std::sync::Mutex<Db>>, bot_id: &str, args: &str) -> String {
    let Ok(parsed) = serde_json::from_str::<Args>(args) else {
        return "Could not read `fact`.".to_string();
    };
    let fact = parsed.fact.trim();
    if fact.is_empty() {
        return "Nothing to note: the fact was empty.".to_string();
    }
    let secs = ttl_secs(parsed.ttl.as_deref());

    let db = lock_db(db);
    store::note(&db, bot_id, fact, secs).expect("note");

    "Noted.".to_string()
}
