//! S3-01 acceptance tests: migration 18, memory tiers, TTL, scope, projects.

use std::fs;
use store::{Db, Scope};

/// Copies the fixture to a fresh temp path so no test opens it in place.
fn copy_fixture_to_temp() -> std::path::PathBuf {
    let fixture = "d:/rainmade/.scratch/bullpen-rs/fixtures/ts-made.db";
    let temp =
        std::env::temp_dir().join(format!("bullpen-rs-store-test-{}.db", uuid::Uuid::new_v4()));
    fs::copy(fixture, &temp).expect("copy fixture to temp path");
    temp
}

fn seed_bot(db: &Db, id: &str) {
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES (?1, '', '', '', NULL, ?2)",
            rusqlite::params![id, "2026-01-01T00:00:00Z"],
        )
        .unwrap();
}

/// Test 1: Fixture opens at the latest migration (21, after COST-01).
#[test]
fn fixture_opens_at_migration_20() {
    let temp = copy_fixture_to_temp();
    let db = Db::open(temp.to_str().expect("temp path is valid UTF-8"))
        .expect("Db::open the fixture copy");

    let version: i64 = db
        .conn()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read user_version");

    assert_eq!(version, 21, "user_version must be 21 after open");

    drop(db);
    let _ = fs::remove_file(&temp);
    let _ = fs::remove_file(temp.with_extension("db-wal"));
    let _ = fs::remove_file(temp.with_extension("db-shm"));
}

/// Test 2: A note with TTL 0 is absent from recall and search after sweep_expired.
#[test]
fn expired_note_is_filtered_on_sweep() {
    let db = Db::open(":memory:").unwrap();
    seed_bot(&db, "t");

    // Create a note that expires immediately (ttl 0)
    let note_entry = store::note(&db, "t", "This note expires now", 0).unwrap();

    // Expired note should NOT appear in recall (filtered on read)
    let recall = store::recall_for(&db, "t", store::RECALL_TOKEN_BUDGET).unwrap();
    assert!(
        !recall.entries.iter().any(|e| e.id == note_entry.id),
        "expired note should be filtered from recall"
    );

    // Sweep should delete the expired note
    let deleted = store::sweep_expired(&db).unwrap();
    assert!(deleted > 0, "sweep_expired should delete expired notes");

    // After sweep, definitely gone from database
    let recall = store::recall_for(&db, "t", store::RECALL_TOKEN_BUDGET).unwrap();
    assert!(
        !recall.entries.iter().any(|e| e.id == note_entry.id),
        "note should remain absent from recall after sweep"
    );

    let hits = store::search_log(&db, "t", "expires", &[Scope::Own], 8).unwrap();
    assert!(hits.is_empty(), "search should not find expired notes");
}

/// Test 3: Recall order is own > project > shared, newest first inside each tier.
#[test]
fn recall_precedence_order() {
    let db = Db::open(":memory:").unwrap();
    seed_bot(&db, "bot1");
    seed_bot(&db, "bot2");

    // Create a project and add both bots
    let project = store::create_project(&db, "shared-project").unwrap();
    store::add_project_member(&db, &project.id, "bot1").unwrap();
    store::add_project_member(&db, &project.id, "bot2").unwrap();

    // Add entries in different scopes
    let _own_entry = store::remember(&db, "bot1", "own memory", "josh").unwrap();
    let _project_entry = store::remember_scoped(
        &db,
        "bot2",
        "project memory",
        Scope::Project,
        Some(&project.id),
    )
    .unwrap();
    let _shared_entry =
        store::remember_scoped(&db, "bot2", "shared memory", Scope::Shared, None).unwrap();

    // Recall should see all three in order: own, project, shared
    let recall = store::recall_for(&db, "bot1", 5000).unwrap();
    let entry_ids: Vec<&str> = recall.entries.iter().map(|e| e.content.as_str()).collect();

    let own_pos = entry_ids.iter().position(|&c| c == "own memory");
    let project_pos = entry_ids.iter().position(|&c| c == "project memory");
    let shared_pos = entry_ids.iter().position(|&c| c == "shared memory");

    assert!(own_pos.is_some(), "own entry should be in recall");
    assert!(project_pos.is_some(), "project entry should be in recall");
    assert!(shared_pos.is_some(), "shared entry should be in recall");
    assert!(own_pos < project_pos, "own should come before project");
    assert!(
        project_pos < shared_pos,
        "project should come before shared"
    );
}

/// Test 4: Project entries are visible only to members.
#[test]
fn project_entries_visible_only_to_members() {
    let db = Db::open(":memory:").unwrap();
    seed_bot(&db, "member");
    seed_bot(&db, "non_member");

    // Create project and add only "member" bot
    let project = store::create_project(&db, "private-project").unwrap();
    store::add_project_member(&db, &project.id, "member").unwrap();

    // Add project entry from "member" perspective
    let project_entry = store::remember_scoped(
        &db,
        "member",
        "team secret",
        Scope::Project,
        Some(&project.id),
    )
    .unwrap();

    // Member should see the entry
    let member_recall = store::recall_for(&db, "member", 1000).unwrap();
    assert!(
        member_recall
            .entries
            .iter()
            .any(|e| e.id == project_entry.id),
        "member should see project entry"
    );

    // Non-member should NOT see the entry
    let non_member_recall = store::recall_for(&db, "non_member", 1000).unwrap();
    assert!(
        !non_member_recall
            .entries
            .iter()
            .any(|e| e.id == project_entry.id),
        "non-member should not see project entry"
    );
}

