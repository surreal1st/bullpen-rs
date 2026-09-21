//! S11-06: APNs device registry + payload decisions (port of `test/push.test.ts`).

mod common;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pem::{Pem, encode};
use rcgen::{KeyPair, PKCS_ECDSA_P256_SHA256};
use serde_json::{Value, json};
use server::push::{
    PushTransport, TransportReply, TransportRequest, build_payload, describe_push,
    forget_provider_token, get_credential, is_dead_device, provider_token, send_to_all,
};
use server::{AppState, build_app};
use std::sync::{Arc, Mutex};
use store::Db;
use store::push::{self, PushEnvironment};
use tower::ServiceExt;

const TOKEN_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TOKEN_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

static APNS_ENV_LOCK: Mutex<()> = Mutex::new(());

#[allow(clippy::type_complexity)]
struct Recorder {
    calls: Mutex<Vec<TransportRequest>>,
    reply: Box<dyn Fn(&str) -> (u16, String) + Send + Sync>,
}

#[async_trait]
impl PushTransport for Recorder {
    async fn send(&self, req: TransportRequest) -> TransportReply {
        self.calls.lock().unwrap().push(req.clone());
        let (status, body) = (self.reply)(&req.token);
        TransportReply { status, body }
    }
}

fn ec_pem() -> String {
    let der = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
        .expect("key")
        .serialize_der();
    encode(&Pem::new("PRIVATE KEY", der))
}

fn write_apns_key(pem: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("key.p8");
    std::fs::write(&path, pem).expect("write key");
    (dir, path.to_string_lossy().into_owned())
}

fn set_apns_env(key_file: &str) {
    // SAFETY: test-only env wiring; single-threaded `cargo test` per process.
    unsafe {
        std::env::set_var("BULLPEN_APNS_KEY_FILE", key_file);
        std::env::set_var("BULLPEN_APNS_KEY_ID", "ABC1234567");
        std::env::set_var("BULLPEN_APNS_TEAM_ID", "TEAM123456");
    }
}

fn clear_apns_env() {
    unsafe {
        std::env::remove_var("BULLPEN_APNS_KEY_FILE");
        std::env::remove_var("BULLPEN_APNS_KEY_ID");
        std::env::remove_var("BULLPEN_APNS_TEAM_ID");
    }
    forget_provider_token();
}

#[test]
fn payload_badge_only_has_no_alert_or_sound() {
    let payload: Value = serde_json::from_str(&build_payload(3, None)).expect("json");
    assert_eq!(payload["aps"]["badge"], 3);
    assert!(payload["aps"].get("alert").is_none());
    assert!(payload["aps"].get("sound").is_none());
}

#[test]
fn payload_with_alert_carries_title_and_body() {
    let alert = server::push::PushAlert {
        title: "Nozdormu".into(),
        body: "wants to run shell".into(),
    };
    let payload: Value = serde_json::from_str(&build_payload(1, Some(&alert))).expect("json");
    assert_eq!(payload["aps"]["badge"], 1);
    assert_eq!(payload["aps"]["alert"]["title"], "Nozdormu");
    assert_eq!(payload["aps"]["alert"]["body"], "wants to run shell");
}

#[test]
fn payload_never_sends_negative_or_fractional_badge() {
    let badge = |v: i64| {
        serde_json::from_str::<Value>(&build_payload(v, None)).unwrap()["aps"]["badge"]
            .as_i64()
            .unwrap()
    };
    assert_eq!(badge(-4), 0);
    assert_eq!(badge(2), 2);
}

#[test]
fn is_dead_device_keeps_temporary_failures() {
    assert!(!is_dead_device(429, "TooManyRequests"));
    assert!(!is_dead_device(503, "ServiceUnavailable"));
    assert!(!is_dead_device(400, "PayloadTooLarge"));
}

#[test]
fn is_dead_device_drops_gone_devices() {
    assert!(is_dead_device(410, "Unregistered"));
    assert!(is_dead_device(400, "BadDeviceToken"));
}

