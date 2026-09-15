use crate::Db;
use chrono::Utc;
use rusqlite::OptionalExtension;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Raw row structure from the routines table.
#[derive(Clone, Debug)]
pub struct RoutineRow {
    pub id: String,
    pub bot_id: String,
    pub name: String,
    pub prompt: String,
    pub schedule: String,
    pub active: i32,
    pub next_run_at: Option<String>,
    pub last_run_at: Option<String>,
    pub tools: Option<String>,
    pub kind: String,
    pub tool: Option<String>,
    pub tool_args: Option<String>,
    pub hook_secret: Option<String>,
    pub hook_kind: String,
    pub hook_events: Option<String>,
    pub hook_match: Option<String>,
    pub conditions: Option<String>,
    pub second_opinion: i32,
    pub consecutive_failures: i32,
    pub paused_reason: Option<String>,
    pub last_error: Option<String>,
}

/// Condition for hook triggering.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Condition {
    pub kind: String, // "github" | "sentry" | "linear" | "pagerduty" | "raw"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_: Option<String>,
}

/// Routine structure with camelCase fields for JSON serialization.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Routine {
    pub id: String,
    pub bot_id: String,
    pub bot_name: String,
    pub name: String,
    pub prompt: String,
    /// S5-F-02 (F3): holds whatever the caller stored - after F3 lands in
    /// `crates/server/src/routes/routines.rs`, that's the TS JSON shape
    /// (`{"kind":"interval","minutes":15}`), not a typed phrase. Store
    /// itself never parses or validates this text (it must not depend on
    /// `server::schedule` - see `S5-tickets.md`'s Design section); the
    /// route layer decodes it and injects a parsed `schedule` object plus a
    /// computed `scheduleText` into the wire JSON on the way out (see
    /// `routes/routines.rs::routine_wire_json`). There is deliberately no
    /// `schedule_text` field here any more - the old stub always serialized
    /// `""` (a TODO, never wired up); computing it needs `server::schedule`,
    /// which this crate cannot reach.
    pub schedule: String,
    pub active: bool,
    pub next_run_at: Option<String>,
    pub last_run_at: Option<String>,
    pub paused_reason: Option<String>,
    pub last_error: Option<String>,
    pub failures: i32,
    pub tools: Option<Vec<String>>,
    pub kind: String,
    pub tool: Option<String>,
    pub tool_args: Option<String>,
    pub has_hook: bool,
    pub hook_kind: String,
    pub hook_events: Option<Vec<String>>,
    pub hook_match: Option<String>,
    pub conditions: Option<Vec<Condition>>,
    pub second_opinion: bool,
}

/// Represents a routine run entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutineRun {
    pub id: String,
    pub status: String,
    pub text: String,
    pub error: Option<String>,
    pub cost_usd: f64,
    pub created_at: String,
}

/// Health outcome from recording a routine run.
#[derive(Clone, Debug)]
pub struct HealthOutcome {
    pub failures: i32,
    pub paused: bool,
}

