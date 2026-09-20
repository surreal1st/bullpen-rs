//! S7-03: OAuth connect + callback integration (fake MCP + fake AS).

mod common;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::seed_session;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use server::egress::Resolver;
use server::mcp::{McpHttpResponse, McpTransport};
use server::oauth::OAuthHttp;
use server::{AppState, build_app};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use store::Db;
use tower::ServiceExt;

struct FakeMcpNeedsAuth;

#[async_trait]
impl McpTransport for FakeMcpNeedsAuth {
    async fn post(
        &self,
        _url: &str,
        _headers: HeaderMap,
        _body: String,
    ) -> Result<McpHttpResponse, String> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "www-authenticate",
            "Bearer error=\"invalid_token\", resource_metadata=\"https://fake-mcp.test/.well-known/oauth-protected-resource\""
                .parse()
                .unwrap(),
        );
        Ok(McpHttpResponse {
            status: 401,
            headers,
            body: String::new(),
        })
    }
}

struct PublicResolver;

#[async_trait]
impl Resolver for PublicResolver {
    async fn resolve(&self, _host: &str) -> Result<Vec<String>, String> {
        Ok(vec!["93.184.216.34".to_string()])
    }
}

struct FakeOAuthHttp {
    responses: Mutex<HashMap<String, Value>>,
}

#[async_trait]
impl OAuthHttp for FakeOAuthHttp {
    async fn get_json(&self, url: &str) -> Result<(u16, Value), String> {
        let map = self.responses.lock().unwrap();
        let body = map
            .get(url)
            .cloned()
            .ok_or_else(|| format!("no fixture for {url}"))?;
        Ok((200, body))
    }

    async fn post_json(&self, _url: &str, _body: Value) -> Result<(u16, Value), String> {
        Ok((
            200,
            json!({ "client_id": "dyn-client", "client_secret": "dyn-secret" }),
        ))
    }

    async fn post_form(
        &self,
        _url: &str,
        _form: &[(&str, String)],
    ) -> Result<(u16, Value), String> {
        Ok((
            200,
            json!({
                "access_token": "access-xyz",
                "refresh_token": "refresh-xyz",
                "expires_in": 3600,
                "scope": "read"
            }),
        ))
    }
}

fn open_db() -> Db {
    let db = Db::open(":memory:").expect("open db");
    model::routing::set_routing_settings(&db, Some(false), None).expect("routing off");
    server::judge::set_judge_enabled(&db, false).expect("judge off");
    db
}

async fn post_json(
    app: &axum::Router,
    path: &str,
    session: &str,
    body: Value,
) -> (StatusCode, Value) {
    let request = Request::post(path)
        .header("cookie", session)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn get_no_auth(app: &axum::Router, path: &str) -> (StatusCode, String) {
    let request = Request::get(path).body(Body::empty()).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

#[tokio::test]
async fn oauth_connect_and_callback_marks_connector_connected() {
    unsafe { std::env::set_var("BULLPEN_PUBLIC_URL", "https://bullpen.test") };

    let mut map = HashMap::new();
    map.insert(
        "https://fake-mcp.test/.well-known/oauth-protected-resource".to_string(),
        json!({ "authorization_servers": ["https://as.test"] }),
    );
    map.insert(
        "https://as.test/.well-known/oauth-authorization-server".to_string(),
        json!({
            "issuer": "https://as.test",
            "authorization_endpoint": "https://as.test/authorize",
            "token_endpoint": "https://as.test/token",
            "registration_endpoint": "https://as.test/register"
        }),
    );
    let oauth = Arc::new(FakeOAuthHttp {
        responses: Mutex::new(map),
    });

    let db = open_db();
    let session = seed_session(&db);
    let app = build_app(AppState::with_mcp_and_oauth(
        db,
        Arc::new(FakeMcpNeedsAuth),
        Arc::new(PublicResolver),
        oauth,
    ));

    let (_, created) = post_json(
        &app,
        "/api/connectors",
        &session,
        json!({ "name": "Fake", "url": "https://fake-mcp.test/mcp" }),
    )
    .await;
    let id = created["connector"]["id"].as_str().unwrap();

    let (status, body) = post_json(
        &app,
        &format!("/api/connectors/{id}/connect"),
        &session,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let authorize_url = body["authorizeUrl"].as_str().expect("authorize url");
    let state = authorize_url
        .split("state=")
        .nth(1)
        .and_then(|s| s.split('&').next())
        .expect("state param");

    let (cb_status, html) = get_no_auth(
        &app,
        &format!("/api/oauth/callback?state={state}&code=test-code"),
    )
    .await;
    assert_eq!(cb_status, StatusCode::OK);
    assert!(html.contains("Connected"));

    let request = Request::get(format!("/api/connectors/{id}/auth"))
        .header("cookie", &session)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let auth: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(auth["connected"], true);
}
