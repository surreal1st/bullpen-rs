//! Tests for VM registry and container state management.

use std::collections::HashMap;
use store::Db;
use store::vms::*;

#[test]
fn vm_config_from_env() {
    let mut env = HashMap::new();
    env.insert("BULLPEN_VM_IMAGE".to_string(), "custom-image".to_string());
    env.insert("BULLPEN_VM_SLOTS".to_string(), "16".to_string());
    env.insert("BULLPEN_VM_CDP_BASE".to_string(), "9400".to_string());

    let cfg = vm_config(&env);
    assert_eq!(cfg.image, "custom-image");
    assert_eq!(cfg.slots, 16);
    assert_eq!(cfg.cdp_base, 9400);
    assert_eq!(cfg.web_base, 6200); // default
}

#[test]
fn vms_enabled_checks_env() {
    let mut env = HashMap::new();
    assert!(!vms_enabled(&env));

    env.insert("BULLPEN_VM".to_string(), "on".to_string());
    assert!(vms_enabled(&env));

    env.insert("BULLPEN_VM".to_string(), "off".to_string());
    assert!(!vms_enabled(&env));
}

#[test]
fn container_for_sanitizes_bot_id() {
    assert_eq!(container_for("my-bot"), "bullpen-vm-my-bot");
    assert_eq!(container_for("my_bot"), "bullpen-vm-my_bot");
    assert_eq!(container_for("my.bot"), "bullpen-vm-my.bot");
    assert_eq!(container_for("my@bot"), "bullpen-vm-my_bot");
    assert_eq!(container_for("my bot"), "bullpen-vm-my_bot");
    assert_eq!(container_for("my/bot"), "bullpen-vm-my_bot");
    assert_eq!(container_for("my:bot"), "bullpen-vm-my_bot");
}

#[test]
fn config_volume_for_sanitizes_bot_id() {
    assert_eq!(config_volume_for("my-bot"), "bullpen-vmcfg-my-bot");
    assert_eq!(config_volume_for("my_bot"), "bullpen-vmcfg-my_bot");
    assert_eq!(config_volume_for("my.bot"), "bullpen-vmcfg-my.bot");
    assert_eq!(config_volume_for("my@bot"), "bullpen-vmcfg-my_bot");
}

// The three tests below used to assert only `!container.starts_with("-")`
// against ids made of nothing but `-` and ASCII letters (`"--rm"`, `"-rm"`,
// `"-it"`, `"-v"`, `"---"`, `"-"`). `sanitize_for_docker` passes every one of
// those characters through UNCHANGED, so the literal `"bullpen-vm-"` prefix
// in `container_for` (`vms.rs:191`) is the only thing making them pass -
// replacing the sanitiser body with `id.to_string()` left all three green,
// `assert_eq!(container, "bullpen-vm---rm")` included, because a leading `-`
// can never happen regardless of what the sanitiser does.
//
// The sanitiser's real job is different: turning a SPACE (or any other
// non-docker-name character) into `_` so a bot id can never split what a
// caller treats as one docker argument into two - e.g. a volume name of
// `bullpen-vmcfg-x --privileged` handed unquoted to something that splits on
// whitespace becomes the two tokens `bullpen-vmcfg-x` and `--privileged`.
// Guard-present world: `sanitize_for_docker`'s wildcard arm (`vms.rs:207`)
// replaces the space, so no whitespace survives into the container name.
// Guard-removed world: widen that arm to also pass `' '` through unchanged
// and the space survives - `.split_whitespace().count()` goes from 1 to 2.
// That is the observable these three now assert.

#[test]
fn sanitizer_prevents_flag_injection() {
    let malicious_id = "--rm x";
    let container = container_for(malicious_id);
    assert_eq!(container, "bullpen-vm---rm_x");
    assert_eq!(
        container.split_whitespace().count(),
        1,
        "a space in the bot id must not survive into the container name: {container:?}"
    );
}

#[test]
fn sanitizer_prevents_flag_injection_at_start() {
    let malicious_id = "-rm y";
    let container = container_for(malicious_id);
    assert_eq!(container, "bullpen-vm--rm_y");
    assert_eq!(
        container.split_whitespace().count(),
        1,
        "a space in the bot id must not survive into the container name: {container:?}"
    );
}

