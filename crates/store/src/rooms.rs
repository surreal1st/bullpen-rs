//! Group chats: several bots sharing one conversation, one of them the DB
//! row's owner (`bot_id`) and the rest in `members`. Port of the TS
//! `threads.ts:330-484` (`listRooms`, `getRoom`, `createRoom`, `updateRoom`).

use crate::Db;
use crate::bots::get_bot;
use crate::conversations::{MAX_ROOM_MEMBERS, get_conversation, now_iso, parse_members};
use crate::roster::first_line;
use rusqlite::params;
use shared::RoomSummary;
use uuid::Uuid;

struct RoomRow {
    id: String,
    title: String,
    bot_id: String,
    members: Option<String>,
    created_at: String,
    preview: Option<String>,
    last_at: Option<String>,
    unread: i64,
}

fn to_room(row: RoomRow) -> RoomSummary {
    let guests = parse_members(row.members.as_deref());
    let mut member_ids = Vec::with_capacity(guests.len() + 1);
    member_ids.push(row.bot_id);
    member_ids.extend(guests);

    RoomSummary {
        id: row.id,
        title: if row.title.is_empty() {
            "Group chat".to_string()
        } else {
            row.title
        },
        member_ids,
        unread: row.unread.max(0) as u32,
        preview: first_line(&row.preview.unwrap_or_default()),
        last_at: Some(row.last_at.unwrap_or(row.created_at.clone())),
        created_at: row.created_at,
    }
}

/// Every group chat, across every bot, for the rail's GROUP CHATS section.
/// `list_threads` cannot answer this - it is scoped to one bot's own strip.
/// Mirrors the TS `listRooms`.
pub fn list_rooms(db: &Db) -> rusqlite::Result<Vec<RoomSummary>> {
    let mut stmt = db.conn().prepare(
        "SELECT c.id, c.title, c.bot_id, c.members, c.created_at,
                (SELECT m.content FROM messages m WHERE m.conversation_id = c.id
                  ORDER BY m.created_at DESC, m.seq DESC LIMIT 1) AS preview,
                (SELECT m.created_at FROM messages m WHERE m.conversation_id = c.id
                  ORDER BY m.created_at DESC, m.seq DESC LIMIT 1) AS last_at,
                (SELECT COUNT(*) FROM messages m WHERE m.conversation_id = c.id
                  AND m.role = 'assistant' AND m.created_at > COALESCE(c.seen_at, '')) AS unread
           FROM conversations c
          WHERE c.kind = 'room' AND c.archived_at IS NULL
          ORDER BY COALESCE(last_at, c.created_at) DESC",
    )?;

    let rows = stmt
        .query_map([], |row| {
            Ok(RoomRow {
                id: row.get(0)?,
                title: row.get(1)?,
                bot_id: row.get(2)?,
                members: row.get(3)?,
                created_at: row.get(4)?,
                preview: row.get(5)?,
                last_at: row.get(6)?,
                unread: row.get(7)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows.into_iter().map(to_room).collect())
}

/// One room by id. Mirrors the TS `getRoom`.
pub fn get_room(db: &Db, id: &str) -> rusqlite::Result<Option<RoomSummary>> {
    Ok(list_rooms(db)?.into_iter().find(|room| room.id == id))
}

/// Checks a proposed full roster for a group chat: two to `MAX_ROOM_MEMBERS`
/// total, every id a real non-archived bot, no duplicates. The first id
/// becomes the DB row's owner. Mirrors the TS `checkRoster`.
fn check_roster(db: &Db, member_ids: &[String]) -> Result<(String, Vec<String>), String> {
    let mut unique: Vec<String> = Vec::new();
    for id in member_ids {
        if !unique.contains(id) {
            unique.push(id.clone());
        }
    }

    if unique.len() < 2 {
        return Err("A group chat needs at least two bots.".to_string());
    }
    if unique.len() > MAX_ROOM_MEMBERS {
        return Err("A group chat can have at most six bots.".to_string());
    }

    let owner_id = unique[0].clone();
    let guests = unique[1..].to_vec();

    let owner = get_bot(db, &owner_id).map_err(|e| e.to_string())?;
    match owner {
        Some(b) if !b.archived => {}
        _ => return Err(format!("No such bot: {owner_id}")),
    }
    for id in &guests {
        let bot = get_bot(db, id).map_err(|e| e.to_string())?;
        match bot {
            Some(b) if !b.archived => {}
            _ => return Err(format!("No such bot: {id}")),
        }
    }

    Ok((owner_id, guests))
}

/// Creates a group chat. `member_ids[0]` becomes the owner (the DB row's
/// `bot_id`), the rest become `members`. Mirrors the TS `createRoom`.
pub fn create_room(db: &Db, title: &str, member_ids: &[String]) -> Result<RoomSummary, String> {
    let (owner_id, guests) = check_roster(db, member_ids)?;

    let id = Uuid::new_v4().to_string();
    let now = now_iso();
    let trimmed_title: String = title.trim().chars().take(120).collect();
    let members_json = serde_json::to_string(&guests).map_err(|e| e.to_string())?;

    db.conn()
        .execute(
            "INSERT INTO conversations (id, bot_id, title, created_at, last_at, kind, members)
             VALUES (?1, ?2, ?3, ?4, ?5, 'room', ?6)",
            params![id, owner_id, trimmed_title, now.clone(), now, members_json],
        )
        .map_err(|e| e.to_string())?;

    get_room(db, &id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "failed to create room".to_string())
}

/// Updates a group chat's title and/or roster. Mirrors the TS `updateRoom`.
pub fn update_room(
    db: &Db,
    id: &str,
    title: Option<&str>,
    member_ids: Option<&[String]>,
) -> Result<RoomSummary, String> {
    let conversation = get_conversation(db, id).map_err(|e| e.to_string())?;
    match &conversation {
        Some(c) if c.kind == "room" => {}
        _ => return Err("no such group chat".to_string()),
    }

    if let Some(member_ids) = member_ids {
        let (owner_id, guests) = check_roster(db, member_ids)?;
        let members_json = serde_json::to_string(&guests).map_err(|e| e.to_string())?;
        db.conn()
            .execute(
                "UPDATE conversations SET bot_id = ?1, members = ?2 WHERE id = ?3",
                params![owner_id, members_json, id],
            )
            .map_err(|e| e.to_string())?;
    }

    if let Some(title) = title {
        let trimmed: String = title.chars().take(120).collect();
        db.conn()
            .execute(
                "UPDATE conversations SET title = ?1 WHERE id = ?2",
                params![trimmed, id],
            )
            .map_err(|e| e.to_string())?;
    }

    get_room(db, id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no such group chat".to_string())
}