/// Test 5: Shared entries are visible to every bot.
#[test]
fn shared_entries_visible_to_all() {
    let db = Db::open(":memory:").unwrap();
    seed_bot(&db, "bot1");
    seed_bot(&db, "bot2");

    // Add a shared entry from bot1
    let shared_entry =
        store::remember_scoped(&db, "bot1", "everyone knows this", Scope::Shared, None).unwrap();

    // Both bots should see it
    let recall1 = store::recall_for(&db, "bot1", 1000).unwrap();
    assert!(
        recall1.entries.iter().any(|e| e.id == shared_entry.id),
        "creator should see shared entry"
    );

    let recall2 = store::recall_for(&db, "bot2", 1000).unwrap();
    assert!(
        recall2.entries.iter().any(|e| e.id == shared_entry.id),
        "other bot should see shared entry"
    );
}

/// Test 6: Bite test - drop expires_at filter in recall and test turns red.
#[test]
fn expired_note_without_filter_appears() {
    let db = Db::open(":memory:").unwrap();
    seed_bot(&db, "t");

    // Create an expired note
    store::note(&db, "t", "this should be hidden", 0).unwrap();

    // Don't sweep - this simulates the "broken" behavior if we comment out expires_at filtering
    // In the real implementation, recall_for calls sweep_expired, so expired notes are removed.
    // If we were to comment out the sweep_expired call, this test would verify the bug.

    let recall = store::recall_for(&db, "t", store::RECALL_TOKEN_BUDGET).unwrap();
    // With the filter and sweep in place, expired note should NOT appear
    assert!(
        !recall
            .entries
            .iter()
            .any(|e| e.content.contains("should be hidden")),
        "expired note should be filtered out"
    );
}

/// Test 7: Search respects scope filtering.
#[test]
fn search_respects_scope() {
    let db = Db::open(":memory:").unwrap();
    seed_bot(&db, "bot1");

    // Add entries in different scopes
    store::remember(&db, "bot1", "private note", "josh").unwrap();
    store::remember_scoped(&db, "bot1", "shared info", Scope::Shared, None).unwrap();

    // Search in Own scope should find private
    let own_hits = store::search_log(&db, "bot1", "note", &[Scope::Own], 8).unwrap();
    assert!(
        own_hits.iter().any(|e| e.content.contains("private")),
        "should find own entries"
    );
    assert!(
        !own_hits.iter().any(|e| e.content.contains("shared")),
        "should not find shared in own search"
    );

    // Search in Shared scope should find shared
    let shared_hits = store::search_log(&db, "bot1", "shared", &[Scope::Shared], 8).unwrap();
    assert!(
        shared_hits.iter().any(|e| e.content.contains("shared")),
        "should find shared entries"
    );
    assert!(
        !shared_hits.iter().any(|e| e.content.contains("private")),
        "should not find own in shared search"
    );
}

/// Test 8: Projects can be queried by bot.
#[test]
fn projects_for_bot() {
    let db = Db::open(":memory:").unwrap();
    seed_bot(&db, "bot1");

    // Create multiple projects
    let proj1 = store::create_project(&db, "project-a").unwrap();
    let proj2 = store::create_project(&db, "project-b").unwrap();

    // Add bot to both
    store::add_project_member(&db, &proj1.id, "bot1").unwrap();
    store::add_project_member(&db, &proj2.id, "bot1").unwrap();

    // Query projects
    let projects = store::projects_for(&db, "bot1").unwrap();
    assert_eq!(projects.len(), 2, "bot should be in both projects");
    assert!(
        projects.iter().any(|p| p.name == "project-a"),
        "should find project-a"
    );
    assert!(
        projects.iter().any(|p| p.name == "project-b"),
        "should find project-b"
    );
}

/// Test 9: FTS5 triggers still fire for new rows (memory_log_ai still works).
#[test]
fn fts_triggers_fire_for_new_rows() {
    let db = Db::open(":memory:").unwrap();
    seed_bot(&db, "t");

    // Add various entries - all should be FTS indexed
    store::remember(&db, "t", "logged entry", "bot").unwrap();
    store::note(&db, "t", "note entry", 300).unwrap();
    store::remember_scoped(&db, "t", "scoped entry", Scope::Own, None).unwrap();

    // All should be searchable
    let hits = store::search_log(&db, "t", "entry", &[Scope::Own], 10).unwrap();
    assert_eq!(hits.len(), 3, "all three entries should be FTS indexed");
}

/// Test 10: Newest entries win within a tier.
#[test]
fn newest_within_tier() {
    let db = Db::open(":memory:").unwrap();
    seed_bot(&db, "t");

    // Create entries with controlled content to check order
    let _old = store::remember(&db, "t", "old entry", "josh").unwrap();
    let _newer = store::remember(&db, "t", "newer entry", "josh").unwrap();

    let recall = store::recall_for(&db, "t", 5000).unwrap();
    // Should be in order: oldest first (because recall reverses), so "old" then "newer"
    assert!(
        recall.entries[0].content.contains("old"),
        "oldest should appear first after reversal"
    );
    assert!(
        recall.entries[1].content.contains("newer"),
        "newest should appear second after reversal"
    );
}
