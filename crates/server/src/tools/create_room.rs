//! `create_room`: start a group chat. One of the two Grok gaps - Grok Bot
//! has no group chats at all. Port over `store::create_room`.

use std::sync::Arc;

use model::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use store::Db;

use super::lock_db;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "create_room".to_string(),
        description: "Start a new group chat with up to six other bots. You are added as the \
owner automatically - no need to include yourself in member_ids."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "the room's name" },
                "member_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "the other bots to add, by id"
                }
            },
            "required": ["title", "member_ids"]
        }),
    }
}

#[derive(Deserialize)]
struct Args {
    title: String,
    #[serde(default)]
    member_ids: Vec<String>,
}

/// Always puts the caller first (the owner), whether or not the model
/// remembered to include itself - a model that forgot itself should not end
/// up a guest in the room it just asked to create.
pub fn run(db: &Arc<std::sync::Mutex<Db>>, caller_bot_id: &str, args: &str) -> String {
    let Ok(parsed) = serde_json::from_str::<Args>(args) else {
        return "Could not read `title`/`member_ids`.".to_string();
    };

    let mut ids = vec![caller_bot_id.to_string()];
    for id in parsed.member_ids {
        if id != caller_bot_id && !ids.contains(&id) {
            ids.push(id);
        }
    }

    let db = lock_db(db);
    match store::create_room(&db, &parsed.title, &ids) {
        Ok(room) => format!("Created \"{}\" (room {}).", room.title, room.id),
        Err(message) => message,
    }
}
