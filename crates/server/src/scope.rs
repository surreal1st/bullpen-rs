//! S11-01: request scope (owner vs member). Full `scopeGuard` lands in S11-02;
//! this module resolves who signed in for `/api/users/me`.

use store::{Db, OWNER_ID, UserRole, adopt_owner, get_user, session_user_id};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub user_id: String,
    pub owner_id: String,
    pub role: UserRole,
    pub is_owner: bool,
}

/// Resolve scope from a valid session token. Calls `adopt_owner` so legacy
/// sessions and rows stay coherent.
pub fn scope_for_token(db: &Db, token: &str) -> rusqlite::Result<Scope> {
    let owner_id = adopt_owner(db)?.unwrap_or_else(|| OWNER_ID.to_string());
    let session_uid = session_user_id(db, token)?;
    let effective_user_id = session_uid.as_deref().unwrap_or(owner_id.as_str());

    if effective_user_id == owner_id {
        return Ok(Scope {
            user_id: owner_id.clone(),
            owner_id,
            role: UserRole::Owner,
            is_owner: true,
        });
    }

    match get_user(db, effective_user_id)? {
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
