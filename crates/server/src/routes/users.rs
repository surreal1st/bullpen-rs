//! S11-01: `GET /api/users/me`. Admin/invite routes land in S11-03.

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::auth::presented_token;
use crate::scope;
use crate::spend;
use crate::{AppError, AppState};

pub fn router() -> Router<AppState> {
    Router::new().route("/api/users/me", get(get_users_me))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UsersMeResponse {
    user: UserJson,
    effective_ceiling: f64,
    spent_this_month: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UserJson {
    id: String,
    name: String,
    email: Option<String>,
    role: String,
    ceiling_usd: Option<f64>,
    created_at: String,
    archived_at: Option<String>,
}

async fn get_users_me(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let token = presented_token(&headers);
    let db = state.db();
    let scope = scope::scope_for_token(&db, &token)?;

    let user = store::get_user(&db, &scope.user_id)?.unwrap_or(store::User {
        id: scope.user_id.clone(),
        name: "Josh Johnson".to_string(),
        email: None,
        role: store::UserRole::Owner,
        ceiling_usd: None,
        created_at: String::new(),
        archived_at: None,
    });

    let platform_ceiling = spend::get_ceiling(&db);
    let effective_ceiling = user.ceiling_usd.unwrap_or(platform_ceiling);

    let month = spend::current_month(chrono::Utc::now());
    let spent_this_month = if scope.is_owner {
        spend::spend_by_bot(&db, &month)?
            .into_iter()
            .map(|row| row.cost_usd)
            .sum()
    } else {
        // S11-04: filter by bots owned by this member.
        0.0
    };

    Ok(Json(UsersMeResponse {
        user: UserJson {
            id: user.id,
            name: user.name,
            email: user.email,
            role: match user.role {
                store::UserRole::Owner => "owner".to_string(),
                store::UserRole::Member => "member".to_string(),
            },
            ceiling_usd: user.ceiling_usd,
            created_at: user.created_at,
            archived_at: user.archived_at,
        },
        effective_ceiling,
        spent_this_month,
    }))
}
