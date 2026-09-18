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
//! 1. A routine always runs on the cheap model - `fire_routine` calls
//!    `model::ladder::safe_fallback` directly (F1: forced, not defaulted,
//!    exactly like the TS `getDefaultModel(db)` comment at `routines.
//!    ts:704-705`), never the bot's own pin. `model_for_run`'s non-`Chat`
//!    floor still runs underneath via `StartOptions`/`start_routine`, but it
//!    is a second net, not the enforcement point - see `fire_routine`'s own
//!    doc for why the bot's pin cannot be trusted to reach it in the first
//!    place.
//! 2. A routine that cannot do its job says so once and stops -
//!    `prompt::STOP_RATHER_THAN_INVENT` rides on every routine prompt,
//!    appended here (not in `build_prompt`, which knows nothing about
//!    triggers - see that constant's own doc).
//!
//! `fire_due`/`run_routine_now` take `&AppState` rather than a bare `db`/
//! `RunManager` pair, per the ticket: the spend-ceiling check needs
//! `state.credits` (a `CreditsPort`, async), and threading four separate
//! params through instead is the same shape with worse call sites.

use chrono::{DateTime, Utc};
use model::ladder::Trigger;
use store::Db;
use store::routines::RoutineRow;

use crate::AppState;
use crate::permissions;
use crate::prompt::{self, HistoryTurn};
use crate::runs::{RecordToolRun, StartOptions};
use crate::schedule::{self, Schedule};
use crate::spend;
use crate::tools;

/// Why an absence pause says what it says, exact and public so a caller (a
/// test, or the login route once it wires `resume_absence_paused` in) can
/// match on it. Verbatim from the TS `ABSENCE_PAUSE_REASON` (`routines.
/// ts:740`).
pub const ABSENCE_PAUSE_REASON: &str = "Paused: no sign-in for 5 days";

/// How long Josh can be gone before an unattended INTERVAL routine stops
/// firing into silence. Matches the TS `ABSENCE_DAYS` (`auth.ts:152`).
const ABSENCE_DAYS: i64 = 5;

/// Marker that a tool found nothing worth reporting. Port of `nothingToReport.ts`.
const NOTHING_NEW: &str = "[[bullpen:nothing-new]]";

