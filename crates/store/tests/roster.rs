use std::fs;
use std::path::PathBuf;
use store::Db;

#[test]
fn test_list_roster() {
    // Copy fixture to temp location
    let fixture = PathBuf::from("d:/rainmade/.scratch/bullpen-rs/fixtures/ts-made.db");
    let temp_dir = std::env::temp_dir();
    let temp_db = temp_dir.join("test_roster.db");

    // Clean up any previous test database
    let _ = fs::remove_file(&temp_db);

    // Copy fixture
    fs::copy(&fixture, &temp_db).expect("Failed to copy fixture");

    // Open database
    let db = Db::open(temp_db.to_str().unwrap()).expect("Failed to open database");

    // Test list_roster returns exactly 3 entries
    let roster = store::list_roster(&db).expect("Failed to list roster");
    assert_eq!(roster.len(), 3, "Expected 3 roster entries");

    // Check names and ids
    let names: Vec<_> = roster.iter().map(|e| e.name.clone()).collect();
    let ids: Vec<_> = roster.iter().map(|e| e.id.clone()).collect();

    assert!(
        names.contains(&"Arthur".to_string()),
        "Expected Arthur in roster"
    );
    assert!(
        names.contains(&"Riley".to_string()),
        "Expected Riley in roster"
    );
    assert!(
        names.contains(&"Jason".to_string()),
        "Expected Jason in roster"
    );

    assert!(ids.contains(&"arthur".to_string()), "Expected arthur id");
    assert!(ids.contains(&"riley".to_string()), "Expected riley id");
    assert!(ids.contains(&"jason".to_string()), "Expected jason id");

    // Check busy status: Jason busy (has waiting run), Riley not busy
    let jason = roster
        .iter()
        .find(|e| e.id == "jason")
        .expect("Jason not found");
    let riley = roster
        .iter()
        .find(|e| e.id == "riley")
        .expect("Riley not found");

    assert!(jason.busy, "Jason should be busy");
    assert!(!riley.busy, "Riley should not be busy");

    // Clean up
    let _ = fs::remove_file(&temp_db);
}

#[test]
fn test_get_bot() {
    // Copy fixture to temp location
    let fixture = PathBuf::from("d:/rainmade/.scratch/bullpen-rs/fixtures/ts-made.db");
    let temp_dir = std::env::temp_dir();
    let temp_db = temp_dir.join("test_get_bot.db");

    // Clean up any previous test database
    let _ = fs::remove_file(&temp_db);

    // Copy fixture
    fs::copy(&fixture, &temp_db).expect("Failed to copy fixture");

    // Open database
    let db = Db::open(temp_db.to_str().unwrap()).expect("Failed to open database");

    // Test get_bot for Arthur
    let arthur = store::get_bot(&db, "arthur")
        .expect("Failed to get bot")
        .expect("Arthur not found");
    assert_eq!(arthur.id, "arthur");
    assert_eq!(arthur.name, "Arthur");
    assert_eq!(arthur.purpose, "Chief of Staff");

    // Test get_bot for non-existent bot
    let nobody = store::get_bot(&db, "nobody").expect("Failed to get bot");
    assert!(nobody.is_none(), "Expected None for non-existent bot");

    // Clean up
    let _ = fs::remove_file(&temp_db);
}

#[test]
fn test_roster_entry_serialization() {
    // Copy fixture to temp location
    let fixture = PathBuf::from("d:/rainmade/.scratch/bullpen-rs/fixtures/ts-made.db");
    let temp_dir = std::env::temp_dir();
    let temp_db = temp_dir.join("test_serialization.db");

    // Clean up any previous test database
    let _ = fs::remove_file(&temp_db);

    // Copy fixture
    fs::copy(&fixture, &temp_db).expect("Failed to copy fixture");

    // Open database
    let db = Db::open(temp_db.to_str().unwrap()).expect("Failed to open database");

    // Get a roster entry
    let roster = store::list_roster(&db).expect("Failed to list roster");
    let arthur = roster
        .iter()
        .find(|e| e.id == "arthur")
        .expect("Arthur not found");

    // Serialize to JSON
    let json = serde_json::to_string(arthur).expect("Failed to serialize");

    // Check for camelCase keys
    assert!(
        json.contains("\"sectionId\""),
        "Expected camelCase sectionId"
    );
    assert!(json.contains("\"lastAt\""), "Expected camelCase lastAt");
    assert!(
        json.contains("\"hasRoutine\""),
        "Expected camelCase hasRoutine"
    );
    assert!(
        !json.contains("\"section_id\""),
        "Should not have snake_case section_id"
    );
    assert!(
        !json.contains("\"last_at\""),
        "Should not have snake_case last_at"
    );
    assert!(
        !json.contains("\"has_routine\""),
        "Should not have snake_case has_routine"
    );

    // Clean up
    let _ = fs::remove_file(&temp_db);
}

#[test]
fn test_first_line_strips_paired_emphasis_and_preserves_bare_underscores() {
    // F15: first_line should strip only paired */* emphasis and preserve bare
    // underscores that are not part of emphasis markers.
    use store::roster::first_line;

    // Test: **bold** should be stripped
    assert_eq!(first_line("**bold** text"), "bold text");

    // Test: *italic* should be stripped
    assert_eq!(first_line("*italic* text"), "italic text");

    // Test: bare underscore in repo_read should be preserved
    assert_eq!(first_line("Ran repo_read on the checkout"), "Ran repo_read on the checkout");

    // Test: # with space should be stripped but #1 without space should be kept
    assert_eq!(first_line("# Heading text"), "Heading text");
    assert_eq!(first_line("#1 on the list"), "#1 on the list");

    // Test: complex case with mixed emphasis and bare underscores
    assert_eq!(first_line("Fixed **task_name** in *module_code*"), "Fixed task_name in module_code");

    // Test: multiple paired emphasis
    assert_eq!(first_line("This is **bold** and *italic* text"), "This is bold and italic text");
}
