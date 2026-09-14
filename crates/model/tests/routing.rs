use model::ladder::{DEFAULT_TIER1, Trigger};
use model::port::{EventStream, ModelEvent, ModelMessage, ModelPort, ModelRequest, ModelUsage};
use model::routing::*;
use std::sync::{Arc, Mutex};
use store::Db;

fn open_db() -> Db {
    Db::open(":memory:").expect("failed to open in-memory db")
}

/// A simple mock port for testing that records requests and replays events.
struct MockPort {
    events: Vec<ModelEvent>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl MockPort {
    fn new(events: Vec<ModelEvent>) -> Self {
        Self {
            events,
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().expect("lock poisoned").clone()
    }
}

impl ModelPort for MockPort {
    fn stream(&self, request: ModelRequest) -> EventStream {
        self.requests.lock().expect("lock poisoned").push(request);
        Box::pin(futures::stream::iter(self.events.clone()))
    }
}

#[tokio::test]
async fn work_verdict_moves_run_to_reason_rung() {
    let db = open_db();
    ensure_routing_tables(&db).ok();

    let events = vec![
        ModelEvent::Delta {
            text: "work".to_string(),
        },
        ModelEvent::Done {
            model: "test/classifier".to_string(),
            usage: Some(ModelUsage {
                cost_usd: 0.0001,
                input_tokens: 100,
                output_tokens: 1,
                cached_tokens: 0,
            }),
            finish_reason: None,
        },
    ];

    let port = MockPort::new(events);
    let messages = vec![ModelMessage::user("write me a migration plan")];

    let result = maybe_route(
        &db,
        &port,
        Trigger::Chat,
        "google/gemini-2.5-flash-lite",
        &messages,
        false,
    )
    .await
    .expect("routing failed");

    let result = result.expect("should route");
    assert_eq!(result.verdict, RoutingVerdict::Work);
    assert_eq!(result.model, DEFAULT_TIER1.reason);
}

#[tokio::test]
async fn lookup_verdict_keeps_current_model() {
    let db = open_db();
    ensure_routing_tables(&db).ok();

    let events = vec![
        ModelEvent::Delta {
            text: "lookup".to_string(),
        },
        ModelEvent::Done {
            model: "test/classifier".to_string(),
            usage: None,
            finish_reason: None,
        },
    ];

    let port = MockPort::new(events);
    let messages = vec![ModelMessage::user("what timezone did I set")];
    let current = "google/gemini-2.5-flash-lite";

    let result = maybe_route(&db, &port, Trigger::Chat, current, &messages, false)
        .await
        .expect("routing failed");

    let result = result.expect("should route");
    assert_eq!(result.verdict, RoutingVerdict::Lookup);
    assert_eq!(result.model, current);
}

#[tokio::test]
async fn routine_run_never_classified() {
    let db = open_db();
    ensure_routing_tables(&db).ok();

    let port = MockPort::new(vec![
        ModelEvent::Delta {
            text: "work".to_string(),
        },
        ModelEvent::Done {
            model: "test/classifier".to_string(),
            usage: None,
            finish_reason: None,
        },
    ]);
    let messages = vec![ModelMessage::user("some message")];

    // Routine trigger should bypass routing entirely
    let result = maybe_route(
        &db,
        &port,
        Trigger::Routine,
        "google/gemini-2.5-flash-lite",
        &messages,
        false,
    )
    .await
    .expect("routing failed");

    assert!(result.is_none());
    // No classifier call was made, so the port should have no requests
    assert_eq!(port.requests().len(), 0);
}

#[tokio::test]
async fn bot_pinned_at_reason_rung_never_routed() {
    let db = open_db();
    ensure_routing_tables(&db).ok();

    let port = MockPort::new(vec![
        ModelEvent::Delta {
            text: "work".to_string(),
        },
        ModelEvent::Done {
            model: "test/classifier".to_string(),
            usage: None,
            finish_reason: None,
        },
    ]);
    let messages = vec![ModelMessage::user("write me something")];
    // Bot pinned at reason rung
    let reason_model = DEFAULT_TIER1.reason;

    let result = maybe_route(&db, &port, Trigger::Chat, reason_model, &messages, false)
        .await
        .expect("routing failed");

    assert!(result.is_none());
    assert_eq!(port.requests().len(), 0);
}

#[tokio::test]
async fn routing_disabled_never_classifies() {
    let db = open_db();
    ensure_routing_tables(&db).ok();

    set_routing_settings(&db, Some(false), None).ok();

    let port = MockPort::new(vec![
        ModelEvent::Delta {
            text: "work".to_string(),
        },
        ModelEvent::Done {
            model: "test/classifier".to_string(),
            usage: None,
            finish_reason: None,
        },
    ]);
    let messages = vec![ModelMessage::user("write me something")];

    let result = maybe_route(
        &db,
        &port,
        Trigger::Chat,
        "google/gemini-2.5-flash-lite",
        &messages,
        false,
    )
    .await
    .expect("routing failed");

    assert!(result.is_none());
    assert_eq!(port.requests().len(), 0);
}

#[test]
fn two_verdicts_produce_two_distinct_log_rows() {
    // F2 bite: the old `uuid()` seeded every byte from its own index, so it
    // returned the SAME string on every call. `routing_log.id` is `TEXT
    // PRIMARY KEY`, so the first insert succeeded and the second failed with
    // a UNIQUE constraint violation - `record_log` returned `Err`, which
    // `.expect` below turns into a panic. A real `uuid::Uuid::new_v4` never
    // collides in a two-call test.
    let db = open_db();
    ensure_routing_tables(&db).ok();

    record_log(&db, RoutingVerdict::Lookup, "model-a").expect("first insert");
    record_log(&db, RoutingVerdict::Action, "model-b").expect("second insert");

    let log = list_routing_log(&db, 20).expect("read log");
    assert_eq!(
        log.len(),
        2,
        "two distinct verdicts must produce two distinct rows, not a PRIMARY KEY collision"
    );
}

#[test]
fn cap_keeps_the_newest_200_rows_even_on_a_created_at_tie() {
    // F12 bite: `format_iso8601` has millisecond resolution, so real inserts
    // in a tight loop can share a `created_at` by chance - not a reliable
    // way to PROVE the tiebreak, so this seeds a deterministic tie directly:
    // 200 rows sharing one fabricated `created_at`, inserted in a known
    // rowid order, then one real `record_log` call whose real-clock
    // timestamp is unambiguously newer than the fabricated one. That leaves
    // exactly 201 rows and a cap of 200 - one of the 200 tied rows must be
    // dropped, and only `rowid DESC` as the tiebreak decides which. The old
    // cap ordered by `created_at` alone, so on this tie SQLite's plan
    // dropped the highest-rowid (most recently inserted) tied row instead of
    // the lowest - the newest of the tied batch, gone.
    let db = open_db();
    ensure_routing_tables(&db).ok();

    {
        let conn = db.conn();
        for i in 0..200 {
            conn.execute(
                "INSERT INTO routing_log (id, created_at, verdict, model) VALUES (?1, ?2, 'lookup', ?3)",
                [
                    format!("seed-{i:03}"),
                    "2020-01-01T00:00:00.000Z".to_string(),
                    format!("seed-model-{i:03}"),
                ],
            )
            .expect("seed insert");
        }
    }

    // A real `record_log` call: its real-clock `created_at` is unambiguously
    // later than every fabricated seed row, so it is never itself part of
    // the tie - it is what pushes the table to 201 rows and triggers the cap.
    record_log(&db, RoutingVerdict::Work, "the-newest").expect("record_log");

    let log = list_routing_log(&db, 300).expect("read log");
    assert_eq!(log.len(), 200, "expected the cap to hold at 200 rows");
    assert!(
        log.iter().any(|e| e.model == "the-newest"),
        "expected the genuinely newest row to survive the cap"
    );
    assert!(
        log.iter().any(|e| e.model == "seed-model-199"),
        "expected the LAST-inserted tied row (highest rowid) to survive the cap"
    );
    assert!(
        !log.iter().any(|e| e.model == "seed-model-000"),
        "expected the FIRST-inserted tied row (lowest rowid) to be the one pruned"
    );
}

#[tokio::test]
async fn log_records_verdict_and_model() {
    let db = open_db();
    ensure_routing_tables(&db).ok();

    let events = vec![
        ModelEvent::Delta {
            text: "action".to_string(),
        },
        ModelEvent::Done {
            model: "test/classifier".to_string(),
            usage: None,
            finish_reason: None,
        },
    ];

    let port = MockPort::new(events);
    let messages = vec![ModelMessage::user("run the backup")];

    maybe_route(
        &db,
        &port,
        Trigger::Chat,
        "google/gemini-2.5-flash-lite",
        &messages,
        false,
    )
    .await
    .ok();

    let log = list_routing_log(&db, 20).expect("failed to read log");
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].verdict, "action");
    assert_eq!(log[0].model, "google/gemini-2.5-flash-lite");
}

