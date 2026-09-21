//! Legacy fixture sessions have NULL `sessions.user_id`; scope must still resolve.

use chrono::{Duration, Utc};
use rusqlite::params;
use server::scope;
use std::fs;
use std::path::PathBuf;
use uuid::Uuid;

const FIXTURE: &str = "d:/rainmade/.scratch/bullpen-rs/fixtures/ts-made.db";

fn fixture_copy() -> store::Db {
    let temp = std::env::temp_dir().join(format!("bullpen_fixture_scope_{}", Uuid::new_v4()));
    fs::copy(PathBuf::from(FIXTURE), &temp).expect("copy fixture");
    store::Db::open(temp.to_str().unwrap()).expect("open db copy")
}

#[test]
fn legacy_session_without_user_id_resolves_owner_scope() {
    let db = fixture_copy();
    let token = "legacy-null-user-id";
    db.conn()
        .execute(
            "INSERT INTO sessions (token, created_at, expires_at) VALUES (?1, ?2, ?3)",
            params![
                token,
                Utc::now().to_rfc3339(),
                (Utc::now() + Duration::days(1)).to_rfc3339()
            ],
        )
        .expect("insert session");
    let scope = scope::scope_for_token(&db, token).expect("scope_for_token");
    assert!(scope.is_owner);
}
