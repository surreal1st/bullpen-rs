//! S0-02 acceptance tests: `Db::open` applies migrations 1..16 and opens the
//! TS-made fixture unchanged. S2-F-07 added migration 17
//! (`crates/store/src/migrations.rs`), bullpen-rs-only - the TS fixture
//! predates it, so `Db::open` migrates it forward same as any other db.

use std::collections::BTreeSet;
use std::fs;

use rusqlite::Connection;
use store::Db;

/// `sqlite_master` object names (tables, indexes, triggers), excluding the
/// FTS5 shadow tables whose internal set can vary independent of schema.
fn schema_names(conn: &Connection) -> BTreeSet<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type IN ('table','index','trigger')")
        .expect("prepare sqlite_master query");
    stmt.query_map([], |row| row.get::<_, String>(0))
        .expect("query sqlite_master")
        .filter_map(Result::ok)
        .filter(|name| !name.starts_with("memory_fts_") && !name.starts_with("message_fts_"))
        .collect()
}

fn user_version(conn: &Connection) -> i64 {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read user_version")
}

/// Copies the fixture to a fresh temp path so no test opens it in place.
fn copy_fixture_to_temp() -> std::path::PathBuf {
    let fixture = "d:/rainmade/.scratch/bullpen-rs/fixtures/ts-made.db";
    let temp =
        std::env::temp_dir().join(format!("bullpen-rs-store-test-{}.db", uuid::Uuid::new_v4()));
    fs::copy(fixture, &temp).expect("copy fixture to temp path");
    temp
}

/// Test 1: `Db::open(":memory:")` leaves `user_version` at the full
/// migration count (17, once S2-F-07's migration lands).
#[test]
fn memory_db_migrates_to_latest_version() {
    let db = Db::open(":memory:").expect("open :memory:");
    assert_eq!(user_version(db.conn()), 17);
}

/// Test 2: opening a copy of the TS-made fixture (checked in at
/// user_version 16, migration 17's own predecessor) brings it forward to 17
/// and leaves the `sqlite_master` object set the same names as before -
/// migration 17 rebuilds `approvals` in place (SQLite has no ALTER TABLE for
/// a CHECK constraint) but the table and its index keep the same names.
#[test]
fn fixture_db_opens_unchanged() {
    let temp = copy_fixture_to_temp();

    let before = {
        let raw = Connection::open(&temp).expect("open raw connection before Db::open");
        assert_eq!(
            user_version(&raw),
            16,
            "fixture should already be at version 16"
        );
        schema_names(&raw)
    };

    let db = Db::open(temp.to_str().expect("temp path is valid UTF-8"))
        .expect("Db::open the fixture copy");

    assert_eq!(
        user_version(db.conn()),
        17,
        "user_version must reach 17 after open"
    );

    let after = schema_names(db.conn());
    assert_eq!(
        before, after,
        "sqlite_master object set must be unchanged before vs after Db::open"
    );

    drop(db);
    let _ = fs::remove_file(&temp);
    let _ = fs::remove_file(temp.with_extension("db-wal"));
    let _ = fs::remove_file(temp.with_extension("db-shm"));
}

/// Test 3: fresh `:memory:` table set is a superset of the 16 migrations'
/// tables, and `bots` carries every column added across those migrations.
#[test]
fn fresh_memory_has_expected_tables_and_bot_columns() {
    let db = Db::open(":memory:").expect("open :memory:");

    let expected_tables = [
        "bots",
        "conversations",
        "messages",
        "settings",
        "memory_log",
        "runs",
        "approvals",
        "routines",
        "attachments",
        "connectors",
        "bot_connectors",
        "marketplaces",
        "sections",
        "connector_auth",
        "oauth_flows",
        "auth_settings",
        "sessions",
        "twitch_pings",
        "link_previews",
    ];

    let tables: BTreeSet<String> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .expect("prepare table list");
        stmt.query_map([], |row| row.get::<_, String>(0))
            .expect("query table list")
            .filter_map(Result::ok)
            .collect()
    };

    for table in expected_tables {
        assert!(tables.contains(table), "missing table `{table}`");
    }

    let expected_bot_columns = [
        "has_routine",
        "memory_core",
        "permissions",
        "section_id",
        "pinned_at",
        "hidden_at",
        "avatar",
        "last_seen_at",
        "shape",
    ];

    let bot_columns: BTreeSet<String> = {
        let mut stmt = db
            .conn()
            .prepare("PRAGMA table_info(bots)")
            .expect("prepare bots table_info");
        stmt.query_map([], |row| row.get::<_, String>(1))
            .expect("query bots table_info")
            .filter_map(Result::ok)
            .collect()
    };

    for column in expected_bot_columns {
        assert!(
            bot_columns.contains(column),
            "bots missing column `{column}`"
        );
    }
}
