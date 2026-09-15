//! SQLite schema and queries. Same file format as the TS Bullpen's `bullpen.db`:
//! migrations 1..16 are equivalent, every self-creating table/column is here.

pub mod auth;
pub mod auto_review;
pub mod bots;
pub mod conversations;
pub mod goals;
pub mod memory;
pub mod messages;
mod migrations;
pub mod questions;
pub mod rooms;
pub mod roster;
pub mod routines;
pub mod slack;
pub mod vms;

pub use auth::{
    PasswordRecord, create_session, destroy_session, is_configured, password_record, session_valid,
    set_password, verify_password,
};
pub use bots::{get_bot, get_bot_egress, list_bots, list_sections, set_bot_egress};
pub use conversations::{
    archive_thread, create_thread, get_conversation, get_or_create_conversation, list_threads,
    rename_thread, title_from_first_message, touch_thread, validate_members,
};
pub use memory::{
    LogEntry, Project, RECALL_TOKEN_BUDGET, Recall, Scope, add_project_member, count_scoped,
    create_project, forget, get_core, get_shared_core, note, projects_for, recall_for, recent_log,
    recent_shared_log, remember, remember_scoped, scoped_entries, search_log, set_core,
    set_shared_core, sweep_expired,
};
pub use messages::{NewMessage, Usage, append_message, delete_message, list_messages};
pub use questions::{OpenQuestion, answer_question, insert_question, list_all_open, list_open};
pub use rooms::{create_room, get_room, list_rooms, mark_room_seen, mark_room_unread, update_room};
pub use roster::{first_line, list_roster};
pub use routines::{
    Condition, HealthOutcome, Routine, RoutineRow, RoutineRun, UpdateRoutineFields,
    clear_routine_hook, create_routine, delete_routine, due_routines, list_routines,
    mint_routine_hook, record_routine_run, resume_routine, routine_by_id, routine_row_by_id,
    routine_runs, set_routine_active, update_routine,
};

use rusqlite::{Connection, OptionalExtension, params};
use std::time::Duration;

/// The tables `ensure_column` (B22) is allowed to touch - closed so the
/// `PRAGMA table_info(...)` it builds can never be handed a name read out of
/// a row.
#[derive(Clone, Copy)]
enum Table {
    Conversations,
    Messages,
    Bots,
    Runs,
}

impl Table {
    fn as_str(self) -> &'static str {
        match self {
            Table::Conversations => "conversations",
            Table::Messages => "messages",
            Table::Bots => "bots",
            Table::Runs => "runs",
        }
    }
}

/// A rusqlite connection carrying the Bullpen schema. Opens the same
/// `bullpen.db` byte-compatible with the TS server.
pub struct Db(Connection);

impl Db {
    /// Opens (or creates) the database at `path` (`:memory:` works),
    /// applying the same pragmas as the TS `openDb` (WAL, foreign_keys,
    /// busy_timeout) and running every migration to bring `user_version`
    /// up to date.
    pub fn open(path: &str) -> rusqlite::Result<Db> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(Duration::from_millis(5000))?;
        migrate(&conn)?;
        let db = Db(conn);

