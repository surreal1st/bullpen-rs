//! S10-08: `hire_bot` — add a named colleague to the roster after Josh
//! approves. Port of `app.ts:6178-6231` (dispatch) and `app.ts:5063-5085`
//! (spec).

use std::sync::{Arc, Mutex};

use model::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use shared::faces::SHAPES;
use store::{
    BotDraft, Db, create_bot, create_section, list_bots, list_sections, move_bot, set_shape,
};

use super::lock_db;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "hire_bot".to_string(),
        description:
            "Hire a named colleague onto the roster with its own purpose and instructions. \
Josh approves every hire before the bot exists. Use it when a goal needs a skill nobody on the \
roster has; use spawn_helper for a throwaway errand instead."
                .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The new colleague's name, as it will appear on the roster." },
                "purpose": { "type": "string", "description": "A short line describing what this bot is for." },
                "instructions": { "type": "string", "description": "The system instructions the new bot runs with." },
                "section": {
                    "type": "string",
                    "description": "A section to place the bot in, by name. An existing name reuses that section; an unknown name creates it. Omitted leaves the bot unassigned."
                },
                "shape": {
                    "type": "string",
                    "description": "The bot's face on the roster: circle, rounded, hexagon, shield, diamond, leaf, arch, or chip. Omit to let it default. Anything else sets no shape."
                }
            },
            "required": ["name", "purpose", "instructions"]
        }),
    }
}

#[derive(Deserialize)]
struct Args {
    #[serde(default)]
    name: String,
    #[serde(default)]
    purpose: String,
    #[serde(default)]
    instructions: String,
    #[serde(default)]
    section: Option<String>,
    #[serde(default)]
    shape: Option<String>,
}

pub fn run(db: &Arc<Mutex<Db>>, _bot_id: &str, args: &str) -> String {
    let input: Args = serde_json::from_str(args).unwrap_or(Args {
        name: String::new(),
        purpose: String::new(),
        instructions: String::new(),
        section: None,
        shape: None,
    });

    let draft_name = input.name.trim();
    if draft_name.is_empty() {
        return "A hire needs a name.".to_string();
    }

    let db = lock_db(db);

    if list_bots(&db, false, None)
        .unwrap_or_default()
        .iter()
        .any(|b| b.name.eq_ignore_ascii_case(draft_name))
    {
        let existing = list_bots(&db, false, None)
            .unwrap_or_default()
            .into_iter()
            .find(|b| b.name.eq_ignore_ascii_case(draft_name))
            .map(|b| b.name)
            .unwrap_or_else(|| draft_name.to_string());
        return format!(
            "There is already a bot named \"{existing}\" on the roster. Nothing was created."
        );
    }

    let hired = match create_bot(
        &db,
        BotDraft {
            name: draft_name.to_string(),
            purpose: input.purpose,
            instructions: input.instructions,
            model: None,
        },
    ) {
        Ok(bot) => bot,
        Err(err) => return format!("Could not create the bot: {err}."),
    };

    let mut notes: Vec<String> = Vec::new();

    let section_name = input.section.as_deref().unwrap_or("").trim().to_string();
    if !section_name.is_empty() {
        let sections = list_sections(&db, None).unwrap_or_default();
        if let Some(target) = sections
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(&section_name))
        {
            let _ = move_bot(&db, &hired.id, Some(&target.id));
        } else if let Some(created) = create_section(&db, &section_name).unwrap_or(None) {
            let _ = move_bot(&db, &hired.id, Some(&created.id));
        } else {
            notes.push(format!(
                "Could not create section \"{section_name}\": unknown error."
            ));
        }
    }

    let raw_shape = input.shape.as_deref().unwrap_or("").trim();
    if !raw_shape.is_empty() {
        if let Some((key, _)) = SHAPES
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(raw_shape))
        {
            let _ = set_shape(&db, &hired.id, Some(key));
        } else {
            let valid: Vec<_> = SHAPES.iter().map(|(k, _)| *k).collect();
            notes.push(format!(
                "\"{raw_shape}\" is not a shape - no shape was set. Valid shapes: {}.",
                valid.join(", ")
            ));
        }
    }

    let suffix = if notes.is_empty() {
        String::new()
    } else {
        format!(" {}", notes.join(" "))
    };

    format!(
        "{} {} is now live on the roster.{suffix}",
        serde_json::json!({ "id": hired.id, "name": hired.name }),
        hired.name
    )
}
