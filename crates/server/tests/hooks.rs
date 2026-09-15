use serde_json::json;
use server::hooks::*;

#[test]
fn reduce_github_ping_is_none() {
    let payload = json!({
        "zen": "Design for failure.",
        "hook_id": 1,
        "repository": {
            "full_name": "test/repo"
        }
    });
    assert_eq!(reduce_github("ping", &payload), None);
}

#[test]
fn reduce_github_pull_request_opened() {
    let payload = json!({
        "action": "opened",
        "number": 123,
        "pull_request": {
            "title": "Add feature",
            "merged": false,
            "html_url": "https://github.com/test/repo/pull/123",
            "user": {
                "login": "alice"
            }
        }
    });
    let result = reduce_github("pull_request", &payload);
    assert_eq!(
        result,
        Some(
            "PR #123 opened by alice: Add feature (https://github.com/test/repo/pull/123)"
                .to_string()
        )
    );
}

#[test]
fn reduce_github_pull_request_closed_and_merged() {
    let payload = json!({
        "action": "closed",
        "number": 42,
        "pull_request": {
            "title": "Fix bug",
            "merged": true,
            "html_url": "https://github.com/test/repo/pull/42",
            "user": {
                "login": "bob"
            }
        }
    });
    let result = reduce_github("pull_request", &payload);
    assert_eq!(
        result,
        Some("PR #42 merged by bob: Fix bug (https://github.com/test/repo/pull/42)".to_string())
    );
}

#[test]
fn reduce_github_issue_opened() {
    let payload = json!({
        "action": "opened",
        "issue": {
            "number": 99,
            "title": "Bug report",
            "html_url": "https://github.com/test/repo/issues/99",
            "user": {
                "login": "charlie"
            }
        }
    });
    let result = reduce_github("issues", &payload);
    assert_eq!(
        result,
        Some(
            "Issue #99 opened by charlie: Bug report (https://github.com/test/repo/issues/99)"
                .to_string()
        )
    );
}

#[test]
fn reduce_github_issue_comment() {
    let payload = json!({
        "action": "created",
        "comment": {
            "body": "This is a comment with some feedback on the pull request",
            "user": {
                "login": "dave"
            }
        },
        "issue": {
            "number": 55,
            "title": "Something"
        }
    });
    let result = reduce_github("issue_comment", &payload);
    assert_eq!(
        result,
        Some(
            "comment by dave on #55: This is a comment with some feedback on the pull request"
                .to_string()
        )
    );
}

#[test]
fn reduce_github_issue_comment_truncated() {
    let long_body = "x".repeat(300);
    let payload = json!({
        "action": "created",
        "comment": {
            "body": &long_body,
            "user": {
                "login": "dave"
            }
        },
        "issue": {
            "number": 55
        }
    });
    let result = reduce_github("issue_comment", &payload);
    let expected = format!("comment by dave on #55: {}", "x".repeat(200));
    assert_eq!(result, Some(expected));
}

#[test]
fn reduce_github_push() {
    let payload = json!({
        "ref": "refs/heads/main",
        "commits": [
            {
                "message": "Add feature\nWith details"
            },
            {
                "message": "Fix typo"
            }
        ],
        "pusher": {
            "name": "eve"
        }
    });
    let result = reduce_github("push", &payload);
    assert_eq!(
        result,
        Some("push to main by eve: 2 commits, last: Fix typo".to_string())
    );
}

#[test]
fn reduce_github_workflow_run_success() {
    let payload = json!({
        "action": "completed",
        "workflow_run": {
            "name": "Tests",
            "conclusion": "success",
            "head_branch": "feature",
            "html_url": "https://github.com/test/repo/actions/runs/123"
        }
    });
    let result = reduce_github("workflow_run", &payload);
    assert_eq!(
        result,
        Some(
            "workflow Tests succeeded on feature (https://github.com/test/repo/actions/runs/123)"
                .to_string()
        )
    );
}

#[test]
fn reduce_github_generic_event() {
    let payload = json!({
        "repository": {
            "full_name": "test/repo"
        }
    });
    let result = reduce_github("release", &payload);
    assert_eq!(result, Some("github release on test/repo".to_string()));
}

#[test]
fn reduce_sentry_with_event() {
    let payload = json!({
        "data": {
            "event": {
                "level": "error",
                "title": "IndexError",
                "culprit": "myapp.views",
                "web_url": "https://sentry.io/errors/123"
            }
        }
    });
    let result = reduce_sentry(&payload);
    assert_eq!(
        result,
        Some("Sentry: error IndexError in myapp.views (https://sentry.io/errors/123)".to_string())
    );
}

#[test]
fn reduce_sentry_empty_event() {
    let payload = json!({
        "data": {
            "event": {}
        }
    });
    assert_eq!(reduce_sentry(&payload), None);
}

#[test]
fn reduce_sentry_no_data() {
    let payload = json!({});
    assert_eq!(reduce_sentry(&payload), None);
}

