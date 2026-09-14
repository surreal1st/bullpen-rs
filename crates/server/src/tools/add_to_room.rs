//! `add_to_room`: add a bot to an existing group chat. The second Grok gap.
//! Port over `store::update_room`.

use std::sync::Arc;

use model::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use store::Db;

use super::lock_db;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "add_to_room".to_string(),
        description: "Add a bot to an existing group chat.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "room": { "type": "string", "description": "the room's id or title" },
                "bot": { "type": "string", "description": "the bot's id or name to add" }
            },
            "required": ["room", "bot"]
        }),
    }
}

#[derive(Deserialize)]
struct Args {
    room: String,
    bot: String,
}

pub fn run(db: &Arc<std::sync::Mutex<Db>>, args: &str) -> String {
    let Ok(parsed) = serde_json::from_str::<Args>(args) else {
        return "Could not read `room`/`bot`.".to_string();
    };

    let db = lock_db(db);

    let rooms = store::list_rooms(&db).expect("list_rooms");
    let Some(room) = rooms
        .into_iter()
        .find(|r| r.id == parsed.room || r.title.eq_ignore_ascii_case(&parsed.room))
    else {
        return format!("No such room: {}", parsed.room);
    };

    let bots = store::list_bots(&db).expect("list_bots");
    let Some(bot) = bots
        .into_iter()
        .find(|b| b.id == parsed.bot || b.name.eq_ignore_ascii_case(&parsed.bot))
    else {
        return format!("No such bot: {}", parsed.bot);
    };

    if room.member_ids.contains(&bot.id) {
        return format!("{} is already in \"{}\".", bot.name, room.title);
    }

    let mut ids = room.member_ids.clone();
    ids.push(bot.id.clone());

    match store::update_room(&db, &room.id, None, Some(&ids)) {
        Ok(updated) => format!(
            "Added {} to \"{}\" (room {}).",
            bot.name, updated.title, updated.id
        ),
        Err(message) => message,
    }
}