/// Ensures all routine-related columns exist. Called from lib.rs during Db::open.
pub(crate) fn ensure_routine_columns(db: &Db) -> rusqlite::Result<()> {
    // Check which columns already exist
    let columns: Vec<String> = {
        let mut stmt = db.conn().prepare("PRAGMA table_info(routines)")?;
        stmt.query_map([], |row| row.get::<_, String>(1))?
            .filter_map(Result::ok)
            .collect()
    };

    let has = |name: &str| columns.contains(&name.to_string());

    // Add columns if they don't exist (idempotent, like TS ensures)
    if !has("tools") {
        db.conn()
            .execute("ALTER TABLE routines ADD COLUMN tools TEXT", [])?;
    }
    if !has("kind") {
        db.conn().execute(
            "ALTER TABLE routines ADD COLUMN kind TEXT NOT NULL DEFAULT 'prompt'",
            [],
        )?;
    }
    if !has("tool") {
        db.conn()
            .execute("ALTER TABLE routines ADD COLUMN tool TEXT", [])?;
    }
    if !has("tool_args") {
        db.conn()
            .execute("ALTER TABLE routines ADD COLUMN tool_args TEXT", [])?;
    }
    if !has("hook_secret") {
        db.conn()
            .execute("ALTER TABLE routines ADD COLUMN hook_secret TEXT", [])?;
    }
    if !has("hook_kind") {
        db.conn().execute(
            "ALTER TABLE routines ADD COLUMN hook_kind TEXT NOT NULL DEFAULT 'raw'",
            [],
        )?;
    }
    if !has("hook_events") {
        db.conn()
            .execute("ALTER TABLE routines ADD COLUMN hook_events TEXT", [])?;
    }
    if !has("hook_match") {
        db.conn()
            .execute("ALTER TABLE routines ADD COLUMN hook_match TEXT", [])?;
    }
    if !has("conditions") {
        db.conn()
            .execute("ALTER TABLE routines ADD COLUMN conditions TEXT", [])?;
    }
    if !has("consecutive_failures") {
        db.conn().execute(
            "ALTER TABLE routines ADD COLUMN consecutive_failures INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !has("paused_reason") {
        db.conn()
            .execute("ALTER TABLE routines ADD COLUMN paused_reason TEXT", [])?;
    }
    if !has("last_error") {
        db.conn()
            .execute("ALTER TABLE routines ADD COLUMN last_error TEXT", [])?;
    }
    if !has("second_opinion") {
        db.conn().execute(
            "ALTER TABLE routines ADD COLUMN second_opinion INTEGER NOT NULL DEFAULT 1",
            [],
        )?;
    }

    // Ensure hook_arrivals table exists
    db.conn().execute(
        "CREATE TABLE IF NOT EXISTS hook_arrivals (
            id TEXT PRIMARY KEY,
            routine_id TEXT NOT NULL,
            kind TEXT NOT NULL,
            arrived_at TEXT NOT NULL,
            reduced_text TEXT NOT NULL,
            FOREIGN KEY (routine_id) REFERENCES routines(id) ON DELETE CASCADE
        )",
        [],
    )?;

    Ok(())
}

/// Parse a JSON string as a vector of strings (tool names).
fn parse_tools(stored: Option<&str>) -> Option<Vec<String>> {
    stored.and_then(|s| serde_json::from_str(s).ok())
}

/// Parse a JSON string as a vector of conditions.
fn parse_conditions(stored: Option<&str>) -> Option<Vec<Condition>> {
    stored.and_then(|s| serde_json::from_str(s).ok())
}

/// Parse a JSON string as a vector of hook events.
fn parse_hook_events(stored: Option<&str>) -> Option<Vec<String>> {
    stored.and_then(|s| serde_json::from_str(s).ok())
}

/// List all routines for a given bot (or all if bot_id is None).
pub fn list_routines(db: &Db, bot_id: Option<&str>) -> rusqlite::Result<Vec<Routine>> {
    let base = "SELECT id, bot_id, name, prompt, schedule, active, next_run_at, last_run_at,
                tools, kind, tool, tool_args, hook_secret, hook_kind, hook_events, hook_match,
                conditions, second_opinion, consecutive_failures, paused_reason, last_error
         FROM routines";

    let query = if bot_id.is_some() {
        format!("{} WHERE bot_id = ?1 ORDER BY created_at", base)
    } else {
        format!("{} ORDER BY created_at", base)
    };

    let mut stmt = db.conn().prepare(&query)?;

    let routines: Vec<Routine> = if let Some(bid) = bot_id {
        stmt.query_map(params![bid], |row| routine_from_row(db, row))?
            .collect::<Result<Vec<_>, _>>()?
    } else {
        stmt.query_map([], |row| routine_from_row(db, row))?
            .collect::<Result<Vec<_>, _>>()?
    };

    Ok(routines)
}

