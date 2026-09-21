//! S11-01: people tables and adopting the single password into an owner row.
//! Port of `projects/bullpen-night/src/server/users.ts` (tables, `adoptOwner`,
//! `getUser`, `OWNER_ID`). Invites and member CRUD land in S11-03.

use crate::Db;
use crate::auth::password_record;
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// The id the adopted single-password account gets. TS `OWNER_ID`.
pub const OWNER_ID: &str = "josh";

/// Invite lifetime in days (stored rows; claim logic in S11-03).
pub const INVITE_DAYS: i64 = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserRole {
    Owner,
    Member,
}

impl UserRole {
    fn from_db(value: &str) -> Self {
        if value == "owner" {
            UserRole::Owner
        } else {
            UserRole::Member
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: String,
    pub name: String,
    pub email: Option<String>,
    pub role: UserRole,
    pub ceiling_usd: Option<f64>,
    pub created_at: String,
    pub archived_at: Option<String>,
}

const OWNED_TABLES: &[&str] = &["bots", "sections", "attachments"];

/// Self-creating tables and `user_id` columns — same pattern as TS `ensureUserTables`.
pub fn ensure_user_tables(db: &Db) -> rusqlite::Result<()> {
    db.conn().execute_batch(
        "CREATE TABLE IF NOT EXISTS users (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            email       TEXT,
            salt        TEXT NOT NULL,
            hash        TEXT NOT NULL,
            role        TEXT NOT NULL CHECK (role IN ('owner','member')),
            ceiling_usd REAL,
            created_at  TEXT NOT NULL,
            archived_at TEXT
        );

        CREATE TABLE IF NOT EXISTS user_invites (
            token      TEXT PRIMARY KEY,
            created_at TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            used_at    TEXT,
            user_id    TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_bots_user ON bots(user_id);
        CREATE INDEX IF NOT EXISTS idx_attachments_user ON attachments(user_id);",
    )?;

    add_column(db, "sessions", "user_id", "TEXT")?;
    for table in OWNED_TABLES {
        add_column(db, table, "user_id", "TEXT")?;
    }
    Ok(())
}

fn add_column(db: &Db, table: &str, column: &str, col_type: &str) -> rusqlite::Result<()> {
    let exists = {
        let mut stmt = db.conn().prepare(&format!("PRAGMA table_info({table})"))?;
        stmt.query_map([], |row| row.get::<_, String>(1))?
            .filter_map(Result::ok)
            .any(|name| name == column)
    };
    if !exists {
        db.conn().execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {col_type}"
        ))?;
    }
    Ok(())
}

/// Turns the single password into the owner account, once. Idempotent.
pub fn adopt_owner(db: &Db) -> rusqlite::Result<Option<String>> {
    adopt_owner_at(db, Utc::now())
}

pub fn adopt_owner_at(db: &Db, now: DateTime<Utc>) -> rusqlite::Result<Option<String>> {
    ensure_user_tables(db)?;

    let existing: Option<String> = db
        .conn()
        .query_row(
            "SELECT id FROM users WHERE role = 'owner' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok(Some(id));
    }

    let Some(record) = password_record(db)? else {
        return Ok(None);
    };

    db.conn().execute(
        "INSERT INTO users (id, name, email, salt, hash, role, ceiling_usd, created_at)
         VALUES (?1, ?2, NULL, ?3, ?4, 'owner', NULL, ?5)",
        params![
            OWNER_ID,
            "Josh Johnson",
            record.salt,
            record.hash,
            now.to_rfc3339(),
        ],
    )?;

    db.conn().execute(
        "UPDATE sessions SET user_id = ?1 WHERE user_id IS NULL",
        params![OWNER_ID],
    )?;
    for table in OWNED_TABLES {
        db.conn().execute(
            &format!("UPDATE {table} SET user_id = ?1 WHERE user_id IS NULL"),
            params![OWNER_ID],
        )?;
    }

    Ok(Some(OWNER_ID.to_string()))
}

pub fn owner_id(db: &Db) -> rusqlite::Result<Option<String>> {
    adopt_owner(db)
}

struct UserRow {
    id: String,
    name: String,
    email: Option<String>,
    role: String,
    ceiling_usd: Option<f64>,
    created_at: String,
    archived_at: Option<String>,
}

