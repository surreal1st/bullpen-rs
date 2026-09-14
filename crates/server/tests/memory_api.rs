//! Tests for memory API routes. Port of `test/memory.test.ts:397-638`.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::seed_session;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use server::AppState;
use server::build_app;
use store::Db;
use tower::ServiceExt;

async fn get(app: &axum::Router, uri: &str, cookie: &str) -> (StatusCode, Value) {
    let req = Request::get(uri)
        .header("cookie", cookie)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("response body is JSON")
    };
    (status, body)
}

async fn post(
    app: &axum::Router,
    uri: &str,
    body_value: Value,
    cookie: &str,
) -> (StatusCode, Value) {
    let body_bytes = serde_json::to_string(&body_value).unwrap();
    let req = Request::post(uri)
        .header("cookie", cookie)
        .header("content-type", "application/json")
        .body(Body::from(body_bytes))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("response body is JSON")
    };
    (status, body)
}

async fn put(
    app: &axum::Router,
    uri: &str,
    body_value: Value,
    cookie: &str,
) -> (StatusCode, Value) {
    let body_bytes = serde_json::to_string(&body_value).unwrap();
    let req = Request::put(uri)
        .header("cookie", cookie)
        .header("content-type", "application/json")
        .body(Body::from(body_bytes))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("response body is JSON")
    };
    (status, body)
}

async fn delete(app: &axum::Router, uri: &str, cookie: &str) -> (StatusCode, Value) {
    let req = Request::delete(uri)
        .header("cookie", cookie)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("response body is JSON")
    };
    (status, body)
}

#[tokio::test]
async fn saves_a_core_and_reports_its_size_against_the_budget() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let cookie = seed_session(&db);

    // Create a test bot
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params!["arthur", "Arthur", "", "", None::<String>, "2026-01-01T00:00:00Z"],
        )
        .expect("seed bot");

    let app = build_app(AppState::new(db));

    // PUT a core
    let (status, _) = put(
        &app,
        "/api/bots/arthur/memory/core",
        json!({ "core": "Josh is the owner." }),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // GET the memory view
    let (status, view) = get(&app, "/api/bots/arthur/memory", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["core"], "Josh is the owner.");
    assert!(view["tokens"].as_i64().unwrap() > 0);
    assert_eq!(view["overBudget"], false);
}

#[tokio::test]
async fn flags_a_core_that_has_grown_past_its_budget() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let cookie = seed_session(&db);

    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params!["arthur", "Arthur", "", "", None::<String>, "2026-01-01T00:00:00Z"],
        )
        .expect("seed bot");

    let app = build_app(AppState::new(db));

    // PUT a huge core (4000 chars = ~1000 tokens, over 500 budget)
    let huge_core = "x".repeat(4000);
    let (status, _) = put(
        &app,
        "/api/bots/arthur/memory/core",
        json!({ "core": huge_core }),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // GET the memory view
    let (status, view) = get(&app, "/api/bots/arthur/memory", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["overBudget"], true);
}

#[tokio::test]
async fn adds_and_forgets_a_log_entry() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let cookie = seed_session(&db);

    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params!["arthur", "Arthur", "", "", None::<String>, "2026-01-01T00:00:00Z"],
        )
        .expect("seed bot");

    let app = build_app(AppState::new(db));

    // POST a memory entry
    let (status, created) = post(
        &app,
        "/api/bots/arthur/memory",
        json!({ "content": "Josh prefers plain language." }),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let entry_id = created["entry"]["id"].as_str().unwrap().to_string();
    assert_eq!(created["entry"]["source"], "josh");

    // GET and verify the entry is there
    let (status, view) = get(&app, "/api/bots/arthur/memory", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["log"].as_array().unwrap().len(), 1);

    // DELETE the entry
    let (status, _) = delete(
        &app,
        &format!("/api/bots/arthur/memory/{}", entry_id),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // GET and verify it's gone
    let (status, view) = get(&app, "/api/bots/arthur/memory", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["log"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn searches_the_log_through_the_api() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let cookie = seed_session(&db);

    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params!["arthur", "Arthur", "", "", None::<String>, "2026-01-01T00:00:00Z"],
        )
        .expect("seed bot");

    let app = build_app(AppState::new(db));

    // Add two entries
    post(
        &app,
        "/api/bots/arthur/memory",
        json!({ "content": "The Zenith 14 finish is a countout." }),
        &cookie,
    )
    .await;

    post(
        &app,
        "/api/bots/arthur/memory",
        json!({ "content": "Unrelated note about backups." }),
        &cookie,
    )
    .await;

    // Search with query
    let (status, view) = get(&app, "/api/bots/arthur/memory?q=zenith", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["searched"], true);
    let log = view["log"].as_array().unwrap();
    assert_eq!(log.len(), 1);
    assert!(
        log[0]["content"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("zenith")
    );
}

#[tokio::test]
async fn keeps_the_core_out_of_the_bots_instructions_field() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let cookie = seed_session(&db);

    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params!["arthur", "Arthur", "", "", None::<String>, "2026-01-01T00:00:00Z"],
        )
        .expect("seed bot");

    let app = build_app(AppState::new(db));

    // PUT a core
    put(
        &app,
        "/api/bots/arthur/memory/core",
        json!({ "core": "Remembered thing." }),
        &cookie,
    )
    .await;

    // GET roster
    let (status, roster_response) = get(&app, "/api/roster", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let bots = roster_response["bots"].as_array().unwrap();
    let arthur = bots.iter().find(|b| b["id"] == "arthur").unwrap();
    assert!(
        !arthur["instructions"]
            .as_str()
            .unwrap()
            .contains("Remembered thing.")
    );

    // GET memory core
    let (status, view) = get(&app, "/api/bots/arthur/memory", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["core"], "Remembered thing.");
}

#[tokio::test]
async fn get_shared_core_empty_initially() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let cookie = seed_session(&db);

    let app = build_app(AppState::new(db));

    let (status, body) = get(&app, "/api/shared-core", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["core"], "");
}

#[tokio::test]
async fn put_and_get_shared_core() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let cookie = seed_session(&db);

    let app = build_app(AppState::new(db));

    // PUT a shared core
    let (status, body) = put(
        &app,
        "/api/shared-core",
        json!({ "core": "Shared instructions for all bots." }),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["core"], "Shared instructions for all bots.");

    // GET the shared core
    let (status, body) = get(&app, "/api/shared-core", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["core"], "Shared instructions for all bots.");
}

