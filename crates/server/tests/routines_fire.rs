//! S5-03/S5-F-01 acceptance: firing, the scheduler's `fire_due`,
//! health/pause tracking, the cheap-model floor, and the STOP block. Drives
//! `server::routines::fire_due`/`resume_absence_paused` directly against a
//! real `AppState` (per the ticket: `POST /api/routines/tick` and `POST
//! /api/routines/:id/run` are S5-04 routes, still stubbed with a TODO
//! naming this ticket as of this writing - see `crates/server/src/routes/
//! routines.rs`), and reads results back through the routes S5-04 HAS
//! already wired (`GET /api/routines`, `GET /api/routines/:id/runs`) plus
//! `ScriptedPort::requests()` for what actually crossed the model boundary.
//!
//! Routines are created via `store::create_routine` directly rather than
//! `POST /api/routines`, to keep this file independent of S5-04's route
//! bodies (owned by another builder, uncommitted as of this writing).
//!
//! `AppState`'s `db` field is private to this crate by design (`lib.rs`'s
//! `AppState::db` doc), and this suite uses `:memory:` (no second handle
//! possible), so every assertion after `AppState::with_port` goes through
//! HTTP - the same posture every other external test file in this crate
//! (`tests/routines_routes.rs` among them) already takes.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Local, TimeZone, Utc};
use common::{ScriptedPort, seed_session, text_script};
use model::{MessageContent, ModelEvent};
use serde_json::Value;
use server::routines::{self, ABSENCE_PAUSE_REASON};
use server::schedule::{next_run, parse_schedule};
use server::{AppState, build_app};
use std::sync::Arc;
use store::Db;
use tower::ServiceExt;

fn open_db() -> Db {
    let db = Db::open(":memory:").expect("open :memory: db");
    model::routing::set_routing_settings(&db, Some(false), None)
        .expect("disable routing classifier");
    server::judge::set_judge_enabled(&db, false).expect("disable judge");
    db
}

fn seed_bot(db: &Db, id: &str, model: Option<&str>) {
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at)
             VALUES (?1, ?2, '', 'do things', ?3, ?4)",
            rusqlite::params![id, id, model, chrono::Utc::now().to_rfc3339()],
        )
        .expect("seed bot");
}

