//! S2-02: permissions routing. Port of
//! `projects/bullpen-night/test/permissions-by-trigger.test.ts` plus
//! HTTP route round-trip tests.

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use serde_json::{Value, json};
use server::AppState;
use store::Db;
use tower::ServiceExt;

mod common;
use common::seed_session;

fn open_db() -> Db {
    Db::open(":memory:").expect("open :memory: db")
}

fn seed_bot(db: &Db, id: &str, name: &str) {
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at, permissions) VALUES (?1, ?2, '', ?3, NULL, '2026-01-01T00:00:00Z', ?4)",
            rusqlite::params![id, name, format!("You are {name}."), "{}"],
        )
        .expect("seed bot");
}

fn app_for(db: Db) -> Router {
    let state = AppState::new(db);
    server::build_app(state)
}

// Helpers for making requests
async fn get_permissions_route(app: &Router, bot_id: &str, session: &str) -> (u16, Value) {
    let request = Request::get(format!("/api/bots/{}/permissions", bot_id))
        .header("cookie", session)
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    (status, value)
}

async fn put_permissions_route(
    app: &Router,
    bot_id: &str,
    session: &str,
    permissions: Value,
) -> (u16, Value) {
    let body_json = json!({ "permissions": permissions });
    let body_bytes = serde_json::to_vec(&body_json).unwrap();

    let request = Request::put(format!("/api/bots/{}/permissions", bot_id))
        .header("cookie", session)
        .header("content-type", "application/json")
        .body(Body::from(body_bytes))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    (status, value)
}

#[tokio::test]
async fn defaults_apply_when_no_stored_override() {
    // Create a bot with no permission overrides
    let db = {
        let db = open_db();
        store::set_password(&db, "test-password").expect("set password");
        seed_bot(&db, "test-bot", "Test Bot");
        db
    };

    let session = seed_session(&db);
    let app = app_for(db);

    let (_status, body) = get_permissions_route(&app, "test-bot", &session).await;
    let perms = body.get("permissions").unwrap().as_object().unwrap();

    // Defaults: click should be "allow"
    assert_eq!(perms.get("click").and_then(|v| v.as_str()), Some("allow"));
    // Defaults: shell should be "ask"
    assert_eq!(perms.get("shell").and_then(|v| v.as_str()), Some("ask"));
    // Defaults: remember should be "allow"
    assert_eq!(
        perms.get("remember").and_then(|v| v.as_str()),
        Some("allow")
    );
}

#[tokio::test]
async fn put_then_get_round_trips() {
    let db = {
        let db = open_db();
        store::set_password(&db, "test-password").expect("set password");
        seed_bot(&db, "test-bot", "Test Bot");
        db
    };

    let session = seed_session(&db);
    let app = app_for(db);

    // Put some overrides
    let overrides = json!({
        "click": "ask",
        "shell": "allow",
        "remember": "deny",
    });

    let (_status, _body) =
        put_permissions_route(&app, "test-bot", &session, overrides.clone()).await;

    // Get should return the overrides
    let (_status, body) = get_permissions_route(&app, "test-bot", &session).await;
    let perms = body.get("permissions").unwrap().as_object().unwrap();

    assert_eq!(perms.get("click").and_then(|v| v.as_str()), Some("ask"));
    assert_eq!(perms.get("shell").and_then(|v| v.as_str()), Some("allow"));
    assert_eq!(perms.get("remember").and_then(|v| v.as_str()), Some("deny"));
}

#[test]
fn stored_override_beats_default() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");

    // Override click to "ask" (default is "allow")
    let perms = server::permissions::Permissions::from_iter(vec![(
        "click".to_string(),
        server::permissions::Decision::Ask,
    )]);
    server::permissions::set_permissions(&db, "test-bot", &perms).unwrap();

    // Direct check via the function
    let perms = server::permissions::get_permissions(&db, "test-bot").unwrap();
    assert_eq!(
        perms.get("click").copied(),
        Some(server::permissions::Decision::Ask)
    );
    assert_eq!(
        perms.get("shell").copied(),
        Some(server::permissions::Decision::Ask)
    ); // default
}