#[test]
fn crafted_id_cannot_produce_flag_like_container() {
    // This test goes RED if the sanitiser stops replacing spaces: widen
    // `sanitize_for_docker`'s wildcard arm to pass `' '` through and every
    // case below produces a container name that splits into 2+ whitespace
    // tokens instead of 1.
    let test_cases = vec![
        ("--rm x", "bullpen-vm---rm_x"),
        ("--network host", "bullpen-vm---network_host"),
        ("-it -v", "bullpen-vm--it_-v"),
        ("y --privileged", "bullpen-vm-y_--privileged"),
        ("a b c", "bullpen-vm-a_b_c"),
    ];

    for (malicious_id, expected) in test_cases {
        let container = container_for(malicious_id);
        assert_eq!(
            container, expected,
            "container_for({malicious_id:?}) did not sanitize as expected"
        );
        assert_eq!(
            container.split_whitespace().count(),
            1,
            "bot_id '{malicious_id}' produced a container name with an \
             embedded space, '{container}' - a downstream whitespace-split \
             would see this as more than one docker argument"
        );
    }
}

#[test]
fn parse_container_state_running() {
    let result = DockerResult {
        ok: true,
        stdout: "running true".to_string(),
        stderr: "".to_string(),
    };
    let state = parse_container_state(&result);
    assert!(state.exists);
    assert!(state.running);
    assert_eq!(state.status, "running");
}

#[test]
fn parse_container_state_stopped() {
    let result = DockerResult {
        ok: true,
        stdout: "exited false".to_string(),
        stderr: "".to_string(),
    };
    let state = parse_container_state(&result);
    assert!(state.exists);
    assert!(!state.running);
    assert_eq!(state.status, "exited");
}

#[test]
fn parse_container_state_not_found() {
    let result = DockerResult {
        ok: false,
        stdout: "".to_string(),
        stderr: "Error response from daemon: No such object: my-container".to_string(),
    };
    let state = parse_container_state(&result);
    assert!(!state.exists);
    assert!(!state.running);
    assert_eq!(state.status, "absent");
}

#[test]
fn parse_container_state_not_found_container_variant() {
    let result = DockerResult {
        ok: false,
        stdout: "".to_string(),
        stderr: "Error response from daemon: No such container: my-container".to_string(),
    };
    let state = parse_container_state(&result);
    assert!(!state.exists);
    assert!(!state.running);
    assert_eq!(state.status, "absent");
}

#[test]
fn parse_container_state_unknown_format() {
    let result = DockerResult {
        ok: true,
        stdout: "".to_string(),
        stderr: "something went wrong".to_string(),
    };
    let state = parse_container_state(&result);
    assert!(!state.exists);
    assert_eq!(state.status, "unknown");
}

#[test]
fn ensure_vm_tables_creates_table() {
    let db = Db::open(":memory:").expect("open :memory:");
    // Table should already exist from ensure_vm_tables called in Db::open
    let result = db.conn().execute(
        "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at)
         VALUES (?, ?, ?, ?, ?, ?)",
        rusqlite::params![
            "test-bot",
            "bullpen-vm-test-bot",
            9300,
            6200,
            "new",
            "2026-09-15T00:00:00Z"
        ],
    );
    assert!(result.is_ok());
}

#[test]
fn get_vm_returns_existing_vm() {
    let db = Db::open(":memory:").expect("open :memory:");

    db.conn()
        .execute(
            "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at)
         VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                "test-bot",
                "bullpen-vm-test",
                9300,
                6200,
                "running",
                "2026-09-15T00:00:00Z"
            ],
        )
        .expect("insert vm");

    let vm = get_vm(&db, "test-bot").expect("get vm");
    assert!(vm.is_some());
    let vm = vm.unwrap();
    assert_eq!(vm.bot_id, "test-bot");
    assert_eq!(vm.container, "bullpen-vm-test");
    assert_eq!(vm.cdp_port, 9300);
    assert_eq!(vm.web_port, 6200);
    assert_eq!(vm.state, "running");
}

#[test]
fn get_vm_returns_none_for_missing_vm() {
    let db = Db::open(":memory:").expect("open :memory:");
    let vm = get_vm(&db, "nonexistent").expect("get vm");
    assert!(vm.is_none());
}

