//! Outbound fetch for link previews and the thumbnail proxy — port of TS
//! `web.ts` (`fetchForBot`, blocklist, internal targets).

use std::time::Duration;

use async_trait::async_trait;
use reqwest::redirect::Policy;
use store::Db;
use url::Url;

use crate::egress::{self, Resolver};

const BLOCKLIST_KEY: &str = "web.blocklist";
const INTERNAL_KEY: &str = "web.internal";

const ALWAYS_BLOCKED: &[&str] = &[
    "metadata.google.internal",
    "instance-data",
    "meridian",
    "meridian.local",
    "meridian.tail74afb5.ts.net",
    "localhost",
];

pub fn get_blocklist(db: &Db) -> Vec<String> {
    let Some(raw) = db.settings_get(BLOCKLIST_KEY).ok().flatten() else {
        return Vec::new();
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Vec::new();
    };
    parsed
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

pub fn get_internal_targets(db: &Db) -> Vec<String> {
    let Some(raw) = db.settings_get(INTERNAL_KEY).ok().flatten() else {
        return Vec::new();
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Vec::new();
    };
    parsed
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

pub fn host_blocked(host: &str, blocklist: &[String]) -> bool {
    let h = host.to_lowercase().trim_end_matches('.').to_string();
    let blocked = |entry: &str| h == entry || h.ends_with(&format!(".{entry}"));
    if ALWAYS_BLOCKED.iter().any(|entry| blocked(entry)) {
        return true;
    }
    blocklist.iter().any(|entry| blocked(entry))
}

#[derive(Debug, Clone)]
pub struct WebFetchPolicy {
    pub blocklist: Vec<String>,
    pub internal_targets: Vec<String>,
}

impl WebFetchPolicy {
    pub fn from_db(db: &Db) -> Self {
        Self {
            blocklist: get_blocklist(db),
            internal_targets: get_internal_targets(db),
        }
    }
}

pub fn internally_allowed(url: &Url, targets: &[String]) -> bool {
    let port = url
        .port_or_known_default()
        .unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
    let Some(host) = url.host_str() else {
        return false;
    };
    let here = format!("{}:{port}", host.to_lowercase());
    targets.iter().any(|t| t.eq_ignore_ascii_case(&here))
}

#[derive(Debug, Clone)]
pub struct FetchOutcome {
    pub ok: bool,
    pub status: Option<u16>,
    pub content_type: Option<String>,
    pub text: Option<String>,
    pub bytes: Option<Vec<u8>>,
    pub error: Option<String>,
}

#[async_trait]
pub trait WebFetch: Send + Sync {
    async fn get(&self, url: &str, timeout_ms: u64) -> Result<FetchedHttp, String>;

    async fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &str,
        timeout_ms: u64,
    ) -> Result<FetchedHttp, String> {
        let _ = (url, headers, body, timeout_ms);
        Err("post_json not implemented".to_string())
    }
}

#[derive(Debug, Clone)]
pub struct FetchedHttp {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

pub struct ReqwestWebFetch {
    client: reqwest::Client,
}

impl ReqwestWebFetch {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(30))
            .user_agent("Bullpen/0.1 (+https://rainmade.io)")
            .build()
            .expect("reqwest client for web fetch");
        Self { client }
    }
}

impl Default for ReqwestWebFetch {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WebFetch for ReqwestWebFetch {
    async fn get(&self, url: &str, timeout_ms: u64) -> Result<FetchedHttp, String> {
        let response = self
            .client
            .get(url)
            .timeout(Duration::from_millis(timeout_ms))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        read_response(response).await
    }

    async fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &str,
        timeout_ms: u64,
    ) -> Result<FetchedHttp, String> {
        let mut req = self
            .client
            .post(url)
            .timeout(Duration::from_millis(timeout_ms))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        let response = req.send().await.map_err(|e| e.to_string())?;
        read_response(response).await
    }
}