#[tokio::test]
async fn routine_tightens_click_to_ask_when_stored_allow() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");

    // Override click to "allow" (it defaults to "allow" anyway)
    let perms = server::permissions::Permissions::from_iter(vec![
        ("click".to_string(), server::permissions::Decision::Allow),
        (
            "fetch_url".to_string(),
            server::permissions::Decision::Allow,
        ),
    ]);
    server::permissions::set_permissions(&db, "test-bot", &perms).unwrap();

    // Chat trigger should keep click as "allow"
    let chat_perms =
        server::permissions::permissions_for_run(&db, "test-bot", model::ladder::Trigger::Chat)
            .unwrap();
    assert_eq!(
        chat_perms.get("click").copied(),
        Some(server::permissions::Decision::Allow)
    );

    // Routine trigger should tighten click to "ask"
    let routine_perms =
        server::permissions::permissions_for_run(&db, "test-bot", model::ladder::Trigger::Routine)
            .unwrap();
    assert_eq!(
        routine_perms.get("click").copied(),
        Some(server::permissions::Decision::Ask)
    );

    // But harmless tools should remain "allow"
    assert_eq!(
        routine_perms.get("fetch_url").copied(),
        Some(server::permissions::Decision::Allow)
    );
}

#[tokio::test]
async fn goal_trigger_tightens_like_routine() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");

    // Set click to allow
    let perms = server::permissions::Permissions::from_iter(vec![(
        "click".to_string(),
        server::permissions::Decision::Allow,
    )]);
    server::permissions::set_permissions(&db, "test-bot", &perms).unwrap();

    // Goal trigger should tighten, same as routine
    let goal_perms =
        server::permissions::permissions_for_run(&db, "test-bot", model::ladder::Trigger::Goal)
            .unwrap();
    assert_eq!(
        goal_perms.get("click").copied(),
        Some(server::permissions::Decision::Ask)
    );
}

#[tokio::test]
async fn webhook_trigger_tightens() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");

    let perms = server::permissions::Permissions::from_iter(vec![(
        "click".to_string(),
        server::permissions::Decision::Allow,
    )]);
    server::permissions::set_permissions(&db, "test-bot", &perms).unwrap();

    // Webhook trigger should tighten
    let webhook_perms =
        server::permissions::permissions_for_run(&db, "test-bot", model::ladder::Trigger::Webhook)
            .unwrap();
    assert_eq!(
        webhook_perms.get("click").copied(),
        Some(server::permissions::Decision::Ask)
    );
}

#[tokio::test]
async fn permissions_never_grant_only_subtract() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");

    // Set shell to "ask" (never "allow")
    let perms = server::permissions::Permissions::from_iter(vec![(
        "shell".to_string(),
        server::permissions::Decision::Ask,
    )]);
    server::permissions::set_permissions(&db, "test-bot", &perms).unwrap();

    // Any trigger should never grant "allow" to shell
    for trigger in &[
        model::ladder::Trigger::Chat,
        model::ladder::Trigger::Routine,
        model::ladder::Trigger::Goal,
        model::ladder::Trigger::Webhook,
    ] {
        let trigger_perms =
            server::permissions::permissions_for_run(&db, "test-bot", *trigger).unwrap();
        assert_ne!(
            trigger_perms.get("shell").copied(),
            Some(server::permissions::Decision::Allow),
            "shell should never be granted to {:?}",
            trigger
        );
    }
}

#[tokio::test]
async fn harmless_tools_stay_allowed_on_routine() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");

    // Set some harmless tools to allow
    let perms = server::permissions::Permissions::from_iter(vec![
        (
            "fetch_url".to_string(),
            server::permissions::Decision::Allow,
        ),
        ("remember".to_string(), server::permissions::Decision::Allow),
        ("add_task".to_string(), server::permissions::Decision::Allow),
    ]);
    server::permissions::set_permissions(&db, "test-bot", &perms).unwrap();

    // On routine, harmless tools should stay allowed
    let routine_perms =
        server::permissions::permissions_for_run(&db, "test-bot", model::ladder::Trigger::Routine)
            .unwrap();

    assert_eq!(
        routine_perms.get("fetch_url").copied(),
        Some(server::permissions::Decision::Allow)
    );
    assert_eq!(
        routine_perms.get("remember").copied(),
        Some(server::permissions::Decision::Allow)
    );
    assert_eq!(
        routine_perms.get("add_task").copied(),
        Some(server::permissions::Decision::Allow)
    );
}

#[test]
fn propose_tool_always_ask_even_if_allow() {
    let decision = server::permissions::decide_call(
        server::permissions::Decision::Allow,
        "propose_tool",
        "{}",
        false,
    );
    assert_eq!(decision, server::permissions::Decision::Ask);

    // But deny stays deny
    let decision = server::permissions::decide_call(
        server::permissions::Decision::Deny,
        "propose_tool",
        "{}",
        false,
    );
    assert_eq!(decision, server::permissions::Decision::Deny);
}

