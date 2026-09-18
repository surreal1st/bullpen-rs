//! COST-01 acceptance: `messages.cost_unknown` (migration 21). Never
//! computed from a call this crate makes - only from the `cost_known` flag
//! the caller hands `append_message` on `NewMessage.usage`, mirroring
//! `model::ModelUsage::cost_known`.

use rusqlite::params;
use store::{Db, NewMessage, Usage};

fn open_memory_with_bot(id: &str) -> Db {
    let db = Db::open(":memory:").expect("open :memory:");
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, created_at) VALUES (?1, ?1, '2026-01-01T00:00:00.000Z')",
            params![id],
        )
        .expect("seed bot");
    db
}

fn cost_unknown_of(db: &Db, message_id: &str) -> i64 {
    db.conn()
        .query_row(
            "SELECT cost_unknown FROM messages WHERE id = ?1",
            params![message_id],
            |row| row.get(0),
        )
        .expect("read cost_unknown")
}

/// A message written from usage the provider actually priced must not be
/// flagged unknown.
#[test]
fn a_priced_message_has_cost_unknown_zero() {
    let db = open_memory_with_bot("bot-0");
    let conversation_id =
        store::get_or_create_conversation(&db, "bot-0").expect("get_or_create_conversation");

    let message = store::append_message(
        &db,
        &conversation_id,
        "assistant",
        "done",
        NewMessage {
            bot_id: Some("bot-0".to_string()),
            usage: Some(Usage {
                cost_usd: 0.05,
                input_tokens: 100,
                output_tokens: 20,
                cached_tokens: 0,
                cost_known: true,
            }),
            ..Default::default()
        },
    )
    .expect("append priced message");

    assert_eq!(cost_unknown_of(&db, &message.id), 0);
}

/// COST-01's own case: a usage frame carried token counts but no cost.
/// `cost_known: false` on the way in must land as `cost_unknown = 1` in the
/// row, not silently as `0` (which is exactly the "reads as free" bug this
/// ticket exists to close).
#[test]
fn a_message_written_from_unpriced_usage_has_cost_unknown_one() {
    let db = open_memory_with_bot("bot-0");
    let conversation_id =
        store::get_or_create_conversation(&db, "bot-0").expect("get_or_create_conversation");

    let message = store::append_message(
        &db,
        &conversation_id,
        "assistant",
        "done",
        NewMessage {
            bot_id: Some("bot-0".to_string()),
            usage: Some(Usage {
                cost_usd: 0.0,
                input_tokens: 100,
                output_tokens: 20,
                cached_tokens: 0,
                cost_known: false,
            }),
            ..Default::default()
        },
    )
    .expect("append unpriced message");

    assert_eq!(cost_unknown_of(&db, &message.id), 1);
}

/// A message with no usage at all (a user turn, or an assistant turn that
/// never called the model) is not the "unknown cost" case this ticket
/// tracks - it stays 0, same as every pre-migration-21 row.
#[test]
fn a_message_with_no_usage_at_all_has_cost_unknown_zero() {
    let db = open_memory_with_bot("bot-0");
    let conversation_id =
        store::get_or_create_conversation(&db, "bot-0").expect("get_or_create_conversation");

    let message = store::append_message(&db, &conversation_id, "user", "hi", NewMessage::default())
        .expect("append message with no usage");

    assert_eq!(cost_unknown_of(&db, &message.id), 0);
}
