//! S4-01: auto-review judge tests.

mod common;

use model::ladder::Trigger;
use server::judge;
use server::judge::Verdict;
use std::sync::{Arc, Mutex};
use store::Db;

fn open_db() -> Arc<Mutex<Db>> {
    let db = Db::open(":memory:").expect("open :memory: db");
    model::routing::set_routing_settings(&db, Some(false), None)
        .expect("disable routing classifier for scripted-model tests");
    Arc::new(Mutex::new(db))
}

// ---- Verdict parsing ----

#[test]
fn test_verdict_parse_safe() {
    assert_eq!(Verdict::parse("safe"), Some(Verdict::Safe));
    assert_eq!(Verdict::Safe.as_str(), "safe");
}

#[test]
fn test_verdict_parse_risky() {
    assert_eq!(Verdict::parse("risky"), Some(Verdict::Risky));
    assert_eq!(Verdict::Risky.as_str(), "risky");
}

#[test]
fn test_verdict_parse_dangerous() {
    assert_eq!(Verdict::parse("dangerous"), Some(Verdict::Dangerous));
    assert_eq!(Verdict::Dangerous.as_str(), "dangerous");
}

#[test]
fn test_verdict_parse_invalid() {
    assert_eq!(Verdict::parse("invalid"), None);
}

// ---- Risky tools ----

#[test]
fn test_is_risky_shell() {
    assert!(judge::is_risky("shell"));
}

#[test]
fn test_is_risky_desk_shell() {
    assert!(judge::is_risky("desk_shell"));
}

#[test]
fn test_is_risky_ssh() {
    assert!(judge::is_risky("ssh"));
}

#[test]
fn test_is_risky_message_bot() {
    assert!(judge::is_risky("message_bot"));
}

#[test]
fn test_is_risky_read_file() {
    assert!(judge::is_risky("read_file"));
}

#[test]
fn test_is_risky_not_risky() {
    assert!(!judge::is_risky("some_other_tool"));
}

// ---- Decision logic ----

#[test]
fn test_decision_for_safe() {
    let decision = judge::decision_for(Verdict::Safe, None);
    assert_eq!(decision, server::permissions::Decision::Allow);
}

#[test]
fn test_decision_for_risky() {
    let decision = judge::decision_for(Verdict::Risky, None);
    assert_eq!(decision, server::permissions::Decision::Ask);
}

#[test]
fn test_decision_for_dangerous_chat() {
    let decision = judge::decision_for(Verdict::Dangerous, Some(&Trigger::Chat));
    assert_eq!(decision, server::permissions::Decision::Ask);
}

#[test]
fn test_decision_for_dangerous_routine() {
    let decision = judge::decision_for(Verdict::Dangerous, Some(&Trigger::Routine));
    assert_eq!(decision, server::permissions::Decision::Deny);
}

#[test]
fn test_decision_for_dangerous_none() {
    let decision = judge::decision_for(Verdict::Dangerous, None);
    assert_eq!(decision, server::permissions::Decision::Deny);
}

// ---- Judge enabled toggle ----

#[test]
fn test_judge_enabled_default_on() {
    let db = open_db();
    let db = db.lock().expect("db mutex poisoned");
    assert!(judge::judge_enabled(&db));
}

#[test]
fn test_set_judge_enabled_false() {
    let db = open_db();
    let db_guard = db.lock().expect("db mutex poisoned");
    judge::set_judge_enabled(&db_guard, false).expect("set_judge_enabled");
    assert!(!judge::judge_enabled(&db_guard));
}

#[test]
fn test_set_judge_enabled_true() {
    let db = open_db();
    let db_guard = db.lock().expect("db mutex poisoned");
    judge::set_judge_enabled(&db_guard, false).expect("set_judge_enabled");
    judge::set_judge_enabled(&db_guard, true).expect("set_judge_enabled");
    assert!(judge::judge_enabled(&db_guard));
}

// ---- Log and cap at 200 rows ----

