//! W6: away rows + gap/cache. Port of `test/away.test.ts`.

mod common;

use chrono::{TimeZone, Utc};
use model::fake::{billed_port, text_port};
use server::away::{AWAY_GAP_HOURS, compute_away, dismiss_away, gather_away_rows};
use std::sync::{Arc, Mutex};
use store::{Db, auth, conversations, questions};

fn t0() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 13, 12, 0, 0)
        .single()
        .expect("fixed test time")
}

fn hours_ago(h: f64) -> String {
    (t0() - chrono::Duration::milliseconds((h * 3_600_000.0) as i64))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn seed() -> (Db, String, String) {
    let db = Db::open(":memory:").expect("open");
    let bot_id = "bot-away";
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at, permissions)
             VALUES (?1, 'Nozdormu', 'p', 'i', NULL, '2026-01-01T00:00:00Z', '{}')",
            rusqlite::params![bot_id],
        )
        .expect("bot");
    let conversation_id = conversations::get_or_create_conversation(&db, bot_id).expect("conv");
    (db, bot_id.to_string(), conversation_id)
}

fn message(db: &Db, conversation_id: &str, role: &str, created_at: &str) {
    let seq: i64 = db
        .conn()
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE conversation_id = ?1",
            rusqlite::params![conversation_id],
            |row| row.get(0),
        )
        .unwrap_or(1);
    db.conn()
        .execute(
            "INSERT INTO messages (id, conversation_id, seq, role, content, created_at)
             VALUES (?1, ?2, ?3, ?4, 'hello', ?5)",
            rusqlite::params![
                uuid::Uuid::new_v4().to_string(),
                conversation_id,
                seq,
                role,
                created_at
            ],
        )
        .expect("message");
}

fn question(db: &Db, bot_id: &str, conversation_id: &str, asked_at: &str) {
    questions::insert_question(
        db,
        bot_id,
        conversation_id,
        None,
        "Drop the dead ones?",
        &[],
    )
    .expect("question");
    let _ = asked_at;
}

fn pending_approval(db: &Db, bot_id: &str, conversation_id: &str, created_at: &str) {
    let run_id = uuid::Uuid::new_v4().to_string();
    db.conn()
        .execute(
            "INSERT INTO runs (id, bot_id, conversation_id, trigger, status, model, messages, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'chat', 'waiting', 'test/model', '[]', ?4, ?4)",
            rusqlite::params![run_id, bot_id, conversation_id, created_at],
        )
        .expect("run");
    db.conn()
        .execute(
            "INSERT INTO approvals (id, run_id, bot_id, tool_name, tool_args, call_id, status, created_at)
             VALUES (?1, ?2, ?3, 'shell', '{}', ?4, 'pending', ?5)",
            rusqlite::params![
                uuid::Uuid::new_v4().to_string(),
                run_id,
                bot_id,
                uuid::Uuid::new_v4().to_string(),
                created_at
            ],
        )
        .expect("approval");
}

fn stopped_routine(db: &Db, bot_id: &str, name: &str, created_at: &str) {
    db.conn()
        .execute(
            "INSERT INTO routines (id, bot_id, name, prompt, schedule, active, created_at, consecutive_failures, paused_reason)
             VALUES (?1, ?2, ?3, 'do the thing', '0 * * * *', 0, ?4, 3, 'too many failures in a row')",
            rusqlite::params![uuid::Uuid::new_v4().to_string(), bot_id, name, created_at],
        )
        .expect("routine");
}

#[test]
fn gather_away_rows_counts_waiting_items() {
    let (db, bot_id, conversation_id) = seed();
    db.conn()
        .execute(
            "UPDATE bots SET last_seen_at = ?1 WHERE id = ?2",
            rusqlite::params![hours_ago(6.0), bot_id],
        )
        .expect("seen");
    message(&db, &conversation_id, "assistant", &hours_ago(5.0));
    message(&db, &conversation_id, "assistant", &hours_ago(4.0));
    question(&db, &bot_id, &conversation_id, &hours_ago(5.0));
    pending_approval(&db, &bot_id, &conversation_id, &hours_ago(5.0));
    stopped_routine(&db, &bot_id, "Streamer watch", &hours_ago(5.0));

    let rows = gather_away_rows(&db).expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].unread, 2);
    assert_eq!(rows[0].questions, 1);
    assert_eq!(rows[0].approvals, 1);
    assert_eq!(rows[0].stopped_routines, vec!["Streamer watch".to_string()]);
}

#[test]
fn gather_away_rows_omits_bot_with_nothing_waiting() {
    let (db, bot_id, conversation_id) = seed();
    db.conn()
        .execute(
            "UPDATE bots SET last_seen_at = ?1 WHERE id = ?2",
            rusqlite::params![hours_ago(6.0), bot_id],
        )
        .expect("seen");
    message(&db, &conversation_id, "assistant", &hours_ago(7.0));
    assert!(gather_away_rows(&db).expect("rows").is_empty());
}

