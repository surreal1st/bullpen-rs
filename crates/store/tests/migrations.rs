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
/// migration count (21, after COST-01's `messages.cost_unknown` migration).
#[test]
fn memory_db_migrates_to_latest_version() {
    let db = Db::open(":memory:").expect("open :memory:");
    assert_eq!(user_version(db.conn()), 21);
}

#[test]
fn run_image_counters_are_self_created_without_bumping_schema_version() {
    let db = Db::open(":memory:").expect("open :memory:");
    let columns: BTreeSet<String> = db
        .conn()
        .prepare("PRAGMA table_info(runs)")
        .expect("prepare runs table_info")
        .query_map([], |row| row.get(1))
        .expect("query runs table_info")
        .collect::<Result<_, _>>()
        .expect("collect runs columns");

    assert!(columns.contains("screen_capture_attempts"));
    assert!(columns.contains("screen_image_dispatches"));
    assert_eq!(user_version(db.conn()), 21);
}

/// Test 2: opening a copy of the TS-made fixture (checked in at
/// user_version 16) brings it forward to 21 and leaves the `sqlite_master`
/// object set the same names as before - migrations 17-21 add tables/columns/indexes
/// but keep existing table and index names. Migration 21 (COST-01) only adds
/// a column to the existing `messages` table, so it contributes no new
/// schema object name here.
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
        21,
        "user_version must reach 21 after open"
    );

    let after = schema_names(db.conn());

    // Migrations 18, 19 and 20 add new tables and indexes. Verify they are present and
    // that no other schema objects were created or removed.
    let mut expected_new: BTreeSet<String> = BTreeSet::new();
    expected_new.insert("projects".to_string());
    expected_new.insert("project_members".to_string());
    expected_new.insert("sqlite_autoindex_projects_1".to_string());
    expected_new.insert("sqlite_autoindex_project_members_1".to_string());
    expected_new.insert("idx_projects_name".to_string());
    // Migration 20 (S4-01): the auto-review judge log (TEXT PRIMARY KEY, so
    // one sqlite_autoindex too).
    expected_new.insert("auto_review_log".to_string());
    expected_new.insert("sqlite_autoindex_auto_review_log_1".to_string());
    // S5b-03: the goals tables are NOT a numbered migration - they are
    // self-creating, exactly as the TS `ensureGoalTables` (`goals.ts:34-65`)
    // creates them, so a db either side wrote still opens on the other. The
    // DDL here is byte-equal to that function's, indexes included; `runs`
    // also gains a `goal_id` COLUMN (`runs.ts:166-167`), which adds no schema
    // name and so does not appear in this set.
    expected_new.insert("goals".to_string());
    expected_new.insert("sqlite_autoindex_goals_1".to_string());
    expected_new.insert("idx_goals_due".to_string());
    expected_new.insert("idx_goals_bot".to_string());

    // After must equal before plus the new objects.
    let expected_after = {
        let mut result = before.clone();
        result.extend(expected_new.clone());
        result
    };

    for new_obj in &expected_new {
        assert!(
            after.contains(new_obj),
            "migrations 18-20 should add {new_obj}"
        );
    }
    assert_eq!(
        after, expected_after,
        "schema must be the fixture's schema plus migrations 18-20's new objects only"
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
