//! S11-01: people tables and adopting the single password into an owner row.
//! Port of `projects/bullpen-night/src/server/users.ts` (tables, `adoptOwner`,
//! `getUser`, `OWNER_ID`). Invites and member CRUD land in S11-03.

use crate::Db;
use crate::auth::password_record;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
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
        );",
    )?;

    add_column(db, "sessions", "user_id", "TEXT")?;
    for table in OWNED_TABLES {
        add_column(db, table, "user_id", "TEXT")?;
    }

    db.conn().execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_bots_user ON bots(user_id);
         CREATE INDEX IF NOT EXISTS idx_attachments_user ON attachments(user_id);",
    )?;
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Invite {
    pub token: String,
    pub created_at: String,
    pub expires_at: String,
    pub used_at: Option<String>,
}

pub fn list_users(db: &Db) -> rusqlite::Result<Vec<User>> {
    adopt_owner(db)?;
    let mut stmt = db.conn().prepare(
        "SELECT id, name, email, role, ceiling_usd, created_at, archived_at FROM users
         ORDER BY archived_at IS NOT NULL, role = 'member', created_at",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(UserRow {
                id: row.get(0)?,
                name: row.get(1)?,
                email: row.get(2)?,
                role: row.get(3)?,
                ceiling_usd: row.get(4)?,
                created_at: row.get(5)?,
                archived_at: row.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows.into_iter().map(to_user).collect())
}

