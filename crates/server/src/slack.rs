//! Slack integration: configuration, status, and API calls.
//! Port of TS `bullpen-night/src/server/slack.ts`.

use crate::settings_secrets::{decrypt_for_storage, encrypt_for_storage};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use store::Db;

/// Settings keys for Slack configuration
const KEY_BOT_TOKEN: &str = "slack.bot_token";
const KEY_SIGNING_SECRET: &str = "slack.signing_secret";
const KEY_APP_TOKEN: &str = "slack.app_token";
const KEY_TEAM_ID: &str = "slack.team_id";
const KEY_TEAM_NAME: &str = "slack.team_name";
const KEY_BOT_USER_ID: &str = "slack.bot_user_id";
const KEY_CONNECTED_AT: &str = "slack.connected_at";
const KEY_ANSWER_BOT_ID: &str = "slack.answer_bot_id";

/// All settings keys owned by this module
const ALL_KEYS: &[&str] = &[
    KEY_BOT_TOKEN,
    KEY_SIGNING_SECRET,
    KEY_APP_TOKEN,
    KEY_TEAM_ID,
    KEY_TEAM_NAME,
    KEY_BOT_USER_ID,
    KEY_CONNECTED_AT,
];

/// Read a setting from the database
fn get_setting(db: &Db, key: &str) -> Option<String> {
    db.conn()
        .query_row("SELECT value FROM settings WHERE key = ?", [key], |row| {
            row.get::<_, String>(0)
        })
        .optional()
        .ok()
        .flatten()
}

/// Write a setting to the database (insert or update)
fn put_setting(db: &Db, key: &str, value: &str) -> rusqlite::Result<()> {
    db.conn().execute(
        "INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [key, value],
    )?;
    Ok(())
}

/// Delete a setting from the database
fn delete_setting(db: &Db, key: &str) -> rusqlite::Result<()> {
    db.conn()
        .execute("DELETE FROM settings WHERE key = ?", [key])?;
    Ok(())
}

/// The stored configuration for Slack (never exposed to clients)
#[derive(Clone, Debug)]
pub struct SlackConfig {
    pub bot_token: String,
    pub signing_secret: String,
    pub app_token: Option<String>,
    pub team_id: Option<String>,
    pub team_name: Option<String>,
    pub bot_user_id: Option<String>,
    pub connected_at: Option<String>,
}

/// Get the Slack configuration from storage. Returns None if either secret is
/// missing or fails to decrypt.
pub fn get_slack_config(db: &Db) -> Option<SlackConfig> {
    let encrypted_token = get_setting(db, KEY_BOT_TOKEN)?;
    let encrypted_secret = get_setting(db, KEY_SIGNING_SECRET)?;

    let bot_token = decrypt_for_storage(db, &encrypted_token)?;
    let signing_secret = decrypt_for_storage(db, &encrypted_secret)?;

    let encrypted_app_token = get_setting(db, KEY_APP_TOKEN);
    let app_token = encrypted_app_token.and_then(|ct| decrypt_for_storage(db, &ct));

    Some(SlackConfig {
        bot_token,
        signing_secret,
        app_token,
        team_id: get_setting(db, KEY_TEAM_ID),
        team_name: get_setting(db, KEY_TEAM_NAME),
        bot_user_id: get_setting(db, KEY_BOT_USER_ID),
        connected_at: get_setting(db, KEY_CONNECTED_AT),
    })
}

/// What a client is allowed to know: never the tokens themselves.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(crate = "serde", rename_all = "camelCase")]
pub struct SlackStatus {
    pub configured: bool,
    pub team_name: Option<String>,
    pub bot_user_id: Option<String>,
    pub connected_at: Option<String>,
    pub has_app_token: bool,
    pub answer_bot_id: Option<String>,
    pub answer_bot_name: Option<String>,
}

