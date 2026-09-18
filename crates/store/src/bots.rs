use crate::Db;
use crate::conversations::now_iso;
use rusqlite::{OptionalExtension, params};
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

/// List bots, ordered by name - port shape of `store.ts:521-528`'s
/// `listAllBots`, narrowed to the one axis this ticket needs (no `scope`
/// support here, same narrowing `create_bot`'s own doc already calls out for
/// this schema). ARCH-01: the filter lives in the CALLER's hands (`archived`
/// picks which side of `archived_at IS NULL` this returns) rather than a
/// second, near-duplicate query function - every existing call site wants
/// `false` (the roster's own bots), and the new archived-bot listing route
/// is the one caller that wants `true`.
pub fn list_bots(db: &Db, archived: bool) -> rusqlite::Result<Vec<Bot>> {
    let mut stmt = db.conn().prepare(
        "SELECT id, name, purpose, instructions, model, archived_at, has_routine,
                section_id, pinned_at, hidden_at, avatar, shape, effort, is_template, voice
         FROM bots WHERE (archived_at IS NOT NULL) = ?1 ORDER BY name",
    )?;

    let bots = stmt
        .query_map(params![archived as i64], |row| {
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

/// Get a bot's egress configuration (as a JSON string). Returns the default
/// if the bot is not found or archived.
pub fn get_bot_egress(db: &Db, id: &str) -> rusqlite::Result<Option<String>> {
    let mut stmt = db
        .conn()
        .prepare("SELECT egress FROM bots WHERE id = ?1 AND archived_at IS NULL")?;

    let result = stmt.query_row(params![id], |row| row.get(0));

    match result {
        Ok(egress) => Ok(Some(egress)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Set a bot's egress configuration (as a JSON string).
pub fn set_bot_egress(db: &Db, id: &str, egress_json: &str) -> rusqlite::Result<()> {
    db.conn().execute(
        "UPDATE bots SET egress = ?1 WHERE id = ?2",
        params![egress_json, id],
    )?;
    Ok(())
}

/// F7b-01: fields for a freshly created bot - port of
/// `projects/bullpen-night/src/server/store.ts:396-404`'s `BotDraft`,
/// narrowed to what this schema stores. No `user_id` (S5b scopes are not
/// ported into this schema - see `crate::migrations`) and no `voice`
/// (S11's device-voice field, also not ported).
pub struct BotDraft {
    pub name: String,
    pub purpose: String,
    pub instructions: String,
    pub model: Option<String>,
}

/// The base slug: `name` lowercased, every run of a non `[a-z0-9]`
/// character collapsed to a single `-`, leading/trailing `-` trimmed,
/// truncated to 40 characters - in that order, matching
/// `store.ts:380-386`'s `slugFor` exactly (truncation happens AFTER
/// trimming, so a cut that lands on an internal `-` is not re-trimmed).
/// `"bot"` when nothing survives, e.g. a name of only punctuation.
fn slug_base(name: &str) -> String {
    let mut collapsed = String::new();
    let mut last_was_dash = false;
    for ch in name.to_lowercase().chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            collapsed.push(ch);
            last_was_dash = false;
        } else if !last_was_dash {
            collapsed.push('-');
            last_was_dash = true;
        }
    }
    let trimmed = collapsed.trim_matches('-');
    let truncated: String = trimmed.chars().take(40).collect();
    if truncated.is_empty() {
        "bot".to_string()
    } else {
        truncated
    }
}

/// A url-safe id derived from the name, unique within the roster - port of
/// `store.ts:379-394`'s `slugFor`. Uniqueness is by suffix against the
/// existing roster: `trinity`, then `trinity-2`, `trinity-3`, matching the
/// TS numbering exactly (starts at 2, not 1).
fn slug_for(db: &Db, name: &str) -> rusqlite::Result<String> {
    let base = slug_base(name);
    let mut candidate = base.clone();
    let mut n = 2;
    loop {
        let taken = db
            .conn()
            .query_row(
                "SELECT 1 FROM bots WHERE id = ?1",
                params![candidate],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !taken {
            return Ok(candidate);
        }
        candidate = format!("{base}-{n}");
        n += 1;
    }
}

/// ARCH-01: archives or restores a bot - port of `store.ts:511-519`'s
/// `setArchived`. `None` when no such bot exists, so the route can answer
/// 404 rather than silently writing nothing. Archiving stamps `archived_at`
/// with the current ISO timestamp; restoring sets it back to `NULL` -
/// nothing else on the row changes, and nothing referencing this bot
/// (conversations, messages, memory) is touched at all: archiving hides a
/// bot from the roster, it does not delete anything.
pub fn set_archived(db: &Db, id: &str, archived: bool) -> rusqlite::Result<Option<Bot>> {
    if get_bot(db, id)?.is_none() {
        return Ok(None);
    }
    let archived_at = archived.then(now_iso);
    db.conn().execute(
        "UPDATE bots SET archived_at = ?1 WHERE id = ?2",
        params![archived_at, id],
    )?;
    get_bot(db, id)
}

/// RAIL-01: pins or unpins a bot - same shape as `set_archived` above,
/// port of `roster.ts:95`'s `setPinned` (itself the `flag` helper bound to
/// `pinned_at`). `None` when no such bot exists. Pinning stamps `pinned_at`
/// with the current ISO timestamp; unpinning sets it back to `NULL`. This
/// function only ever writes the one column - moving a pinned bot to the
/// front of the rail is `crate::roster::list_roster`'s own `ORDER BY`, not
/// anything this function does.
pub fn set_pinned(db: &Db, id: &str, pinned: bool) -> rusqlite::Result<Option<Bot>> {
    if get_bot(db, id)?.is_none() {
        return Ok(None);
    }
    let pinned_at = pinned.then(now_iso);
    db.conn().execute(
        "UPDATE bots SET pinned_at = ?1 WHERE id = ?2",
        params![pinned_at, id],
    )?;
    get_bot(db, id)
}

/// RAIL-01: hides or unhides a bot - same shape as `set_pinned` above, port
/// of `roster.ts:96`'s `setHidden`. `None` when no such bot exists. Hiding
/// is independent of archiving: this only ever touches `hidden_at`, never
/// `archived_at`, so a bot can carry either flag, both, or neither at once
/// (`crates/server/tests/bots.rs`'s independence test covers this).
pub fn set_hidden(db: &Db, id: &str, hidden: bool) -> rusqlite::Result<Option<Bot>> {
    if get_bot(db, id)?.is_none() {
        return Ok(None);
    }
    let hidden_at = hidden.then(now_iso);
    db.conn().execute(
        "UPDATE bots SET hidden_at = ?1 WHERE id = ?2",
        params![hidden_at, id],
    )?;
    get_bot(db, id)
}

/// RAIL-01: every hidden bot, independent of `archived_at` - the only way a
/// hidden bot is reachable again once `crate::roster::list_roster` stops
/// carrying it, same reason `list_bots(db, true)` exists for archived bots.
/// Deliberately its own query rather than a second axis bolted onto
/// `list_bots` above: that function's own doc already narrows it to the ONE
/// axis every existing caller wants (`archived`), and `hidden_at`/
/// `archived_at` are independent columns - `WHERE (archived_at IS NOT NULL)
/// = ?1` has nothing to say about `hidden_at`, and a hidden *and* archived
/// bot must still show up here (see the independence test this ticket
/// adds), which a shared "narrow to whichever flag was asked for" query
/// cannot express without also filtering on the other one.
pub fn list_hidden(db: &Db) -> rusqlite::Result<Vec<Bot>> {
    let mut stmt = db.conn().prepare(
        "SELECT id, name, purpose, instructions, model, archived_at, has_routine,
                section_id, pinned_at, hidden_at, avatar, shape, effort, is_template, voice
         FROM bots WHERE hidden_at IS NOT NULL ORDER BY name",
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

/// F7b-01: adds a bot to the roster - port of `store.ts:411-426`'s
/// `createBot`. Only `id, name, purpose, instructions, model, created_at`
/// are written; every other column keeps its schema default (`crate::
/// migrations`'s `CREATE TABLE bots` / later `ALTER TABLE` defaults).
/// Returns the row through `get_bot` so the caller sees the exact same
/// shape a getter would.
pub fn create_bot(db: &Db, draft: BotDraft) -> rusqlite::Result<Bot> {
    let id = slug_for(db, &draft.name)?;
    db.conn().execute(
        "INSERT INTO bots (id, name, purpose, instructions, model, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            id,
            draft.name,
            draft.purpose,
            draft.instructions,
            draft.model,
            now_iso(),
        ],
    )?;
    Ok(get_bot(db, &id)?.expect("just-inserted bot row must exist"))
}