#[test]
fn test_log_insert_caps_at_200() {
    let db = open_db();
    let db_guard = db.lock().expect("db mutex poisoned");

    // Insert 205 entries with incrementing timestamps
    for i in 0..205 {
        let hours = i / 60;
        let minutes = i % 60;
        let entry = store::auto_review::LogEntry {
            id: format!("id-{}", i),
            bot_id: "bot1".to_string(),
            run_id: format!("run-{}", i),
            tool_name: "shell".to_string(),
            description: format!("desc-{}", i),
            verdict: "safe".to_string(),
            reason: "no risk".to_string(),
            decision: "allow".to_string(),
            created_at: format!("2026-01-01T{:02}:{:02}:00Z", hours, minutes),
        };
        judge::log_judgement(&db_guard, entry).expect("log_judgement");
    }

    // Count should be exactly 200
    let logs = judge::list_log(&db_guard, 300).expect("list_log");
    assert_eq!(logs.len(), 200);

    // Newest should have id-204 (the last inserted)
    if let Some(first) = logs.first() {
        assert_eq!(first.id, "id-204");
    }

    // Oldest should be id-5 (since we keep 200 newest of 205, which are ids 5-204)
    if let Some(last) = logs.last() {
        assert_eq!(last.id, "id-5");
    }
}

#[test]
fn test_log_list_newest_first() {
    let db = open_db();
    let db_guard = db.lock().expect("db mutex poisoned");

    // Insert 3 entries with different times
    for i in 0..3 {
        let entry = store::auto_review::LogEntry {
            id: format!("id-{}", i),
            bot_id: "bot1".to_string(),
            run_id: format!("run-{}", i),
            tool_name: "shell".to_string(),
            description: format!("desc-{}", i),
            verdict: "safe".to_string(),
            reason: "no risk".to_string(),
            decision: "allow".to_string(),
            created_at: format!("2026-01-01T00:{:02}:00Z", i),
        };
        judge::log_judgement(&db_guard, entry).expect("log_judgement");
    }

    // List should be newest first
    let logs = judge::list_log(&db_guard, 10).expect("list_log");
    assert_eq!(logs.len(), 3);
    assert_eq!(logs[0].id, "id-2");
    assert_eq!(logs[1].id, "id-1");
    assert_eq!(logs[2].id, "id-0");
}

#[test]
fn test_log_list_with_limit() {
    let db = open_db();
    let db_guard = db.lock().expect("db mutex poisoned");

    // Insert 10 entries with incrementing timestamps
    for i in 0..10 {
        let entry = store::auto_review::LogEntry {
            id: format!("id-{}", i),
            bot_id: "bot1".to_string(),
            run_id: format!("run-{}", i),
            tool_name: "shell".to_string(),
            description: format!("desc-{}", i),
            verdict: "safe".to_string(),
            reason: "no risk".to_string(),
            decision: "allow".to_string(),
            created_at: format!("2026-01-01T00:00:{:02}Z", i),
        };
        judge::log_judgement(&db_guard, entry).expect("log_judgement");
    }

    // List with limit=3
    let logs = judge::list_log(&db_guard, 3).expect("list_log");
    assert_eq!(logs.len(), 3);
    assert_eq!(logs[0].id, "id-9");
}

// ---- Migration 20 ----

#[test]
fn test_migration_20_opens_fixture() {
    // Copy the fixture to a temp location
    let temp_path = std::env::temp_dir().join("test_migration_20.db");
    let fixture_path = "d:/rainmade/.scratch/bullpen-rs/fixtures/ts-made.db";
    std::fs::copy(fixture_path, &temp_path).expect("copy fixture to temp");

    // Open it and check version
    {
        let db = store::Db::open(temp_path.to_str().unwrap()).expect("open db");

        // Check PRAGMA user_version = 20
        let version: i32 = db
            .conn()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("query user_version");
        assert_eq!(version, 20);
        // db is dropped here, releasing the lock
    }

    // Cleanup
    std::fs::remove_file(&temp_path).expect("cleanup temp db");
}
