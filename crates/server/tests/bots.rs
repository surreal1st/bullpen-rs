//! S2-09b: `PATCH /api/bots/:id` for the model pin and reasoning effort.
//! Port of `projects/bullpen-night/src/server/app.ts:1190-1240`'s `model`/
//! `effort` handling, narrowed to the premium refusal (reusing
//! `routes/settings.rs::refuse_if_premium`) - see that route's own doc.

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request};
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

async fn post_route(app: &Router, path: &str, session: &str, body: Value) -> (u16, Value) {
    let request = Request::post(path)
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

/* --------------------------------------------------------- F7b-01: create */

/// Bite: creating a bot returns 201 with the bot in the body, and a
/// subsequent roster fetch carries it too - not just the POST's own echo.
#[tokio::test]
async fn create_bot_returns_201_and_appears_in_roster() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) = post_route(
        &app,
        "/api/bots",
        &session,
        json!({ "name": "Trinity", "purpose": "Watches the error log" }),
    )
    .await;
    assert_eq!(status, 201);
    assert_eq!(response["bot"]["name"], "Trinity");
    assert_eq!(response["bot"]["id"], "trinity");
    assert_eq!(response["bot"]["purpose"], "Watches the error log");
    assert!(response["bot"]["model"].is_null());

    let (status, roster) = get_route(&app, "/api/roster", &session).await;
    assert_eq!(status, 200);
    let bots = roster["bots"].as_array().unwrap();
    assert!(
        bots.iter()
            .any(|b| b["id"] == "trinity" && b["name"] == "Trinity"),
        "roster must carry the new bot: {bots:?}"
    );
}

/// Bite: the name-required check. A blank name and a whitespace-only name
/// are both refused, and neither leaves a row behind.
#[tokio::test]
async fn create_bot_blank_or_whitespace_name_is_400_and_roster_unchanged() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    for name in ["", "   "] {
        let (status, response) =
            post_route(&app, "/api/bots", &session, json!({ "name": name })).await;
        assert_eq!(status, 400, "name {name:?}");
        assert_eq!(response["error"], "name is required");
    }

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    assert!(
        roster["bots"].as_array().unwrap().is_empty(),
        "a rejected name must not create a row"
    );
}

/// Bite: slug uniqueness. Two bots named "Trinity" get `trinity` and
/// `trinity-2` - without the uniqueness check the second either collides
/// with the first (an insert error, no 201) or overwrites it (the roster
/// would carry one bot, not two).
#[tokio::test]
async fn create_bot_duplicate_name_gets_numbered_slug() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let (status1, first) =
        post_route(&app, "/api/bots", &session, json!({ "name": "Trinity" })).await;
    assert_eq!(status1, 201);
    assert_eq!(first["bot"]["id"], "trinity");

    let (status2, second) =
        post_route(&app, "/api/bots", &session, json!({ "name": "Trinity" })).await;
    assert_eq!(status2, 201);
    assert_eq!(second["bot"]["id"], "trinity-2");

    let (status3, third) =
        post_route(&app, "/api/bots", &session, json!({ "name": "Trinity" })).await;
    assert_eq!(status3, 201);
    assert_eq!(third["bot"]["id"], "trinity-3");

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    let bots = roster["bots"].as_array().unwrap();
    assert_eq!(bots.len(), 3, "all three must have landed as distinct rows");
}

/// Bite: the empty-slug fallback. A name that slugs to nothing (only
/// punctuation) becomes the id `"bot"` rather than an empty string.
#[tokio::test]
async fn create_bot_name_with_no_alnum_becomes_bot() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) =
        post_route(&app, "/api/bots", &session, json!({ "name": "!!!" })).await;
    assert_eq!(status, 201);
    assert_eq!(response["bot"]["id"], "bot");
}

/// Bite: the 40-character truncation. A name that slugs to more than 40
/// characters is cut to exactly 40 - without the truncation, the id would
/// be the full 45-character slug instead.
#[tokio::test]
async fn create_bot_slug_truncates_to_40_chars() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let long_name = "a".repeat(45);
    let (status, response) =
        post_route(&app, "/api/bots", &session, json!({ "name": long_name })).await;
    assert_eq!(status, 201);
    let id = response["bot"]["id"].as_str().unwrap();
    assert_eq!(id.len(), 40);
    assert_eq!(id, "a".repeat(40));
}

/// Bite: `judge_pin` runs BEFORE the insert. A model the fixture catalog
/// refuses is a 400 with the refusal text, and the roster stays completely
/// empty afterward - not just the status code, the row itself must never
/// have landed (if `judge_pin` ran after the insert instead, this would
/// still be 400 but the roster would carry one bot).
#[tokio::test]
async fn create_bot_refused_model_is_400_and_nothing_inserted() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_with_catalog(db, fixture_catalog());

    let (status, response) = post_route(
        &app,
        "/api/bots",
        &session,
        json!({ "name": "Trinity", "model": "unknown/does-not-exist" }),
    )
    .await;
    assert_eq!(status, 400);
    assert!(
        response["error"]
            .as_str()
            .unwrap_or("")
            .contains("does not list this model")
    );

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    assert!(
        roster["bots"].as_array().unwrap().is_empty(),
        "a refused pin must not create the row at all"
    );
}

/// A body with no `model` key at all creates a bot with a null pin - the
/// platform default, same as the TS route's own `model ?? null`.
#[tokio::test]
async fn create_bot_without_model_key_has_null_pin() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) =
        post_route(&app, "/api/bots", &session, json!({ "name": "Trinity" })).await;
    assert_eq!(status, 201);
    assert!(response["bot"]["model"].is_null());
}

/* ------------------------------------------------------ ARCH-01: archive */

/// Bite: archiving answers 200 with the bot (now `archived: true`), and the
/// live roster - the thing the rail actually renders from - no longer
/// carries it. Without `set_archived` stamping `archived_at`, or without
/// `list_roster`'s own `WHERE archived_at IS NULL`, this bot would still
/// show up here.
#[tokio::test]
async fn archive_returns_200_and_roster_drops_the_bot() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) = post_route(
        &app,
        "/api/bots/test-bot/archive",
        &session,
        json!({ "archived": true }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["bot"]["archived"], true);

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    let bots = roster["bots"].as_array().unwrap();
    assert!(
        !bots.iter().any(|b| b["id"] == "test-bot"),
        "an archived bot must not appear in the live roster: {bots:?}"
    );
}

/// Bite: the archived-bot listing is the only place an archived bot is
/// still reachable - without `GET /api/bots/archived` (or if it read the
/// same `WHERE archived_at IS NULL` the roster does), this bot would be
/// unlisted anywhere and therefore unrestorable.
#[tokio::test]
async fn archived_listing_carries_the_archived_bot() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, _response) = post_route(
        &app,
        "/api/bots/test-bot/archive",
        &session,
        json!({ "archived": true }),
    )
    .await;
    assert_eq!(status, 200);

    let (status, archived) = get_route(&app, "/api/bots/archived", &session).await;
    assert_eq!(status, 200);
    let bots = archived["bots"].as_array().unwrap();
    let bot = bots
        .iter()
        .find(|b| b["id"] == "test-bot")
        .expect("archived bot must be in the archived listing");
    assert_eq!(bot["archived"], true);
}

