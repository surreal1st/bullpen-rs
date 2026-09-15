//! S5b-03 goals tests: store layer for goals (CRUD, log trim, due-ness checks).

use chrono::{DateTime, Utc};
use rusqlite::params;
use std::fs;
use std::path::PathBuf;
use uuid::Uuid;

use store::Db;
use store::goals::{
    CreateGoalInput, MAX_ACTIVE_GOALS, MAX_LOG_ENTRIES, UpdateGoalPatch, budget_overrun,
    create_goal, delete_goal, due_for_weekly_report, due_goals, goal_by_id, goal_runs, list_goals,
    most_recent_active_goal, reflect_on_goal, update_goal,
};

/// Helper to create a test bot in the database.
fn create_test_bot(db: &Db) -> String {
    let bot_id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![&bot_id, "Test Bot", "Testing", "Do things", &now],
        )
        .expect("create test bot");
    bot_id
}

/// Copies the fixture to a fresh temp path for isolation.
fn copy_fixture_to_temp() -> PathBuf {
    let fixture = "d:/rainmade/.scratch/bullpen-rs/fixtures/ts-made.db";
    let temp = std::env::temp_dir().join(format!("bullpen-rs-goals-test-{}.db", Uuid::new_v4()));
    fs::copy(fixture, &temp).expect("copy fixture to temp path");
    temp
}

fn t(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

/// Test 1: ensureGoalTables is idempotent (opening twice doesn't error), and
/// the fixture a live TS Bullpen wrote still opens.
#[test]
fn ensure_goal_tables_is_idempotent_and_opens_ts_fixture() {
    let temp = copy_fixture_to_temp();
    let db1 = Db::open(temp.to_str().unwrap()).expect("open first time");
    let db2 = Db::open(temp.to_str().unwrap()).expect("open second time");

    let goals1 = list_goals(&db1, None).expect("list goals db1");
    let goals2 = list_goals(&db2, None).expect("list goals db2");
    assert_eq!(goals1.len(), goals2.len(), "same number of goals");

    let _ = fs::remove_file(&temp);
}

/// Test 2: create a goal and see it in list_goals; bot_name is resolved.
#[test]
fn create_goal_appears_in_list_with_bot_name() {
    let db = Db::open(":memory:").expect("open memory db");
    let bot_id = create_test_bot(&db);

    let goal = create_goal(
        &db,
        CreateGoalInput {
            bot_id: bot_id.clone(),
            objective: "Ship the thing".to_string(),
            done_when: "It's shipped".to_string(),
            budget_tokens: Some(50_000.0),
            budget_until: None,
        },
        t("2026-09-15T00:00:00Z"),
    )
    .expect("create goal");

    assert_eq!(goal.objective, "Ship the thing");
    assert_eq!(goal.bot_name, "Test Bot");
    assert_eq!(goal.status, "active");
    assert_eq!(goal.budget_tokens, Some(50_000));
    assert_eq!(goal.spent_tokens, 0);
    // Due right away: next_session_at is stamped alongside created_at.
    assert_eq!(
        goal.next_session_at.as_deref(),
        Some("2026-09-15T00:00:00+00:00")
    );

    let listed = list_goals(&db, Some(&bot_id)).expect("list goals");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, goal.id);
}

/// Test 3: create_goal refuses an unknown bot, a blank objective, and a
/// blank done_when.
#[test]
fn create_goal_validates_input() {
    let db = Db::open(":memory:").expect("open memory db");
    let bot_id = create_test_bot(&db);

    let no_bot = create_goal(
        &db,
        CreateGoalInput {
            bot_id: "nonexistent".to_string(),
            objective: "x".to_string(),
            done_when: "y".to_string(),
            ..Default::default()
        },
        Utc::now(),
    );
    assert_eq!(no_bot.unwrap_err(), "no such bot");

    let blank_objective = create_goal(
        &db,
        CreateGoalInput {
            bot_id: bot_id.clone(),
            objective: "   ".to_string(),
            done_when: "y".to_string(),
            ..Default::default()
        },
        Utc::now(),
    );
    assert_eq!(blank_objective.unwrap_err(), "Say what the goal is.");

    let blank_done_when = create_goal(
        &db,
        CreateGoalInput {
            bot_id,
            objective: "x".to_string(),
            done_when: "  ".to_string(),
            ..Default::default()
        },
        Utc::now(),
    );
    assert_eq!(blank_done_when.unwrap_err(), "Say what done looks like.");
}