#[test]
fn list_vms_returns_all_vms_ordered_by_cdp_port() {
    let db = Db::open(":memory:").expect("open :memory:");

    db.conn()
        .execute(
            "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at)
         VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                "bot2",
                "bullpen-vm-bot2",
                9301,
                6201,
                "new",
                "2026-09-15T00:00:00Z"
            ],
        )
        .expect("insert bot2");

    db.conn()
        .execute(
            "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at)
         VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                "bot1",
                "bullpen-vm-bot1",
                9300,
                6200,
                "running",
                "2026-09-15T00:00:00Z"
            ],
        )
        .expect("insert bot1");

    db.conn()
        .execute(
            "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at)
         VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                "bot3",
                "bullpen-vm-bot3",
                9302,
                6202,
                "stopped",
                "2026-09-15T00:00:00Z"
            ],
        )
        .expect("insert bot3");

    let vms = list_vms(&db).expect("list vms");
    assert_eq!(vms.len(), 3);
    assert_eq!(vms[0].bot_id, "bot1");
    assert_eq!(vms[0].cdp_port, 9300);
    assert_eq!(vms[1].bot_id, "bot2");
    assert_eq!(vms[1].cdp_port, 9301);
    assert_eq!(vms[2].bot_id, "bot3");
    assert_eq!(vms[2].cdp_port, 9302);
}

#[test]
fn next_slot_finds_first_free_slot() {
    let db = Db::open(":memory:").expect("open :memory:");
    let cfg = VmConfig {
        image: "test".to_string(),
        docker_host: "unix:///run/user/1004/docker.sock".to_string(),
        cdp_base: 9300,
        web_base: 6200,
        slots: 24,
        idle_ms: 1800000,
        init_dir: "/home/bullpen/vm-init".to_string(),
        memory: "3g".to_string(),
        cpus: "1.5".to_string(),
        shm_size: "1g".to_string(),
        timezone: "America/New_York".to_string(),
        puid: "1004".to_string(),
        pgid: "1004".to_string(),
    };

    db.conn()
        .execute(
            "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at)
         VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                "bot1",
                "bullpen-vm-bot1",
                9300,
                6200,
                "running",
                "2026-09-15T00:00:00Z"
            ],
        )
        .expect("insert bot1");

    // First free slot should be 1
    let slot = next_slot(&db, &cfg).expect("next slot");
    assert_eq!(slot, Some(1));
}

#[test]
fn next_slot_returns_none_when_all_slots_taken() {
    let db = Db::open(":memory:").expect("open :memory:");
    let cfg = VmConfig {
        image: "test".to_string(),
        docker_host: "unix:///run/user/1004/docker.sock".to_string(),
        cdp_base: 9300,
        web_base: 6200,
        slots: 2,
        idle_ms: 1800000,
        init_dir: "/home/bullpen/vm-init".to_string(),
        memory: "3g".to_string(),
        cpus: "1.5".to_string(),
        shm_size: "1g".to_string(),
        timezone: "America/New_York".to_string(),
        puid: "1004".to_string(),
        pgid: "1004".to_string(),
    };

    // Fill all 2 slots
    db.conn()
        .execute(
            "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at)
         VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                "bot1",
                "bullpen-vm-bot1",
                9300,
                6200,
                "running",
                "2026-09-15T00:00:00Z"
            ],
        )
        .expect("insert bot1");

    db.conn()
        .execute(
            "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at)
         VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                "bot2",
                "bullpen-vm-bot2",
                9301,
                6201,
                "running",
                "2026-09-15T00:00:00Z"
            ],
        )
        .expect("insert bot2");

    // Both slots are taken, should return None
    let slot = next_slot(&db, &cfg).expect("next slot");
    assert!(
        slot.is_none(),
        "Should return None when all slots are exhausted"
    );
}

#[test]
fn next_slot_reuses_deleted_slot() {
    let db = Db::open(":memory:").expect("open :memory:");
    let cfg = VmConfig {
        image: "test".to_string(),
        docker_host: "unix:///run/user/1004/docker.sock".to_string(),
        cdp_base: 9300,
        web_base: 6200,
        slots: 10,
        idle_ms: 1800000,
        init_dir: "/home/bullpen/vm-init".to_string(),
        memory: "3g".to_string(),
        cpus: "1.5".to_string(),
        shm_size: "1g".to_string(),
        timezone: "America/New_York".to_string(),
        puid: "1004".to_string(),
        pgid: "1004".to_string(),
    };

    // Insert at slot 2 and 4
    db.conn()
        .execute(
            "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at)
         VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                "bot1",
                "bullpen-vm-bot1",
                9302,
                6202,
                "running",
                "2026-09-15T00:00:00Z"
            ],
        )
        .expect("insert bot1");

    db.conn()
        .execute(
            "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at)
         VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                "bot2",
                "bullpen-vm-bot2",
                9304,
                6204,
                "running",
                "2026-09-15T00:00:00Z"
            ],
        )
        .expect("insert bot2");

    // Next free should be slot 0
    let slot = next_slot(&db, &cfg).expect("next slot");
    assert_eq!(slot, Some(0));

    // Delete slot 2
    db.conn()
        .execute(
            "DELETE FROM vms WHERE bot_id = ?",
            rusqlite::params!["bot1"],
        )
        .expect("delete");

    // Now next free should still be 0 (lowest)
    let slot = next_slot(&db, &cfg).expect("next slot");
    assert_eq!(slot, Some(0));
}