/// Get a single routine by ID.
/// Get a routine row by ID, including the raw hook_secret field.
pub fn routine_row_by_id(db: &Db, id: &str) -> rusqlite::Result<Option<RoutineRow>> {
    db.conn()
        .prepare(
            "SELECT id, bot_id, name, prompt, schedule, active, next_run_at, last_run_at, tools, kind, tool, tool_args, hook_secret, hook_kind, hook_events, hook_match, conditions, second_opinion, consecutive_failures, paused_reason, last_error FROM routines WHERE id = ?1",
        )?
        .query_row([id], |row| {
            Ok(RoutineRow {
                id: row.get(0)?,
                bot_id: row.get(1)?,
                name: row.get(2)?,
                prompt: row.get(3)?,
                schedule: row.get(4)?,
                active: row.get(5)?,
                next_run_at: row.get(6)?,
                last_run_at: row.get(7)?,
                tools: row.get(8)?,
                kind: row.get(9)?,
                tool: row.get(10)?,
                tool_args: row.get(11)?,
                hook_secret: row.get(12)?,
                hook_kind: row.get(13)?,
                hook_events: row.get(14)?,
                hook_match: row.get(15)?,
                conditions: row.get(16)?,
                second_opinion: row.get(17)?,
                consecutive_failures: row.get(18)?,
                paused_reason: row.get(19)?,
                last_error: row.get(20)?,
            })
        })
        .optional()
}

pub fn routine_by_id(db: &Db, id: &str) -> rusqlite::Result<Option<Routine>> {
    let mut stmt = db.conn().prepare(
        "SELECT id, bot_id, name, prompt, schedule, active, next_run_at, last_run_at,
                tools, kind, tool, tool_args, hook_secret, hook_kind, hook_events, hook_match,
                conditions, second_opinion, consecutive_failures, paused_reason, last_error
         FROM routines WHERE id = ?1",
    )?;

    let routine = stmt
        .query_row(params![id], |row| routine_from_row(db, row))
        .ok();

    Ok(routine)
}

/// Create a new routine.
#[allow(clippy::too_many_arguments)]
pub fn create_routine(
    db: &Db,
    bot_id: &str,
    name: &str,
    prompt: &str,
    schedule: String,
    next_run_at: Option<String>,
    tools: Option<Vec<String>>,
    kind: Option<&str>,
    tool: Option<&str>,
    tool_args: Option<&str>,
    hook_kind: Option<&str>,
    hook_events: Option<Vec<String>>,
    hook_match: Option<&str>,
    conditions: Option<Vec<Condition>>,
) -> Result<String, String> {
    // Check the 50-routine cap for this bot
    let count: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM routines WHERE bot_id = ?1",
            params![bot_id],
            |row| row.get(0),
        )
        .map_err(|e| format!("Failed to check routine count: {}", e))?;

    if count >= 50 {
        return Err("Each bot can have at most 50 routines".to_string());
    }

    let id = Uuid::new_v4().to_string();
    let kind_normalized = kind.unwrap_or("prompt");
    let hook_kind_normalized = hook_kind.unwrap_or("raw");
    let now = Utc::now().to_rfc3339();

    let tools_json = tools.map(|t| serde_json::to_string(&t).unwrap_or_default());
    let hook_events_json = hook_events.map(|e| serde_json::to_string(&e).unwrap_or_default());
    let conditions_json = conditions.map(|c| serde_json::to_string(&c).unwrap_or_default());

    db.conn()
        .execute(
            "INSERT INTO routines (id, bot_id, name, prompt, schedule, active, next_run_at, created_at,
                                   tools, kind, tool, tool_args, hook_kind, hook_events, hook_match, conditions)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                &id,
                bot_id,
                name,
                prompt,
                &schedule,
                next_run_at,
                &now,
                tools_json,
                kind_normalized,
                tool,
                tool_args,
                hook_kind_normalized,
                hook_events_json,
                hook_match,
                conditions_json,
            ],
        )
        .map_err(|e| format!("Failed to insert routine: {}", e))?;

    // Set has_routine on the bot
    db.conn()
        .execute(
            "UPDATE bots SET has_routine = 1 WHERE id = ?1",
            params![bot_id],
        )
        .map_err(|e| format!("Failed to update bot: {}", e))?;

    Ok(id)
}

