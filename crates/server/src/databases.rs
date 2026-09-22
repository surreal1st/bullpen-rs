//! W9: named read-only database targets (`db.targets` settings row) and
//! `query_db`. Port of `projects/bullpen-night/src/server/databases.ts`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use model::ToolSpec;
use regex::Regex;
use rusqlite::{Connection, OpenFlags, Row, types::ValueRef};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use store::Db;
use tokio_postgres::NoTls;
use uuid::Uuid;

use crate::settings_secrets::{decrypt_for_storage, encrypt_for_storage};
use crate::tools::query_db;

const TARGETS_KEY: &str = "db.targets";
const STATEMENT_TIMEOUT_MS: u64 = 10_000;

pub const DEFAULT_ROW_LIMIT: i64 = 200;
pub const MAX_ROW_LIMIT: i64 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DbKind {
    Sqlite,
    Postgres,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DbTarget {
    id: String,
    label: String,
    kind: DbKind,
    path: String,
    #[serde(rename = "dsnEncrypted", default)]
    dsn_encrypted: String,
    tables: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DbTargetPublic {
    pub id: String,
    pub label: String,
    pub kind: DbKind,
    pub path: String,
    pub has_dsn: bool,
    pub tables: Vec<String>,
}

fn to_public(target: &DbTarget) -> DbTargetPublic {
    DbTargetPublic {
        id: target.id.clone(),
        label: target.label.clone(),
        kind: target.kind,
        path: target.path.clone(),
        has_dsn: !target.dsn_encrypted.is_empty(),
        tables: target.tables.clone(),
    }
}

fn clean_tables(input: &[Value]) -> Vec<String> {
    let mut set = HashSet::new();
    for v in input {
        if let Some(s) = v.as_str() {
            let t = s.trim().to_lowercase();
            if !t.is_empty() {
                set.insert(t);
            }
        }
    }
    let mut out: Vec<String> = set.into_iter().collect();
    out.sort();
    out
}

fn sanitize_stored(raw: &Value) -> Option<DbTarget> {
    let obj = raw.as_object()?;
    let id = obj.get("id")?.as_str()?.to_string();
    let label = obj.get("label")?.as_str()?.to_string();
    if id.is_empty() || label.is_empty() {
        return None;
    }
    let kind = match obj.get("kind").and_then(|v| v.as_str()) {
        Some("postgres") => DbKind::Postgres,
        _ => DbKind::Sqlite,
    };
    let tables = obj
        .get("tables")
        .and_then(|v| v.as_array())
        .map(|a| clean_tables(a))
        .unwrap_or_default();
    Some(DbTarget {
        id,
        label,
        kind,
        path: obj
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        dsn_encrypted: obj
            .get("dsnEncrypted")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        tables,
    })
}

fn read_all(db: &Db) -> Vec<DbTarget> {
    let Some(raw) = db.settings_get(TARGETS_KEY).ok().flatten() else {
        return vec![];
    };
    let parsed: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let Some(arr) = parsed.as_array() else {
        return vec![];
    };
    arr.iter().filter_map(sanitize_stored).collect()
}

fn write_all(db: &Db, targets: &[DbTarget]) {
    let _ = db.settings_set(
        TARGETS_KEY,
        &serde_json::to_string(targets).unwrap_or_else(|_| "[]".to_string()),
    );
}

pub fn list_db_targets(db: &Db) -> Vec<DbTargetPublic> {
    read_all(db).iter().map(to_public).collect()
}

pub(crate) fn resolve_db_target(db: &Db, reference: &str) -> Option<DbTarget> {
    let all = read_all(db);
    if let Some(t) = all.iter().find(|t| t.id == reference) {
        return Some(t.clone());
    }
    let norm = reference.trim().to_lowercase();
    all.into_iter()
        .find(|t| t.label.trim().to_lowercase() == norm)
}

#[derive(Debug, Clone)]
pub struct DbTargetInput {
    pub id: Option<String>,
    pub label: String,
    pub kind: DbKind,
    pub path: Option<String>,
    pub dsn: Option<String>,
    pub tables: Vec<String>,
}

pub fn parse_target_input(body: &Value) -> DbTargetInput {
    let obj = body.as_object();
    let label = obj
        .and_then(|o| o.get("label"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let kind = match obj.and_then(|o| o.get("kind")).and_then(|v| v.as_str()) {
        Some("postgres") => DbKind::Postgres,
        _ => DbKind::Sqlite,
    };
    let tables = obj
        .and_then(|o| o.get("tables"))
        .and_then(|v| v.as_array())
        .map(|a| clean_tables(a))
        .unwrap_or_default();
    DbTargetInput {
        id: obj
            .and_then(|o| o.get("id"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
        label,
        kind,
        path: obj
            .and_then(|o| o.get("path"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
        dsn: obj
            .and_then(|o| o.get("dsn"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
        tables,
    }
}

pub struct UpsertResult {
    pub ok: bool,
    pub target: Option<DbTargetPublic>,
    pub error: Option<String>,
}

pub fn upsert_db_target(db: &Db, input: DbTargetInput) -> UpsertResult {
    let label = input.label.trim();
    if label.is_empty() {
        return UpsertResult {
            ok: false,
            target: None,
            error: Some("A label is required.".to_string()),
        };
    }

    let mut all = read_all(db);
    let existing_index = input
        .id
        .as_ref()
        .and_then(|id| all.iter().position(|t| t.id == *id))
        .unwrap_or(usize::MAX);
    let existing = if existing_index == usize::MAX {
        None
    } else {
        Some(all[existing_index].clone())
    };

    let id = input
        .id
        .clone()
        .or_else(|| existing.as_ref().map(|e| e.id.clone()))
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let (path, dsn_encrypted) = match input.kind {
        DbKind::Sqlite => {
            let path = input
                .path
                .or_else(|| existing.as_ref().map(|e| e.path.clone()))
                .unwrap_or_default()
                .trim()
                .to_string();
            if path.is_empty() {
                return UpsertResult {
                    ok: false,
                    target: None,
                    error: Some("A file path is required for a sqlite target.".to_string()),
                };
            }
            (path, String::new())
        }
        DbKind::Postgres => {
            let dsn = input.dsn.unwrap_or_default().trim().to_string();
            let dsn_encrypted = if !dsn.is_empty() {
                match encrypt_for_storage(db, &dsn) {
                    Ok(enc) => enc,
                    Err(e) => {
                        return UpsertResult {
                            ok: false,
                            target: None,
                            error: Some(e),
                        };
                    }
                }
            } else {
                existing
                    .as_ref()
                    .map(|e| e.dsn_encrypted.clone())
                    .unwrap_or_default()
            };
            if dsn_encrypted.is_empty() {
                return UpsertResult {
                    ok: false,
                    target: None,
                    error: Some(
                        "A connection string is required for a postgres target.".to_string(),
                    ),
                };
            }
            (String::new(), dsn_encrypted)
        }
    };

    let target = DbTarget {
        id,
        label: label.to_string(),
        kind: input.kind,
        path,
        dsn_encrypted,
        tables: clean_tables(&input.tables.iter().map(|s| json!(s)).collect::<Vec<_>>()),
    };

    if existing_index == usize::MAX {
        all.push(target.clone());
    } else {
        all[existing_index] = target.clone();
    }
    write_all(db, &all);
    UpsertResult {
        ok: true,
        target: Some(to_public(&target)),
        error: None,
    }
}

pub fn remove_db_target(db: &Db, id: &str) -> Vec<DbTargetPublic> {
    let all: Vec<_> = read_all(db).into_iter().filter(|t| t.id != id).collect();
    write_all(db, &all);
    all.iter().map(to_public).collect()
}

#[derive(Debug, Clone)]
pub struct ParseResult {
    pub ok: bool,
    pub reason: Option<String>,
    pub cleaned: Option<String>,
}

static DANGEROUS: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
static CTE_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
static TABLE_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();

fn dangerous_re() -> &'static Regex {
    DANGEROUS.get_or_init(|| {
        Regex::new(r"(?i)\b(INSERT|UPDATE|DELETE|DROP|ALTER|CREATE|REPLACE|TRUNCATE|ATTACH|DETACH|PRAGMA|VACUUM|REINDEX|GRANT|REVOKE|EXEC|EXECUTE|CALL|MERGE|LOCK|COPY|INTO)\b")
            .expect("dangerous sql regex")
    })
}

fn cte_re() -> &'static Regex {
    CTE_RE.get_or_init(|| {
        Regex::new(r"(?i)(?:\bWITH\b|,)\s*([a-zA-Z_][a-zA-Z0-9_]*)\s+AS\s*\(").expect("cte regex")
    })
}

fn table_re() -> &'static Regex {
    TABLE_RE.get_or_init(|| {
        Regex::new(r#"(?i)\b(?:FROM|JOIN)\s+"?([a-zA-Z_][a-zA-Z0-9_.]*)"?"#).expect("table regex")
    })
}

pub fn parse_read_only_select(raw: &str, allowed_tables: &[String]) -> ParseResult {
    let sql = raw.trim();
    if sql.is_empty() {
        return ParseResult {
            ok: false,
            reason: Some("The statement is empty.".to_string()),
            cleaned: None,
        };
    }
    if sql.contains("--") || sql.contains("/*") {
        return ParseResult {
            ok: false,
            reason: Some("Comments are not allowed in the statement.".to_string()),
            cleaned: None,
        };
    }

    let no_trailing = sql.strip_suffix(';').unwrap_or(sql).trim();
    if no_trailing.contains(';') {
        return ParseResult {
            ok: false,
            reason: Some("Only a single statement is allowed - no ; chains.".to_string()),
            cleaned: None,
        };
    }
    if no_trailing.is_empty() {
        return ParseResult {
            ok: false,
            reason: Some("The statement is empty.".to_string()),
            cleaned: None,
        };
    }

    let trimmed = no_trailing.trim();
    let starts_select =
        trimmed.len() >= 6 && trimmed.as_bytes()[0..6].eq_ignore_ascii_case(b"SELECT");
    let starts_with = trimmed.len() >= 4 && trimmed.as_bytes()[0..4].eq_ignore_ascii_case(b"WITH");
    if !starts_select && !starts_with {
        return ParseResult {
            ok: false,
            reason: Some("Only SELECT or WITH ... SELECT is allowed.".to_string()),
            cleaned: None,
        };
    }

    if dangerous_re().is_match(no_trailing) {
        return ParseResult {
            ok: false,
            reason: Some("That statement is not a read-only SELECT.".to_string()),
            cleaned: None,
        };
    }

    let mut cte_names = HashSet::new();
    for cap in cte_re().captures_iter(no_trailing) {
        if let Some(n) = cap.get(1) {
            cte_names.insert(n.as_str().to_lowercase());
        }
    }

    let allow_set: HashSet<String> = allowed_tables.iter().map(|t| t.to_lowercase()).collect();
    for cap in table_re().captures_iter(no_trailing) {
        let Some(ident) = cap.get(1) else {
            continue;
        };
        let lower = ident.as_str().to_lowercase();
        let last = lower
            .split('.')
            .next_back()
            .unwrap_or(lower.as_str())
            .to_string();
        if cte_names.contains(&lower) || cte_names.contains(&last) {
            continue;
        }
        if !allow_set.contains(&lower) && !allow_set.contains(&last) {
            return ParseResult {
                ok: false,
                reason: Some(format!(
                    "\"{}\" is not on this target's table allow list.",
                    ident.as_str()
                )),
                cleaned: None,
            };
        }
    }

    ParseResult {
        ok: true,
        reason: None,
        cleaned: Some(no_trailing.to_string()),
    }
}

type QueryRow = HashMap<String, Value>;

#[derive(Debug)]
enum QueryOutcome {
    Ok { rows: Vec<QueryRow> },
    Err { error: String },
}

fn cell_to_json(row: &Row<'_>, idx: usize) -> rusqlite::Result<Value> {
    match row.get_ref(idx)? {
        ValueRef::Null => Ok(Value::Null),
        ValueRef::Integer(i) => Ok(json!(i)),
        ValueRef::Real(f) => Ok(json!(f)),
        ValueRef::Text(s) => Ok(json!(String::from_utf8_lossy(s))),
        ValueRef::Blob(b) => Ok(json!(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            b
        ))),
    }
}

fn rows_from_query(conn: &Connection, sql: &str) -> rusqlite::Result<Vec<QueryRow>> {
    let mut stmt = conn.prepare(sql)?;
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut out = Vec::new();
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let mut map = HashMap::new();
        for (i, name) in names.iter().enumerate() {
            map.insert(name.clone(), cell_to_json(row, i)?);
        }
        out.push(map);
    }
    Ok(out)
}

async fn run_sqlite_query(path: String, sql: String) -> QueryOutcome {
    let timeout = Duration::from_millis(STATEMENT_TIMEOUT_MS);
    match tokio::time::timeout(
        timeout,
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY);
            let conn = match conn {
                Ok(c) => c,
                Err(e) => {
                    return QueryOutcome::Err {
                        error: e.to_string(),
                    };
                }
            };
            match rows_from_query(&conn, &sql) {
                Ok(rows) => QueryOutcome::Ok { rows },
                Err(e) => QueryOutcome::Err {
                    error: e.to_string(),
                },
            }
        }),
    )
    .await
    {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(_join)) => QueryOutcome::Err {
            error: "The database could not be reached.".to_string(),
        },
        Err(_) => QueryOutcome::Err {
            error: format!("Query timed out after {}s.", STATEMENT_TIMEOUT_MS / 1000),
        },
    }
}

fn redact_dsn(text: &str, dsn: &str) -> String {
    let mut out = text.to_string();
    if let Ok(url) = url::Url::parse(dsn)
        && let Some(pass) = url.password()
        && !pass.is_empty()
    {
        out = out.replace(pass, "[redacted]");
    }
    static KV: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = KV.get_or_init(|| Regex::new(r"(?i)password=([^&\s]+)").expect("dsn kv regex"));
    if let Some(cap) = re.captures(dsn)
        && let Some(m) = cap.get(1)
        && !m.as_str().is_empty()
    {
        out = out.replace(m.as_str(), "[redacted]");
    }
    out
}

async fn query_postgres(dsn: &str, wrapped_sql: &str) -> QueryOutcome {
    let dsn = dsn.to_string();
    let sql = wrapped_sql.to_string();
    let timeout = Duration::from_millis(STATEMENT_TIMEOUT_MS);
    match tokio::time::timeout(timeout, async move {
        let (client, connection) = match tokio_postgres::connect(&dsn, NoTls).await {
            Ok(v) => v,
            Err(e) => {
                return QueryOutcome::Err {
                    error: redact_dsn(&e.to_string(), &dsn),
                };
            }
        };
        tokio::spawn(async move {
            let _ = connection.await;
        });
        if let Err(e) = client
            .batch_execute(&format!("SET statement_timeout = '{STATEMENT_TIMEOUT_MS}'"))
            .await
        {
            return QueryOutcome::Err {
                error: redact_dsn(&e.to_string(), &dsn),
            };
        }
        match client.query(&sql, &[]).await {
            Ok(rows) => {
                let mut out = Vec::new();
                for row in rows {
                    let mut map = HashMap::new();
                    for (i, col) in row.columns().iter().enumerate() {
                        let name = col.name().to_string();
                        let val: Value = match row.try_get::<_, Option<String>>(i) {
                            Ok(Some(s)) => json!(s),
                            Ok(None) => Value::Null,
                            Err(_) => match row.try_get::<_, Option<i64>>(i) {
                                Ok(Some(n)) => json!(n),
                                Ok(None) => Value::Null,
                                Err(_) => match row.try_get::<_, Option<f64>>(i) {
                                    Ok(Some(f)) => json!(f),
                                    Ok(None) => Value::Null,
                                    Err(_) => {
                                        json!(row.try_get::<_, String>(i).unwrap_or_default())
                                    }
                                },
                            },
                        };
                        map.insert(name, val);
                    }
                    out.push(map);
                }
                QueryOutcome::Ok { rows: out }
            }
            Err(e) => QueryOutcome::Err {
                error: redact_dsn(&e.to_string(), &dsn),
            },
        }
    })
    .await
    {
        Ok(outcome) => outcome,
        Err(_) => QueryOutcome::Err {
            error: format!("Query timed out after {}s.", STATEMENT_TIMEOUT_MS / 1000),
        },
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestOutcome {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tables: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

async fn test_sqlite_target(path: &str) -> TestOutcome {
    let path = path.to_string();
    let timeout = Duration::from_millis(STATEMENT_TIMEOUT_MS);
    match tokio::time::timeout(timeout, tokio::task::spawn_blocking(move || {
        let conn = match Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(c) => c,
            Err(e) => {
                return TestOutcome {
                    ok: false,
                    tables: None,
                    error: Some(e.to_string()),
                };
            }
        };
        if conn
            .prepare("SELECT 1")
            .and_then(|mut stmt| stmt.query([]).map(|_| ()))
            .is_err()
        {
            return TestOutcome {
                ok: false,
                tables: None,
                error: Some("The database could not be reached.".to_string()),
            };
        }
        let tables: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .and_then(|mut stmt| {
                let mut names = Vec::new();
                let mut rows = stmt.query([])?;
                while let Some(row) = rows.next()? {
                    names.push(row.get(0)?);
                }
                Ok(names)
            })
            .unwrap_or_default();
        TestOutcome {
            ok: true,
            tables: Some(tables),
            error: None,
        }
    }))
    .await
    {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(_)) => TestOutcome {
            ok: false,
            tables: None,
            error: Some("The database could not be reached.".to_string()),
        },
        Err(_) => TestOutcome {
            ok: false,
            tables: None,
            error: Some(format!(
                "Query timed out after {}s.",
                STATEMENT_TIMEOUT_MS / 1000
            )),
        },
    }
}

async fn test_postgres_target(dsn: &str) -> TestOutcome {
    let dsn = dsn.to_string();
    let timeout = Duration::from_millis(STATEMENT_TIMEOUT_MS);
    match tokio::time::timeout(timeout, async move {
        let (client, connection) = match tokio_postgres::connect(&dsn, NoTls).await {
            Ok(v) => v,
            Err(e) => {
                return TestOutcome {
                    ok: false,
                    tables: None,
                    error: Some(redact_dsn(&e.to_string(), &dsn)),
                };
            }
        };
        tokio::spawn(async move {
            let _ = connection.await;
        });
        if client.query("SELECT 1", &[]).await.is_err() {
            return TestOutcome {
                ok: false,
                tables: None,
                error: Some("The database could not be reached.".to_string()),
            };
        }
        match client
            .query(
                "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public' ORDER BY table_name",
                &[],
            )
            .await
        {
            Ok(rows) => TestOutcome {
                ok: true,
                tables: Some(
                    rows.iter()
                        .filter_map(|r| r.try_get::<_, String>(0).ok())
                        .collect(),
                ),
                error: None,
            },
            Err(e) => TestOutcome {
                ok: false,
                tables: None,
                error: Some(redact_dsn(&e.to_string(), &dsn)),
            },
        }
    })
    .await
    {
        Ok(outcome) => outcome,
        Err(_) => TestOutcome {
            ok: false,
            tables: None,
            error: Some(format!(
                "Query timed out after {}s.",
                STATEMENT_TIMEOUT_MS / 1000
            )),
        },
    }
}

pub async fn test_db_target(db: Arc<Mutex<Db>>, id: &str) -> TestOutcome {
    let work = {
        let guard = db.lock().unwrap_or_else(|e| e.into_inner());
        let target = read_all(&guard).into_iter().find(|t| t.id == id);
        let Some(target) = target else {
            return TestOutcome {
                ok: false,
                tables: None,
                error: Some("No such target.".to_string()),
            };
        };
        match target.kind {
            DbKind::Sqlite => EitherWork::Sqlite(target.path.clone()),
            DbKind::Postgres => {
                let Some(dsn) = decrypt_for_storage(&guard, &target.dsn_encrypted) else {
                    return TestOutcome {
                        ok: false,
                        tables: None,
                        error: Some("No connection string is saved for this target.".to_string()),
                    };
                };
                EitherWork::Postgres(dsn)
            }
        }
    };
    match work {
        EitherWork::Sqlite(path) => test_sqlite_target(&path).await,
        EitherWork::Postgres(dsn) => test_postgres_target(&dsn).await,
    }
}

enum EitherWork {
    Sqlite(String),
    Postgres(String),
}

pub fn database_tool_specs(db: &Db) -> Vec<ToolSpec> {
    if list_db_targets(db).is_empty() {
        vec![]
    } else {
        vec![query_db::spec()]
    }
}

#[derive(Debug, Clone)]
pub struct DatabaseToolResult {
    pub text: String,
    pub is_error: bool,
}

fn format_cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => value.to_string(),
    }
}

fn render_table(rows: &[QueryRow]) -> String {
    let columns: Vec<String> = rows
        .first()
        .map(|r| r.keys().cloned().collect())
        .unwrap_or_default();
    let mut lines = vec![
        columns.join(" | "),
        columns
            .iter()
            .map(|_| "---")
            .collect::<Vec<_>>()
            .join(" | "),
    ];
    for row in rows {
        lines.push(
            columns
                .iter()
                .map(|c| format_cell(row.get(c).unwrap_or(&Value::Null)))
                .collect::<Vec<_>>()
                .join(" | "),
        );
    }
    format!(
        "{}\n\n{} row{}.",
        lines.join("\n"),
        rows.len(),
        if rows.len() == 1 { "" } else { "s" }
    )
}

fn outcome_to_result(outcome: QueryOutcome) -> DatabaseToolResult {
    match outcome {
        QueryOutcome::Err { error } => DatabaseToolResult {
            text: format!("Query failed: {error}"),
            is_error: true,
        },
        QueryOutcome::Ok { rows } if rows.is_empty() => DatabaseToolResult {
            text: "No rows.".to_string(),
            is_error: false,
        },
        QueryOutcome::Ok { rows } => DatabaseToolResult {
            text: render_table(&rows),
            is_error: false,
        },
    }
}

pub async fn run_database_tool(db: Arc<Mutex<Db>>, name: &str, args: &Value) -> DatabaseToolResult {
    if name != "query_db" {
        return DatabaseToolResult {
            text: format!("There is no tool called {name}."),
            is_error: true,
        };
    }

    let target_ref = args
        .get("target")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let sql = args.get("sql").and_then(|v| v.as_str()).unwrap_or("");
    if target_ref.is_empty() {
        return DatabaseToolResult {
            text: "No target was given.".to_string(),
            is_error: true,
        };
    }
    if sql.trim().is_empty() {
        return DatabaseToolResult {
            text: "No SQL was given.".to_string(),
            is_error: true,
        };
    }

    let run = {
        let guard = db.lock().unwrap_or_else(|e| e.into_inner());
        let Some(target) = resolve_db_target(&guard, target_ref) else {
            return DatabaseToolResult {
                text: format!(
                    "No database target called \"{target_ref}\". Check Settings > Computer > Databases."
                ),
                is_error: true,
            };
        };
        if target.tables.is_empty() {
            return DatabaseToolResult {
                text: format!("\"{}\" has no tables on its allow list yet.", target.label),
                is_error: true,
            };
        }

        let requested = args
            .get("limit")
            .and_then(|v| v.as_f64())
            .filter(|n| n.is_finite())
            .map(|n| n.floor() as i64)
            .unwrap_or(DEFAULT_ROW_LIMIT);
        let limit = requested.clamp(1, MAX_ROW_LIMIT);

        let parsed = parse_read_only_select(sql, &target.tables);
        if !parsed.ok {
            return DatabaseToolResult {
                text: format!(
                    "Refused: {}",
                    parsed
                        .reason
                        .unwrap_or_else(|| "not a read-only SELECT".to_string())
                ),
                is_error: true,
            };
        }
        let Some(cleaned) = parsed.cleaned else {
            return DatabaseToolResult {
                text: "Refused: not a read-only SELECT".to_string(),
                is_error: true,
            };
        };

        let wrapped = format!("SELECT * FROM ({cleaned}) AS query_db_capped LIMIT {limit}");
        match target.kind {
            DbKind::Sqlite => RunQuery::Sqlite(target.path.clone(), wrapped),
            DbKind::Postgres => {
                let Some(dsn) = decrypt_for_storage(&guard, &target.dsn_encrypted) else {
                    return DatabaseToolResult {
                        text: "This target's connection string could not be read. Re-enter it in Settings."
                            .to_string(),
                        is_error: true,
                    };
                };
                RunQuery::Postgres(dsn, wrapped)
            }
        }
    };

    let outcome = match run {
        RunQuery::Sqlite(path, wrapped) => run_sqlite_query(path, wrapped).await,
        RunQuery::Postgres(dsn, wrapped) => query_postgres(&dsn, &wrapped).await,
    };
    outcome_to_result(outcome)
}

enum RunQuery {
    Sqlite(String, String),
    Postgres(String, String),
}
