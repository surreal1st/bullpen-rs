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

/// Get a single bot by id, or `None` if no such row exists - the query
/// carries no `archived_at` filter, so an ARCHIVED bot IS returned here.
/// (This doc comment previously said the opposite; it was wrong, not the
/// query - see IMPORT-01a. Archiving hides a bot from the roster
/// (`crate::roster::list_roster`'s own `WHERE archived_at IS NULL`) without
/// deleting anything, and this function is how an archived bot's own data
/// stays reachable at all - `crates/server/tests/bots.rs`'s
/// `archived_bots_conversations_messages_and_memory_survive` test depends on
/// exactly this.)
///
/// 🔴 `routes/import.rs`'s duplicate-name check (`get_bot(&db,
/// &slug_base(&parsed.name))`) relies on this returning `Some` for an
/// archived bot too - an archived bot is kept, not gone, so importing a file
/// whose name collides with one must still refuse rather than silently
/// creating a second row with a numbered suffix. Do not add an
/// `archived_at IS NULL` filter here without checking that caller first.
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

/// RAIL-02: creates a section - port of `roster.ts:29`'s `createSection`,
/// narrowed to the one rule this ticket's contract asks for: an empty or
/// whitespace-only name is rejected (`Ok(None)`), the same `Option` shape
/// `set_archived` already uses for "this did not happen" rather than a
/// bespoke error type.
///
/// It also trims to 60 characters and refuses a case-insensitive duplicate,
/// both ported from `roster.ts:34-40`. The duplicate check is not cosmetic:
/// without it a second section called "SHOOT" gets the id `shoot-2` and the
/// rail renders two headers with the same name and no way to tell them apart,
/// which is worse than the refusal. 60 characters is the TS cap and keeps a
/// pasted paragraph from becoming a rail header.
///
/// The id is server-assigned, never taken from the client - same posture as
/// `create_bot`'s own `slug_for` below, reusing its `slug_base` helper so a
/// section named "SHOOT" gets `shoot`, and a second section also named
/// "SHOOT" gets `shoot-2` rather than colliding.
pub fn create_section(db: &Db, name: &str) -> rusqlite::Result<Option<Section>> {
    let trimmed = name.trim();
    // `chars().take(60)`, not `&clean[..60]`: a byte slice would panic on a
    // multi-byte character straddling the boundary, and a section name is
    // free text.
    let clean: String = trimmed.chars().take(60).collect();
    if clean.is_empty() {
        return Ok(None);
    }

    let taken: Option<String> = db
        .conn()
        .query_row(
            "SELECT id FROM sections WHERE lower(name) = lower(?1)",
            params![clean],
            |row| row.get(0),
        )
        .optional()?;
    if taken.is_some() {
        return Ok(None);
    }

    let clean = clean.as_str();
    let id = slug_for_section(db, clean)?;
    let position: i32 = db.conn().query_row(
        "SELECT COALESCE(MAX(position), 0) + 1 FROM sections",
        [],
        |row| row.get(0),
    )?;
    db.conn().execute(
        "INSERT INTO sections (id, name, position) VALUES (?1, ?2, ?3)",
        params![id, clean, position],
    )?;
    Ok(Some(Section {
        id,
        name: clean.to_string(),
        position,
    }))
}