/// Update an existing routine (all fields optional).
pub fn update_routine(db: &Db, id: &str, updates: &UpdateRoutineFields) -> rusqlite::Result<()> {
    // Build dynamic SQL based on which fields are set
    let mut set_clauses = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(n) = &updates.name {
        set_clauses.push("name = ?");
        params.push(Box::new(n.clone()));
    }
    if let Some(p) = &updates.prompt {
        set_clauses.push("prompt = ?");
        params.push(Box::new(p.clone()));
    }
    if let Some(s) = &updates.schedule {
        set_clauses.push("schedule = ?");
        params.push(Box::new(s.clone()));
    }
    if let Some(nra) = &updates.next_run_at {
        set_clauses.push("next_run_at = ?");
        params.push(Box::new(nra.clone()));
    }
    if let Some(t) = &updates.tools {
        let json = t
            .as_ref()
            .map(|tv| serde_json::to_string(tv).unwrap_or_default());
        set_clauses.push("tools = ?");
        params.push(Box::new(json));
    }
    if let Some(k) = &updates.kind {
        set_clauses.push("kind = ?");
        params.push(Box::new(k.clone()));
    }
    if let Some(tl) = &updates.tool {
        set_clauses.push("tool = ?");
        params.push(Box::new(tl.clone()));
    }
    if let Some(ta) = &updates.tool_args {
        set_clauses.push("tool_args = ?");
        params.push(Box::new(ta.clone()));
    }
    if let Some(hk) = &updates.hook_kind {
        set_clauses.push("hook_kind = ?");
        params.push(Box::new(hk.clone()));
    }
    if let Some(he) = &updates.hook_events {
        let json = he
            .as_ref()
            .map(|ev| serde_json::to_string(ev).unwrap_or_default());
        set_clauses.push("hook_events = ?");
        params.push(Box::new(json));
    }
    if let Some(hm) = &updates.hook_match {
        set_clauses.push("hook_match = ?");
        params.push(Box::new(hm.clone()));
    }
    if let Some(c) = &updates.conditions {
        let json = c
            .as_ref()
            .map(|cv| serde_json::to_string(cv).unwrap_or_default());
        set_clauses.push("conditions = ?");
        params.push(Box::new(json));
    }
    if let Some(so) = updates.second_opinion {
        set_clauses.push("second_opinion = ?");
        params.push(Box::new(if so { 1 } else { 0 }));
    }

    if !set_clauses.is_empty() {
        let query = format!(
            "UPDATE routines SET {} WHERE id = ?",
            set_clauses.join(", ")
        );
        params.push(Box::new(id.to_string()));

        // Convert params to references for execute
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let mut stmt = db.conn().prepare(&query)?;
        stmt.execute(param_refs.as_slice())?;
    }

    Ok(())
}

/// Fields that can be updated in a routine.
#[derive(Default)]
pub struct UpdateRoutineFields {
    pub name: Option<String>,
    pub prompt: Option<String>,
    pub schedule: Option<String>,
    pub next_run_at: Option<Option<String>>,
    pub tools: Option<Option<Vec<String>>>,
    pub kind: Option<String>,
    pub tool: Option<Option<String>>,
    pub tool_args: Option<Option<String>>,
    pub hook_kind: Option<String>,
    pub hook_events: Option<Option<Vec<String>>>,
    pub hook_match: Option<Option<String>>,
    pub conditions: Option<Option<Vec<Condition>>>,
    pub second_opinion: Option<bool>,
}

/// Set a routine's active status and optionally update next_run_at.
pub fn set_routine_active(
    db: &Db,
    id: &str,
    active: bool,
    next_run_at: Option<String>,
) -> rusqlite::Result<()> {
    db.conn().execute(
        "UPDATE routines SET active = ?1, next_run_at = ?2 WHERE id = ?3",
        params![if active { 1 } else { 0 }, next_run_at, id],
    )?;
    Ok(())
}