/// Bite: restoring answers 200, and the bot is back in the live roster -
/// proves `set_archived(..., false)` actually NULLs `archived_at` rather
/// than, say, doing nothing on the "un-archive" branch.
#[tokio::test]
async fn restore_returns_200_and_roster_carries_the_bot_again() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, _response) = post_route(
        &app,
        "/api/bots/test-bot/archive",
        &session,
        json!({ "archived": true }),
    )
    .await;
    assert_eq!(status, 200);

    let (status, response) = post_route(
        &app,
        "/api/bots/test-bot/archive",
        &session,
        json!({ "archived": false }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["bot"]["archived"], false);

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    let bots = roster["bots"].as_array().unwrap();
    assert!(
        bots.iter().any(|b| b["id"] == "test-bot"),
        "a restored bot must be back in the live roster: {bots:?}"
    );
}

/// Bite: `{"archived": false}` on a bot that is not archived is a no-op
/// success (200, `archived: false`), not an error - the route must not
/// assume "restore" implies "was archived".
#[tokio::test]
async fn restore_on_an_unarchived_bot_is_a_noop_success() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) = post_route(
        &app,
        "/api/bots/test-bot/archive",
        &session,
        json!({ "archived": false }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["bot"]["archived"], false);

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    let bots = roster["bots"].as_array().unwrap();
    assert!(bots.iter().any(|b| b["id"] == "test-bot"));
}

/// Bite: a missing body archives - matching the TS's `body.archived !==
/// false` exactly. This is the surprising branch the ticket calls out by
/// name: tighten the route to "only `true` archives" (the mutation the
/// coordinator will try) and this goes red, since an empty body has no
/// `archived` key at all to be `true`.
#[tokio::test]
async fn missing_body_archives() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");
    let session = seed_session(&db);
    let app = app_for(db);

    let request = axum::http::Request::post("/api/bots/test-bot/archive")
        .header("cookie", session.clone())
        .body(axum::body::Body::empty())
        .unwrap();
    let response = tower::ServiceExt::oneshot(app.clone(), request)
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    assert_eq!(status, 200);
    assert_eq!(value["bot"]["archived"], true);

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    let bots = roster["bots"].as_array().unwrap();
    assert!(
        !bots.iter().any(|b| b["id"] == "test-bot"),
        "a missing body must archive, same as the TS: {bots:?}"
    );
}

/// Bite: an unknown id is 404 for both directions, and nothing is written -
/// checked by re-reading the (empty) roster and the (empty) archived
/// listing afterward, not just the status code.
#[tokio::test]
async fn archive_and_restore_unknown_id_is_404_and_writes_nothing() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    for archived in [true, false] {
        let (status, response) = post_route(
            &app,
            "/api/bots/does-not-exist/archive",
            &session,
            json!({ "archived": archived }),
        )
        .await;
        assert_eq!(status, 404, "archived={archived}");
        assert_eq!(response["error"], "no such bot");
    }

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    assert!(roster["bots"].as_array().unwrap().is_empty());
    let (_status, archived) = get_route(&app, "/api/bots/archived", &session).await;
    assert!(archived["bots"].as_array().unwrap().is_empty());
}

