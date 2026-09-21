//! APNs badge + alerts. Port of `projects/bullpen-night/src/server/push.ts`.

use async_trait::async_trait;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::Serialize;
use std::sync::{Arc, Mutex};
use store::{Db, push as store_push};

pub const TOPIC: &str = "io.rainmade.bullpen";

pub const HOST_SANDBOX: &str = "https://api.sandbox.push.apple.com";
pub const HOST_PRODUCTION: &str = "https://api.push.apple.com";

const KEY_FILE_VAR: &str = "BULLPEN_APNS_KEY_FILE";
const KEY_ID_VAR: &str = "BULLPEN_APNS_KEY_ID";
const TEAM_ID_VAR: &str = "BULLPEN_APNS_TEAM_ID";
const TOKEN_TTL_MS: i64 = 45 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushAlert {
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct ApnsCredential {
    key_pem: String,
    key_id: String,
    team_id: String,
}

#[derive(Serialize)]
struct ProviderClaims {
    iss: String,
    iat: i64,
}

static PROVIDER_CACHE: Mutex<Option<(String, i64, String)>> = Mutex::new(None);

pub fn get_credential() -> Option<ApnsCredential> {
    let file = std::env::var(KEY_FILE_VAR).ok().filter(|s| !s.is_empty())?;
    let key_id = std::env::var(KEY_ID_VAR).ok().filter(|s| !s.is_empty())?;
    let team_id = std::env::var(TEAM_ID_VAR).ok().filter(|s| !s.is_empty())?;
    let key_pem = std::fs::read_to_string(&file).ok()?;
    if !key_pem.contains("BEGIN PRIVATE KEY") {
        return None;
    }
    Some(ApnsCredential {
        key_pem,
        key_id,
        team_id,
    })
}

pub fn sign_provider_token(credential: &ApnsCredential, now_ms: i64) -> Result<String, String> {
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(credential.key_id.clone());
    let claims = ProviderClaims {
        iss: credential.team_id.clone(),
        iat: now_ms / 1000,
    };
    let key = EncodingKey::from_ec_pem(credential.key_pem.as_bytes()).map_err(|e| e.to_string())?;
    encode(&header, &claims, &key).map_err(|e| e.to_string())
}

pub fn provider_token(credential: &ApnsCredential, now_ms: i64) -> Result<String, String> {
    let mut cache = PROVIDER_CACHE.lock().expect("provider token cache");
    if let Some((token, at, kid)) = cache.as_ref()
        && kid == &credential.key_id
        && now_ms - at < TOKEN_TTL_MS
    {
        return Ok(token.clone());
    }
    let token = sign_provider_token(credential, now_ms)?;
    *cache = Some((token.clone(), now_ms, credential.key_id.clone()));
    Ok(token)
}

/// Test seam (integration tests in `tests/push.rs`).
pub fn forget_provider_token() {
    *PROVIDER_CACHE.lock().expect("provider token cache") = None;
}

pub fn build_payload(badge: i64, alert: Option<&PushAlert>) -> String {
    let badge = badge.max(0);
    let mut aps = serde_json::json!({ "badge": badge });
    if let Some(alert) = alert {
        aps["alert"] = serde_json::json!({ "title": alert.title, "body": alert.body });
        aps["sound"] = serde_json::json!("default");
    }
    serde_json::json!({ "aps": aps }).to_string()
}

pub fn is_dead_device(status: u16, reason: &str) -> bool {
    status == 410
        || (status == 400 && (reason == "BadDeviceToken" || reason == "DeviceTokenNotForTopic"))
}

#[derive(Debug, Clone)]
pub struct PushResult {
    pub token: String,
    pub status: u16,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SendAllResult {
    pub sent: usize,
    pub pruned: usize,
    pub results: Vec<PushResult>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TransportRequest {
    pub host: String,
    pub token: String,
    pub payload: String,
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct TransportReply {
    pub status: u16,
    pub body: String,
}

#[async_trait]
pub trait PushTransport: Send + Sync {
    async fn send(&self, req: TransportRequest) -> TransportReply;
}

pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new() -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder().build()?;
        Ok(Self { client })
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new().unwrap_or_else(|_| Self {
            client: reqwest::Client::new(),
        })
    }
}

#[async_trait]
impl PushTransport for ReqwestTransport {
    async fn send(&self, req: TransportRequest) -> TransportReply {
        let url = format!("{}/3/device/{}", req.host.trim_end_matches('/'), req.token);
        let mut builder = self.client.post(url).body(req.payload);
        for (k, v) in req.headers {
            builder = builder.header(k, v);
        }
        match builder.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let body = resp.text().await.unwrap_or_default();
                TransportReply { status, body }
            }
            Err(err) => TransportReply {
                status: 0,
                body: err.to_string(),
            },
        }
    }
}