#[tokio::test]
async fn compute_away_shows_card_after_four_hour_gap() {
    let (db, bot_id, conversation_id) = seed();
    db.conn()
        .execute(
            "UPDATE bots SET last_seen_at = ?1 WHERE id = ?2",
            rusqlite::params![hours_ago(6.0), bot_id],
        )
        .expect("seen");
    message(&db, &conversation_id, "assistant", &hours_ago(5.0));
    pending_approval(&db, &bot_id, &conversation_id, &hours_ago(5.0));

    let port: Arc<dyn model::ModelPort> = Arc::new(billed_port(
        "Nozdormu got one reply and has one approval waiting.",
        0.001,
        "test/model",
    ));
    let db = Arc::new(Mutex::new(db));
    let payload = compute_away(db, port, Some("test/model"), t0())
        .await
        .expect("away");
    assert!(payload.show);
    assert!(payload.gap_hours.unwrap() > 5.9);
    assert_eq!(payload.bots.as_ref().expect("bots").len(), 1);
    assert_eq!(
        payload.summary.as_deref(),
        Some("Nozdormu got one reply and has one approval waiting.")
    );
}

#[tokio::test]
async fn compute_away_hides_inside_gap_without_model_call() {
    let (db, bot_id, conversation_id) = seed();
    db.conn()
        .execute(
            "UPDATE bots SET last_seen_at = ?1 WHERE id = ?2",
            rusqlite::params![hours_ago(AWAY_GAP_HOURS - 0.5), bot_id],
        )
        .expect("seen");
    message(&db, &conversation_id, "assistant", &hours_ago(1.0));

    let port: Arc<dyn model::ModelPort> =
        Arc::new(text_port("should never be requested", "test/model"));
    let db = Arc::new(Mutex::new(db));
    let payload = compute_away(Arc::clone(&db), port.clone(), Some("test/model"), t0())
        .await
        .expect("away");
    assert!(!payload.show);
    let utility_rows: i64 = db
        .lock()
        .expect("db mutex")
        .conn()
        .query_row("SELECT COUNT(*) FROM utility_calls", [], |row| row.get(0))
        .expect("utility");
    assert_eq!(utility_rows, 0);
}

#[tokio::test]
async fn compute_away_caches_summary_and_records_utility_spend() {
    let (db, bot_id, conversation_id) = seed();
    db.conn()
        .execute(
            "UPDATE bots SET last_seen_at = ?1 WHERE id = ?2",
            rusqlite::params![hours_ago(5.0), bot_id],
        )
        .expect("seen");
    message(&db, &conversation_id, "assistant", &hours_ago(4.0));

    let fake = Arc::new(billed_port("One reply waiting.", 0.0037, "test/model"));
    let port: Arc<dyn model::ModelPort> = fake.clone();
    let db = Arc::new(Mutex::new(db));
    compute_away(Arc::clone(&db), port, Some("test/model"), t0())
        .await
        .expect("away");

    let cost: f64 = db
        .lock()
        .expect("db mutex")
        .conn()
        .query_row(
            "SELECT cost_usd FROM utility_calls WHERE kind = 'away_summary'",
            [],
            |row| row.get(0),
        )
        .expect("spend");
    assert!((cost - 0.0037).abs() < 1e-6);
    assert_eq!(fake.requests().len(), 1);

    let second = compute_away(
        Arc::clone(&db),
        fake.clone(),
        Some("test/model"),
        t0() + chrono::Duration::minutes(1),
    )
    .await
    .expect("away");
    assert_eq!(second.summary.as_deref(), Some("One reply waiting."));
    assert_eq!(fake.requests().len(), 1);
}

#[tokio::test]
async fn dismiss_stays_until_next_login() {
    let (db, bot_id, conversation_id) = seed();
    db.settings_set(auth::LAST_LOGIN_KEY, &hours_ago(8.0))
        .expect("login");
    db.conn()
        .execute(
            "UPDATE bots SET last_seen_at = ?1 WHERE id = ?2",
            rusqlite::params![hours_ago(6.0), bot_id],
        )
        .expect("seen");
    message(&db, &conversation_id, "assistant", &hours_ago(5.0));

    let fake = Arc::new(billed_port("One reply waiting.", 0.001, "test/model"));
    let db = Arc::new(Mutex::new(db));
    assert!(
        compute_away(Arc::clone(&db), fake.clone(), Some("test/model"), t0())
            .await
            .expect("away")
            .show
    );

    dismiss_away(&db.lock().expect("db mutex")).expect("dismiss");
    let after = compute_away(
        Arc::clone(&db),
        fake.clone(),
        Some("test/model"),
        t0() + chrono::Duration::minutes(5),
    )
    .await
    .expect("away");
    assert!(!after.show);
    assert_eq!(fake.requests().len(), 1);

    db.lock()
        .expect("db mutex")
        .settings_set(
            auth::LAST_LOGIN_KEY,
            &(t0() + chrono::Duration::minutes(10)).to_rfc3339(),
        )
        .expect("new login");
    let after_login = compute_away(
        db,
        fake.clone(),
        Some("test/model"),
        t0() + chrono::Duration::minutes(20),
    )
    .await
    .expect("away");
    assert!(after_login.show);
    assert_eq!(fake.requests().len(), 2);
}
