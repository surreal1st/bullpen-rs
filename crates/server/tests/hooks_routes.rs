//! Tests for S5b-06b: the webhook delivery path. S5b-06/`eb7ffaa` proved the
//! failure paths (mint/clear 404s, wrong-signature 401) but left a TODO in
//! the success path (`crates/server/src/routes/hooks.rs`), so an
//! authenticated delivery reduced/narrowed/matched by nothing and never fired
//! the routine. This file proves the SUCCESS path: a correctly signed
//! delivery reduces, narrows by `hook_events`, filters by `hook_match`,
//! records `hook_arrivals` for a `conditions` routine, and fires the routine
//! with the reduced text fenced as "## What arrived" / "## All conditions
//! met" - drives everything through HTTP plus a `ScriptedPort` request log,
//! the same posture `tests/routines_fire.rs` uses (`AppState`'s `db` is
//! private and `:memory:` allows no second handle).

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{ScriptedPort, seed_session, text_script};
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use model::MessageContent;
use serde_json::{Value, json};
use sha2::Sha256;
use std::sync::Arc;
use store::Db;
use tower::ServiceExt;

fn open_db() -> Db {
    let db = Db::open(":memory:").expect("open :memory: db");
    model::routing::set_routing_settings(&db, Some(false), None)
        .expect("disable routing classifier");
    server::judge::set_judge_enabled(&db, false).expect("disable judge");
    db
}

fn seed_bot(db: &Db, id: &str, name: &str) {
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES (?1, ?2, '', ?3, NULL, '2026-01-01T00:00:00Z')",
            rusqlite::params![id, name, format!("You are {name}.")],
        )
        .expect("seed bot");
}

#[allow(clippy::too_many_arguments)]
fn seed_routine(
    db: &Db,
    bot_id: &str,
    name: &str,
    hook_kind: &str,
    hook_events: Option<Vec<String>>,
    hook_match: Option<&str>,
    conditions: Option<Vec<store::Condition>>,
) -> String {
    store::create_routine(
        db,
        bot_id,
        name,
        "Tell me what happened.",
        "hourly".to_string(),
        None,
        None,
        None,
        None,
        None,
        Some(hook_kind),
        hook_events,
        hook_match,
        conditions,
    )
    .expect("create routine")
}

async fn send(req: Request<Body>, router: axum::Router) -> (StatusCode, Value) {
    let resp = router.oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "response body is JSON: {e}; status={status}; raw={:?}",
                String::from_utf8_lossy(&bytes)
            )
        })
    };
    (status, body)
}

/// Mints a webhook secret for a routine over HTTP (the real route, not a
/// store shortcut) and returns it - S5b-06's own contract is that the
/// secret is shown exactly once.
async fn mint_secret(router: &axum::Router, session: &str, routine_id: &str) -> String {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/routines/{routine_id}/hook"))
        .header("Cookie", session)
        .body(Body::empty())
        .expect("build request");
    let (status, body) = send(req, router.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "mint should succeed: {body:?}");
    body["secret"]
        .as_str()
        .expect("secret string in mint response")
        .to_string()
}