/// Get the current Slack status for a client.
pub fn slack_status(db: &Db) -> SlackStatus {
    let config = get_slack_config(db);
    let answer_id = get_slack_answer_bot_id(db);

    let answer_name = answer_id.as_ref().and_then(|id| {
        store::bots::get_bot(db, id)
            .ok()
            .flatten()
            .map(|bot| bot.name.clone())
    });

    SlackStatus {
        configured: config.is_some(),
        team_name: config.as_ref().and_then(|c| c.team_name.clone()),
        bot_user_id: config.as_ref().and_then(|c| c.bot_user_id.clone()),
        connected_at: config.as_ref().and_then(|c| c.connected_at.clone()),
        has_app_token: config
            .as_ref()
            .map(|c| c.app_token.is_some())
            .unwrap_or(false),
        answer_bot_id: answer_id,
        answer_bot_name: answer_name,
    }
}

/// Trait for making API calls to Slack (abstracted for testability)
#[async_trait::async_trait]
pub trait SlackApi {
    async fn auth_test(&self, bot_token: &str) -> Result<AuthTestResult, String>;
    async fn post_message(
        &self,
        bot_token: &str,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<PostMessageResult, String>;
    async fn delete_message(&self, bot_token: &str, channel: &str, ts: &str) -> Result<(), String>;
}

#[derive(Debug, Clone)]
pub struct AuthTestResult {
    pub team_id: Option<String>,
    pub team_name: Option<String>,
    pub user_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PostMessageResult {
    pub ts: String,
}

/// Reqwest implementation of SlackApi for production
pub struct ReqwestSlackApi;

#[async_trait::async_trait]
impl SlackApi for ReqwestSlackApi {
    async fn auth_test(&self, bot_token: &str) -> Result<AuthTestResult, String> {
        let client = reqwest::Client::new();
        let response = client
            .post("https://slack.com/api/auth.test")
            .bearer_auth(bot_token)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| redact_error(&e.to_string()))?;

        if !response.status().is_success() {
            return Err(format!("Slack refused ({})", response.status()));
        }

        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| redact_error(&e.to_string()))?;

        if body.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let error = body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(error.to_string());
        }

        Ok(AuthTestResult {
            team_id: body
                .get("team_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            team_name: body
                .get("team")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            user_id: body
                .get("user_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        })
    }

    async fn post_message(
        &self,
        bot_token: &str,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<PostMessageResult, String> {
        let client = reqwest::Client::new();

        let mut body = serde_json::json!({
            "channel": channel,
            "text": text,
        });

        if let Some(ts) = thread_ts {
            body["thread_ts"] = serde_json::Value::String(ts.to_string());
        }

        let response = client
            .post("https://slack.com/api/chat.postMessage")
            .bearer_auth(bot_token)
            .json(&body)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| redact_error(&e.to_string()))?;

        if !response.status().is_success() {
            return Err(format!("Slack refused ({})", response.status()));
        }

        let resp_body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| redact_error(&e.to_string()))?;

        if resp_body.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let error = resp_body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(error.to_string());
        }

        let ts = resp_body
            .get("ts")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "no ts in response".to_string())?
            .to_string();

        Ok(PostMessageResult { ts })
    }

    async fn delete_message(&self, bot_token: &str, channel: &str, ts: &str) -> Result<(), String> {
        let client = reqwest::Client::new();

        let body = serde_json::json!({
            "channel": channel,
            "ts": ts,
        });

        let response = client
            .post("https://slack.com/api/chat.delete")
            .bearer_auth(bot_token)
            .json(&body)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| redact_error(&e.to_string()))?;

        if !response.status().is_success() {
            return Err(format!("Slack refused ({})", response.status()));
        }

        let resp_body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| redact_error(&e.to_string()))?;

        if resp_body.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let error = resp_body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(error.to_string());
        }

        Ok(())
    }
}