fn to_user(row: UserRow) -> User {
    User {
        id: row.id,
        name: row.name,
        email: row.email,
        role: UserRole::from_db(&row.role),
        ceiling_usd: row.ceiling_usd,
        created_at: row.created_at,
        archived_at: row.archived_at,
    }
}

/// Stamps a root-owned row at creation time (S11-02). Missed stamps still read
/// as the owner's via `COALESCE`, but the column should say who created it.
pub fn stamp_owned_root(db: &Db, table: &str, id: &str, user_id: &str) -> rusqlite::Result<()> {
    let sql = match table {
        "bots" => "UPDATE bots SET user_id = ?1 WHERE id = ?2",
        "sections" => "UPDATE sections SET user_id = ?1 WHERE id = ?2",
        "attachments" => "UPDATE attachments SET user_id = ?1 WHERE id = ?2",
        _ => return Ok(()),
    };
    db.conn().execute(sql, params![user_id, id])?;
    Ok(())
}

/// Creates a member account (S11-03 invite claim will reuse this shape).
pub fn create_member(
    db: &Db,
    name: &str,
    password: &str,
    email: Option<&str>,
) -> Result<User, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Give the person a name.".to_string());
    }
    if password.len() < 8 {
        return Err("Use a password of at least 8 characters.".to_string());
    }
    let record = crate::auth::hash_new_password(password);
    let id = user_id_for(db, name);
    let now = Utc::now().to_rfc3339();
    db.conn()
        .execute(
            "INSERT INTO users (id, name, email, salt, hash, role, ceiling_usd, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'member', NULL, ?6)",
            params![
                id,
                name,
                email.filter(|e| !e.is_empty()),
                record.salt,
                record.hash,
                now,
            ],
        )
        .map_err(|e| e.to_string())?;
    get_user(db, &id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "member row missing after insert".to_string())
}

fn user_id_for(db: &Db, name: &str) -> String {
    let base: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(40)
        .collect();
    let base = if base.is_empty() {
        "member".to_string()
    } else {
        base
    };
    let mut candidate = base.clone();
    let mut n = 2i32;
    while db
        .conn()
        .query_row(
            "SELECT 1 FROM users WHERE id = ?1",
            params![candidate],
            |_| Ok(()),
        )
        .is_ok()
    {
        candidate = format!("{base}-{n}");
        n += 1;
    }
    candidate
}

pub fn get_user(db: &Db, id: &str) -> rusqlite::Result<Option<User>> {
    let row = db
        .conn()
        .query_row(
            "SELECT id, name, email, role, ceiling_usd, created_at, archived_at
             FROM users WHERE id = ?1",
            params![id],
            |row| {
                Ok(UserRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    email: row.get(2)?,
                    role: row.get(3)?,
                    ceiling_usd: row.get(4)?,
                    created_at: row.get(5)?,
                    archived_at: row.get(6)?,
                })
            },
        )
        .optional()?;
    Ok(row.map(to_user))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;
    use crate::auth::{create_session, session_valid, set_password};

    #[test]
    fn adopt_owner_copies_password_and_backfills_sessions() {
        let db = Db::open(":memory:").expect("open");
        set_password(&db, "long-enough-password").expect("password");
        let token = create_session(&db).expect("session before adopt");

        let owner = adopt_owner(&db).expect("adopt").expect("owner id");
        assert_eq!(owner, OWNER_ID);

        let user = get_user(&db, OWNER_ID).expect("get").expect("row");
        assert_eq!(user.name, "Josh Johnson");
        assert_eq!(user.role, UserRole::Owner);

        let session_user: Option<String> = db
            .conn()
            .query_row(
                "SELECT user_id FROM sessions WHERE token = ?1",
                params![token],
                |row| row.get(0),
            )
            .expect("session row");
        assert_eq!(session_user.as_deref(), Some(OWNER_ID));

        assert!(session_valid(&db, &token).expect("valid"));
    }

    #[test]
    fn adopt_owner_is_idempotent() {
        let db = Db::open(":memory:").expect("open");
        set_password(&db, "long-enough-password").expect("password");
        let first = adopt_owner(&db).expect("first").expect("id");
        let second = adopt_owner(&db).expect("second").expect("id");
        assert_eq!(first, second);
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM users WHERE role = 'owner'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(count, 1);
    }
}
