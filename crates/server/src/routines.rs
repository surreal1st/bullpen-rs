//! S5-03: firing, the scheduler tick, pauses, and health for routines. Port
//! of `src/server/routines.ts`'s `fireRoutine`/`fireDue`/`runRoutineNow`/
//! `resumeAbsencePaused`/`startScheduler` (`routines.ts:19-43,650-936,967-
//! 1117`), narrowed the same way S5's ticket narrows the whole slice: a
//! "prompt"-kind routine only. A "tool"-kind routine (`routines.ts:809-878,
//! 982-1067`, E2's direct-tool-call path) is S5b - `fire_due`/
//! `run_routine_now` below skip a `kind == "tool"` row rather than firing it,
//! which is a routine that silently does nothing this slice rather than one
//! that crashes or invents a result; see this module's `Deliver` note in
//! `S5-tickets.md` for why (S5-02's store layer has the column, nothing yet
//! reads `permissionsForRun`/`getToolBox`/`runFoundNothing` on the Rust
//! side).
//!
//! Two rules from the TS module doc, unchanged (`routines.ts:19-33`):
//! 1. A routine always runs on the cheap model - enforced by `model::ladder::
//!    model_for_run`'s existing floor for any non-`Chat` trigger, not
//!    reimplemented here. `fire_routine` passes `Trigger::Routine` and the
//!    bot's own pin; the ladder is the only place that actually settles it.
//! 2. A routine that cannot do its job says so once and stops -
//!    `prompt::STOP_RATHER_THAN_INVENT` rides on every routine prompt,
//!    appended here (not in `build_prompt`, which knows nothing about
//!    triggers - see that constant's own doc).
//!
//! `fire_due`/`run_routine_now` take `&AppState` rather than a bare `db`/
//! `RunManager` pair, per the ticket: the spend-ceiling check needs
//! `state.credits` (a `CreditsPort`, async), and threading four separate
//! params through instead is the same shape with worse call sites.

use chrono::{DateTime, Local, Timelike, Utc};
use model::ladder::Trigger;
use store::Db;
use store::routines::RoutineRow;

use crate::AppState;
use crate::prompt::{self, HistoryTurn};
use crate::runs::StartOptions;
use crate::schedule::{self, Schedule};
use crate::spend;

/// Why an absence pause says what it says, exact and public so a caller (a
/// test, or the login route once it wires `resume_absence_paused` in) can
/// match on it. Verbatim from the TS `ABSENCE_PAUSE_REASON` (`routines.
/// ts:740`).
pub const ABSENCE_PAUSE_REASON: &str = "Paused: no sign-in for 5 days";

/// How long Josh can be gone before an unattended INTERVAL routine stops
/// firing into silence. Matches the TS `ABSENCE_DAYS` (`auth.ts:152`).
const ABSENCE_DAYS: i64 = 5;

/// Quiet hours, server-local, matching `goal-scheduler.ts:54-59`'s
/// `isQuietHours` exactly (23:00-07:00). TS only applies this to GOALS
/// (S5b); the orchestrator's Design section extends it to every routine
/// firing for this port - see `S5-tickets.md`'s Design section, "Scheduler".
/// A routine due DURING quiet hours is simply left alone (not rescheduled,
/// not paused): the next tick after quiet hours ends finds it still due
/// (`next_run_at <= now`) and fires it exactly once, catching up rather than
/// firing every 30s it was skipped.
pub fn is_quiet_hours(now: DateTime<Utc>) -> bool {
    let hour = now.with_timezone(&Local).hour();
    !(7..23).contains(&hour)
}

/// Raw row lookup by id. `store::routines::routine_by_id` returns the
/// friendly camelCase `Routine` DTO (bot name resolved, JSON columns
/// decoded) - firing needs the raw `prompt`/`schedule`/`kind` text instead,
/// the same shape `due_routines` already returns, so this runs the same
/// query with `id = ?1` rather than `active = 1 AND next_run_at <= ?1`.
/// Kept here rather than added to `store::routines` (S5-02's file, already
/// committed and not this ticket's to extend) since `Db::conn()` is public
/// and this is a three-line read.
fn routine_row_by_id(db: &Db, id: &str) -> Option<RoutineRow> {
    db.conn()
        .query_row(
            "SELECT id, bot_id, name, prompt, schedule, active, next_run_at, last_run_at,
                    tools, kind, tool, tool_args, hook_secret, hook_kind, hook_events, hook_match,
                    conditions, second_opinion, consecutive_failures, paused_reason, last_error
             FROM routines WHERE id = ?1",
            rusqlite::params![id],
            |row| {
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
            },
        )
        .ok()
}

