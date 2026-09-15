//! Tests for Slack thread-to-conversation mapping.

use rusqlite::OptionalExtension;
use store::Db;
use store::slack::{
    ensure_slack_tables, get_or_create_slack_conversation, slack_event_matches_trigger_kind,
};
use uuid::Uuid;

/// Helper to create a test bot in the database.
fn create_test_bot(db: &Db) -> String {
    let bot_id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, created_at) VALUES (?, ?, ?)",
            rusqlite::params![bot_id, "test_bot", now],
        )
        .expect("insert test bot");
    bot_id
}

#[test]
fn test_ensure_slack_tables() {
    let db = Db::open(":memory:").expect("open in-memory db");
    ensure_slack_tables(&db).expect("ensure slack tables");

    // Verify table exists by querying it
    let exists: bool = db
        .conn()
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='slack_threads'",
            [],
            |_| Ok(true),
        )
        .optional()
        .expect("query")
        .is_some();
    assert!(exists, "slack_threads table should exist");
}

#[test]
fn test_get_or_create_slack_conversation_creates_new() {
    let db = Db::open(":memory:").expect("open in-memory db");
    ensure_slack_tables(&db).expect("ensure slack tables");

    let bot_id = create_test_bot(&db);
    let channel = "#general";
    let thread_ts = "1234567890.123456";

    let conversation_id = get_or_create_slack_conversation(&db, &bot_id, channel, thread_ts)
        .expect("create conversation");

    // Verify conversation was created
    let title: String = db
        .conn()
        .query_row(
            "SELECT title FROM conversations WHERE id = ?",
            [&conversation_id],
            |row| row.get(0),
        )
        .expect("get conversation title");
    assert_eq!(title, "Slack (#general)");

    // Verify slack_threads entry was created
    let stored_id: String = db
        .conn()
        .query_row(
            "SELECT conversation_id FROM slack_threads WHERE channel = ? AND thread_ts = ?",
            [channel, thread_ts],
            |row| row.get(0),
        )
        .expect("get slack thread");
    assert_eq!(stored_id, conversation_id);
}

#[test]
fn test_get_or_create_slack_conversation_reuses_existing() {
    let db = Db::open(":memory:").expect("open in-memory db");
    ensure_slack_tables(&db).expect("ensure slack tables");

    let bot_id = create_test_bot(&db);
    let channel = "#general";
    let thread_ts = "1234567890.123456";

    let conversation_id_1 = get_or_create_slack_conversation(&db, &bot_id, channel, thread_ts)
        .expect("create conversation");

    // Call again with same parameters
    let conversation_id_2 = get_or_create_slack_conversation(&db, &bot_id, channel, thread_ts)
        .expect("reuse conversation");

    assert_eq!(
        conversation_id_1, conversation_id_2,
        "should reuse same conversation"
    );

    // Verify only one entry in slack_threads
    let count: i32 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM slack_threads WHERE channel = ? AND thread_ts = ?",
            [channel, thread_ts],
            |row| row.get(0),
        )
        .expect("count entries");
    assert_eq!(count, 1, "should have exactly one slack_threads entry");
}

#[test]
fn test_get_or_create_slack_conversation_different_threads() {
    let db = Db::open(":memory:").expect("open in-memory db");
    ensure_slack_tables(&db).expect("ensure slack tables");

    let bot_id = create_test_bot(&db);
    let channel = "#general";

    let conversation_id_1 =
        get_or_create_slack_conversation(&db, &bot_id, channel, "1111111111.111111")
            .expect("create conversation 1");

    let conversation_id_2 =
        get_or_create_slack_conversation(&db, &bot_id, channel, "2222222222.222222")
            .expect("create conversation 2");

    assert_ne!(
        conversation_id_1, conversation_id_2,
        "different threads should get different conversations"
    );
}

#[test]
fn test_slack_event_matches_trigger_kind_truth_table() {
    // reaction: only matches "reaction_added" eventType
    assert!(slack_event_matches_trigger_kind(
        "reaction",
        "reaction_added",
        false
    ));
    assert!(!slack_event_matches_trigger_kind(
        "reaction", "message", false
    ));
    assert!(!slack_event_matches_trigger_kind(
        "reaction",
        "app_mention",
        false
    ));

    // mention: matches "app_mention" or isMention=true
    assert!(slack_event_matches_trigger_kind(
        "mention",
        "app_mention",
        false
    ));
    assert!(slack_event_matches_trigger_kind("mention", "message", true));
    assert!(!slack_event_matches_trigger_kind(
        "mention", "message", false
    ));
    assert!(!slack_event_matches_trigger_kind(
        "mention",
        "reaction_added",
        false
    ));

    // message: matches "message" or "app_mention"
    assert!(slack_event_matches_trigger_kind(
        "message", "message", false
    ));
    assert!(slack_event_matches_trigger_kind(
        "message",
        "app_mention",
        false
    ));
    assert!(!slack_event_matches_trigger_kind(
        "message",
        "reaction_added",
        false
    ));

    // keyword: same as message (hook_match narrows it)
    assert!(slack_event_matches_trigger_kind(
        "keyword", "message", false
    ));
    assert!(slack_event_matches_trigger_kind(
        "keyword",
        "app_mention",
        false
    ));
    assert!(!slack_event_matches_trigger_kind(
        "keyword",
        "reaction_added",
        false
    ));
}
