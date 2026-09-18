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
