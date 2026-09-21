//! Login/session reads and writes. Schema is migration 13 (`auth_settings`,
//! `sessions`); see `crates/store/src/migrations.rs`. S0-04 only needed
//! `is_configured`/`session_valid` for `/api/auth/status` and
//! `/api/auth/check`; S1-F-05 adds the write half a real login needs:
//! `set_password`/`verify_password` (port of `auth.ts:59-92`,
//! `hashPassword`/`verifyPassword`/`setPassword`) and
//! `create_session`/`destroy_session` (port of `auth.ts:121-147,162-164`).
//!
//! S11-01: `sessions.user_id` is self-created by `users::ensure_user_tables`.
//! NULL means the owner everywhere (`users::adopt_owner` backfills on first
//! open). Member sessions set an explicit id in S11-03 invite claim.

use crate::Db;
use chrono::{DateTime, Duration, Utc};
use rusqlite::{OptionalExtension, params};
use scrypt::Params;

/// scrypt parameters. `log_n = 15` is `N = 2^15 = 32768`, the same cost TS's
/// `SCRYPT_N` uses - about 100ms per hash, expensive enough that an offline
/// guessing attack against a stolen row is slow.
const SCRYPT_LOG_N: u8 = 15;
const SCRYPT_R: u32 = 8;
const SCRYPT_P: u32 = 1;
const KEY_LENGTH: usize = 32;

/// A session lasts this long without being used. Mirrors the TS
/// `SESSION_DAYS`.
const SESSION_DAYS: i64 = 60;