fn read_reason(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("reason").and_then(|r| r.as_str()).map(str::to_string))
        .unwrap_or_default()
}

pub async fn send_to_all(
    db: Arc<Mutex<Db>>,
    badge: i64,
    alert: Option<&PushAlert>,
    transport: &dyn PushTransport,
    now_ms: Option<i64>,
) -> SendAllResult {
    let Some(credential) = get_credential() else {
        return SendAllResult {
            error: Some("APNs is not configured.".to_string()),
            ..Default::default()
        };
    };

    let devices = {
        let guard = db.lock().expect("db mutex");
        match store_push::list_devices(&guard) {
            Ok(d) => d,
            Err(err) => {
                return SendAllResult {
                    error: Some(err.to_string()),
                    ..Default::default()
                };
            }
        }
    };
    if devices.is_empty() {
        return SendAllResult::default();
    }

    let now_ms = now_ms.unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
    let bearer = match provider_token(&credential, now_ms) {
        Ok(t) => t,
        Err(err) => {
            return SendAllResult {
                error: Some(err),
                ..Default::default()
            };
        }
    };
    let payload = build_payload(badge, alert);
    let exp = (now_ms / 1000) + 3600;

    let mut sent = 0usize;
    let mut pruned = 0usize;
    let mut results = Vec::new();

    for device in devices {
        let host = match device.environment {
            store_push::PushEnvironment::Sandbox => HOST_SANDBOX,
            store_push::PushEnvironment::Production => HOST_PRODUCTION,
        };
        let headers = vec![
            ("authorization".to_string(), format!("bearer {bearer}")),
            ("apns-topic".to_string(), TOPIC.to_string()),
            ("apns-push-type".to_string(), "alert".to_string()),
            ("apns-expiration".to_string(), exp.to_string()),
            (
                "apns-priority".to_string(),
                if alert.is_some() { "10" } else { "5" }.to_string(),
            ),
            ("apns-collapse-id".to_string(), "badge".to_string()),
            ("apns-id".to_string(), uuid::Uuid::new_v4().to_string()),
        ];
        let reply = transport
            .send(TransportRequest {
                host: host.to_string(),
                token: device.token.clone(),
                payload: payload.clone(),
                headers,
            })
            .await;
        if reply.status == 200 {
            sent += 1;
        }
        let reason = read_reason(&reply.body);
        let reason_opt = if reason.is_empty() {
            None
        } else {
            Some(reason.clone())
        };
        results.push(PushResult {
            token: device.token.clone(),
            status: reply.status,
            reason: reason_opt,
        });
        if is_dead_device(reply.status, &reason) {
            store_push::forget_device(&db.lock().expect("db mutex"), &device.token);
            pruned += 1;
        }
    }

    SendAllResult {
        sent,
        pruned,
        results,
        error: None,
    }
}

pub fn describe_push(db: &Db) -> serde_json::Value {
    let configured = get_credential().is_some();
    let devices = store_push::list_devices(db).map(|d| d.len()).unwrap_or(0);
    let detail = if !configured {
        format!("No APNs key. Set {KEY_FILE_VAR}, {KEY_ID_VAR} and {TEAM_ID_VAR}.")
    } else if devices == 0 {
        "Ready. No phone has registered yet.".to_string()
    } else {
        format!("{devices} phone(s) registered.")
    };
    serde_json::json!({
        "configured": configured,
        "devices": devices,
        "detail": detail,
    })
}

/// Fire-and-forget badge refresh for the run manager / seen routes.
pub fn spawn_badge_update(db: Arc<Mutex<Db>>, alert: Option<PushAlert>) {
    tokio::spawn(async move {
        let needs_send = get_credential().is_some() || {
            let db = db.lock().expect("db mutex");
            store_push::list_devices(&db)
                .map(|d| !d.is_empty())
                .unwrap_or(false)
        };
        if !needs_send {
            return;
        }
        let badge = {
            let db = db.lock().expect("db mutex");
            store::attention::attention_count(&db, None)
                .map(|a| a.total)
                .unwrap_or(0)
        };
        let transport = ReqwestTransport::default();
        if let SendAllResult {
            error: Some(err), ..
        } = send_to_all(Arc::clone(&db), badge, alert.as_ref(), &transport, None).await
        {
            tracing::debug!("push badge update: {err}");
        }
    });
}