fn hmac_sha256_hex(secret: &str, body: &str) -> String {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(body.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn github_signature(secret: &str, body: &str) -> String {
    format!("sha256={}", hmac_sha256_hex(secret, body))
}

fn pagerduty_signature(secret: &str, body: &str) -> String {
    format!("v1={}", hmac_sha256_hex(secret, body))
}

fn github_push_payload(branch: &str, pusher: &str, message: &str) -> String {
    json!({
        "ref": format!("refs/heads/{branch}"),
        "pusher": {"name": pusher},
        "commits": [{"message": message}],
    })
    .to_string()
}

fn pagerduty_incident_payload(number: u32, title: &str, urgency: &str) -> String {
    json!({
        "incidents": [{
            "incident_number": number,
            "title": title,
            "urgency": urgency,
        }],
    })
    .to_string()
}

fn message_text(msg: &model::ModelMessage) -> &str {
    match &msg.content {
        MessageContent::Text(text) => text.as_str(),
        MessageContent::Parts(_) => "",
    }
}

/// The last user-turn text sent to the model across every request the
/// `ScriptedPort` logged, in order - what `fire_webhook_routine` actually
/// handed the model, reduced text and all.
fn last_user_texts(scripted: &ScriptedPort) -> Vec<String> {
    scripted
        .requests()
        .iter()
        .map(|req| {
            req.messages
                .iter()
                .rev()
                .find(|m| m.role == "user")
                .map(message_text)
                .unwrap_or("")
                .to_string()
        })
        .collect()
}

async fn wait_until_requests(scripted: &ScriptedPort, count: usize) {
    for _ in 0..300 {
        if scripted.requests().len() >= count {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!(
        "expected at least {count} model request(s), got {}",
        scripted.requests().len()
    );
}

#[tokio::test]
async fn mint_hook_returns_404_for_nonexistent_routine() {
    let db = open_db();
    let cookie = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    let router = server::build_app(server::AppState::new(db));

    let req = Request::builder()
        .method("POST")
        .uri("/api/routines/nonexistent-id/hook")
        .header("Cookie", &cookie)
        .body(Body::empty())
        .expect("build request");

    let (status, body) = send(req, router).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no such routine");
}

#[tokio::test]
async fn clear_hook_returns_404_for_nonexistent_routine() {
    let db = open_db();
    let cookie = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    let router = server::build_app(server::AppState::new(db));

    let req = Request::builder()
        .method("DELETE")
        .uri("/api/routines/nonexistent-id/hook")
        .header("Cookie", &cookie)
        .body(Body::empty())
        .expect("build request");

    let (status, body) = send(req, router).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no such routine");
}

#[tokio::test]
async fn webhook_delivery_without_secret_returns_404() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let router = server::build_app(server::AppState::new(db));

    let req = Request::builder()
        .method("POST")
        .uri("/api/hooks/nonexistent-routine")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"test":"payload"}"#))
        .expect("build request");

    let (status, body) = send(req, router).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no such webhook");
}

/// A correctly signed github delivery reduces the payload, fires the
/// routine, and the reduced text rides inside "## What arrived" - the whole
/// point of this ticket. Also the base for BITE (a) below.
#[tokio::test]
async fn correctly_signed_github_delivery_fires_the_routine_with_reduced_text() {
    let db = open_db();
    let session = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    let routine_id = seed_routine(&db, "arthur", "watch", "github", None, None, None);

    let scripted = Arc::new(ScriptedPort::new(vec![text_script("noted")]));
    let port: Arc<dyn model::ModelPort> = scripted.clone();
    let state = server::AppState::with_port(db, port);
    let router = server::build_app(state);

    let secret = mint_secret(&router, &session, &routine_id).await;
    let body = github_push_payload("main", "octocat", "fix bug");
    let sig = github_signature(&secret, &body);

    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/hooks/{routine_id}"))
        .header("x-github-event", "push")
        .header("x-hub-signature-256", sig)
        .header("Content-Type", "application/json")
        .body(Body::from(body))
        .expect("build request");

    let (status, resp_body) = send(req, router.clone()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "body: {resp_body:?}");
    assert_eq!(resp_body["ok"], true);
    assert!(resp_body["runId"].is_string());

    wait_until_requests(&scripted, 1).await;
    let texts = last_user_texts(&scripted);
    assert_eq!(texts.len(), 1);
    assert!(
        texts[0].contains("## What arrived"),
        "prompt must fence the delivery: {}",
        texts[0]
    );
    assert!(
        texts[0].contains("push to main by octocat"),
        "reduced github text must ride along: {}",
        texts[0]
    );
}

/// An event outside the routine's configured `hook_events` must not fire -
/// `hook_events = NULL` means every event (proven by the test above using a
/// routine with no `hook_events` at all); this proves the narrowing side.
#[tokio::test]
async fn github_event_outside_hook_events_does_not_fire() {
    let db = open_db();
    let session = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    let routine_id = seed_routine(
        &db,
        "arthur",
        "watch-prs",
        "github",
        Some(vec!["pull_request".to_string()]),
        None,
        None,
    );

    let scripted = Arc::new(ScriptedPort::new(vec![text_script("noted")]));
    let port: Arc<dyn model::ModelPort> = scripted.clone();
    let state = server::AppState::with_port(db, port);
    let router = server::build_app(state);

    let secret = mint_secret(&router, &session, &routine_id).await;
    let body = github_push_payload("main", "octocat", "fix bug");
    let sig = github_signature(&secret, &body);

    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/hooks/{routine_id}"))
        .header("x-github-event", "push")
        .header("x-hub-signature-256", sig)
        .header("Content-Type", "application/json")
        .body(Body::from(body))
        .expect("build request");

    let (status, _resp_body) = send(req, router).await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "a push event must not fire a routine only watching pull_request"
    );
    assert!(
        scripted.requests().is_empty(),
        "the routine must never have fired"
    );
}

/// Reduced text that fails the routine's `hook_match` regex must not fire;
/// text that passes must.
#[tokio::test]
async fn hook_match_filters_reduced_text() {
    let db = open_db();
    let session = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    let routine_id = seed_routine(
        &db,
        "arthur",
        "urgent-only",
        "raw",
        None,
        Some("urgent"),
        None,
    );

    let scripted = Arc::new(ScriptedPort::new(vec![text_script("noted")]));
    let port: Arc<dyn model::ModelPort> = scripted.clone();
    let state = server::AppState::with_port(db, port);
    let router = server::build_app(state);

    let secret = mint_secret(&router, &session, &routine_id).await;

    // Non-matching delivery: must not fire.
    let quiet_req = Request::builder()
        .method("POST")
        .uri(format!("/api/hooks/{routine_id}"))
        .header("authorization", format!("Bearer {secret}"))
        .header("Content-Type", "application/json")
        .body(Body::from(json!({"text": "just an update"}).to_string()))
        .expect("build request");
    let (status, _) = send(quiet_req, router.clone()).await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "text failing hook_match must not fire"
    );
    assert!(scripted.requests().is_empty());

    // Matching delivery (case-insensitive, like the TS `new RegExp(p, "i")"): must fire.
    let loud_req = Request::builder()
        .method("POST")
        .uri(format!("/api/hooks/{routine_id}"))
        .header("authorization", format!("Bearer {secret}"))
        .header("Content-Type", "application/json")
        .body(Body::from(
            json!({"text": "URGENT: server down"}).to_string(),
        ))
        .expect("build request");
    let (status, resp_body) = send(loud_req, router).await;
    assert_eq!(status, StatusCode::ACCEPTED, "body: {resp_body:?}");

    wait_until_requests(&scripted, 1).await;
    let texts = last_user_texts(&scripted);
    assert!(texts[0].contains("URGENT: server down"), "{}", texts[0]);
}

