//! S12-07: purchasing — port of `purchasing.test.ts` (core bites).

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hmac::{Hmac, Mac};
use server::AppState;
use server::build_app;
use server::purchasing::{
    StripeHttp, StripeKeyKind, StripeResponse, classify_stripe_key, create_issuing_card,
    ensure_purchasing_schema, find_matching_pending_purchase, handle_stripe_webhook,
    monthly_spent_usd, purchase_would_exceed_limit, run_purchase_tool, save_stripe_keys,
    set_bot_purchasing, validate_stripe_key, verify_stripe_signature,
};
use sha2::Sha256;
use tower::ServiceExt;

struct MockStripe {
    calls: Mutex<Vec<String>>,
}

#[async_trait]
impl StripeHttp for MockStripe {
    async fn post_form(
        &self,
        _secret_key: &str,
        path: &str,
        _form: &HashMap<String, String>,
    ) -> StripeResponse {
        self.calls.lock().unwrap().push(path.to_string());
        if path.contains("issuing/cards") {
            return StripeResponse {
                ok: true,
                status: 200,
                body: serde_json::json!({ "id": "ic_test_123", "last4": "4242" }),
            };
        }
        StripeResponse {
            ok: true,
            status: 200,
            body: serde_json::json!({ "id": "iauth_test_1" }),
        }
    }
}

fn stripe_signature(secret: &str, timestamp: &str, raw_body: &str) -> String {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(format!("{timestamp}.{raw_body}").as_bytes());
    format!(
        "t={timestamp},v1={}",
        hex::encode(mac.finalize().into_bytes())
    )
}

fn memory_db() -> Arc<Mutex<store::Db>> {
    let db = Arc::new(Mutex::new(store::Db::open(":memory:").expect("open")));
    ensure_purchasing_schema(&db.lock().unwrap()).expect("schema");
    common::seed_bot(&db, "arthur", "Arthur");
    db
}

fn app_with_session() -> (axum::Router, String) {
    let db = store::Db::open(":memory:").expect("open");
    ensure_purchasing_schema(&db).expect("schema");
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES ('arthur', 'Arthur', '', '', NULL, '2026-01-01T00:00:00Z')",
            [],
        )
        .expect("seed");
    store::set_password(&db, "owner-password-long-enough").expect("password");
    let token = store::create_session(&db).expect("session");
    (build_app(AppState::new(db)), token)
}

#[test]
fn classify_and_validate_stripe_keys() {
    assert_eq!(classify_stripe_key("sk_test_abc123"), StripeKeyKind::Test);
    assert_eq!(classify_stripe_key("sk_live_abc123"), StripeKeyKind::Live);
    assert_eq!(classify_stripe_key("not-a-key"), StripeKeyKind::Invalid);
    assert!(validate_stripe_key("sk_test_abc", false).ok);
    assert!(!validate_stripe_key("sk_live_abc", false).ok);
    assert!(validate_stripe_key("sk_live_abc", true).ok);
}

#[test]
fn save_keys_refuses_live_until_allow_live() {
    let db = store::Db::open(":memory:").expect("open");
    ensure_purchasing_schema(&db).expect("schema");
    let refused = save_stripe_keys(&db, Some("sk_live_abc123"), None, None);
    assert!(!refused.ok);
    assert!(!purchasing_status_safe(&db).has_secret_key);

    server::purchasing::set_allow_live(&db, true).unwrap();
    let ok = save_stripe_keys(&db, Some("sk_live_abc123"), None, None);
    assert!(ok.ok);
}

fn purchasing_status_safe(db: &store::Db) -> server::purchasing::PurchasingStatus {
    server::purchasing::purchasing_status(db)
}

#[tokio::test]
async fn create_card_posts_monthly_limit_to_stripe() {
    let db = memory_db();
    {
        let guard = db.lock().unwrap();
        save_stripe_keys(&guard, Some("sk_test_abc"), None, Some("ich_abc"));
        set_bot_purchasing(&guard, "arthur", Some(true), Some(Some(75.0))).unwrap();
    }
    let stripe = MockStripe {
        calls: Mutex::new(Vec::new()),
    };
    let result = create_issuing_card(&db, "arthur", &stripe).await;
    assert!(result.ok);
    assert_eq!(result.card_id.as_deref(), Some("ic_test_123"));
    assert!(stripe.calls.lock().unwrap()[0].contains("issuing/cards"));
    let guard = db.lock().unwrap();
    assert_eq!(
        get_bot_purchasing(&guard).card_id.as_deref(),
        Some("ic_test_123")
    );
}

fn get_bot_purchasing(db: &store::Db) -> server::purchasing::BotPurchasing {
    server::purchasing::get_bot_purchasing(db, "arthur")
}

