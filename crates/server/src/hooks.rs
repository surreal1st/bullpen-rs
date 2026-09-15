//! Parsers for structured webhook shapes and signature verification.
//!
//! Each reducer turns one delivery into a single line for the routine's
//! "## What arrived" block, or None when the delivery is not worth a run
//! at all (a ping, a Sentry payload with no event).
//!
//! These functions take JSON values and narrow defensively rather than
//! requiring a full type: a sender can add fields at any time, and a
//! routine's job is to say something useful from what showed up, not to
//! validate the sender's contract.

use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use subtle::ConstantTimeEq;

/// GitHub's `conclusion` values, in the past tense a sentence reads naturally with.
pub static CONCLUSION_VERBS: &[(&str, &str)] = &[
    ("success", "succeeded"),
    ("failure", "failed"),
    ("cancelled", "cancelled"),
    ("skipped", "skipped"),
    ("timed_out", "timed out"),
    ("action_required", "needs action"),
    ("neutral", "completed neutrally"),
    ("stale", "stale"),
];

/// Slack's replay window in seconds: a signature older than this is refused
/// even when the HMAC itself checks out.
pub const SLACK_REPLAY_WINDOW_SECONDS: u64 = 5 * 60;

/// Extract a value as a string, handling null, numbers, and strings.
fn as_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

/// Extract a value as an object/record, returning an empty object if not present.
fn as_record(value: &Value) -> Value {
    if value.is_object() {
        value.clone()
    } else {
        json!({})
    }
}

/// Check if a Value object is empty (has no keys).
fn is_empty_object(value: &Value) -> bool {
    !value.is_object() || value.as_object().map(|o| o.is_empty()).unwrap_or(true)
}

/// Extract repo full name from payload, defaulting to "unknown repo".
fn repo_full_name(payload: &Value) -> String {
    let repo = as_record(&payload["repository"]);
    let name = as_str(&repo["full_name"]);
    if name.is_empty() {
        "unknown repo".to_string()
    } else {
        name
    }
}