/// A routine holding `conditions` records a `hook_arrivals` row per
/// delivery and only fires once every condition in the AND-group has a
/// recent arrival - proven end to end over HTTP (no second db handle is
/// possible against `:memory:`): the first delivery (github) must 204 and
/// leave the routine unfired; the second (pagerduty), within the window,
/// must fire with BOTH reduced texts under "## All conditions met" - which
/// is only possible if the first delivery's `hook_arrivals` row was really
/// recorded and read back.
#[tokio::test]
async fn conditions_routine_records_arrivals_and_fires_once_all_are_met() {
    let db = open_db();
    let session = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    let conditions = vec![
        store::Condition {
            kind: "github".to_string(),
            match_: None,
        },
        store::Condition {
            kind: "pagerduty".to_string(),
            match_: None,
        },
    ];
    let routine_id = seed_routine(
        &db,
        "arthur",
        "ship-and-page",
        "raw",
        None,
        None,
        Some(conditions),
    );

    let scripted = Arc::new(ScriptedPort::new(vec![text_script("noted")]));
    let port: Arc<dyn model::ModelPort> = scripted.clone();
    let state = server::AppState::with_port(db, port);
    let router = server::build_app(state);

    let secret = mint_secret(&router, &session, &routine_id).await;

    // First arrival: github push. Only one of two conditions met - must 204
    // and must not fire yet.
    let gh_body = github_push_payload("main", "octocat", "ship it");
    let gh_sig = github_signature(&secret, &gh_body);
    let gh_req = Request::builder()
        .method("POST")
        .uri(format!("/api/hooks/{routine_id}"))
        .header("x-github-event", "push")
        .header("x-hub-signature-256", gh_sig)
        .header("Content-Type", "application/json")
        .body(Body::from(gh_body))
        .expect("build request");
    let (status, _) = send(gh_req, router.clone()).await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "only one of two conditions has arrived"
    );
    assert!(
        scripted.requests().is_empty(),
        "must not fire on a partial AND-group"
    );

    // Second arrival: pagerduty incident, within the 60-minute window. Both
    // conditions are now met - must fire with both texts fenced together.
    let pd_body = pagerduty_incident_payload(42, "DB down", "high");
    let pd_sig = pagerduty_signature(&secret, &pd_body);
    let pd_req = Request::builder()
        .method("POST")
        .uri(format!("/api/hooks/{routine_id}"))
        .header("x-pagerduty-event-type", "incident.triggered")
        .header("x-pagerduty-signature", pd_sig)
        .header("Content-Type", "application/json")
        .body(Body::from(pd_body))
        .expect("build request");
    let (status, resp_body) = send(pd_req, router).await;
    assert_eq!(status, StatusCode::ACCEPTED, "body: {resp_body:?}");

    wait_until_requests(&scripted, 1).await;
    let texts = last_user_texts(&scripted);
    assert_eq!(texts.len(), 1, "the AND-group must fire exactly once");
    assert!(texts[0].contains("## All conditions met"), "{}", texts[0]);
    assert!(
        texts[0].contains("push to main by octocat"),
        "the recorded github arrival must ride along: {}",
        texts[0]
    );
    assert!(
        texts[0].contains("Incident #42 triggered"),
        "the pagerduty arrival must ride along: {}",
        texts[0]
    );
}

