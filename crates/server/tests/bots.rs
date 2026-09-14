//! S2-09b: `PATCH /api/bots/:id` for the model pin and reasoning effort.
//! Port of `projects/bullpen-night/src/server/app.ts:1190-1240`'s `model`/
//! `effort` handling, narrowed to the premium refusal (reusing
//! `routes/settings.rs::refuse_if_premium`) - see that route's own doc.

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use serde_json::{Value, json};
use server::AppState;
use std::sync::Arc;
use store::Db;
use tower::ServiceExt;

mod common;
use common::seed_session;

fn open_db() -> Db {
    Db::open(":memory:").expect("open :memory: db")
}

/// F2/D9: `PATCH /api/bots/:id`'s model pin now judges against the
/// catalogue (`model::judge_pin`), so any test that expects a pin to be
/// ACCEPTED needs one that actually lists the model - `AppState::new`'s
/// default is an empty fixture in tests, which would refuse every pin with
/// "OpenRouter does not list this model."
fn fixture_catalog() -> Arc<dyn model::catalog::Catalog> {
    let json = r#"[
        {
            "id": "anthropic/claude-sonnet-5",
            "name": "Claude Sonnet 5",
            "inPerM": 3.0,
            "outPerM": 15.0,
            "contextLength": 200000,
            "supportsTools": true,
            "supportsImages": true,
            "supportsReasoning": false,
            "providerCount": 2,
            "supportsCaching": true
        },
        {
            "id": "anthropic/claude-fable-5.1",
            "name": "Claude Fable 5.1",
            "inPerM": 1.5,
            "outPerM": 6.0,
            "contextLength": 100000,
            "supportsTools": true,
            "supportsImages": true,
            "supportsReasoning": false,
            "providerCount": 2,
            "supportsCaching": true
        }
    ]"#;
    Arc::new(model::catalog::FixtureCatalog::from_json(json).expect("parse fixture"))
}

fn app_with_catalog(db: Db, catalog: Arc<dyn model::catalog::Catalog>) -> Router {
    let state = AppState::with_catalog(db, catalog);
    server::build_app(state)
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

async fn patch_route(app: &Router, path: &str, session: &str, body: Value) -> (u16, Value) {
    let request = Request::patch(path)
        .header("cookie", session)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    (status, value)
}

async fn get_route(app: &Router, path: &str, session: &str) -> (u16, Value) {
    let request = Request::get(path)
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

/// Bite: the model pin round-trips - PATCH sets it, and the roster (which
/// the client actually reads it back from - there is no `GET /api/bots/:id`)
/// shows the new value on the right bot, not just the PATCH's own echo.
#[tokio::test]
async fn patch_model_round_trips_via_roster() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");
    let session = seed_session(&db);
    let app = app_with_catalog(db, fixture_catalog());

    let (status, response) = patch_route(
        &app,
        "/api/bots/test-bot",
        &session,
        json!({ "model": "anthropic/claude-sonnet-5" }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["bot"]["model"], "anthropic/claude-sonnet-5");

    let (status, roster) = get_route(&app, "/api/roster", &session).await;
    assert_eq!(status, 200);
    let bots = roster["bots"].as_array().unwrap();
    let bot = bots.iter().find(|b| b["id"] == "test-bot").unwrap();
    assert_eq!(bot["model"], "anthropic/claude-sonnet-5");

    // Clearing the pin (null) round-trips too.
    let (status, response) = patch_route(
        &app,
        "/api/bots/test-bot",
        &session,
        json!({ "model": null }),
    )
    .await;
    assert_eq!(status, 200);
    assert!(response["bot"]["model"].is_null());
}

/// A premium pin is refused with the exact text `/api/default-model` uses -
/// they share `refuse_if_premium`, so a pin that should be refused and is
/// not means that helper stopped being called here.
#[tokio::test]
async fn patch_model_refuses_premium() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) = patch_route(
        &app,
        "/api/bots/test-bot",
        &session,
        json!({ "model": "anthropic/claude-fable-5.1" }),
    )
    .await;
    assert_eq!(status, 400);
    assert!(
        response["error"]
            .as_str()
            .unwrap_or("")
            .contains("premium model")
    );

    // Bite: the pin must not have landed - GET the roster and confirm the
    // refused model never reached the bot row.
    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    let bots = roster["bots"].as_array().unwrap();
    let bot = bots.iter().find(|b| b["id"] == "test-bot").unwrap();
    assert!(bot["model"].is_null(), "a refused pin must not be stored");
}

/// F2/D9: `judge_pin` refuses a `:batch` slug with TS's message - "13 of 15
/// bots died on it on the previous platform" (S2-R-spec.md D9). Bite: before
/// this ticket, `judge_pin` had no caller here at all, so this pin landed
/// and a run against it would 404.
#[tokio::test]
async fn patch_model_refuses_batch_suffix() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");
    let session = seed_session(&db);
    let app = app_with_catalog(db, fixture_catalog());

    let (status, response) = patch_route(
        &app,
        "/api/bots/test-bot",
        &session,
        json!({ "model": "anthropic/claude-sonnet-5:batch" }),
    )
    .await;
    assert_eq!(status, 400);
    assert!(
        response["error"]
            .as_str()
            .unwrap_or("")
            .contains("batch-only")
    );

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    let bots = roster["bots"].as_array().unwrap();
    let bot = bots.iter().find(|b| b["id"] == "test-bot").unwrap();
    assert!(bot["model"].is_null(), "a refused pin must not be stored");
}

#[tokio::test]
async fn patch_effort_round_trips() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) = patch_route(
        &app,
        "/api/bots/test-bot",
        &session,
        json!({ "effort": "high" }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["bot"]["effort"], "high");

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    let bots = roster["bots"].as_array().unwrap();
    let bot = bots.iter().find(|b| b["id"] == "test-bot").unwrap();
    assert_eq!(bot["effort"], "high");
}

#[tokio::test]
async fn patch_effort_rejects_bad_value() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) = patch_route(
        &app,
        "/api/bots/test-bot",
        &session,
        json!({ "effort": "extreme" }),
    )
    .await;
    assert_eq!(status, 400);
    assert!(
        response["error"]
            .as_str()
            .unwrap_or("")
            .contains("low, medium or high")
    );
}

#[tokio::test]
async fn patch_no_such_bot_is_404() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, _response) = patch_route(
        &app,
        "/api/bots/does-not-exist",
        &session,
        json!({ "effort": "high" }),
    )
    .await;
    assert_eq!(status, 404);
}