/// Bite: archiving is not deletion. A bot's conversation, its messages, and
/// its memory (core + log entry) all still exist, unchanged, after
/// archiving - read directly through the store, not just "the route did not
/// 500", since a 200 alone proves nothing about what else got touched.
#[tokio::test]
async fn archived_bots_conversations_messages_and_memory_survive() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");
    let session = seed_session(&db);

    let conversation_id =
        store::get_or_create_conversation(&db, "test-bot").expect("get_or_create_conversation");
    store::append_message(
        &db,
        &conversation_id,
        "user",
        "Remember this.",
        store::NewMessage::default(),
    )
    .expect("append message");
    store::set_core(&db, "test-bot", "Core fact about this bot.").expect("set_core");
    store::remember(&db, "test-bot", "A logged memory entry.", "josh").expect("remember");

    let app = app_for(db);

    let (status, response) = post_route(
        &app,
        "/api/bots/test-bot/archive",
        &session,
        json!({ "archived": true }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["bot"]["archived"], true);

    // Re-fetch the conversation through the same route a live bot uses -
    // `get_bot` (which `store::get_conversation`/`get_or_create_conversation`
    // sit beside) is deliberately not filtered by `archived_at`, so an
    // archived bot's own data must still be reachable through it.
    let conv_url = format!("/api/bots/test-bot/conversation?thread={conversation_id}");
    let request = axum::http::Request::get(&conv_url)
        .header("cookie", session.clone())
        .body(axum::body::Body::empty())
        .unwrap();
    let conv_response = tower::ServiceExt::oneshot(app.clone(), request)
        .await
        .unwrap();
    assert_eq!(conv_response.status().as_u16(), 200);
    let body = axum::body::to_bytes(conv_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let conv: Value = serde_json::from_slice(&body).unwrap();
    let messages = conv["messages"].as_array().unwrap();
    assert!(
        messages.iter().any(|m| m["content"] == "Remember this."),
        "an archived bot's messages must survive: {messages:?}"
    );

    let mem_url = "/api/bots/test-bot/memory";
    let request = axum::http::Request::get(mem_url)
        .header("cookie", session.clone())
        .body(axum::body::Body::empty())
        .unwrap();
    let mem_response = tower::ServiceExt::oneshot(app.clone(), request)
        .await
        .unwrap();
    assert_eq!(mem_response.status().as_u16(), 200);
    let body = axum::body::to_bytes(mem_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let mem: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(mem["core"], "Core fact about this bot.");
    let log = mem["log"].as_array().unwrap();
    assert!(
        log.iter().any(|e| e["content"] == "A logged memory entry."),
        "an archived bot's memory log must survive: {log:?}"
    );
}

/* --------------------------------------------------------------- RAIL-01 */

fn ids_in_order(bots: &Value) -> Vec<String> {
    bots.as_array()
        .unwrap()
        .iter()
        .map(|b| b["id"].as_str().unwrap().to_string())
        .collect()
}

/// Bite: pinning must move the bot to the FRONT of the roster, not just
/// flip its flag - checked against both the PATCH's own echoed roster and a
/// fresh `/api/roster` fetch, so a route that returned the right order once
/// but wrote an ORDER BY that only happens to match on this one query
/// cannot pass by accident. Unpinning puts it back in plain name order.
#[tokio::test]
async fn pinning_moves_the_bot_to_the_front_unpinning_returns_it_to_name_order() {
    let db = open_db();
    seed_bot(&db, "alpha", "Alpha");
    seed_bot(&db, "bravo", "Bravo");
    seed_bot(&db, "charlie", "Charlie");
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) = patch_route(
        &app,
        "/api/bots/charlie/rail",
        &session,
        json!({ "pinned": true }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        ids_in_order(&response["bots"]),
        vec!["charlie", "alpha", "bravo"],
        "a pinned bot must lead the roster, not just carry pinned:true"
    );

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    assert_eq!(
        ids_in_order(&roster["bots"]),
        vec!["charlie", "alpha", "bravo"],
        "the order must hold on a fresh fetch too, not just the PATCH's own echo"
    );

    let (status, response) = patch_route(
        &app,
        "/api/bots/charlie/rail",
        &session,
        json!({ "pinned": false }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        ids_in_order(&response["bots"]),
        vec!["alpha", "bravo", "charlie"],
        "unpinning must put the bot back in plain name order"
    );
}

/// Bite: hiding flags the bot (`hidden: true` on the roster row) and puts
/// it in the hidden listing; unhiding clears the flag and drops it from
/// that listing. `rail.rs:44`'s client-side `.filter(|b| !b.hidden)` is
/// what actually keeps a hidden bot off the rendered rail (see
/// `crate::store::roster::list_roster`'s own doc on why `/api/roster`
/// itself still carries it) - this test proves the server-observable half:
/// the flag round-trips and the hidden listing is the way back.
#[tokio::test]
async fn hiding_flags_the_bot_and_the_hidden_listing_carries_it() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) = patch_route(
        &app,
        "/api/bots/test-bot/rail",
        &session,
        json!({ "hidden": true }),
    )
    .await;
    assert_eq!(status, 200);
    let bots = response["bots"].as_array().unwrap();
    let bot = bots.iter().find(|b| b["id"] == "test-bot").unwrap();
    assert_eq!(bot["hidden"], true);

    let (status, hidden) = get_route(&app, "/api/bots/hidden", &session).await;
    assert_eq!(status, 200);
    let hidden_bots = hidden["bots"].as_array().unwrap();
    assert!(
        hidden_bots.iter().any(|b| b["id"] == "test-bot"),
        "a hidden bot must be in the hidden listing: {hidden_bots:?}"
    );
}

/// Bite: unhiding is not a one-way door in reverse either - the flag clears
/// and the bot drops out of the hidden listing.
#[tokio::test]
async fn unhiding_clears_the_flag_and_drops_it_from_the_hidden_listing() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, _response) = patch_route(
        &app,
        "/api/bots/test-bot/rail",
        &session,
        json!({ "hidden": true }),
    )
    .await;
    assert_eq!(status, 200);

    let (status, response) = patch_route(
        &app,
        "/api/bots/test-bot/rail",
        &session,
        json!({ "hidden": false }),
    )
    .await;
    assert_eq!(status, 200);
    let bots = response["bots"].as_array().unwrap();
    let bot = bots.iter().find(|b| b["id"] == "test-bot").unwrap();
    assert_eq!(bot["hidden"], false);

    let (_status, hidden) = get_route(&app, "/api/bots/hidden", &session).await;
    let hidden_bots = hidden["bots"].as_array().unwrap();
    assert!(
        !hidden_bots.iter().any(|b| b["id"] == "test-bot"),
        "an unhidden bot must not linger in the hidden listing: {hidden_bots:?}"
    );
}

/// Bite: `{}` is a no-op success, not an error - neither flag moves.
#[tokio::test]
async fn rail_empty_body_is_a_noop_success() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) =
        patch_route(&app, "/api/bots/test-bot/rail", &session, json!({})).await;
    assert_eq!(status, 200);
    let bots = response["bots"].as_array().unwrap();
    let bot = bots.iter().find(|b| b["id"] == "test-bot").unwrap();
    assert_eq!(bot["pinned"], false);
    assert_eq!(bot["hidden"], false);
}

/// Bite: a non-boolean `pinned`/`hidden` (a string, a number, `null`) is
/// IGNORED, matching the TS `typeof body[...] === "boolean"` guard exactly -
/// not refused with a 400, and not coerced (`"false"` must not hide/pin the
/// bot the way an "anything but literal false" coercion - the archive
/// route's OWN rule, on a different field - would). Both keys are checked,
/// not just `pinned`: the guard is two separate `if let` blocks in the
/// route, and a mutation that drops only one of them must not slip past
/// this test.
#[tokio::test]
async fn rail_non_boolean_values_are_ignored_not_refused() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");
    let session = seed_session(&db);
    let app = app_for(db);

    for value in [json!("false"), json!(1), json!(null)] {
        let (status, response) = patch_route(
            &app,
            "/api/bots/test-bot/rail",
            &session,
            json!({ "pinned": value }),
        )
        .await;
        assert_eq!(status, 200, "pinned value={value:?}");
        let bots = response["bots"].as_array().unwrap();
        let bot = bots.iter().find(|b| b["id"] == "test-bot").unwrap();
        assert_eq!(bot["pinned"], false, "pinned value={value:?} must not pin");

        let (status, response) = patch_route(
            &app,
            "/api/bots/test-bot/rail",
            &session,
            json!({ "hidden": value }),
        )
        .await;
        assert_eq!(status, 200, "hidden value={value:?}");
        let bots = response["bots"].as_array().unwrap();
        let bot = bots.iter().find(|b| b["id"] == "test-bot").unwrap();
        assert_eq!(bot["hidden"], false, "hidden value={value:?} must not hide");
    }
}

/// Bite: an unknown id is 404 with the exact ticket-specified body, checked
/// BEFORE anything is written - re-fetches both the roster and the hidden
/// listing afterward (both empty) rather than trusting the status code
/// alone.
#[tokio::test]
async fn rail_unknown_id_is_404_and_writes_nothing() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) = patch_route(
        &app,
        "/api/bots/does-not-exist/rail",
        &session,
        json!({ "pinned": true, "hidden": true }),
    )
    .await;
    assert_eq!(status, 404);
    assert_eq!(response["error"], "no such bot");

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    assert!(roster["bots"].as_array().unwrap().is_empty());
    let (_status, hidden) = get_route(&app, "/api/bots/hidden", &session).await;
    assert!(hidden["bots"].as_array().unwrap().is_empty());
}