/// Delete a routine and update bot's has_routine flag if needed.
pub fn delete_routine(db: &Db, id: &str) -> rusqlite::Result<bool> {
    // Get the bot_id before deleting
    let mut stmt = db
        .conn()
        .prepare("SELECT bot_id FROM routines WHERE id = ?1")?;
    let bot_id: Option<String> = stmt.query_row(params![id], |row| row.get(0)).ok();

    if let Some(bid) = bot_id {
        db.conn()
            .execute("DELETE FROM routines WHERE id = ?1", params![id])?;

        // Check if bot still has routines
        let mut count_stmt = db
            .conn()
            .prepare("SELECT COUNT(*) FROM routines WHERE bot_id = ?1")?;
        let count: i64 = count_stmt.query_row(params![&bid], |row| row.get(0))?;

        if count == 0 {
            db.conn().execute(
                "UPDATE bots SET has_routine = 0 WHERE id = ?1",
                params![&bid],
            )?;
        }
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Get the last N runs for a routine.
pub fn routine_runs(db: &Db, routine_id: &str, limit: i32) -> rusqlite::Result<Vec<RoutineRun>> {
    let mut stmt = db.conn().prepare(
        "SELECT id, status, text, error, cost_usd, created_at FROM runs
         WHERE routine_id = ?1 ORDER BY created_at DESC LIMIT ?2",
    )?;

    let runs = stmt
        .query_map(params![routine_id, limit], |row| {
            Ok(RoutineRun {
                id: row.get(0)?,
                status: row.get(1)?,
                text: row.get(2)?,
                error: row.get(3)?,
                cost_usd: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(runs)
}

/// Get all active routines that are due to run now.
pub fn due_routines(db: &Db, now: &str) -> rusqlite::Result<Vec<RoutineRow>> {
    let mut stmt = db.conn().prepare(
        "SELECT id, bot_id, name, prompt, schedule, active, next_run_at, last_run_at,
                tools, kind, tool, tool_args, hook_secret, hook_kind, hook_events, hook_match,
                conditions, second_opinion, consecutive_failures, paused_reason, last_error
         FROM routines WHERE active = 1 AND next_run_at IS NOT NULL AND next_run_at <= ?1",
    )?;

    let routines = stmt
        .query_map(params![now], |row| {
            Ok(RoutineRow {
                id: row.get(0)?,
                bot_id: row.get(1)?,
                name: row.get(2)?,
                prompt: row.get(3)?,
                schedule: row.get(4)?,
                active: row.get(5)?,
                next_run_at: row.get(6)?,
                last_run_at: row.get(7)?,
                tools: row.get(8)?,
                kind: row.get(9)?,
                tool: row.get(10)?,
                tool_args: row.get(11)?,
                hook_secret: row.get(12)?,
                hook_kind: row.get(13)?,
                hook_events: row.get(14)?,
                hook_match: row.get(15)?,
                conditions: row.get(16)?,
                second_opinion: row.get(17)?,
                consecutive_failures: row.get(18)?,
                paused_reason: row.get(19)?,
                last_error: row.get(20)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(routines)
}

/// Record the outcome of a routine run for health tracking.
pub fn record_routine_run(
    db: &Db,
    routine_id: &str,
    ok: bool,
    error: Option<&str>,
) -> rusqlite::Result<HealthOutcome> {
    const FAILURE_LIMIT: i32 = 3;

    if ok {
        db.conn().execute(
            "UPDATE routines SET consecutive_failures = 0, last_error = NULL WHERE id = ?1",
            params![routine_id],
        )?;
        return Ok(HealthOutcome {
            failures: 0,
            paused: false,
        });
    }

    // Check current state
    let mut stmt = db
        .conn()
        .prepare("SELECT consecutive_failures, active FROM routines WHERE id = ?1")?;
    let (n, active): (i32, i32) =
        stmt.query_row(params![routine_id], |row| Ok((row.get(0)?, row.get(1)?)))?;

    let failures = n + 1;
    let should_pause = failures >= FAILURE_LIMIT && active == 1;

    if should_pause {
        db.conn().execute(
            "UPDATE routines SET consecutive_failures = ?1, last_error = ?2, active = 0, paused_reason = ?3 WHERE id = ?4",
            params![
                failures,
                error.unwrap_or(""),
                format!("Stopped after {} failures in a row.", FAILURE_LIMIT),
                routine_id
            ],
        )?;
    } else {
        db.conn().execute(
            "UPDATE routines SET consecutive_failures = ?1, last_error = ?2 WHERE id = ?3",
            params![failures, error.unwrap_or(""), routine_id],
        )?;
    }

    Ok(HealthOutcome {
        failures,
        paused: should_pause,
    })
}

/// Resume a paused routine (clear pause reason, reset failures, reactivate).
pub fn resume_routine(db: &Db, routine_id: &str) -> rusqlite::Result<()> {
    db.conn().execute(
        "UPDATE routines SET active = 1, consecutive_failures = 0, paused_reason = NULL, last_error = NULL WHERE id = ?1",
        params![routine_id],
    )?;
    Ok(())
}

/// Mints a fresh webhook secret for a routine. Returns None if the routine
/// doesn't exist. The secret is returned ONCE and never again - `GET /api/
/// routines` only ever says `hasHook: true`.
pub fn mint_routine_hook(db: &Db, id: &str) -> rusqlite::Result<Option<String>> {
    // Check if routine exists
    let exists = db
        .conn()
        .query_row("SELECT 1 FROM routines WHERE id = ?1", params![id], |_| {
            Ok(())
        });

    if exists.is_err() {
        return Ok(None);
    }

    // Generate a 32-byte secret: concatenate two UUIDs (each is 128 bits) and
    // encode as hex. This matches the TS `randomBytes(32).toString("hex")`.
    let secret = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    db.conn().execute(
        "UPDATE routines SET hook_secret = ?1 WHERE id = ?2",
        params![&secret, id],
    )?;
    Ok(Some(secret))
}

/// Clears the webhook secret from a routine. Returns true if the routine
/// existed and was updated, false if the routine doesn't exist.
pub fn clear_routine_hook(db: &Db, id: &str) -> rusqlite::Result<bool> {
    let changes = db.conn().execute(
        "UPDATE routines SET hook_secret = NULL WHERE id = ?1",
        params![id],
    )?;
    Ok(changes > 0)
}

// Helper function to convert a database row into a Routine.
fn routine_from_row(db: &Db, row: &rusqlite::Row) -> rusqlite::Result<Routine> {
    let id: String = row.get(0)?;
    let bot_id: String = row.get(1)?;
    let name: String = row.get(2)?;
    let prompt: String = row.get(3)?;
    let schedule: String = row.get(4)?;
    let active: i32 = row.get(5)?;
    let next_run_at: Option<String> = row.get(6)?;
    let last_run_at: Option<String> = row.get(7)?;
    let tools_json: Option<String> = row.get(8)?;
    let kind: String = row.get(9)?;
    let tool: Option<String> = row.get(10)?;
    let tool_args: Option<String> = row.get(11)?;
    let hook_secret: Option<String> = row.get(12)?;
    let hook_kind: String = row.get(13)?;
    let hook_events_json: Option<String> = row.get(14)?;
    let hook_match: Option<String> = row.get(15)?;
    let conditions_json: Option<String> = row.get(16)?;
    let second_opinion: i32 = row.get(17)?;
    let consecutive_failures: i32 = row.get(18)?;
    let paused_reason: Option<String> = row.get(19)?;
    let last_error: Option<String> = row.get(20)?;

    // Get bot name
    let bot_name = get_bot_name(db, &bot_id).unwrap_or_else(|| bot_id.clone());

    Ok(Routine {
        id,
        bot_id,
        bot_name,
        name,
        prompt,
        schedule,
        active: active == 1,
        next_run_at,
        last_run_at,
        paused_reason,
        last_error,
        failures: consecutive_failures,
        tools: parse_tools(tools_json.as_deref()),
        kind: if kind == "tool" {
            "tool".to_string()
        } else {
            "prompt".to_string()
        },
        tool,
        tool_args,
        has_hook: hook_secret.is_some(),
        hook_kind,
        hook_events: parse_hook_events(hook_events_json.as_deref()),
        hook_match,
        conditions: parse_conditions(conditions_json.as_deref()),
        second_opinion: second_opinion != 0,
    })
}

// Helper to get bot name from bot_id
fn get_bot_name(db: &Db, bot_id: &str) -> Option<String> {
    db.conn()
        .prepare("SELECT name FROM bots WHERE id = ?1")
        .ok()?
        .query_row([bot_id], |row| row.get(0))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_tools() {
        assert_eq!(
            parse_tools(Some(r#"["search","shell"]"#)),
            Some(vec!["search".to_string(), "shell".to_string()])
        );
        assert_eq!(parse_tools(None), None);
        assert_eq!(parse_tools(Some("invalid")), None);
    }

    #[test]
    fn test_parse_hook_events() {
        assert_eq!(
            parse_hook_events(Some(r#"["push","pull_request"]"#)),
            Some(vec!["push".to_string(), "pull_request".to_string()])
        );
        assert_eq!(parse_hook_events(None), None);
    }
}
