//! S5b-04: the goals-shaped half of routines-shaped machinery - the due
//! loop, firing an hourly work session, and settling one once it finishes.
//! Port of `src/server/goal-scheduler.ts` (298 lines, read whole). Goal DATA
//! (CRUD, log append/trim, the pure due-ness checks) is `store::goals`
//! (S5b-03, already landed); this file is what makes one actually RUN, the
//! same split `crates/server/src/routines.rs` and TS's `routine-health.ts`
//! already have for routines.
//!
//! Quiet hours (23:00-07:00 local) belong to GOALS only - `routines.rs`'s
//! own `fire_due` doc explains why S5-03 deleted an earlier, wrongly-placed
//! quiet-hours gate there (TS `fireDue` has none). `goal-scheduler.ts:54-72`
//! is this file's own source for the rule.
//!
//! 🔴 KNOWN GAP, flagged for the reviewer/orchestrator rather than worked
//! around: TS wires `settleGoalRun` to `runs.onRunDone`, called by `app.ts`
//! for EVERY finished run regardless of what started it. `RunManager::
//! on_run_done` (`crates/server/src/runs.rs`) is a SINGLE-SLOT hook already
//! claimed by `rooms::RoomEngine::install` (called from `AppState::build` in
//! `crates/server/src/lib.rs`) to chain a room round - `runs.rs` and the body
//! of `AppState::build` are both outside this ticket's owned files (the
//! shared-tree protocol's three allowed edits are one `mod` line each in
//! `lib.rs`/`routes/mod.rs` plus one call line in `main.rs`, none of which
//! reach `on_run_done`). `settle_goal_run` below is fully ported and directly
//! testable (mirrors TS exactly, called with an explicit `run_id`/`now`), but
//! nothing in this file's own `start_goal_scheduler` calls it automatically
//! for a run that finished on its own spawned task - `fire_due_goals`'s
//! 30s tick only starts sessions, same as TS's own `startGoalScheduler`
//! timer body (`goal-scheduler.ts:287-298`) does; TS's actual settling
//! happens through the separate `onRunDone` wiring this file cannot reach.
//! Production wiring needs either a multi-listener `on_run_done` on
//! `RunManager` or an explicit call added to `AppState::build`, both one
//! ticket away and both out of this one's scope.
//!
//! Per-member spend ceilings (TS `overUserCeiling`/`scopeForBot`) are a
//! second, smaller known gap - no member-scope concept exists on the Rust
//! side yet, the same gap `routines.rs`'s own `fire_due` names for the
//! identical TS call in `routines.ts:791-807`. The platform-wide ceiling
//! `routines.rs` DOES enforce (`crate::spend::gate_run`) has no goals
//! equivalent in the TS source either (`fireDueGoals` never calls it), so
//! this file adds none - not a narrowing, TS genuinely has nothing there.

use chrono::{DateTime, Local, TimeZone, Timelike, Utc};
use model::ladder::Trigger;
use rusqlite::OptionalExtension;
use store::goals::{self, GoalLogEntry, GoalRow};

use crate::AppState;
use crate::prompt::{self, HistoryTurn};
use crate::runs::StartOptions;

/// Verbatim from TS's array-joined `GOAL_SESSION_INSTRUCTIONS`
/// (`goal-scheduler.ts:29-37`, `.join("\n")` of the listed lines, the first
/// of which is an empty string - hence the leading `\n` here).
const GOAL_SESSION_INSTRUCTIONS: &str = "\n## How to behave on a goal work session\nNobody is reading this as it happens. This is one work session toward the goal above, not a conversation.\nRead the plan and your own log before doing anything, so you pick up where the last session left off rather than starting over.\nUse `update_goal` to revise the plan as you learn things, and to close the goal the moment done_when is genuinely satisfied - with evidence in `note`.\nCall `reflect` before you finish: say what was unexpected and what the next session should do.\nIf something you need is missing, say exactly what is missing and stop. Do not guess, and do not report a result you did not actually obtain.";