/// Bite: hidden and archived are independent flags on the same row -
/// archiving a hidden bot keeps it hidden, and neither listing loses it;
/// restoring (un-archiving) it afterward must not clear the hidden flag as
/// a side effect.
#[tokio::test]
async fn hidden_and_archived_are_independent() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, _response) = patch_route(
        &app,
        "/api/bots/test-bot/rail",
        &session,
        json!({ "hidden": true }),
    )
    .await;
    assert_eq!(status, 200);
    let (status, _response) = post_route(
        &app,
        "/api/bots/test-bot/archive",
        &session,
        json!({ "archived": true }),
    )
    .await;
    assert_eq!(status, 200);

    let (_status, hidden) = get_route(&app, "/api/bots/hidden", &session).await;
    let hidden_bots = hidden["bots"].as_array().unwrap();
    let bot = hidden_bots
        .iter()
        .find(|b| b["id"] == "test-bot")
        .expect("an archived bot must still be in the hidden listing");
    assert_eq!(bot["archived"], true);
    assert_eq!(bot["hidden"], true);

    let (_status, archived) = get_route(&app, "/api/bots/archived", &session).await;
    let archived_bots = archived["bots"].as_array().unwrap();
    let bot = archived_bots
        .iter()
        .find(|b| b["id"] == "test-bot")
        .expect("a hidden bot must still be in the archived listing");
    assert_eq!(bot["archived"], true);
    assert_eq!(bot["hidden"], true);

    let (status, _response) = post_route(
        &app,
        "/api/bots/test-bot/archive",
        &session,
        json!({ "archived": false }),
    )
    .await;
    assert_eq!(status, 200);
    let (_status, hidden) = get_route(&app, "/api/bots/hidden", &session).await;
    let hidden_bots = hidden["bots"].as_array().unwrap();
    assert!(
        hidden_bots.iter().any(|b| b["id"] == "test-bot"),
        "restoring an archived bot must not clear its hidden flag: {hidden_bots:?}"
    );
}

/* --------------------------------------------------------------- RAIL-02 */
//
// `sectionId` on the same `PATCH /api/bots/:id/rail` route these RAIL-01
// tests above already exercise - kept in this file rather than the new
// `tests/sections.rs` (which owns the `POST`/`PATCH`/`DELETE /api/sections`
// routes) because these tests drive the SAME route/handler as
// `pinning_moves_the_bot_to_the_front...` and the rest above, and reuse
// their local helpers (`seed_bot`, `patch_route`, `get_route`) directly.

fn seed_section(db: &Db, id: &str, name: &str, position: i32) {
    db.conn()
        .execute(
            "INSERT INTO sections (id, name, position) VALUES (?1, ?2, ?3)",
            rusqlite::params![id, name, position],
        )
        .expect("seed section");
}

/// Bite: `sectionId` moves the bot - checked via `sectionId` on the PATCH's
/// own echoed roster row AND a fresh `/api/roster` fetch (same "not just the
/// echo" posture the pinning test above already takes). `null` and `""`
/// both fall back to Unassigned (`sectionId: null`), matching the TS guard
/// `typeof section === "string" && section !== "" ? section : null` exactly -
/// a mutation that only special-cased `null` (and let `""` slip through as
/// a literal empty-string section id) would still pass a narrower test than
/// this one.
#[tokio::test]
async fn rail_section_id_moves_the_bot_null_and_empty_string_unassign() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");
    seed_section(&db, "shoot", "SHOOT", 1);
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) = patch_route(
        &app,
        "/api/bots/test-bot/rail",
        &session,
        json!({ "sectionId": "shoot" }),
    )
    .await;
    assert_eq!(status, 200);
    let bots = response["bots"].as_array().unwrap();
    let bot = bots.iter().find(|b| b["id"] == "test-bot").unwrap();
    assert_eq!(bot["sectionId"], "shoot");

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    let bots = roster["bots"].as_array().unwrap();
    let bot = bots.iter().find(|b| b["id"] == "test-bot").unwrap();
    assert_eq!(
        bot["sectionId"], "shoot",
        "the move must hold on a fresh fetch too, not just the PATCH's own echo"
    );

    for unassign in [json!(null), json!("")] {
        let (status, response) = patch_route(
            &app,
            "/api/bots/test-bot/rail",
            &session,
            json!({ "sectionId": unassign }),
        )
        .await;
        assert_eq!(status, 200, "sectionId={unassign:?}");
        let bots = response["bots"].as_array().unwrap();
        let bot = bots.iter().find(|b| b["id"] == "test-bot").unwrap();
        assert!(
            bot["sectionId"].is_null(),
            "sectionId={unassign:?} must unassign: {bot:?}"
        );
    }
}

/// Bite: an unknown `sectionId` is 400 with the ticket's exact message, and
/// nothing else in the SAME request lands - `pinned` is sent alongside it
/// and must NOT be applied. Checked against a fresh roster fetch afterward,
/// not just the 400 status: a mutation that dropped the existence check
/// would move the bot to a section id that does not exist (still visible as
/// a `sectionId` the roster query never filters on) and this test would
/// catch the pin leaking through even if that half were somehow missed.
#[tokio::test]
async fn rail_unknown_section_id_is_400_and_nothing_in_the_request_applies() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) = patch_route(
        &app,
        "/api/bots/test-bot/rail",
        &session,
        json!({ "sectionId": "does-not-exist", "pinned": true }),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(response["error"], "no such section");

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    let bots = roster["bots"].as_array().unwrap();
    let bot = bots.iter().find(|b| b["id"] == "test-bot").unwrap();
    assert!(
        bot["sectionId"].is_null(),
        "an unknown section must not have moved the bot: {bot:?}"
    );
    assert_eq!(
        bot["pinned"], false,
        "pinned must not apply when sectionId in the same request is refused: {bot:?}"
    );
}

/* --------------------------------------------------------------- RAIL-03 */
//
// `avatar`/`shape` on the same `PATCH /api/bots/:id/rail` route the RAIL-01/
// RAIL-02 tests above already exercise. Kept in this file for the same
// reason the RAIL-02 block above is: same route, same handler, same local
// helpers (`seed_bot`, `patch_route`, `get_route`).

/// Bite: setting an avatar shows up on the bot in the roster, and clearing
/// it works both ways the TS route allows - an explicit `""` and an
/// explicit `null` both return it to NULL. Checked against a fresh
/// `/api/roster` fetch too, not just the PATCH's own echo, same "not just
/// the echo" posture every other rail test above takes.
#[tokio::test]
async fn avatar_sets_and_both_empty_string_and_null_clear_it() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) = patch_route(
        &app,
        "/api/bots/test-bot/rail",
        &session,
        json!({ "avatar": "🎉" }),
    )
    .await;
    assert_eq!(status, 200);
    let bots = response["bots"].as_array().unwrap();
    let bot = bots.iter().find(|b| b["id"] == "test-bot").unwrap();
    assert_eq!(bot["avatar"], "🎉");

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    let bots = roster["bots"].as_array().unwrap();
    let bot = bots.iter().find(|b| b["id"] == "test-bot").unwrap();
    assert_eq!(
        bot["avatar"], "🎉",
        "the avatar must hold on a fresh fetch too, not just the PATCH's own echo"
    );

    for clear in [json!(""), json!(null)] {
        let (status, response) = patch_route(
            &app,
            "/api/bots/test-bot/rail",
            &session,
            json!({ "avatar": clear }),
        )
        .await;
        assert_eq!(status, 200, "avatar={clear:?}");
        let bots = response["bots"].as_array().unwrap();
        let bot = bots.iter().find(|b| b["id"] == "test-bot").unwrap();
        assert!(
            bot["avatar"].is_null(),
            "avatar={clear:?} must clear it: {bot:?}"
        );
    }
}