pub fn list_invites(db: &Db, now: DateTime<Utc>) -> rusqlite::Result<Vec<Invite>> {
    let mut stmt = db.conn().prepare(
        "SELECT token, created_at, expires_at, used_at FROM user_invites
         WHERE used_at IS NULL AND expires_at > ?1 ORDER BY created_at DESC",
    )?;
    let rows = stmt
        .query_map([now.to_rfc3339()], |row| {
            Ok(Invite {
                token: row.get(0)?,
                created_at: row.get(1)?,
                expires_at: row.get(2)?,
                used_at: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn random_bytes(n: usize) -> Vec<u8> {
    use uuid::Uuid;
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        out.extend_from_slice(Uuid::new_v4().as_bytes());
    }
    out.truncate(n);
    out
}

pub fn create_invite(db: &Db, now: DateTime<Utc>) -> rusqlite::Result<Invite> {
    let token = URL_SAFE_NO_PAD.encode(random_bytes(32));
    let expires = now + chrono::Duration::days(INVITE_DAYS);
    db.conn().execute(
        "INSERT INTO user_invites (token, created_at, expires_at) VALUES (?1, ?2, ?3)",
        params![token, now.to_rfc3339(), expires.to_rfc3339()],
    )?;
    Ok(Invite {
        token,
        created_at: now.to_rfc3339(),
        expires_at: expires.to_rfc3339(),
        used_at: None,
    })
}

pub fn revoke_invite(db: &Db, token: &str) -> rusqlite::Result<bool> {
    Ok(db.conn().execute(
        "DELETE FROM user_invites WHERE token = ?1 AND used_at IS NULL",
        params![token],
    )? > 0)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

pub fn invite_valid(db: &Db, token: &str, now: DateTime<Utc>) -> rusqlite::Result<bool> {
    if token.is_empty() {
        return Ok(false);
    }
    let presented = token.as_bytes();
    let mut stmt = db
        .conn()
        .prepare("SELECT token, expires_at, used_at FROM user_invites")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (candidate, expires_at, used_at) in rows {
        if !constant_time_eq(candidate.as_bytes(), presented) {
            continue;
        }
        if used_at.is_some() {
            return Ok(false);
        }
        let expires = DateTime::parse_from_rfc3339(&expires_at)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or(now);
        return Ok(expires > now);
    }
    Ok(false)
}

pub fn archive_user(db: &Db, id: &str, now: DateTime<Utc>) -> rusqlite::Result<bool> {
    let user = get_user(db, id)?;
    let Some(user) = user else {
        return Ok(false);
    };
    if user.role == UserRole::Owner || user.archived_at.is_some() {
        return Ok(false);
    }
    db.conn().execute(
        "UPDATE users SET archived_at = ?1 WHERE id = ?2",
        params![now.to_rfc3339(), id],
    )?;
    db.conn()
        .execute("DELETE FROM sessions WHERE user_id = ?1", params![id])?;
    Ok(true)
}

pub fn set_user_ceiling(db: &Db, id: &str, usd: Option<f64>) -> rusqlite::Result<Option<User>> {
    if get_user(db, id)?.is_none() {
        return Ok(None);
    }
    let stored = usd.map(|v| v.max(0.0));
    db.conn().execute(
        "UPDATE users SET ceiling_usd = ?1 WHERE id = ?2",
        params![stored, id],
    )?;
    get_user(db, id)
}

#[derive(Debug)]
pub enum ClaimInviteError {
    Invalid(String),
}

pub fn claim_invite(
    db: &Db,
    token: &str,
    name: &str,
    email: Option<&str>,
    password: &str,
    now: DateTime<Utc>,
) -> Result<User, ClaimInviteError> {
    if !invite_valid(db, token, now).map_err(|e| ClaimInviteError::Invalid(e.to_string()))? {
        return Err(ClaimInviteError::Invalid(
            "That invite link is not valid any more.".to_string(),
        ));
    }

    let tx = db
        .conn()
        .unchecked_transaction()
        .map_err(|e| ClaimInviteError::Invalid(e.to_string()))?;
    let updated = tx
        .execute(
            "UPDATE user_invites SET used_at = ?1 WHERE token = ?2 AND used_at IS NULL",
            params![now.to_rfc3339(), token],
        )
        .map_err(|e| ClaimInviteError::Invalid(e.to_string()))?;
    if updated == 0 {
        return Err(ClaimInviteError::Invalid(
            "That invite link has already been used.".to_string(),
        ));
    }

    let member =
        create_member_in_tx(&tx, name, email, password, now).map_err(ClaimInviteError::Invalid)?;
    tx.execute(
        "UPDATE user_invites SET user_id = ?1 WHERE token = ?2",
        params![member.id, token],
    )
    .map_err(|e| ClaimInviteError::Invalid(e.to_string()))?;
    tx.commit()
        .map_err(|e| ClaimInviteError::Invalid(e.to_string()))?;
    Ok(member)
}

fn create_member_in_tx(
    tx: &rusqlite::Transaction<'_>,
    name: &str,
    email: Option<&str>,
    password: &str,
    now: DateTime<Utc>,
) -> Result<User, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Give the person a name.".to_string());
    }
    let name = if name.len() > 60 {
        name[..60].to_string()
    } else {
        name.to_string()
    };
    if password.len() < 8 {
        return Err("Use a password of at least 8 characters.".to_string());
    }
    let record = crate::auth::hash_new_password(password);
    let id = user_id_for_tx(tx, &name);
    tx.execute(
        "INSERT INTO users (id, name, email, salt, hash, role, ceiling_usd, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'member', NULL, ?6)",
        params![
            id,
            name,
            email.filter(|e| !e.is_empty()),
            record.salt,
            record.hash,
            now.to_rfc3339(),
        ],
    )
    .map_err(|e| e.to_string())?;
    tx.query_row(
        "SELECT id, name, email, role, ceiling_usd, created_at, archived_at FROM users WHERE id = ?1",
        params![id],
        |row| {
            Ok(to_user(UserRow {
                id: row.get(0)?,
                name: row.get(1)?,
                email: row.get(2)?,
                role: row.get(3)?,
                ceiling_usd: row.get(4)?,
                created_at: row.get(5)?,
                archived_at: row.get(6)?,
            }))
        },
    )
    .map_err(|e| e.to_string())
}

fn user_id_for_tx(tx: &rusqlite::Transaction<'_>, name: &str) -> String {
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
    while tx
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

    #[test]
    fn invite_is_single_use() {
        let db = Db::open(":memory:").expect("open");
        set_password(&db, "long-enough-password").expect("password");
        adopt_owner(&db).expect("adopt");
        let now = Utc::now();
        let invite = create_invite(&db, now).expect("mint");
        assert!(invite_valid(&db, &invite.token, now).expect("valid"));

        let member = claim_invite(
            &db,
            &invite.token,
            "Kellie",
            None,
            "member-password-long",
            now,
        )
        .expect("claim");
        assert_eq!(member.role, UserRole::Member);

        assert!(
            !invite_valid(&db, &invite.token, now).expect("check"),
            "used invite must not validate"
        );
        assert!(
            claim_invite(
                &db,
                &invite.token,
                "Someone else",
                None,
                "other-password-long",
                now,
            )
            .is_err(),
            "second claim must fail"
        );
    }
}
