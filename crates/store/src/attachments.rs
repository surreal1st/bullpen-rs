//! Attachment records and on-disk bytes. Port of `attachments.ts`.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

use crate::Db;
use crate::list_scope::ListScope;

pub const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub content_type: String,
    pub bytes: i64,
    pub created_at: String,
}

pub struct StoreAttachmentInput<'a> {
    pub name: &'a str,
    pub content_type: &'a str,
    pub data: &'a [u8],
    pub bot_id: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub user_id: Option<&'a str>,
}

pub enum StoreAttachmentError {
    Empty,
    TooLarge { bytes: usize },
    Store(rusqlite::Error),
}

impl From<rusqlite::Error> for StoreAttachmentError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Store(err)
    }
}

pub fn ensure_library_tables(db: &Db) -> rusqlite::Result<()> {
    add_column_if_missing(db, "attachments", "bot_id", "TEXT")?;
    add_column_if_missing(db, "attachments", "kind", "TEXT NOT NULL DEFAULT 'file'")?;

    let exists: Option<String> = db
        .conn()
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'attachment_fts'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if exists.is_none() {
        db.conn().execute_batch(
            "
            CREATE VIRTUAL TABLE attachment_fts USING fts5(name, content='attachments', content_rowid='rowid');

            CREATE TRIGGER attachments_ai AFTER INSERT ON attachments BEGIN
              INSERT INTO attachment_fts(rowid, name) VALUES (new.rowid, new.name);
            END;

            CREATE TRIGGER attachments_ad AFTER DELETE ON attachments BEGIN
              INSERT INTO attachment_fts(attachment_fts, rowid, name) VALUES ('delete', old.rowid, old.name);
            END;

            CREATE TRIGGER attachments_au AFTER UPDATE ON attachments BEGIN
              INSERT INTO attachment_fts(attachment_fts, rowid, name) VALUES ('delete', old.rowid, old.name);
              INSERT INTO attachment_fts(rowid, name) VALUES (new.rowid, new.name);
            END;

            INSERT INTO attachment_fts(rowid, name) SELECT rowid, name FROM attachments;
            ",
        )?;
    }
    Ok(())
}

fn add_column_if_missing(
    db: &Db,
    table: &str,
    column: &str,
    col_type: &str,
) -> rusqlite::Result<()> {
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

fn attachments_dir(data_dir: &str) -> PathBuf {
    Path::new(data_dir).join("attachments")
}

pub fn store_attachment(
    db: &Db,
    data_dir: &str,
    input: StoreAttachmentInput<'_>,
) -> Result<Attachment, StoreAttachmentError> {
    if input.data.is_empty() {
        return Err(StoreAttachmentError::Empty);
    }
    if input.data.len() > MAX_ATTACHMENT_BYTES {
        return Err(StoreAttachmentError::TooLarge {
            bytes: input.data.len(),
        });
    }

    let id = Uuid::new_v4().to_string();
    let name = trim_name(input.name);
    let content_type = if input.content_type.trim().is_empty() {
        "application/octet-stream".to_string()
    } else {
        input.content_type.to_string()
    };
    let created_at = Utc::now().to_rfc3339();
    let kind = input.kind.unwrap_or("file");
    let bytes = input.data.len() as i64;

    let dir = attachments_dir(data_dir);
    fs::create_dir_all(&dir).map_err(|e| rusqlite::Error::InvalidParameterName(e.to_string()))?;
    fs::write(dir.join(&id), input.data)
        .map_err(|e| rusqlite::Error::InvalidParameterName(e.to_string()))?;

    db.conn().execute(
        "INSERT INTO attachments (id, name, content_type, bytes, created_at, bot_id, kind, user_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            id,
            name,
            content_type,
            bytes,
            created_at,
            input.bot_id,
            kind,
            input.user_id,
        ],
    )?;

    Ok(Attachment {
        id,
        name: name.to_string(),
        content_type,
        bytes,
        created_at,
    })
}

fn trim_name(name: &str) -> &str {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        "file"
    } else {
        &trimmed[..trimmed.len().min(200)]
    }
}

pub fn get_attachment(db: &Db, id: &str) -> rusqlite::Result<Option<Attachment>> {
    db.conn()
        .query_row(
            "SELECT id, name, content_type, bytes, created_at FROM attachments WHERE id = ?1",
            params![id],
            |row| {
                Ok(Attachment {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    content_type: row.get(2)?,
                    bytes: row.get(3)?,
                    created_at: row.get(4)?,
                })
            },
        )
        .optional()
}