/// Bite: a WHITESPACE-ONLY avatar clears to NULL rather than storing an
/// empty string.
///
/// This test exists because a mutation survived without it. Removing
/// `set_avatar`'s `.filter(|s| !s.is_empty())` stayed green against the
/// suite above, because the ROUTE already maps a literal `""` to `None`
/// before the store is reached - so the store's own guard looked equivalent.
/// It is not: `"   "` is a non-empty string to the route, reaches the store,
/// trims to nothing, and without the filter is written as `''` instead of
/// NULL. An empty-string avatar is not the same as no avatar - the client
/// renders a set avatar INSTEAD of the generated face
/// (`crates/client/src/avatar.rs:91`), so a bot would show a blank badge
/// where its face should be.
///
/// The lesson, not just the case: a surviving mutant is a defective test
/// until the two worlds are named and no observable differs. Here one did.
#[tokio::test]
async fn a_whitespace_only_avatar_clears_to_null_not_an_empty_string() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, _response) = patch_route(
        &app,
        "/api/bots/test-bot/rail",
        &session,
        json!({ "avatar": "🎉" }),
    )
    .await;
    assert_eq!(status, 200);

    for blank in ["   ", "\t", " \n "] {
        let (status, response) = patch_route(
            &app,
            "/api/bots/test-bot/rail",
            &session,
            json!({ "avatar": blank }),
        )
        .await;
        assert_eq!(status, 200, "avatar={blank:?}");
        let bots = response["bots"].as_array().unwrap();
        let bot = bots.iter().find(|b| b["id"] == "test-bot").unwrap();
        assert!(
            bot["avatar"].is_null(),
            "a whitespace-only avatar must be NULL, never an empty string: {bot:?}"
        );

        // Re-set it, so each blank in the loop starts from a set avatar
        // rather than passing because the previous iteration already cleared.
        patch_route(
            &app,
            "/api/bots/test-bot/rail",
            &session,
            json!({ "avatar": "🎉" }),
        )
        .await;
    }
}

/// Bite: a long avatar is cut to two CODE POINTS, checked with
/// `.chars().count()` rather than `.len()` (a byte count would pass even if
/// the route cut by bytes) - and a multi-byte emoji does not panic the
/// server, proving the cut is `chars().take(2)`, never a byte slice.
#[tokio::test]
async fn avatar_is_cut_to_two_code_points_and_multibyte_does_not_panic() {
    let db = open_db();
    seed_bot(&db, "ascii-bot", "Ascii Bot");
    seed_bot(&db, "emoji-bot", "Emoji Bot");
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) = patch_route(
        &app,
        "/api/bots/ascii-bot/rail",
        &session,
        json!({ "avatar": "robot" }),
    )
    .await;
    assert_eq!(status, 200);
    let bots = response["bots"].as_array().unwrap();
    let bot = bots.iter().find(|b| b["id"] == "ascii-bot").unwrap();
    let stored = bot["avatar"].as_str().unwrap();
    assert_eq!(
        stored.chars().count(),
        2,
        "a long avatar must be cut to two CODE POINTS: {stored:?}"
    );
    assert_eq!(stored, "ro");

    // Three multi-byte emoji (each > 1 byte in UTF-8) - a byte slice at
    // index 2 would panic mid-character; `chars().take(2)` must not.
    let (status, response) = patch_route(
        &app,
        "/api/bots/emoji-bot/rail",
        &session,
        json!({ "avatar": "🔥🔥🔥" }),
    )
    .await;
    assert_eq!(status, 200, "a multi-byte avatar must not panic the server");
    let bots = response["bots"].as_array().unwrap();
    let bot = bots.iter().find(|b| b["id"] == "emoji-bot").unwrap();
    let stored = bot["avatar"].as_str().unwrap();
    assert_eq!(
        stored.chars().count(),
        2,
        "a multi-byte avatar must also be cut to two CODE POINTS, not two bytes: {stored:?}"
    );
    assert_eq!(stored, "🔥🔥");
}

/// Bite: a known shape is stored as-is; an unknown one stores NULL and does
/// NOT 400, matching the TS `Object.hasOwn` guard exactly - "shape" is
/// present-but-invalid input, not a malformed request.
#[tokio::test]
async fn shape_known_value_stored_unknown_value_stores_null_not_400() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");
    let session = seed_session(&db);
    let app = app_for(db);

    let known = shared::faces::SHAPES[0].0;
    let (status, response) = patch_route(
        &app,
        "/api/bots/test-bot/rail",
        &session,
        json!({ "shape": known }),
    )
    .await;
    assert_eq!(status, 200);
    let bots = response["bots"].as_array().unwrap();
    let bot = bots.iter().find(|b| b["id"] == "test-bot").unwrap();
    assert_eq!(bot["shape"], known);

    let (status, response) = patch_route(
        &app,
        "/api/bots/test-bot/rail",
        &session,
        json!({ "shape": "not-a-real-shape" }),
    )
    .await;
    assert_eq!(
        status, 200,
        "an unknown shape must not be refused with a 400"
    );
    let bots = response["bots"].as_array().unwrap();
    let bot = bots.iter().find(|b| b["id"] == "test-bot").unwrap();
    assert!(
        bot["shape"].is_null(),
        "an unknown shape must store NULL: {bot:?}"
    );
}

/// Bite: an unknown bot id is 404 for both `avatar` and `shape`, same as
/// every other field this route carries - checked BEFORE the body is even
/// read, so a bogus id never gets far enough to look at either key.
#[tokio::test]
async fn rail_unknown_id_is_404_for_avatar_and_shape() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) = patch_route(
        &app,
        "/api/bots/does-not-exist/rail",
        &session,
        json!({ "avatar": "🎉", "shape": "circle" }),
    )
    .await;
    assert_eq!(status, 404);
    assert_eq!(response["error"], "no such bot");
}

/// Bite: `avatar` and `shape` apply together with `pinned` in the SAME
/// request - the whole reason this is one route rather than three: a
/// context menu that sets several things at once should be one call, not
/// three round trips that could each partially fail.
#[tokio::test]
async fn avatar_and_shape_apply_together_with_pinned_in_one_request() {
    let db = open_db();
    seed_bot(&db, "test-bot", "Test Bot");
    let session = seed_session(&db);
    let app = app_for(db);

    let known = shared::faces::SHAPES[1].0;
    let (status, response) = patch_route(
        &app,
        "/api/bots/test-bot/rail",
        &session,
        json!({ "avatar": "🚀", "shape": known, "pinned": true }),
    )
    .await;
    assert_eq!(status, 200);
    let bots = response["bots"].as_array().unwrap();
    let bot = bots.iter().find(|b| b["id"] == "test-bot").unwrap();
    assert_eq!(bot["avatar"], "🚀");
    assert_eq!(bot["shape"], known);
    assert_eq!(bot["pinned"], true);

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    let bots = roster["bots"].as_array().unwrap();
    let bot = bots.iter().find(|b| b["id"] == "test-bot").unwrap();
    assert_eq!(bot["avatar"], "🚀");
    assert_eq!(bot["shape"], known);
    assert_eq!(bot["pinned"], true);
}

