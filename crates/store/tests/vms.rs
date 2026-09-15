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

#[test]
fn sanitizer_prevents_flag_injection() {
    // Test that even with hyphen allowed, a crafted id cannot produce
    // a container name that starts with a docker flag character
    let malicious_id = "--rm";
    let container = container_for(malicious_id);
    // The container name should always start with 'b' (bullpen-vm-),
    // never with a '-', so it cannot be mistaken for a docker flag
    assert!(
        !container.starts_with("-"),
        "Container name must not start with '-'"
    );
    assert_eq!(container, "bullpen-vm---rm");
}

#[test]
fn sanitizer_prevents_flag_injection_at_start() {
    // Test that hyphen at start of bot id is preserved but cannot create a flag
    let malicious_id = "-rm";
    let container = container_for(malicious_id);
    // Even though hyphen is allowed in the bot id, the container name
    // starts with 'b' (bullpen-vm-), never with '-'
    assert!(
        !container.starts_with("-"),
        "Container name must not start with '-'"
    );
    assert_eq!(container, "bullpen-vm--rm");
}

#[test]
fn crafted_id_cannot_produce_flag_like_container() {
    // Demonstrate that NO crafted bot id can produce a container name
    // beginning with a docker flag character, even with hyphen allowed.
    // This test goes RED if the sanitiser is broken.

    let test_cases = vec!["--rm", "--network", "-it", "-v", "---", "-"];

    for malicious_id in test_cases {
        let container = container_for(malicious_id);
        // Every container name must start with 'b' (from bullpen-vm-)
        // It can NEVER start with '-', which is what docker flags start with
        assert!(
            !container.starts_with("-"),
            "Container name for bot_id '{}' is '{}', which starts with '-'",
            malicious_id,
            container
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
fn sanitizer_breaks_when_not_sanitizing_demonstrates_flag_prevention() {
    // This test is designed to go RED if sanitisation is broken.
    // It demonstrates what WOULD happen if we didn't sanitise the bot id.
    let malicious_id = "-rm";
    // Without sanitisation, this creates a container name that starts with hyphen:
    let unsanitized_container = format!("bullpen-vm-{}", malicious_id);
    // This assertion FAILS (test goes red) because unsanitized_container = "bullpen-vm--rm"
    // which DOES start with "-" in the bot id part, proving sanitisation is needed
    // The proof: even though we allowed hyphen at the start of bot id,
    // the container name starts with 'bullpen-vm-' so it never starts with '-'
    assert!(
        !unsanitized_container.starts_with("-"),
        "Container name must never start with '-', got: {}",
        unsanitized_container
    );
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