#[test]
fn reduce_linear_issue_created() {
    let payload = json!({
        "action": "create",
        "data": {
            "identifier": "PROJ-123",
            "title": "New feature",
            "creator": {
                "displayName": "frank"
            }
        }
    });
    let result = reduce_linear("Issue", &payload);
    assert_eq!(
        result,
        Some("Issue PROJ-123 created by frank: New feature".to_string())
    );
}

#[test]
fn reduce_linear_issue_status_changed() {
    let payload = json!({
        "action": "update",
        "data": {
            "identifier": "PROJ-456",
            "title": "Bug fix",
            "state": {
                "name": "In Progress"
            }
        }
    });
    let result = reduce_linear("Issue", &payload);
    assert_eq!(
        result,
        Some("Issue PROJ-456 status changed to In Progress: Bug fix".to_string())
    );
}

#[test]
fn reduce_linear_cycle_ended() {
    let payload = json!({
        "action": "create",
        "data": {
            "displayIdentifier": "Sprint 5"
        }
    });
    let result = reduce_linear("Cycle", &payload);
    assert_eq!(result, Some("Cycle ended: Sprint 5".to_string()));
}

#[test]
fn reduce_pager_duty_incident_triggered() {
    let payload = json!({
        "incidents": [
            {
                "incident_number": "PD-001",
                "title": "Database down",
                "urgency": "high"
            }
        ]
    });
    let result = reduce_pager_duty("incident.triggered", &payload);
    assert_eq!(
        result,
        Some("Incident #PD-001 triggered: Database down, urgency high".to_string())
    );
}

#[test]
fn reduce_pager_duty_incident_acknowledged() {
    let payload = json!({
        "incidents": [
            {
                "incident_number": "PD-002",
                "title": "Server error"
            }
        ]
    });
    let result = reduce_pager_duty("incident.acknowledged", &payload);
    assert_eq!(
        result,
        Some("Incident #PD-002 acknowledged: Server error".to_string())
    );
}

#[test]
fn reduce_pager_duty_incident_resolved() {
    let payload = json!({
        "incidents": [
            {
                "incident_number": "PD-003",
                "title": "API latency"
            }
        ]
    });
    let result = reduce_pager_duty("incident.resolved", &payload);
    assert_eq!(
        result,
        Some("Incident #PD-003 resolved: API latency".to_string())
    );
}

#[test]
fn reduce_pager_duty_no_incidents() {
    let payload = json!({
        "incidents": []
    });
    assert_eq!(reduce_pager_duty("incident.triggered", &payload), None);
}

#[test]
fn reduce_slack_app_mention() {
    let event = json!({
        "type": "app_mention",
        "user": "U01234567",
        "text": "Hey bot, what's the status?"
    });
    let result = reduce_slack(&event);
    assert_eq!(
        result,
        Some("Slack: U01234567 mentioned the app: Hey bot, what's the status?".to_string())
    );
}

#[test]
fn reduce_slack_message_in_channel() {
    let event = json!({
        "type": "message",
        "user": "U89012345",
        "channel_type": "channel",
        "text": "Let me check the logs"
    });
    let result = reduce_slack(&event);
    assert_eq!(
        result,
        Some("Slack: U89012345 said in a channel: Let me check the logs".to_string())
    );
}

#[test]
fn reduce_slack_message_in_dm() {
    let event = json!({
        "type": "message",
        "user": "U11111111",
        "channel_type": "im",
        "text": "Direct message here"
    });
    let result = reduce_slack(&event);
    assert_eq!(
        result,
        Some("Slack: U11111111 said in a DM: Direct message here".to_string())
    );
}

#[test]
fn reduce_slack_reaction_added() {
    let event = json!({
        "type": "reaction_added",
        "user": "U22222222",
        "reaction": "thumbsup"
    });
    let result = reduce_slack(&event);
    assert!(result.is_some());
    assert!(result.unwrap().contains("thumbsup"));
}

#[test]
fn reduce_slack_empty_reaction() {
    let event = json!({
        "type": "reaction_added",
        "user": "U33333333",
        "reaction": ""
    });
    assert_eq!(reduce_slack(&event), None);
}

#[test]
fn mentions_slack_user_found() {
    assert!(mentions_slack_user("<@U01234567> what's up?", "U01234567"));
    assert!(mentions_slack_user("Hey <@U01234567> there", "U01234567"));
}

#[test]
fn mentions_slack_user_not_found() {
    assert!(!mentions_slack_user("<@U01234567> what's up?", "U99999999"));
    assert!(!mentions_slack_user("No mention here", "U01234567"));
}

#[test]
fn mentions_slack_user_empty_id() {
    assert!(!mentions_slack_user("Some text", ""));
}

#[test]
fn verify_github_signature_correct() {
    let secret = "my-secret";
    let body = "{\"test\": \"payload\"}";

    // Note: For testing, we need to compute the actual HMAC
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    let result = mac.finalize();
    let computed_header = format!("sha256={}", hex::encode(result.into_bytes()));

    assert!(verify_github_signature(
        secret,
        body,
        Some(&computed_header)
    ));
}