/* --------------------------------------------------------------- EXPORT-01 */
//
// `GET /api/bots/:id/export` - the response is `text/markdown`, not JSON,
// so this block reads the raw response directly rather than through
// `get_route`/`patch_route` above (both of those parse every body as JSON
// and fold a parse failure into `json!({})` - exactly the wrong thing for
// a route whose body is deliberately not JSON).

async fn export_route(app: &Router, path: &str, session: &str) -> (u16, HeaderMap, String) {
    let request = Request::get(path)
        .header("cookie", session)
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).expect("export body must be valid utf-8");
    (status, headers, text)
}

/// Parses the export route's own frontmatter shape back apart - finds the
/// `---` delimiters and JSON-decodes each value - rather than eyeballing
/// the raw string, so a quote or an embedded newline in a value is proven
/// to round-trip rather than just "the substring looks present". Returns
/// the decoded key/value map and the body text after the blank line that
/// follows the closing `---`.
fn split_frontmatter(text: &str) -> (std::collections::HashMap<String, String>, String) {
    let mut lines = text.lines();
    assert_eq!(
        lines.next(),
        Some("---"),
        "export must open with a --- line: {text:?}"
    );
    let mut map = std::collections::HashMap::new();
    for line in &mut lines {
        if line == "---" {
            break;
        }
        let (key, value) = line
            .split_once(": ")
            .unwrap_or_else(|| panic!("frontmatter line must be key: value: {line:?}"));
        let decoded: String = serde_json::from_str(value)
            .unwrap_or_else(|e| panic!("frontmatter value must be JSON-encoded: {value:?}: {e}"));
        map.insert(key.to_string(), decoded);
    }
    let rest: Vec<&str> = lines.collect();
    // The export route always writes a blank line right after the closing
    // `---` (the ticket's own "then a blank line, then the instructions
    // verbatim") - `rest[0]` is that blank line, not part of the body.
    assert_eq!(
        rest.first(),
        Some(&""),
        "a blank line must separate frontmatter from instructions: {text:?}"
    );
    (map, rest[1..].join("\n"))
}

/// Bite: a bot with a model pin exports `name`, `description` and `model`
/// in the frontmatter, then the instructions verbatim - and both headers
/// this route promises (`Content-Type`, `Content-Disposition` with the
/// filename) are present and exactly right. Covers the ticket's third
/// mutation target (the `Content-Disposition` header dropped) directly:
/// dropping it fails this test's header assertion, not just a vibe check.
#[tokio::test]
async fn export_with_a_model_pin_includes_name_description_model_then_instructions() {
    let db = open_db();
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at, permissions)
             VALUES ('test-bot', 'Test Bot', 'helps with tests', 'Be helpful.',
                     'anthropic/claude-sonnet-5', '2026-01-01T00:00:00Z', '{}')",
            [],
        )
        .expect("seed bot with a model pin");
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, headers, text) = export_route(&app, "/api/bots/test-bot/export", &session).await;
    assert_eq!(status, 200);
    assert_eq!(
        headers.get("content-type").unwrap(),
        "text/markdown; charset=utf-8"
    );
    assert_eq!(
        headers.get("content-disposition").unwrap(),
        "attachment; filename=\"test-bot.md\""
    );

    let (frontmatter, body) = split_frontmatter(&text);
    assert_eq!(frontmatter.get("name").unwrap(), "Test Bot");
    assert_eq!(frontmatter.get("description").unwrap(), "helps with tests");
    assert_eq!(
        frontmatter.get("model").unwrap(),
        "anthropic/claude-sonnet-5"
    );
    assert_eq!(body, "Be helpful.");
}

/// Bite: an unpinned bot's export carries NO `model:` line at all - not an
/// empty one, not a `null` one, absent - checked both through the parsed
/// frontmatter map (no `model` key) and directly against the raw text (no
/// line starting `model:`), so a route that emitted `model: null` would
/// fail this even if `split_frontmatter` were ever loosened to tolerate a
/// null value. Covers the ticket's first mutation target (the `model` line
/// emitted unconditionally).
#[tokio::test]
async fn export_with_no_model_pin_has_no_model_line_at_all() {
    let db = open_db();
    seed_bot(&db, "unpinned-bot", "Unpinned Bot");
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, _headers, text) =
        export_route(&app, "/api/bots/unpinned-bot/export", &session).await;
    assert_eq!(status, 200);

    let (frontmatter, _body) = split_frontmatter(&text);
    assert!(
        !frontmatter.contains_key("model"),
        "an unpinned bot's frontmatter must carry no model key at all: {text:?}"
    );
    assert!(
        !text.lines().any(|l| l.starts_with("model:")),
        "raw text must not contain a model: line either: {text:?}"
    );
}

/// Bite: a name AND a purpose containing both a quote and an embedded
/// newline round-trip correctly through the JSON-encoded frontmatter -
/// parsed back out by `split_frontmatter`'s real JSON decode, not eyeballed.
/// Covers the ticket's second mutation target (the JSON encoding of values
/// dropped, so a quote breaks the frontmatter): dropping the encoding would
/// either panic `split_frontmatter`'s `serde_json::from_str` or, if the
/// dropped quote itself corrupts the `---` structure, fail the very first
/// `assert_eq!(lines.next(), Some("---"))` check.
#[tokio::test]
async fn a_name_and_purpose_with_a_quote_and_a_newline_round_trip_through_the_frontmatter() {
    let db = open_db();
    let tricky_name = "Weird \"Bot\"\nName";
    let tricky_purpose = "a \"purpose\"\nwith an embedded line";
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at, permissions)
             VALUES ('tricky-bot', ?1, ?2, 'instructions here', NULL, '2026-01-01T00:00:00Z', '{}')",
            rusqlite::params![tricky_name, tricky_purpose],
        )
        .expect("seed tricky bot");
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, _headers, text) =
        export_route(&app, "/api/bots/tricky-bot/export", &session).await;
    assert_eq!(status, 200);

    let (frontmatter, body) = split_frontmatter(&text);
    assert_eq!(frontmatter.get("name").unwrap(), tricky_name);
    assert_eq!(frontmatter.get("description").unwrap(), tricky_purpose);
    assert_eq!(body, "instructions here");
}

