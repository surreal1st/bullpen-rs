//! S7-03: OAuth storage for connectors — port of `oauth.ts` storage section.

use crate::Db;
use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorAuth {
    pub connector_id: String,
    pub issuer: String,
    pub authorize_url: String,
    pub token_url: String,
    pub resource: String,
    pub client_id: String,
    #[serde(skip_serializing)]
    pub client_secret: Option<String>,
    #[serde(skip_serializing)]
    pub access_token: Option<String>,
    #[serde(skip_serializing)]
    pub refresh_token: Option<String>,
    pub expires_at: Option<String>,
    pub scope: Option<String>,
    pub connected_at: Option<String>,
}

struct AuthRow {
    connector_id: String,
    issuer: String,
    authorize_url: String,
    token_url: String,
    resource: String,
    client_id: String,
    client_secret: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_at: Option<String>,
    scope: Option<String>,
    connected_at: Option<String>,
}

fn row_from_sql(row: &rusqlite::Row) -> rusqlite::Result<AuthRow> {
    Ok(AuthRow {
        connector_id: row.get(0)?,
        issuer: row.get(1)?,
        authorize_url: row.get(2)?,
        token_url: row.get(3)?,
        resource: row.get(4)?,
        client_id: row.get(5)?,
        client_secret: row.get(6)?,
        access_token: row.get(7)?,
        refresh_token: row.get(8)?,
        expires_at: row.get(9)?,
        scope: row.get(10)?,
        connected_at: row.get(11)?,
    })
}

fn to_auth(row: AuthRow) -> ConnectorAuth {
    ConnectorAuth {
        connector_id: row.connector_id,
        issuer: row.issuer,
        authorize_url: row.authorize_url,
        token_url: row.token_url,
        resource: row.resource,
        client_id: row.client_id,
        client_secret: row.client_secret,
        access_token: row.access_token,
        refresh_token: row.refresh_token,
        expires_at: row.expires_at,
        scope: row.scope,
        connected_at: row.connected_at,
    }
}

const AUTH_COLUMNS: &str = "connector_id, issuer, authorize_url, token_url, resource, client_id, client_secret, access_token, refresh_token, expires_at, scope, connected_at";

pub fn get_auth(db: &Db, connector_id: &str) -> rusqlite::Result<Option<ConnectorAuth>> {
    db.conn()
        .query_row(
            &format!("SELECT {AUTH_COLUMNS} FROM connector_auth WHERE connector_id = ?1"),
            params![connector_id],
            row_from_sql,
        )
        .optional()
        .map(|opt| opt.map(to_auth))
}

pub struct PutConnectorAuth<'a> {
    pub connector_id: &'a str,
    pub issuer: &'a str,
    pub authorize_url: &'a str,
    pub token_url: &'a str,
    pub resource: &'a str,
    pub client_id: &'a str,
    pub client_secret: Option<&'a str>,
}

pub fn put_auth(db: &Db, input: PutConnectorAuth<'_>) -> rusqlite::Result<()> {
    db.conn().execute(
        "INSERT INTO connector_auth
           (connector_id, issuer, authorize_url, token_url, resource, client_id, client_secret)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(connector_id) DO UPDATE SET
           issuer = excluded.issuer,
           authorize_url = excluded.authorize_url,
           token_url = excluded.token_url,
           resource = excluded.resource,
           client_id = excluded.client_id,
           client_secret = excluded.client_secret",
        params![
            input.connector_id,
            input.issuer,
            input.authorize_url,
            input.token_url,
            input.resource,
            input.client_id,
            input.client_secret
        ],
    )?;
    Ok(())
}

pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<String>,
    pub scope: Option<String>,
}

pub fn put_tokens(db: &Db, connector_id: &str, tokens: &OAuthTokens) -> rusqlite::Result<()> {
    db.conn().execute(
        "UPDATE connector_auth
            SET access_token = ?1, refresh_token = COALESCE(?2, refresh_token),
                expires_at = ?3, scope = ?4, connected_at = ?5
          WHERE connector_id = ?6",
        params![
            tokens.access_token,
            tokens.refresh_token,
            tokens.expires_at,
            tokens.scope,
            Utc::now().to_rfc3339(),
            connector_id,
        ],
    )?;
    Ok(())
}

pub fn forget_auth(db: &Db, connector_id: &str) -> rusqlite::Result<()> {
    let conn = db.conn();
    conn.execute(
        "DELETE FROM oauth_flows WHERE connector_id = ?1",
        params![connector_id],
    )?;
    conn.execute(
        "DELETE FROM connector_auth WHERE connector_id = ?1",
        params![connector_id],
    )?;
    Ok(())
}

pub fn start_flow(
    db: &Db,
    connector_id: &str,
    state: &str,
    verifier: &str,
) -> rusqlite::Result<()> {
    db.conn().execute(
        "INSERT INTO oauth_flows (state, connector_id, verifier, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![state, connector_id, verifier, Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

pub struct ClaimedFlow {
    pub connector_id: String,
    pub verifier: String,
}

pub fn claim_flow(db: &Db, state: &str) -> rusqlite::Result<Option<ClaimedFlow>> {
    let row = db
        .conn()
        .query_row(
            "SELECT connector_id, verifier, created_at FROM oauth_flows WHERE state = ?1",
            params![state],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;

    let Some((connector_id, verifier, created_at)) = row else {
        return Ok(None);
    };

    db.conn()
        .execute("DELETE FROM oauth_flows WHERE state = ?1", params![state])?;

    let created = chrono::DateTime::parse_from_rfc3339(&created_at)
        .map(|t| t.timestamp_millis())
        .unwrap_or(0);
    if Utc::now().timestamp_millis() - created > 600_000 {
        return Ok(None);
    }

    Ok(Some(ClaimedFlow {
        connector_id,
        verifier,
    }))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthStatus {
    pub configured: bool,
    pub connected: bool,
    pub issuer: Option<String>,
    pub scope: Option<String>,
}

pub fn auth_status(db: &Db, connector_id: &str) -> rusqlite::Result<AuthStatus> {
    let auth = get_auth(db, connector_id)?;
    Ok(match auth {
        None => AuthStatus {
            configured: false,
            connected: false,
            issuer: None,
            scope: None,
        },
        Some(a) => AuthStatus {
            configured: true,
            connected: a.access_token.as_deref().is_some_and(|t| !t.is_empty()),
            issuer: Some(a.issuer),
            scope: a.scope,
        },
    })
}
