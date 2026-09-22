//! S11: request scope (owner vs member) and the `/api/*` scope guard.
//! Port of `projects/bullpen-night/src/server/scope.ts`.

use axum::Json;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rusqlite::OptionalExtension;
use serde_json::json;
use store::{
    Db, ListScope, OWNER_ID, UserRole, adopt_owner, get_attachment, get_user, session_user_id,
};

use crate::auth::{is_open_path, presented_token};
use crate::{AppError, AppState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub user_id: String,
    pub owner_id: String,
    pub role: UserRole,
    pub is_owner: bool,
}

impl Scope {
    pub fn list_filter(&self) -> ListScope {
        ListScope {
            owner_id: self.owner_id.clone(),
            user_id: self.user_id.clone(),
        }
    }
}

/// Resolve scope from a valid session token. Calls `adopt_owner` so legacy
/// sessions and rows stay coherent.
pub fn scope_for_token(db: &Db, token: &str) -> rusqlite::Result<Scope> {
    let owner_id = adopt_owner(db)?.unwrap_or_else(|| OWNER_ID.to_string());
    let session_uid = session_user_id(db, token)?;
    let effective_user_id = session_uid
        .as_deref()
        .unwrap_or(owner_id.as_str())
        .to_string();

    scope_for_user_id(db, &effective_user_id, owner_id)
}

fn scope_for_user_id(db: &Db, user_id: &str, owner_id: String) -> rusqlite::Result<Scope> {
    if user_id == owner_id {
        return Ok(Scope {
            user_id: owner_id.clone(),
            owner_id,
            role: UserRole::Owner,
            is_owner: true,
        });
    }
    match get_user(db, user_id)? {
        Some(user) if user.archived_at.is_none() => Ok(Scope {
            user_id: user.id.clone(),
            owner_id,
            role: user.role,
            is_owner: user.role == UserRole::Owner,
        }),
        _ => Ok(Scope {
            user_id: owner_id.clone(),
            owner_id,
            role: UserRole::Owner,
            is_owner: true,
        }),
    }
}

/// Scope for a bot row (routines, goals, background work). Port of `scopeForBot`.
pub fn scope_for_bot(db: &Db, bot_id: &str) -> rusqlite::Result<Scope> {
    let owner_id = adopt_owner(db)?.unwrap_or_else(|| OWNER_ID.to_string());
    let user_id: Option<String> = db
        .conn()
        .query_row(
            "SELECT user_id FROM bots WHERE id = ?1",
            rusqlite::params![bot_id],
            |row| row.get(0),
        )
        .optional()?;
    let effective = user_id.as_deref().unwrap_or(owner_id.as_str()).to_string();
    scope_for_user_id(db, &effective, owner_id)
}

const NOT_IDS: &[&str] = &["tick", "archived", "hidden", "preview", "devices"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    Owner,
    Missing,
}

fn addressed(path: &str) -> Option<(String, Option<String>)> {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.first() != Some(&"api") {
        return None;
    }
    let resource = parts.get(1)?.to_string();
    let id = parts.get(2).map(|s| (*s).to_string());
    let id = id.filter(|i| !NOT_IDS.contains(&i.as_str()));
    Some((resource, id))
}

fn one(db: &Db, sql: &str, id: &str) -> rusqlite::Result<Option<String>> {
    let row = db
        .conn()
        .query_row(sql, rusqlite::params![id], |row| {
            row.get::<_, Option<String>>(0)
        })
        .optional()?;
    Ok(row.flatten())
}

/// Whether `id` is an attachment this scope may attach to a message or
/// fetch — same holder rule as [`require_scope`]'s cross-account 404s.
pub fn can_use_attachment(db: &Db, scope: &Scope, id: &str) -> rusqlite::Result<bool> {
    if get_attachment(db, id)?.is_none() {
        return Ok(false);
    }
    let holder = scoped_owner(db, "attachments", id)?;
    match holder {
        None => Ok(scope.is_owner),
        Some(h) => Ok(h == scope.user_id),
    }
}

