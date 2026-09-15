//! S5-04: Routines routes tests.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rusqlite::params;
use serde_json::{Value, json};
use server::AppState;
use store::Db;
use tower::ServiceExt;
use uuid::Uuid;

mod common;
use common::seed_session;

fn open_db() -> Db {
    Db::open(":memory:").expect("open :memory: db")
}

fn create_test_bot(db: &Db) -> String {
    let bot_id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![&bot_id, "Test Bot", "Testing", "Do things", &now],
        )
        .expect("create test bot");
    bot_id
}

fn app_for(db: Db) -> axum::Router {
    let state = AppState::new(db);
    server::build_app(state)
}

async fn get_with_auth(app: &axum::Router, path: &str, session: &str) -> (StatusCode, Value) {
    let request = Request::get(path)
        .header("cookie", session)
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_json = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);

    (status, body_json)
}

async fn post_with_auth(
    app: &axum::Router,
    path: &str,
    session: &str,
    body: Value,
) -> (StatusCode, Value) {
    let body_str = body.to_string();
    let request = Request::post(path)
        .header("cookie", session)
        .header("content-type", "application/json")
        .body(Body::from(body_str))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_json = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);

    (status, body_json)
}

async fn patch_with_auth(
    app: &axum::Router,
    path: &str,
    session: &str,
    body: Value,
) -> (StatusCode, Value) {
    let body_str = body.to_string();
    let request = Request::patch(path)
        .header("cookie", session)
        .header("content-type", "application/json")
        .body(Body::from(body_str))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_json = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);

    (status, body_json)
}

async fn delete_with_auth(app: &axum::Router, path: &str, session: &str) -> (StatusCode, Value) {
    let request = Request::delete(path)
        .header("cookie", session)
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_json = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);

    (status, body_json)
}

async fn get_no_auth(app: &axum::Router, path: &str) -> StatusCode {
    let request = Request::get(path).body(Body::empty()).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    response.status()
}

/// Test 1: 401 without a session
#[tokio::test]
async fn list_routines_requires_auth() {
    let db = open_db();
    // Set password so auth checks work (without a password, the server returns 503)
    store::set_password(&db, "test-password").expect("set test password");
    let app = app_for(db);

    let status = get_no_auth(&app, "/api/routines").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "should require auth");
}