/// Creates a routine (always starts paused - `create_routine`'s own rule)
/// and immediately activates it with `next_run_at` set to `due_at` - a
/// PAST or CURRENT instant relative to whatever `now` a test later hands
/// `fire_due`, exactly the way the TS test suite's `setRoutineActive(db,
/// id, true, new Date("...07:00:00"))` does (an explicit stamp, never
/// `next_run(schedule, now)`, which by definition lands in the future and
/// would never be due at that same `now`).
fn seed_active_routine(
    db: &Db,
    bot_id: &str,
    name: &str,
    prompt: &str,
    schedule: &str,
    due_at: DateTime<Utc>,
) -> String {
    let id = store::create_routine(
        db,
        bot_id,
        name,
        prompt,
        schedule.to_string(),
        Some(due_at.to_rfc3339()),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("create routine");
    store::set_routine_active(db, &id, true, Some(due_at.to_rfc3339())).expect("activate routine");
    id
}

fn error_script() -> Vec<ModelEvent> {
    vec![ModelEvent::Error {
        message: "429 upstream_provider_shared_pool".to_string(),
        status: Some(429),
    }]
}

fn message_text(msg: &model::ModelMessage) -> &str {
    match &msg.content {
        MessageContent::Text(text) => text.as_str(),
        MessageContent::Parts(_) => "",
    }
}

async fn get_json(app: &axum::Router, path: &str, session: &str) -> (StatusCode, Value) {
    let request = Request::get(path)
        .header("cookie", session)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Polls `GET /api/routines/:id/runs` until its NEWEST run row exists and
/// its status is no longer "running" - the run itself finishes on a
/// spawned task, same as production; this waits on the observable result
/// the same way `tests/rooms.rs`'s HTTP-driven tests do (small sleeps in a
/// loop), never on a fixed delay, and never on the scheduler's own clock
/// (which every call here passes explicitly).
async fn wait_for_run(app: &axum::Router, session: &str, routine_id: &str) -> Value {
    for _ in 0..300 {
        let (_, body) = get_json(app, &format!("/api/routines/{routine_id}/runs"), session).await;
        if let Some(run) = body
            .get("runs")
            .and_then(|r| r.as_array())
            .and_then(|a| a.first())
            && run.get("status").and_then(|s| s.as_str()) != Some("running")
        {
            return run.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("run against routine {routine_id} never left 'running'");
}

async fn routine_row(app: &axum::Router, session: &str, bot_id: &str, routine_id: &str) -> Value {
    let (_, body) = get_json(app, &format!("/api/routines?bot={bot_id}"), session).await;
    body.get("routines")
        .and_then(|r| r.as_array())
        .and_then(|a| {
            a.iter()
                .find(|r| r.get("id").and_then(|v| v.as_str()) == Some(routine_id))
        })
        .cloned()
        .expect("routine present in list")
}

/// A given hour/minute expressed as THIS machine's local wall clock,
/// converted to UTC - `schedule.rs`'s `next_run` reads `chrono::Local` for
/// daily/weekly schedules, so a fixed UTC instant would land at a different
/// local hour on some timezones than others.
fn local_wall_clock_utc(hour: u32, minute: u32) -> DateTime<Utc> {
    let today = Local::now().date_naive();
    Local
        .from_local_datetime(&today.and_hms_opt(hour, minute, 0).unwrap())
        .single()
        .expect("unambiguous local time")
        .with_timezone(&Utc)
}

#[tokio::test]
async fn fires_a_due_routine_once_and_reschedules_so_it_does_not_fire_twice() {
    let db = open_db();
    let session = seed_session(&db);
    seed_bot(&db, "arthur", None);
    let now = local_wall_clock_utc(9, 0);
    let id = seed_active_routine(&db, "arthur", "x", "Do the thing.", "every 15 minutes", now);

    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![text_script("done")]));
    let state = AppState::with_port(db, port);
    let app = build_app(state.clone());

    let started = routines::fire_due(&state, now).await;
    assert_eq!(started.len(), 1, "the due routine should fire exactly once");
    assert_eq!(started[0].0, id);

    // Firing again at the SAME instant must find nothing due - reschedule
    // happens BEFORE the run starts, so a hung run cannot loop.
    let started_again = routines::fire_due(&state, now).await;
    assert!(
        started_again.is_empty(),
        "must not fire twice for one due instant"
    );

    let row = routine_row(&app, &session, "arthur", &id).await;
    let next_run_at = row
        .get("nextRunAt")
        .and_then(|v| v.as_str())
        .expect("nextRunAt set");
    let next_run_at: DateTime<Utc> = next_run_at.parse().expect("valid timestamp");
    assert!(
        next_run_at > now,
        "next_run_at must advance past the fired instant"
    );

    let run = wait_for_run(&app, &session, &id).await;
    assert_eq!(run.get("status").and_then(|s| s.as_str()), Some("done"));
}

#[tokio::test]
async fn a_routine_not_yet_due_does_not_fire() {
    let db = open_db();
    seed_bot(&db, "arthur", None);
    let now = local_wall_clock_utc(9, 0);
    // `next_run_at` set to the real FUTURE occurrence (20:00 today, after
    // `now`=09:00) - unlike `seed_active_routine`, which stamps `due_at`
    // directly so a test can make a routine due right now.
    let parsed = parse_schedule("daily at 20:00").unwrap();
    let next = next_run(&parsed, now);
    let id = store::create_routine(
        &db,
        "arthur",
        "x",
        "y",
        "daily at 20:00".to_string(),
        Some(next.to_rfc3339()),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .unwrap();
    store::set_routine_active(&db, &id, true, Some(next.to_rfc3339())).unwrap();

    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![text_script("done")]));
    let state = AppState::with_port(db, port);

    let started = routines::fire_due(&state, now).await;
    assert!(started.is_empty(), "a routine not yet due must not fire");
}

/// F4: TS `fireDue` has no quiet-hours rule at all (`routines.ts:753-916`,
/// confirmed by full read) - the Rust port's own quiet-hours gate silenced
/// every routine due between 23:00 and 07:00 local, which was a divergence
/// from TS, not an extension of it (goals get their own quiet hours in
/// S5b). This is the "would have been skipped, must not be" case the old
/// `quiet_hours_skip_firing_entirely` test asserted backwards: a daily
/// routine due at 02:00 local fires exactly like one due at noon.
#[tokio::test]
async fn a_routine_due_at_night_fires_like_any_other_hour() {
    let db = open_db();
    let session = seed_session(&db);
    seed_bot(&db, "arthur", None);
    let night_now = local_wall_clock_utc(2, 0); // 02:00 local
    let id = seed_active_routine(&db, "arthur", "x", "y", "every 15 minutes", night_now);

    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![text_script("done")]));
    let state = AppState::with_port(db, port);
    let app = build_app(state.clone());

    let started = routines::fire_due(&state, night_now).await;
    assert_eq!(started.len(), 1, "a routine due at 02:00 local must fire");
    assert_eq!(started[0].0, id);

    wait_for_run(&app, &session, &id).await;
    let row = routine_row(&app, &session, "arthur", &id).await;
    assert_eq!(row.get("active").and_then(|v| v.as_bool()), Some(true));
}