/// Verbatim from TS's `WEEKLY_REPORT_INSTRUCTIONS` (`goal-scheduler.ts:39-43`).
const WEEKLY_REPORT_INSTRUCTIONS: &str = "\n## This session also owes a weekly report\nBefore anything else, give a short status update on this goal: what has been done, what is left, and whether the deadline or budget still looks right. It will be delivered as your reply.";

/// A fixed quiet window rather than a per-bot setting - same as TS (`goal-
/// scheduler.ts:46-55`): no settings surface for one yet, and Josh's own
/// hours do not vary bot to bot. Server-LOCAL clock, the same one every
/// other timestamp in this file works from via `DateTime<Local>` conversion.
const QUIET_HOURS_START: u32 = 23;
const QUIET_HOURS_END: u32 = 7;

/// Port of TS `isQuietHours` (`goal-scheduler.ts:57-60`).
pub fn is_quiet_hours(now: DateTime<Utc>) -> bool {
    let hour = now.with_timezone(&Local).hour();
    !(QUIET_HOURS_END..QUIET_HOURS_START).contains(&hour)
}

/// Port of TS `nextWorkableSession` (`goal-scheduler.ts:62-71`): outside
/// quiet hours, just the ordinary session cadence ahead; inside it, pushed
/// to the quiet window's own end (07:00 LOCAL) rather than stacking another
/// hour on top, so a goal due at 01:00 gets its next session at 07:00, not
/// 08:00.
fn next_workable_session(now: DateTime<Utc>) -> DateTime<Utc> {
    if !is_quiet_hours(now) {
        return now + chrono::Duration::milliseconds(goals::GOAL_SESSION_MS);
    }
    let local_now = now.with_timezone(&Local);
    let today = local_now.date_naive();
    let same_day = Local
        .from_local_datetime(
            &today
                .and_hms_opt(QUIET_HOURS_END, 0, 0)
                .expect("valid time"),
        )
        .single();
    let next_local = match same_day {
        Some(candidate) if candidate > local_now => candidate,
        _ => {
            let tomorrow = today + chrono::Duration::days(1);
            Local
                .from_local_datetime(
                    &tomorrow
                        .and_hms_opt(QUIET_HOURS_END, 0, 0)
                        .expect("valid time"),
                )
                .single()
                .unwrap_or(local_now)
        }
    };
    next_local.with_timezone(&Utc)
}