#[test]
fn next_slot_must_return_none_on_exhaustion() {
    // This test goes RED if next_slot returns a slot when all are taken.
    // It verifies the exhaustion behavior is correct.
    let db = Db::open(":memory:").expect("open :memory:");
    let cfg = VmConfig {
        image: "test".to_string(),
        docker_host: "unix:///run/user/1004/docker.sock".to_string(),
        cdp_base: 9300,
        web_base: 6200,
        slots: 1,
        idle_ms: 1800000,
        init_dir: "/home/bullpen/vm-init".to_string(),
        memory: "3g".to_string(),
        cpus: "1.5".to_string(),
        shm_size: "1g".to_string(),
        timezone: "America/New_York".to_string(),
        puid: "1004".to_string(),
        pgid: "1004".to_string(),
    };

    db.conn()
        .execute(
            "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at)
         VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                "bot1",
                "bullpen-vm-bot1",
                9300,
                6200,
                "running",
                "2026-09-15T00:00:00Z"
            ],
        )
        .expect("insert bot1");

    // With 1 slot total and it filled, next_slot must return None
    let slot = next_slot(&db, &cfg).expect("next slot");
    assert!(
        slot.is_none(),
        "next_slot must return None when all slots are exhausted, not return a slot"
    );
}

/// Pins the `vms` table's column types, NOT NULL flags and primary key -
/// this must stay byte-compatible with the live TypeScript Bullpen's
/// database (`vms.rs:1-2`).
///
/// Guard-present world: `ensure_vm_tables`'s DDL (`vms.rs:140-148`)
/// declares `cdp_port`/`web_port` `INTEGER NOT NULL` and
/// `container`/`state`/`last_used_at` `TEXT NOT NULL`, with `bot_id` the
/// primary key. Guard-removed world: `ensure_vm_tables_creates_table`
/// (above) only inserts one well-formed row, so changing `cdp_port INTEGER`
/// to `cdp_port TEXT` - SQLite is dynamically typed and accepts the exact
/// same INSERT either way - or dropping any `NOT NULL` leaves it green.
/// The observable that differs is `PRAGMA table_info(vms)`: this test reads
/// each column's declared type and NOT NULL flag directly from sqlite's own
/// catalog instead of inferring them from what one INSERT happens to accept.
#[test]
fn ensure_vm_tables_pins_the_schema() {
    let db = Db::open(":memory:").expect("open :memory:");
    let mut stmt = db
        .conn()
        .prepare("PRAGMA table_info(vms)")
        .expect("prepare pragma");
    let cols: Vec<(String, String, bool, bool)> = stmt
        .query_map([], |row| {
            let name: String = row.get(1)?;
            let ty: String = row.get(2)?;
            let notnull: i64 = row.get(3)?;
            let pk: i64 = row.get(5)?;
            Ok((name, ty, notnull != 0, pk != 0))
        })
        .expect("query pragma")
        .filter_map(|r| r.ok())
        .collect();

    assert_eq!(
        cols,
        vec![
            // bot_id TEXT PRIMARY KEY - SQLite does not set the NOT NULL
            // flag on a non-INTEGER primary key unless declared explicitly
            // (a documented SQLite quirk), so notnull is false here even
            // though it is the key.
            ("bot_id".to_string(), "TEXT".to_string(), false, true),
            ("container".to_string(), "TEXT".to_string(), true, false),
            ("cdp_port".to_string(), "INTEGER".to_string(), true, false),
            ("web_port".to_string(), "INTEGER".to_string(), true, false),
            ("state".to_string(), "TEXT".to_string(), true, false),
            ("last_used_at".to_string(), "TEXT".to_string(), true, false),
        ],
        "vms table schema drifted from what must stay byte-compatible with \
         the live TS Bullpen database"
    );
}