#[tokio::test]
async fn post_memory_note_with_ttl() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let cookie = seed_session(&db);

    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params!["arthur", "Arthur", "", "", None::<String>, "2026-01-01T00:00:00Z"],
        )
        .expect("seed bot");

    let app = build_app(AppState::new(db));

    // POST a note with TTL
    let (status, created) = post(
        &app,
        "/api/bots/arthur/memory/notes",
        json!({ "content": "Temporary reminder", "ttlSeconds": 3600 }),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["entry"]["content"], "Temporary reminder");

    // Verify it appears in the log
    let (status, view) = get(&app, "/api/bots/arthur/memory", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let log = view["log"].as_array().unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0]["content"], "Temporary reminder");
}

#[tokio::test]
async fn get_projects() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let cookie = seed_session(&db);

    let app = build_app(AppState::new(db));

    let (status, body) = get(&app, "/api/projects", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["projects"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn post_project() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let cookie = seed_session(&db);

    let app = build_app(AppState::new(db));

    // POST a project
    let (status, created) = post(
        &app,
        "/api/projects",
        json!({ "name": "My Project" }),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let project_id = created["project"]["id"].as_str().unwrap().to_string();
    assert_eq!(created["project"]["name"], "My Project");

    // GET projects
    let (status, body) = get(&app, "/api/projects", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let projects = body["projects"].as_array().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0]["id"], project_id);
    assert_eq!(projects[0]["name"], "My Project");
}

#[tokio::test]
async fn post_project_member() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let cookie = seed_session(&db);

    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params!["arthur", "Arthur", "", "", None::<String>, "2026-01-01T00:00:00Z"],
        )
        .expect("seed bot");

    let app = build_app(AppState::new(db));

    // POST a project
    let (status, created) = post(
        &app,
        "/api/projects",
        json!({ "name": "My Project" }),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let project_id = created["project"]["id"].as_str().unwrap().to_string();

    // POST a member to the project
    let (status, _) = post(
        &app,
        &format!("/api/projects/{}/members", project_id),
        json!({ "botId": "arthur" }),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn get_shared_memory() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let cookie = seed_session(&db);

    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params!["arthur", "Arthur", "", "", None::<String>, "2026-01-01T00:00:00Z"],
        )
        .expect("seed bot");

    // Add a shared memory entry directly to the database
    db.conn()
        .execute(
            "INSERT INTO memory_log (id, bot_id, content, source, created_at, kind, scope) VALUES (?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params!["entry1", "arthur", "Shared knowledge", "bot", "2026-01-01T00:00:00Z", "log", "shared"],
        )
        .expect("insert shared memory");

    let app = build_app(AppState::new(db));

    // GET shared memory
    let (status, body) = get(&app, "/api/memory/shared", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let log = body["log"].as_array().unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0]["content"], "Shared knowledge");
    assert_eq!(log[0]["source"], "bot");
}

#[tokio::test]
async fn delete_shared_memory_entry() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let cookie = seed_session(&db);

    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params!["arthur", "Arthur", "", "", None::<String>, "2026-01-01T00:00:00Z"],
        )
        .expect("seed bot");

    // Add a shared memory entry
    db.conn()
        .execute(
            "INSERT INTO memory_log (id, bot_id, content, source, created_at, kind, scope) VALUES (?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params!["entry1", "arthur", "Shared knowledge", "bot", "2026-01-01T00:00:00Z", "log", "shared"],
        )
        .expect("insert shared memory");

    let app = build_app(AppState::new(db));

    // DELETE the entry
    let (status, _) = delete(&app, "/api/memory/shared/entry1", &cookie).await;
    assert_eq!(status, StatusCode::OK);

    // GET shared memory and verify it's gone
    let (status, body) = get(&app, "/api/memory/shared", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["log"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn delete_unknown_shared_entry_returns_404() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let cookie = seed_session(&db);

    let app = build_app(AppState::new(db));

    let (status, _) = delete(&app, "/api/memory/shared/nonexistent", &cookie).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_unknown_entry_returns_404() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let cookie = seed_session(&db);

    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params!["arthur", "Arthur", "", "", None::<String>, "2026-01-01T00:00:00Z"],
        )
        .expect("seed bot");

    let app = build_app(AppState::new(db));

    let (status, _) = delete(&app, "/api/bots/arthur/memory/nonexistent", &cookie).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