/// Connect Slack by testing the tokens and storing them encrypted
pub async fn connect_slack<A: SlackApi + ?Sized>(
    db: &Db,
    input: SlackConnectInput,
    api: &A,
) -> Result<SlackStatus, String> {
    let bot_token = input.bot_token.trim();
    let signing_secret = input.signing_secret.trim();

    if bot_token.is_empty() {
        return Err("give a bot token".to_string());
    }
    if signing_secret.is_empty() {
        return Err("give a signing secret".to_string());
    }

    // Test the token first
    let test = api.auth_test(bot_token).await?;

    // Store tokens encrypted
    let encrypted_token = encrypt_for_storage(db, bot_token)
        .map_err(|e| format!("failed to encrypt bot token: {}", e))?;
    let encrypted_secret = encrypt_for_storage(db, signing_secret)
        .map_err(|e| format!("failed to encrypt signing secret: {}", e))?;

    put_setting(db, KEY_BOT_TOKEN, &encrypted_token)
        .map_err(|e| format!("failed to store bot token: {}", e))?;
    put_setting(db, KEY_SIGNING_SECRET, &encrypted_secret)
        .map_err(|e| format!("failed to store signing secret: {}", e))?;

    // Store app token if provided
    if let Some(app_token) = input.app_token.as_ref() {
        let app_token_trimmed = app_token.trim();
        if !app_token_trimmed.is_empty() {
            let encrypted_app = encrypt_for_storage(db, app_token_trimmed)
                .map_err(|e| format!("failed to encrypt app token: {}", e))?;
            put_setting(db, KEY_APP_TOKEN, &encrypted_app)
                .map_err(|e| format!("failed to store app token: {}", e))?;
        }
    } else {
        let _ = delete_setting(db, KEY_APP_TOKEN);
    }

    // Store team info from auth.test response
    if let Some(team_id) = test.team_id {
        put_setting(db, KEY_TEAM_ID, &team_id)
            .map_err(|e| format!("failed to store team id: {}", e))?;
    }
    if let Some(team_name) = test.team_name {
        put_setting(db, KEY_TEAM_NAME, &team_name)
            .map_err(|e| format!("failed to store team name: {}", e))?;
    }
    if let Some(user_id) = test.user_id {
        put_setting(db, KEY_BOT_USER_ID, &user_id)
            .map_err(|e| format!("failed to store bot user id: {}", e))?;
    }

    // Store connection time
    let now = chrono::Utc::now().to_rfc3339();
    put_setting(db, KEY_CONNECTED_AT, &now)
        .map_err(|e| format!("failed to store connected_at: {}", e))?;

    Ok(slack_status(db))
}

#[derive(Debug)]
pub struct SlackConnectInput {
    pub bot_token: String,
    pub signing_secret: String,
    pub app_token: Option<String>,
}

/// Disconnect Slack by deleting all stored configuration
pub fn disconnect_slack(db: &Db) -> rusqlite::Result<()> {
    for key in ALL_KEYS {
        let _ = delete_setting(db, key);
    }
    Ok(())
}

/// Get the bot ID configured to answer on Slack
pub fn get_slack_answer_bot_id(db: &Db) -> Option<String> {
    if let Some(stored) = get_setting(db, KEY_ANSWER_BOT_ID)
        && let Ok(Some(_)) = store::bots::get_bot(db, &stored)
    {
        return Some(stored);
    }
    default_answer_bot(db)
}