/// Test 4 (BITE 1): MAX_ACTIVE_GOALS caps active goals per bot at 20 -
/// TS's number, hardcoded here rather than read back off the constant, so a
/// changed constant actually turns this test red (see Results). The
/// exported `MAX_ACTIVE_GOALS` is asserted equal to the same literal as a
/// belt-and-braces check.
#[test]
fn create_goal_enforces_max_active_goals_cap() {
    assert_eq!(MAX_ACTIVE_GOALS, 20, "TS's MAX_ACTIVE_GOALS is 20");

    let db = Db::open(":memory:").expect("open memory db");
    let bot_id = create_test_bot(&db);

    for i in 0..20 {
        let result = create_goal(
            &db,
            CreateGoalInput {
                bot_id: bot_id.clone(),
                objective: format!("Goal {}", i),
                done_when: "done".to_string(),
                ..Default::default()
            },
            Utc::now(),
        );
        assert!(result.is_ok(), "goal {} should succeed (count < cap)", i);
    }

    let result = create_goal(
        &db,
        CreateGoalInput {
            bot_id: bot_id.clone(),
            objective: "One too many".to_string(),
            done_when: "done".to_string(),
            ..Default::default()
        },
        Utc::now(),
    );

    assert_eq!(
        result.unwrap_err(),
        "Already 20 active goals, which is the limit. Close or stop some first."
    );

    let active = list_goals(&db, Some(&bot_id)).expect("list goals");
    assert_eq!(active.len(), 20, "cap held at exactly 20");
}

/// Test 5: update_goal changes status/plan, appends a log note, and refuses
/// a status->done with no note.
#[test]
fn update_goal_changes_status_and_logs_note() {
    let db = Db::open(":memory:").expect("open memory db");
    let bot_id = create_test_bot(&db);

    let goal = create_goal(
        &db,
        CreateGoalInput {
            bot_id: bot_id.clone(),
            objective: "Ship it".to_string(),
            done_when: "It's live".to_string(),
            ..Default::default()
        },
        t("2026-09-15T00:00:00Z"),
    )
    .expect("create goal");

    // done with no note is refused.
    let refused = update_goal(
        &db,
        &goal.id,
        &UpdateGoalPatch {
            status: Some("done".to_string()),
            ..Default::default()
        },
        None,
        t("2026-09-15T01:00:00Z"),
    );
    assert_eq!(
        refused.unwrap_err(),
        "Say what evidence shows done_when is satisfied, in note, before closing it."
    );

    // done with a note succeeds and logs it.
    let updated = update_goal(
        &db,
        &goal.id,
        &UpdateGoalPatch {
            status: Some("done".to_string()),
            note: Some("Verified live at rainmade.io".to_string()),
            ..Default::default()
        },
        None,
        t("2026-09-15T01:00:00Z"),
    )
    .expect("update to done");

    assert_eq!(updated.status, "done");
    assert_eq!(updated.log.len(), 1);
    assert_eq!(updated.log[0].kind, "note");
    assert_eq!(updated.log[0].text, "Verified live at rainmade.io");
    assert_eq!(
        updated.reason.as_deref(),
        Some("Verified live at rainmade.io")
    );
}

/// Test 6: update_goal scoped to a bot_id refuses another bot's goal.
#[test]
fn update_goal_scoped_to_bot_refuses_other_bots_goal() {
    let db = Db::open(":memory:").expect("open memory db");
    let bot_id = create_test_bot(&db);
    let other_bot_id = create_test_bot(&db);

    let goal = create_goal(
        &db,
        CreateGoalInput {
            bot_id: bot_id.clone(),
            objective: "Mine".to_string(),
            done_when: "done".to_string(),
            ..Default::default()
        },
        Utc::now(),
    )
    .expect("create goal");

    let result = update_goal(
        &db,
        &goal.id,
        &UpdateGoalPatch {
            plan: Some("sneaky".to_string()),
            ..Default::default()
        },
        Some(&other_bot_id),
        Utc::now(),
    );
    assert_eq!(result.unwrap_err(), "No goal of yours has that id.");
}