#[tokio::test]
async fn classifier_error_defaults_to_lookup() {
    let db = open_db();
    ensure_routing_tables(&db).ok();

    let events = vec![ModelEvent::Error {
        message: "connection lost".to_string(),
        status: Some(500),
    }];

    let port = MockPort::new(events);
    let messages = vec![ModelMessage::user("write me something new")];

    let result = maybe_route(
        &db,
        &port,
        Trigger::Chat,
        "google/gemini-2.5-flash-lite",
        &messages,
        false,
    )
    .await
    .expect("routing failed");

    let result = result.expect("should route even on error");
    assert_eq!(result.verdict, RoutingVerdict::Lookup);
}

#[tokio::test]
async fn room_members_never_routed() {
    let db = open_db();
    ensure_routing_tables(&db).ok();

    let port = MockPort::new(vec![
        ModelEvent::Delta {
            text: "work".to_string(),
        },
        ModelEvent::Done {
            model: "test/classifier".to_string(),
            usage: None,
            finish_reason: None,
        },
    ]);
    let messages = vec![ModelMessage::user("plan out the rebuild")];

    let result = maybe_route(
        &db,
        &port,
        Trigger::Chat,
        "google/gemini-2.5-flash-lite",
        &messages,
        true,
    )
    .await
    .expect("routing failed");

    assert!(result.is_none());
    assert_eq!(port.requests().len(), 0);
}