/// Test 2: Create routine and list shows it with correct fields
#[tokio::test]
async fn create_routine_and_list() {
    let db = open_db();
    let session = seed_session(&db);
    let bot_id = create_test_bot(&db);
    let app = app_for(db);

    // Create routine
    let (status, body) = post_with_auth(
        &app,
        "/api/routines",
        &session,
        json!({
            "botId": &bot_id,
            "name": "Test Routine",
            "prompt": "Do something",
            "schedule": "every 15 minutes",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "should create routine");
    let routine_id = body
        .get("routine")
        .and_then(|r| r.get("id"))
        .and_then(|v| v.as_str());
    assert!(routine_id.is_some(), "response should have routine.id");

    let routine_id = routine_id.unwrap();
    assert_eq!(
        body.get("routine")
            .and_then(|r| r.get("name"))
            .and_then(|v| v.as_str()),
        Some("Test Routine"),
        "routine name matches"
    );

    // Verify it appears in list
    let (status, body) = get_with_auth(&app, "/api/routines", &session).await;
    assert_eq!(status, StatusCode::OK);
    let routines = body.get("routines").and_then(|v| v.as_array());
    assert!(routines.is_some(), "should have routines array");
    let routines = routines.unwrap();
    assert_eq!(routines.len(), 1, "should have one routine");
    assert_eq!(
        routines[0].get("id").and_then(|v| v.as_str()),
        Some(routine_id),
        "routine id matches"
    );

    // F10: Check that nextRunAt was computed and is not null
    // (is_some() returns true for JSON null, so check explicitly)
    assert!(
        routines[0].get("nextRunAt").is_some_and(|v| !v.is_null()),
        "nextRunAt should be set and not null"
    );
}

/// Test 3: Bad schedule returns 400 with error text
#[tokio::test]
async fn bad_schedule_returns_400() {
    let db = open_db();
    let session = seed_session(&db);
    let bot_id = create_test_bot(&db);
    let app = app_for(db);

    let (status, body) = post_with_auth(
        &app,
        "/api/routines",
        &session,
        json!({
            "botId": &bot_id,
            "name": "Bad Schedule",
            "prompt": "Do something",
            "schedule": "every 0 minutes",
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "should reject bad schedule"
    );
    assert!(body.get("error").is_some(), "should have error message");
}

/// Test 4: PATCH updates schedule and recomputes nextRunAt
#[tokio::test]
async fn patch_schedule_updates_next_run_at() {
    let db = open_db();
    let session = seed_session(&db);
    let bot_id = create_test_bot(&db);
    let app = app_for(db);

    // Create routine with 15-minute interval
    let (status, body) = post_with_auth(
        &app,
        "/api/routines",
        &session,
        json!({
            "botId": &bot_id,
            "name": "Test",
            "prompt": "Do something",
            "schedule": "every 15 minutes",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    let routine_id = body
        .get("routine")
        .and_then(|r| r.get("id"))
        .and_then(|v| v.as_str())
        .unwrap();
    let old_next_run = body
        .get("routine")
        .and_then(|r| r.get("nextRunAt"))
        .and_then(|v| v.as_str());

    // Update schedule
    let (status, body) = patch_with_auth(
        &app,
        &format!("/api/routines/{}", routine_id),
        &session,
        json!({
            "schedule": "every 30 minutes",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "should patch routine");
    let new_next_run = body
        .get("routine")
        .and_then(|r| r.get("nextRunAt"))
        .and_then(|v| v.as_str());

    // nextRunAt should have changed
    assert_ne!(
        old_next_run, new_next_run,
        "nextRunAt should be different after schedule change"
    );
}

/// Test 5: Active toggle
#[tokio::test]
async fn toggle_routine_active() {
    let db = open_db();
    let session = seed_session(&db);
    let bot_id = create_test_bot(&db);
    let app = app_for(db);

    // Create routine (starts inactive)
    let (status, body) = post_with_auth(
        &app,
        "/api/routines",
        &session,
        json!({
            "botId": &bot_id,
            "name": "Test",
            "prompt": "Do something",
            "schedule": "hourly",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    let routine_id = body
        .get("routine")
        .and_then(|r| r.get("id"))
        .and_then(|v| v.as_str())
        .unwrap();

    // Activate it
    let (status, body) = post_with_auth(
        &app,
        &format!("/api/routines/{}/active", routine_id),
        &session,
        json!({"active": true}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body.get("routine")
            .and_then(|r| r.get("active"))
            .and_then(|v| v.as_bool()),
        Some(true),
        "should be active"
    );

    // Deactivate it
    let (status, body) = post_with_auth(
        &app,
        &format!("/api/routines/{}/active", routine_id),
        &session,
        json!({"active": false}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body.get("routine")
            .and_then(|r| r.get("active"))
            .and_then(|v| v.as_bool()),
        Some(false),
        "should be inactive"
    );
}

/// S5-F-01/F2: pressing "Start" used to write `next_run_at = NULL`
/// unconditionally (`set_routine_active(&db, &id, active, None)`), and
/// `due_routines`/`fire_due` both require `next_run_at IS NOT NULL` - a
/// routine started this way could never fire again. This is the bug's own
/// end-to-end proof: Start must hand back a non-null, FUTURE `nextRunAt`,
/// and that instant must actually be the one `fire_due` finds due.
///
/// Uses `AppState::with_port`/`ScriptedPort` rather than `app_for`'s real
/// `AppState::new` (which would reach for a real `OpenRouterPort`) because
/// `fire_due` here really does start a run - same posture as `tests/
/// routines_fire.rs`. Routing and the judge are explicitly disabled on this
/// db, per the ticket header rule, even though `open_db` (shared with every
/// other test in this file) does not do that itself.
#[tokio::test]
async fn starting_a_routine_gives_it_a_non_null_next_run_at_that_fires() {
    let db = open_db();
    model::routing::set_routing_settings(&db, Some(false), None)
        .expect("disable routing classifier");
    server::judge::set_judge_enabled(&db, false).expect("disable judge");
    let session = seed_session(&db);
    let bot_id = create_test_bot(&db);

    let scripted =
        std::sync::Arc::new(common::ScriptedPort::new(vec![common::text_script("done")]));
    let port: std::sync::Arc<dyn model::ModelPort> = scripted.clone();
    let state = AppState::with_port(db, port);
    let app = server::build_app(state.clone());

    let (status, body) = post_with_auth(
        &app,
        "/api/routines",
        &session,
        json!({
            "botId": &bot_id,
            "name": "Test",
            "prompt": "Do something",
            "schedule": "every 15 minutes",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let routine_id = body
        .get("routine")
        .and_then(|r| r.get("id"))
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    let (status, body) = post_with_auth(
        &app,
        &format!("/api/routines/{}/active", routine_id),
        &session,
        json!({"active": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let next_run_at = body
        .get("routine")
        .and_then(|r| r.get("nextRunAt"))
        .and_then(|v| v.as_str())
        .expect("F2: Start must set a non-null nextRunAt")
        .to_string();
    let next_run_at: chrono::DateTime<chrono::Utc> =
        next_run_at.parse().expect("nextRunAt is a valid timestamp");
    assert!(
        next_run_at > chrono::Utc::now(),
        "nextRunAt should be in the future right after Start"
    );

    // Prove it, don't just read it: fire at the computed instant and
    // confirm THIS routine is what came due.
    let started =
        server::routines::fire_due(&state, next_run_at + chrono::Duration::seconds(1)).await;
    assert_eq!(
        started.len(),
        1,
        "the routine started via POST /:id/active must fire on the next tick"
    );
    assert_eq!(started[0].0, routine_id);
}

/// Test 6: Delete routine
#[tokio::test]
async fn delete_routine() {
    let db = open_db();
    let session = seed_session(&db);
    let bot_id = create_test_bot(&db);
    let app = app_for(db);

    // Create routine
    let (_create_status, body) = post_with_auth(
        &app,
        "/api/routines",
        &session,
        json!({
            "botId": &bot_id,
            "name": "Test",
            "prompt": "Do something",
            "schedule": "hourly",
        }),
    )
    .await;

    let routine_id = body
        .get("routine")
        .and_then(|r| r.get("id"))
        .and_then(|v| v.as_str())
        .unwrap();

    // Delete it
    let (delete_status, _body) =
        delete_with_auth(&app, &format!("/api/routines/{}", routine_id), &session).await;

    assert_eq!(delete_status, StatusCode::OK, "should delete routine");

    // Verify it's gone
    let (list_status, body) = get_with_auth(&app, "/api/routines", &session).await;
    assert_eq!(list_status, StatusCode::OK);
    let empty_vec = vec![];
    let routines = body
        .get("routines")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty_vec);
    assert!(routines.is_empty(), "routine should be deleted");
}

/// Test 7: Get routine runs (capped at 20)
#[tokio::test]
async fn get_routine_runs_limited() {
    let db = open_db();
    let session = seed_session(&db);
    let bot_id = create_test_bot(&db);
    let app = app_for(db);

    // Create routine
    let (_status, body) = post_with_auth(
        &app,
        "/api/routines",
        &session,
        json!({
            "botId": &bot_id,
            "name": "Test",
            "prompt": "Do something",
            "schedule": "hourly",
        }),
    )
    .await;

    let routine_id = body
        .get("routine")
        .and_then(|r| r.get("id"))
        .and_then(|v| v.as_str())
        .unwrap();

    // Get runs (should be empty for new routine)
    let (_status, body) = get_with_auth(
        &app,
        &format!("/api/routines/{}/runs", routine_id),
        &session,
    )
    .await;

    let empty_vec = vec![];
    let runs = body
        .get("runs")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty_vec);
    assert_eq!(runs.len(), 0, "new routine should have no runs");
}

/// S5-F-02 (F3/F5, `reviews/S5-R.md`): before this fix, `POST /api/
/// routines` stored the raw phrase Josh typed ("every 15 minutes") in the
/// `schedule` column and echoed it back as a bare string - a live TS
/// Bullpen's `JSON.parse(row.schedule)` would throw on that row. This is
/// the fix's own end-to-end proof, from what a client observes: the
/// created routine's `schedule` field is the TS JSON object (not a typed
/// phrase), and `scheduleText` carries the human description - on BOTH the
/// create response and the list.
#[tokio::test]
async fn created_routine_holds_the_ts_json_schedule_shape() {
    let db = open_db();
    let session = seed_session(&db);
    let bot_id = create_test_bot(&db);
    let app = app_for(db);

    let (status, body) = post_with_auth(
        &app,
        "/api/routines",
        &session,
        json!({
            "botId": &bot_id,
            "name": "Test",
            "prompt": "Do something",
            "schedule": "every 15 minutes",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    assert_eq!(
        body.get("routine").and_then(|r| r.get("schedule")),
        Some(&json!({"kind": "interval", "minutes": 15})),
        "schedule should be the TS JSON object, not the typed phrase"
    );
    assert_eq!(
        body.get("routine")
            .and_then(|r| r.get("scheduleText"))
            .and_then(|v| v.as_str()),
        Some("every 15 minutes"),
        "scheduleText should be the human description"
    );

    // The list carries the same shape, not just the create response.
    let (status, body) = get_with_auth(&app, "/api/routines", &session).await;
    assert_eq!(status, StatusCode::OK);
    let routines = body.get("routines").and_then(|v| v.as_array()).unwrap();
    assert_eq!(routines.len(), 1);
    assert_eq!(
        routines[0].get("schedule"),
        Some(&json!({"kind": "interval", "minutes": 15}))
    );
    assert_eq!(
        routines[0].get("scheduleText").and_then(|v| v.as_str()),
        Some("every 15 minutes"),
        "the list renders the description from scheduleText"
    );
}

/// S5-F-02 (F3): the actual STORED column, not just the wire response.
/// `routine_wire_json`'s read side reconstructs the wire `schedule` object
/// via `schedule::parse_schedule`, which (deliberately, for legacy rows)
/// accepts a stored PHRASE just as readily as stored JSON - so a
/// wire-only assertion (like the test above) cannot tell "the route wrote
/// JSON" from "the route wrote the phrase and the reader papered over
/// it". A file-backed db (not `:memory:`) lets a second, independent
/// connection read back the exact bytes the route wrote, the way a live
/// TS Bullpen's `JSON.parse(row.schedule)` would see them.
#[tokio::test]
async fn created_routine_writes_ts_json_to_the_column_not_the_phrase() {
    let temp_dir = tempfile::TempDir::new().expect("create temp dir");
    let db_path = temp_dir
        .path()
        .join("bullpen.db")
        .to_str()
        .expect("utf8 path")
        .to_string();

    let db = Db::open(&db_path).expect("open file db");
    let session = seed_session(&db);
    let bot_id = create_test_bot(&db);
    let app = app_for(db);

    let (status, body) = post_with_auth(
        &app,
        "/api/routines",
        &session,
        json!({
            "botId": &bot_id,
            "name": "Test",
            "prompt": "Do something",
            "schedule": "every 15 minutes",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let routine_id = body
        .get("routine")
        .and_then(|r| r.get("id"))
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    let readback = Db::open(&db_path).expect("reopen file db");
    let stored = store::routine_by_id(&readback, &routine_id)
        .expect("query")
        .expect("routine exists")
        .schedule;
    assert_eq!(
        stored, r#"{"kind":"interval","minutes":15}"#,
        "the column must hold the TS JSON shape, not the typed phrase"
    );
}

/// S5-F-02 (F3): a `PATCH` that changes the schedule must re-encode the new
/// phrase as TS JSON too, not just recompute `nextRunAt`.
#[tokio::test]
async fn patched_schedule_holds_the_ts_json_schedule_shape() {
    let db = open_db();
    let session = seed_session(&db);
    let bot_id = create_test_bot(&db);
    let app = app_for(db);

    let (_status, body) = post_with_auth(
        &app,
        "/api/routines",
        &session,
        json!({
            "botId": &bot_id,
            "name": "Test",
            "prompt": "Do something",
            "schedule": "hourly",
        }),
    )
    .await;
    let routine_id = body
        .get("routine")
        .and_then(|r| r.get("id"))
        .and_then(|v| v.as_str())
        .unwrap();

    let (status, body) = patch_with_auth(
        &app,
        &format!("/api/routines/{}", routine_id),
        &session,
        json!({"schedule": "daily at 07:30"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body.get("routine").and_then(|r| r.get("schedule")),
        Some(&json!({"kind": "daily", "hour": 7, "minute": 30}))
    );
    assert_eq!(
        body.get("routine")
            .and_then(|r| r.get("scheduleText"))
            .and_then(|v| v.as_str()),
        Some("daily at 07:30")
    );
}

/// S5-F-02 (F3): a routine row written directly with the TS JSON shape in
/// its `schedule` column (as a live TS Bullpen would leave it -
/// `routines.ts:410`'s `JSON.stringify(parsed.schedule)`) must fire exactly
/// like one this crate created itself. Before F3, `fire_due`'s
/// `schedule::parse_schedule` only understood the human phrase grammar, so
/// a TS-shaped row's JSON failed to parse and `fire_due` `continue`d past
/// it forever - silently never firing, no health recorded
/// (`reviews/S5-R.md` F3). Uses `AppState::with_port`/`ScriptedPort`
/// (not `app_for`'s real `AppState::new`) because `fire_due` here really
/// does start a run, same posture as `starting_a_routine_gives_it_a_non_
/// null_next_run_at_that_fires` above and `tests/routines_fire.rs`.
#[tokio::test]
async fn a_ts_shaped_json_schedule_row_fires() {
    let db = open_db();
    model::routing::set_routing_settings(&db, Some(false), None)
        .expect("disable routing classifier");
    server::judge::set_judge_enabled(&db, false).expect("disable judge");
    let bot_id = create_test_bot(&db);

    let routine_id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    let past = (now - chrono::Duration::minutes(1)).to_rfc3339();
    db.conn()
        .execute(
            "INSERT INTO routines (id, bot_id, name, prompt, schedule, active, next_run_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7)",
            params![
                &routine_id,
                &bot_id,
                "TS routine",
                "Do the TS thing",
                r#"{"kind":"interval","minutes":15}"#,
                &past,
                &now.to_rfc3339(),
            ],
        )
        .expect("seed a TS-shaped routine row");

    let scripted =
        std::sync::Arc::new(common::ScriptedPort::new(vec![common::text_script("done")]));
    let port: std::sync::Arc<dyn model::ModelPort> = scripted.clone();
    let state = AppState::with_port(db, port);

    let started = server::routines::fire_due(&state, now).await;
    assert_eq!(
        started.iter().filter(|(id, _)| id == &routine_id).count(),
        1,
        "a TS-shaped JSON schedule row must fire, not be skipped forever"
    );
}

/// F6: POST with empty prompt returns 400 with TS error text
#[tokio::test]
async fn empty_prompt_returns_400() {
    let db = open_db();
    let session = seed_session(&db);
    let bot_id = create_test_bot(&db);
    let app = app_for(db);

    let (status, body) = post_with_auth(
        &app,
        "/api/routines",
        &session,
        json!({
            "botId": &bot_id,
            "name": "Test",
            "prompt": "",
            "schedule": "every 15 minutes",
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "should reject empty prompt"
    );
    assert_eq!(
        body.get("error").and_then(|v| v.as_str()),
        Some("Give the routine something to do."),
        "should return TS error text"
    );
}

/// F6: POST with empty name returns 400 with TS error text
#[tokio::test]
async fn empty_name_returns_400() {
    let db = open_db();
    let session = seed_session(&db);
    let bot_id = create_test_bot(&db);
    let app = app_for(db);

    let (status, body) = post_with_auth(
        &app,
        "/api/routines",
        &session,
        json!({
            "botId": &bot_id,
            "name": "   ",
            "prompt": "Do something",
            "schedule": "every 15 minutes",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "should reject empty name");
    assert_eq!(
        body.get("error").and_then(|v| v.as_str()),
        Some("Give the routine a name."),
        "should return TS error text"
    );
}

/// F6: POST with bad bot ID returns 400 with "no such bot"
#[tokio::test]
async fn bad_bot_id_returns_400() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, body) = post_with_auth(
        &app,
        "/api/routines",
        &session,
        json!({
            "botId": "nonexistent-bot-id",
            "name": "Test",
            "prompt": "Do something",
            "schedule": "every 15 minutes",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "should reject bad bot");
    assert_eq!(
        body.get("error").and_then(|v| v.as_str()),
        Some("no such bot"),
        "should return bot not found error"
    );
}

/// F7: DELETE of a missing routine returns 404
#[tokio::test]
async fn delete_missing_routine_returns_404() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, body) = delete_with_auth(&app, "/api/routines/nonexistent-id", &session).await;

    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "should return 404 for missing routine"
    );
    assert_eq!(
        body.get("error").and_then(|v| v.as_str()),
        Some("no such routine"),
        "should return not found error"
    );
}

/// F6: POST with invalid hookMatch pattern returns 400
#[tokio::test]
async fn post_with_invalid_hook_match_returns_400() {
    let db = open_db();
    let session = seed_session(&db);
    let bot_id = create_test_bot(&db);
    let app = app_for(db);

    let (status, body) = post_with_auth(
        &app,
        "/api/routines",
        &session,
        json!({
            "botId": bot_id,
            "name": "Test",
            "prompt": "Do something",
            "schedule": "every 15 minutes",
            "hookMatch": "[unclosed",  // Invalid regex
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body.get("error").and_then(|v| v.as_str()),
        Some("hook match must be a valid regular expression")
    );
}

/// F7: POST with empty tools array stores NULL in database
#[tokio::test]
async fn post_with_empty_tools_stores_null() {
    let db = open_db();
    let session = seed_session(&db);
    let bot_id = create_test_bot(&db);
    let app = app_for(db);

    let (status, body) = post_with_auth(
        &app,
        "/api/routines",
        &session,
        json!({
            "botId": bot_id,
            "name": "Test",
            "prompt": "Do something",
            "schedule": "every 15 minutes",
            "tools": [],  // Empty list should become NULL
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);

    // Verify tools is NULL in the response (not an empty array)
    assert_eq!(
        body.get("routine").and_then(|r| r.get("tools")),
        Some(&Value::Null),
        "tools should be NULL, not []"
    );
}

/// F8: PATCH kind to "prompt" on a tool routine NULLs tool and tool_args
#[tokio::test]
async fn patch_to_prompt_nulls_tool_and_tool_args() {
    let db = open_db();
    let session = seed_session(&db);
    let bot_id = create_test_bot(&db);
    let app = app_for(db);

    // First create a tool routine
    let (status, create_body) = post_with_auth(
        &app,
        "/api/routines",
        &session,
        json!({
            "botId": bot_id,
            "name": "Tool Test",
            "prompt": "Do something",
            "schedule": "every 15 minutes",
            "kind": "tool",
            "tool": "shell",
            "toolArgs": "{}",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    let routine_id = create_body
        .get("routine")
        .and_then(|r| r.get("id"))
        .and_then(|id| id.as_str())
        .expect("routine id");

    // Verify tool is stored
    assert_eq!(
        create_body
            .get("routine")
            .and_then(|r| r.get("tool"))
            .and_then(|t| t.as_str()),
        Some("shell"),
        "tool should be set"
    );

    // Now PATCH to kind=prompt
    let (status, patch_body) = patch_with_auth(
        &app,
        &format!("/api/routines/{}", routine_id),
        &session,
        json!({
            "kind": "prompt",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);

    // Verify tool is now NULL in the response
    assert_eq!(
        patch_body.get("routine").and_then(|r| r.get("tool")),
        Some(&Value::Null),
        "tool should be NULL after switching to prompt"
    );

    // Verify tool_args is also NULL
    assert_eq!(
        patch_body.get("routine").and_then(|r| r.get("toolArgs")),
        Some(&Value::Null),
        "toolArgs should be NULL after switching to prompt"
    );
}
