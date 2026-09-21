//! `POST /api/auth/login` and `POST /api/auth/logout`. Port of
//! `projects/bullpen-night/src/server/app.ts:3241-3298`, using the throttle
//! and cookie helpers in `crate::auth`.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::parse_body;
use crate::AppError;
use crate::AppState;
use crate::auth::{clear_session_cookie, presented_token, set_session_cookie};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
}

#[derive(Deserialize, Default)]
struct LoginBody {
    #[serde(default)]
    password: Option<String>,
}

/// Mirrors `app.ts:3241-3285`. A malformed body is deliberately never a 400
/// here (unlike `parse_body`'s usual callers) - the TS route catches a parse
/// failure and falls back to `{}`, so a broken body just means "no
/// password", which fails the same way an empty one does.
async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, AppError> {
    if state.login_throttle.throttled() {
        return Ok((
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error": "Too many attempts. Wait a few minutes."})),
        )
            .into_response());
    }

    let parsed: LoginBody = parse_body(&body).unwrap_or_default();
    let password = parsed.password.unwrap_or_default();

    let record = {
        let db = state.db();
        store::password_record(&db)?
    };
    let Some(record) = record else {
        return Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "This Bullpen has no password set yet.", "setup": true})),
        )
            .into_response());
    };

    if password.is_empty() || !store::verify_password(&password, &record) {
        state.login_throttle.record_failure();
        // Deliberately the same message either way: "no such user" and
        // "wrong password" are the same fact here, and a distinct message
        // is a hint.
        return Ok((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "That password is not right."})),
        )
            .into_response());
    }

    state.login_throttle.clear();
    let token = {
        let db = state.db();
        let owner_id = store::adopt_owner(&db)?.unwrap_or_else(|| store::OWNER_ID.to_string());
        store::create_session_with(&db, store::CreateSessionOpts::owner_sign_in(owner_id))?
    };

    // Resume routines that were paused for absence when Josh signs in.
    let resumed = crate::routines::resume_absence_paused(&state, chrono::Utc::now());
    if resumed > 0 {
        tracing::info!("resumed {resumed} routines paused for absence");
    }

    let mut response = (StatusCode::OK, Json(json!({"ok": true, "token": token}))).into_response();
    set_session_cookie(&mut response, &headers, &token);
    Ok(response)
}

/// Mirrors `app.ts:3290-3298`. Not gated open by whether a token is present
/// or valid - clearing a stale/absent cookie must always succeed.
async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, AppError> {
    let token = presented_token(&headers);
    if !token.is_empty() {
        let db = state.db();
        store::destroy_session(&db, &token)?;
    }

    let mut response = Json(json!({"ok": true})).into_response();
    clear_session_cookie(&mut response);
    Ok(response)
}
