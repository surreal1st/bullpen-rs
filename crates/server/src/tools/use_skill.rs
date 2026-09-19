//! S10-01: `use_skill` - loads a skill's body by name and hands it back as
//! the tool result. Port of `app.ts:5270-5279` (spec) and `app.ts:6543-6545`
//! (dispatch), over `store::skills::read_skill`.
//!
//! A skill is DATA, not authority (see `store::skills`'s own doc): this tool
//! only reads text Josh already enabled for the calling bot. It grants no
//! tool, widens no permission, and its default decision is `Allow`
//! (`permissions.rs`) for exactly that reason.

use std::sync::Arc;
use std::sync::Mutex;

use model::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use store::Db;

use super::lock_db;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "use_skill".to_string(),
        description: "Load a skill by name and follow it. The skills you have are listed in \
your instructions, each with when to use it. Call this before starting a job one of them \
covers, rather than working it out yourself."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The skill's name." }
            },
            "required": ["name"]
        }),
    }
}

#[derive(Deserialize)]
struct Args {
    name: String,
}

/// `readSkill(db, botId, input.name)`, TS `app.ts:6544`. A missing/wrong-
/// typed `name` field parses to `""`, which `store::skills::read_skill`
/// already turns into its own "No skill called \"\"" message rather than a
/// hard error - matching the TS's own `typeof input.name === "string" ?
/// input.name : ""` coercion, not this crate's usual strict-`Args` refusal.
pub fn run(db: &Arc<Mutex<Db>>, bot_id: &str, args: &str) -> String {
    let name = serde_json::from_str::<Args>(args)
        .map(|a| a.name)
        .unwrap_or_default();
    let db = lock_db(db);
    store::skills::read_skill(&db, bot_id, &name)
}
