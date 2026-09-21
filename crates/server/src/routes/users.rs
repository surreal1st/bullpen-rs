//! S11: people routes — `GET /api/users/me`, admin list/invite/archive/ceiling,
//! and open invite claim. Port of `user-routes.ts`.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;

use super::parse_body;
use crate::auth::{presented_token, set_session_cookie};
use crate::scope;
use crate::spend;
use crate::{AppError, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/users/me", get(get_users_me))
        .route("/api/users", get(list_users_and_invites))
        .route("/api/users/invite", post(mint_invite))
        .route("/api/users/invite/{token}", delete(revoke_invite_route))
        .route("/api/users/{id}", delete(archive_user_route))
        .route("/api/users/{id}/ceiling", put(set_ceiling_route))
        .route("/api/invites/{token}", get(invite_status))
        .route("/api/invites/{token}/claim", post(claim_invite_route))
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

fn user_json(user: store::User) -> UserJson {
    UserJson {
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
    }
}

async fn get_users_me(
    State(state): State<AppState>,
    headers: HeaderMap,
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
        0.0
    };

    Ok(Json(UsersMeResponse {
        user: user_json(user),
        effective_ceiling,
        spent_this_month,
    }))
}

async fn list_users_and_invites(
    State(state): State<AppState>,
    Extension(scope): Extension<scope::Scope>,
) -> Result<impl IntoResponse, AppError> {
    if !scope.is_owner {
        return Ok((StatusCode::NOT_FOUND, Json(json!({"error": "Not found."}))).into_response());
    }
    let db = state.db();
    let now = chrono::Utc::now();
    let users = store::list_users(&db)?;
    let invites = store::list_invites(&db, now)?;
    Ok(Json(json!({
        "users": users,
        "invites": invites,
    }))
    .into_response())
}

async fn mint_invite(
    State(state): State<AppState>,
    Extension(scope): Extension<scope::Scope>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    if !scope.is_owner {
        return Ok((StatusCode::NOT_FOUND, Json(json!({"error": "Not found."}))).into_response());
    }
    let db = state.db();
    let invite = store::create_invite(&db, chrono::Utc::now())?;
    let origin = request_origin(&headers);
    let url = format!("{origin}/invite/{}", invite.token);
    Ok((
        StatusCode::CREATED,
        Json(json!({ "invite": invite, "url": url })),
    )
        .into_response())
}

fn request_origin(headers: &HeaderMap) -> String {
    if let Some(host) = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
    {
        let secure = headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            == Some("https");
        let scheme = if secure { "https" } else { "http" };
        return format!("{scheme}://{host}");
    }
    "http://localhost".to_string()
}

async fn revoke_invite_route(
    State(state): State<AppState>,
    Extension(scope): Extension<scope::Scope>,
    Path(token): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    if !scope.is_owner {
        return Ok((StatusCode::NOT_FOUND, Json(json!({"error": "Not found."}))).into_response());
    }
    let db = state.db();
    if store::revoke_invite(&db, &token)? {
        Ok(Json(json!({"ok": true})).into_response())
    } else {
        Ok((StatusCode::NOT_FOUND, Json(json!({"error": "Not found."}))).into_response())
    }
}

async fn archive_user_route(
    State(state): State<AppState>,
    Extension(scope): Extension<scope::Scope>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    if !scope.is_owner {
        return Ok((StatusCode::NOT_FOUND, Json(json!({"error": "Not found."}))).into_response());
    }
    let db = state.db();
    if store::archive_user(&db, &id, chrono::Utc::now())? {
        let users = store::list_users(&db)?;
        Ok(Json(json!({ "users": users })).into_response())
    } else {
        Ok((StatusCode::NOT_FOUND, Json(json!({"error": "Not found."}))).into_response())
    }
}

#[derive(Deserialize, Default)]
struct CeilingBody {
    ceiling: Option<serde_json::Value>,
}

async fn set_ceiling_route(
    State(state): State<AppState>,
    Extension(scope): Extension<scope::Scope>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, AppError> {
    if !scope.is_owner {
        return Ok((StatusCode::NOT_FOUND, Json(json!({"error": "Not found."}))).into_response());
    }
    let parsed: CeilingBody = parse_body(&body).unwrap_or_default();
    let usd = match parsed.ceiling {
        None => None,
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Number(n)) => {
            let v = n.as_f64().filter(|x| x.is_finite() && *x >= 0.0);
            if v.is_none() {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "ceiling must be a number of dollars, zero or more, or null"})),
                )
                    .into_response());
            }
            v
        }
        Some(_) => {
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(
                    json!({"error": "ceiling must be a number of dollars, zero or more, or null"}),
                ),
            )
                .into_response());
        }
    };
    let db = state.db();
    match store::set_user_ceiling(&db, &id, usd)? {
        Some(user) => Ok(Json(json!({ "user": user })).into_response()),
        None => Ok((StatusCode::NOT_FOUND, Json(json!({"error": "Not found."}))).into_response()),
    }
}

async fn invite_status(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let db = state.db();
    if store::invite_valid(&db, &token, chrono::Utc::now())? {
        Ok(Json(json!({"ok": true})).into_response())
    } else {
        Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "That invite link is not valid any more."})),
        )
            .into_response())
    }
}

#[derive(Deserialize, Default)]
struct ClaimBody {
    #[serde(default)]
    name: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    password: String,
}

async fn claim_invite_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, AppError> {
    let parsed: ClaimBody = parse_body(&body).unwrap_or_default();
    let email = parsed.email.as_deref();
    let db = state.db();
    let now = chrono::Utc::now();
    let user = match store::claim_invite(&db, &token, &parsed.name, email, &parsed.password, now) {
        Ok(user) => user,
        Err(store::ClaimInviteError::Invalid(msg)) => {
            return Ok((StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response());
        }
    };

    let session = store::create_session_with(
        &db,
        store::CreateSessionOpts {
            user_id: Some(user.id.clone()),
            stamp_last_login: false,
        },
    )?;

    let mut response = (
        StatusCode::CREATED,
        Json(json!({
            "ok": true,
            "token": session,
            "user": user,
        })),
    )
        .into_response();
    set_session_cookie(&mut response, &headers, &session);
    Ok(response)
}
