//! S2-06: settings routes - theme, timezone, rules, models. Port of
//! `projects/bullpen-night/src/server/app.ts:2902-3080,4374-4400`.

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use model::catalog::FixtureCatalog;
use serde_json::{Value, json};
use server::AppState;
use std::sync::Arc;
use store::Db;
use tower::ServiceExt;

mod common;
use common::seed_session;

fn open_db() -> Db {
    // F1: no `ensure_routing_tables` call here on purpose - `AppState::new`
    // (via `app_for` below) has to be the thing that creates `routing_log`,
    // or this test would pass for the same reason the review flagged: the
    // harness creating the table the server never does.
    Db::open(":memory:").expect("open :memory: db")
}

fn app_for(db: Db) -> Router {
    let state = AppState::new(db);
    server::build_app(state)
}

fn app_with_catalog(db: Db, catalog: Arc<dyn model::catalog::Catalog>) -> Router {
    let state = AppState::with_catalog(db, catalog);
    server::build_app(state)
}

// Fixture catalog for models tests
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
    Arc::new(FixtureCatalog::from_json(json).expect("parse fixture"))
}

// Helpers for making requests
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

async fn put_route(app: &Router, path: &str, session: &str, body: Value) -> (u16, Value) {
    let body_json = body.to_string();
    let request = Request::put(path)
        .header("cookie", session)
        .header("content-type", "application/json")
        .body(Body::from(body_json))
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
async fn test_theme_get_put_roundtrip() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    // GET returns default theme
    let (status, response) = get_route(&app, "/api/theme", &session).await;
    assert_eq!(status, 200);
    assert_eq!(
        response["theme"]["accent"], "seam",
        "default theme should have seam accent"
    );

    // PUT sets theme
    let theme = json!({
        "accent": "ember",
        "mine": "neutral",
        "ground": "ink",
        "radius": "sharp"
    });
    let (status, response) =
        put_route(&app, "/api/theme", &session, json!({ "theme": theme })).await;
    assert_eq!(status, 200);
    assert_eq!(response["theme"]["accent"], "ember");
    assert_eq!(response["theme"]["mine"], "neutral");

    // GET returns the set theme
    let (status, response) = get_route(&app, "/api/theme", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["theme"]["accent"], "ember");
    assert_eq!(response["theme"]["mine"], "neutral");
}

#[tokio::test]
async fn test_timezone_get_put_roundtrip() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    // GET returns "auto" by default
    let (status, response) = get_route(&app, "/api/timezone", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["timezone"], "auto");

    // PUT sets timezone
    let (status, response) = put_route(
        &app,
        "/api/timezone",
        &session,
        json!({ "timezone": "America/Chicago" }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["timezone"], "America/Chicago");

    // GET returns the set timezone
    let (status, response) = get_route(&app, "/api/timezone", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["timezone"], "America/Chicago");
}

#[tokio::test]
async fn test_rules_get_put_roundtrip() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    // GET returns the house rules
    let (status, response) = get_route(&app, "/api/rules", &session).await;
    assert_eq!(status, 200);
    assert!(response["rules"].is_string());

    // PUT sets new rules
    let (status, response) = put_route(
        &app,
        "/api/rules",
        &session,
        json!({ "rules": "Test rules" }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["rules"], "Test rules");

    // GET returns the set rules
    let (status, response) = get_route(&app, "/api/rules", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["rules"], "Test rules");
}