#[test]
fn purchase_always_ask_even_if_allow() {
    let decision = server::permissions::decide_call(
        server::permissions::Decision::Allow,
        "purchase",
        "{}",
        false,
    );
    assert_eq!(decision, server::permissions::Decision::Ask);

    // But deny stays deny
    let decision = server::permissions::decide_call(
        server::permissions::Decision::Deny,
        "purchase",
        "{}",
        false,
    );
    assert_eq!(decision, server::permissions::Decision::Deny);
}

// ---- S13b-03-02: the pin that cannot be lifted (design §4.6/§7 bite 11) ----
//
// `read_file` is the one CLIENT_FULFILLED tool this port has registered so
// far, and gets the identical `decide_call` pin `propose_tool`/`purchase`
// already had - "always allow" in the grid must not be able to skip the one
// human step a client-fulfilled read needs. Mutation: drop `"read_file"`
// from the `matches!` in `decide_call` and this goes red - `decide_call`
// would just hand back the stored `Allow` untouched.
#[test]
fn read_file_always_ask_even_if_allow() {
    let decision = server::permissions::decide_call(
        server::permissions::Decision::Allow,
        "read_file",
        "{}",
        false,
    );
    assert_eq!(decision, server::permissions::Decision::Ask);

    // But deny stays deny - "Never" still works.
    let decision = server::permissions::decide_call(
        server::permissions::Decision::Deny,
        "read_file",
        "{}",
        false,
    );
    assert_eq!(decision, server::permissions::Decision::Deny);
}

// `cannot_be_lifted_at_all` is the SEPARATE guard `rules::resolve_decision`
// consults - deliberately not the same code object `decide_call` reads
// above, so each can be broken (and proven) independently. Mutation: drop
// a name from the `matches!` inside `cannot_be_lifted_at_all` and its own
// assertion below goes red, while `decide_call`'s pin (tested above) stays
// green - proving the two guards do not share a blind spot.
#[test]
fn cannot_be_lifted_at_all_covers_read_file_purchase_and_propose_tool() {
    for name in ["read_file", "purchase", "propose_tool"] {
        assert!(
            server::permissions::cannot_be_lifted_at_all(name),
            "{name} must never be liftable to allow by a rule, at any trigger"
        );
    }
    // A tool that is only unattended-tightened (not pinned) must NOT be in
    // this stricter set, or the chat-trigger floor would wrongly swallow
    // tools Josh is allowed to lift himself while watching.
    assert!(!server::permissions::cannot_be_lifted_at_all("shell"));
}

// The THIRD independent guard (`routes/approvals.rs::decide`'s `remember`
// refusal) - a separate code object from `cannot_be_lifted_at_all` on
// purpose, proven by `tests/rules.rs`'s
// `remembering_read_file_as_allow_is_refused_and_writes_neither` over HTTP.
// This is the fast unit-level companion: mutation: drop a name from
// `cannot_be_remembered_as_allow`'s `matches!` and this goes red without
// touching `cannot_be_lifted_at_all`'s own test above.
#[test]
fn cannot_be_remembered_as_allow_covers_read_file_purchase_and_propose_tool() {
    for name in ["read_file", "purchase", "propose_tool"] {
        assert!(
            server::permissions::cannot_be_remembered_as_allow(name),
            "{name} must never be rememberable as allow"
        );
    }
    assert!(!server::permissions::cannot_be_remembered_as_allow("shell"));
}

// ---- S13b-03-03: a run's own taint tightens thirteen names to "ask"
// (design §4.5). Pure unit-level companion to the bite-10 integration
// tests in `tests/approvals.rs` (parts a/b/c) and `tests/rules.rs` (part
// d) - this proves `decide_call`'s own half of the mechanism without
// spinning up a run. ----

const TAINT_TIGHTENED_TOOLS: &[&str] = &[
    "browse",
    "read_page",
    "message_bot",
    "remember",
    "note",
    "remember_shared",
    "project_remember",
    "set_goal",
    "update_goal",
    "reflect",
    "say",
    "shell",
];

