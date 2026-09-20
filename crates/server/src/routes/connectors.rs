//! S7-01: connector registry and per-bot enablement — port of
//! `projects/bullpen-night/src/server/app.ts:2384-2445`.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::mcp::{self, McpCallOptions};
use crate::oauth;
use crate::{ApiResult, AppError, AppState};
use store::{connectors, get_bot, oauth as store_oauth};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/connectors", get(list).post(create))
        .route("/api/connectors/{id}/tools", get(connector_tools))
        .route("/api/connectors/{id}", delete(remove))
        .route("/api/bots/{id}/connectors", get(bot_connectors))
        .route(
            "/api/bots/{id}/connectors/{connector_id}",
            put(set_bot_connector_handler),
        )
        .route("/api/connectors/{id}/connect", post(connect_connector))
        .route(
            "/api/connectors/{id}/auth",
            get(get_auth).delete(delete_auth),
        )
        .route("/api/oauth/callback", get(oauth_callback))
}

#[derive(Deserialize)]
struct OAuthCallbackQuery {
    state: Option<String>,
    code: Option<String>,
    error: Option<String>,
}

async fn list(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    let list = connectors::list_connectors(&db)?;
    Ok(Json(json!({ "connectors": list })))
}

async fn create(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> ApiResult<impl IntoResponse> {
    let (connector, connector_id) = {
        let db = state.db();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
        let name = value.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let url = value.get("url").and_then(|v| v.as_str()).unwrap_or("");
        let auth_header = value
            .get("authHeader")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());

        let result = connectors::add_connector(&db, name, url, auth_header)?;
        if !result.ok {
            return Err(AppError::bad_request(
                result
                    .error
                    .unwrap_or_else(|| "could not add connector".to_string()),
            ));
        }
        let connector = result.connector.clone();
        let connector_id = connector.as_ref().map(|c| c.id.clone()).unwrap_or_default();
        (connector, connector_id)
    };
    let reached = state.refresh_connector_tools(&connector_id).await;
    Ok((
        axum::http::StatusCode::CREATED,
        Json(json!({
            "connector": connector,
            "reachable": reached.ok,
            "error": reached.error,
        })),
    ))
}

async fn connector_tools(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<axum::response::Response> {
    let listed = state.refresh_connector_tools(&id).await;
    if listed.error.as_deref() == Some("no such connector") {
        return Err(AppError::not_found("no such connector"));
    }
    if listed.ok {
        Ok((
            axum::http::StatusCode::OK,
            Json(json!({ "tools": listed.tools })),
        )
            .into_response())
    } else {
        Ok((
            axum::http::StatusCode::BAD_GATEWAY,
            Json(json!({ "error": listed.error, "tools": [] })),
        )
            .into_response())
    }
}

async fn remove(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    if connectors::remove_connector(&db, &id)? {
        Ok(Json(json!({ "ok": true })))
    } else {
        Err(AppError::not_found("no such connector"))
    }
}

async fn bot_connectors(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    if get_bot(&db, &id)?.is_none() {
        return Err(AppError::not_found("no such bot"));
    }
    let enabled: std::collections::HashSet<String> = connectors::connectors_for_bot(&db, &id)?
        .into_iter()
        .map(|c| c.id)
        .collect();
    let list = connectors::list_connectors(&db)?;
    let out: Vec<serde_json::Value> = list
        .into_iter()
        .map(|c| {
            json!({
                "id": c.id,
                "name": c.name,
                "url": c.url,
                "createdAt": c.created_at,
                "enabled": enabled.contains(&c.id),
            })
        })
        .collect();
    Ok(Json(json!({ "connectors": out })))
}

async fn set_bot_connector_handler(
    State(state): State<AppState>,
    Path((bot_id, connector_id)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    if get_bot(&db, &bot_id)?.is_none() {
        return Err(AppError::not_found("no such bot"));
    }
    if connectors::get_connector(&db, &connector_id)?.is_none() {
        return Err(AppError::not_found("no such connector"));
    }
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let enabled = value
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    connectors::set_bot_connector(&db, &bot_id, &connector_id, enabled)?;
    Ok(Json(json!({ "ok": true })))
}

async fn connect_connector(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Response> {
    let (connector, given_id, given_secret, scope) = {
        let db = state.db();
        let full = store::get_connector(&db, &id)?;
        let Some(full) = full else {
            return Err(AppError::not_found("no such connector"));
        };
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
        let given_id = value
            .get("clientId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let given_secret = value
            .get("clientSecret")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let scope = value
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        (full, given_id, given_secret, scope)
    };

    let bearer = state.bearer_for_connector(&id).await;

    let probe = mcp::list_connector_tools(
        &connector,
        McpCallOptions {
            transport: state.mcp_transport.as_ref(),
            resolver: state.mcp_resolver.as_ref(),
            bearer: bearer.as_deref(),
        },
    )
    .await;
    if probe.ok {
        return Ok(Json(json!({
            "ok": true,
            "alreadyOpen": true,
            "message": "That connector needs no authorization.",
        }))
        .into_response());
    }
    let challenge = if probe.needs_auth {
        probe.challenge.as_deref()
    } else {
        None
    };

    let found = oauth::discover(
        &connector.url,
        challenge,
        state.oauth_http.as_ref(),
        state.mcp_resolver.as_ref(),
    )
    .await;
    let Some(metadata) = found.metadata else {
        return Ok((
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": found.error.unwrap_or_else(|| "could not find an authorization server".to_string()) })),
        )
            .into_response());
    };
    let Some(resource) = found.resource else {
        return Ok((
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": found.error.unwrap_or_else(|| "could not find an authorization server".to_string()) })),
        )
            .into_response());
    };

    let redirect = oauth::redirect_uri();
    let (client_id, client_secret) = if given_id.is_empty() {
        let reuse = {
            let db = state.db();
            store_oauth::get_auth(&db, &id)
                .ok()
                .flatten()
                .and_then(|existing| {
                    (existing.issuer == metadata.issuer).then_some((
                        existing.client_id,
                        existing.client_secret.unwrap_or_default(),
                    ))
                })
        };
        if let Some(pair) = reuse {
            pair
        } else if let Some(reg) = &metadata.registration_endpoint {
            match oauth::register_client(reg, &redirect, state.oauth_http.as_ref()).await {
                Ok((cid, secret)) => (cid, secret.unwrap_or_default()),
                Err(e) => {
                    return Ok(
                        (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))).into_response()
                    );
                }
            }
        } else {
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("{} does not register clients automatically. Create an OAuth client there yourself, set its redirect URI to exactly {redirect}, and send the client id and secret back here.", metadata.issuer),
                    "needsClientCredentials": true,
                    "redirectUri": redirect,
                    "issuer": metadata.issuer,
                })),
            )
                .into_response());
        }
    } else {
        (given_id, given_secret)
    };

    {
        let db = state.db();
        store_oauth::put_auth(
            &db,
            store_oauth::PutConnectorAuth {
                connector_id: &id,
                issuer: &metadata.issuer,
                authorize_url: &metadata.authorization_endpoint,
                token_url: &metadata.token_endpoint,
                resource: &resource,
                client_id: &client_id,
                client_secret: if client_secret.is_empty() {
                    None
                } else {
                    Some(client_secret.as_str())
                },
            },
        )?;
    }

    let start = oauth::authorization_url(
        &metadata,
        &client_id,
        &redirect,
        &resource,
        if scope.is_empty() { None } else { Some(&scope) },
    );

    {
        let db = state.db();
        store_oauth::start_flow(&db, &id, &start.state, &start.verifier)?;
    }

    Ok(Json(json!({ "ok": true, "authorizeUrl": start.url })).into_response())
}