#[tokio::test]
async fn test_default_model_get_put_roundtrip() {
    let db = open_db();
    let session = seed_session(&db);
    // F2/D9: the PUT now judges the pin against the catalogue (as a ROUTINE
    // pin, `judge_pin(..., true)`), so it needs a catalogue that actually
    // lists the model - an empty fixture would refuse every pin with
    // "OpenRouter does not list this model."
    let app = app_with_catalog(db, fixture_catalog());

    // GET returns default model (cheap default)
    let (status, response) = get_route(&app, "/api/default-model", &session).await;
    assert_eq!(status, 200);
    assert!(response["model"].is_string());

    // PUT sets model
    let (status, response) = put_route(
        &app,
        "/api/default-model",
        &session,
        json!({ "model": "anthropic/claude-sonnet-5" }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["model"], "anthropic/claude-sonnet-5");

    // GET returns the set model
    let (status, response) = get_route(&app, "/api/default-model", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["model"], "anthropic/claude-sonnet-5");
}

/// F2/D9: a `:batch` slug is refused for the same reason `judge_pin` refuses
/// any other pin with it - this is what every unpinned bot and every timer
/// run falls back to, so a `:batch` id here would 404 unattended.
#[tokio::test]
async fn test_default_model_refuses_batch_suffix() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_with_catalog(db, fixture_catalog());

    let (status, response) = put_route(
        &app,
        "/api/default-model",
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
}

#[tokio::test]
async fn test_default_model_refuses_premium() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    // PUT with a premium model (contains "fable") should be refused
    let (status, response) = put_route(
        &app,
        "/api/default-model",
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
}

#[tokio::test]
async fn test_mid_model_get_put_roundtrip() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    // GET returns default mid model
    let (status, response) = get_route(&app, "/api/mid-model", &session).await;
    assert_eq!(status, 200);
    assert!(response["model"].is_string());

    // PUT sets model
    let (status, response) = put_route(
        &app,
        "/api/mid-model",
        &session,
        json!({ "model": "anthropic/claude-sonnet-5" }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["model"], "anthropic/claude-sonnet-5");
}

#[tokio::test]
async fn test_tier1_models_get_put_roundtrip() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    // GET returns tier1 models
    let (status, response) = get_route(&app, "/api/tier1-models", &session).await;
    assert_eq!(status, 200);
    assert!(response["models"]["code"].is_string());
    assert!(response["models"]["reason"].is_string());
    assert!(response["models"]["vision"].is_string());
    assert_eq!(response["kinds"], json!(["code", "reason", "vision"]));

    // PUT sets a code tier1 model
    let (status, response) = put_route(
        &app,
        "/api/tier1-models",
        &session,
        json!({
            "kind": "code",
            "model": "anthropic/claude-sonnet-5"
        }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["kind"], "code");
    assert_eq!(response["model"], "anthropic/claude-sonnet-5");

    // GET returns the updated model
    let (status, response) = get_route(&app, "/api/tier1-models", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["models"]["code"], "anthropic/claude-sonnet-5");
}

#[tokio::test]
async fn test_premium_model_get_put_roundtrip() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    // GET returns default premium model
    let (status, response) = get_route(&app, "/api/premium-model", &session).await;
    assert_eq!(status, 200);
    assert!(response["model"].is_string());

    // PUT sets model
    let (status, response) = put_route(
        &app,
        "/api/premium-model",
        &session,
        json!({ "model": "anthropic/claude-fable-5.1" }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["model"], "anthropic/claude-fable-5.1");
}

/// F12/T7: curates the mainstream list down to ONE of the two fixture
/// models, so the default view and `all=1` actually differ - before the
/// fix, `mainstream_ids` was every catalogue model, so this test could not
/// have told the difference (T7's own bite: deleting the scoping branch
/// stayed green).
#[tokio::test]
async fn test_models_search() {
    let db = open_db();
    let session = seed_session(&db);
    db.settings_set("models.mainstream", r#"["anthropic/claude-sonnet-5"]"#)
        .expect("seed models.mainstream");
    let catalog = fixture_catalog();
    let app = app_with_catalog(db, catalog);

    // Default view (mainstream-scoped): one model.
    let (status, response) = get_route(&app, "/api/models", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["models"].as_array().unwrap().len(), 1);
    assert_eq!(response["models"][0]["id"], "anthropic/claude-sonnet-5");
    assert_eq!(response["total"], 2);
    assert_eq!(response["mainstreamTotal"], 1);

    // `?all=1`: both models, `mainstreamTotal` unchanged.
    let (status, response) = get_route(&app, "/api/models?all=1", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["models"].as_array().unwrap().len(), 2);
    assert_eq!(response["total"], 2);
    assert_eq!(response["mainstreamTotal"], 1);

    // Search by model name, scoped to `all=1` - the mainstream-only default
    // view cannot reach the non-mainstream Fable match at all.
    let (status, response) = get_route(&app, "/api/models?all=1&q=sonnet", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["models"].as_array().unwrap().len(), 1);
    assert_eq!(response["models"][0]["id"], "anthropic/claude-sonnet-5");

    // Search by model id, also non-mainstream.
    let (status, response) = get_route(&app, "/api/models?all=1&q=fable", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["models"].as_array().unwrap().len(), 1);
    assert_eq!(response["models"][0]["name"], "Claude Fable 5.1");
}

/// F2/F12: with no `models.mainstream` row at all, the default view falls
/// back to the curated fifteen - a model the catalogue lists but that is
/// NOT on that default list stays out of the default view while `all=1`
/// still shows it. (The two `fixture_catalog()` ids are deliberately both
/// on the default fifteen, which is what `test_models_search` exercises;
/// this test needs one that is not.)
#[tokio::test]
async fn test_models_search_default_mainstream_excludes_unlisted_fixture_models() {
    let db = open_db();
    let session = seed_session(&db);
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
            "id": "some-lab/off-the-curated-list",
            "name": "Off The Curated List",
            "inPerM": 1.0,
            "outPerM": 2.0,
            "contextLength": 32000,
            "supportsTools": true,
            "supportsImages": false,
            "supportsReasoning": false,
            "providerCount": 3,
            "supportsCaching": false
        }
    ]"#;
    let catalog: Arc<dyn model::catalog::Catalog> =
        Arc::new(FixtureCatalog::from_json(json).expect("parse fixture"));
    let app = app_with_catalog(db, catalog);

    let (status, response) = get_route(&app, "/api/models", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["models"].as_array().unwrap().len(), 1);
    assert_eq!(response["models"][0]["id"], "anthropic/claude-sonnet-5");
    assert_eq!(response["mainstreamTotal"], 1);
    assert_eq!(response["total"], 2);

    let (status, response) = get_route(&app, "/api/models?all=1", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["models"].as_array().unwrap().len(), 2);
}

