//! Tiered memory. Port of the TS `memory.ts`'s `getCore`/`setCore`,
//! `remember`, `recallFor`, `searchLog`.
//!
//! A small **core** rides in every request, at the very front of the prompt
//! and byte-identical between turns, so a bot cannot forget who it is and
//! the provider can cache the prefix. Everything else lives in a searchable
//! **log** the bot queries as a tool - `recall_for` is the exception: it
//! surfaces the newest of that log uninvited, because a bot that has to
//! decide to look something up can decide not to.

use crate::Db;
use crate::conversations::now_iso;
use rusqlite::{OptionalExtension, params};

/// One entry in a bot's memory log.
#[derive(Debug, Clone, PartialEq)]
pub struct LogEntry {
    pub id: String,
    pub content: String,
    /// `"bot"` (written during a run) or `"josh"` (written by hand).
    pub source: String,
    pub created_at: String,
}

/// Roughly four characters per token. Enough to police a budget, not a
/// billing system.
fn approx_tokens(text: &str) -> i64 {
    (text.len() as i64 + 3) / 4
}

/// How much recent memory rides along in every prompt, uninvited. Separate
/// from the (later) core budget and larger - the core is hand-written
/// identity, this is "what has been going on".
pub const RECALL_TOKEN_BUDGET: i64 = 1200;

