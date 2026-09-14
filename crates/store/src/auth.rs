//! Login/session reads. Schema is migration 13 (`auth_settings`, `sessions`);
//! see `crates/store/src/migrations.rs`. No writes here yet - S0-04 only needs
//! to answer `/api/auth/status` and `/api/auth/check`.

use crate::Db;
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};

/// True once a password has been set: `auth_settings` carries its single row
/// (`id = 1`, enforced by a CHECK constraint - there is never more than one).
pub fn is_configured(db: &Db) -> rusqlite::Result<bool> {
    let row: Option<i64> = db
        .conn()
        .query_row("SELECT 1 FROM auth_settings WHERE id = 1", [], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(row.is_some())
}

/// True when `token` names a session that has not expired. Mirrors the TS
/// `sessionValid`: an expired row is deleted on the way out so the table
/// does not grow forever. An empty token is never valid and is never queried.
pub fn session_valid(db: &Db, token: &str) -> rusqlite::Result<bool> {
    if token.is_empty() {
        return Ok(false);
    }

    let expires_at: Option<String> = db
        .conn()
        .query_row(
            "SELECT expires_at FROM sessions WHERE token = ?1",
            params![token],
            |row| row.get(0),
        )
        .optional()?;

    let Some(expires_at) = expires_at else {
        return Ok(false);
    };

    let expired = DateTime::parse_from_rfc3339(&expires_at)
        .map(|dt| dt <= Utc::now())
        .unwrap_or(true); // an unparsable timestamp is never valid

    if expired {
        db.conn()
            .execute("DELETE FROM sessions WHERE token = ?1", params![token])?;
        return Ok(false);
    }

    Ok(true)
}
