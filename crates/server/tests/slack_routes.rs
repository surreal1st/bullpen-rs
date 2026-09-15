//! Tests for S5c-03: `/api/slack/events` (the Events API delivery route),
//! the reply half of `on_run_done`, and the `hook_kind: "slack"` routine
//! trigger loop. Drives everything through HTTP plus a `ScriptedPort`
//! request log and a `FakeSlackApi` call log - same posture
//! `tests/hooks_routes.rs` uses for `/api/hooks/:routineId`.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::ScriptedPort;
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::Sha256;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use store::Db;
use tower::ServiceExt;

use server::slack::{
    AuthTestResult, PostMessageResult, SlackApi, SlackConnectInput, connect_slack,
    set_slack_answer_bot_id,
};

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
fn seed_slack_routine(
    db: &Db,
    bot_id: &str,
    name: &str,
    trigger_kind: &str,
    hook_match: Option<&str>,
) -> String {
    let id = store::create_routine(
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
        Some("slack"),
        Some(vec![trigger_kind.to_string()]),
        hook_match,
        None,
    )
    .expect("create slack routine");
    // `create_routine` inserts every new routine `active = 0` - a hook
    // trigger only ever offers itself to an ACTIVE routine
    // (`active_routines_by_hook_kind`'s own `WHERE active = 1`), same as a
    // schedule trigger's `due_routines`.
    store::set_routine_active(db, &id, true, None).expect("activate routine");
    id
}

/// What one `chat.postMessage` call looked like.
#[derive(Clone, Debug, PartialEq)]
struct PostedMessage {
    channel: String,
    text: String,
    thread_ts: Option<String>,
}

/// Scripted `SlackApi`: `auth_test` answers a fixed `AuthTestResult` (used
/// once, by the test's own setup call to `connect_slack`), and every
/// `post_message` call is logged rather than sent anywhere real.
struct FakeSlackApi {
    bot_user_id: String,
    posts: Mutex<Vec<PostedMessage>>,
}

impl FakeSlackApi {
    fn new(bot_user_id: &str) -> Self {
        Self {
            bot_user_id: bot_user_id.to_string(),
            posts: Mutex::new(Vec::new()),
        }
    }

    fn posts(&self) -> Vec<PostedMessage> {
        self.posts.lock().expect("posts mutex poisoned").clone()
    }
}

#[async_trait::async_trait]
impl SlackApi for FakeSlackApi {
    async fn auth_test(&self, _bot_token: &str) -> Result<AuthTestResult, String> {
        Ok(AuthTestResult {
            team_id: Some("T123".to_string()),
            team_name: Some("test-team".to_string()),
            user_id: Some(self.bot_user_id.clone()),
        })
    }

    async fn post_message(
        &self,
        _bot_token: &str,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<PostMessageResult, String> {
        self.posts
            .lock()
            .expect("posts mutex poisoned")
            .push(PostedMessage {
                channel: channel.to_string(),
                text: text.to_string(),
                thread_ts: thread_ts.map(|s| s.to_string()),
            });
        Ok(PostMessageResult {
            ts: "1690000000.000200".to_string(),
        })
    }

    async fn delete_message(
        &self,
        _bot_token: &str,
        _channel: &str,
        _ts: &str,
    ) -> Result<(), String> {
        Ok(())
    }
}

/// A `ModelPort` whose `stream` never resolves - what the in-flight-guard
/// bite needs: a run that stays `running` for the whole test, so a second
/// delivery for the same routine has something real to be refused against.
/// Logs every request it was handed, same as `ScriptedPort`.
struct HangingPort {
    requests: Mutex<Vec<model::ModelRequest>>,
}

impl HangingPort {
    fn new() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
        }
    }

    fn request_count(&self) -> usize {
        self.requests.lock().expect("requests mutex poisoned").len()
    }
}

impl model::ModelPort for HangingPort {
    fn stream(&self, request: model::ModelRequest) -> model::EventStream {
        self.requests
            .lock()
            .expect("requests mutex poisoned")
            .push(request);
        Box::pin(futures::stream::pending())
    }
}