/// Starts a "prompt"-kind routine's run. Port of the TS `fireRoutine`
/// (`routines.ts:665-737`), the "tool" branch of which is S5b (see the
/// module doc) - a `row.kind == "tool"` never reaches here, both callers
/// below check first.
///
/// `extra` is an already-formed block appended before the STOP block -
/// empty for an ordinary scheduled firing, which is all this slice needs
/// (the TS "what the tool found" / "what arrived" callers are S5b/hooks).
///
/// Returns `None` when the routine's bot no longer exists (deleted out from
/// under it) rather than starting nothing silently and rather than
/// panicking a scheduler tick over one stale row.
fn fire_routine(state: &AppState, row: &RoutineRow, extra: &str) -> Option<String> {
    // B2-style discipline (see `AppState::db`'s doc, and this crate's own
    // `routes/messages.rs`): the db guard is locked, read, and DROPPED
    // before `state.runs.start_routine` is called below - that call locks
    // the SAME `Arc<Mutex<Db>>` internally (via `RunManager::db`), and
    // `std::sync::Mutex` is not reentrant. Holding this guard across that
    // call would deadlock the calling thread, not merely race it.
    let (bot_id, conversation_id, model, messages) = {
        let db = state.db();
        let bot = store::get_bot(&db, &row.bot_id).ok().flatten()?;
        let conversation_id = store::get_or_create_conversation(&db, &bot.id).ok()?;
        let model = bot
            .model
            .clone()
            .unwrap_or_else(|| model::ladder::default_model(&db));

        // S5-03: only a SHORT marker rides in the conversation's own
        // history - `[name] ran` - never the full prompt text. Port of the
        // TS `+RED+` fix (`routines.ts:679-699`): the model still receives
        // the whole `prompt_text` below via `messages`, but a record of
        // what happened does not need the instructions replayed into it
        // every firing, or an interval routine's history grows forever and
        // every later firing pays to re-read its own old instructions.
        let _ = store::append_message(
            &db,
            &conversation_id,
            "user",
            &format!("[{}] ran", row.name),
            store::NewMessage::default(),
        );

        // S5-03: NO history. A routine is a job, not a conversation - port
        // of the TS `messages: buildPromptFor(db, bot, [], promptText)`
        // (`routines.ts:732`, see its own doc on the 58,665-token-per-run
        // measurement this fixed). The routine's prompt PLUS the STOP
        // block below is the only "history" turn `build_prompt` sees.
        let prompt_text = format!("{}{}{}", row.prompt, extra, prompt::STOP_RATHER_THAN_INVENT);
        let messages = prompt::build_prompt(&db, &bot, &[HistoryTurn::user(prompt_text)]);

        (bot.id, conversation_id, model, messages)
    };

    Some(state.runs.start_routine(
        StartOptions {
            bot_id,
            conversation_id,
            model,
            messages,
            trigger: Trigger::Routine,
            room: false,
        },
        row.id.clone(),
    ))
}

/// `POST /api/routines/:id/run` - "Run now": fires REGARDLESS of the
/// routine's schedule or active state. Port of the TS `runRoutineNow`
/// (`routines.ts:967-1069`), "tool"-kind branch out of scope (module doc).
pub fn run_routine_now(state: &AppState, id: &str) -> Result<String, String> {
    let (row, bot_exists) = {
        let db = state.db();
        let row = routine_row_by_id(&db, id);
        let bot_exists = row
            .as_ref()
            .map(|r| store::get_bot(&db, &r.bot_id).ok().flatten().is_some())
            .unwrap_or(false);
        (row, bot_exists)
    };
    let Some(row) = row else {
        return Err("no such routine".to_string());
    };
    if !bot_exists {
        return Err("no such bot".to_string());
    }
    if row.kind == "tool" {
        return Err("tool routines are not yet supported (S5b)".to_string());
    }
    fire_routine(state, &row, "").ok_or_else(|| "could not start".to_string())
}

