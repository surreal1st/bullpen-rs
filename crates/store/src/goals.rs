//! S5b-03: goals store layer, ported from `goals.ts`.
//!
//! A goal is deliberately not built on `routines`. A routine's whole shape
//! (prompt, schedule, three-strikes health) is "repeat this forever"; a goal
//! is something a bot works TOWARD across many sessions until it is done,
//! stuck, or out of budget, then stops on its own and says so. It needs its
//! own end states (done / stopped) a routine has no room for, a token budget
//! instead of a wall-clock one, and a plan + log that survive between
//! sessions.
//!
//! This file is the goal's own data: CRUD, log append/trim, and the pure
//! due-ness checks a scheduler consults. The scheduler and HTTP routes are
//! S5b-04, not this file.

use crate::Db;
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// How many sessions in a row may pass with no tool call before a goal
/// pauses itself. Same number and reasoning as a routine's `FAILURE_LIMIT`:
/// one bad session is a fluke, three in a row is a pattern nobody is awake
/// to notice. Consumed by the scheduler (S5b-04), not this file.
pub const NO_TOOL_LIMIT: i32 = 3;

/// Verbatim from TS, which builds this from `NO_TOOL_LIMIT` via a template
/// literal (`` `Paused: ${NO_TOOL_LIMIT} sessions in a row made no progress` ``).
/// Rust consts can't interpolate another const into a string literal without
/// a new dependency, so the "3" is hand-kept in sync with `NO_TOOL_LIMIT`
/// above - the reviewer should check both move together.
pub const NO_TOOL_PAUSE_REASON: &str = "Paused: 3 sessions in a row made no progress";

/// How many entries a goal's log is allowed to grow before the oldest drop.
/// Same reasoning as `MAX_OPEN_TASKS`: this rides in the prompt, so it
/// cannot be unbounded.
pub const MAX_LOG_ENTRIES: usize = 200;

/// The most active goals one bot may hold at once. A safety valve, not a
/// design constraint - Josh has never asked for more than a couple.
pub const MAX_ACTIVE_GOALS: i64 = 20;

/// Default cadence for a work session: hourly, per the plan.
pub const GOAL_SESSION_MS: i64 = 60 * 60 * 1000;

pub const WEEKLY_MS: i64 = 7 * 24 * 60 * 60 * 1000;

const GOAL_STATUSES: [&str; 4] = ["active", "paused", "done", "stopped"];

/// Whether `value` is one of the four goal statuses TS's `GoalStatus` union
/// allows.
pub fn is_goal_status(value: &str) -> bool {
    GOAL_STATUSES.contains(&value)
}

/// Self-creating, the same reason every other table added mid-tree is: two
/// sessions both appending to a numbered migration list race on position,
/// and this does not. Called from `Db::open` beside the other `ensure_*`
/// calls, so a db a live TS Bullpen wrote still opens here, and vice versa.
pub(crate) fn ensure_goal_tables(db: &Db) -> rusqlite::Result<()> {
    db.conn().execute_batch(
        "CREATE TABLE IF NOT EXISTS goals (
            id               TEXT PRIMARY KEY,
            bot_id           TEXT NOT NULL,
            objective        TEXT NOT NULL,
            done_when        TEXT NOT NULL,
            status           TEXT NOT NULL DEFAULT 'active',
            budget_tokens    INTEGER,
            spent_tokens     INTEGER NOT NULL DEFAULT 0,
            budget_until     TEXT,
            plan             TEXT NOT NULL DEFAULT '',
            log              TEXT NOT NULL DEFAULT '[]',
            no_tool_streak   INTEGER NOT NULL DEFAULT 0,
            reason           TEXT,
            next_session_at  TEXT,
            last_session_at  TEXT,
            last_report_at   TEXT,
            created_at       TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_goals_due ON goals(status, next_session_at);
        CREATE INDEX IF NOT EXISTS idx_goals_bot ON goals(bot_id, created_at);",
    )?;

    // `runs.goal_id` is NOT created by `ensureGoalTables` in the TS source -
    // it is `ensureGoalIdColumn` in `runs.ts`, a file outside this ticket's
    // read window (S5b-03 owns `goals.ts` only). `goal_runs` below needs the
    // column to exist on every db this crate opens - TS-made or fresh - so
    // it is added here as the same kind of self-creating column
    // `ensure_routine_columns` adds to `routines`. Checked against the TS by
    // the orchestrator: `runs.ts:166-167` is `ALTER TABLE runs ADD COLUMN
    // goal_id TEXT` and nothing more, so there is NO index on it here either -
    // an `idx_runs_goal` was added by S5b-03 and removed again, because
    // `tests/migrations.rs` guards this schema against exactly that kind of
    // invention and TS has no such index.
    let has_goal_id = {
        let mut stmt = db.conn().prepare("PRAGMA table_info(runs)")?;
        stmt.query_map([], |row| row.get::<_, String>(1))?
            .filter_map(Result::ok)
            .any(|name| name == "goal_id")
    };
    if !has_goal_id {
        db.conn()
            .execute("ALTER TABLE runs ADD COLUMN goal_id TEXT", [])?;
    }
    Ok(())
}

