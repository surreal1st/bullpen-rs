//! S12-08: databases + `query_db` — port of `databases.test.ts` bites.

mod common;

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rusqlite::{Connection, OpenFlags};
use server::AppState;
use server::build_app;
use server::databases::{
    DEFAULT_ROW_LIMIT, DbKind, DbTargetInput, MAX_ROW_LIMIT, list_db_targets,
    parse_read_only_select, run_database_tool, upsert_db_target,
};
use server::permissions::{cannot_be_lifted_unattended, decide};
use store::Db;
use tempfile::TempDir;
use tower::ServiceExt;

fn seed_sqlite_file(dir: &TempDir) -> String {
    let path = dir.path().join("target.db");
    let conn = Connection::open(&path).expect("open seed");
    conn.execute_batch(
        "
        CREATE TABLE leads (id INTEGER PRIMARY KEY, name TEXT NOT NULL, stage TEXT NOT NULL);
        CREATE TABLE secrets (id INTEGER PRIMARY KEY, value TEXT NOT NULL);
        ",
    )
    .expect("schema");
    conn.execute(
        "INSERT INTO leads (name, stage) VALUES ('Acme', 'qualified')",
        [],
    )
    .expect("row1");
    conn.execute(
        "INSERT INTO leads (name, stage) VALUES ('Brassrook', 'closed')",
        [],
    )
    .expect("row2");
    conn.execute("INSERT INTO secrets (value) VALUES ('do-not-read-me')", [])
        .expect("secret");
    path.to_string_lossy().into_owned()
}

fn fresh_db() -> Db {
    Db::open(":memory:").expect("memory db")
}

fn target_db(db_path: &str) -> Db {
    let db = fresh_db();
    upsert_db_target(
        &db,
        DbTargetInput {
            id: None,
            label: "CRM".to_string(),
            kind: DbKind::Sqlite,
            path: Some(db_path.to_string()),
            dsn: None,
            tables: vec!["leads".to_string()],
        },
    );
    db
}

#[test]
fn parse_accepts_plain_select() {
    assert!(parse_read_only_select("SELECT * FROM leads", &["leads".to_string()]).ok);
}

#[test]
fn parse_accepts_with_select() {
    let r = parse_read_only_select(
        "WITH recent AS (SELECT * FROM leads) SELECT * FROM recent",
        &["leads".to_string()],
    );
    assert!(r.ok);
}

#[test]
fn parse_bite_refuses_semicolon_chain() {
    let r = parse_read_only_select(
        "SELECT * FROM leads; DROP TABLE leads",
        &["leads".to_string()],
    );
    assert!(!r.ok);
    assert!(
        r.reason
            .unwrap_or_default()
            .to_lowercase()
            .contains("single statement")
    );
}

#[test]
fn parse_refuses_update_delete_pragma_attach() {
    assert!(!parse_read_only_select("UPDATE leads SET stage = 'dead'", &["leads".to_string()]).ok);
    assert!(!parse_read_only_select("DELETE FROM leads", &["leads".to_string()]).ok);
    assert!(!parse_read_only_select("PRAGMA table_info(leads)", &["leads".to_string()]).ok);
    assert!(
        !parse_read_only_select("ATTACH DATABASE '/etc/passwd' AS x", &["leads".to_string()]).ok
    );
}

#[test]
fn parse_refuses_disallowed_table_and_join() {
    let r = parse_read_only_select("SELECT * FROM secrets", &["leads".to_string()]);
    assert!(!r.ok);
    assert!(r.reason.unwrap_or_default().contains("secrets"));
    let j = parse_read_only_select(
        "SELECT * FROM leads JOIN secrets ON secrets.id = leads.id",
        &["leads".to_string()],
    );
    assert!(!j.ok);
}

#[test]
fn parse_refuses_comments() {
    assert!(
        !parse_read_only_select(
            "SELECT * FROM leads -- fetch everything",
            &["leads".to_string()]
        )
        .ok
    );
}

#[test]
fn parse_accepts_trailing_semicolon() {
    assert!(parse_read_only_select("SELECT * FROM leads;", &["leads".to_string()]).ok);
}

#[tokio::test]
async fn tool_select_returns_table() {
    let dir = TempDir::new().expect("tmpdir");
    let path = seed_sqlite_file(&dir);
    let db = Arc::new(Mutex::new(target_db(&path)));
    let result = run_database_tool(
        Arc::clone(&db),
        "query_db",
        &serde_json::json!({ "target": "CRM", "sql": "SELECT * FROM leads" }),
    )
    .await;
    assert!(!result.is_error);
    assert!(result.text.contains("Acme"));
    assert!(result.text.contains("2 rows."));
}