/// Fires every routine that is due. Port of the TS `fireDue` (`routines.
/// ts:753-916`), narrowed to "prompt"-kind routines (module doc) and
/// extended to skip quiet hours (`is_quiet_hours`'s doc explains why that
/// is this port's own call, not TS's). Reschedules BEFORE starting a run,
/// same as TS, so a hung run cannot make its routine fire in a loop.
///
/// Returns `(routine_id, run_id)` for everything it actually started - the
/// same shape `POST /api/routines/tick` (S5-04, stubbed pending this
/// function) hands back as `{"started": [...]}`.
pub async fn fire_due(state: &AppState, now: DateTime<Utc>) -> Vec<(String, String)> {
    // Design: a routine due DURING quiet hours is left exactly as it is -
    // not rescheduled, not paused - so the very next tick after quiet hours
    // end finds it still due and fires it once, rather than firing on
    // every 30s tick it was skipped.
    if is_quiet_hours(now) {
        return Vec::new();
    }

    let now_str = now.to_rfc3339();
    let due = {
        let db = state.db();
        store::routines::due_routines(&db, &now_str).unwrap_or_default()
    };

    let mut started = Vec::new();
    for row in due {
        // E8: an unattended INTERVAL routine stops firing once nobody has
        // signed in for `ABSENCE_DAYS` - daily/weekdays/clock schedules are
        // untouched (those are worth a stream of history to come back to).
        // Port of `routines.ts:763-780`.
        if matches!(
            schedule::parse_schedule(&row.schedule),
            Ok(Schedule::Interval { .. })
        ) {
            let last_login = {
                let db = state.db();
                db.settings_get(store::auth::LAST_LOGIN_KEY).unwrap_or(None)
            };
            let absent = last_login
                .as_deref()
                .and_then(|iso| DateTime::parse_from_rfc3339(iso).ok())
                .map(|last| now.signed_duration_since(last.with_timezone(&Utc)))
                .is_some_and(|gone| gone > chrono::Duration::days(ABSENCE_DAYS));
            if absent {
                let db = state.db();
                let _ = db.conn().execute(
                    "UPDATE routines SET active = 0, paused_reason = ?1 WHERE id = ?2",
                    rusqlite::params![ABSENCE_PAUSE_REASON, row.id],
                );
                continue;
            }
        }

        // Reschedule before firing - port of `routines.ts:782-786`. A
        // schedule string that no longer parses (hand-edited db, a future
        // grammar change) is skipped rather than firing forever on a stale
        // `next_run_at` or panicking the whole tick over one bad row.
        let Ok(parsed) = schedule::parse_schedule(&row.schedule) else {
            continue;
        };
        let next = schedule::next_run(&parsed, now);
        {
            let db = state.db();
            let _ = db.conn().execute(
                "UPDATE routines SET next_run_at = ?1, last_run_at = ?2 WHERE id = ?3",
                rusqlite::params![next.to_rfc3339(), now.to_rfc3339(), row.id],
            );
        }

        // S5-03: the platform-wide spend ceiling, reused from `crate::
        // spend` rather than reimplemented - port of `routines.ts:791-807`.
        // Per-member ceilings (`scopeForBot`/`overUserCeiling`) are S5b: no
        // member scope exists on this side yet (same gap `store::auth`'s
        // module doc names for sessions). B2 discipline again: `db` is
        // locked for the two synchronous reads and dropped before the
        // `credits.total_usage()` await, exactly like `routes/messages.rs`.
        let ceiling = {
            let db = state.db();
            spend::get_ceiling(&db)
        };
        let account_usage = state.credits.total_usage().await.ok();
        let gate = {
            let db = state.db();
            spend::gate_run(&db, ceiling, account_usage)
        };
        if let spend::GateResult::Denied { reason } = gate {
            let db = state.db();
            let _ = db.conn().execute(
                "UPDATE routines SET active = 0, paused_reason = ?1 WHERE id = ?2",
                rusqlite::params![reason, row.id],
            );
            continue;
        }

        // S5b: a "tool"-kind routine calls its tool directly with no model
        // turn unless the tool found something (`routines.ts:809-878`,
        // E2). Skipped rather than fired wrong - see the module doc.
        if row.kind == "tool" {
            continue;
        }

        if let Some(run_id) = fire_routine(state, &row, "") {
            started.push((row.id.clone(), run_id));
        }
    }

    started
}

/// Restarts every routine an absence pause switched off, and only those -
/// port of the TS `resumeAbsencePaused` (`routines.ts:923-936`). A routine
/// paused for FAILING three times running keeps its own reason; this never
/// touches it. Not yet wired to a login route: `POST /api/auth/login`
/// lives in `crates/server/src/routes/auth.rs`, which S5-04 owns per this
/// ticket's header rule - see this module's `S5-tickets.md` Result entry.
pub fn resume_absence_paused(state: &AppState, now: DateTime<Utc>) -> usize {
    let db = state.db();
    let rows: Vec<(String, String)> = {
        let mut stmt = match db
            .conn()
            .prepare("SELECT id, schedule FROM routines WHERE paused_reason = ?1")
        {
            Ok(stmt) => stmt,
            Err(_) => return 0,
        };
        stmt.query_map(rusqlite::params![ABSENCE_PAUSE_REASON], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .and_then(|mapped| mapped.collect::<Result<Vec<_>, _>>())
        .unwrap_or_default()
    };

    let mut resumed = 0usize;
    for (id, schedule_text) in rows {
        let Ok(parsed) = schedule::parse_schedule(&schedule_text) else {
            continue;
        };
        let next = schedule::next_run(&parsed, now);
        if db
            .conn()
            .execute(
                "UPDATE routines SET active = 1, paused_reason = NULL, next_run_at = ?1 WHERE id = ?2",
                rusqlite::params![next.to_rfc3339(), id],
            )
            .is_ok()
        {
            resumed += 1;
        }
    }
    resumed
}

/// Starts the 30s scheduler tick. Port of the TS `startScheduler`
/// (`routines.ts:1100-1117`): a `tokio::spawn`'d task, never awaited by the
/// caller (mirrors TS's `timer.unref?.()` - the process can still exit with
/// this running). A panic inside `fire_due`/one tick is caught by nothing
/// here on purpose - tokio isolates a spawned task's panic to that task, so
/// the worst case is this ONE tick's loop iteration never completing rather
/// than the process going down; `main.rs` never `.await`s the returned
/// handle.
pub fn start_scheduler(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            let now = Utc::now();
            for (routine_id, run_id) in fire_due(&state, now).await {
                tracing::info!(%routine_id, %run_id, "routine started run");
            }
        }
    })
}