#[test]
fn get_routing_settings_returns_defaults() {
    let db = open_db();
    let settings = get_routing_settings(&db).expect("failed to get settings");
    assert!(settings.enabled);
    assert_eq!(settings.text, DEFAULT_ROUTING_TEXT);
}

#[test]
fn set_routing_settings_persists() {
    let db = open_db();
    let new_text = "new routing rule";
    set_routing_settings(&db, Some(false), Some(new_text.to_string())).expect("failed to set");

    let settings = get_routing_settings(&db).expect("failed to get");
    assert!(!settings.enabled);
    assert_eq!(settings.text, new_text);
}

#[test]
fn routing_text_capped_at_600_chars() {
    let db = open_db();
    let long_text = "x".repeat(1000);
    set_routing_settings(&db, None, Some(long_text)).expect("failed to set");

    let settings = get_routing_settings(&db).expect("failed to get");
    assert_eq!(settings.text.len(), 600);
}

#[tokio::test]
async fn classifier_includes_usage_in_result() {
    let db = open_db();
    ensure_routing_tables(&db).ok();

    let events = vec![
        ModelEvent::Delta {
            text: "work".to_string(),
        },
        ModelEvent::Done {
            model: "test/classifier".to_string(),
            usage: Some(ModelUsage {
                cost_usd: 0.0002,
                input_tokens: 40,
                output_tokens: 1,
                cached_tokens: 0,
            }),
            finish_reason: None,
        },
    ];

    let port = MockPort::new(events);
    let messages = vec![ModelMessage::user("write something")];

    let result = maybe_route(
        &db,
        &port,
        Trigger::Chat,
        "google/gemini-2.5-flash-lite",
        &messages,
        false,
    )
    .await
    .expect("routing failed");

    let result = result.expect("should route");
    assert!(result.usage.is_some());
    let usage = result.usage.unwrap();
    assert_eq!(usage.cost_usd, 0.0002);
}

#[test]
fn recent_turns_text_skips_system_messages() {
    let messages = vec![
        ModelMessage::system("system message"),
        ModelMessage::user("user message"),
    ];
    let text = recent_turns_text(&messages);
    assert!(!text.contains("system message"));
    assert!(text.contains("user message"));
}