fn slack_signature(secret: &str, timestamp: &str, body: &str) -> String {
    type HmacSha256 = Hmac<Sha256>;
    let base = format!("v0:{timestamp}:{body}");
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(base.as_bytes());
    format!("v0={}", hex::encode(mac.finalize().into_bytes()))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_secs()
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

fn post_events_request(body: &str, timestamp: &str, signature: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/slack/events")
        .header("x-slack-request-timestamp", timestamp)
        .header("x-slack-signature", signature)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("build request")
}

fn dm_event_payload(channel: &str, user: &str, text: &str, ts: &str) -> String {
    json!({
        "type": "event_callback",
        "event": {
            "type": "message",
            "channel_type": "im",
            "channel": channel,
            "user": user,
            "text": text,
            "ts": ts,
        }
    })
    .to_string()
}

fn channel_message_event_payload(channel: &str, user: &str, text: &str, ts: &str) -> String {
    json!({
        "type": "event_callback",
        "event": {
            "type": "message",
            "channel_type": "channel",
            "channel": channel,
            "user": user,
            "text": text,
            "ts": ts,
        }
    })
    .to_string()
}

async fn wait_until<F: Fn() -> bool>(condition: F, what: &str) {
    for _ in 0..300 {
        if condition() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

const SIGNING_SECRET: &str = "test-signing-secret";

/// Connects Slack in the db (via the real `connect_slack`, same as the
/// production `PUT /api/slack` route calls, minus the HTTP/spawn_blocking
/// plumbing - a plain async call is fine here, this is test setup code, not
/// an axum handler bound to `Send`) and answers with `bot_user_id`.
async fn configure_slack(db: &Db, api: &FakeSlackApi) {
    connect_slack(
        db,
        SlackConnectInput {
            bot_token: "xoxb-test-token".to_string(),
            signing_secret: SIGNING_SECRET.to_string(),
            app_token: None,
        },
        api,
    )
    .await
    .expect("connect slack");
}

/// BITE: a correctly signed DM starts a run and, when the scripted model
/// answers, the fake `SlackApi` records ONE `chat.postMessage` with the
/// same channel + thread_ts the DM arrived on.
#[tokio::test]
async fn correctly_signed_dm_starts_a_run_and_replies_in_thread() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");

    let fake_slack = Arc::new(FakeSlackApi::new("UBOT1"));
    configure_slack(&db, &fake_slack).await;
    set_slack_answer_bot_id(&db, "arthur").expect("set answer bot");

    let scripted = Arc::new(ScriptedPort::new(vec![common::text_script("On it.")]));
    let port: Arc<dyn model::ModelPort> = scripted.clone();
    let slack_api: Arc<dyn SlackApi + Send + Sync> = fake_slack.clone();
    let state = server::AppState::with_port_and_slack_api(db, port, slack_api);
    let router = server::build_app(state);

    let body = dm_event_payload("D123", "U999", "help me please", "1700000000.000100");
    let timestamp = now_unix().to_string();
    let signature = slack_signature(SIGNING_SECRET, &timestamp, &body);

    let (status, _) = send(post_events_request(&body, &timestamp, &signature), router).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    wait_until(|| !fake_slack.posts().is_empty(), "a chat.postMessage call").await;

    let posts = fake_slack.posts();
    assert_eq!(posts.len(), 1, "expected exactly one reply, got {posts:?}");
    assert_eq!(posts[0].channel, "D123");
    assert_eq!(posts[0].thread_ts.as_deref(), Some("1700000000.000100"));
    assert_eq!(posts[0].text, "On it.");
}

/// A `ModelPort` whose stream fails every turn - what a run that errors out
/// needs, to prove the reply-posting `on_run_done` hook's `status == "done"`
/// guard (`AppState::build`, `crates/server/src/lib.rs`).
struct FailingPort;

impl model::ModelPort for FailingPort {
    fn stream(&self, _request: model::ModelRequest) -> model::EventStream {
        Box::pin(futures::stream::iter(vec![model::ModelEvent::Error {
            message: "simulated model failure".to_string(),
            status: None,
        }]))
    }
}

/// A run that FAILS posts no reply at all - only a `done` run with non-empty
/// text does (the same guard the TS `pendingSlackReplies` branch of
/// `onRunDone` applies: `finished.status === "done" && finished.text.trim()
/// !== ""`). Documents the judgment call named in this ticket: a failed run
/// is a silently missed reply, nothing else.
#[tokio::test]
async fn a_failed_run_posts_no_slack_reply() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");

    let fake_slack = Arc::new(FakeSlackApi::new("UBOT1"));
    configure_slack(&db, &fake_slack).await;
    set_slack_answer_bot_id(&db, "arthur").expect("set answer bot");

    let port: Arc<dyn model::ModelPort> = Arc::new(FailingPort);
    let slack_api: Arc<dyn SlackApi + Send + Sync> = fake_slack.clone();
    let state = server::AppState::with_port_and_slack_api(db, port, slack_api);
    let router = server::build_app(state);

    let body = dm_event_payload("D123", "U999", "help me please", "1700000000.000100");
    let timestamp = now_unix().to_string();
    let signature = slack_signature(SIGNING_SECRET, &timestamp, &body);

    let (status, _) = send(post_events_request(&body, &timestamp, &signature), router).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // No observable "done" signal from outside for a run that FAILS (that's
    // exactly what this test proves the absence of) - a fixed grace period
    // stands in, long enough for `FailingPort`'s single-frame stream to
    // settle many times over.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        fake_slack.posts().is_empty(),
        "a failed run must never post a Slack reply, got {:?}",
        fake_slack.posts()
    );
}