/// F13: `GET /api/rules` answers `{rules, fallback}` - the fallback is the
/// default house rules text, present even once Josh has written his own.
#[tokio::test]
async fn test_rules_get_carries_fallback() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) = get_route(&app, "/api/rules", &session).await;
    assert_eq!(status, 200);
    assert!(response["fallback"].is_string());
    assert!(
        response["fallback"]
            .as_str()
            .unwrap()
            .contains("Never claim what you have not verified")
    );
    // The fallback is present even after Josh writes his own rules.
    let (_status, _response) = put_route(
        &app,
        "/api/rules",
        &session,
        json!({ "rules": "Custom rules" }),
    )
    .await;
    let (status, response) = get_route(&app, "/api/rules", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["rules"], "Custom rules");
    assert!(response["fallback"].is_string());
}

/// `GET/PUT /api/models/mainstream` - the curated list route mounted for
/// F2/F12/D9. Round-trips a valid write and refuses a `:batch` id.
#[tokio::test]
async fn test_models_mainstream_get_put() {
    let db = open_db();
    let session = seed_session(&db);
    let catalog = fixture_catalog();
    let app = app_with_catalog(db, catalog);

    // GET with nothing configured falls back to the default fifteen.
    let (status, response) = get_route(&app, "/api/models/mainstream", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["ids"].as_array().unwrap().len(), 15);

    // PUT a curated list drawn from the fixture catalogue.
    let (status, response) = put_route(
        &app,
        "/api/models/mainstream",
        &session,
        json!({ "ids": ["anthropic/claude-sonnet-5", "anthropic/claude-fable-5.1"] }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["ids"].as_array().unwrap().len(), 2);
    assert_eq!(response["models"][0]["id"], "anthropic/claude-sonnet-5");
    assert_eq!(response["models"][0]["label"], "Sonnet 5");

    // GET reflects the write.
    let (status, response) = get_route(&app, "/api/models/mainstream", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["ids"].as_array().unwrap().len(), 2);

    // A `:batch` id is refused and does not overwrite the saved list.
    let (status, response) = put_route(
        &app,
        "/api/models/mainstream",
        &session,
        json!({ "ids": ["anthropic/claude-sonnet-5:batch"] }),
    )
    .await;
    assert_eq!(status, 400);
    assert!(
        response["error"]
            .as_str()
            .unwrap_or("")
            .contains("batch-only")
    );
    let (_status, response) = get_route(&app, "/api/models/mainstream", &session).await;
    assert_eq!(response["ids"].as_array().unwrap().len(), 2);

    // An id the catalogue does not list is refused.
    let (status, response) = put_route(
        &app,
        "/api/models/mainstream",
        &session,
        json!({ "ids": ["nobody/no-such-model"] }),
    )
    .await;
    assert_eq!(status, 400);
    assert!(
        response["error"]
            .as_str()
            .unwrap_or("")
            .contains("does not list")
    );
}

#[tokio::test]
async fn test_routing_get_put_toggle() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    // GET returns routing settings
    let (status, response) = get_route(&app, "/api/routing", &session).await;
    assert_eq!(status, 200);
    assert!(response["enabled"].is_boolean());
    assert!(response["text"].is_string());
    assert!(response["log"].is_array());

    // PUT to toggle enabled
    let (status, _response) =
        put_route(&app, "/api/routing", &session, json!({ "enabled": false })).await;
    assert_eq!(status, 200);

    // GET should show toggled value
    let (status, response) = get_route(&app, "/api/routing", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["enabled"], false);
}

#[tokio::test]
async fn test_second_opinion_get_put_roundtrip() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    // F9: default is TRUE on a fresh db - TS's "the switch here is the OFF
    // switch" (`second-opinion.ts:65`), not Rust's earlier `false`.
    let (status, response) = get_route(&app, "/api/second-opinion", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["enabled"], true);

    // PUT to disable
    let (status, response) = put_route(
        &app,
        "/api/second-opinion",
        &session,
        json!({ "enabled": false }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["enabled"], false);

    // GET should show disabled
    let (status, response) = get_route(&app, "/api/second-opinion", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["enabled"], false);

    // PUT back to enabled, round-trips too.
    let (status, response) = put_route(
        &app,
        "/api/second-opinion",
        &session,
        json!({ "enabled": true }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["enabled"], true);
}

/// F9: `app.ts:4396` answers `400 {"error":"enabled must be a boolean"}` on
/// a non-boolean body - a missing/non-boolean `enabled` must not silently
/// store `false`, which is what the pre-fix Rust route did.
#[tokio::test]
async fn test_second_opinion_put_rejects_non_boolean() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) = put_route(
        &app,
        "/api/second-opinion",
        &session,
        json!({ "enabled": "yes" }),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(response["error"], "enabled must be a boolean");

    let (status, response) = put_route(&app, "/api/second-opinion", &session, json!({})).await;
    assert_eq!(status, 400);
    assert_eq!(response["error"], "enabled must be a boolean");

    // The default must not have been disturbed by the refused writes.
    let (_status, response) = get_route(&app, "/api/second-opinion", &session).await;
    assert_eq!(response["enabled"], true);
}