async fn oauth_callback(
    State(state): State<AppState>,
    Query(query): Query<OAuthCallbackQuery>,
) -> Response {
    if let Some(denied) = query.error.filter(|e| !e.is_empty()) {
        return (
            StatusCode::BAD_REQUEST,
            Html(oauth::oauth_callback_page(
                "Not connected",
                &format!("The authorization server said: {denied}"),
            )),
        )
            .into_response();
    }

    let state_param = query.state.unwrap_or_default();
    let code = query.code.unwrap_or_default();

    let flow = {
        let db = state.db();
        store_oauth::claim_flow(&db, &state_param).ok().flatten()
    };
    let Some(flow) = flow else {
        return (
            StatusCode::BAD_REQUEST,
            Html(oauth::oauth_callback_page(
                "Not connected",
                "That authorization link was already used, or it expired. Press Connect again.",
            )),
        )
            .into_response();
    };
    if code.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html(oauth::oauth_callback_page(
                "Not connected",
                "No authorization code came back.",
            )),
        )
            .into_response();
    }

    let auth = {
        let db = state.db();
        store_oauth::get_auth(&db, &flow.connector_id)
            .ok()
            .flatten()
    };
    let Some(auth) = auth else {
        return (
            StatusCode::BAD_REQUEST,
            Html(oauth::oauth_callback_page(
                "Not connected",
                "That connector is no longer set up.",
            )),
        )
            .into_response();
    };

    let exchanged = oauth::exchange_code(
        &auth,
        &code,
        &flow.verifier,
        &oauth::redirect_uri(),
        state.oauth_http.as_ref(),
    )
    .await;

    match exchanged {
        Ok(tokens) => {
            let db = state.db();
            let _ = store_oauth::put_tokens(&db, &flow.connector_id, &tokens);
            (
                StatusCode::OK,
                Html(oauth::oauth_callback_page(
                    "Connected",
                    "You can close this tab and go back to Bullpen.",
                )),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Html(oauth::oauth_callback_page("Not connected", &e)),
        )
            .into_response(),
    }
}

async fn get_auth(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    if store::get_connector(&db, &id)?.is_none() {
        return Err(AppError::not_found("no such connector"));
    }
    let status = store_oauth::auth_status(&db, &id)?;
    Ok(Json(json!({
        "configured": status.configured,
        "connected": status.connected,
        "issuer": status.issuer,
        "scope": status.scope,
        "redirectUri": oauth::redirect_uri(),
    })))
}

async fn delete_auth(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let db = state.db();
    if store::get_connector(&db, &id)?.is_none() {
        return Err(AppError::not_found("no such connector"));
    }
    store_oauth::forget_auth(&db, &id)?;
    Ok(Json(json!({ "ok": true })))
}