/// One entry in a goal's log - a reflection, a status note, or a weekly
/// report line.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct GoalLogEntry {
    pub at: String,
    /// "reflect" | "note" | "report" - kept as a plain string, the same
    /// convention `Condition.kind` uses elsewhere in this crate.
    pub kind: String,
    pub text: String,
}

/// Raw row shape from the `goals` table.
#[derive(Clone, Debug)]
pub struct GoalRow {
    pub id: String,
    pub bot_id: String,
    pub objective: String,
    pub done_when: String,
    pub status: String,
    pub budget_tokens: Option<i64>,
    pub spent_tokens: i64,
    pub budget_until: Option<String>,
    pub plan: String,
    pub log: String,
    pub no_tool_streak: i64,
    pub reason: Option<String>,
    pub next_session_at: Option<String>,
    pub last_session_at: Option<String>,
    pub last_report_at: Option<String>,
    pub created_at: String,
}

/// Wire shape for a goal - camelCase to match the TS JSON a client expects.
/// Deliberately excludes `no_tool_streak`, matching TS's `toGoal` (the
/// `Goal` interface has no `noToolStreak` field even though `GoalRow` does).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Goal {
    pub id: String,
    pub bot_id: String,
    pub bot_name: String,
    pub objective: String,
    pub done_when: String,
    pub status: String,
    pub budget_tokens: Option<i64>,
    pub spent_tokens: i64,
    pub budget_until: Option<String>,
    pub plan: String,
    pub log: Vec<GoalLogEntry>,
    pub reason: Option<String>,
    pub next_session_at: Option<String>,
    pub last_session_at: Option<String>,
    pub last_report_at: Option<String>,
    pub created_at: String,
}

const GOAL_COLUMNS: &str = "id, bot_id, objective, done_when, status, budget_tokens, \
     spent_tokens, budget_until, plan, log, no_tool_streak, reason, next_session_at, \
     last_session_at, last_report_at, created_at";

fn goal_row_from_row(row: &rusqlite::Row) -> rusqlite::Result<GoalRow> {
    Ok(GoalRow {
        id: row.get(0)?,
        bot_id: row.get(1)?,
        objective: row.get(2)?,
        done_when: row.get(3)?,
        status: row.get(4)?,
        budget_tokens: row.get(5)?,
        spent_tokens: row.get(6)?,
        budget_until: row.get(7)?,
        plan: row.get(8)?,
        log: row.get(9)?,
        no_tool_streak: row.get(10)?,
        reason: row.get(11)?,
        next_session_at: row.get(12)?,
        last_session_at: row.get(13)?,
        last_report_at: row.get(14)?,
        created_at: row.get(15)?,
    })
}

/// Parses a stored log column, same fallback TS uses: anything that isn't a
/// JSON array (malformed, or absent) becomes an empty log rather than an
/// error.
fn parse_log(stored: &str) -> Vec<GoalLogEntry> {
    serde_json::from_str(stored).unwrap_or_default()
}

fn to_goal(db: &Db, row: &GoalRow) -> Goal {
    let bot_name = crate::bots::get_bot(db, &row.bot_id)
        .ok()
        .flatten()
        .map(|b| b.name)
        .unwrap_or_else(|| row.bot_id.clone());

    Goal {
        id: row.id.clone(),
        bot_id: row.bot_id.clone(),
        bot_name,
        objective: row.objective.clone(),
        done_when: row.done_when.clone(),
        status: if is_goal_status(&row.status) {
            row.status.clone()
        } else {
            "active".to_string()
        },
        budget_tokens: row.budget_tokens,
        spent_tokens: row.spent_tokens,
        budget_until: row.budget_until.clone(),
        plan: row.plan.clone(),
        log: parse_log(&row.log),
        reason: row.reason.clone(),
        next_session_at: row.next_session_at.clone(),
        last_session_at: row.last_session_at.clone(),
        last_report_at: row.last_report_at.clone(),
        created_at: row.created_at.clone(),
    }
}

