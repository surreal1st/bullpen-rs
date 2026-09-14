use crate::Db;
use rusqlite::params;
use shared::{Bot, Effort, Section};

struct BotRow {
    id: String,
    name: String,
    purpose: String,
    instructions: String,
    model: Option<String>,
    archived_at: Option<String>,
    has_routine: i32,
    section_id: Option<String>,
    pinned_at: Option<String>,
    hidden_at: Option<String>,
    avatar: Option<String>,
    shape: Option<String>,
    effort: Option<String>,
    is_template: i32,
    voice: Option<String>,
}

fn row_to_bot(row: BotRow) -> Bot {
    Bot {
        id: row.id,
        name: row.name,
        purpose: row.purpose,
        instructions: row.instructions,
        model: row.model,
        archived: row.archived_at.is_some(),
        has_routine: row.has_routine == 1,
        section_id: row.section_id,
        pinned: row.pinned_at.is_some(),
        hidden: row.hidden_at.is_some(),
        avatar: row.avatar,
        shape: row.shape,
        effort: row
            .effort
            .as_deref()
            .unwrap_or("medium")
            .parse()
            .unwrap_or(Effort::Medium),
        is_template: row.is_template == 1,
        voice: row.voice,
    }
}

/// List all non-archived bots, ordered by name.
pub fn list_bots(db: &Db) -> rusqlite::Result<Vec<Bot>> {
    let mut stmt = db.conn().prepare(
        "SELECT id, name, purpose, instructions, model, archived_at, has_routine,
                section_id, pinned_at, hidden_at, avatar, shape, effort, is_template, voice
         FROM bots WHERE archived_at IS NULL ORDER BY name",
    )?;

    let bots = stmt
        .query_map([], |row| {
            Ok(BotRow {
                id: row.get(0)?,
                name: row.get(1)?,
                purpose: row.get(2)?,
                instructions: row.get(3)?,
                model: row.get(4)?,
                archived_at: row.get(5)?,
                has_routine: row.get(6)?,
                section_id: row.get(7)?,
                pinned_at: row.get(8)?,
                hidden_at: row.get(9)?,
                avatar: row.get(10)?,
                shape: row.get(11)?,
                effort: row.get(12)?,
                is_template: row.get(13)?,
                voice: row.get(14)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(bots.into_iter().map(row_to_bot).collect())
}

/// Get a single bot by id, or None if not found or archived.
pub fn get_bot(db: &Db, id: &str) -> rusqlite::Result<Option<Bot>> {
    let mut stmt = db.conn().prepare(
        "SELECT id, name, purpose, instructions, model, archived_at, has_routine,
                section_id, pinned_at, hidden_at, avatar, shape, effort, is_template, voice
         FROM bots WHERE id = ?1",
    )?;

    let result = stmt.query_row(params![id], |row| {
        Ok(BotRow {
            id: row.get(0)?,
            name: row.get(1)?,
            purpose: row.get(2)?,
            instructions: row.get(3)?,
            model: row.get(4)?,
            archived_at: row.get(5)?,
            has_routine: row.get(6)?,
            section_id: row.get(7)?,
            pinned_at: row.get(8)?,
            hidden_at: row.get(9)?,
            avatar: row.get(10)?,
            shape: row.get(11)?,
            effort: row.get(12)?,
            is_template: row.get(13)?,
            voice: row.get(14)?,
        })
    });

    match result {
        Ok(row) => Ok(Some(row_to_bot(row))),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e),
    }
}

/// List all sections, ordered by position.
pub fn list_sections(db: &Db) -> rusqlite::Result<Vec<Section>> {
    let mut stmt = db
        .conn()
        .prepare("SELECT id, name, position FROM sections ORDER BY position")?;

    let sections = stmt
        .query_map([], |row| {
            Ok(Section {
                id: row.get(0)?,
                name: row.get(1)?,
                position: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(sections)
}