        // S1-01: self-creating columns, same pattern (and same reason) as the
        // TS `ensure*Column` functions called from `openDb` - two sessions
        // appending to MIGRATIONS at once silently skips whichever lands
        // second, so schema landing outside a slice window creates itself
        // instead, which is order-independent and idempotent. Mirrors
        // `ensureConversationRoomColumns` (threads.ts:26-35),
        // `ensureMessageAuthorColumn` (routine-health.ts) and
        // `ensureMessageReactionColumn` (store.ts).
        // S6-W-02 (converted from a numbered migration by the orchestrator):
        // a bot's egress policy. Self-creating for the same reason every
        // other self-added column here is - a numbered migration bumps
        // user_version on a database the live TypeScript Bullpen also opens,
        // and `tests/migrations.rs` went red the moment one was added.
        db.ensure_column(
            Table::Bots,
            "egress",
            r#"ALTER TABLE bots ADD COLUMN egress TEXT NOT NULL DEFAULT '{"mode":"off","allow":[]}'"#,
        )?;
        db.ensure_column(
            Table::Conversations,
            "kind",
            "ALTER TABLE conversations ADD COLUMN kind TEXT NOT NULL DEFAULT 'chat'",
        )?;
        db.ensure_column(
            Table::Conversations,
            "members",
            "ALTER TABLE conversations ADD COLUMN members TEXT",
        )?;
        db.ensure_column(
            Table::Conversations,
            "seen_at",
            "ALTER TABLE conversations ADD COLUMN seen_at TEXT",
        )?;
        db.ensure_column(
            Table::Messages,
            "bot_id",
            "ALTER TABLE messages ADD COLUMN bot_id TEXT",
        )?;
        db.ensure_column(
            Table::Messages,
            "reactions",
            "ALTER TABLE messages ADD COLUMN reactions TEXT",
        )?;
        // S1-04: `bots.rs` (`get_bot`/`list_bots`) already reads these three -
        // self-creating in the TS original too (`ensureBotsEffortColumn`,
        // `ensureBotVoiceColumn`, `ensureBotIsTemplateColumn`), never a
        // MIGRATIONS entry. Missing here meant `get_bot` threw "no such
        // column: effort" on any database that only ran migrations 1..16 -
        // every fresh `:memory:` db, just not the checked-in fixture, which
        // already carries them from the TS export.
        db.ensure_column(
            Table::Bots,
            "effort",
            "ALTER TABLE bots ADD COLUMN effort TEXT",
        )?;
        db.ensure_column(
            Table::Bots,
            "voice",
            "ALTER TABLE bots ADD COLUMN voice TEXT",
        )?;
        db.ensure_column(
            Table::Bots,
            "is_template",
            "ALTER TABLE bots ADD COLUMN is_template INTEGER NOT NULL DEFAULT 0",
        )?;

        questions::ensure_table(&db)?;
        routines::ensure_routine_columns(&db)?;
        // S5b-F: TS `ensureRunToolsColumn` (`runs.ts:148-153`) - the
        // per-run tool narrowing (`[]` = ALWAYS_ON only) and the tool a
        // tool-kind routine ran, written by `recordToolRun`. Same
        // self-creating shape as `goal_id` in `goals::ensure_goal_tables`.
        db.ensure_column(
            Table::Runs,
            "tools",
            "ALTER TABLE runs ADD COLUMN tools TEXT",
        )?;
        goals::ensure_goal_tables(&db)?;
        // S5c-F-02 (F5): was only wired into `server::AppState::build`
        // (S5c-03's stopgap), so a bare `store::Db::open` got a database
        // with no `slack_threads` - the same self-creating discipline as
        // `ensure_goal_tables` above, moved to the central choke point every
        // `Db` passes through.
        slack::ensure_slack_tables(&db)?;
        vms::ensure_vm_tables(&db)?;

        Ok(db)
    }

    /// Adds `column` to `table` via `ddl` only when it is not already there -
    /// `ALTER TABLE ADD COLUMN` throws on a column that already exists, which
    /// a fixture opened a second time (or the TS-made `ts-made.db`) always
    /// has. Checked with `PRAGMA table_info` first, same as every TS
    /// `ensure*Column`.
    ///
    /// B22: `table` is a closed enum, not a caller-supplied `&str`, so this
    /// stays the one string-built `PRAGMA` statement in the crate without
    /// being the one a future caller makes injectable by passing a name read
    /// out of a row.
    fn ensure_column(&self, table: Table, column: &str, ddl: &str) -> rusqlite::Result<()> {
        let exists = {
            let mut stmt = self
                .0
                .prepare(&format!("PRAGMA table_info({})", table.as_str()))?;
            stmt.query_map([], |row| row.get::<_, String>(1))?
                .filter_map(Result::ok)
                .any(|name| name == column)
        };
        if !exists {
            self.ensure(ddl)?;
        }
        Ok(())
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