pub fn read_attachment(data_dir: &str, id: &str) -> std::io::Result<Vec<u8>> {
    fs::read(attachments_dir(data_dir).join(id))
}

pub fn attachment_exists(data_dir: &str, id: &str) -> bool {
    fs::metadata(attachments_dir(data_dir).join(id))
        .map(|m| m.len() > 0)
        .unwrap_or(false)
}

pub fn delete_attachment(db: &Db, data_dir: &str, id: &str) -> rusqlite::Result<bool> {
    if get_attachment(db, id)?.is_none() {
        return Ok(false);
    }
    let path = attachments_dir(data_dir).join(id);
    let _ = fs::remove_file(path);
    db.conn()
        .execute("DELETE FROM attachments WHERE id = ?1", params![id])?;
    Ok(true)
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryItem {
    pub id: String,
    pub name: String,
    pub content_type: String,
    pub bytes: i64,
    pub created_at: String,
    pub kind: String,
    pub bot_id: Option<String>,
    pub bot_name: Option<String>,
    pub message_id: Option<String>,
}

#[derive(Debug, Default)]
pub struct LibraryQuery<'a> {
    pub q: Option<&'a str>,
    pub bot: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub limit: Option<usize>,
    pub scope: Option<ListScope>,
}

pub fn list_library(db: &Db, options: LibraryQuery<'_>) -> rusqlite::Result<Vec<LibraryItem>> {
    let limit = options.limit.unwrap_or(200).clamp(1, 500);
    let q = options.q.unwrap_or("").trim();
    let bot = options.bot.unwrap_or("").trim();
    let kind = options.kind.unwrap_or("").trim();

    let row_select = "a.id, a.name, a.content_type, a.bytes, a.created_at, a.kind, a.bot_id,
        b.name AS bot_name,
        (SELECT m.id FROM messages m WHERE m.attachment_id = a.id ORDER BY m.created_at LIMIT 1) AS message_id";

    let mut conditions: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if let Some(scope) = &options.scope {
        conditions.push("COALESCE(a.user_id, ?) = ?".to_string());
        let (owner, user) = scope.bind_values();
        params.push(Box::new(owner.to_string()));
        params.push(Box::new(user.to_string()));
    }
    if !bot.is_empty() {
        conditions.push("a.bot_id = ?".to_string());
        params.push(Box::new(bot.to_string()));
    }
    if !kind.is_empty() {
        conditions.push("a.kind = ?".to_string());
        params.push(Box::new(kind.to_string()));
    }

    if !q.is_empty() {
        let terms = library_terms(q);
        if terms.is_empty() {
            return Ok(vec![]);
        }
        conditions.push("attachment_fts MATCH ?".to_string());
        params.push(Box::new(
            terms
                .iter()
                .map(|t| format!("{t}*"))
                .collect::<Vec<_>>()
                .join(" OR "),
        ));
        params.push(Box::new(limit));

        let where_sql = conditions.join(" AND ");
        let sql = format!(
            "SELECT {row_select}
             FROM attachment_fts f
             JOIN attachments a ON a.rowid = f.rowid
             LEFT JOIN bots b ON b.id = a.bot_id
             WHERE {where_sql}
             ORDER BY a.created_at DESC
             LIMIT ?"
        );
        return query_library_rows(db, &sql, &params);
    }

    params.push(Box::new(limit));
    let where_sql = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };
    let sql = format!(
        "SELECT {row_select}
         FROM attachments a
         LEFT JOIN bots b ON b.id = a.bot_id
         {where_sql}
         ORDER BY a.created_at DESC
         LIMIT ?"
    );
    query_library_rows(db, &sql, &params)
}

fn library_terms(query: &str) -> Vec<String> {
    query
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() > 1)
        .take(16)
        .map(|s| s.to_string())
        .collect()
}

fn query_library_rows(
    db: &Db,
    sql: &str,
    params: &[Box<dyn rusqlite::types::ToSql>],
) -> rusqlite::Result<Vec<LibraryItem>> {
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = db.conn().prepare(sql)?;
    let rows = stmt.query_map(param_refs.as_slice(), map_library_row)?;
    rows.collect()
}

fn map_library_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LibraryItem> {
    Ok(LibraryItem {
        id: row.get(0)?,
        name: row.get(1)?,
        content_type: row.get(2)?,
        bytes: row.get(3)?,
        created_at: row.get(4)?,
        kind: row.get(5)?,
        bot_id: row.get(6)?,
        bot_name: row.get(7)?,
        message_id: row.get(8)?,
    })
}