#[tokio::test]
async fn three_failures_in_a_row_pause_the_routine_with_the_reason() {
    let db = open_db();
    let session = seed_session(&db);
    seed_bot(&db, "arthur", None);
    let start = local_wall_clock_utc(9, 0);
    let id = seed_active_routine(&db, "arthur", "x", "y", "hourly", start);

    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![error_script()]));
    let state = AppState::with_port(db, port);
    let app = build_app(state.clone());

    // "hourly" advances `next_run_at` by exactly one hour on every
    // reschedule (see `schedule::next_run`'s `Hourly` arm), so firing at
    // 09:00, then 10:00, then 11:00 finds the routine due each time with
    // no manual re-activation - `record_routine_run` only PAUSES it
    // (`active = 0`) on the THIRD failure, not before.
    for n in 0..3i64 {
        let at = start + chrono::Duration::hours(n);
        let started = routines::fire_due(&state, at).await;
        assert_eq!(
            started.len(),
            1,
            "firing #{n} should start a run while still active"
        );
        wait_for_run(&app, &session, &id).await;
    }

    let row = routine_row(&app, &session, "arthur", &id).await;
    assert_eq!(row.get("active").and_then(|v| v.as_bool()), Some(false));
    assert!(
        row.get("pausedReason")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .contains("in a row"),
        "pausedReason should name the streak: {row:?}"
    );
}

