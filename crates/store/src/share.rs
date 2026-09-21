//! S11-05 / S5a: share tokens for unauthenticated bot export downloads.
//! Port of `projects/bullpen-night/src/server/share.ts` (table + persistence).

use crate::Db;
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareToken {
    pub token: String,
    pub bot_id: String,
    pub expires_at: String,
}

pub fn ensure_share_tokens_table(db: &Db) -> rusqlite::Result<()> {
    db.conn().execute_batch(
        "CREATE TABLE IF NOT EXISTS share_tokens (
            id           TEXT PRIMARY KEY,
            bot_id       TEXT NOT NULL REFERENCES bots(id),
            token        TEXT NOT NULL UNIQUE,
            created_at   TEXT NOT NULL,
            expires_at   TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_share_tokens_expires ON share_tokens(expires_at);",
    )
}

pub fn purge_expired(db: &Db, now: DateTime<Utc>) -> rusqlite::Result<()> {
    db.conn().execute(
        "DELETE FROM share_tokens WHERE expires_at < ?1",
        params![now.to_rfc3339()],
    )?;
    Ok(())
}

pub fn delete_for_bot(db: &Db, bot_id: &str) -> rusqlite::Result<()> {
    db.conn().execute(
        "DELETE FROM share_tokens WHERE bot_id = ?1",
        params![bot_id],
    )?;
    Ok(())
}

pub fn insert_token(
    db: &Db,
    id: &str,
    bot_id: &str,
    token: &str,
    created_at: &str,
    expires_at: &str,
) -> rusqlite::Result<()> {
    db.conn().execute(
        "INSERT INTO share_tokens (id, bot_id, token, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, bot_id, token, created_at, expires_at],
    )?;
    Ok(())
}

pub fn row_by_token(db: &Db, token: &str) -> rusqlite::Result<Option<(String, String)>> {
    db.conn()
        .query_row(
            "SELECT bot_id, token FROM share_tokens WHERE token = ?1",
            params![token],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
}

pub fn row_by_bot(db: &Db, bot_id: &str) -> rusqlite::Result<Option<(String, String)>> {
    db.conn()
        .query_row(
            "SELECT token, expires_at FROM share_tokens WHERE bot_id = ?1",
            params![bot_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
}

pub fn delete_for_bot_if_any(db: &Db, bot_id: &str) -> rusqlite::Result<bool> {
    let n = db.conn().execute(
        "DELETE FROM share_tokens WHERE bot_id = ?1",
        params![bot_id],
    )?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bots::{BotDraft, create_bot};

    #[test]
    fn share_token_round_trip_rows() {
        let db = Db::open(":memory:").expect("open");
        create_bot(
            &db,
            BotDraft {
                name: "Share Bot".to_string(),
                purpose: String::new(),
                instructions: "x".to_string(),
                model: None,
            },
        )
        .expect("bot");
        let now = Utc::now();
        let exp = (now + chrono::Duration::days(7)).to_rfc3339();
        insert_token(
            &db,
            "id1",
            "share-bot",
            "share-bot:abc:deadbeef",
            &now.to_rfc3339(),
            &exp,
        )
        .expect("insert");
        let row = row_by_bot(&db, "share-bot").expect("get").expect("some");
        assert_eq!(row.0, "share-bot:abc:deadbeef");
        let by_tok = row_by_token(&db, "share-bot:abc:deadbeef")
            .expect("q")
            .expect("found");
        assert_eq!(by_tok.0, "share-bot");
    }
}
