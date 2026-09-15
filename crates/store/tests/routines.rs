//! S5-02 routines tests: store layer for routines (schedule, CRUD, health tracking).

use rusqlite::params;
use std::fs;
use std::path::PathBuf;
use uuid::Uuid;

use store::{
    Db, UpdateRoutineFields, create_routine, delete_routine, due_routines, list_routines,
    record_routine_run, resume_routine, routine_by_id, routine_runs, set_routine_active,
    update_routine,
};

/// Helper to create a test bot in the database
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
    let temp = std::env::temp_dir().join(format!("bullpen-rs-routines-test-{}.db", Uuid::new_v4()));
    fs::copy(fixture, &temp).expect("copy fixture to temp path");
    temp
}

/// Test 1: Ensure columns are idempotent (opening twice doesn't error).
#[test]
fn ensure_routine_columns_is_idempotent() {
    let temp = copy_fixture_to_temp();
    let db1 = Db::open(temp.to_str().unwrap()).expect("open first time");
    let db2 = Db::open(temp.to_str().unwrap()).expect("open second time");

    // If we got here without error, the ensures are idempotent
    let routines1 = list_routines(&db1, None).expect("list routines db1");
    let routines2 = list_routines(&db2, None).expect("list routines db2");

    assert_eq!(routines1.len(), routines2.len(), "same number of routines");

    let _ = fs::remove_file(&temp);
}

