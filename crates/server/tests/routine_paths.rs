//! S5b-01: routine path guard - test the validation logic for routine prompts.
//! Port of `src/server/routine-paths.ts`, testing `checkRoutinePaths`,
//! `blockingProblems`, and `describePathProblems`.

use axum::body::Body;
use axum::http::Request;
use serde_json::json;
use server::AppState;
use store::Db;
use tower::ServiceExt;

mod common;
use common::seed_session;

fn open_db() -> Db {
    Db::open(":memory:").expect("open :memory: db")
}

fn app_for(db: Db) -> axum::Router {
    let state = AppState::new(db);
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

async fn post_create_routine(
    app: &axum::Router,
    session: &str,
    bot_id: &str,
    name: &str,
    prompt: &str,
    schedule: &str,
) -> (u16, serde_json::Value) {
    let body = json!({
        "botId": bot_id,
        "name": name,
        "prompt": prompt,
        "schedule": schedule,
    });

    let request = Request::post("/api/routines")
        .header("cookie", session)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("create request");

    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("request succeeded");

    let status = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("parse JSON");

    (status, json)
}

#[tokio::test]
async fn test_create_routine_no_paths() {
    let db = open_db();
    seed_bot(&db, "bot1", "Test Bot");
    let session = seed_session(&db);

    let app = app_for(db);

    let (status, _json) = post_create_routine(
        &app,
        &session,
        "bot1",
        "No Paths Routine",
        "This is a routine that does not mention any files or paths.",
        "every 15 minutes",
    )
    .await;

    assert_eq!(status, 201, "routine without paths should be created");
}

#[tokio::test]
async fn test_create_routine_with_workspace_path_blocked() {
    let db = open_db();
    seed_bot(&db, "bot1", "Test Bot");
    let session = seed_session(&db);

    let app = app_for(db);

    let (status, json) = post_create_routine(
        &app,
        &session,
        "bot1",
        "Workspace Routine",
        "Read the file at /workspace/pres-coach/config.txt and process it.",
        "every 15 minutes",
    )
    .await;

    assert_eq!(
        status, 400,
        "routine with /workspace path should be rejected"
    );
    let error = json["error"].as_str().expect("error message");
    assert!(
        error.contains("/workspace/pres-coach/config.txt"),
        "error should mention the path"
    );
    assert!(error.contains("Grok Bot"), "error should mention Grok Bot");
}

#[tokio::test]
async fn test_create_routine_with_workdir_allowed() {
    let db = open_db();
    seed_bot(&db, "bot1", "Test Bot");
    let session = seed_session(&db);

    let app = app_for(db);

    let (status, _json) = post_create_routine(
        &app,
        &session,
        "bot1",
        "Work Dir Routine",
        "Read and process /work/data.txt from the work directory.",
        "every 15 minutes",
    )
    .await;

    assert_eq!(status, 201, "routine with /work path should be created");
}

#[tokio::test]
async fn test_create_routine_with_windows_path_needs_tool() {
    let db = open_db();
    seed_bot(&db, "bot1", "Test Bot");
    let session = seed_session(&db);

    let app = app_for(db);

    let (status, _json) = post_create_routine(
        &app,
        &session,
        "bot1",
        "Windows Routine",
        "Copy the file from C:\\Users\\Josh\\data.txt to the server.",
        "every 15 minutes",
    )
    .await;

    // Windows paths are "needs-a-tool", not blocking - they don't refuse creation
    assert_eq!(
        status, 201,
        "routine with Windows path should be created (needs a tool, not unreachable)"
    );
}

#[tokio::test]
async fn test_create_routine_url_route_allowed() {
    let db = open_db();
    seed_bot(&db, "bot1", "Test Bot");
    let session = seed_session(&db);

    let app = app_for(db);

    let (status, _json) = post_create_routine(
        &app,
        &session,
        "bot1",
        "Route Check Routine",
        "Check the /login and /admin routes for errors in the SHOOT site.",
        "every 15 minutes",
    )
    .await;

    assert_eq!(
        status, 201,
        "routine mentioning URL routes should be created"
    );
}

#[tokio::test]
async fn test_create_routine_mixed_paths_blocks_on_unreachable() {
    let db = open_db();
    seed_bot(&db, "bot1", "Test Bot");
    let session = seed_session(&db);

    let app = app_for(db);

    let (status, json) = post_create_routine(
        &app,
        &session,
        "bot1",
        "Mixed Paths Routine",
        "Read /workspace/data.txt and /work/safe.txt and /home/rainmade/logs.txt and C:\\temp.txt",
        "every 15 minutes",
    )
    .await;

    assert_eq!(
        status, 400,
        "routine with /workspace (unreachable) should be rejected even with other paths"
    );
    let error = json["error"].as_str().expect("error message");
    assert!(
        error.contains("/workspace/data.txt"),
        "error should mention the unreachable path"
    );
}

#[tokio::test]
async fn test_create_routine_trailing_punctuation_trimmed() {
    let db = open_db();
    seed_bot(&db, "bot1", "Test Bot");
    let session = seed_session(&db);

    let app = app_for(db);

    // Path with trailing period should still be detected and blocked
    let (status, json) = post_create_routine(
        &app,
        &session,
        "bot1",
        "Punctuation Routine",
        "The file is at /workspace/config.txt. Read it carefully.",
        "every 15 minutes",
    )
    .await;

    assert_eq!(
        status, 400,
        "routine with /workspace path (even with trailing punctuation) should be rejected"
    );
    let error = json["error"].as_str().expect("error message");
    assert!(
        error.contains("/workspace/config.txt"),
        "error should mention the path without period"
    );
}

#[tokio::test]
async fn test_home_path_on_host_needs_tool() {
    let db = open_db();
    seed_bot(&db, "bot1", "Test Bot");
    let session = seed_session(&db);

    let app = app_for(db);

    let (status, _json) = post_create_routine(
        &app,
        &session,
        "bot1",
        "Host Path Routine",
        "Read the config at /home/rainmade/app/config.json",
        "every 15 minutes",
    )
    .await;

    // /home paths are "needs-a-tool", not blocking - they don't refuse creation
    assert_eq!(
        status, 201,
        "routine with /home path should be created (needs ssh tool, not unreachable)"
    );
}

#[tokio::test]
async fn test_root_path_needs_tool() {
    let db = open_db();
    seed_bot(&db, "bot1", "Test Bot");
    let session = seed_session(&db);

    let app = app_for(db);

    let (status, _json) = post_create_routine(
        &app,
        &session,
        "bot1",
        "Root Path Routine",
        "Read /root/.ssh/config on the server",
        "every 15 minutes",
    )
    .await;

    // /root paths are "needs-a-tool" (ssh), not blocking
    assert_eq!(
        status, 201,
        "routine with /root path should be created (needs ssh tool, not unreachable)"
    );
}

#[tokio::test]
async fn test_update_routine_with_blocking_path() {
    let db = open_db();
    seed_bot(&db, "bot1", "Test Bot");
    let session = seed_session(&db);

    let app = app_for(db);

    // First create a routine with a safe prompt
    let (status, create_json) = post_create_routine(
        &app,
        &session,
        "bot1",
        "Original Routine",
        "This routine is safe and has no paths.",
        "every 15 minutes",
    )
    .await;

    assert_eq!(status, 201);
    let routine_id = create_json["routine"]["id"].as_str().expect("routine id");

    // Now try to update it with a blocking path
    let update_body = json!({
        "prompt": "Now read from /workspace/new-path/file.txt"
    });

    let request = Request::patch(format!("/api/routines/{routine_id}"))
        .header("cookie", &session)
        .header("content-type", "application/json")
        .body(Body::from(update_body.to_string()))
        .expect("update request");

    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("request succeeded");

    let status = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("parse JSON");

    assert_eq!(status, 400, "update with blocking path should be rejected");
    let error = json["error"].as_str().expect("error message");
    assert!(
        error.contains("/workspace/new-path/file.txt"),
        "error should mention the new blocking path"
    );
}

#[tokio::test]
async fn test_describe_path_problems_message() {
    // Test the human-readable error message
    let problems = vec![
        server::routine_paths::PathProblem {
            path: "/workspace/test.txt".to_string(),
            why: "Grok Bot's layout; a Bullpen sandbox has no /workspace".to_string(),
            suggestion: Some("/work/test.txt".to_string()),
            kind: server::routine_paths::PathKind::Unreachable,
        },
        server::routine_paths::PathProblem {
            path: "/home/rainmade/logs.txt".to_string(),
            why: "on meridian itself, so it needs the ssh tool".to_string(),
            suggestion: None,
            kind: server::routine_paths::PathKind::NeedsATool,
        },
    ];

    let description = server::routine_paths::describe_path_problems(&problems);
    assert!(
        description.contains("paths"),
        "should say 'paths' for multiple"
    );
    assert!(
        description.contains("/workspace/test.txt"),
        "should mention /workspace path"
    );
    assert!(
        description.contains("/work/test.txt"),
        "should show suggestion"
    );
    assert!(
        description.contains("/work"),
        "should mention /work as the only sandbox path"
    );
}