#[tokio::test]
async fn a_routine_that_keeps_working_is_never_paused() {
    let db = open_db();
    let session = seed_session(&db);
    seed_bot(&db, "arthur", None);
    let start = local_wall_clock_utc(9, 0);
    let id = seed_active_routine(&db, "arthur", "x", "y", "hourly", start);

    let port: Arc<dyn model::ModelPort> =
        Arc::new(ScriptedPort::new(vec![text_script("all good")]));
    let state = AppState::with_port(db, port);
    let app = build_app(state.clone());

    // One more firing than the failure limit (4 vs FAILURE_LIMIT=3) - the
    // other half of the same rule: a reset on success is not optional, or
    // an old failure streak eventually pauses an otherwise healthy routine.
    for n in 0..4i64 {
        let at = start + chrono::Duration::hours(n);
        routines::fire_due(&state, at).await;
        wait_for_run(&app, &session, &id).await;
    }

    let row = routine_row(&app, &session, "arthur", &id).await;
    assert_eq!(row.get("active").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(row.get("failures").and_then(|v| v.as_i64()), Some(0));
}

#[tokio::test]
async fn absence_pause_stops_an_interval_routine_but_leaves_a_daily_one_alone() {
    let db = open_db();
    let session = seed_session(&db);
    seed_bot(&db, "arthur", None);
    let now = local_wall_clock_utc(9, 0);
    let six_days_ago = (now - chrono::Duration::days(6)).to_rfc3339();
    db.settings_set(store::auth::LAST_LOGIN_KEY, &six_days_ago)
        .expect("seed last_login_at");

    let interval_id = seed_active_routine(&db, "arthur", "interval", "y", "every 15 minutes", now);
    let daily_id = seed_active_routine(&db, "arthur", "daily", "y", "daily at 07:30", now);

    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![text_script("done")]));
    let state = AppState::with_port(db, port);
    let app = build_app(state.clone());

    let started = routines::fire_due(&state, now).await;
    assert_eq!(
        started.len(),
        1,
        "only the daily routine should fire - the interval one is absence-paused"
    );
    assert_eq!(started[0].0, daily_id);

    let interval_row = routine_row(&app, &session, "arthur", &interval_id).await;
    assert_eq!(
        interval_row.get("active").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        interval_row.get("pausedReason").and_then(|v| v.as_str()),
        Some(ABSENCE_PAUSE_REASON)
    );

    let daily_row = routine_row(&app, &session, "arthur", &daily_id).await;
    assert_eq!(
        daily_row.get("active").and_then(|v| v.as_bool()),
        Some(true)
    );
}

#[tokio::test]
async fn no_signin_ever_recorded_never_reads_as_five_days_overdue() {
    let db = open_db();
    seed_bot(&db, "arthur", None);
    let now = local_wall_clock_utc(9, 0);
    // No `auth.last_login_at` row at all - a fresh install, and every other
    // test in this file. Must never read as "absent".
    let id = seed_active_routine(&db, "arthur", "x", "y", "every 15 minutes", now);

    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![text_script("done")]));
    let state = AppState::with_port(db, port);

    let started = routines::fire_due(&state, now).await;
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].0, id);
}

#[tokio::test]
async fn resume_absence_paused_resumes_only_absence_paused_routines() {
    let db = open_db();
    let session = seed_session(&db);
    seed_bot(&db, "arthur", None);
    let now = local_wall_clock_utc(9, 0);

    let absent_id = seed_active_routine(&db, "arthur", "absent", "y", "hourly", now);
    db.conn()
        .execute(
            "UPDATE routines SET active = 0, paused_reason = ?1 WHERE id = ?2",
            rusqlite::params![ABSENCE_PAUSE_REASON, absent_id],
        )
        .unwrap();

    let failed_id = seed_active_routine(&db, "arthur", "failed", "y", "hourly", now);
    db.conn()
        .execute(
            "UPDATE routines SET active = 0, paused_reason = 'Stopped after 3 failures in a row.' WHERE id = ?1",
            rusqlite::params![failed_id],
        )
        .unwrap();

    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![text_script("done")]));
    let state = AppState::with_port(db, port);
    let app = build_app(state.clone());

    let resumed = routines::resume_absence_paused(&state, now);
    assert_eq!(resumed, 1, "only the absence-paused routine should resume");

    let absent_row = routine_row(&app, &session, "arthur", &absent_id).await;
    assert_eq!(
        absent_row.get("active").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert!(
        absent_row
            .get("pausedReason")
            .map(|v| v.is_null())
            .unwrap_or(true)
    );

    let failed_row = routine_row(&app, &session, "arthur", &failed_id).await;
    assert_eq!(
        failed_row.get("active").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        failed_row.get("pausedReason").and_then(|v| v.as_str()),
        Some("Stopped after 3 failures in a row.")
    );
}

