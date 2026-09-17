//! `search_memory`: full-text search over a bot's memory - its own log,
//! every project it belongs to, and shared. Port of the TS `search_memory`
//! tool (`app.ts:7196-7206`) and `formatMemoryHits` (`app.ts:7262-7273`).

use std::sync::Arc;

use model::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use store::{Db, Scope};

use super::{fence_tool_output, lock_db};

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "search_memory".to_string(),
        description: "Search your own memory for something you were told or learned earlier. \
Use this before saying you do not know something. If the exact words fail, related words are \
tried for you."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What to look for, in plain words." }
            },
            "required": ["query"]
        }),
    }
}

#[derive(Deserialize)]
struct Args {
    query: String,
}

/// True when `content` already starts with a `(YYYY-MM-DD)` date prefix -
/// an imported memory carries its own date, and prefixing the row's
/// `created_at` on top of that would print the date twice. Port of the TS
/// regex `/^\(\d{4}-\d{2}-\d{2}\)/`.
fn already_dated(content: &str) -> bool {
    let b = content.as_bytes();
    b.len() >= 12
        && b[0] == b'('
        && b[5] == b'-'
        && b[8] == b'-'
        && b[11] == b')'
        && b[1..5].iter().all(u8::is_ascii_digit)
        && b[6..8].iter().all(u8::is_ascii_digit)
        && b[9..11].iter().all(u8::is_ascii_digit)
}

/// Port of the TS `formatMemoryHits`.
fn format_hits(hits: &[store::LogEntry]) -> String {
    if hits.is_empty() {
        return "Nothing in memory matches that.".to_string();
    }
    hits.iter()
        .map(|h| {
            if already_dated(&h.content) {
                format!("- {}", h.content)
            } else {
                let date = &h.created_at[..h.created_at.len().min(10)];
                format!("- ({date}) {}", h.content)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn run(db: &Arc<std::sync::Mutex<Db>>, bot_id: &str, args: &str) -> String {
    let Ok(parsed) = serde_json::from_str::<Args>(args) else {
        return "Could not read `query`.".to_string();
    };
    let query = parsed.query.trim();
    if query.is_empty() {
        return "Nothing in memory matches that.".to_string();
    }

    let db = lock_db(db);
    let hits = store::search_log(
        &db,
        bot_id,
        query,
        &[Scope::Own, Scope::Project, Scope::Shared],
        20,
    )
    .expect("search_log");

    // S8b-06 Decision: fenced when there is a hit. Every row here was
    // written by `remember`/`note`/`remember_shared`/`project_remember`
    // (`tools/remember.rs` etc.) taking a `fact` STRING FROM THE MODEL, not
    // literally typed by Josh - `Scope::Shared`/`Scope::Project` rows can be
    // another bot's own words, and even an `Own`-scope row can be this same
    // bot's summary of a page it browsed earlier and chose to keep. That is
    // exactly the "a model's own generated text, read back later with
    // nothing marking it as data" shape this ticket's `message_bot` gap has -
    // memory is just a slower relay than a live tool call. The empty-result
    // sentence stays unfenced: it is server-generated, not a fact anyone
    // wrote (same reasoning as `desk_shell`'s "(no output)").
    if hits.is_empty() {
        format_hits(&hits)
    } else {
        fence_tool_output(&format_hits(&hits))
    }
}