/// Input to `create_goal`. `budget_tokens` is `f64` (not `i64`) to mirror
/// the TS `number` type, which the same finite/positive check as TS applies
/// to before flooring.
#[derive(Clone, Debug, Default)]
pub struct CreateGoalInput {
    pub bot_id: String,
    pub objective: String,
    pub done_when: String,
    pub budget_tokens: Option<f64>,
    pub budget_until: Option<String>,
}

/// Creates a goal. Mirrors TS `createGoal`'s `{ok, goal, error}` shape as a
/// `Result`, the same convention `create_routine` already uses in this
/// crate.
pub fn create_goal(db: &Db, input: CreateGoalInput, now: DateTime<Utc>) -> Result<Goal, String> {
    let bot = crate::bots::get_bot(db, &input.bot_id).map_err(|e| e.to_string())?;
    if bot.is_none() {
        return Err("no such bot".to_string());
    }

    let objective = input.objective.trim().to_string();
    let done_when = input.done_when.trim().to_string();
    if objective.is_empty() {
        return Err("Say what the goal is.".to_string());
    }
    if done_when.is_empty() {
        return Err("Say what done looks like.".to_string());
    }

    let active: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM goals WHERE bot_id = ?1 AND status = 'active'",
            params![&input.bot_id],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    if active >= MAX_ACTIVE_GOALS {
        return Err(format!(
            "Already {} active goals, which is the limit. Close or stop some first.",
            active
        ));
    }

    let budget_tokens = match input.budget_tokens {
        Some(v) if v.is_finite() && v > 0.0 => Some(v.floor() as i64),
        _ => None,
    };

    let budget_until = match &input.budget_until {
        Some(s) if s.trim().is_empty() => None,
        other => other.clone(),
    };

    let id = Uuid::new_v4().to_string();
    let now_str = now.to_rfc3339();

    db.conn()
        .execute(
            "INSERT INTO goals (id, bot_id, objective, done_when, status, budget_tokens, budget_until, created_at, next_session_at)
             VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?6, ?7, ?8)",
            params![&id, &input.bot_id, &objective, &done_when, budget_tokens, budget_until, &now_str, &now_str],
        )
        .map_err(|e| e.to_string())?;

    let row = goal_by_id(db, &id)
        .map_err(|e| e.to_string())?
        .expect("just inserted");
    Ok(to_goal(db, &row))
}

/// Lists goals, optionally scoped to one bot. TS's `listGoals` also accepts
/// a room-visibility `MaybeScope` from `scope.ts`, a file outside this
/// ticket's read window; `list_routines` in this crate already omits the
/// equivalent parameter, so this follows the same, already-established
/// convention rather than porting it here.
pub fn list_goals(db: &Db, bot_id: Option<&str>) -> rusqlite::Result<Vec<Goal>> {
    let base = format!("SELECT {} FROM goals", GOAL_COLUMNS);
    let query = if bot_id.is_some() {
        format!("{} WHERE bot_id = ?1 ORDER BY created_at", base)
    } else {
        format!("{} ORDER BY created_at", base)
    };

    let mut stmt = db.conn().prepare(&query)?;
    let rows: Vec<GoalRow> = if let Some(bid) = bot_id {
        stmt.query_map(params![bid], goal_row_from_row)?
            .collect::<Result<Vec<_>, _>>()?
    } else {
        stmt.query_map([], goal_row_from_row)?
            .collect::<Result<Vec<_>, _>>()?
    };

    Ok(rows.iter().map(|r| to_goal(db, r)).collect())
}

/// Gets a single goal's raw row by id.
pub fn goal_by_id(db: &Db, id: &str) -> rusqlite::Result<Option<GoalRow>> {
    let mut stmt = db
        .conn()
        .prepare(&format!("SELECT {} FROM goals WHERE id = ?1", GOAL_COLUMNS))?;
    stmt.query_row(params![id], goal_row_from_row).optional()
}