fn scoped_owner(db: &Db, resource: &str, id: &str) -> rusqlite::Result<Option<String>> {
    match resource {
        "bots" => one(db, "SELECT user_id FROM bots WHERE id = ?1", id),
        "sections" => one(db, "SELECT user_id FROM sections WHERE id = ?1", id),
        "attachments" => one(db, "SELECT user_id FROM attachments WHERE id = ?1", id),
        "conversations" | "threads" => one(
            db,
            "SELECT b.user_id FROM conversations c JOIN bots b ON b.id = c.bot_id WHERE c.id = ?1",
            id,
        ),
        "rooms" => one(
            db,
            "SELECT b.user_id FROM conversations c JOIN bots b ON b.id = c.bot_id WHERE c.id = ?1 AND c.kind = 'room'",
            id,
        ),
        "messages" => one(
            db,
            "SELECT b.user_id FROM messages m
             JOIN conversations c ON c.id = m.conversation_id
             JOIN bots b ON b.id = c.bot_id WHERE m.id = ?1",
            id,
        ),
        "runs" => one(
            db,
            "SELECT b.user_id FROM runs r JOIN bots b ON b.id = r.bot_id WHERE r.id = ?1",
            id,
        ),
        "routines" => one(
            db,
            "SELECT b.user_id FROM routines t JOIN bots b ON b.id = t.bot_id WHERE t.id = ?1",
            id,
        ),
        "goals" => one(
            db,
            "SELECT b.user_id FROM goals g JOIN bots b ON b.id = g.bot_id WHERE g.id = ?1",
            id,
        ),
        "approvals" => one(
            db,
            "SELECT b.user_id FROM approvals a JOIN bots b ON b.id = a.bot_id WHERE a.id = ?1",
            id,
        ),
        "questions" => one(
            db,
            "SELECT b.user_id FROM questions q JOIN bots b ON b.id = q.bot_id WHERE q.id = ?1",
            id,
        ),
        "report-cards" => one(db, "SELECT user_id FROM bots WHERE id = ?1", id),
        _ => Ok(None),
    }
}

/// Owner-only platform routes (member gets 404). Mirrors TS `OWNER_ONLY`.
fn owner_only_refusal(
    scope: &Scope,
    method: &str,
    resource: &str,
    id: Option<&str>,
) -> Option<Refusal> {
    if scope.is_owner {
        return None;
    }
    const ALL: &[&str] = &[
        "spend",
        "vault",
        "history",
        "purchasing",
        "workers",
        "web",
        "import",
        "shared-core",
        "auto-review",
        "routing",
        "second-opinion",
        "bot-tools",
        "connectors",
        "slack",
        "teams",
        "sandbox",
        "remote",
        "vms",
    ];
    if ALL.contains(&resource) {
        return Some(Refusal::Owner);
    }
    if resource == "users" && id != Some("me") {
        return Some(Refusal::Owner);
    }
    const WRITE: &[&str] = &["PUT", "PATCH", "POST", "DELETE"];
    if WRITE.contains(&method)
        && matches!(
            resource,
            "default-model" | "mid-model" | "tier1-models" | "premium-model" | "rules" | "skills"
        )
    {
        return Some(Refusal::Owner);
    }
    None
}

fn refusal(db: &Db, scope: &Scope, method: &str, path: &str) -> rusqlite::Result<Option<Refusal>> {
    let Some((resource, id)) = addressed(path) else {
        return Ok(None);
    };

    if let Some(r) = owner_only_refusal(scope, method, &resource, id.as_deref()) {
        return Ok(Some(r));
    }

    if resource == "users" && id.as_deref() == Some("me") {
        return Ok(None);
    }

    let Some(id) = id else {
        return Ok(None);
    };

    let holder = scoped_owner(db, &resource, &id)?;
    if holder.is_none() {
        return Ok(if scope.is_owner {
            None
        } else {
            Some(Refusal::Missing)
        });
    }
    if holder.as_deref() == Some(scope.user_id.as_str()) {
        Ok(None)
    } else {
        Ok(Some(Refusal::Missing))
    }
}

/// Middleware: after session gate, attach `Scope` and enforce cross-account 404s.
pub async fn require_scope(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();
    if is_open_path(&path) {
        return next.run(req).await;
    }

    let token = presented_token(req.headers());
    let scope = {
        let db = state.db();
        match scope_for_token(&db, &token) {
            Ok(s) => s,
            Err(err) => return AppError::from(err).into_response(),
        }
    };

    {
        let db = state.db();
        match refusal(&db, &scope, req.method().as_str(), &path) {
            Ok(None) => {}
            Ok(Some(_)) => {
                return (StatusCode::NOT_FOUND, Json(json!({"error": "Not found."})))
                    .into_response();
            }
            Err(err) => return AppError::from(err).into_response(),
        }
    }

    req.extensions_mut().insert(scope);
    next.run(req).await
}