/// Bite: an unknown bot id is 404 JSON (`{"error": "no such bot"}`), not a
/// markdown body - the same shape every other route in this file answers
/// an unknown id with, so a client's generic error handling still works
/// for this one non-JSON-on-success route.
#[tokio::test]
async fn export_unknown_id_is_404_json_not_markdown() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, headers, text) =
        export_route(&app, "/api/bots/does-not-exist/export", &session).await;
    assert_eq!(status, 404);
    assert!(
        headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/json")),
        "a 404 must be JSON, not markdown: {headers:?}"
    );
    let parsed: Value = serde_json::from_str(&text).expect("404 body must be JSON");
    assert_eq!(parsed["error"], "no such bot");
}

/* ------------------------------------------------------------------ DUP-01 */

/// Bite: the copy carries `purpose`/`instructions`/`model`, and the SOURCE
/// is left completely unchanged - checked by re-reading BOTH bots off a
/// fresh `/api/roster` fetch afterward, not just the duplicate response's
/// own echo, so a mutation that wrote the copy's fields onto the source row
/// by mistake (or vice versa) cannot pass.
#[tokio::test]
async fn duplicate_carries_purpose_instructions_and_model_and_leaves_the_source_untouched() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_with_catalog(db, fixture_catalog());

    let (status, created) = post_route(
        &app,
        "/api/bots",
        &session,
        json!({
            "name": "Devon",
            "purpose": "Watches the deploy pipeline",
            "instructions": "Be terse. Flag red builds immediately.",
            "model": "anthropic/claude-sonnet-5",
        }),
    )
    .await;
    assert_eq!(status, 201);
    let source_id = created["bot"]["id"].as_str().unwrap().to_string();

    let (status, response) = post_route(
        &app,
        &format!("/api/bots/{source_id}/duplicate"),
        &session,
        json!({}),
    )
    .await;
    assert_eq!(status, 201, "{response:?}");
    let copy = &response["bot"];
    assert_eq!(copy["purpose"], "Watches the deploy pipeline");
    assert_eq!(
        copy["instructions"],
        "Be terse. Flag red builds immediately."
    );
    assert_eq!(copy["model"], "anthropic/claude-sonnet-5");
    assert_ne!(copy["id"], source_id, "the copy must have its own id");

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    let bots = roster["bots"].as_array().unwrap();
    let source = bots
        .iter()
        .find(|b| b["id"] == source_id)
        .expect("source bot must still be in the roster");
    assert_eq!(source["purpose"], "Watches the deploy pipeline");
    assert_eq!(
        source["instructions"],
        "Be terse. Flag red builds immediately."
    );
    assert_eq!(source["model"], "anthropic/claude-sonnet-5");
    assert_eq!(
        source["name"], "Devon",
        "duplicating must not rename the source"
    );
}

/// Bite: the naming rule. The TS just appends `" copy"` and stops, so
/// duplicating the same bot twice would show two IDENTICAL names on the
/// rail; this numbers past the collision instead - `"Devon copy"`, then
/// `"Devon copy 2"` - with different ids, checked against the roster (not
/// just each duplicate response) so a mutation that only fixed the echoed
/// response cannot pass.
#[tokio::test]
async fn duplicate_twice_numbers_the_name_past_the_first_collision() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, created) =
        post_route(&app, "/api/bots", &session, json!({ "name": "Devon" })).await;
    assert_eq!(status, 201);
    let source_id = created["bot"]["id"].as_str().unwrap().to_string();

    let (status, first) = post_route(
        &app,
        &format!("/api/bots/{source_id}/duplicate"),
        &session,
        json!({}),
    )
    .await;
    assert_eq!(status, 201);
    assert_eq!(first["bot"]["name"], "Devon copy");

    let (status, second) = post_route(
        &app,
        &format!("/api/bots/{source_id}/duplicate"),
        &session,
        json!({}),
    )
    .await;
    assert_eq!(status, 201);
    assert_eq!(second["bot"]["name"], "Devon copy 2");
    assert_ne!(
        first["bot"]["id"], second["bot"]["id"],
        "two duplicates must be two distinct rows"
    );

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    let names: Vec<&str> = roster["bots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"Devon"));
    assert!(names.contains(&"Devon copy"));
    assert!(names.contains(&"Devon copy 2"));
}

/// Bite: an unknown id is 404 with the same body every other unknown-id
/// route in this file answers with, and nothing is created - checked
/// against the roster afterward (still empty), not just the status code.
#[tokio::test]
async fn duplicate_unknown_id_is_404_and_creates_nothing() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) = post_route(
        &app,
        "/api/bots/does-not-exist/duplicate",
        &session,
        json!({}),
    )
    .await;
    assert_eq!(status, 404);
    assert_eq!(response["error"], "no such bot");

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    assert!(roster["bots"].as_array().unwrap().is_empty());
}

/// Bite: a routine is copied - same prompt and schedule, but `active` is
/// FALSE on the copy even though the source routine was left ACTIVE with a
/// real `nextRunAt` before duplicating (so a mutation that carried those two
/// fields across would be caught, not just a mutation that never touches
/// them), the copy gets its own routine id, and the bot-level `hasRoutine`
/// flag is set on the copy. This is the guard the coordinator is mutating
/// directly: a duplicate that starts firing on its own is the worst outcome
/// this feature has.
#[tokio::test]
async fn duplicate_copies_a_routine_inactive_with_its_own_id() {
    let db = open_db();
    seed_bot(&db, "source-bot", "Source Bot");
    let routine_id = store::create_routine(
        &db,
        "source-bot",
        "Daily Check",
        "Check the overnight logs",
        "0 9 * * *".to_string(),
        None,
        None,
        Some("prompt"),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("create routine");
    // Left ACTIVE with a real next run before duplicating - the copy must
    // NOT inherit either of these, so the test actually exercises the
    // guard rather than trivially passing because the source was already
    // inactive.
    store::set_routine_active(
        &db,
        &routine_id,
        true,
        Some("2026-06-01T09:00:00Z".to_string()),
    )
    .expect("activate source routine");

    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) =
        post_route(&app, "/api/bots/source-bot/duplicate", &session, json!({})).await;
    assert_eq!(status, 201, "{response:?}");
    let copy_id = response["bot"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        response["bot"]["hasRoutine"], true,
        "the copy must carry hasRoutine once a routine was copied onto it"
    );

    let (_status, copy_routines) =
        get_route(&app, &format!("/api/routines?bot={copy_id}"), &session).await;
    let copy_list = copy_routines["routines"].as_array().unwrap();
    assert_eq!(copy_list.len(), 1, "exactly one routine must be copied");
    let copied = &copy_list[0];
    assert_eq!(copied["prompt"], "Check the overnight logs");
    assert_ne!(
        copied["id"], routine_id,
        "the copy's routine must have its own id"
    );
    assert_eq!(
        copied["active"], false,
        "a copied routine must never start active"
    );
    assert!(
        copied["nextRunAt"].is_null(),
        "a copied routine must carry no next run time: {copied:?}"
    );

    let (_status, source_routines) =
        get_route(&app, "/api/routines?bot=source-bot", &session).await;
    let source_list = source_routines["routines"].as_array().unwrap();
    assert_eq!(
        source_list[0]["active"], true,
        "the SOURCE routine must be untouched"
    );
}