#[test]
fn verify_github_signature_wrong_signature() {
    let secret = "my-secret";
    let body = "{\"test\": \"payload\"}";
    let wrong_header = "sha256=0000000000000000000000000000000000000000000000000000000000000000";

    assert!(!verify_github_signature(secret, body, Some(wrong_header)));
}

#[test]
fn verify_github_signature_empty_header() {
    let secret = "my-secret";
    let body = "{\"test\": \"payload\"}";

    assert!(!verify_github_signature(secret, body, None));
    assert!(!verify_github_signature(secret, body, Some("")));
}

#[test]
fn verify_github_signature_length_mismatch() {
    let secret = "my-secret";
    let body = "{\"test\": \"payload\"}";
    let too_short = "sha256=abc";

    assert!(!verify_github_signature(secret, body, Some(too_short)));
}

#[test]
fn verify_linear_signature_correct() {
    let secret = "linear-secret";
    let body = "{\"action\": \"create\"}";

    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    let result = mac.finalize();
    let computed_header = format!("sha256={}", hex::encode(result.into_bytes()));

    assert!(verify_linear_signature(
        secret,
        body,
        Some(&computed_header)
    ));
}

#[test]
fn verify_linear_signature_wrong_signature() {
    let secret = "linear-secret";
    let body = "{\"action\": \"create\"}";
    let wrong_header = "sha256=ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

    assert!(!verify_linear_signature(secret, body, Some(wrong_header)));
}

#[test]
fn verify_pager_duty_signature_correct() {
    let secret = "pd-secret";
    let body = "{\"incidents\": []}";

    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    let result = mac.finalize();
    let computed_header = format!("v1={}", hex::encode(result.into_bytes()));

    assert!(verify_pager_duty_signature(
        secret,
        body,
        Some(&computed_header)
    ));
}

#[test]
fn verify_pager_duty_signature_wrong_signature() {
    let secret = "pd-secret";
    let body = "{\"incidents\": []}";
    let wrong_header = "v1=0000000000000000000000000000000000000000000000000000000000000000";

    assert!(!verify_pager_duty_signature(
        secret,
        body,
        Some(wrong_header)
    ));
}

#[test]
fn verify_pager_duty_signature_invalid_format() {
    let secret = "pd-secret";
    let body = "{\"incidents\": []}";
    let invalid = "v2=abcdef";

    assert!(!verify_pager_duty_signature(secret, body, Some(invalid)));
}

#[test]
fn verify_slack_signature_correct() {
    let secret = "slack-secret";
    let body = "{\"type\": \"url_verification\"}";
    let timestamp = "1234567890";
    let now = 1234567890u64;

    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    type HmacSha256 = Hmac<Sha256>;
    let base = format!("v0:{}:{}", timestamp, body);
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(base.as_bytes());
    let result = mac.finalize();
    let computed_signature = format!("v0={}", hex::encode(result.into_bytes()));

    assert!(verify_slack_signature(
        secret,
        body,
        Some(timestamp),
        Some(&computed_signature),
        now
    ));
}

#[test]
fn verify_slack_signature_wrong_signature() {
    let secret = "slack-secret";
    let body = "{\"type\": \"url_verification\"}";
    let timestamp = "1234567890";
    let now = 1234567890u64;
    let wrong_signature = "v0=0000000000000000000000000000000000000000000000000000000000000000";

    assert!(!verify_slack_signature(
        secret,
        body,
        Some(timestamp),
        Some(wrong_signature),
        now
    ));
}

#[test]
fn verify_slack_signature_replay_window_exceeded() {
    let secret = "slack-secret";
    let body = "{\"type\": \"url_verification\"}";
    let timestamp = "1000000000";
    let now = 1000000000u64 + SLACK_REPLAY_WINDOW_SECONDS + 1;

    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    type HmacSha256 = Hmac<Sha256>;
    let base = format!("v0:{}:{}", timestamp, body);
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(base.as_bytes());
    let result = mac.finalize();
    let computed_signature = format!("v0={}", hex::encode(result.into_bytes()));

    assert!(!verify_slack_signature(
        secret,
        body,
        Some(timestamp),
        Some(&computed_signature),
        now
    ));
}

#[test]
fn verify_slack_signature_empty_timestamp() {
    let secret = "slack-secret";
    let body = "{\"type\": \"url_verification\"}";
    let signature = "v0=abc123";
    let now = 1234567890u64;

    assert!(!verify_slack_signature(
        secret,
        body,
        Some(""),
        Some(signature),
        now
    ));
}

#[test]
fn verify_slack_signature_invalid_timestamp() {
    let secret = "slack-secret";
    let body = "{\"type\": \"url_verification\"}";
    let signature = "v0=abc123";
    let now = 1234567890u64;

    assert!(!verify_slack_signature(
        secret,
        body,
        Some("not-a-number"),
        Some(signature),
        now
    ));
}
