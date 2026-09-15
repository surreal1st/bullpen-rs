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

/// S5c-F-02 bite (F1): TS is `if appToken !== "" put else delete`
/// (`slack.ts:138-140`) - a reconnect with a blank app token must delete the
/// previously stored one, not leave it in place. Before the fix, the `else`
/// hung off `if let Some(..)` so a present-but-blank token took neither
/// branch and the old token survived.
#[tokio::test]
async fn reconnect_with_blank_app_token_deletes_the_stored_one() {
    use server::slack::connect_slack;

    let db = Db::open(":memory:").expect("open db");
    let api = FakeSlackApi {
        auth_test_result: None,
    };

    let status = connect_slack(
        &db,
        SlackConnectInput {
            bot_token: "xoxb-1".to_string(),
            signing_secret: "s1".to_string(),
            app_token: Some("xapp-1".to_string()),
        },
        &api,
    )
    .await
    .expect("connect with app token");
    assert!(status.has_app_token, "app token should be stored");

    let status = connect_slack(
        &db,
        SlackConnectInput {
            bot_token: "xoxb-1".to_string(),
            signing_secret: "s1".to_string(),
            app_token: Some("   ".to_string()),
        },
        &api,
    )
    .await
    .expect("reconnect with blank app token");
    assert!(
        !status.has_app_token,
        "F1: a blank app token on reconnect must delete the stored one, not leave the old one in place"
    );
}

/// S5c-F-02 bite (F2): TS writes `test.teamId ?? ""` for all three fields
/// unconditionally (`slack.ts:142-144`) - a reconnect whose `auth.test`
/// omits a field must blank the stored value, not keep the previous
/// workspace's. Before the fix, `if let Some(..) { put }` skipped the write
/// entirely on `None`, so a malformed-but-`ok:true` response left the first
/// workspace's team name behind.
#[tokio::test]
async fn reconnect_with_missing_team_name_blanks_it_instead_of_keeping_the_old_one() {
    use server::slack::connect_slack;

    let db = Db::open(":memory:").expect("open db");
    let first_api = FakeSlackApi {
        auth_test_result: None,
    };
    let status = connect_slack(
        &db,
        SlackConnectInput {
            bot_token: "xoxb-1".to_string(),
            signing_secret: "s1".to_string(),
            app_token: None,
        },
        &first_api,
    )
    .await
    .expect("first connect");
    assert_eq!(status.team_name, Some("test-team".to_string()));

    let second_api = FakeSlackApi {
        auth_test_result: Some(Ok(AuthTestResult {
            team_id: None,
            team_name: None,
            user_id: None,
        })),
    };
    let status = connect_slack(
        &db,
        SlackConnectInput {
            bot_token: "xoxb-2".to_string(),
            signing_secret: "s2".to_string(),
            app_token: None,
        },
        &second_api,
    )
    .await
    .expect("second connect");
    assert_eq!(
        status.team_name,
        Some(String::new()),
        "F2: a reconnect whose auth.test omits team_name must blank slack.team_name, not keep the first workspace's name"
    );
}

/// S5c-F-02 bite (F5): `store::Db::open` must wire `slack::
/// ensure_slack_tables` itself - a bare open, with nothing else touching
/// Slack, has to be able to insert into `slack_threads`. Before the fix this
/// table was only created by `AppState::build` (S5c-03's stopgap), so a
/// plain `store::Db::open` gave a database with no `slack_threads` at all.
#[test]
fn fresh_db_open_alone_can_insert_into_slack_threads() {
    let db = Db::open(":memory:").expect("open db");
    let now = chrono::Utc::now().to_rfc3339();
    db.conn()
        .execute(
            "INSERT INTO slack_threads (channel, thread_ts, bot_id, conversation_id, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params!["C1", "1.1", "arthur", "conv-1", now],
        )
        .expect(
            "F5: a bare Db::open must already have created slack_threads, with no separate ensure_slack_tables call",
        );
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
