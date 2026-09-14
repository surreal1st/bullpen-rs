//! SQLite schema and queries. Same file format as the TS Bullpen's `bullpen.db`:
//! migrations 1..16 are equivalent, every self-creating table/column is here.

pub mod bots;
mod migrations;
pub mod roster;

pub use bots::{get_bot, list_bots, list_sections};
pub use roster::list_roster;

use rusqlite::{Connection, OptionalExtension, params};
use std::time::Duration;

/// A rusqlite connection carrying the Bullpen schema. Opens the same
/// `bullpen.db` byte-compatible with the TS server.
pub struct Db(Connection);

impl Db {
    /// Opens (or creates) the database at `path` (`:memory:` works),
    /// applying the same pragmas as the TS `openDb` (WAL, foreign_keys,
    /// busy_timeout) and running migrations 1..16 to bring `user_version`
    /// up to 16.
    pub fn open(path: &str) -> rusqlite::Result<Db> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(Duration::from_millis(5000))?;
        migrate(&conn)?;
        Ok(Db(conn))
    }

    /// `SELECT value FROM settings WHERE key = ?`.
    pub fn settings_get(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.0
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
    }

    /// Upsert, same as the TS `INSERT ... ON CONFLICT`.
    pub fn settings_set(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.0.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Runs self-creating schema (`ensure*` in the TS source) that later
    /// slices port. Not applied here; this ticket only carries migrations
    /// 1..16 and `settings`.
    pub fn ensure(&self, sql: &str) -> rusqlite::Result<()> {
        self.0.execute_batch(sql)
    }

    /// Raw connection access. Not a query method (no domain reads/writes
    /// beyond `settings` live in this crate yet) - it exists so callers
    /// (today: `tests/migrations.rs`) can inspect `PRAGMA user_version` and
    /// `sqlite_master` directly.
    pub fn conn(&self) -> &Connection {
        &self.0
    }
}

/// Mirrors the TS `migrate()`: reads `PRAGMA user_version`, then applies
/// `MIGRATIONS[version..]` in order, each in its own transaction, bumping
/// `user_version` after each one commits.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let mut version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;

    while (version as usize) < migrations::MIGRATIONS.len() {
        let sql = migrations::MIGRATIONS[version as usize];
        conn.execute_batch("BEGIN")?;
        match conn.execute_batch(sql) {
            Ok(()) => {
                version += 1;
                conn.pragma_update(None, "user_version", version)?;
                conn.execute_batch("COMMIT")?;
            }
            Err(err) => {
                conn.execute_batch("ROLLBACK")?;
                return Err(err);
            }
        }
    }

    Ok(())
}