/// BITE: a forged signature is refused (401) and starts no run at all - the
/// model is never even called.
#[tokio::test]
async fn forged_signature_is_refused_and_starts_no_run() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");

    let fake_slack = Arc::new(FakeSlackApi::new("UBOT1"));
    configure_slack(&db, &fake_slack).await;
    set_slack_answer_bot_id(&db, "arthur").expect("set answer bot");

    let scripted = Arc::new(ScriptedPort::new(vec![common::text_script(
        "should not run",
    )]));
    let port: Arc<dyn model::ModelPort> = scripted.clone();
    let slack_api: Arc<dyn SlackApi + Send + Sync> = fake_slack.clone();
    let state = server::AppState::with_port_and_slack_api(db, port, slack_api);
    let router = server::build_app(state);

    let body = dm_event_payload("D123", "U999", "help me please", "1700000000.000100");
    let timestamp = now_unix().to_string();
    // Signed with the WRONG secret.
    let signature = slack_signature("not-the-real-secret", &timestamp, &body);

    let (status, response_body) =
        send(post_events_request(&body, &timestamp, &signature), router).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(response_body["error"], "not authorized");

    assert!(
        scripted.requests().is_empty(),
        "no model call for a forged signature"
    );
    assert!(
        fake_slack.posts().is_empty(),
        "no reply for a forged signature"
    );
}

/// BITE: a correctly-signed but replayed (stale) timestamp is refused (401)
/// even though the HMAC itself is valid for that timestamp.
#[tokio::test]
async fn replayed_timestamp_outside_window_is_refused() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");

    let fake_slack = Arc::new(FakeSlackApi::new("UBOT1"));
    configure_slack(&db, &fake_slack).await;
    set_slack_answer_bot_id(&db, "arthur").expect("set answer bot");

    let scripted = Arc::new(ScriptedPort::new(vec![common::text_script(
        "should not run",
    )]));
    let port: Arc<dyn model::ModelPort> = scripted.clone();
    let slack_api: Arc<dyn SlackApi + Send + Sync> = fake_slack.clone();
    let state = server::AppState::with_port_and_slack_api(db, port, slack_api);
    let router = server::build_app(state);

    let body = dm_event_payload("D123", "U999", "help me please", "1700000000.000100");
    // SLACK_REPLAY_WINDOW_SECONDS is 300; 400 seconds old is outside it, but
    // the signature is still computed correctly FOR that stale timestamp.
    let stale_timestamp = (now_unix() - 400).to_string();
    let signature = slack_signature(SIGNING_SECRET, &stale_timestamp, &body);

    let (status, response_body) = send(
        post_events_request(&body, &stale_timestamp, &signature),
        router,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(response_body["error"], "not authorized");

    assert!(
        scripted.requests().is_empty(),
        "no model call for a replayed timestamp"
    );
}