/// BITE (a) target: a forged github delivery (wrong signature) against a
/// routine that DOES have a real secret configured must be refused and must
/// never reach the reducer/firing path. See S5b-06b's Results for the RED
/// line proving this test bites when the signature check is skipped.
#[tokio::test]
async fn forged_github_signature_is_refused_and_never_fires() {
    let db = open_db();
    let session = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    let routine_id = seed_routine(&db, "arthur", "watch", "github", None, None, None);

    let scripted = Arc::new(ScriptedPort::new(vec![text_script("noted")]));
    let port: Arc<dyn model::ModelPort> = scripted.clone();
    let state = server::AppState::with_port(db, port);
    let router = server::build_app(state);

    // Mint a REAL secret so this is a routine that actually has a webhook
    // configured, not the 404-before-auth case the old smoke test covered.
    let _secret = mint_secret(&router, &session, &routine_id).await;
    let body = github_push_payload("main", "octocat", "fix bug");

    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/hooks/{routine_id}"))
        .header("x-github-event", "push")
        .header(
            "x-hub-signature-256",
            "sha256=0000000000000000000000000000000000000000000000000000000000000000",
        )
        .header("Content-Type", "application/json")
        .body(Body::from(body))
        .expect("build request");

    let (status, resp_body) = send(req, router).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {resp_body:?}");
    assert_eq!(resp_body["error"], "not authorized");
    assert!(
        scripted.requests().is_empty(),
        "a forged delivery must never reach the reducer or fire the routine"
    );
}

/// BITE (b) target: `GET /api/routines` must never carry the raw webhook
/// secret, only `hasHook`. See S5b-06b's Results for the RED line proving
/// this test bites when the route is made to leak it.
#[tokio::test]
async fn list_routines_never_exposes_the_webhook_secret() {
    let db = open_db();
    let session = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    let routine_id = seed_routine(&db, "arthur", "watch", "github", None, None, None);

    let router = server::build_app(server::AppState::new(db));
    let secret = mint_secret(&router, &session, &routine_id).await;

    let req = Request::builder()
        .method("GET")
        .uri("/api/routines?bot=arthur")
        .header("Cookie", &session)
        .body(Body::empty())
        .expect("build request");
    let (status, body) = send(req, router).await;
    assert_eq!(status, StatusCode::OK);

    let raw = body.to_string();
    assert!(
        !raw.contains(&secret),
        "the minted secret must never appear in GET /api/routines: {raw}"
    );
    let routine = body["routines"]
        .as_array()
        .and_then(|a| a.iter().find(|r| r["id"] == routine_id))
        .expect("routine present in list");
    assert_eq!(routine["hasHook"], true);
}