fn scrypt_params() -> Params {
    Params::new(SCRYPT_LOG_N, SCRYPT_R, SCRYPT_P, KEY_LENGTH)
        .expect("fixed scrypt params are valid")
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// `n` bytes of randomness, built out of `uuid::Uuid::new_v4()` (already a
/// `store` dependency, used for every id in this crate) rather than pulling
/// in a second new dependency (`rand`/`getrandom`) alongside `scrypt` just
/// for this. A UUIDv4 has ~122 random bits - a few bits short of "fully
/// random 128 bits" because the version/variant nibbles are fixed - which is
/// plenty for a salt (needs uniqueness, not secrecy) and a session token
/// (guessed offline, not brute-forced live; `ATTEMPT_LIMIT`-style throttling
/// lives on the login route, not the token itself).
fn random_bytes(n: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(n);
    while bytes.len() < n {
        bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    bytes.truncate(n);
    bytes
}

/// Byte-for-byte equal, without branching on the first differing byte - a
/// `==` on a hash leaks how many leading bytes matched through timing, which
/// over enough attempts recovers the hash (the reason TS uses
/// `timingSafeEqual`). No `subtle`/`constant_time_eq` crate: this is the
/// same accumulate-with-OR shape those crates use, without a third new
/// dependency.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// A stored password: hex-encoded salt and scrypt hash, same shape as the
/// `auth_settings` row. Mirrors the TS `PasswordRecord`.
pub struct PasswordRecord {
    pub salt: String,
    pub hash: String,
}

/// Hashes a password for storage. Never store, log or return the plaintext.
/// Mirrors the TS `hashPassword`.
pub fn hash_new_password(password: &str) -> PasswordRecord {
    hash_password(password)
}

fn hash_password(password: &str) -> PasswordRecord {
    let salt = random_bytes(16);
    let mut output = [0u8; KEY_LENGTH];
    scrypt::scrypt(password.as_bytes(), &salt, &scrypt_params(), &mut output)
        .expect("fixed-size scrypt output buffer matches KEY_LENGTH");
    PasswordRecord {
        salt: hex_encode(&salt),
        hash: hex_encode(&output),
    }
}

/// Constant-time check of `password` against a stored `record`. Mirrors the
/// TS `verifyPassword`.
pub fn verify_password(password: &str, record: &PasswordRecord) -> bool {
    let Some(salt) = hex_decode(&record.salt) else {
        return false;
    };
    let Some(expected) = hex_decode(&record.hash) else {
        return false;
    };
    if expected.len() != KEY_LENGTH {
        return false;
    }
    let mut actual = [0u8; KEY_LENGTH];
    if scrypt::scrypt(password.as_bytes(), &salt, &scrypt_params(), &mut actual).is_err() {
        return false;
    }
    constant_time_eq(&actual, &expected)
}

/// The stored password record, or `None` before one has ever been set.
/// Mirrors the TS `passwordRecord`.
pub fn password_record(db: &Db) -> rusqlite::Result<Option<PasswordRecord>> {
    db.conn()
        .query_row(
            "SELECT salt, hash FROM auth_settings WHERE id = 1",
            [],
            |row| {
                Ok(PasswordRecord {
                    salt: row.get(0)?,
                    hash: row.get(1)?,
                })
            },
        )
        .optional()
}

/// Sets (or replaces) the one password. Mirrors the TS `setPassword`:
/// changing the password ends every session, because "change the password"
/// has to mean "lock out whoever is already in".
pub fn set_password(db: &Db, password: &str) -> rusqlite::Result<()> {
    let record = hash_password(password);
    db.conn().execute(
        "INSERT INTO auth_settings (id, salt, hash, updated_at) VALUES (1, ?1, ?2, ?3)
         ON CONFLICT(id) DO UPDATE SET salt = excluded.salt, hash = excluded.hash,
           updated_at = excluded.updated_at",
        params![record.salt, record.hash, Utc::now().to_rfc3339()],
    )?;
    db.conn().execute("DELETE FROM sessions", [])?;
    Ok(())
}

/// Same settings key the TS `LAST_LOGIN_KEY` uses (`auth.ts:149`) - what
/// S5-03's absence pause (`server::routines::fire_due`) reads via
/// `Db::settings_get` to decide whether an unattended interval routine has
/// been running into silence for `ABSENCE_DAYS`.
pub const LAST_LOGIN_KEY: &str = "auth.last_login_at";

/// Options for `create_session`. Mirrors TS `createSession`'s second argument.
#[derive(Debug, Clone, Default)]
pub struct CreateSessionOpts {
    pub user_id: Option<String>,
    /// When false, do not stamp `LAST_LOGIN_KEY` (member invite claim in S11-03).
    pub stamp_last_login: bool,
}

impl CreateSessionOpts {
    pub fn owner_sign_in(user_id: String) -> Self {
        Self {
            user_id: Some(user_id),
            stamp_last_login: true,
        }
    }
}

/// Mints a new session token and writes its row. Mirrors the TS `createSession`.
pub fn create_session(db: &Db) -> rusqlite::Result<String> {
    create_session_with(db, CreateSessionOpts::default())
}

pub fn create_session_with(db: &Db, opts: CreateSessionOpts) -> rusqlite::Result<String> {
    let token = hex_encode(&random_bytes(32));
    let now = Utc::now();
    let expires = now + Duration::days(SESSION_DAYS);
    db.conn().execute(
        "INSERT INTO sessions (token, created_at, expires_at, user_id) VALUES (?1, ?2, ?3, ?4)",
        params![token, now.to_rfc3339(), expires.to_rfc3339(), opts.user_id],
    )?;
    if opts.stamp_last_login {
        db.settings_set(LAST_LOGIN_KEY, &now.to_rfc3339())?;
    }
    Ok(token)
}

/// The user id stored on a session row, if any. Does not validate expiry.
pub fn session_user_id(db: &Db, token: &str) -> rusqlite::Result<Option<String>> {
    if token.is_empty() {
        return Ok(None);
    }
    let row = db
        .conn()
        .query_row(
            "SELECT user_id FROM sessions WHERE token = ?1",
            params![token],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?;
    Ok(row.flatten())
}

/// Ends one session (logout). A no-op if `token` names no row. Mirrors the
/// TS `destroySession`.
pub fn destroy_session(db: &Db, token: &str) -> rusqlite::Result<()> {
    db.conn()
        .execute("DELETE FROM sessions WHERE token = ?1", params![token])?;
    Ok(())
}

/// True once a password has been set: `auth_settings` carries its single row
/// (`id = 1`, enforced by a CHECK constraint - there is never more than one).
pub fn is_configured(db: &Db) -> rusqlite::Result<bool> {
    let row: Option<i64> = db
        .conn()
        .query_row("SELECT 1 FROM auth_settings WHERE id = 1", [], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(row.is_some())
}

/// True when `token` names a session that has not expired. Mirrors the TS
/// `sessionValid`: an expired row is deleted on the way out so the table
/// does not grow forever. An empty token is never valid and is never queried.
pub fn session_valid(db: &Db, token: &str) -> rusqlite::Result<bool> {
    if token.is_empty() {
        return Ok(false);
    }

    let expires_at: Option<String> = db
        .conn()
        .query_row(
            "SELECT expires_at FROM sessions WHERE token = ?1",
            params![token],
            |row| row.get(0),
        )
        .optional()?;

    let Some(expires_at) = expires_at else {
        return Ok(false);
    };

    let expired = DateTime::parse_from_rfc3339(&expires_at)
        .map(|dt| dt <= Utc::now())
        .unwrap_or(true); // an unparsable timestamp is never valid

    if expired {
        db.conn()
            .execute("DELETE FROM sessions WHERE token = ?1", params![token])?;
        return Ok(false);
    }

    Ok(true)
}
