//! Conversations, threads, and rooms' member rules. Port of the TS
//! `getOrCreateConversation`/`rowToMessage` neighbourhood in `store.ts` and
//! the thread half of `threads.ts` (`listThreads`, `createThread`,
//! `touchThread`, `titleFromFirstMessage`, `getConversation`,
//! `validateMembers`). Room listing/creation lives in `rooms.rs`.

use crate::Db;
use crate::bots::get_bot;
use chrono::{SecondsFormat, Utc};
use rusqlite::{OptionalExtension, params};
use shared::{Conversation, ThreadSummary};
use uuid::Uuid;

/// A room tops out at this many OTHER bots (`validate_members`) or this many
/// bots total (`rooms::create_room`'s roster check) - the TS `threads.ts:142`
/// `MAX_ROOM_MEMBERS`, raised from 5 to 6 for S1-01.
pub(crate) const MAX_ROOM_MEMBERS: usize = 6;

/// Same format as JS `new Date().toISOString()` (millisecond precision, `Z`
/// suffix), so timestamps sort and compare identically to rows the TS server
/// wrote.
pub(crate) fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub(crate) fn parse_members(raw: Option<&str>) -> Vec<String> {
    match raw {
        None | Some("") => Vec::new(),
        Some(s) => serde_json::from_str(s).unwrap_or_default(),
    }
}

fn kind_or_chat(raw: Option<&str>) -> String {
    if raw == Some("room") {
        "room".to_string()
    } else {
        "chat".to_string()
    }
}

