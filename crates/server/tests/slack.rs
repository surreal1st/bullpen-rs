//! Tests for Slack integration: config, status, connect/disconnect, and answer bot selection.

use server::slack::{
    AuthTestResult, PostMessageResult, SlackApi, SlackConnectInput, disconnect_slack,
    get_slack_answer_bot_id, set_slack_answer_bot_id, slack_status,
};
use store::Db;
use uuid::Uuid;

struct FakeSlackApi {
    pub auth_test_result: Option<Result<AuthTestResult, String>>,
}

#[async_trait::async_trait]
impl SlackApi for FakeSlackApi {
    async fn auth_test(&self, _bot_token: &str) -> Result<AuthTestResult, String> {
        self.auth_test_result.as_ref().cloned().unwrap_or_else(|| {
            Ok(AuthTestResult {
                team_id: Some("T123".to_string()),
                team_name: Some("test-team".to_string()),
                user_id: Some("U123".to_string()),
            })
        })
    }

    async fn post_message(
        &self,
        _bot_token: &str,
        _channel: &str,
        _text: &str,
        _thread_ts: Option<&str>,
    ) -> Result<PostMessageResult, String> {
        Ok(PostMessageResult {
            ts: "1234567890.123456".to_string(),
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

/// Helper to create a test bot in the database
fn create_test_bot(db: &Db, name: &str) -> String {
    let bot_id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, created_at) VALUES (?, ?, ?)",
            rusqlite::params![bot_id, name, now],
        )
        .expect("insert test bot");
    bot_id
}

#[test]
fn test_slack_status_when_not_configured() {
    let db = Db::open(":memory:").expect("open db");
    let status = slack_status(&db);
    assert!(!status.configured);
    assert_eq!(status.team_name, None);
    assert_eq!(status.bot_user_id, None);
    assert!(!status.has_app_token);
}

#[tokio::test]
async fn test_connect_slack_and_status_configured() {
    use server::slack::connect_slack;

    let db = Db::open(":memory:").expect("open db");
    let api = FakeSlackApi {
        auth_test_result: None,
    };

    let result = connect_slack(
        &db,
        SlackConnectInput {
            bot_token: "xoxb-123456".to_string(),
            signing_secret: "secret123".to_string(),
            app_token: Some("xapp-456789".to_string()),
        },
        &api,
    )
    .await;

    assert!(result.is_ok());
    let status = result.unwrap();
    assert!(status.configured);
    assert_eq!(status.team_name, Some("test-team".to_string()));
    assert_eq!(status.bot_user_id, Some("U123".to_string()));
    assert!(status.has_app_token);
}

#[tokio::test]
async fn test_connect_slack_stores_nothing_on_auth_failure() {
    use server::slack::connect_slack;

    let db = Db::open(":memory:").expect("open db");
    let api = FakeSlackApi {
        auth_test_result: Some(Err("invalid_token".to_string())),
    };

    let result = connect_slack(
        &db,
        SlackConnectInput {
            bot_token: "bad-token".to_string(),
            signing_secret: "secret".to_string(),
            app_token: None,
        },
        &api,
    )
    .await;

    assert!(result.is_err());
    // Verify nothing was stored: status should show not configured
    let status = slack_status(&db);
    assert!(!status.configured);
}

#[test]
fn test_disconnect_slack() {
    let db = Db::open(":memory:").expect("open db");

    // Manually put some settings
    db.conn()
        .execute(
            "INSERT INTO settings (key, value) VALUES (?, ?)",
            rusqlite::params!["slack.team_id", "T123"],
        )
        .expect("insert setting");

    // Disconnect
    disconnect_slack(&db).expect("disconnect");

    // Verify settings are gone
    let count: i32 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM settings WHERE key LIKE 'slack.%'",
            [],
            |row| row.get(0),
        )
        .expect("count settings");
    assert_eq!(count, 0, "all slack settings should be deleted");
}

#[test]
fn test_get_slack_answer_bot_id_none_when_not_set() {
    let db = Db::open(":memory:").expect("open db");
    assert_eq!(get_slack_answer_bot_id(&db), None);
}

#[test]
fn test_get_slack_answer_bot_id_returns_pinned_bot() {
    let db = Db::open(":memory:").expect("open db");

    let pinned_id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let pinned_at = now.clone();

    // Create a pinned bot
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, created_at, pinned_at) VALUES (?, ?, ?, ?)",
            rusqlite::params![pinned_id, "pinned_bot", now, pinned_at],
        )
        .expect("insert bot");

    let answer_id = get_slack_answer_bot_id(&db);
    assert_eq!(answer_id, Some(pinned_id));
}

#[test]
fn test_get_slack_answer_bot_id_returns_arthur_fallback() {
    let db = Db::open(":memory:").expect("open db");

    let now = chrono::Utc::now().to_rfc3339();

    // Create Arthur (but not pinned)
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, created_at) VALUES (?, ?, ?)",
            rusqlite::params!["arthur", "Arthur", now],
        )
        .expect("insert arthur");

    let answer_id = get_slack_answer_bot_id(&db);
    assert_eq!(answer_id, Some("arthur".to_string()));
}

#[test]
fn test_set_slack_answer_bot_id_with_valid_bot() {
    let db = Db::open(":memory:").expect("open db");

    let bot_id = create_test_bot(&db, "test_bot");

    let result = set_slack_answer_bot_id(&db, &bot_id).expect("set answer bot");
    assert!(result, "should return true for valid bot");

    let stored = get_slack_answer_bot_id(&db);
    assert_eq!(stored, Some(bot_id));
}

#[test]
fn test_set_slack_answer_bot_id_with_invalid_bot() {
    let db = Db::open(":memory:").expect("open db");

    let result = set_slack_answer_bot_id(&db, "nonexistent").expect("set answer bot");
    assert!(!result, "should return false for nonexistent bot");
}