/// Test 7: delete_goal removes the row and reports whether it did.
#[test]
fn delete_goal_removes_row() {
    let db = Db::open(":memory:").expect("open memory db");
    let bot_id = create_test_bot(&db);

    let goal = create_goal(
        &db,
        CreateGoalInput {
            bot_id,
            objective: "Delete me".to_string(),
            done_when: "gone".to_string(),
            ..Default::default()
        },
        Utc::now(),
    )
    .expect("create goal");

    assert!(delete_goal(&db, &goal.id).expect("delete goal"));
    assert!(goal_by_id(&db, &goal.id).expect("query").is_none());
    assert!(
        !delete_goal(&db, &goal.id).expect("delete again"),
        "second delete returns false"
    );
}

/// Test 8: reflect_on_goal with no active goal, then with content, logs a
/// "reflect" entry and returns a confirmation naming the objective.
#[test]
fn reflect_on_goal_logs_and_confirms() {
    let db = Db::open(":memory:").expect("open memory db");
    let bot_id = create_test_bot(&db);

    let none_yet = reflect_on_goal(&db, &bot_id, "", "", Utc::now()).expect("reflect with no goal");
    assert_eq!(
        none_yet,
        "You have no active goal to reflect on. Call set_goal first."
    );

    let goal = create_goal(
        &db,
        CreateGoalInput {
            bot_id: bot_id.clone(),
            objective: "Investigate the outage".to_string(),
            done_when: "root cause known".to_string(),
            ..Default::default()
        },
        Utc::now(),
    )
    .expect("create goal");

    let empty = reflect_on_goal(&db, &bot_id, "  ", "  ", Utc::now()).expect("reflect blank");
    assert_eq!(
        empty,
        "Say what was unexpected or what comes next - reflect needs at least one."
    );

    let msg = reflect_on_goal(
        &db,
        &bot_id,
        "the logs were rotated",
        "check the backup host",
        Utc::now(),
    )
    .expect("reflect with content");
    assert_eq!(msg, "Logged on \"Investigate the outage\".");

    let row = goal_by_id(&db, &goal.id).expect("query").expect("exists");
    let listed = list_goals(&db, Some(&bot_id)).expect("list");
    let g = listed.iter().find(|g| g.id == row.id).unwrap();
    assert_eq!(g.log.len(), 1);
    assert_eq!(g.log[0].kind, "reflect");
    assert_eq!(
        g.log[0].text,
        "Unexpected: the logs were rotated Next: check the backup host"
    );
}

/// Test 9 (BITE 2): the log trims to MAX_LOG_ENTRIES, dropping the oldest
/// first. See Results for the red-run proving this actually bites.
#[test]
fn goal_log_trims_to_max_entries_dropping_oldest() {
    let db = Db::open(":memory:").expect("open memory db");
    let bot_id = create_test_bot(&db);

    let goal = create_goal(
        &db,
        CreateGoalInput {
            bot_id: bot_id.clone(),
            objective: "Long runner".to_string(),
            done_when: "never, this is a log-trim test".to_string(),
            ..Default::default()
        },
        Utc::now(),
    )
    .expect("create goal");

    let overflow = MAX_LOG_ENTRIES + 5;
    for i in 0..overflow {
        reflect_on_goal(&db, &bot_id, &format!("event {}", i), "", Utc::now()).expect("reflect");
    }

    let listed = list_goals(&db, Some(&bot_id)).expect("list");
    let g = listed.iter().find(|g| g.id == goal.id).unwrap();
    assert_eq!(
        g.log.len(),
        MAX_LOG_ENTRIES,
        "log capped at MAX_LOG_ENTRIES"
    );
    // Oldest dropped first: entry 0 is gone, the earliest surviving entry is
    // "event 5" (5 entries were pushed off the front).
    assert_eq!(g.log[0].text, "Unexpected: event 5");
    // Newest entry is still present at the end.
    assert_eq!(
        g.log[MAX_LOG_ENTRIES - 1].text,
        format!("Unexpected: event {}", overflow - 1)
    );
}

