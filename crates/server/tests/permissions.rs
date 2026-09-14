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
    );
    assert_eq!(decision, server::permissions::Decision::Ask);

    // But deny stays deny
    let decision =
        server::permissions::decide_call(server::permissions::Decision::Deny, "propose_tool", "{}");
    assert_eq!(decision, server::permissions::Decision::Deny);
}

#[test]
fn purchase_always_ask_even_if_allow() {
    let decision =
        server::permissions::decide_call(server::permissions::Decision::Allow, "purchase", "{}");
    assert_eq!(decision, server::permissions::Decision::Ask);

    // But deny stays deny
    let decision =
        server::permissions::decide_call(server::permissions::Decision::Deny, "purchase", "{}");
    assert_eq!(decision, server::permissions::Decision::Deny);
}

#[test]
fn ask_josh_with_wait_true_returns_ask() {
    let decision = server::permissions::decide_call(
        server::permissions::Decision::Allow,
        "ask_josh",
        r#"{"wait": true}"#,
    );
    assert_eq!(decision, server::permissions::Decision::Ask);
}

#[test]
fn ask_josh_without_wait_returns_allow() {
    let decision = server::permissions::decide_call(
        server::permissions::Decision::Allow,
        "ask_josh",
        r#"{"wait": false}"#,
    );
    assert_eq!(decision, server::permissions::Decision::Allow);

    let decision =
        server::permissions::decide_call(server::permissions::Decision::Allow, "ask_josh", "{}");
    assert_eq!(decision, server::permissions::Decision::Allow);

    let decision =
        server::permissions::decide_call(server::permissions::Decision::Allow, "ask_josh", "");
    assert_eq!(decision, server::permissions::Decision::Allow);
}

#[test]
fn ask_josh_bad_json_returns_allow() {
    let decision = server::permissions::decide_call(
        server::permissions::Decision::Allow,
        "ask_josh",
        "not json",
    );
    assert_eq!(decision, server::permissions::Decision::Allow);
}