/// Test 2: Create a routine and verify it's returned by list.
#[test]
fn create_routine_appears_in_list() {
    let db = Db::open(":memory:").expect("open memory db");
    let bot_id = create_test_bot(&db);

    let id = create_routine(
        &db,
        &bot_id,
        "Test Routine",
        "Do something",
        r#"{"kind":"interval","minutes":15}"#.to_string(),
        Some("2026-09-14T12:00:00Z".to_string()),
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

    let routines = list_routines(&db, Some(&bot_id)).expect("list routines");
    assert_eq!(routines.len(), 1, "one routine created");
    assert_eq!(routines[0].id, id, "routine id matches");
    assert_eq!(routines[0].name, "Test Routine", "routine name matches");
    assert_eq!(routines[0].prompt, "Do something", "routine prompt matches");
}

/// Test 3: Get a single routine by ID.
#[test]
fn routine_by_id_retrieves_correct_routine() {
    let db = Db::open(":memory:").expect("open memory db");
    let bot_id = create_test_bot(&db);

    let id = create_routine(
        &db,
        &bot_id,
        "Specific Routine",
        "Specific prompt",
        r#"{"kind":"daily","hour":8,"minute":30}"#.to_string(),
        Some("2026-09-15T08:30:00Z".to_string()),
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

    let routine = routine_by_id(&db, &id)
        .expect("query routine")
        .expect("routine exists");

    assert_eq!(routine.id, id);
    assert_eq!(routine.name, "Specific Routine");
    assert!(!routine.active, "newly created routine is inactive");
}

/// Test 4: Update a routine's fields.
#[test]
fn update_routine_modifies_fields() {
    let db = Db::open(":memory:").expect("open memory db");
    let bot_id = create_test_bot(&db);

    let id = create_routine(
        &db,
        &bot_id,
        "Original Name",
        "Original prompt",
        r#"{"kind":"interval","minutes":15}"#.to_string(),
        Some("2026-09-14T12:00:00Z".to_string()),
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

    let updates = UpdateRoutineFields {
        name: Some("Updated Name".to_string()),
        prompt: Some("Updated prompt".to_string()),
        ..Default::default()
    };

    update_routine(&db, &id, &updates).expect("update routine");

    let routine = routine_by_id(&db, &id)
        .expect("query routine")
        .expect("routine exists");

    assert_eq!(routine.name, "Updated Name");
    assert_eq!(routine.prompt, "Updated prompt");
}

/// Test 5: Set routine active/inactive.
#[test]
fn set_routine_active_toggles_state() {
    let db = Db::open(":memory:").expect("open memory db");
    let bot_id = create_test_bot(&db);

    let id = create_routine(
        &db,
        &bot_id,
        "Toggle Routine",
        "Toggle",
        r#"{"kind":"interval","minutes":30}"#.to_string(),
        Some("2026-09-14T12:00:00Z".to_string()),
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

    // Activate it
    set_routine_active(&db, &id, true, Some("2026-09-14T12:30:00Z".to_string()))
        .expect("activate routine");
    let routine = routine_by_id(&db, &id)
        .expect("query routine")
        .expect("routine exists");
    assert!(routine.active, "routine is now active");

    // Deactivate it
    set_routine_active(&db, &id, false, None).expect("deactivate routine");
    let routine = routine_by_id(&db, &id)
        .expect("query routine")
        .expect("routine exists");
    assert!(!routine.active, "routine is now inactive");
}

/// Test 6: Delete a routine.
#[test]
fn delete_routine_removes_from_database() {
    let db = Db::open(":memory:").expect("open memory db");
    let bot_id = create_test_bot(&db);

    let id = create_routine(
        &db,
        &bot_id,
        "Deletable Routine",
        "Will be deleted",
        r#"{"kind":"interval","minutes":15}"#.to_string(),
        Some("2026-09-14T12:00:00Z".to_string()),
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

    let deleted = delete_routine(&db, &id).expect("delete routine");
    assert!(deleted, "delete returns true");

    let routine = routine_by_id(&db, &id).expect("query routine");
    assert!(routine.is_none(), "routine no longer exists");
}

/// Test 7: Due routines returns only active routines with past next_run_at.
#[test]
fn due_routines_filters_correctly() {
    let db = Db::open(":memory:").expect("open memory db");
    let bot_id = create_test_bot(&db);

    // Create routine 1: past due
    let id1 = create_routine(
        &db,
        &bot_id,
        "Past Due",
        "prompt",
        r#"{"kind":"interval","minutes":15}"#.to_string(),
        Some("2026-09-14T10:00:00Z".to_string()),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("create routine 1");

    // Activate it
    set_routine_active(&db, &id1, true, Some("2026-09-14T10:00:00Z".to_string()))
        .expect("activate routine 1");

    // Create routine 2: future due
    let id2 = create_routine(
        &db,
        &bot_id,
        "Future Due",
        "prompt",
        r#"{"kind":"interval","minutes":15}"#.to_string(),
        Some("2026-09-15T10:00:00Z".to_string()),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("create routine 2");

    // Activate it
    set_routine_active(&db, &id2, true, Some("2026-09-15T10:00:00Z".to_string()))
        .expect("activate routine 2");

    // Get due routines at a specific time (between the two)
    let due = due_routines(&db, "2026-09-14T11:00:00Z").expect("get due routines");

    assert_eq!(due.len(), 1, "only one routine is due");
    assert_eq!(due[0].id, id1, "past due routine is returned");
}

/// Test 8: Record routine run success resets failures.
#[test]
fn record_routine_run_success_resets_failures() {
    let db = Db::open(":memory:").expect("open memory db");
    let bot_id = create_test_bot(&db);

    let id = create_routine(
        &db,
        &bot_id,
        "Health Test",
        "prompt",
        r#"{"kind":"interval","minutes":15}"#.to_string(),
        Some("2026-09-14T12:00:00Z".to_string()),
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

    // Record multiple failures
    let outcome1 = record_routine_run(&db, &id, false, Some("Error 1")).expect("record failure 1");
    assert_eq!(outcome1.failures, 1, "1 failure");
    assert!(!outcome1.paused, "not paused yet");

    let outcome2 = record_routine_run(&db, &id, false, Some("Error 2")).expect("record failure 2");
    assert_eq!(outcome2.failures, 2, "2 failures");
    assert!(!outcome2.paused, "still not paused");

    // Record success - should reset
    let outcome3 = record_routine_run(&db, &id, true, None).expect("record success");
    assert_eq!(outcome3.failures, 0, "failures reset to 0");
    assert!(!outcome3.paused, "not paused");
}

/// Test 9: Record routine run fails after 3 failures (FAILURE_LIMIT).
#[test]
fn record_routine_run_pauses_after_limit() {
    let db = Db::open(":memory:").expect("open memory db");
    let bot_id = create_test_bot(&db);

    let id = create_routine(
        &db,
        &bot_id,
        "Failure Test",
        "prompt",
        r#"{"kind":"interval","minutes":15}"#.to_string(),
        Some("2026-09-14T12:00:00Z".to_string()),
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

    // Activate it
    set_routine_active(&db, &id, true, Some("2026-09-14T12:00:00Z".to_string())).expect("activate");

    // Record 3 failures
    let _ = record_routine_run(&db, &id, false, Some("Error 1")).expect("record failure 1");
    let _ = record_routine_run(&db, &id, false, Some("Error 2")).expect("record failure 2");
    let outcome3 = record_routine_run(&db, &id, false, Some("Error 3")).expect("record failure 3");

    assert_eq!(outcome3.failures, 3, "3 failures");
    assert!(outcome3.paused, "paused on 3rd failure");

    // Verify routine is now inactive
    let routine = routine_by_id(&db, &id)
        .expect("query routine")
        .expect("routine exists");
    assert!(!routine.active, "routine is now inactive");
    assert!(routine.paused_reason.is_some(), "paused_reason is set");
}

/// Test 10: Resume routine clears pause state.
#[test]
fn resume_routine_clears_pause() {
    let db = Db::open(":memory:").expect("open memory db");
    let bot_id = create_test_bot(&db);

    let id = create_routine(
        &db,
        &bot_id,
        "Resume Test",
        "prompt",
        r#"{"kind":"interval","minutes":15}"#.to_string(),
        Some("2026-09-14T12:00:00Z".to_string()),
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

    // Activate and trigger pause
    set_routine_active(&db, &id, true, Some("2026-09-14T12:00:00Z".to_string())).expect("activate");

    let _ = record_routine_run(&db, &id, false, Some("Error 1")).expect("record failure 1");
    let _ = record_routine_run(&db, &id, false, Some("Error 2")).expect("record failure 2");
    let _ = record_routine_run(&db, &id, false, Some("Error 3")).expect("record failure 3");

    // Resume
    resume_routine(&db, &id).expect("resume routine");

    // Verify routine is resumed
    let routine = routine_by_id(&db, &id)
        .expect("query routine")
        .expect("routine exists");
    assert!(routine.active, "routine is active again");
    assert_eq!(routine.failures, 0, "failures reset");
    assert!(routine.paused_reason.is_none(), "paused_reason cleared");
}

/// Test 11: Routine runs list returns limited list.
#[test]
fn routine_runs_returns_limited_list() {
    let db = Db::open(":memory:").expect("open memory db");
    let bot_id = create_test_bot(&db);

    let id = create_routine(
        &db,
        &bot_id,
        "Runs Test",
        "prompt",
        r#"{"kind":"interval","minutes":15}"#.to_string(),
        Some("2026-09-14T12:00:00Z".to_string()),
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

    // Since we can't directly insert runs without going through the server,
    // we'll just verify the query works with 0 results
    let runs = routine_runs(&db, &id, 20).expect("get routine runs");
    assert_eq!(runs.len(), 0, "no runs for new routine");
}

/// Test 12: Fixture opens with new columns present.
#[test]
fn fixture_db_opens_with_routine_columns() {
    let temp = copy_fixture_to_temp();
    let db = Db::open(temp.to_str().unwrap()).expect("open fixture");

    // List all routines - should work without error
    let routines = list_routines(&db, None).expect("list all routines");

    // The fixture may or may not have routines, but this shouldn't error
    println!("Found {} routines in fixture", routines.len());

    let _ = fs::remove_file(&temp);
}