/// BITE: a `keyword` routine with `hook_match: "deploy"` fires on "we
/// deploy at noon" and not on "having lunch".
#[tokio::test]
async fn keyword_routine_fires_on_match_and_not_on_miss() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let routine_id = seed_slack_routine(&db, "arthur", "deploy-watch", "keyword", Some("deploy"));

    let fake_slack = Arc::new(FakeSlackApi::new("UBOT1"));
    configure_slack(&db, &fake_slack).await;
    // No answer bot set - this event is a plain channel message (not a DM
    // or mention), so the chat-surface branch never engages regardless;
    // only the routine loop should touch it.

    let scripted = Arc::new(ScriptedPort::new(vec![common::text_script("noted")]));
    let port: Arc<dyn model::ModelPort> = scripted.clone();
    let slack_api: Arc<dyn SlackApi + Send + Sync> = fake_slack.clone();
    let state = server::AppState::with_port_and_slack_api(db, port, slack_api);
    let router = server::build_app(state);

    let timestamp = now_unix().to_string();

    // Miss first: "having lunch" does not contain "deploy".
    let miss_body = channel_message_event_payload("C1", "U1", "having lunch", "1700000000.000001");
    let miss_signature = slack_signature(SIGNING_SECRET, &timestamp, &miss_body);
    let (status, _) = send(
        post_events_request(&miss_body, &timestamp, &miss_signature),
        router.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Hit: "we deploy at noon" contains "deploy".
    let hit_body =
        channel_message_event_payload("C1", "U1", "we deploy at noon", "1700000000.000002");
    let hit_signature = slack_signature(SIGNING_SECRET, &timestamp, &hit_body);
    let (status, _) = send(
        post_events_request(&hit_body, &timestamp, &hit_signature),
        router,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    wait_until(
        || !scripted.requests().is_empty(),
        "the routine to fire once",
    )
    .await;
    // Give the miss a fair chance to (wrongly) fire too, before asserting
    // the count stays at exactly one.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let requests = scripted.requests();
    assert_eq!(
        requests.len(),
        1,
        "expected exactly one fire (the match), got {}",
        requests.len()
    );
    let sent_text = match &requests[0]
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .expect("a user turn")
        .content
    {
        model::MessageContent::Text(text) => text.clone(),
        model::MessageContent::Parts(_) => String::new(),
    };
    assert!(
        sent_text.contains("we deploy at noon"),
        "expected the reduced deploy text in the fired prompt, got: {sent_text}"
    );
    let _ = routine_id;
}

/// BITE: the in-flight guard refuses a second trigger for the same routine
/// while its first run is still going - a second delivery starts no second
/// model call.
#[tokio::test]
async fn in_flight_guard_refuses_a_second_trigger_for_the_same_routine() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    seed_slack_routine(&db, "arthur", "deploy-watch", "keyword", Some("deploy"));

    let fake_slack = Arc::new(FakeSlackApi::new("UBOT1"));
    configure_slack(&db, &fake_slack).await;

    let hanging = Arc::new(HangingPort::new());
    let port: Arc<dyn model::ModelPort> = hanging.clone();
    let slack_api: Arc<dyn SlackApi + Send + Sync> = fake_slack.clone();
    let state = server::AppState::with_port_and_slack_api(db, port, slack_api);
    let router = server::build_app(state);

    let timestamp = now_unix().to_string();
    let body1 = channel_message_event_payload("C1", "U1", "we deploy now", "1700000000.000010");
    let signature1 = slack_signature(SIGNING_SECRET, &timestamp, &body1);
    let (status, _) = send(
        post_events_request(&body1, &timestamp, &signature1),
        router.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    wait_until(|| hanging.request_count() >= 1, "the first run to start").await;
    assert_eq!(hanging.request_count(), 1);

    // Second delivery for the SAME routine while the first run is still
    // `running` (the HangingPort's stream never resolves) - the in-flight
    // guard should refuse it before ever touching the model.
    let body2 = channel_message_event_payload("C1", "U2", "deploy again", "1700000000.000011");
    let signature2 = slack_signature(SIGNING_SECRET, &timestamp, &body2);
    let (status, _) = send(post_events_request(&body2, &timestamp, &signature2), router).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Give a wrongly-started second run a fair chance to reach the model
    // before asserting the count never moved past one.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(
        hanging.request_count(),
        1,
        "the in-flight guard should have refused the second delivery"
    );
}