/// RAIL-02: a url-safe id derived from a section's name, unique within
/// `sections` - same shape as `slug_for` below (bots' own id generation),
/// reusing its `slug_base` helper rather than a second copy of the
/// collapse/trim/truncate logic, just checked against a different table.
fn slug_for_section(db: &Db, name: &str) -> rusqlite::Result<String> {
    let base = slug_base(name);
    let mut candidate = base.clone();
    let mut n = 2;
    loop {
        let taken = db
            .conn()
            .query_row(
                "SELECT 1 FROM sections WHERE id = ?1",
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

/// RAIL-02: renames a section - port of `roster.ts:55`'s `renameSection`.
/// `false` for an unknown id OR an empty/whitespace name - the same single
/// boolean the TS function returns, which is why the route answers both
/// cases with one combined message rather than telling them apart.
pub fn rename_section(db: &Db, id: &str, name: &str) -> rusqlite::Result<bool> {
    let clean = name.trim();
    if clean.is_empty() {
        return Ok(false);
    }
    let changed = db.conn().execute(
        "UPDATE sections SET name = ?1 WHERE id = ?2",
        params![clean, id],
    )?;
    Ok(changed > 0)
}

/// RAIL-02: deletes a section - port of `roster.ts:65`'s `deleteSection`.
/// 🔴 The bots that were in it are NEVER deleted: `section_id` is set back
/// to NULL (Unassigned) for every bot that carried this section, and only
/// THEN is the section row itself removed - in that order, deliberately,
/// because `bots.section_id` is a live foreign key into `sections(id)`
/// (`crate::Db::open` turns `foreign_keys` ON) and deleting a
/// still-referenced section row would fail the constraint outright rather
/// than cascade. `false` for an unknown id; nothing is written in that case.
pub fn delete_section(db: &Db, id: &str) -> rusqlite::Result<bool> {
    let exists: Option<String> = db
        .conn()
        .query_row(
            "SELECT id FROM sections WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()?;
    if exists.is_none() {
        return Ok(false);
    }

    db.conn().execute(
        "UPDATE bots SET section_id = NULL WHERE section_id = ?1",
        params![id],
    )?;
    db.conn()
        .execute("DELETE FROM sections WHERE id = ?1", params![id])?;
    Ok(true)
}

/// RAIL-02: moves a bot to a section, or to Unassigned - port of
/// `roster.ts:73`'s `moveBot`. `section_id: None` is Unassigned and always
/// valid; `Some(id)` for an id that is not a real section returns `false`
/// and writes nothing, checked BEFORE the `UPDATE` - same "refuse before
/// touching the row" posture `false` for an unknown bot id already has.
pub fn move_bot(db: &Db, bot_id: &str, section_id: Option<&str>) -> rusqlite::Result<bool> {
    if get_bot(db, bot_id)?.is_none() {
        return Ok(false);
    }
    if let Some(sid) = section_id {
        let exists: Option<String> = db
            .conn()
            .query_row(
                "SELECT id FROM sections WHERE id = ?1",
                params![sid],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Ok(false);
        }
    }
    db.conn().execute(
        "UPDATE bots SET section_id = ?1 WHERE id = ?2",
        params![section_id, bot_id],
    )?;
    Ok(true)
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
///
/// IMPORT-01: `pub`, and re-exported from `crate::lib`, so
/// `server::routes::import` can compute the id a name WOULD get (its own
/// duplicate-name check) without a second copy of this logic drifting from
/// `slug_for`'s own base below.
pub fn slug_base(name: &str) -> String {
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

/// RAIL-03: sets or clears a bot's avatar override - port of
/// `roster.ts:98-104`'s `setAvatar`. `false` for an unknown bot; nothing is
/// written in that case, same posture as `move_bot` above. `None`, or a
/// trimmed value that collapses to nothing (all whitespace), stores NULL.
///
/// **Kept to the first two CODE POINTS, never two bytes**: two is enough
/// for one emoji plus a variation selector or a skin-tone modifier -
/// anything longer is a label, not an avatar - and `chars().take(2)` is the
/// only way to take that cap that does not panic. A byte slice (`&clean[..2]`)
/// would cut a multi-byte emoji in half the moment its boundary lands
/// mid-character; `crates/server/tests/bots.rs` has a multi-byte case for
/// exactly this.
pub fn set_avatar(db: &Db, id: &str, avatar: Option<&str>) -> rusqlite::Result<bool> {
    if get_bot(db, id)?.is_none() {
        return Ok(false);
    }
    let clean: Option<String> = avatar
        .map(|a| a.trim().chars().take(2).collect::<String>())
        .filter(|s| !s.is_empty());
    db.conn().execute(
        "UPDATE bots SET avatar = ?1 WHERE id = ?2",
        params![clean, id],
    )?;
    Ok(true)
}

/// RAIL-03: sets or clears which silhouette a bot's generated face wears -
/// port of `roster.ts:112-123`'s `setShape`. `false` for an unknown bot.
///
/// A shape that is not a key in `shared::faces::SHAPES` stores NULL rather
/// than being refused, matching the TS `Object.hasOwn(SHAPES, shape)`
/// check exactly - an unrecognised shape is silently treated as "no
/// preference" (`shared::faces::normalize_shape` then hashes a default from
/// the name), not a 400. The membership check reads `SHAPES` itself - the
/// same table `crates/client/src/avatar.rs` already renders the picker and
/// the face from - rather than a second hardcoded list of names, which
/// would drift the moment a shape is added to one and not the other.
pub fn set_shape(db: &Db, id: &str, shape: Option<&str>) -> rusqlite::Result<bool> {
    if get_bot(db, id)?.is_none() {
        return Ok(false);
    }
    let clean = shape.filter(|s| shared::faces::SHAPES.iter().any(|(k, _)| k == s));
    db.conn().execute(
        "UPDATE bots SET shape = ?1 WHERE id = ?2",
        params![clean, id],
    )?;
    Ok(true)
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