/// Check if a tool result declared nothing worth reporting.
fn tool_found_nothing(result: &str) -> bool {
    result.contains(NOTHING_NEW)
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

/// S5b: Runs a "tool"-kind routine: executes the tool, then starts a model
/// run ONLY if the tool found something worth reporting. Port of the TS
/// `fireRoutine`'s tool branch (`routines.ts:809-916` scheduled, merged
/// with the run-now caller in the same function per the TS source).
///
/// F1/F2 (`reviews/S5b-R.md`): every exit below records a REAL `runs` row
/// via `RunManager::record_tool_run` - permission-refused, unknown-name,
/// bad-args, quiet, and found-something alike - where this used to be a
/// bare `return None` on the first two and nothing at all tracking the
/// third. That is what makes the 3-strikes auto-pause
/// (`store::routines::record_routine_run`, unchanged, already worked)
/// reachable from a tool routine at all, and what makes "Recent runs"
/// show anything for one.
///
/// Returns `Some(run_id)` on EVERY branch - refused, unknown/bad-args,
/// quiet, and found - `None` only when the bot row itself is gone
/// underneath this call (a race with a delete; `run_routine_now`'s only
/// caller already confirmed the bot exists moments before, so this is
/// unreachable from there and purely defensive for `fire_due`'s scheduled
/// sweep).
async fn fire_routine_tool(state: &AppState, row: &RoutineRow) -> Option<String> {
    let (bot_id, conversation_id, model, tool_name, tool_args_raw) = {
        let db = state.db();
        let bot = store::get_bot(&db, &row.bot_id).ok().flatten()?;
        let conversation_id = store::get_or_create_conversation(&db, &bot.id).ok()?;
        let model = model::ladder::safe_fallback(&db);
        let tool_name = row.tool.as_ref().cloned().unwrap_or_default();
        let tool_args_raw = row
            .tool_args
            .as_ref()
            .cloned()
            .unwrap_or_else(|| "{}".to_string());
        (
            bot.id.clone(),
            conversation_id,
            model,
            tool_name,
            tool_args_raw,
        )
    };

    // Records the FAILURE branch: a failed `runs` row plus the routine's
    // own health update, shared by every "cannot run" exit below.
    let record_failure = |message: &str| -> String {
        let run_id = state.runs.record_tool_run(RecordToolRun {
            bot_id: bot_id.clone(),
            conversation_id: conversation_id.clone(),
            routine_id: row.id.clone(),
            tool: tool_name.clone(),
            args: tool_args_raw.clone(),
            result: String::new(),
            ok: false,
            error: Some(message.to_string()),
        });
        let db = state.db();
        let _ = store::routines::record_routine_run(&db, &row.id, false, Some(message));
        run_id
    };

    // (a) Permission not Allow - port of the TS `perms[tool] !== "allow"`
    // (`routines.ts:824`). Checked with whatever decision the tool NAME
    // resolves to, known or not - matches TS, which has no separate
    // unknown-name gate because its permission map IS the tool catalog.
    let perms = {
        let db = state.db();
        permissions::permissions_for_run(&db, &bot_id, Trigger::Routine).unwrap_or_default()
    };
    if !matches!(perms.get(&tool_name), Some(&permissions::Decision::Allow)) {
        let run_id =
            record_failure("This routine's tool needs approval, so it cannot run unattended.");
        return Some(run_id);
    }

    // (b) F1: an unknown tool name - a row written before the save-time
    // gate existed (`validate_tool_kind`, `routes/routines.rs`), or
    // hand-edited, must not reach `toolbox.run`'s `Unknown tool: {name}`
    // fallback (`tools/mod.rs`), which used to read back as a FOUND
    // result and bill a real model call to report failure. This is the
    // bug F1 names: `fetch_url` carries a default `Allow` decision (the
    // permission map is the full ~60-name TS catalog, not this toolbox's
    // 13), so the check above alone does not catch it.
    if !tools::known_tool_names()
        .iter()
        .any(|name| name == &tool_name)
    {
        let run_id = record_failure(&format!("Bullpen has no tool called {tool_name}."));
        return Some(run_id);
    }

    if tool_name == "snap_desk" {
        let run_id = record_failure(
            "Screen capture requires a following model step and cannot run as a direct tool routine.",
        );
        return Some(run_id);
    }

    // (b) F2: `tool_args` must parse as a JSON object before it reaches
    // the toolbox - port of the TS `JSON.parse(argsText)` wrapped in
    // `fireRoutine`'s own try/catch (`routines.ts:846-864`), which records
    // a failure on a throw instead of running the tool on garbage input or
    // treating the raw string as a find. Save-time validation
    // (`validate_tool_kind`) already guarantees a freshly-saved row cannot
    // reach here with bad args; this is defense for a legacy or
    // hand-edited row, same reasoning as (b) above.
    let args_ok = serde_json::from_str::<serde_json::Value>(&tool_args_raw)
        .map(|v| v.is_object())
        .unwrap_or(false);
    if !args_ok {
        let run_id = record_failure("tool arguments must be a JSON object");
        return Some(run_id);
    }

    // The tool call itself gets the FULL toolbox (`only: None`) - F3's
    // narrowing to ALWAYS_ON applies only to the PHRASING run below, which
    // is a MODEL turn; this is the routine calling its own tool directly.
    let toolbox = state.runs.toolbox_for_context(
        &bot_id,
        Trigger::Routine,
        false,
        &model,
        None,
        tools::RunExecutionContext::DirectRoutine,
    );
    let (result, _usage) = toolbox
        .run(&tool_name, &tool_args_raw)
        .await
        .into_text_only("direct tool routine");

    // (c) Nothing worth reporting - record an OK tool run and an OK health
    // update, but no model turn (TS `runFoundNothing`, `routines.ts:866`).
    if tool_found_nothing(&result) {
        let run_id = state.runs.record_tool_run(RecordToolRun {
            bot_id: bot_id.clone(),
            conversation_id: conversation_id.clone(),
            routine_id: row.id.clone(),
            tool: tool_name.clone(),
            args: tool_args_raw.clone(),
            result,
            ok: true,
            error: None,
        });
        let db = state.db();
        let _ = store::routines::record_routine_run(&db, &row.id, true, None);
        return Some(run_id);
    }

    // (d) Found something - record the tool run for history FIRST (TS does
    // the same: `recordToolRun` before `appendMessage`/`runs.start`,
    // `routines.ts:882-897`), then start the phrasing run. That run's OWN
    // health update happens when IT settles (`RunManager::settle`, already
    // wired to a run's `routine_id`) - not duplicated here.
    let _ = state.runs.record_tool_run(RecordToolRun {
        bot_id: bot_id.clone(),
        conversation_id: conversation_id.clone(),
        routine_id: row.id.clone(),
        tool: tool_name.clone(),
        args: tool_args_raw.clone(),
        result: result.clone(),
        ok: true,
        error: None,
    });

    let (new_bot_id, new_conversation_id, new_model, new_messages) = {
        let db = state.db();
        let bot = store::get_bot(&db, &bot_id).ok().flatten()?;
        let _ = store::append_message(
            &db,
            &conversation_id,
            "user",
            &format!("[{}] ran", row.name),
            store::NewMessage::default(),
        );
        let prompt_text = format!(
            "{}\n\n## What the tool found\n\n{}{}",
            row.prompt,
            result,
            prompt::STOP_RATHER_THAN_INVENT
        );
        let messages = prompt::build_prompt(&db, &bot, &[HistoryTurn::user(prompt_text)]);
        (
            bot_id.clone(),
            conversation_id.clone(),
            model.clone(),
            messages,
        )
    };

    // F3: `vec![]` narrows the phrasing run to ALWAYS_ON only - port of
    // the TS `tools: []` (`routines.ts:908-910`), whose own comment says
    // why: "the model can say something, not run anything."
    Some(state.runs.start_routine_narrowed(
        StartOptions {
            bot_id: new_bot_id,
            conversation_id: new_conversation_id,
            model: new_model,
            messages: new_messages,
            trigger: Trigger::Routine,
            room: false,
        },
        row.id.clone(),
        vec![],
    ))
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
        // F1: forced, not defaulted - the bot's own pin is deliberately
        // ignored, exactly like the TS `getDefaultModel(db)` comment at
        // `routines.ts:704-705`. `bot.model` used to be read here first,
        // which meant `model_for_run`'s non-Chat floor was the only thing
        // standing between a routine and its bot's pin - and that floor
        // waves `anthropic/claude-sonnet-5` straight through on purpose
        // (`ladder::model_for_run`'s own doc: "sonnet" is deliberately
        // absent from `PREMIUM_MARKERS`), so a bot pinned to Sonnet for
        // chat fired every fifteen minutes on Sonnet, unattended.
        // `safe_fallback` is the same floor a timer run gets nowhere near a
        // bot at all, so there is no pin left to ignore.
        let model = model::ladder::safe_fallback(&db);

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
/// (`routines.ts:967-1069`), including the tool-kind branch (S5b).
pub async fn run_routine_now(state: &AppState, id: &str) -> Result<String, String> {
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
        // F9: `fire_routine_tool` now records a real run and returns
        // `Some(run_id)` on every branch - refused, unknown/bad-args,
        // quiet, and found alike (F1/F2) - so a check that correctly
        // found nothing (or was correctly refused) no longer maps to the
        // generic "could not start" 400 `routes/routines.rs` paints red
        // under the row. `None` here means the bot row vanished out from
        // under a call this function already confirmed had one - see
        // `fire_routine_tool`'s own doc.
        fire_routine_tool(state, &row)
            .await
            .ok_or_else(|| "no such bot".to_string())
    } else {
        fire_routine(state, &row, "").ok_or_else(|| "could not start".to_string())
    }
}

/// Fires every routine that is due. Port of the TS `fireDue` (`routines.
/// ts:753-916`), narrowed to "prompt"-kind routines (module doc). Reschedules
/// BEFORE starting a run, same as TS, so a hung run cannot make its routine
/// fire in a loop.
///
/// F4: NO quiet-hours gate here. S5-03 added one (`is_quiet_hours`, since
/// deleted - nothing else in this crate calls it) on the strength of
/// `goal-scheduler.ts:57`/`quiet-routine.test.ts`, but `fireDue` in the TS
/// source (`routines.ts:753-916`, read whole) has no quiet-hours rule at
/// all - that test is the nothing-to-report rule for a live ping, not a
/// silence window. The Rust gate silenced a "daily at 23:30" routine for
/// 7.5 hours and an interval routine for 8 hours every night, and skipped
/// the absence/spend pauses below along with it. Quiet hours are a GOALS
/// concept (S5b), not a routines one.
///
/// Returns `(routine_id, run_id)` for everything it actually started - the
/// same shape `POST /api/routines/tick` (S5-04, stubbed pending this
/// function) hands back as `{"started": [...]}`.
pub async fn fire_due(state: &AppState, now: DateTime<Utc>) -> Vec<(String, String)> {
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
        // E2). Fire the tool and only start a model run if it found something.
        if row.kind == "tool" {
            if let Some(run_id) = fire_routine_tool(state, &row).await {
                started.push((row.id.clone(), run_id));
            }
        } else {
            if let Some(run_id) = fire_routine(state, &row, "") {
                started.push((row.id.clone(), run_id));
            }
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
/// this running).
///
/// F8: each tick's `fire_due` runs as ITS OWN `tokio::spawn`'d task, awaited
/// by this loop rather than called inline. The doc this replaces claimed "the
/// worst case is this ONE tick's loop iteration never completing" - that is
/// wrong: a panic inside `fire_due` called directly on this task would abort
/// the whole `loop`, and nothing restarts it, so the scheduler goes silently
/// dead until the next deploy with one panic line as the only trace.
/// Reachable today: an interval count large enough to overflow `parse_
/// schedule`'s `n * 60` panics in a debug build. Spawning the tick's work
/// separately isolates a panic to that one task - tokio catches it at the
/// task boundary, `JoinHandle::await` comes back `Err`, and this loop's next
/// `interval.tick()` still runs. `main.rs` never `.await`s the OUTER handle
/// this function itself returns.
pub fn start_scheduler(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            let now = Utc::now();
            let tick_state = state.clone();
            match tokio::spawn(async move { fire_due(&tick_state, now).await }).await {
                Ok(started) => {
                    for (routine_id, run_id) in started {
                        tracing::info!(%routine_id, %run_id, "routine started run");
                    }
                }
                Err(join_err) => {
                    tracing::error!(
                        panic = join_err.is_panic(),
                        "routine scheduler tick failed: {join_err}"
                    );
                }
            }
        }
    })
}