async fn read_response(response: reqwest::Response) -> Result<FetchedHttp, String> {
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = response.bytes().await.map_err(|e| e.to_string())?.to_vec();
    Ok(FetchedHttp {
        status,
        content_type,
        body,
    })
}

pub async fn fetch_for_bot(
    policy: &WebFetchPolicy,
    raw_url: &str,
    resolver: &dyn Resolver,
    fetch: &dyn WebFetch,
    options: FetchOptions<'_>,
) -> FetchOutcome {
    let max_bytes = options.max_bytes.unwrap_or(200_000);
    let timeout_ms = options.timeout_ms.unwrap_or(15_000);
    let binary = options.binary;

    let url = match Url::parse(raw_url) {
        Ok(u) => u,
        Err(_) => {
            return FetchOutcome {
                ok: false,
                status: None,
                content_type: None,
                text: None,
                bytes: None,
                error: Some(format!("Not a URL: {raw_url}")),
            };
        }
    };

    if url.scheme() != "http" && url.scheme() != "https" {
        return FetchOutcome {
            ok: false,
            status: None,
            content_type: None,
            text: None,
            bytes: None,
            error: Some(format!(
                "Only http and https are allowed, not {}:",
                url.scheme()
            )),
        };
    }

    if let Some(host) = url.host_str()
        && host_blocked(host, &policy.blocklist)
    {
        return FetchOutcome {
            ok: false,
            status: None,
            content_type: None,
            text: None,
            bytes: None,
            error: Some(format!("{host} is blocked.")),
        };
    }

    let internal = internally_allowed(&url, &policy.internal_targets);

    if !internal {
        if let Some(host) = url.host_str()
            && egress::is_private_address(host)
        {
            return FetchOutcome {
                ok: false,
                status: None,
                content_type: None,
                text: None,
                bytes: None,
                error: Some(format!("{host} is inside this network. Refused.")),
            };
        }

        if let Some(host) = url.host_str() {
            match resolver.resolve(host).await {
                Ok(addresses) => {
                    if let Some(priv_addr) =
                        addresses.iter().find(|a| egress::is_private_address(a))
                    {
                        return FetchOutcome {
                            ok: false,
                            status: None,
                            content_type: None,
                            text: None,
                            bytes: None,
                            error: Some(format!(
                                "{host} resolves to {priv_addr}, which is inside this network. Refused."
                            )),
                        };
                    }
                }
                Err(e) => {
                    return FetchOutcome {
                        ok: false,
                        status: None,
                        content_type: None,
                        text: None,
                        bytes: None,
                        error: Some(format!("Could not resolve {host}: {e}")),
                    };
                }
            }
        }
    }

    match fetch.get(url.as_str(), timeout_ms).await {
        Ok(res) => {
            let status = res.status;
            let ok = (200..300).contains(&status);
            let content_type = Some(res.content_type);
            let truncated = res.body.into_iter().take(max_bytes).collect::<Vec<_>>();
            if binary {
                FetchOutcome {
                    ok,
                    status: Some(status),
                    content_type,
                    text: None,
                    bytes: Some(truncated),
                    error: None,
                }
            } else {
                let text = String::from_utf8_lossy(&truncated).into_owned();
                FetchOutcome {
                    ok,
                    status: Some(status),
                    content_type,
                    text: Some(text),
                    bytes: None,
                    error: None,
                }
            }
        }
        Err(e) => FetchOutcome {
            ok: false,
            status: None,
            content_type: None,
            text: None,
            bytes: None,
            error: Some(e),
        },
    }
}

pub struct FetchOptions<'a> {
    pub max_bytes: Option<usize>,
    pub timeout_ms: Option<u64>,
    pub binary: bool,
    pub _marker: std::marker::PhantomData<&'a ()>,
}

impl Default for FetchOptions<'_> {
    fn default() -> Self {
        Self {
            max_bytes: None,
            timeout_ms: None,
            binary: false,
            _marker: std::marker::PhantomData,
        }
    }
}
