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
use chrono::Utc;
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

/// Scope of a memory entry: own (bot's private), project (shared within a project), or shared (visible to all).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Own,
    Project,
    Shared,
}

impl Scope {
    /// Convert to SQL string representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Own => "own",
            Scope::Project => "project",
            Scope::Shared => "shared",
        }
    }

    /// Parse from SQL string representation.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "own" => Some(Scope::Own),
            "project" => Some(Scope::Project),
            "shared" => Some(Scope::Shared),
            _ => None,
        }
    }
}

/// A project that bots can be members of for scoped memory.
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    pub id: String,
    pub name: String,
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
        "INSERT INTO memory_log (id, bot_id, content, source, created_at, kind, scope) VALUES (?1, ?2, ?3, ?4, ?5, 'log', 'own')",
        params![entry.id, bot_id, entry.content, entry.source, entry.created_at],
    )?;
    Ok(entry)
}

/// Appends a note (temporary memory entry) to a bot's memory log with a TTL.
pub fn note(db: &Db, bot_id: &str, content: &str, ttl_secs: u64) -> rusqlite::Result<LogEntry> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now();
    let now_iso = now.to_rfc3339();
    let expires_at = if ttl_secs == 0 {
        // Already expired
        now_iso.clone()
    } else {
        let expires_time = now + chrono::Duration::seconds(ttl_secs as i64);
        expires_time.to_rfc3339()
    };

    let entry = LogEntry {
        id: id.clone(),
        content: content.trim().to_string(),
        source: "bot".to_string(),
        created_at: now_iso,
    };

    db.conn().execute(
        "INSERT INTO memory_log (id, bot_id, content, source, created_at, kind, expires_at, scope) VALUES (?1, ?2, ?3, ?4, ?5, 'note', ?6, 'own')",
        params![entry.id, bot_id, entry.content, entry.source, entry.created_at, expires_at],
    )?;
    Ok(entry)
}

/// Appends a scoped memory entry to a bot's memory log.
pub fn remember_scoped(
    db: &Db,
    bot_id: &str,
    content: &str,
    scope: Scope,
    project_id: Option<&str>,
) -> rusqlite::Result<LogEntry> {
    let entry = LogEntry {
        id: uuid::Uuid::new_v4().to_string(),
        content: content.trim().to_string(),
        source: "bot".to_string(),
        created_at: now_iso(),
    };
    db.conn().execute(
        "INSERT INTO memory_log (id, bot_id, content, source, created_at, kind, scope, project_id) VALUES (?1, ?2, ?3, ?4, ?5, 'log', ?6, ?7)",
        params![entry.id, bot_id, entry.content, entry.source, entry.created_at, scope.as_str(), project_id],
    )?;
    Ok(entry)
}

/// Removes expired notes from the memory log. Returns the count of deleted entries.
pub fn sweep_expired(db: &Db) -> rusqlite::Result<usize> {
    let now = now_iso();
    let changes = db.conn().execute(
        "DELETE FROM memory_log WHERE kind = 'note' AND expires_at IS NOT NULL AND expires_at <= ?1",
        params![now],
    )?;
    Ok(changes)
}

/// Creates a new project.
pub fn create_project(db: &Db, name: &str) -> rusqlite::Result<Project> {
    let id = uuid::Uuid::new_v4().to_string();
    let created_at = now_iso();
    let project = Project {
        id: id.clone(),
        name: name.to_string(),
        created_at: created_at.clone(),
    };
    db.conn().execute(
        "INSERT INTO projects (id, name, created_at) VALUES (?1, ?2, ?3)",
        params![project.id, project.name, project.created_at],
    )?;
    Ok(project)
}

/// Adds a bot as a member of a project.
pub fn add_project_member(db: &Db, project_id: &str, bot_id: &str) -> rusqlite::Result<()> {
    db.conn().execute(
        "INSERT OR IGNORE INTO project_members (project_id, bot_id) VALUES (?1, ?2)",
        params![project_id, bot_id],
    )?;
    Ok(())
}