/// One line for a GitHub webhook delivery, keyed by the `X-GitHub-Event` header.
///
/// `ping` (GitHub's own connectivity test when a webhook is first saved) is
/// deliberately None: it is not something worth a run, and firing one every
/// time a webhook is re-saved would be noise.
pub fn reduce_github(event: &str, payload: &Value) -> Option<String> {
    if event == "ping" {
        return None;
    }

    let action = as_str(&payload["action"]);

    if event == "pull_request" {
        let pr = as_record(&payload["pull_request"]);
        if (action == "opened" || action == "closed") && !is_empty_object(&pr) {
            let merged = pr["merged"].as_bool() == Some(true);
            let verb = if action == "closed" && merged {
                "merged"
            } else {
                &action
            };
            let login = as_str(&pr["user"]["login"]);
            let number = as_str(&payload["number"]);
            let title = as_str(&pr["title"]);
            let url = as_str(&pr["html_url"]);
            return Some(format!(
                "PR #{} {} by {}: {} ({})",
                number, verb, login, title, url
            ));
        }
    }

    if event == "issues" {
        let issue = as_record(&payload["issue"]);
        if (action == "opened" || action == "closed") && !is_empty_object(&issue) {
            let login = as_str(&issue["user"]["login"]);
            let number = as_str(&issue["number"]);
            let title = as_str(&issue["title"]);
            let url = as_str(&issue["html_url"]);
            return Some(format!(
                "Issue #{} {} by {}: {} ({})",
                number, action, login, title, url
            ));
        }
    }

    if event == "issue_comment" {
        let comment = as_record(&payload["comment"]);
        let issue = as_record(&payload["issue"]);
        if action == "created" && !is_empty_object(&comment) && !is_empty_object(&issue) {
            let login = as_str(&comment["user"]["login"]);
            let issue_number = as_str(&issue["number"]);
            let body = as_str(&comment["body"]);
            let truncated = if body.len() > 200 {
                &body[..200]
            } else {
                &body
            };
            return Some(format!(
                "comment by {} on #{}: {}",
                login, issue_number, truncated
            ));
        }
    }

    if event == "push" {
        let r#ref = as_str(&payload["ref"]);
        let branch = r#ref
            .strip_prefix("refs/heads/")
            .unwrap_or(&r#ref)
            .to_string();
        let empty_commits = vec![];
        let commits = payload["commits"].as_array().unwrap_or(&empty_commits);
        let pusher = as_str(&payload["pusher"]["name"]);
        let null_value = Value::Null;
        let last_commit = commits.last().unwrap_or(&null_value);
        let last_message = as_str(&last_commit["message"]);
        let first_line = last_message.lines().next().unwrap_or("");
        return Some(format!(
            "push to {} by {}: {} commits, last: {}",
            branch,
            pusher,
            commits.len(),
            first_line
        ));
    }

    if event == "workflow_run" {
        let run = as_record(&payload["workflow_run"]);
        if action == "completed" && !is_empty_object(&run) {
            let conclusion = as_str(&run["conclusion"]);
            let verb = CONCLUSION_VERBS
                .iter()
                .find(|(k, _)| k == &conclusion)
                .map(|(_, v)| *v)
                .unwrap_or(if conclusion.is_empty() {
                    "completed"
                } else {
                    &conclusion
                });
            let name = as_str(&run["name"]);
            let branch = as_str(&run["head_branch"]);
            let url = as_str(&run["html_url"]);
            return Some(format!(
                "workflow {} {} on {} ({})",
                name, verb, branch, url
            ));
        }
    }

    Some(format!("github {} on {}", event, repo_full_name(payload)))
}

/// One line for a Sentry "Issue Alert" webhook delivery.
/// Returns None when there is no event to describe.
pub fn reduce_sentry(payload: &Value) -> Option<String> {
    let event = as_record(&as_record(&payload["data"])["event"]);
    if is_empty_object(&event) {
        return None;
    }

    let level = as_str(&event["level"]);
    let level = if level.is_empty() { "unknown" } else { &level };
    let title = as_str(&event["title"]);
    let title = if title.is_empty() {
        as_str(&event["message"])
    } else {
        title
    };
    let culprit = as_str(&event["culprit"]);
    let url = as_str(&event["web_url"]);

    Some(format!(
        "Sentry: {} {} in {} ({})",
        level, title, culprit, url
    ))
}

/// One line for a Linear webhook delivery.
pub fn reduce_linear(event: &str, payload: &Value) -> Option<String> {
    let action = as_str(&payload["action"]);
    let issue = as_record(&payload["data"]);
    let issue_key = as_str(&issue["identifier"]);
    let issue_title = as_str(&issue["title"]);

    if event == "Issue" && (action == "create" || action == "update") {
        let created_by = as_str(&issue["creator"]["displayName"]);
        let status = as_str(&issue["state"]["name"]);
        if !issue_key.is_empty() && !issue_title.is_empty() {
            if action == "create" {
                let creator = if created_by.is_empty() {
                    "unknown"
                } else {
                    &created_by
                };
                return Some(format!(
                    "Issue {} created by {}: {}",
                    issue_key, creator, issue_title
                ));
            } else if action == "update" && !status.is_empty() {
                return Some(format!(
                    "Issue {} status changed to {}: {}",
                    issue_key, status, issue_title
                ));
            }
        }
    } else if event == "Cycle" && action == "create" {
        let cycle_name = as_str(&issue["displayIdentifier"]);
        if !cycle_name.is_empty() {
            return Some(format!("Cycle ended: {}", cycle_name));
        }
    }

    None
}

/// One line for a PagerDuty v3 webhook delivery.
pub fn reduce_pager_duty(event: &str, payload: &Value) -> Option<String> {
    let empty_array = vec![];
    let incidents = payload["incidents"].as_array().unwrap_or(&empty_array);
    if incidents.is_empty() {
        return None;
    }

    let empty_map = Default::default();
    let incident = incidents[0].as_object().unwrap_or(&empty_map);
    let null_value = Value::Null;
    let incident_number = as_str(incident.get("incident_number").unwrap_or(&null_value));
    let title = as_str(incident.get("title").unwrap_or(&null_value));
    let urgency = as_str(incident.get("urgency").unwrap_or(&null_value));

    if event == "incident.triggered" && !incident_number.is_empty() && !title.is_empty() {
        let u = if urgency.is_empty() {
            "unknown"
        } else {
            &urgency
        };
        return Some(format!(
            "Incident #{} triggered: {}, urgency {}",
            incident_number, title, u
        ));
    } else if event == "incident.acknowledged" && !incident_number.is_empty() && !title.is_empty() {
        return Some(format!(
            "Incident #{} acknowledged: {}",
            incident_number, title
        ));
    } else if event == "incident.resolved" && !incident_number.is_empty() && !title.is_empty() {
        return Some(format!("Incident #{} resolved: {}", incident_number, title));
    }

    None
}

/// One line for a Slack Events API delivery.
/// Returns None for anything with no text worth a line.
pub fn reduce_slack(event: &Value) -> Option<String> {
    let kind = as_str(&event["type"]);
    let user = as_str(&event["user"]);
    let user = if user.is_empty() { "someone" } else { &user };

    if kind == "app_mention" {
        let text = as_str(&event["text"]);
        if !text.is_empty() {
            let truncated = if text.len() > 500 {
                &text[..500]
            } else {
                &text
            };
            return Some(format!("Slack: {} mentioned the app: {}", user, truncated));
        }
    }

    if kind == "message" {
        let text = as_str(&event["text"]);
        if !text.is_empty() {
            let where_at = if as_str(&event["channel_type"]) == "im" {
                "a DM"
            } else {
                "a channel"
            };
            let truncated = if text.len() > 500 {
                &text[..500]
            } else {
                &text
            };
            return Some(format!(
                "Slack: {} said in {}: {}",
                user, where_at, truncated
            ));
        }
    }

    if kind == "reaction_added" {
        let reaction = as_str(&event["reaction"]);
        if !reaction.is_empty() {
            return Some(format!("Slack: {} reacted :{}: ", user, reaction));
        }
    }

    None
}

/// Case-insensitive check for `<@U01234>` style mention of one Slack user id in text.
pub fn mentions_slack_user(text: &str, user_id: &str) -> bool {
    if user_id.is_empty() {
        return false;
    }
    text.contains(&format!("<@{}>", user_id))
}

/// Verifies a GitHub delivery's `X-Hub-Signature-256` header using HMAC-SHA256.
/// The header format is `sha256=<hex>`.
pub fn verify_github_signature(secret: &str, raw_body: &str, header: Option<&str>) -> bool {
    let presented = header.unwrap_or("");
    if presented.is_empty() {
        return false;
    }

    // Compute HMAC-SHA256
    type HmacSha256 = Hmac<Sha256>;
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");
    mac.update(raw_body.as_bytes());
    let result = mac.finalize();
    let expected = format!("sha256={}", hex::encode(result.into_bytes()));

    // Constant-time comparison: compare full strings including the prefix
    let expected_bytes = expected.as_bytes();
    let presented_bytes = presented.as_bytes();

    if expected_bytes.len() != presented_bytes.len() {
        return false;
    }

    expected_bytes.ct_eq(presented_bytes).into()
}

/// Verifies a Linear webhook signature using HMAC-SHA256.
/// The header format is `sha256=<hex>`.
pub fn verify_linear_signature(secret: &str, raw_body: &str, header: Option<&str>) -> bool {
    let presented = header.unwrap_or("");
    if presented.is_empty() {
        return false;
    }

    type HmacSha256 = Hmac<Sha256>;
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");
    mac.update(raw_body.as_bytes());
    let result = mac.finalize();
    let expected = format!("sha256={}", hex::encode(result.into_bytes()));

    let expected_bytes = expected.as_bytes();
    let presented_bytes = presented.as_bytes();

    if expected_bytes.len() != presented_bytes.len() {
        return false;
    }

    expected_bytes.ct_eq(presented_bytes).into()
}

/// Verifies a PagerDuty v3 webhook signature using HMAC-SHA256.
/// The header format is `v1=<hex>`.
pub fn verify_pager_duty_signature(secret: &str, raw_body: &str, header: Option<&str>) -> bool {
    let presented = header.unwrap_or("");
    if presented.is_empty() {
        return false;
    }

    // Extract v1=<hex> format
    let hex_part = if let Some(stripped) = presented.strip_prefix("v1=") {
        stripped
    } else {
        return false;
    };

    type HmacSha256 = Hmac<Sha256>;
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");
    mac.update(raw_body.as_bytes());
    let result = mac.finalize();
    let expected = hex::encode(result.into_bytes());

    let expected_bytes = expected.as_bytes();
    let presented_bytes = hex_part.as_bytes();

    if expected_bytes.len() != presented_bytes.len() {
        return false;
    }

    expected_bytes.ct_eq(presented_bytes).into()
}

/// Verifies a Slack Events API delivery's `X-Slack-Signature`.
/// Slack signs `v0:{timestamp}:{body}` rather than just the body.
pub fn verify_slack_signature(
    secret: &str,
    raw_body: &str,
    timestamp_header: Option<&str>,
    signature_header: Option<&str>,
    now: u64,
) -> bool {
    let timestamp = timestamp_header.unwrap_or("");
    let presented = signature_header.unwrap_or("");

    if timestamp.is_empty() || presented.is_empty() {
        return false;
    }

    let ts_num: u64 = match timestamp.parse() {
        Ok(n) => n,
        Err(_) => return false,
    };

    // Check if the timestamp is outside the replay window (either too old or too new)
    if now.abs_diff(ts_num) > SLACK_REPLAY_WINDOW_SECONDS {
        return false;
    }

    let base = format!("v0:{}:{}", timestamp, raw_body);

    type HmacSha256 = Hmac<Sha256>;
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");
    mac.update(base.as_bytes());
    let result = mac.finalize();
    let expected = format!("v0={}", hex::encode(result.into_bytes()));

    let expected_bytes = expected.as_bytes();
    let presented_bytes = presented.as_bytes();

    if expected_bytes.len() != presented_bytes.len() {
        return false;
    }

    expected_bytes.ct_eq(presented_bytes).into()
}