#[test]
fn taint_tightens_allow_to_ask_for_every_named_tool_except_ask_josh() {
    // `ask_josh` gets its own test below - its OWN branch has a special
    // case (non-wait -> allow, whatever `base` says) that only a taint
    // check running BEFORE that branch can override, which is exactly
    // what the next test isolates.
    for tool in TAINT_TIGHTENED_TOOLS {
        let decision = server::permissions::decide_call(
            server::permissions::Decision::Allow,
            tool,
            "{}",
            true,
        );
        assert_eq!(
            decision,
            server::permissions::Decision::Ask,
            "{tool} must tighten to ask once the run is tainted"
        );
    }
}

#[test]
fn taint_never_widens_a_stored_deny() {
    for tool in TAINT_TIGHTENED_TOOLS {
        let decision =
            server::permissions::decide_call(server::permissions::Decision::Deny, tool, "{}", true);
        assert_eq!(
            decision,
            server::permissions::Decision::Deny,
            "{tool}'s stored deny must survive taint tightening - the taint only ever \
             TIGHTENS, it must never widen a tool Josh turned off"
        );
    }
}

#[test]
fn an_untainted_run_leaves_these_tools_alone() {
    for tool in TAINT_TIGHTENED_TOOLS {
        let decision = server::permissions::decide_call(
            server::permissions::Decision::Allow,
            tool,
            "{}",
            false,
        );
        assert_eq!(
            decision,
            server::permissions::Decision::Allow,
            "{tool} must not tighten when `tainted` is false"
        );
    }
}

// THE bite that matters for `decide_call` itself (design §4.5's own 🔴): a
// NON-wait `ask_josh` must tighten to "ask" under taint, proving the taint
// check runs BEFORE the `ask_josh`-specific branch - that branch otherwise
// returns `Allow` for a non-wait `ask_josh` WHATEVER `base` says, so a
// taint check placed after it would do nothing at all. Mutation: move the
// taint check below `if tool_name != "ask_josh" { return base; }`, and
// this goes red while `taint_tightens_allow_to_ask_for_every_named_tool_
// except_ask_josh` (which never exercises ask_josh's early-return branch)
// stays green.
#[test]
fn taint_tightens_a_non_wait_ask_josh_even_though_its_own_branch_says_allow() {
    let decision = server::permissions::decide_call(
        server::permissions::Decision::Allow,
        "ask_josh",
        r#"{"wait": false}"#,
        true,
    );
    assert_eq!(decision, server::permissions::Decision::Ask);

    let decision = server::permissions::decide_call(
        server::permissions::Decision::Allow,
        "ask_josh",
        "{}",
        true,
    );
    assert_eq!(decision, server::permissions::Decision::Ask);
}

#[test]
fn ask_josh_with_wait_true_returns_ask() {
    let decision = server::permissions::decide_call(
        server::permissions::Decision::Allow,
        "ask_josh",
        r#"{"wait": true}"#,
        false,
    );
    assert_eq!(decision, server::permissions::Decision::Ask);
}

#[test]
fn ask_josh_without_wait_returns_allow() {
    let decision = server::permissions::decide_call(
        server::permissions::Decision::Allow,
        "ask_josh",
        r#"{"wait": false}"#,
        false,
    );
    assert_eq!(decision, server::permissions::Decision::Allow);

    let decision = server::permissions::decide_call(
        server::permissions::Decision::Allow,
        "ask_josh",
        "{}",
        false,
    );
    assert_eq!(decision, server::permissions::Decision::Allow);

    let decision = server::permissions::decide_call(
        server::permissions::Decision::Allow,
        "ask_josh",
        "",
        false,
    );
    assert_eq!(decision, server::permissions::Decision::Allow);
}

#[test]
fn ask_josh_bad_json_returns_allow() {
    let decision = server::permissions::decide_call(
        server::permissions::Decision::Allow,
        "ask_josh",
        "not json",
        false,
    );
    assert_eq!(decision, server::permissions::Decision::Allow);
}

// ---- F10: a lenient PUT ----

