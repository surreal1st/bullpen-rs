//! S1-01 acceptance tests: conversations, messages and rooms (cap 6).

use std::fs;

use rusqlite::params;
use store::{Db, NewMessage};

/// Copies the fixture to a fresh temp path so no test opens it in place -
/// same pattern `tests/migrations.rs` uses.
fn open_fixture_copy() -> Db {
    let fixture = "d:/rainmade/.scratch/bullpen-rs/fixtures/ts-made.db";
    let temp = std::env::temp_dir().join(format!(
        "bullpen-rs-store-rooms-{}.db",
        uuid::Uuid::new_v4()
    ));
    fs::copy(fixture, &temp).expect("copy fixture to temp path");
    Db::open(temp.to_str().expect("temp path is valid utf-8")).expect("open fixture copy")
}

/// A fresh `:memory:` db seeded with `n` bots (`bot-1`..`bot-n`), for tests
/// that need real, non-archived bot ids but not the fixture's history.
fn open_memory_with_bots(n: usize) -> Db {
    let db = Db::open(":memory:").expect("open :memory:");
    for i in 0..n {
        db.conn()
            .execute(
                "INSERT INTO bots (id, name, created_at) VALUES (?1, ?2, ?3)",
                params![
                    format!("bot-{i}"),
                    format!("Bot {i}"),
                    "2026-01-01T00:00:00.000Z"
                ],
            )
            .expect("seed bot");
    }
    db
}

/// Inserts one more bot directly, bypassing `bots.rs`'s create path (this
/// slice doesn't own one). The fixture's `bots` table already carries every
/// self-created column (`effort`, `voice`, `is_template`...) from the TS
/// side, so a minimal insert is enough - unlike a fresh `:memory:` db, whose
/// `bots` table only has migrations 1..16's columns and would reject
/// `get_bot`'s `SELECT` for want of those.
fn seed_bot(db: &Db, id: &str) {
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, created_at) VALUES (?1, ?1, '2026-01-01T00:00:00.000Z')",
            params![id],
        )
        .expect("seed bot");
}

/// 1. The fixture's room ("Growth") lists with 3 member ids incl. `arthur`,
///    `kind == "room"`; `list_messages` on it returns 3 messages ordered by
///    seq.
#[test]
fn fixture_room_lists_with_members_and_ordered_messages() {
    let db = open_fixture_copy();

    let rooms = store::list_rooms(&db).expect("list_rooms");
    let growth = rooms
        .iter()
        .find(|r| r.title == "Growth")
        .expect("fixture has a Growth room");

    assert_eq!(growth.member_ids.len(), 3);
    assert!(growth.member_ids.contains(&"arthur".to_string()));

    let conversation = store::get_conversation(&db, &growth.id)
        .expect("get_conversation")
        .expect("Growth room's conversation exists");
    assert_eq!(conversation.kind, "room");

    let messages = store::list_messages(&db, &growth.id).expect("list_messages");
    assert_eq!(messages.len(), 3);
    assert!(messages[0].content.contains("SAM.gov"));
    assert!(messages[1].content.contains("keywords"));
    assert!(messages[2].content.contains("have a look"));
}

/// 2. `create_room("Solo", ["arthur"])` -> Err containing "at least two";
///    7 ids -> Err containing "at most six"; 6 ids -> Ok.
#[test]
fn create_room_enforces_the_member_cap() {
    let db = open_fixture_copy(); // arthur, riley, jason already exist

    let too_few = store::create_room(&db, "Solo", &["arthur".to_string()]);
    let err = too_few.expect_err("one member must be refused");
    assert!(
        err.contains("at least two"),
        "expected 'at least two' in {err:?}"
    );

    let seven: Vec<String> = (0..7).map(|i| format!("nobody-{i}")).collect();
    let too_many = store::create_room(&db, "Too Many", &seven);
    let err = too_many.expect_err("seven members must be refused");
    assert!(
        err.contains("at most six"),
        "expected 'at most six' in {err:?}"
    );

    for id in ["extra-1", "extra-2", "extra-3"] {
        seed_bot(&db, id);
    }
    let six = vec![
        "arthur".to_string(),
        "riley".to_string(),
        "jason".to_string(),
        "extra-1".to_string(),
        "extra-2".to_string(),
        "extra-3".to_string(),
    ];
    let ok = store::create_room(&db, "Six", &six).expect("six members must be accepted");
    assert_eq!(ok.member_ids, six);
}

/// 3. `append_message` then `list_messages` round-trips content, role,
///    bot_id, and increments seq; `delete_message` removes it.
#[test]
fn append_list_and_delete_message_round_trip() {
    let db = open_memory_with_bots(1);
    let conversation_id =
        store::get_or_create_conversation(&db, "bot-0").expect("get_or_create_conversation");

    let first = store::append_message(
        &db,
        &conversation_id,
        "user",
        "hello",
        NewMessage::default(),
    )
    .expect("append first message");

    let second = store::append_message(
        &db,
        &conversation_id,
        "assistant",
        "hi there",
        NewMessage {
            bot_id: Some("bot-0".to_string()),
            ..Default::default()
        },
    )
    .expect("append second message");

    let messages = store::list_messages(&db, &conversation_id).expect("list_messages");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].id, first.id);
    assert_eq!(messages[0].content, "hello");
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[0].bot_id, None);
    assert_eq!(messages[1].id, second.id);
    assert_eq!(messages[1].content, "hi there");
    assert_eq!(messages[1].role, "assistant");
    assert_eq!(messages[1].bot_id, Some("bot-0".to_string()));

    store::delete_message(&db, &first.id).expect("delete_message");
    let after_delete = store::list_messages(&db, &conversation_id).expect("list_messages");
    assert_eq!(after_delete.len(), 1);
    assert_eq!(after_delete[0].id, second.id);
}

/// 4. `get_or_create_conversation("riley")` returns the same id twice.
#[test]
fn get_or_create_conversation_is_idempotent() {
    let db = open_memory_with_bots(1);

    let first = store::get_or_create_conversation(&db, "bot-0")
        .expect("get_or_create_conversation (first call)");
    let second = store::get_or_create_conversation(&db, "bot-0")
        .expect("get_or_create_conversation (second call)");

    assert_eq!(first, second);
}