/// Port of TS `recentLog` (`goal-scheduler.ts:73-77`), default `count = 5`.
fn recent_log(log: &[GoalLogEntry], count: usize) -> String {
    let tail: &[GoalLogEntry] = if log.len() > count {
        &log[log.len() - count..]
    } else {
        log
    };
    if tail.is_empty() {
        return "Nothing logged yet - this is the first session.".to_string();
    }
    tail.iter()
        .map(|e| format!("- [{}] {}", e.kind, e.text))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Port of TS `goalPromptText` (`goal-scheduler.ts:80-89`). `store::goals::
/// parse_log` is private to that module (S5b-03's own file), so the log
/// column is parsed here directly with the same "malformed becomes empty"
/// fallback rather than reached across the module boundary.
fn goal_prompt_text(goal: &GoalRow, weekly_report_due: bool) -> String {
    let parsed: Vec<GoalLogEntry> = serde_json::from_str(&goal.log).unwrap_or_default();
    let plan = goal.plan.trim();
    let plan_block = if plan.is_empty() {
        "No plan yet - write one with update_goal before doing anything else.".to_string()
    } else {
        goal.plan.clone()
    };
    let blocks = [
        format!("## Goal\n\n{}", goal.objective),
        format!("## Done when\n\n{}", goal.done_when),
        format!("## Current plan\n\n{plan_block}"),
        format!("## Recent log\n\n{}", recent_log(&parsed, 5)),
    ];
    let mut text = blocks.join("\n\n");
    if weekly_report_due {
        text.push_str(WEEKLY_REPORT_INSTRUCTIONS);
    }
    text.push_str(GOAL_SESSION_INSTRUCTIONS);
    text
}

/// Starts one goal's work session right now, regardless of its schedule.
/// Port of TS `fireGoalSession` (`goal-scheduler.ts:99-126`).
///
/// Model: `model::ladder::default_model` (TS `getDefaultModel(db)`) rather
/// than the bot's own pin - deliberately, the same reason `routines.rs`'s
/// `fire_routine` ignores `bot.model` (see that function's own "F1: forced,
/// not defaulted" doc): a goal session must not inherit whatever premium
/// model the bot happens to be chatting on. `Trigger::Goal` then carries the
/// cheap-model floor through `model_for_run` inside `RunManager::start`
/// exactly as `Trigger::Routine` does (`model::ladder::model_for_run`'s own
/// doc), so a platform default that DOES look premium (a hand-edited
/// setting, a vendor rename) still gets floored.
///
/// `goal_id` is stamped onto the run row with a follow-up `UPDATE` rather
/// than at INSERT time (contrast `RunManager::start_routine`, which stamps
/// `routine_id` synchronously before its drive task spawns, precisely to
/// avoid a race with `settle`'s own UPDATE reading it back). Two things make
/// that same race harmless here: `RunManager` has no `start_goal` entry
/// point to add one without editing `crates/server/src/runs.rs` (not owned
/// by this ticket - see this module's top doc), and `settle`'s own UPDATE in
/// `runs.rs` never touches the `goal_id` column, so writing it a moment
/// after `start` returns can never be clobbered by anything that column
/// itself.
fn fire_goal_session(state: &AppState, row: &GoalRow, now: DateTime<Utc>) -> Option<String> {
    let (bot_id, conversation_id, model, messages, weekly_report_due) = {
        let db = state.db();
        let bot = store::get_bot(&db, &row.bot_id).ok().flatten()?;
        let conversation_id = store::get_or_create_conversation(&db, &bot.id).ok()?;
        let weekly_report_due =
            goals::due_for_weekly_report(row.last_report_at.as_deref(), &row.created_at, now);

        let _ = store::append_message(
            &db,
            &conversation_id,
            "user",
            &format!("[goal: {}] work session", row.objective),
            store::NewMessage::default(),
        );

        let model = model::ladder::default_model(&db);
        let prompt_text = goal_prompt_text(row, weekly_report_due);
        let messages = prompt::build_prompt(&db, &bot, &[HistoryTurn::user(prompt_text)]);

        (bot.id, conversation_id, model, messages, weekly_report_due)
    };

    let run_id = state.runs.start(StartOptions {
        bot_id,
        conversation_id,
        model,
        messages,
        trigger: Trigger::Goal,
        room: false,
    });

    {
        let db = state.db();
        let _ = db.conn().execute(
            "UPDATE runs SET goal_id = ?1 WHERE id = ?2",
            rusqlite::params![row.id, run_id],
        );
        let _ = db.conn().execute(
            "UPDATE goals SET last_session_at = ?1 WHERE id = ?2",
            rusqlite::params![now.to_rfc3339(), row.id],
        );
        if weekly_report_due {
            let _ = db.conn().execute(
                "UPDATE goals SET last_report_at = ?1 WHERE id = ?2",
                rusqlite::params![now.to_rfc3339(), row.id],
            );
        }
    }

    Some(run_id)
}

/// Fires every goal whose next session is due. Returns `(goal_id, run_id)`
/// for everything it actually started. Port of TS `fireDueGoals`
/// (`goal-scheduler.ts:133-194`), minus the per-member spend ceiling check
/// (this module's top doc: `overUserCeiling`/`scopeForBot` have no Rust
/// equivalent yet). Reschedules BEFORE starting, same reason `routines.rs`'s
/// `fire_due` does: a run that hangs must not make its goal fire in a loop.
pub fn fire_due_goals(state: &AppState, now: DateTime<Utc>) -> Vec<(String, String)> {
    let due = {
        let db = state.db();
        goals::due_goals(&db, now).unwrap_or_default()
    };

    let mut started = Vec::new();
    for row in due {
        // Money first, then the clock - same ordering TS's own comment
        // insists on (`goal-scheduler.ts:148-155`): a goal past its deadline
        // stops here rather than spending a session on it, checked BEFORE
        // the quiet-hours skip below so a goal cannot sit "active" all
        // night on an already-missed deadline.
        let overrun = goals::budget_overrun(&row, now);
        if let Some(overrun) = overrun {
            let db = state.db();
            let _ = db.conn().execute(
                "UPDATE goals SET status = 'stopped', reason = ?1 WHERE id = ?2",
                rusqlite::params![overrun, row.id],
            );
            if let Ok(conversation_id) = store::get_or_create_conversation(&db, &row.bot_id) {
                let _ = store::append_message(
                    &db,
                    &conversation_id,
                    "assistant",
                    &overrun,
                    store::NewMessage::default(),
                );
            }
            continue;
        }

        if is_quiet_hours(now) {
            let db = state.db();
            let _ = db.conn().execute(
                "UPDATE goals SET next_session_at = ?1 WHERE id = ?2",
                rusqlite::params![next_workable_session(now).to_rfc3339(), row.id],
            );
            continue;
        }

        {
            let db = state.db();
            let next = now + chrono::Duration::milliseconds(goals::GOAL_SESSION_MS);
            let _ = db.conn().execute(
                "UPDATE goals SET next_session_at = ?1 WHERE id = ?2",
                rusqlite::params![next.to_rfc3339(), row.id],
            );
        }

        if let Some(run_id) = fire_goal_session(state, &row, now) {
            started.push((row.id.clone(), run_id));
        }
    }

    started
}

/// Fires one goal's session right now, on demand. Port of TS `runGoalNow`
/// (`goal-scheduler.ts:197-202`).
pub fn run_goal_now(state: &AppState, id: &str, now: DateTime<Utc>) -> Result<String, String> {
    let row = {
        let db = state.db();
        goals::goal_by_id(&db, id).ok().flatten()
    };
    let Some(row) = row else {
        return Err("no such goal".to_string());
    };
    fire_goal_session(state, &row, now).ok_or_else(|| "no such bot".to_string())
}

struct SettleRunRow {
    goal_id: Option<String>,
    conversation_id: String,
    status: String,
    messages: String,
    input_tokens: i64,
    output_tokens: i64,
}

/// Everything a goal's bookkeeping needs once one of its work sessions
/// settles. Port of TS `settleGoalRun` (`goal-scheduler.ts:226-267`) - see
/// this module's top doc for why nothing in `start_goal_scheduler` calls
/// this automatically yet (the `RunManager::on_run_done` hook TS wires this
/// through is out of this ticket's reach). Directly callable (a test, or a
/// future caller once the hook exists), same as TS's own export.
///
/// `run.status` is read but deliberately unused beyond existing on the row -
/// TS's own `settleGoalRun` never branches on it either (`goal.status !==
/// 'active'` is the only status guard, and that is the GOAL's status, not
/// the RUN's); kept as a struct field anyway so a caller reading this code
/// can see the shape actually matches the `runs` table.
pub fn settle_goal_run(state: &AppState, run_id: &str, now: DateTime<Utc>) {
    let run = {
        let db = state.db();
        db.conn()
            .query_row(
                "SELECT goal_id, conversation_id, status, messages, input_tokens, output_tokens
                   FROM runs WHERE id = ?1",
                rusqlite::params![run_id],
                |row| {
                    Ok(SettleRunRow {
                        goal_id: row.get(0)?,
                        conversation_id: row.get(1)?,
                        status: row.get(2)?,
                        messages: row.get(3)?,
                        input_tokens: row.get(4)?,
                        output_tokens: row.get(5)?,
                    })
                },
            )
            .optional()
            .unwrap_or(None)
    };
    let Some(run) = run else { return };
    let _ = &run.status; // see doc above
    let Some(goal_id) = run.goal_id else { return };

    let goal = {
        let db = state.db();
        goals::goal_by_id(&db, &goal_id).ok().flatten()
    };
    let Some(goal) = goal else { return };
    if goal.status != "active" {
        return;
    }

    let did_work = serde_json::from_str::<Vec<serde_json::Value>>(&run.messages)
        .map(|messages| {
            messages
                .iter()
                .any(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
        })
        .unwrap_or(false);

    let spent = run.input_tokens.max(0) + run.output_tokens.max(0);
    if spent > 0 {
        let db = state.db();
        let _ = db.conn().execute(
            "UPDATE goals SET spent_tokens = spent_tokens + ?1 WHERE id = ?2",
            rusqlite::params![spent, goal.id],
        );
    }

    let streak = if did_work { 0 } else { goal.no_tool_streak + 1 };
    {
        let db = state.db();
        let _ = db.conn().execute(
            "UPDATE goals SET no_tool_streak = ?1 WHERE id = ?2",
            rusqlite::params![streak, goal.id],
        );
    }

    if streak >= i64::from(goals::NO_TOOL_LIMIT) {
        {
            let db = state.db();
            let _ = db.conn().execute(
                "UPDATE goals SET status = 'paused', reason = ?1 WHERE id = ?2",
                rusqlite::params![goals::NO_TOOL_PAUSE_REASON, goal.id],
            );
        }
        append_goal_log(state, &goal.id, "note", goals::NO_TOOL_PAUSE_REASON, now);
        {
            let db = state.db();
            let _ = store::append_message(
                &db,
                &run.conversation_id,
                "assistant",
                goals::NO_TOOL_PAUSE_REASON,
                store::NewMessage::default(),
            );
        }
        return;
    }

    // Re-read: spent_tokens just moved, and this session's own spend can be
    // exactly what pushes it over - same re-read TS does.
    let after = {
        let db = state.db();
        goals::goal_by_id(&db, &goal.id).ok().flatten()
    };
    let Some(after) = after else { return };
    if let Some(overrun) = goals::budget_overrun(&after, now) {
        {
            let db = state.db();
            let _ = db.conn().execute(
                "UPDATE goals SET status = 'stopped', reason = ?1 WHERE id = ?2",
                rusqlite::params![overrun, goal.id],
            );
        }
        append_goal_log(state, &goal.id, "report", &overrun, now);
        {
            let db = state.db();
            let _ = store::append_message(
                &db,
                &run.conversation_id,
                "assistant",
                &overrun,
                store::NewMessage::default(),
            );
        }
    }
}

/// Same trimming and shape `store::goals`' own (private) `append_log` uses,
/// duplicated rather than reached across the module boundary - the same
/// choice TS itself makes (`goal-scheduler.ts:269-284`'s own doc: "duplicated
/// rather than exported/imported across the module boundary for one call
/// each").
pub fn append_goal_log(state: &AppState, id: &str, kind: &str, text: &str, now: DateTime<Utc>) {
    let db = state.db();
    let stored: Option<String> = db
        .conn()
        .query_row(
            "SELECT log FROM goals WHERE id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .optional()
        .unwrap_or(None);
    let Some(stored) = stored else { return };

    let mut log: Vec<GoalLogEntry> = serde_json::from_str(&stored).unwrap_or_default();
    log.push(GoalLogEntry {
        at: now.to_rfc3339(),
        kind: kind.to_string(),
        text: text.to_string(),
    });
    let trimmed = if log.len() > goals::MAX_LOG_ENTRIES {
        log.split_off(log.len() - goals::MAX_LOG_ENTRIES)
    } else {
        log
    };
    let json = serde_json::to_string(&trimmed).unwrap_or_else(|_| "[]".to_string());
    let _ = db.conn().execute(
        "UPDATE goals SET log = ?1 WHERE id = ?2",
        rusqlite::params![json, id],
    );
}

/// Checks for due goal sessions on a timer. Port of TS `startGoalScheduler`
/// (`goal-scheduler.ts:287-298`): its own timer body calls ONLY
/// `fireDueGoals`, same as this one - `settleGoalRun` is wired through
/// `onRunDone` elsewhere in TS, not through this timer, and this port's own
/// equivalent wiring is the gap this module's top doc names.
pub fn start_goal_scheduler(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            let now = Utc::now();
            let started = fire_due_goals(&state, now);
            for (goal_id, run_id) in started {
                tracing::info!(%goal_id, %run_id, "goal started session");
            }
        }
    })
}