#[tokio::test]
async fn tool_resolves_label_case_insensitive() {
    let dir = TempDir::new().expect("tmpdir");
    let path = seed_sqlite_file(&dir);
    let db = Arc::new(Mutex::new(target_db(&path)));
    let result = run_database_tool(
        Arc::clone(&db),
        "query_db",
        &serde_json::json!({ "target": "crm", "sql": "SELECT * FROM leads" }),
    )
    .await;
    assert!(!result.is_error);
}

#[tokio::test]
async fn tool_refuses_writes_before_sqlite() {
    let dir = TempDir::new().expect("tmpdir");
    let path = seed_sqlite_file(&dir);
    let db = Arc::new(Mutex::new(target_db(&path)));
    let result = run_database_tool(
        Arc::clone(&db),
        "query_db",
        &serde_json::json!({ "target": "CRM", "sql": "UPDATE leads SET stage = 'dead'" }),
    )
    .await;
    assert!(result.is_error);
    assert!(result.text.contains("Refused"));
    let check = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).expect("ro");
    let stage: String = check
        .query_row("SELECT stage FROM leads WHERE name = 'Acme'", [], |r| {
            r.get(0)
        })
        .expect("row");
    assert_eq!(stage, "qualified");
}

#[tokio::test]
async fn tool_refuses_secrets_table() {
    let dir = TempDir::new().expect("tmpdir");
    let path = seed_sqlite_file(&dir);
    let db = Arc::new(Mutex::new(target_db(&path)));
    let result = run_database_tool(
        Arc::clone(&db),
        "query_db",
        &serde_json::json!({ "target": "CRM", "sql": "SELECT * FROM secrets" }),
    )
    .await;
    assert!(result.is_error);
    assert!(!result.text.contains("do-not-read-me"));
}

#[tokio::test]
async fn tool_row_limits() {
    let dir = TempDir::new().expect("tmpdir");
    let path = seed_sqlite_file(&dir);
    {
        let conn = Connection::open(&path).expect("open");
        for i in 0..1500 {
            conn.execute(
                "INSERT INTO leads (name, stage) VALUES (?1, 'new')",
                [format!("row-{i}")],
            )
            .expect("insert");
        }
    }
    let db = Arc::new(Mutex::new(target_db(&path)));
    let default_result = run_database_tool(
        Arc::clone(&db),
        "query_db",
        &serde_json::json!({ "target": "CRM", "sql": "SELECT * FROM leads" }),
    )
    .await;
    assert!(
        default_result
            .text
            .contains(&format!("{DEFAULT_ROW_LIMIT} rows."))
    );
    let capped = run_database_tool(
        Arc::clone(&db),
        "query_db",
        &serde_json::json!({ "target": "CRM", "sql": "SELECT * FROM leads", "limit": 5000 }),
    )
    .await;
    assert!(capped.text.contains(&format!("{MAX_ROW_LIMIT} rows.")));
}

#[test]
fn sqlite_engine_readonly_bite() {
    let dir = TempDir::new().expect("tmpdir");
    let path = seed_sqlite_file(&dir);
    let ro = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).expect("ro");
    let err = ro.execute("UPDATE leads SET stage = 'dead'", []);
    assert!(err.is_err());
    let check = Connection::open(&path).expect("rw");
    let stage: String = check
        .query_row("SELECT stage FROM leads WHERE name = 'Acme'", [], |r| {
            r.get(0)
        })
        .expect("row");
    assert_eq!(stage, "qualified");
}