/// Gets all projects a bot is a member of.
pub fn projects_for(db: &Db, bot_id: &str) -> rusqlite::Result<Vec<Project>> {
    let mut stmt = db.conn().prepare(
        "SELECT p.id, p.name, p.created_at FROM projects p
          JOIN project_members pm ON p.id = pm.project_id
          WHERE pm.bot_id = ?1
          ORDER BY p.created_at DESC",
    )?;
    let projects = stmt
        .query_map(params![bot_id], |row| {
            Ok(Project {
                id: row.get(0)?,
                name: row.get(1)?,
                created_at: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(projects)
}

/// The most recent `limit` entries for a bot, newest first. Mirrors the TS
/// `recentLog`.
pub fn recent_log(db: &Db, bot_id: &str, limit: i64) -> rusqlite::Result<Vec<LogEntry>> {
    let mut stmt = db.conn().prepare(
        "SELECT id, content, source, created_at FROM memory_log
          WHERE bot_id = ?1 ORDER BY created_at DESC, rowid DESC LIMIT ?2",
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

/// Get entries from a specific scope for recall purposes, excluding expired
/// notes. `pub`: S3-03's tiered prompt recall needs this split by tier
/// (own/project/shared, one header each) which `recall_for`'s flat
/// `Vec<LogEntry>` cannot express since `LogEntry` carries no `scope` or
/// `project_id` of its own.
pub fn scoped_entries(
    db: &Db,
    bot_id: &str,
    scope: Scope,
    project_ids: &[String],
    limit: i64,
) -> rusqlite::Result<Vec<LogEntry>> {
    let now = now_iso();
    match scope {
        Scope::Own => {
            let mut stmt = db.conn().prepare(
                "SELECT id, content, source, created_at FROM memory_log
                  WHERE bot_id = ?1 AND scope = 'own' AND (expires_at IS NULL OR expires_at > ?2)
                  ORDER BY created_at DESC, rowid DESC LIMIT ?3",
            )?;
            let rows = stmt
                .query_map(params![bot_id, now, limit], |row| {
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
        Scope::Project => {
            if project_ids.is_empty() {
                return Ok(Vec::new());
            }
            let placeholders = project_ids
                .iter()
                .map(|_| "?")
                .collect::<Vec<_>>()
                .join(",");
            let query = format!(
                "SELECT id, content, source, created_at FROM memory_log
                  WHERE scope = 'project' AND project_id IN ({}) AND (expires_at IS NULL OR expires_at > ?)
                  ORDER BY created_at DESC, rowid DESC LIMIT ?",
                placeholders
            );
            let mut stmt = db.conn().prepare(&query)?;
            let mut params: Vec<&dyn rusqlite::ToSql> = project_ids
                .iter()
                .map(|id| id as &dyn rusqlite::ToSql)
                .collect();
            params.push(&now);
            params.push(&limit);
            let rows = stmt
                .query_map(rusqlite::params_from_iter(params.iter()), |row| {
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
        Scope::Shared => {
            let mut stmt = db.conn().prepare(
                "SELECT id, content, source, created_at FROM memory_log
                  WHERE scope = 'shared' AND (expires_at IS NULL OR expires_at > ?1)
                  ORDER BY created_at DESC, rowid DESC LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![now, limit], |row| {
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
    }
}

/// How many entries exist in one scope (ignoring `scoped_entries`' `limit`)
/// so a caller rendering that tier can report an accurate "N older" count.
/// Same filters as `scoped_entries`, as a `COUNT(*)` instead of a `SELECT`.
pub fn count_scoped(
    db: &Db,
    bot_id: &str,
    scope: Scope,
    project_ids: &[String],
) -> rusqlite::Result<i64> {
    let now = now_iso();
    match scope {
        Scope::Own => db.conn().query_row(
            "SELECT COUNT(*) FROM memory_log
              WHERE bot_id = ?1 AND scope = 'own' AND (expires_at IS NULL OR expires_at > ?2)",
            params![bot_id, now],
            |row| row.get(0),
        ),
        Scope::Project => {
            if project_ids.is_empty() {
                return Ok(0);
            }
            let placeholders = project_ids
                .iter()
                .map(|_| "?")
                .collect::<Vec<_>>()
                .join(",");
            let query = format!(
                "SELECT COUNT(*) FROM memory_log
                  WHERE scope = 'project' AND project_id IN ({}) AND (expires_at IS NULL OR expires_at > ?)",
                placeholders
            );
            let mut params: Vec<&dyn rusqlite::ToSql> = project_ids
                .iter()
                .map(|id| id as &dyn rusqlite::ToSql)
                .collect();
            params.push(&now);
            db.conn()
                .query_row(&query, rusqlite::params_from_iter(params.iter()), |row| {
                    row.get(0)
                })
        }
        Scope::Shared => db.conn().query_row(
            "SELECT COUNT(*) FROM memory_log
              WHERE scope = 'shared' AND (expires_at IS NULL OR expires_at > ?1)",
            params![now],
            |row| row.get(0),
        ),
    }
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
/// implying this is everything. Implements precedence: own > project > shared,
/// newest first within each tier. Filters out expired notes (swept on read).
pub fn recall_for(db: &Db, bot_id: &str, budget: i64) -> rusqlite::Result<Recall> {
    // Get the bot's project IDs
    let projects = projects_for(db, bot_id)?;
    let project_ids: Vec<String> = projects.iter().map(|p| p.id.clone()).collect();

    // Collect entries by tier, newest first in each tier.
    // Note: we extend in reverse tier order (shared, project, own) so that when we
    // reverse the entire list at the end, the tiers come out in the right order
    // (own, project, shared) with oldest entries first within each tier.
    let mut candidates: Vec<LogEntry> = Vec::new();

    // Tier 3: Shared entries (most recent 40) - added first for reverse
    let shared_entries = scoped_entries(db, bot_id, Scope::Shared, &[], 40)?;
    candidates.extend(shared_entries);

    // Tier 2: Project entries (most recent 40)
    let project_entries = scoped_entries(db, bot_id, Scope::Project, &project_ids, 40)?;
    candidates.extend(project_entries);

    // Tier 1: Own entries (most recent 40) - added last for reverse
    let own_entries = scoped_entries(db, bot_id, Scope::Own, &[], 40)?;
    candidates.extend(own_entries);

    // Trim by token budget
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

/// Full-text search over a bot's memory by scope. FTS5 treats punctuation as query
/// syntax, so a raw question is a syntax error rather than a search -
/// splitting on non-alphanumerics is what prevents that. Filters by scopes
/// and excludes expired notes.
pub fn search_log(
    db: &Db,
    bot_id: &str,
    query: &str,
    scopes: &[Scope],
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
    let now = now_iso();

    // For Own scope: filter by bot_id and scope='own'
    // For Project scope: filter by project membership (bot is a member)
    // For Shared scope: filter by scope='shared' (anyone can see)
    let has_own = scopes.contains(&Scope::Own);
    let has_project = scopes.contains(&Scope::Project);
    let has_shared = scopes.contains(&Scope::Shared);

    let mut filters = Vec::new();
    filters.push("(l.expires_at IS NULL OR l.expires_at > ?)".to_string());

    if has_own && !has_project && !has_shared {
        // Only own scope: simple filter
        filters.push("(l.bot_id = ? AND l.scope = 'own')".to_string());
        let query_str = format!(
            "SELECT l.id, l.content, l.source, l.created_at
               FROM memory_fts f
               JOIN memory_log l ON l.rowid = f.rowid
              WHERE memory_fts MATCH ?
                AND {}
              ORDER BY rank
              LIMIT ?",
            filters.join(" AND ")
        );
        let mut stmt = db.conn().prepare(&query_str)?;
        let rows = stmt
            .query_map(params![match_query, now, bot_id, limit], |row| {
                Ok(LogEntry {
                    id: row.get(0)?,
                    content: row.get(1)?,
                    source: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    } else if has_project && !has_own && !has_shared {
        // Only project scope: filter by project membership
        let projects = projects_for(db, bot_id)?;
        if projects.is_empty() {
            return Ok(Vec::new());
        }
        let project_placeholders = projects.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        filters.push(format!(
            "(l.scope = 'project' AND l.project_id IN ({}))",
            project_placeholders
        ));
        let query_str = format!(
            "SELECT l.id, l.content, l.source, l.created_at
               FROM memory_fts f
               JOIN memory_log l ON l.rowid = f.rowid
              WHERE memory_fts MATCH ?
                AND {}
              ORDER BY rank
              LIMIT ?",
            filters.join(" AND ")
        );
        let mut stmt = db.conn().prepare(&query_str)?;
        let mut params: Vec<&dyn rusqlite::ToSql> = vec![&match_query, &now];
        for proj in &projects {
            params.push(&proj.id);
        }
        params.push(&limit);
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                Ok(LogEntry {
                    id: row.get(0)?,
                    content: row.get(1)?,
                    source: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    } else if has_shared && !has_own && !has_project {
        // Only shared scope
        filters.push("l.scope = 'shared'".to_string());
        let query_str = format!(
            "SELECT l.id, l.content, l.source, l.created_at
               FROM memory_fts f
               JOIN memory_log l ON l.rowid = f.rowid
              WHERE memory_fts MATCH ?
                AND {}
              ORDER BY rank
              LIMIT ?",
            filters.join(" AND ")
        );
        let mut stmt = db.conn().prepare(&query_str)?;
        let rows = stmt
            .query_map(params![match_query, now, limit], |row| {
                Ok(LogEntry {
                    id: row.get(0)?,
                    content: row.get(1)?,
                    source: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    } else {
        // Multiple scopes combined - need a more complex query
        // Collect project IDs first to avoid lifetime issues
        let project_ids = if has_project {
            projects_for(db, bot_id)?
                .into_iter()
                .map(|p| p.id)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        // Build scope conditions
        let mut scope_conditions = Vec::new();
        if has_own {
            scope_conditions.push("(l.bot_id = ? AND l.scope = 'own')".to_string());
        }
        if has_project && !project_ids.is_empty() {
            let placeholders = project_ids
                .iter()
                .map(|_| "?")
                .collect::<Vec<_>>()
                .join(",");
            scope_conditions.push(format!(
                "(l.scope = 'project' AND l.project_id IN ({}))",
                placeholders
            ));
        }
        if has_shared {
            scope_conditions.push("(l.scope = 'shared')".to_string());
        }

        if scope_conditions.is_empty() {
            return Ok(Vec::new());
        }

        let scope_filter = format!("({})", scope_conditions.join(" OR "));
        let query_str = format!(
            "SELECT l.id, l.content, l.source, l.created_at
               FROM memory_fts f
               JOIN memory_log l ON l.rowid = f.rowid
              WHERE memory_fts MATCH ?
                AND (l.expires_at IS NULL OR l.expires_at > ?)
                AND {}
              ORDER BY rank
              LIMIT ?",
            scope_filter
        );
        let mut stmt = db.conn().prepare(&query_str)?;
        let mut params: Vec<&dyn rusqlite::ToSql> = vec![&match_query, &now];
        if has_own {
            params.push(&bot_id);
        }
        for proj_id in &project_ids {
            params.push(proj_id);
        }
        params.push(&limit);
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params.iter()), |row| {
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
}

/// Deletes a log entry by ID for a given bot. Returns true if an entry was deleted,
/// false if the entry does not exist or belongs to a different bot.
pub fn forget(db: &Db, bot_id: &str, entry_id: &str) -> rusqlite::Result<bool> {
    let changes = db.conn().execute(
        "DELETE FROM memory_log WHERE id = ?1 AND bot_id = ?2",
        params![entry_id, bot_id],
    )?;
    Ok(changes > 0)
}

/// Gets the shared core text from settings. Empty string if not set.
pub fn get_shared_core(db: &Db) -> rusqlite::Result<String> {
    const SHARED_CORE_KEY: &str = "memory.shared_core";
    let core: Option<String> = db
        .conn()
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![SHARED_CORE_KEY],
            |row| row.get(0),
        )
        .optional()?;
    Ok(core.unwrap_or_default())
}

/// Sets the shared core text in settings.
pub fn set_shared_core(db: &Db, core: &str) -> rusqlite::Result<()> {
    const SHARED_CORE_KEY: &str = "memory.shared_core";
    db.conn().execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![SHARED_CORE_KEY, core],
    )?;
    Ok(())
}

/// Gets the most recent entries from the shared scope (newest first).
pub fn recent_shared_log(db: &Db, limit: i64) -> rusqlite::Result<Vec<LogEntry>> {
    let now = now_iso();
    let mut stmt = db.conn().prepare(
        "SELECT id, content, source, created_at FROM memory_log
          WHERE scope = 'shared' AND (expires_at IS NULL OR expires_at > ?)
          ORDER BY created_at DESC, rowid DESC LIMIT ?",
    )?;
    let rows = stmt
        .query_map(params![now, limit], |row| {
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
        let hits = search_log(
            &db,
            "t",
            "What about \"Zenith 14\" -- the main event?",
            &[Scope::Own],
            8,
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn search_log_keeps_one_bots_memory_out_of_anothers() {
        let db = Db::open(":memory:").unwrap();
        seed_bot(&db, "t");
        seed_bot(&db, "u");
        remember(&db, "t", "A secret only T knows about zenith.", "bot").unwrap();
        assert_eq!(
            search_log(&db, "u", "zenith", &[Scope::Own], 8)
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            search_log(&db, "t", "zenith", &[Scope::Own], 8)
                .unwrap()
                .len(),
            1
        );
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