#[test]
fn limit_check_and_monthly_spent() {
    let db = store::Db::open(":memory:").expect("open");
    ensure_purchasing_schema(&db).expect("schema");
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES ('arthur', 'Arthur', '', '', NULL, '2026-01-01T00:00:00Z')",
            [],
        )
        .expect("seed");
    assert!(purchase_would_exceed_limit(&db, "arthur", 1.0, chrono::Utc::now()).exceeds);
    set_bot_purchasing(&db, "arthur", Some(true), Some(Some(100.0))).unwrap();
    assert!(
        run_purchase_tool(
            &db,
            "arthur",
            r#"{"merchant":"A","amount_usd":60,"reason":"x"}"#
        )
        .contains("Approved")
    );
    assert_eq!(monthly_spent_usd(&db, "arthur", chrono::Utc::now()), 60.0);
    assert!(purchase_would_exceed_limit(&db, "arthur", 50.0, chrono::Utc::now()).exceeds);
    assert!(!purchase_would_exceed_limit(&db, "arthur", 30.0, chrono::Utc::now()).exceeds);
}

#[test]
fn find_matching_pending_purchase_bites() {
    let db = store::Db::open(":memory:").expect("open");
    ensure_purchasing_schema(&db).expect("schema");
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES ('arthur', 'Arthur', '', '', NULL, '2026-01-01T00:00:00Z')",
            [],
        )
        .expect("seed");
    set_bot_purchasing(&db, "arthur", Some(true), Some(Some(100.0))).unwrap();
    run_purchase_tool(
        &db,
        "arthur",
        r#"{"merchant":"Amazon","amount_usd":20,"reason":"supplies"}"#,
    );
    let hit = find_matching_pending_purchase(&db, "arthur", "Amazon.com", 20.0);
    assert!(hit.is_some());
    assert!(find_matching_pending_purchase(&db, "arthur", "Other", 20.0).is_none());
}

#[test]
fn verify_stripe_signature_replay() {
    let secret = "whsec_test";
    let body = r#"{"type":"issuing_authorization.request"}"#;
    let now = server::purchasing::unix_now();
    let ts = now.to_string();
    let sig = stripe_signature(secret, &ts, body);
    assert!(verify_stripe_signature(secret, body, Some(&sig), 300, now));
    let stale = (now - 600).to_string();
    let stale_sig = stripe_signature(secret, &stale, body);
    assert!(!verify_stripe_signature(
        secret,
        body,
        Some(&stale_sig),
        300,
        now
    ));
}

#[tokio::test]
async fn webhook_approves_matching_authorization() {
    let db = memory_db();
    {
        let guard = db.lock().unwrap();
        save_stripe_keys(
            &guard,
            Some("sk_test_abc"),
            Some("whsec_test"),
            Some("ich_abc"),
        );
        guard
            .conn()
            .execute("UPDATE bots SET card_id = 'ic_1' WHERE id = 'arthur'", [])
            .unwrap();
        set_bot_purchasing(&guard, "arthur", Some(true), Some(Some(100.0))).unwrap();
        run_purchase_tool(
            &guard,
            "arthur",
            r#"{"merchant":"Amazon","amount_usd":20,"reason":"supplies"}"#,
        );
    }
    let stripe = MockStripe {
        calls: Mutex::new(Vec::new()),
    };
    let payload = serde_json::json!({
        "type": "issuing_authorization.request",
        "data": { "object": {
            "id": "iauth_1",
            "amount": 2000,
            "card": { "id": "ic_1" },
            "merchant_data": { "name": "Amazon.com" }
        }}
    });
    let outcome = handle_stripe_webhook(&db, Some("sk_test_abc"), &payload, &stripe).await;
    assert!(outcome.handled);
    assert_eq!(outcome.approved, Some(true));
    let guard = db.lock().unwrap();
    let status: String = guard
        .conn()
        .query_row(
            "SELECT status FROM purchases WHERE merchant = 'Amazon'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status, "authorized");
}

#[tokio::test]
async fn put_keys_route_and_webhook_auth() {
    let (app, token) = app_with_session();
    let res = app
        .clone()
        .oneshot(
            Request::put("/api/purchasing/keys")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"secretKey":"sk_test_abc123","webhookSecret":"whsec_test","cardholderId":"ich_abc"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let bad = app
        .oneshot(
            Request::post("/api/stripe/webhook")
                .header("content-type", "application/json")
                .header("stripe-signature", "t=1,v1=bad")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::UNAUTHORIZED);

    let unconfigured = build_app(AppState::new(store::Db::open(":memory:").unwrap()))
        .oneshot(
            Request::post("/api/stripe/webhook")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unconfigured.status(), StatusCode::NOT_FOUND);
}