#[tokio::test]
async fn dsn_never_leaks_in_api_or_tool() {
    let db = fresh_db();
    let dsn = "postgres://reader:super-secret-password@127.0.0.1:5432/crm";
    upsert_db_target(
        &db,
        DbTargetInput {
            id: None,
            label: "CRM PG".to_string(),
            kind: DbKind::Postgres,
            path: None,
            dsn: Some(dsn.to_string()),
            tables: vec!["leads".to_string()],
        },
    );
    store::set_password(&db, "owner-password-long-enough").expect("password");
    let token = store::create_session(&db).expect("session");
    let app = build_app(AppState::new(db));

    let post = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/databases")
                .header("content-type", "application/json")
                .header("cookie", format!("bullpen_session={token}"))
                .body(Body::from(
                    serde_json::json!({
                        "label": "CRM PG2",
                        "kind": "postgres",
                        "dsn": dsn,
                        "tables": ["leads"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(post.status(), StatusCode::CREATED);
    let post_body = axum::body::to_bytes(post.into_body(), usize::MAX)
        .await
        .unwrap();
    let post_text = String::from_utf8_lossy(&post_body);
    assert!(!post_text.contains("super-secret-password"));

    let get = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/databases")
                .header("cookie", format!("bullpen_session={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let get_bytes = axum::body::to_bytes(get.into_body(), usize::MAX)
        .await
        .unwrap();
    let get_text = String::from_utf8_lossy(&get_bytes);
    assert!(!get_text.contains("super-secret-password"));
    assert!(!get_text.contains(dsn));

    let db2 = Arc::new(Mutex::new(fresh_db()));
    upsert_db_target(
        &db2.lock().unwrap(),
        DbTargetInput {
            id: None,
            label: "CRM PG".to_string(),
            kind: DbKind::Postgres,
            path: None,
            dsn: Some(dsn.to_string()),
            tables: vec!["leads".to_string()],
        },
    );
    let tool = run_database_tool(
        db2,
        "query_db",
        &serde_json::json!({ "target": "CRM PG", "sql": "SELECT * FROM leads" }),
    )
    .await;
    assert!(tool.is_error);
    assert!(!tool.text.contains("super-secret-password"));
}

#[test]
fn settings_row_encrypts_dsn() {
    let db = fresh_db();
    let dsn = "postgres://reader:super-secret-password@127.0.0.1:5432/crm";
    upsert_db_target(
        &db,
        DbTargetInput {
            id: None,
            label: "CRM PG".to_string(),
            kind: DbKind::Postgres,
            path: None,
            dsn: Some(dsn.to_string()),
            tables: vec!["leads".to_string()],
        },
    );
    let value: String = db
        .conn()
        .query_row(
            "SELECT value FROM settings WHERE key = 'db.targets'",
            [],
            |r| r.get(0),
        )
        .expect("row");
    assert!(!value.contains("super-secret-password"));
}

#[tokio::test]
async fn routes_sqlite_round_trip() {
    let dir = TempDir::new().expect("tmpdir");
    let path = seed_sqlite_file(&dir);
    let db = fresh_db();
    store::set_password(&db, "owner-password-long-enough").expect("password");
    let token = store::create_session(&db).expect("session");
    let app = build_app(AppState::new(db));

    let post = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/databases")
                .header("content-type", "application/json")
                .header("cookie", format!("bullpen_session={token}"))
                .body(Body::from(
                    serde_json::json!({
                        "label": "CRM",
                        "kind": "sqlite",
                        "path": path,
                        "tables": ["leads"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(post.status(), StatusCode::CREATED);
    let created: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(post.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    let id = created["target"]["id"].as_str().expect("id");

    let list = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/databases")
                .header("cookie", format!("bullpen_session={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let list_body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(list.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(list_body["targets"].as_array().unwrap().len(), 1);

    let test = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/databases/{id}/test"))
                .header("cookie", format!("bullpen_session={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(test.status(), StatusCode::OK);
    let test_body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(test.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(test_body["ok"], true);
    let tables = test_body["tables"].as_array().unwrap();
    assert!(tables.iter().any(|t| t == "leads"));
    assert!(tables.iter().any(|t| t == "secrets"));

    let del = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/databases/{id}"))
                .header("cookie", format!("bullpen_session={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let del_body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(del.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(del_body["targets"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn post_garbage_body_is_400() {
    let db = fresh_db();
    store::set_password(&db, "owner-password-long-enough").expect("password");
    let token = store::create_session(&db).expect("session");
    let app = build_app(AppState::new(db));
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/databases")
                .header("content-type", "application/json")
                .header("cookie", format!("bullpen_session={token}"))
                .body(Body::from(
                    serde_json::json!({ "label": "", "kind": "sqlite", "tables": [] }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn query_db_permission_posture() {
    let db_arc = Arc::new(Mutex::new(fresh_db()));
    common::seed_bot(&db_arc, "scout", "Scout");
    assert_eq!(
        decide(&db_arc.lock().unwrap(), "scout", "query_db").expect("decide"),
        server::permissions::Decision::Ask
    );
    assert!(cannot_be_lifted_unattended("query_db"));
    let dir = TempDir::new().expect("tmpdir");
    let path = seed_sqlite_file(&dir);
    upsert_db_target(
        &db_arc.lock().unwrap(),
        DbTargetInput {
            id: None,
            label: "CRM".to_string(),
            kind: DbKind::Sqlite,
            path: Some(path),
            dsn: None,
            tables: vec!["leads".to_string()],
        },
    );
    assert_eq!(list_db_targets(&db_arc.lock().unwrap()).len(), 1);
}