// A client (a stale tab, or an old iOS build) that sends one legacy value
// ("always", never a real `Decision`) next to a valid one must not lose
// the valid change too - TS's own `setPermissions` (`permissions.ts:512-
// 521`) keeps the entries it can parse and drops the rest, where the
// pre-fix typed `HashMap<String, Decision>` body made serde reject the
// WHOLE PUT the moment any one value failed to parse.
#[tokio::test]
async fn put_with_one_bad_value_keeps_the_good_ones() {
    let db = {
        let db = open_db();
        store::set_password(&db, "test-password").expect("set password");
        seed_bot(&db, "test-bot", "Test Bot");
        db
    };

    let session = seed_session(&db);
    let app = app_for(db);

    let overrides = json!({
        "click": "deny",
        "shell": "always",
    });

    let (status, _body) = put_permissions_route(&app, "test-bot", &session, overrides).await;
    assert_eq!(status, 200, "one bad value must not 400 the whole PUT");

    let (_status, body) = get_permissions_route(&app, "test-bot", &session).await;
    let perms = body.get("permissions").unwrap().as_object().unwrap();
    assert_eq!(
        perms.get("click").and_then(|v| v.as_str()),
        Some("deny"),
        "the valid entry alongside the bad one must still be saved"
    );
    assert_eq!(
        perms.get("shell").and_then(|v| v.as_str()),
        Some("ask"),
        "the unparseable value must be dropped, leaving shell at its default"
    );
}

// ---- B-F9: the route stores only overrides, never the whole merged map ----

// B-F9: the client PUTs back the whole map it got from GET (defaults
// merged with overrides), and `set_permissions` used to store whatever it
// was handed wholesale - so the first touch on any one toggle froze every
// CURRENT default into the bot's row, and a later tightening of
// `default_decisions()` would never reach it. Storing only the divergence
// means a map IDENTICAL to `default_decisions()` leaves nothing stored.
// Bite: revert `set_permissions` to store `permissions` wholesale and the
// assertion below goes red - the raw column holds all N default entries
// instead of `{}`.
#[test]
fn storing_the_full_default_map_leaves_no_explicit_overrides() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");

    let defaults = server::permissions::default_decisions();
    server::permissions::set_permissions(&db, "test-bot", &defaults).expect("set perms");

    let raw: String = db
        .conn()
        .query_row(
            "SELECT permissions FROM bots WHERE id = ?1",
            ["test-bot"],
            |row| row.get(0),
        )
        .expect("read raw permissions column");
    let parsed: Value = serde_json::from_str(&raw).expect("parse stored permissions json");
    assert_eq!(
        parsed.as_object().map(|o| o.len()),
        Some(0),
        "expected no explicit overrides stored, got {raw}"
    );

    // The merged read-back is still every default, unaffected by nothing
    // being stored.
    let merged = server::permissions::get_permissions(&db, "test-bot").expect("get perms");
    assert_eq!(
        merged.get("click").copied(),
        Some(server::permissions::Decision::Allow)
    );
}

// ---- T6: the shell session aliases ----

// S10/T6: `shell_open`/`shell_write`/`shell_read` carry no permission row
// of their own - they take whatever `shell` resolves to unless stored
// explicitly. Bite: delete the `SHELL_SESSION_TOOLS` loop in
// `permissions.rs`'s `get_permissions` and this goes red - the three
// aliases fall back to their own (absent) `default_decisions()` entry
// instead of following `shell`.
#[test]
fn shell_aliases_follow_shells_own_decision_when_not_stored_explicitly() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");

    let perms = server::permissions::Permissions::from_iter(vec![(
        "shell".to_string(),
        server::permissions::Decision::Allow,
    )]);
    server::permissions::set_permissions(&db, "test-bot", &perms).expect("set perms");

    let merged = server::permissions::get_permissions(&db, "test-bot").expect("get perms");
    for alias in ["shell_open", "shell_write", "shell_read"] {
        assert_eq!(
            merged.get(alias).copied(),
            Some(server::permissions::Decision::Allow),
            "{alias} should follow shell's allow"
        );
    }
}

// T6: an alias stored EXPLICITLY survives, even while `shell` itself is
// something else.
#[test]
fn an_explicit_shell_alias_override_survives_shells_own_decision() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");

    let perms = server::permissions::Permissions::from_iter(vec![
        ("shell".to_string(), server::permissions::Decision::Allow),
        (
            "shell_read".to_string(),
            server::permissions::Decision::Deny,
        ),
    ]);
    server::permissions::set_permissions(&db, "test-bot", &perms).expect("set perms");

    let merged = server::permissions::get_permissions(&db, "test-bot").expect("get perms");
    assert_eq!(
        merged.get("shell_open").copied(),
        Some(server::permissions::Decision::Allow)
    );
    assert_eq!(
        merged.get("shell_write").copied(),
        Some(server::permissions::Decision::Allow)
    );
    assert_eq!(
        merged.get("shell_read").copied(),
        Some(server::permissions::Decision::Deny),
        "an explicit alias override must survive shell's own decision"
    );
}
