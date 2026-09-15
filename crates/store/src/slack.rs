//! Slack thread-to-conversation mapping and trigger matching.
//! Port of TS `bullpen-night/src/server/slack.ts` (threads and trigger sections).

use crate::Db;
use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

use crate::conversations::now_iso;

/// Create the `slack_threads` table if it does not exist.
/// Self-creating to avoid concurrent builder conflicts on the MIGRATIONS array.
pub fn ensure_slack_tables(db: &Db) -> rusqlite::Result<()> {
    db.conn().execute_batch(
        "CREATE TABLE IF NOT EXISTS slack_threads (
            channel         TEXT NOT NULL,
            thread_ts       TEXT NOT NULL,
            bot_id          TEXT NOT NULL,
            conversation_id TEXT NOT NULL,
            created_at      TEXT NOT NULL,
            PRIMARY KEY (channel, thread_ts)
        );",
    )?;
    Ok(())
}

/// The Bullpen conversation for one Slack thread, creating it the first time
/// this exact (channel, thread_ts) pair is seen and reusing it every time
/// after - what keeps a back-and-forth in a Slack thread reading as one
/// conversation in Bullpen instead of a new one per message.
///
/// `thread_ts` is the THREAD's ts, not necessarily the message's own: a first
/// message in a channel has no `thread_ts` field at all, and its own `ts` IS
/// the thread it starts - the caller passes `event.thread_ts ?? event.ts`.
pub fn get_or_create_slack_conversation(
    db: &Db,
    bot_id: &str,
    channel: &str,
    thread_ts: &str,
) -> rusqlite::Result<String> {
    let existing = db
        .conn()
        .query_row(
            "SELECT conversation_id FROM slack_threads WHERE channel = ? AND thread_ts = ?",
            params![channel, thread_ts],
            |row| row.get::<_, String>(0),
        )
        .optional()?;

    if let Some(conversation_id) = existing {
        return Ok(conversation_id);
    }

    let conversation_id = Uuid::new_v4().to_string();
    let now = now_iso();

    db.conn().execute(
        "INSERT INTO conversations (id, bot_id, title, created_at) VALUES (?, ?, ?, ?)",
        params![conversation_id, bot_id, format!("Slack ({})", channel), now],
    )?;

    db.conn().execute(
        "INSERT INTO slack_threads (channel, thread_ts, bot_id, conversation_id, created_at) VALUES (?, ?, ?, ?, ?)",
        params![channel, thread_ts, bot_id, conversation_id, now],
    )?;

    Ok(conversation_id)
}

/// The four trigger shapes a `hook_kind: "slack"` routine can be set to fire on.
pub const SLACK_TRIGGER_KINDS: &[&str] = &["mention", "keyword", "message", "reaction"];

/// Whether one Slack event is the shape a routine's chosen trigger kind wants,
/// BEFORE `hook_match` (the existing free-text regex every other hook kind
/// already uses) is applied. "keyword" is deliberately just "message" here -
/// hook_match is what actually narrows it to a keyword, the same way an
/// ordinary raw webhook's hook_match narrows an arbitrary body.
pub fn slack_event_matches_trigger_kind(
    trigger_kind: &str,
    event_type: &str,
    is_mention: bool,
) -> bool {
    match trigger_kind {
        "reaction" => event_type == "reaction_added",
        "mention" => event_type == "app_mention" || is_mention,
        // "message" and "keyword" both want an ordinary message or a mention -
        // hook_match narrows "keyword" the rest of the way.
        _ => event_type == "message" || event_type == "app_mention",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slack_event_matches_trigger_kind_reaction() {
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
    }

    #[test]
    fn test_slack_event_matches_trigger_kind_mention() {
        assert!(slack_event_matches_trigger_kind(
            "mention",
            "app_mention",
            false
        ));
        assert!(slack_event_matches_trigger_kind("mention", "message", true));
        assert!(!slack_event_matches_trigger_kind(
            "mention", "message", false
        ));
    }

    #[test]
    fn test_slack_event_matches_trigger_kind_message() {
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
    }

    #[test]
    fn test_slack_event_matches_trigger_kind_keyword() {
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
}
