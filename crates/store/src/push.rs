//! S11-06: APNs device registry. Port of `push.ts` table + register/forget/list.

use crate::Db;
use rusqlite::params;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushEnvironment {
    Sandbox,
    Production,
}

impl PushEnvironment {
    pub fn as_str(self) -> &'static str {
        match self {
            PushEnvironment::Sandbox => "sandbox",
            PushEnvironment::Production => "production",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "sandbox" => Some(PushEnvironment::Sandbox),
            "production" => Some(PushEnvironment::Production),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushDevice {
    pub token: String,
    pub environment: PushEnvironment,
    pub created_at: String,
    pub last_seen_at: String,
}

pub fn ensure_push_table(db: &Db) -> rusqlite::Result<()> {
    db.conn().execute_batch(
        "CREATE TABLE IF NOT EXISTS push_devices (
            token        TEXT PRIMARY KEY,
            environment  TEXT NOT NULL,
            created_at   TEXT NOT NULL,
            last_seen_at TEXT NOT NULL
        );",
    )
}

pub fn register_device(db: &Db, token: &str, environment: PushEnvironment) -> bool {
    let clean = token.trim();
    if !is_valid_token(clean) {
        return false;
    }
    let now = chrono::Utc::now().to_rfc3339();
    let lower = clean.to_ascii_lowercase();
    db.conn()
        .execute(
            "INSERT INTO push_devices (token, environment, created_at, last_seen_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(token) DO UPDATE SET environment = excluded.environment,
                                              last_seen_at = excluded.last_seen_at",
            params![lower, environment.as_str(), now, now],
        )
        .is_ok()
}

pub fn forget_device(db: &Db, token: &str) {
    let _ = db.conn().execute(
        "DELETE FROM push_devices WHERE token = ?1",
        params![token.trim().to_ascii_lowercase()],
    );
}

pub fn list_devices(db: &Db) -> rusqlite::Result<Vec<PushDevice>> {
    let mut stmt = db
        .conn()
        .prepare("SELECT token, environment, created_at, last_seen_at FROM push_devices ORDER BY last_seen_at DESC")?;
    let rows = stmt
        .query_map([], |row| {
            let env: String = row.get(1)?;
            Ok(PushDevice {
                token: row.get(0)?,
                environment: PushEnvironment::parse(&env).unwrap_or(PushEnvironment::Production),
                created_at: row.get(2)?,
                last_seen_at: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn is_valid_token(token: &str) -> bool {
    token.len() >= 64 && token.len() <= 200 && token.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_tokens() {
        let db = Db::open(":memory:").expect("open");
        assert!(!register_device(
            &db,
            "not-a-token",
            PushEnvironment::Sandbox
        ));
        assert!(!register_device(&db, "", PushEnvironment::Sandbox));
        assert!(list_devices(&db).unwrap().is_empty());
    }

    #[test]
    fn normalizes_case_and_updates_environment() {
        let db = Db::open(":memory:").expect("open");
        let token = "a".repeat(64);
        assert!(register_device(
            &db,
            &token.to_uppercase(),
            PushEnvironment::Production
        ));
        assert!(register_device(&db, &token, PushEnvironment::Sandbox));
        let devices = list_devices(&db).expect("list");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].token, token);
        assert_eq!(devices[0].environment, PushEnvironment::Sandbox);
    }
}