/// The goal a bot's `reflect` call applies to, when the tool call itself
/// carries no goal id.
///
/// KNOWN GAP (ported verbatim from TS): a bot working two active goals in
/// the same scheduler tick can have this resolve to the wrong one, because
/// `last_session_at` is stamped when a session STARTS and both are started
/// before either finishes. Correct for the overwhelmingly common case (0 or
/// 1 active goal per bot).
pub fn most_recent_active_goal(db: &Db, bot_id: &str) -> rusqlite::Result<Option<GoalRow>> {
    let mut stmt = db.conn().prepare(&format!(
        "SELECT {} FROM goals WHERE bot_id = ?1 AND status = 'active' ORDER BY last_session_at DESC, created_at DESC LIMIT 1",
        GOAL_COLUMNS
    ))?;
    stmt.query_row(params![bot_id], goal_row_from_row)
        .optional()
}

/// Appends one entry to a goal's log, trimming the oldest entries first once
/// the log exceeds `MAX_LOG_ENTRIES`. Not "trim to fit the prompt" - this is
/// stored state Josh reads on the goal's own page, so it stays generous
/// compared to a task list.
fn append_log(
    db: &Db,
    id: &str,
    kind: &str,
    text: &str,
    now: DateTime<Utc>,
) -> rusqlite::Result<()> {
    let stored: Option<String> = db
        .conn()
        .query_row("SELECT log FROM goals WHERE id = ?1", params![id], |row| {
            row.get(0)
        })
        .optional()?;
    let Some(stored) = stored else {
        return Ok(());
    };

    let mut log = parse_log(&stored);
    log.push(GoalLogEntry {
        at: now.to_rfc3339(),
        kind: kind.to_string(),
        text: text.to_string(),
    });

    // Oldest drop first - see MAX_LOG_ENTRIES.
    let trimmed = if log.len() > MAX_LOG_ENTRIES {
        log.split_off(log.len() - MAX_LOG_ENTRIES)
    } else {
        log
    };

    let json = serde_json::to_string(&trimmed).unwrap_or_else(|_| "[]".to_string());
    db.conn()
        .execute("UPDATE goals SET log = ?1 WHERE id = ?2", params![json, id])?;
    Ok(())
}

/// A patch to a goal's mutable fields. The double-`Option` fields
/// (`budget_tokens`, `budget_until`) follow `UpdateRoutineFields`'s
/// convention in this crate: outer `None` means "field omitted, leave
/// unchanged"; `Some(None)` means "explicitly clear it"; `Some(Some(v))`
/// means "set it".
#[derive(Clone, Debug, Default)]
pub struct UpdateGoalPatch {
    pub status: Option<String>,
    pub plan: Option<String>,
    /// Evidence or a reason, depending on the status change. Required when
    /// moving TO "done".
    pub note: Option<String>,
    /// Admin-only fields (Josh editing the goal's own page). A bot's
    /// `update_goal` tool call never sends these.
    pub objective: Option<String>,
    pub done_when: Option<String>,
    pub budget_tokens: Option<Option<f64>>,
    pub budget_until: Option<Option<String>>,
}

