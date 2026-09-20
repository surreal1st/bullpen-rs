//! S7-01: connector registry — port of DB helpers in
//! `projects/bullpen-night/src/server/mcp.ts` (read-only reference).
//!
//! OAuth rows (`connector_auth`, `oauth_flows`) are touched in S7-03; removal
//! here matches TS `removeConnector` (bot links + connector row only).

use crate::Db;
use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

struct ConnectorRow {
    id: String,
    name: String,
    url: String,
    auth_header: Option<String>,
    created_at: String,
}

fn row_from_sql(row: &rusqlite::Row) -> rusqlite::Result<ConnectorRow> {
    Ok(ConnectorRow {
        id: row.get(0)?,
        name: row.get(1)?,
        url: row.get(2)?,
        auth_header: row.get(3)?,
        created_at: row.get(4)?,
    })
}

/// Full row including optional static credential — server-only.
#[derive(Clone, Debug)]
pub struct ConnectorFull {
    pub id: String,
    pub name: String,
    pub url: String,
    pub auth_header: Option<String>,
    pub created_at: String,
}

/// JSON-safe connector (no secrets).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Connector {
    pub id: String,
    pub name: String,
    pub url: String,
    pub created_at: String,
}

fn to_public(row: ConnectorRow) -> Connector {
    Connector {
        id: row.id,
        name: row.name,
        url: row.url,
        created_at: row.created_at,
    }
}

pub fn strip_secret(full: &ConnectorFull) -> Connector {
    Connector {
        id: full.id.clone(),
        name: full.name.clone(),
        url: full.url.clone(),
        created_at: full.created_at.clone(),
    }
}

pub fn list_connectors(db: &Db) -> rusqlite::Result<Vec<Connector>> {
    let mut stmt = db
        .conn()
        .prepare("SELECT id, name, url, auth_header, created_at FROM connectors ORDER BY name")?;
    let rows = stmt
        .query_map([], row_from_sql)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows.into_iter().map(to_public).collect())
}

pub fn get_connector(db: &Db, id: &str) -> rusqlite::Result<Option<ConnectorFull>> {
    db.conn()
        .query_row(
            "SELECT id, name, url, auth_header, created_at FROM connectors WHERE id = ?1",
            params![id],
            row_from_sql,
        )
        .optional()
        .map(|opt| {
            opt.map(|row| ConnectorFull {
                id: row.id,
                name: row.name,
                url: row.url,
                auth_header: row.auth_header,
                created_at: row.created_at,
            })
        })
}

pub struct AddConnectorResult {
    pub ok: bool,
    pub connector: Option<Connector>,
    pub error: Option<String>,
}

pub fn add_connector(
    db: &Db,
    name: &str,
    url: &str,
    auth_header: Option<&str>,
) -> rusqlite::Result<AddConnectorResult> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(AddConnectorResult {
            ok: false,
            connector: None,
            error: Some("Give the connector a name.".to_string()),
        });
    }

    let url_string = match normalize_http_url(url) {
        Ok(u) => u,
        Err(msg) => {
            return Ok(AddConnectorResult {
                ok: false,
                connector: None,
                error: Some(msg),
            });
        }
    };

    let id = Uuid::new_v4().to_string();
    let created_at = Utc::now().to_rfc3339();
    db.conn().execute(
        "INSERT INTO connectors (id, name, url, auth_header, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, name, url_string, auth_header, created_at],
    )?;

    let full = get_connector(db, &id)?.expect("just inserted connector");
    Ok(AddConnectorResult {
        ok: true,
        connector: Some(strip_secret(&full)),
        error: None,
    })
}

pub fn remove_connector(db: &Db, id: &str) -> rusqlite::Result<bool> {
    if get_connector(db, id)?.is_none() {
        return Ok(false);
    }
    let conn = db.conn();
    conn.execute(
        "DELETE FROM bot_connectors WHERE connector_id = ?1",
        params![id],
    )?;
    conn.execute("DELETE FROM connectors WHERE id = ?1", params![id])?;
    Ok(true)
}

pub fn connectors_for_bot(db: &Db, bot_id: &str) -> rusqlite::Result<Vec<ConnectorFull>> {
    let mut stmt = db.conn().prepare(
        "SELECT c.id, c.name, c.url, c.auth_header, c.created_at
           FROM connectors c
           JOIN bot_connectors bc ON bc.connector_id = c.id
          WHERE bc.bot_id = ?1
          ORDER BY c.name",
    )?;
    let rows = stmt
        .query_map(params![bot_id], row_from_sql)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .map(|row| ConnectorFull {
            id: row.id,
            name: row.name,
            url: row.url,
            auth_header: row.auth_header,
            created_at: row.created_at,
        })
        .collect())
}

fn normalize_http_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    let (scheme, rest) = if let Some(r) = trimmed.strip_prefix("https://") {
        ("https", r)
    } else if let Some(r) = trimmed.strip_prefix("http://") {
        ("http", r)
    } else {
        return Err(format!("Not a URL: {raw}"));
    };
    if rest.is_empty() || rest.contains(char::is_whitespace) {
        return Err(format!("Not a URL: {raw}"));
    }
    Ok(format!("{scheme}://{rest}"))
}

pub fn set_bot_connector(
    db: &Db,
    bot_id: &str,
    connector_id: &str,
    enabled: bool,
) -> rusqlite::Result<()> {
    if enabled {
        db.conn().execute(
            "INSERT OR IGNORE INTO bot_connectors (bot_id, connector_id) VALUES (?1, ?2)",
            params![bot_id, connector_id],
        )?;
    } else {
        db.conn().execute(
            "DELETE FROM bot_connectors WHERE bot_id = ?1 AND connector_id = ?2",
            params![bot_id, connector_id],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;

    #[test]
    fn add_and_remove_connector_clears_bot_links() {
        let db = Db::open(":memory:").unwrap();
        db.conn()
            .execute(
                "INSERT INTO bots (id, name, purpose, instructions, model, created_at)
                 VALUES ('b1', 'Bot', '', '', NULL, '2020-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        let added = add_connector(
            &db,
            "Weather",
            "https://mcp.example.com/weather",
            Some("Bearer x"),
        )
        .unwrap();
        assert!(added.ok);
        let id = added.connector.unwrap().id;
        set_bot_connector(&db, "b1", &id, true).unwrap();
        assert_eq!(connectors_for_bot(&db, "b1").unwrap().len(), 1);
        assert!(remove_connector(&db, &id).unwrap());
        assert!(get_connector(&db, &id).unwrap().is_none());
        assert!(connectors_for_bot(&db, "b1").unwrap().is_empty());
    }
}
