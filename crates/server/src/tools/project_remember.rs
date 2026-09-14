//! `project_remember`: save one fact into a named project's memory,
//! visible to every bot on that project rather than the whole roster. Port
//! of the Grok-shaped tiered-memory tools (S3-03).

use std::sync::Arc;

use model::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use store::{Db, Scope};

use super::lock_db;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "project_remember".to_string(),
        description: "Save one fact into a named PROJECT's shared memory, visible to every bot \
on that project. Name the project exactly as you know it; an unknown name is refused rather \
than guessed."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "project": { "type": "string", "description": "The project's exact name." },
                "fact": { "type": "string", "description": "One fact, stated plainly and in full." }
            },
            "required": ["project", "fact"]
        }),
    }
}

#[derive(Deserialize)]
struct Args {
    project: String,
    fact: String,
}

pub fn run(db: &Arc<std::sync::Mutex<Db>>, bot_id: &str, args: &str) -> String {
    let Ok(parsed) = serde_json::from_str::<Args>(args) else {
        return "Could not read `project` and `fact`.".to_string();
    };
    let fact = parsed.fact.trim();
    if fact.is_empty() {
        return "Nothing to remember: the fact was empty.".to_string();
    }
    let wanted = parsed.project.trim();

    let db = lock_db(db);
    let projects = store::projects_for(&db, bot_id).expect("projects_for");
    let Some(project) = projects.iter().find(|p| p.name == wanted) else {
        if projects.is_empty() {
            return "You are not a member of any project.".to_string();
        }
        let names: Vec<&str> = projects.iter().map(|p| p.name.as_str()).collect();
        return format!(
            "No project named \"{wanted}\". Your projects: {}.",
            names.join(", ")
        );
    };

    store::remember_scoped(&db, bot_id, fact, Scope::Project, Some(&project.id))
        .expect("remember_scoped");

    format!("Remembered, shared with Project: {}.", project.name)
}