#[tokio::test]
async fn a_routine_fires_on_the_cheap_model_even_with_a_premium_pin() {
    let db = open_db();
    seed_bot(&db, "arthur", Some("anthropic/claude-fable-5.1"));
    let now = local_wall_clock_utc(9, 0);
    let id = seed_active_routine(&db, "arthur", "nightly", "Check the logs.", "hourly", now);

    let scripted = Arc::new(ScriptedPort::new(vec![text_script("done")]));
    let port: Arc<dyn model::ModelPort> = scripted.clone();
    let state = AppState::with_port(db, port);

    let started = routines::fire_due(&state, now).await;
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].0, id);

    // Wait for the model call to actually land (the run drives on a
    // spawned task) before reading `requests()`.
    for _ in 0..300 {
        if !scripted.requests().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let requests = scripted.requests();
    assert_eq!(requests.len(), 1, "exactly one model call for one firing");
    assert_eq!(
        requests[0].model,
        model::CHEAP_DEFAULT_MODEL,
        "a routine must never reach a model its bot is merely pinned to when that pin is premium"
    );
}

/// F1: `anthropic/claude-sonnet-5` is deliberately absent from
/// `ladder::PREMIUM_MARKERS` (see its own doc - a ROOM round needs that
/// gap, since sonnet pins are exactly what ran up the room bill), so
/// `model_for_run`'s ordinary non-Chat floor would wave a Sonnet pin
/// straight through unflagged. `fire_routine` must never even hand it the
/// pin to wave through - it calls `safe_fallback` directly, the same as TS
/// `getDefaultModel(db)`, so this is the one pin the marker-based floor
/// alone cannot catch.
#[tokio::test]
async fn a_routine_fires_on_the_cheap_default_even_with_a_sonnet_pin() {
    let db = open_db();
    seed_bot(&db, "arthur", Some("anthropic/claude-sonnet-5"));
    let now = local_wall_clock_utc(9, 0);
    let id = seed_active_routine(&db, "arthur", "nightly", "Check the logs.", "hourly", now);

    let scripted = Arc::new(ScriptedPort::new(vec![text_script("done")]));
    let port: Arc<dyn model::ModelPort> = scripted.clone();
    let state = AppState::with_port(db, port);

    let started = routines::fire_due(&state, now).await;
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].0, id);

    for _ in 0..300 {
        if !scripted.requests().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let requests = scripted.requests();
    assert_eq!(requests.len(), 1, "exactly one model call for one firing");
    assert_eq!(
        requests[0].model,
        model::CHEAP_DEFAULT_MODEL,
        "a sonnet pin must not survive a routine firing - the marker-based \
         floor alone would let it through"
    );
}

#[tokio::test]
async fn the_stop_rather_than_invent_block_rides_on_every_routine_prompt() {
    let db = open_db();
    seed_bot(&db, "arthur", None);
    let now = local_wall_clock_utc(9, 0);
    let id = seed_active_routine(
        &db,
        "arthur",
        "digest",
        "Post the daily digest.",
        "hourly",
        now,
    );

    let scripted = Arc::new(ScriptedPort::new(vec![text_script("done")]));
    let port: Arc<dyn model::ModelPort> = scripted.clone();
    let state = AppState::with_port(db, port);

    let started = routines::fire_due(&state, now).await;
    assert_eq!(started[0].0, id);

    for _ in 0..300 {
        if !scripted.requests().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let requests = scripted.requests();
    assert_eq!(requests.len(), 1);
    let last_user = requests[0]
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .expect("a user turn carrying the routine prompt");
    let text = message_text(last_user);
    assert!(
        text.contains("Post the daily digest."),
        "the routine's own prompt must ride along: {text}"
    );
    assert!(
        text.to_lowercase()
            .contains("say exactly what is missing and stop"),
        "STOP_RATHER_THAN_INVENT must be appended: {text}"
    );
    assert!(
        text.to_lowercase().contains("do not substitute"),
        "STOP_RATHER_THAN_INVENT must be appended: {text}"
    );
}