/// Test 10: budget_overrun fires on tokens spent past budget, on a deadline
/// passed, and stays quiet otherwise; an unparseable deadline never fires.
#[test]
fn budget_overrun_checks_tokens_and_deadline() {
    let db = Db::open(":memory:").expect("open memory db");
    let bot_id = create_test_bot(&db);

    let goal = create_goal(
        &db,
        CreateGoalInput {
            bot_id,
            objective: "Budget test".to_string(),
            done_when: "done".to_string(),
            budget_tokens: Some(100.0),
            budget_until: None,
        },
        Utc::now(),
    )
    .expect("create goal");
    let mut row = goal_by_id(&db, &goal.id).expect("query").expect("exists");

    assert!(
        budget_overrun(&row, Utc::now()).is_none(),
        "0 spent, no overrun"
    );

    row.spent_tokens = 150;
    let msg = budget_overrun(&row, Utc::now()).expect("overrun by tokens");
    assert_eq!(
        msg,
        "Stopped \"Budget test\": used 150 of its 100-token budget."
    );

    row.spent_tokens = 0;
    row.budget_tokens = None;
    row.budget_until = Some("2020-01-01T00:00:00Z".to_string());
    let msg = budget_overrun(&row, t("2026-09-15T00:00:00Z")).expect("overrun by deadline");
    assert_eq!(
        msg,
        "Stopped \"Budget test\": past its 2020-01-01T00:00:00Z deadline."
    );

    row.budget_until = Some("not a date".to_string());
    assert!(
        budget_overrun(&row, t("2026-09-15T00:00:00Z")).is_none(),
        "unparseable deadline never overruns"
    );
}

/// Test 11: due_for_weekly_report fires at/after WEEKLY_MS since the last
/// report (or since created_at, when no report has ever gone out).
#[test]
fn due_for_weekly_report_uses_last_report_or_created_at() {
    let created = t("2026-09-01T00:00:00Z");

    assert!(!due_for_weekly_report(
        None,
        "2026-09-01T00:00:00Z",
        t("2026-09-05T00:00:00Z")
    ));
    assert!(due_for_weekly_report(
        None,
        "2026-09-01T00:00:00Z",
        t("2026-09-08T00:00:00Z")
    ));

    assert!(!due_for_weekly_report(
        Some("2026-09-10T00:00:00Z"),
        "2026-09-01T00:00:00Z",
        t("2026-09-12T00:00:00Z")
    ));
    assert!(due_for_weekly_report(
        Some("2026-09-10T00:00:00Z"),
        "2026-09-01T00:00:00Z",
        t("2026-09-17T00:00:00Z")
    ));

    let _ = created;
}

/// Test 12: due_goals returns only active goals whose next_session_at has
/// passed.
#[test]
fn due_goals_filters_by_status_and_next_session_at() {
    let db = Db::open(":memory:").expect("open memory db");
    let bot_id = create_test_bot(&db);

    // Due right away (create_goal stamps next_session_at = created_at).
    let due = create_goal(
        &db,
        CreateGoalInput {
            bot_id: bot_id.clone(),
            objective: "Due now".to_string(),
            done_when: "done".to_string(),
            ..Default::default()
        },
        t("2026-09-14T00:00:00Z"),
    )
    .expect("create due goal");

    // Not due: created later than the `now` we check against.
    let not_due = create_goal(
        &db,
        CreateGoalInput {
            bot_id: bot_id.clone(),
            objective: "Not due".to_string(),
            done_when: "done".to_string(),
            ..Default::default()
        },
        t("2026-09-20T00:00:00Z"),
    )
    .expect("create future goal");

    // Paused: excluded even though next_session_at has passed.
    update_goal(
        &db,
        &due.id,
        &UpdateGoalPatch {
            status: Some("paused".to_string()),
            note: Some("pausing".to_string()),
            ..Default::default()
        },
        None,
        t("2026-09-14T00:00:00Z"),
    )
    .expect("pause");

    let results = due_goals(&db, t("2026-09-15T00:00:00Z")).expect("due goals");
    assert!(
        results.is_empty(),
        "paused goal excluded even though due, future goal not yet due"
    );

    update_goal(
        &db,
        &due.id,
        &UpdateGoalPatch {
            status: Some("active".to_string()),
            note: Some("resuming".to_string()),
            ..Default::default()
        },
        None,
        t("2026-09-14T00:00:00Z"),
    )
    .expect("resume");

    let results = due_goals(&db, t("2026-09-15T00:00:00Z")).expect("due goals");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, due.id);

    let _ = not_due;
}