/// One conversation per bot, outside of rooms. Mirrors the TS
/// `getOrCreateConversation`: `kind != 'room'` on purpose - a bot that
/// happens to own a group chat's database row still gets its own ordinary
/// conversation here, never falling back to that room's messages as if they
/// were its own.
pub fn get_or_create_conversation(db: &Db, bot_id: &str) -> rusqlite::Result<String> {
    let existing: Option<String> = db
        .conn()
        .query_row(
            "SELECT id FROM conversations WHERE bot_id = ?1 AND kind != 'room' ORDER BY created_at LIMIT 1",
            params![bot_id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok(id);
    }

    let id = Uuid::new_v4().to_string();
    db.conn().execute(
        "INSERT INTO conversations (id, bot_id, title, created_at) VALUES (?1, ?2, '', ?3)",
        params![id, bot_id, now_iso()],
    )?;
    Ok(id)
}

/// The owner and room membership of one conversation. Mirrors the TS
/// `getConversation`.
pub fn get_conversation(db: &Db, id: &str) -> rusqlite::Result<Option<Conversation>> {
    db.conn()
        .query_row(
            "SELECT id, bot_id, kind, members FROM conversations WHERE id = ?1",
            params![id],
            |row| {
                let kind: Option<String> = row.get(2)?;
                let members: Option<String> = row.get(3)?;
                Ok(Conversation {
                    id: row.get(0)?,
                    bot_id: row.get(1)?,
                    kind: kind_or_chat(kind.as_deref()),
                    members: parse_members(members.as_deref()),
                })
            },
        )
        .optional()
}

struct ThreadRow {
    id: String,
    bot_id: String,
    bot_name: String,
    title: String,
    last_at: Option<String>,
    created_at: String,
    kind: Option<String>,
    members: Option<String>,
    message_count: i64,
}

fn to_thread(row: ThreadRow) -> ThreadSummary {
    ThreadSummary {
        id: row.id,
        bot_id: row.bot_id,
        bot_name: row.bot_name,
        title: row.title,
        message_count: row.message_count,
        last_at: row.last_at,
        created_at: row.created_at,
        kind: kind_or_chat(row.kind.as_deref()),
        members: parse_members(row.members.as_deref()),
    }
}

const THREAD_COLUMNS: &str =
    "c.id, c.bot_id, b.name, c.title, c.last_at, c.created_at, c.kind, c.members,
       (SELECT COUNT(*) FROM messages m WHERE m.conversation_id = c.id) AS message_count";

fn row_to_thread_row(row: &rusqlite::Row) -> rusqlite::Result<ThreadRow> {
    Ok(ThreadRow {
        id: row.get(0)?,
        bot_id: row.get(1)?,
        bot_name: row.get(2)?,
        title: row.get(3)?,
        last_at: row.get(4)?,
        created_at: row.get(5)?,
        kind: row.get(6)?,
        members: row.get(7)?,
        message_count: row.get(8)?,
    })
}

/// A bot's own thread strip. Excludes rooms on purpose (`listRooms` in
/// `rooms.rs` is where a group chat shows up instead) - S15's fix for a room
/// leaking into the bot's own strip it happens to be the database owner of.
pub fn list_threads(db: &Db, bot_id: &str) -> rusqlite::Result<Vec<ThreadSummary>> {
    let mut stmt = db.conn().prepare(&format!(
        "SELECT {THREAD_COLUMNS}
           FROM conversations c
           JOIN bots b ON b.id = c.bot_id
          WHERE c.bot_id = ?1 AND c.archived_at IS NULL AND c.kind != 'room'
          ORDER BY COALESCE(c.last_at, c.created_at) DESC"
    ))?;
    let rows = stmt
        .query_map(params![bot_id], row_to_thread_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows.into_iter().map(to_thread).collect())
}

/// One thread by id, room or not - unlike `list_threads`, which deliberately
/// excludes rooms. Used right after `create_thread` writes the row.
fn thread_by_id(db: &Db, id: &str) -> rusqlite::Result<Option<ThreadSummary>> {
    db.conn()
        .query_row(
            &format!(
                "SELECT {THREAD_COLUMNS}
                   FROM conversations c
                   JOIN bots b ON b.id = c.bot_id
                  WHERE c.id = ?1"
            ),
            params![id],
            row_to_thread_row,
        )
        .optional()
        .map(|opt| opt.map(to_thread))
}

/// Checks a proposed member list before it is ever written: every id has to
/// resolve to a real, non-archived bot, none may be the owner (already in
/// the room by definition), and a room tops out at `MAX_ROOM_MEMBERS` guests.
/// Mirrors the TS `validateMembers`.
pub fn validate_members(db: &Db, bot_id: &str, members: &[String]) -> Result<Vec<String>, String> {
    let mut unique: Vec<String> = Vec::new();
    for member in members {
        if !unique.contains(member) {
            unique.push(member.clone());
        }
    }

    if unique.len() > MAX_ROOM_MEMBERS - 1 {
        return Err("A room can have at most six bots.".to_string());
    }

    for id in &unique {
        if id == bot_id {
            return Err("The owner is already in its own room.".to_string());
        }
        let bot = get_bot(db, id).map_err(|e| e.to_string())?;
        match bot {
            Some(b) if !b.archived => {}
            _ => return Err(format!("No such bot: {id}")),
        }
    }

    Ok(unique)
}

/// Creates a bot's own thread - a plain chat when `members` is empty, a room
/// when it is not. Mirrors the TS `createThread`.
pub fn create_thread(db: &Db, bot_id: &str, members: &[String]) -> Result<ThreadSummary, String> {
    let guests = validate_members(db, bot_id, members)?;

    let id = Uuid::new_v4().to_string();
    let now = now_iso();
    let kind = if guests.is_empty() { "chat" } else { "room" };
    let members_json = serde_json::to_string(&guests).map_err(|e| e.to_string())?;

    db.conn()
        .execute(
            "INSERT INTO conversations (id, bot_id, title, created_at, last_at, kind, members)
             VALUES (?1, ?2, '', ?3, ?4, ?5, ?6)",
            params![id, bot_id, now.clone(), now, kind, members_json],
        )
        .map_err(|e| e.to_string())?;

    thread_by_id(db, &id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "failed to create thread".to_string())
}

/// Bumps a conversation's `last_at` to now. Mirrors the TS `touchThread`.
pub fn touch_thread(db: &Db, id: &str) -> rusqlite::Result<()> {
    db.conn().execute(
        "UPDATE conversations SET last_at = ?1 WHERE id = ?2",
        params![now_iso(), id],
    )?;
    Ok(())
}

/// Renames a conversation - thread or room alike, both are just
/// `conversations` rows. Capped at 120 characters. Mirrors the TS
/// `renameThread`. `false` when nothing matched `id`.
pub fn rename_thread(db: &Db, id: &str, title: &str) -> rusqlite::Result<bool> {
    let trimmed: String = title.chars().take(120).collect();
    let changed = db.conn().execute(
        "UPDATE conversations SET title = ?1 WHERE id = ?2",
        params![trimmed, id],
    )?;
    Ok(changed > 0)
}

/// Archives a conversation - thread or room - rather than destroying it.
/// Mirrors the TS `archiveThread`, reused by both `/api/threads/:id` and
/// `/api/rooms/:id`'s DELETE routes, same as the TS original. `false` when
/// `id` does not exist or was already archived.
pub fn archive_thread(db: &Db, id: &str) -> rusqlite::Result<bool> {
    let changed = db.conn().execute(
        "UPDATE conversations SET archived_at = ?1 WHERE id = ?2 AND archived_at IS NULL",
        params![now_iso(), id],
    )?;
    Ok(changed > 0)
}

/// Names a thread from its first message, so a list of conversations reads
/// as subjects rather than a column of identical dates. Mirrors the TS
/// `titleFromFirstMessage`; a no-op once the thread already has a title.
pub fn title_from_first_message(db: &Db, conversation_id: &str) -> rusqlite::Result<()> {
    let existing: Option<String> = db
        .conn()
        .query_row(
            "SELECT title FROM conversations WHERE id = ?1",
            params![conversation_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(existing) = existing else {
        return Ok(());
    };
    if !existing.trim().is_empty() {
        return Ok(());
    }

    let first: Option<String> = db
        .conn()
        .query_row(
            "SELECT content FROM messages WHERE conversation_id = ?1 AND role = 'user' ORDER BY seq LIMIT 1",
            params![conversation_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(first) = first else {
        return Ok(());
    };

    let line = first.split('\n').next().unwrap_or("").trim();
    let title = if line.chars().count() > 60 {
        let head: String = line.chars().take(57).collect();
        format!("{head}...")
    } else {
        line.to_string()
    };

    db.conn().execute(
        "UPDATE conversations SET title = ?1 WHERE id = ?2",
        params![title, conversation_id],
    )?;
    Ok(())
}