#[test]
fn provider_token_reuses_within_ttl() {
    let _lock = APNS_ENV_LOCK.lock().unwrap();
    clear_apns_env();
    let pem = ec_pem();
    let (_dir, path) = write_apns_key(&pem);
    set_apns_env(&path);
    let cred = get_credential().expect("credential");
    let first = provider_token(&cred, 0).expect("token");
    let second = provider_token(&cred, 40 * 60 * 1000).expect("token");
    assert_eq!(first, second);
    let third = provider_token(&cred, 50 * 60 * 1000).expect("token");
    assert_ne!(first, third);
    clear_apns_env();
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn send_to_all_without_key_returns_not_configured() {
    let _lock = APNS_ENV_LOCK.lock().unwrap();
    clear_apns_env();
    let db = Arc::new(Mutex::new(Db::open(":memory:").expect("open")));
    push::register_device(&db.lock().unwrap(), TOKEN_A, PushEnvironment::Sandbox);
    let transport = Recorder {
        calls: Mutex::new(Vec::new()),
        reply: Box::new(|_| (200, String::new())),
    };
    let result = send_to_all(db, 2, None, &transport, None).await;
    assert_eq!(result.sent, 0);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|e| e.contains("not configured"))
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn send_to_all_routes_sandbox_and_production_hosts() {
    let pem = ec_pem();
    let (_dir, path) = write_apns_key(&pem);
    let _lock = APNS_ENV_LOCK.lock().unwrap();
    clear_apns_env();
    set_apns_env(&path);

    let db = Arc::new(Mutex::new(Db::open(":memory:").expect("open")));
    push::register_device(&db.lock().unwrap(), TOKEN_A, PushEnvironment::Sandbox);
    push::register_device(&db.lock().unwrap(), TOKEN_B, PushEnvironment::Production);

    let transport = Recorder {
        calls: Mutex::new(Vec::new()),
        reply: Box::new(|_| (200, String::new())),
    };
    send_to_all(Arc::clone(&db), 1, None, &transport, Some(0)).await;

    let calls = transport.calls.lock().unwrap();
    let host_a = calls.iter().find(|c| c.token == TOKEN_A).expect("a");
    let host_b = calls.iter().find(|c| c.token == TOKEN_B).expect("b");
    assert!(host_a.host.contains("sandbox"));
    assert!(!host_b.host.contains("sandbox"));
    clear_apns_env();
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn send_to_all_uses_alert_push_type_without_alert() {
    let pem = ec_pem();
    let (_dir, path) = write_apns_key(&pem);
    let _lock = APNS_ENV_LOCK.lock().unwrap();
    clear_apns_env();
    set_apns_env(&path);

    let db = Arc::new(Mutex::new(Db::open(":memory:").expect("open")));
    push::register_device(&db.lock().unwrap(), TOKEN_A, PushEnvironment::Sandbox);

    let transport = Recorder {
        calls: Mutex::new(Vec::new()),
        reply: Box::new(|_| (200, String::new())),
    };
    send_to_all(db, 0, None, &transport, Some(0)).await;

    let call = transport.calls.lock().unwrap().pop().expect("one call");
    let push_type = call
        .headers
        .iter()
        .find(|(k, _)| k == "apns-push-type")
        .map(|(_, v)| v.as_str());
    assert_eq!(push_type, Some("alert"));
    clear_apns_env();
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn send_to_all_prunes_dead_devices() {
    let pem = ec_pem();
    let (_dir, path) = write_apns_key(&pem);
    let _lock = APNS_ENV_LOCK.lock().unwrap();
    clear_apns_env();
    set_apns_env(&path);

    let db = Arc::new(Mutex::new(Db::open(":memory:").expect("open")));
    push::register_device(&db.lock().unwrap(), TOKEN_A, PushEnvironment::Sandbox);
    push::register_device(&db.lock().unwrap(), TOKEN_B, PushEnvironment::Sandbox);

    let transport = Recorder {
        calls: Mutex::new(Vec::new()),
        reply: Box::new(|token| {
            if token == TOKEN_A {
                (410, r#"{"reason":"Unregistered"}"#.into())
            } else {
                (429, r#"{"reason":"TooManyRequests"}"#.into())
            }
        }),
    };
    let result = send_to_all(db.clone(), 1, None, &transport, Some(0)).await;
    assert_eq!(result.pruned, 1);
    let tokens: Vec<_> = push::list_devices(&db.lock().unwrap())
        .expect("list")
        .into_iter()
        .map(|d| d.token)
        .collect();
    assert_eq!(tokens, vec![TOKEN_B.to_string()]);
    clear_apns_env();
}

#[test]
fn describe_push_without_key_names_env_vars_not_secret() {
    let _lock = APNS_ENV_LOCK.lock().unwrap();
    clear_apns_env();
    let db = Db::open(":memory:").expect("open");
    let described = describe_push(&db);
    assert_eq!(described["configured"], false);
    assert!(
        described["detail"]
            .as_str()
            .is_some_and(|d| d.contains("BULLPEN_APNS_KEY_FILE"))
    );
}

async fn post_json(app: &Router, path: &str, cookie: &str, body: Value) -> (u16, Value) {
    let req = Request::post(path)
        .header("cookie", cookie)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let parsed = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    (status, parsed)
}

#[tokio::test]
async fn push_device_register_and_forget_via_http() {
    let db = Db::open(":memory:").expect("open");
    let cookie = common::seed_session(&db);
    let app = build_app(AppState::new(db));

    let (status, body) = post_json(
        &app,
        "/api/push/devices",
        &cookie,
        json!({ "token": TOKEN_A, "environment": "sandbox" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK.as_u16());
    assert_eq!(body["environment"], "sandbox");

    let (status, body) = post_json(
        &app,
        "/api/push/devices",
        &cookie,
        json!({ "token": "not-a-token" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST.as_u16());
    assert_eq!(body["error"], "that is not an APNs device token");

    let req = Request::delete(format!("/api/push/devices/{TOKEN_A}"))
        .header("cookie", cookie)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