/// Changes a goal's status or plan.
///
/// Scoped to the owning bot when `bot_id` is given - the tool path, where
/// the bot supplies its own id and must not be able to touch another bot's
/// goal. `bot_id` omitted is the admin path: Josh's own Pause/Resume/Stop
/// buttons on the goal's page, not scoped because he can already see and
/// open any goal.
pub fn update_goal(
    db: &Db,
    id: &str,
    patch: &UpdateGoalPatch,
    bot_id: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Goal, String> {
    let row = match bot_id {
        None => goal_by_id(db, id).map_err(|e| e.to_string())?,
        Some(bid) => {
            let mut stmt = db
                .conn()
                .prepare(&format!(
                    "SELECT {} FROM goals WHERE id = ?1 AND bot_id = ?2",
                    GOAL_COLUMNS
                ))
                .map_err(|e| e.to_string())?;
            stmt.query_row(params![id, bid], goal_row_from_row)
                .optional()
                .map_err(|e| e.to_string())?
        }
    };
    let row = match row {
        Some(r) => r,
        None => {
            return Err(if bot_id.is_none() {
                "no such goal".to_string()
            } else {
                "No goal of yours has that id.".to_string()
            });
        }
    };

    // A "done" the model cannot back up is exactly the invented-answer shape
    // `unverified.ts` exists to stop elsewhere in this codebase - so it is
    // refused here too, at the one place a goal closes.
    if patch.status.as_deref() == Some("done")
        && patch.note.as_deref().unwrap_or("").trim().is_empty()
    {
        return Err(
            "Say what evidence shows done_when is satisfied, in note, before closing it."
                .to_string(),
        );
    }

    let status = patch.status.clone().unwrap_or_else(|| {
        if is_goal_status(&row.status) {
            row.status.clone()
        } else {
            "active".to_string()
        }
    });
    let plan = match &patch.plan {
        Some(p) => p.trim().chars().take(8000).collect::<String>(),
        None => row.plan.clone(),
    };

    // Admin-only fields - a blank objective or done_when is refused rather
    // than silently kept, the same reason `create_goal` refuses one: a goal
    // with no objective is not a smaller goal, it is a broken one.
    let mut objective = row.objective.clone();
    if let Some(o) = &patch.objective {
        let trimmed = o.trim();
        if trimmed.is_empty() {
            return Err("Say what the goal is.".to_string());
        }
        objective = trimmed.to_string();
    }
    let mut done_when = row.done_when.clone();
    if let Some(d) = &patch.done_when {
        let trimmed = d.trim();
        if trimmed.is_empty() {
            return Err("Say what done looks like.".to_string());
        }
        done_when = trimmed.to_string();
    }

    let mut budget_tokens = row.budget_tokens;
    if let Some(inner) = &patch.budget_tokens {
        budget_tokens = match inner {
            Some(v) if v.is_finite() && *v > 0.0 => Some(v.floor() as i64),
            _ => None,
        };
    }

    let mut budget_until = row.budget_until.clone();
    if let Some(inner) = &patch.budget_until {
        budget_until = match inner {
            None => None,
            Some(s) if s.trim().is_empty() => None,
            Some(s) => Some(s.trim().to_string()),
        };
    }

    // `patch.note` defined but blank still overwrites `reason` with "" (not
    // null) - matches TS's unconditional `patch.note.trim().slice(0, 2000)`.
    let reason = match &patch.note {
        Some(n) => Some(n.trim().chars().take(2000).collect::<String>()),
        None => row.reason.clone(),
    };

    db.conn()
        .execute(
            "UPDATE goals
                SET status = ?1, plan = ?2, reason = ?3, objective = ?4, done_when = ?5,
                    budget_tokens = ?6, budget_until = ?7
              WHERE id = ?8",
            params![
                &status,
                &plan,
                &reason,
                &objective,
                &done_when,
                budget_tokens,
                &budget_until,
                id
            ],
        )
        .map_err(|e| e.to_string())?;

    if patch.status.is_some() || patch.note.is_some() {
        let text = match &patch.note {
            Some(n) if !n.trim().is_empty() => n.trim().to_string(),
            _ => format!("status -> {}", status),
        };
        append_log(db, id, "note", &text, now).map_err(|e| e.to_string())?;
    }

    let updated = goal_by_id(db, id)
        .map_err(|e| e.to_string())?
        .expect("just updated");
    Ok(to_goal(db, &updated))
}

/// Deletes a goal. Returns whether a row was actually removed.
pub fn delete_goal(db: &Db, id: &str) -> rusqlite::Result<bool> {
    let n = db
        .conn()
        .execute("DELETE FROM goals WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

/// `reflect(unexpected, next_steps)` - the model's own account of what a
/// session found, written to the goal's log rather than only into a chat
/// message that scrolls away. Always returns a message (never a hard
/// error), the same as TS.
pub fn reflect_on_goal(
    db: &Db,
    bot_id: &str,
    unexpected: &str,
    next_steps: &str,
    now: DateTime<Utc>,
) -> rusqlite::Result<String> {
    let Some(goal) = most_recent_active_goal(db, bot_id)? else {
        return Ok("You have no active goal to reflect on. Call set_goal first.".to_string());
    };

    let mut parts = Vec::new();
    let unexpected = unexpected.trim();
    if !unexpected.is_empty() {
        parts.push(format!("Unexpected: {}", unexpected));
    }
    let next_steps = next_steps.trim();
    if !next_steps.is_empty() {
        parts.push(format!("Next: {}", next_steps));
    }
    let text = parts.join(" ");
    if text.is_empty() {
        return Ok(
            "Say what was unexpected or what comes next - reflect needs at least one.".to_string(),
        );
    }

    append_log(db, &goal.id, "reflect", &text, now)?;
    Ok(format!("Logged on \"{}\".", goal.objective))
}

/// Whether `goal` has run past whichever budget it has, as of `now`.
/// Returns the report line to deliver, or `None` when it is still within
/// bounds.
pub fn budget_overrun(goal: &GoalRow, now: DateTime<Utc>) -> Option<String> {
    if let Some(budget_tokens) = goal.budget_tokens
        && goal.spent_tokens >= budget_tokens
    {
        return Some(format!(
            "Stopped \"{}\": used {} of its {}-token budget.",
            goal.objective, goal.spent_tokens, budget_tokens
        ));
    }
    if let Some(budget_until) = &goal.budget_until {
        // Date.parse on an unparseable string yields NaN in TS, and
        // `now > NaN` is always false - an unparseable deadline never
        // triggers an overrun there either, so a parse failure here just
        // falls through to `None` the same way.
        if let Ok(deadline) = DateTime::parse_from_rfc3339(budget_until)
            && now.timestamp_millis() > deadline.timestamp_millis()
        {
            return Some(format!(
                "Stopped \"{}\": past its {} deadline.",
                goal.objective, budget_until
            ));
        }
    }
    None
}

/// Whether a goal's weekly report is due, as of `now`. A pure function of
/// stamped timestamps rather than of the model's own behaviour - the
/// scheduler stamps `last_report_at` itself the moment it decides a session
/// should carry the weekly instruction, which is what makes this fire
/// exactly once a week rather than "probably about once a week".
pub fn due_for_weekly_report(
    last_report_at: Option<&str>,
    created_at: &str,
    now: DateTime<Utc>,
) -> bool {
    let since = last_report_at.unwrap_or(created_at);
    match DateTime::parse_from_rfc3339(since) {
        Ok(dt) => now.timestamp_millis() - dt.timestamp_millis() >= WEEKLY_MS,
        // Same NaN-comparison-is-false fallback as `budget_overrun`.
        Err(_) => false,
    }
}

/// Goals whose next session is due, in the same shape `due_routines` uses.
pub fn due_goals(db: &Db, now: DateTime<Utc>) -> rusqlite::Result<Vec<GoalRow>> {
    let now_str = now.to_rfc3339();
    let mut stmt = db.conn().prepare(&format!(
        "SELECT {} FROM goals WHERE status = 'active' AND next_session_at IS NOT NULL AND next_session_at <= ?1",
        GOAL_COLUMNS
    ))?;
    stmt.query_map(params![now_str], goal_row_from_row)?
        .collect()
}

/// One of a goal's own work sessions.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalRun {
    pub id: String,
    pub status: String,
    pub text: String,
    pub error: Option<String>,
    pub cost_usd: f64,
    pub created_at: String,
}

/// A goal's own work sessions, newest first - the same shape `routine_runs`
/// gives a routine.
pub fn goal_runs(db: &Db, goal_id: &str, limit: i32) -> rusqlite::Result<Vec<GoalRun>> {
    let mut stmt = db.conn().prepare(
        "SELECT id, status, text, error, cost_usd, created_at FROM runs
         WHERE goal_id = ?1 ORDER BY created_at DESC LIMIT ?2",
    )?;

    stmt.query_map(params![goal_id, limit], |row| {
        Ok(GoalRun {
            id: row.get(0)?,
            status: row.get(1)?,
            text: row.get(2)?,
            error: row.get(3)?,
            cost_usd: row.get(4)?,
            created_at: row.get(5)?,
        })
    })?
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_goal_status() {
        assert!(is_goal_status("active"));
        assert!(is_goal_status("paused"));
        assert!(is_goal_status("done"));
        assert!(is_goal_status("stopped"));
        assert!(!is_goal_status("bogus"));
    }

    #[test]
    fn test_parse_log() {
        assert_eq!(
            parse_log(r#"[{"at":"2026-01-01T00:00:00Z","kind":"note","text":"hi"}]"#),
            vec![GoalLogEntry {
                at: "2026-01-01T00:00:00Z".to_string(),
                kind: "note".to_string(),
                text: "hi".to_string(),
            }]
        );
        assert_eq!(parse_log("not json"), Vec::<GoalLogEntry>::new());
        assert_eq!(
            parse_log(r#"{"not":"an array"}"#),
            Vec::<GoalLogEntry>::new()
        );
    }
}
