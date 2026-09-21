//! W6: away cache + utility spend log. Port of `away.ts` self-creating tables.

use crate::Db;

pub fn ensure_away_tables(db: &Db) -> rusqlite::Result<()> {
    db.conn().execute_batch(
        "CREATE TABLE IF NOT EXISTS away_state (
            id           INTEGER PRIMARY KEY CHECK (id = 1),
            epoch        TEXT,
            summary      TEXT,
            generated_at TEXT,
            dismissed_at TEXT,
            gap_hours    REAL
        );

        CREATE TABLE IF NOT EXISTS utility_calls (
            id         TEXT PRIMARY KEY,
            kind       TEXT NOT NULL,
            cost_usd   REAL NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL
        );",
    )
}