/// `SELECT memory_core FROM bots WHERE id = ?`. Empty string when the bot has
/// no core, or does not exist.
pub fn get_core(db: &Db, bot_id: &str) -> rusqlite::Result<String> {
    let core: Option<String> = db
        .conn()
        .query_row(
            "SELECT memory_core FROM bots WHERE id = ?1",
            params![bot_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(core.unwrap_or_default())
}

/// `UPDATE bots SET memory_core = ? WHERE id = ?`.
pub fn set_core(db: &Db, bot_id: &str, core: &str) -> rusqlite::Result<()> {
    db.conn().execute(
        "UPDATE bots SET memory_core = ?1 WHERE id = ?2",
        params![core, bot_id],
    )?;
    Ok(())
}

/// Appends one entry to a bot's memory log. Mirrors the TS `remember`
/// (without W7's evidence/run-id tagging - not in this schema yet).
pub fn remember(db: &Db, bot_id: &str, content: &str, source: &str) -> rusqlite::Result<LogEntry> {
    let entry = LogEntry {
        id: uuid::Uuid::new_v4().to_string(),
        content: content.trim().to_string(),
        source: source.to_string(),
        created_at: now_iso(),
    };
    db.conn().execute(
        "INSERT INTO memory_log (id, bot_id, content, source, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![entry.id, bot_id, entry.content, entry.source, entry.created_at],
    )?;
    Ok(entry)
}

/// The most recent `limit` entries for a bot, newest first. Mirrors the TS
/// `recentLog`.
fn recent_log(db: &Db, bot_id: &str, limit: i64) -> rusqlite::Result<Vec<LogEntry>> {
    let mut stmt = db.conn().prepare(
        "SELECT id, content, source, created_at FROM memory_log
          WHERE bot_id = ?1 ORDER BY created_at DESC LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![bot_id, limit], |row| {
            Ok(LogEntry {
                id: row.get(0)?,
                content: row.get(1)?,
                source: row.get(2)?,
                created_at: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// `SELECT COUNT(*) FROM memory_log WHERE bot_id = ?`.
fn count_log(db: &Db, bot_id: &str) -> rusqlite::Result<i64> {
    db.conn().query_row(
        "SELECT COUNT(*) FROM memory_log WHERE bot_id = ?1",
        params![bot_id],
        |row| row.get(0),
    )
}

/// What `recall_for` hands back: the newest entries that fit the budget,
/// oldest first (a prompt reads better that way), and how many more exist
/// that did not fit.
#[derive(Debug, Clone, PartialEq)]
pub struct Recall {
    pub entries: Vec<LogEntry>,
    pub older: i64,
}

/// The most recent memory a bot gets without asking. Newest entries win,
/// oldest are dropped, and the count of what was dropped comes back too, so
/// the prompt can tell the bot there is more to search for rather than
/// implying this is everything. Mirrors the TS `recallFor`.
pub fn recall_for(db: &Db, bot_id: &str, budget: i64) -> rusqlite::Result<Recall> {
    // A generous window, then trimmed by real token cost rather than by
    // count: entries vary from one line to several paragraphs.
    let candidates = recent_log(db, bot_id, 120)?;

    let mut kept: Vec<LogEntry> = Vec::new();
    let mut spent: i64 = 0;
    for entry in &candidates {
        let cost = approx_tokens(&entry.content) + 2;
        if spent + cost > budget {
            break;
        }
        kept.push(entry.clone());
        spent += cost;
    }

    // A single entry can be bigger than the whole budget. Without this the
    // loop keeps nothing at all and the bot is told nothing, which is the
    // same amnesia this function exists to fix. Take the newest one, clipped.
    if kept.is_empty()
        && let Some(newest) = candidates.first()
    {
        let clip = (budget * 4).max(0) as usize;
        let mut clipped = newest.clone();
        let truncated: String = clipped.content.chars().take(clip).collect();
        clipped.content = format!("{truncated}\n[...truncated, search_memory for the rest]");
        kept.push(clipped);
    }

    let total = count_log(db, bot_id)?;
    let older = (total - kept.len() as i64).max(0);

    // recent_log returns newest first; a prompt reads better oldest first.
    kept.reverse();
    Ok(Recall {
        entries: kept,
        older,
    })
}

/// Full-text search over one bot's log. FTS5 treats punctuation as query
/// syntax, so a raw question is a syntax error rather than a search -
/// splitting on non-alphanumerics is what prevents that. Mirrors the TS
/// `searchLog`.
pub fn search_log(
    db: &Db,
    bot_id: &str,
    query: &str,
    limit: i64,
) -> rusqlite::Result<Vec<LogEntry>> {
    let terms: Vec<String> = query
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() > 1)
        .map(str::to_string)
        .collect();
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let match_query = terms.join(" OR ");

    let mut stmt = db.conn().prepare(
        "SELECT l.id, l.content, l.source, l.created_at
           FROM memory_fts f
           JOIN memory_log l ON l.rowid = f.rowid
          WHERE memory_fts MATCH ?1
            AND l.bot_id = ?2
          ORDER BY rank
          LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(params![match_query, bot_id, limit], |row| {
            Ok(LogEntry {
                id: row.get(0)?,
                content: row.get(1)?,
                source: row.get(2)?,
                created_at: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;

    fn seed_bot(db: &Db, id: &str) {
        db.conn()
            .execute(
                "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES (?1, '', '', '', NULL, ?2)",
                params![id, now_iso()],
            )
            .unwrap();
    }

    #[test]
    fn core_round_trips() {
        let db = Db::open(":memory:").unwrap();
        seed_bot(&db, "t");
        assert_eq!(get_core(&db, "t").unwrap(), "");
        set_core(&db, "t", "Josh is the owner.").unwrap();
        assert_eq!(get_core(&db, "t").unwrap(), "Josh is the owner.");
    }

    #[test]
    fn search_log_survives_punctuation() {
        let db = Db::open(":memory:").unwrap();
        seed_bot(&db, "t");
        remember(&db, "t", "Zenith 14 main event ends on a countout.", "bot").unwrap();
        let hits = search_log(&db, "t", "What about \"Zenith 14\" -- the main event?", 8).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn search_log_keeps_one_bots_memory_out_of_anothers() {
        let db = Db::open(":memory:").unwrap();
        seed_bot(&db, "t");
        seed_bot(&db, "u");
        remember(&db, "t", "A secret only T knows about zenith.", "bot").unwrap();
        assert_eq!(search_log(&db, "u", "zenith", 8).unwrap().len(), 0);
        assert_eq!(search_log(&db, "t", "zenith", 8).unwrap().len(), 1);
    }

    #[test]
    fn recall_for_reports_older_count() {
        let db = Db::open(":memory:").unwrap();
        seed_bot(&db, "t");
        // ~200 tokens (~800 chars) each, so the 1200-token budget cannot hold
        // all 30 - same shape as the ticket's acceptance case.
        let filler = "x".repeat(800);
        for i in 0..30 {
            remember(&db, "t", &format!("note {i} {filler}"), "bot").unwrap();
        }
        let recall = recall_for(&db, "t", RECALL_TOKEN_BUDGET).unwrap();
        assert!(!recall.entries.is_empty());
        assert!(recall.older > 0);
        assert_eq!(recall.entries.len() as i64 + recall.older, 30);
    }
}