/// Test 13: most_recent_active_goal picks the most recently sessioned
/// active goal, falling back to newest created when neither has run yet.
#[test]
fn most_recent_active_goal_picks_latest_session() {
    let db = Db::open(":memory:").expect("open memory db");
    let bot_id = create_test_bot(&db);

    let g1 = create_goal(
        &db,
        CreateGoalInput {
            bot_id: bot_id.clone(),
            objective: "First".to_string(),
            done_when: "done".to_string(),
            ..Default::default()
        },
        t("2026-09-14T00:00:00Z"),
    )
    .expect("create g1");
    let g2 = create_goal(
        &db,
        CreateGoalInput {
            bot_id: bot_id.clone(),
            objective: "Second".to_string(),
            done_when: "done".to_string(),
            ..Default::default()
        },
        t("2026-09-15T00:00:00Z"),
    )
    .expect("create g2");

    // Neither has a last_session_at yet - falls back to created_at DESC, so
    // the newer g2 wins.
    let picked = most_recent_active_goal(&db, &bot_id)
        .expect("query")
        .expect("one active goal found");
    assert_eq!(picked.id, g2.id);

    // Stamp g1 as having just run - it should now win.
    db.conn()
        .execute(
            "UPDATE goals SET last_session_at = ?1 WHERE id = ?2",
            params!["2026-09-16T00:00:00Z", &g1.id],
        )
        .expect("stamp last_session_at");

    let picked = most_recent_active_goal(&db, &bot_id)
        .expect("query")
        .expect("one active goal found");
    assert_eq!(picked.id, g1.id);
}

/// Test 14: goal_runs reads from `runs` filtered by goal_id, newest first,
/// capped at `limit` - the same shape `routine_runs` gives a routine, and
/// proof that the self-created `runs.goal_id` column actually works.
#[test]
fn goal_runs_returns_limited_list_newest_first() {
    let db = Db::open(":memory:").expect("open memory db");
    let bot_id = create_test_bot(&db);

    let goal = create_goal(
        &db,
        CreateGoalInput {
            bot_id: bot_id.clone(),
            objective: "Runs test".to_string(),
            done_when: "done".to_string(),
            ..Default::default()
        },
        Utc::now(),
    )
    .expect("create goal");

    let conv_id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    db.conn()
        .execute(
            "INSERT INTO conversations (id, bot_id, title, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![&conv_id, &bot_id, "Test", &now],
        )
        .expect("create conversation");

    for i in 0..25 {
        db.conn()
            .execute(
                "INSERT INTO runs (id, bot_id, conversation_id, trigger, status, model, messages, text, cost_usd, goal_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    Uuid::new_v4().to_string(),
                    &bot_id,
                    &conv_id,
                    "chat",
                    "done",
                    "anthropic/claude-fable-5.1",
                    "[]",
                    format!("Run {} result", i),
                    0.001,
                    &goal.id,
                    format!("2026-09-15T00:{:02}:00Z", i),
                    format!("2026-09-15T00:{:02}:00Z", i),
                ],
            )
            .expect("insert run");
    }

    let runs = goal_runs(&db, &goal.id, 20).expect("goal runs");
    assert_eq!(runs.len(), 20, "capped at 20, not all 25");
    assert_eq!(runs[0].text, "Run 24 result", "newest first");
}