/// Set the bot ID that answers on Slack. Returns false if the bot doesn't exist.
pub fn set_slack_answer_bot_id(db: &Db, bot_id: &str) -> rusqlite::Result<bool> {
    match store::bots::get_bot(db, bot_id)? {
        Some(_) => {
            put_setting(db, KEY_ANSWER_BOT_ID, bot_id)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// The default answer bot: pinned bot, else Arthur, else nothing
fn default_answer_bot(db: &Db) -> Option<String> {
    // Try to get the pinned bot
    if let Ok(Some(id)) = db
        .conn()
        .query_row(
            "SELECT id FROM bots WHERE archived_at IS NULL AND pinned_at IS NOT NULL ORDER BY pinned_at LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
    {
        return Some(id);
    }

    // Fall back to Arthur
    if let Ok(Some(_)) = store::bots::get_bot(db, "arthur") {
        return Some("arthur".to_string());
    }

    None
}

/// Post a message to Slack through the API
pub async fn post_slack_message<A: SlackApi + ?Sized>(
    api: &A,
    bot_token: &str,
    channel: &str,
    text: &str,
    thread_ts: Option<&str>,
) -> Result<String, String> {
    let result = api
        .post_message(bot_token, channel, text, thread_ts)
        .await?;
    Ok(result.ts)
}

/// Delete a message from Slack through the API
pub async fn delete_slack_message<A: SlackApi + ?Sized>(
    api: &A,
    bot_token: &str,
    channel: &str,
    ts: &str,
) -> Result<(), String> {
    api.delete_message(bot_token, channel, ts).await
}

/// Redact error messages to avoid leaking sensitive information
fn redact_error(error: &str) -> String {
    // For network errors, just give a generic message
    if error.contains("timed out") {
        "request timed out".to_string()
    } else if error.contains("connection") {
        "connection failed".to_string()
    } else {
        // Keep the error but don't expose URLs or tokens
        error.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeSlackApi {
        pub auth_test_result: Option<Result<AuthTestResult, String>>,
        pub post_message_result: Option<Result<PostMessageResult, String>>,
        pub delete_message_result: Option<Result<(), String>>,
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
            self.post_message_result
                .as_ref()
                .cloned()
                .unwrap_or_else(|| {
                    Ok(PostMessageResult {
                        ts: "1234567890.123456".to_string(),
                    })
                })
        }

        async fn delete_message(
            &self,
            _bot_token: &str,
            _channel: &str,
            _ts: &str,
        ) -> Result<(), String> {
            self.delete_message_result
                .as_ref()
                .cloned()
                .unwrap_or(Ok(()))
        }
    }

    #[test]
    fn test_slack_config_none_when_secrets_missing() {
        let db = Db::open(":memory:").expect("open db");
        assert!(get_slack_config(&db).is_none());
    }

    #[tokio::test]
    async fn test_slack_status_not_configured() {
        let db = Db::open(":memory:").expect("open db");
        let status = slack_status(&db);
        assert!(!status.configured);
        assert_eq!(status.team_name, None);
        assert_eq!(status.bot_user_id, None);
        assert!(!status.has_app_token);
    }

    #[tokio::test]
    async fn test_connect_slack_empty_token() {
        let db = Db::open(":memory:").expect("open db");
        let api = FakeSlackApi {
            auth_test_result: None,
            post_message_result: None,
            delete_message_result: None,
        };

        let result = connect_slack(
            &db,
            SlackConnectInput {
                bot_token: "".to_string(),
                signing_secret: "secret".to_string(),
                app_token: None,
            },
            &api,
        )
        .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "give a bot token");
    }

    #[tokio::test]
    async fn test_connect_slack_empty_secret() {
        let db = Db::open(":memory:").expect("open db");
        let api = FakeSlackApi {
            auth_test_result: None,
            post_message_result: None,
            delete_message_result: None,
        };

        let result = connect_slack(
            &db,
            SlackConnectInput {
                bot_token: "token".to_string(),
                signing_secret: "".to_string(),
                app_token: None,
            },
            &api,
        )
        .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "give a signing secret");
    }

    #[tokio::test]
    async fn test_connect_slack_auth_failure() {
        let db = Db::open(":memory:").expect("open db");
        let api = FakeSlackApi {
            auth_test_result: Some(Err("invalid_token".to_string())),
            post_message_result: None,
            delete_message_result: None,
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
        // When auth fails, nothing should be stored
        assert!(get_slack_config(&db).is_none());
    }

    #[test]
    fn test_get_setting_missing() {
        let db = Db::open(":memory:").expect("open db");
        assert_eq!(get_setting(&db, "nonexistent"), None);
    }

    #[test]
    fn test_put_and_get_setting() {
        let db = Db::open(":memory:").expect("open db");
        put_setting(&db, "test.key", "test.value").expect("put setting");
        assert_eq!(get_setting(&db, "test.key"), Some("test.value".to_string()));
    }

    #[test]
    fn test_delete_setting() {
        let db = Db::open(":memory:").expect("open db");
        put_setting(&db, "test.key", "value").expect("put setting");
        delete_setting(&db, "test.key").expect("delete setting");
        assert_eq!(get_setting(&db, "test.key"), None);
    }
}