/// Bite: `hook_secret` is NOT carried onto the copy - a webhook secret is a
/// credential for one routine, not something a duplicate should inherit.
/// Checked through `hasHook` (the only wire-visible reflection of
/// `hook_secret`, per `store::routines::routine_from_row`'s own
/// `has_hook: hook_secret.is_some()`): the source keeps `hasHook: true`
/// after minting a real secret, and the copy's own routine shows
/// `hasHook: false`.
#[tokio::test]
async fn duplicate_does_not_carry_the_hook_secret() {
    let db = open_db();
    seed_bot(&db, "source-bot", "Source Bot");
    let routine_id = store::create_routine(
        &db,
        "source-bot",
        "On Push",
        "React to the push",
        "".to_string(),
        None,
        None,
        Some("prompt"),
        None,
        None,
        Some("github"),
        None,
        None,
        None,
    )
    .expect("create routine");
    store::mint_routine_hook(&db, &routine_id)
        .expect("mint hook")
        .expect("routine must exist");

    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) =
        post_route(&app, "/api/bots/source-bot/duplicate", &session, json!({})).await;
    assert_eq!(status, 201, "{response:?}");
    let copy_id = response["bot"]["id"].as_str().unwrap().to_string();

    let (_status, copy_routines) =
        get_route(&app, &format!("/api/routines?bot={copy_id}"), &session).await;
    let copied = &copy_routines["routines"].as_array().unwrap()[0];
    assert_eq!(
        copied["hasHook"], false,
        "the copy must not carry the source's webhook secret: {copied:?}"
    );

    let (_status, source_routines) =
        get_route(&app, "/api/routines?bot=source-bot", &session).await;
    let source = &source_routines["routines"].as_array().unwrap()[0];
    assert_eq!(
        source["hasHook"], true,
        "the SOURCE routine must keep its own hook secret"
    );
}

/// Bite: `sectionId`/`avatar`/`shape`/`effort` (set through the same real
/// routes a user would use) and `voice` (no route writes this column yet in
/// this port - set directly, see this test's own comment) are ALL carried
/// onto the copy - the deliberate divergence from the TS this ticket calls
/// for, so a copy does not land in Unassigned wearing a different face.
#[tokio::test]
async fn duplicate_carries_section_avatar_shape_effort_and_voice() {
    let db = open_db();
    seed_bot(&db, "source-bot", "Source Bot");
    // RAIL-01/03 wired PATCH .../rail for sectionId/avatar/shape and
    // PATCH .../{id} for effort below, but nothing in this port ever
    // writes `voice` (S11's device-voice field is not built yet -
    // `crates/store/src/bots.rs::BotDraft`'s own doc says so) - set
    // directly, before the db moves into `app_for` below, so this test
    // still proves the COLUMN carries, ready for whenever a route does
    // write it.
    db.conn()
        .execute(
            "UPDATE bots SET voice = ?1 WHERE id = 'source-bot'",
            rusqlite::params!["alloy"],
        )
        .expect("seed voice directly");

    let session = seed_session(&db);
    let app = app_for(db);

    let (status, section) =
        post_route(&app, "/api/sections", &session, json!({ "name": "SHOOT" })).await;
    assert_eq!(status, 201);
    let section_id = section["section"]["id"].as_str().unwrap().to_string();

    let (status, _response) = patch_route(
        &app,
        "/api/bots/source-bot/rail",
        &session,
        json!({ "sectionId": section_id, "avatar": "🚀", "shape": shared::faces::SHAPES[0].0 }),
    )
    .await;
    assert_eq!(status, 200);

    let (status, _response) = patch_route(
        &app,
        "/api/bots/source-bot",
        &session,
        json!({ "effort": "high" }),
    )
    .await;
    assert_eq!(status, 200);

    let (status, response) =
        post_route(&app, "/api/bots/source-bot/duplicate", &session, json!({})).await;
    assert_eq!(status, 201, "{response:?}");
    let copy = &response["bot"];
    assert_eq!(copy["sectionId"], section_id);
    assert_eq!(copy["avatar"], "🚀");
    assert_eq!(copy["shape"], shared::faces::SHAPES[0].0);
    assert_eq!(copy["effort"], "high");
    assert_eq!(copy["voice"], "alloy");
}

/// Bite: `pinned`/`hidden` are NOT carried - pin and hide the source
/// through the real rail route, duplicate it, and assert the copy is
/// neither. A duplicate appearing pinned above everything, or invisible
/// from the moment it exists, would be a surprise, not a copy.
#[tokio::test]
async fn duplicate_does_not_carry_pinned_or_hidden() {
    let db = open_db();
    seed_bot(&db, "source-bot", "Source Bot");
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, _response) = patch_route(
        &app,
        "/api/bots/source-bot/rail",
        &session,
        json!({ "pinned": true, "hidden": true }),
    )
    .await;
    assert_eq!(status, 200);

    let (status, response) =
        post_route(&app, "/api/bots/source-bot/duplicate", &session, json!({})).await;
    assert_eq!(status, 201, "{response:?}");
    let copy = &response["bot"];
    assert_eq!(
        copy["pinned"], false,
        "a duplicate must not be pinned: {copy:?}"
    );
    assert_eq!(
        copy["hidden"], false,
        "a duplicate must not be hidden: {copy:?}"
    );
}

/// Bite: the copy's memory is empty although the source has an entry - a
/// duplicate remembers nothing the source ever learned, checked by reading
/// the copy's own `/memory` route.
#[tokio::test]
async fn duplicate_starts_with_empty_memory() {
    let db = open_db();
    seed_bot(&db, "source-bot", "Source Bot");
    store::remember(
        &db,
        "source-bot",
        "The deploy key rotates every 90 days.",
        "josh",
    )
    .expect("seed source memory");

    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) =
        post_route(&app, "/api/bots/source-bot/duplicate", &session, json!({})).await;
    assert_eq!(status, 201, "{response:?}");
    let copy_id = response["bot"]["id"].as_str().unwrap().to_string();

    let (_status, memory) = get_route(&app, &format!("/api/bots/{copy_id}/memory"), &session).await;
    assert_eq!(memory["core"], "");
    assert!(
        memory["log"].as_array().unwrap().is_empty(),
        "the copy must start with no memory log entries: {memory:?}"
    );

    let (_status, source_memory) = get_route(&app, "/api/bots/source-bot/memory", &session).await;
    assert!(
        !source_memory["log"].as_array().unwrap().is_empty(),
        "the SOURCE must keep its own memory"
    );
}
