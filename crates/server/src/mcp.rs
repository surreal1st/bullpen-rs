//! S7-02: MCP Streamable HTTP client for hosted connectors — port of
//! `projects/bullpen-night/src/server/mcp.ts` (read-only reference).
//!
//! stdio MCP is intentionally out of scope (same rationale as TS).

use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use store::ConnectorFull;
use uuid::Uuid;

use crate::egress::{self, Resolver};

pub const PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

pub struct RpcResult {
    pub ok: bool,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub needs_auth: bool,
    pub challenge: Option<String>,
}

pub struct ListToolsOutcome {
    pub ok: bool,
    pub tools: Vec<ConnectorTool>,
    pub error: Option<String>,
    pub needs_auth: bool,
    pub challenge: Option<String>,
}

/// HTTP seam for MCP POSTs — production uses reqwest; tests inject a fake.
#[async_trait]
pub trait McpTransport: Send + Sync {
    async fn post(
        &self,
        url: &str,
        headers: HeaderMap,
        body: String,
    ) -> Result<McpHttpResponse, String>;
}

#[derive(Debug, Clone)]
pub struct McpHttpResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: String,
}

pub struct ReqwestMcpTransport {
    client: reqwest::Client,
}

impl ReqwestMcpTransport {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client for MCP");
        Self { client }
    }
}

impl Default for ReqwestMcpTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl McpTransport for ReqwestMcpTransport {
    async fn post(
        &self,
        url: &str,
        headers: HeaderMap,
        body: String,
    ) -> Result<McpHttpResponse, String> {
        let response = self
            .client
            .post(url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = response.text().await.map_err(|e| e.to_string())?;
        Ok(McpHttpResponse {
            status,
            headers,
            body,
        })
    }
}

#[derive(Copy, Clone)]
pub struct McpCallOptions<'a> {
    pub transport: &'a dyn McpTransport,
    pub resolver: &'a dyn Resolver,
    pub bearer: Option<&'a str>,
}

pub async fn list_connector_tools(
    connector: &ConnectorFull,
    options: McpCallOptions<'_>,
) -> ListToolsOutcome {
    let init = rpc(
        connector,
        "initialize",
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "Bullpen", "version": "0.1.0" },
        }),
        options,
    )
    .await;
    if !init.ok {
        return ListToolsOutcome {
            ok: false,
            tools: vec![],
            error: init.error,
            needs_auth: init.needs_auth,
            challenge: init.challenge,
        };
    }

    let listed = rpc(connector, "tools/list", json!({}), options).await;
    if !listed.ok {
        return ListToolsOutcome {
            ok: false,
            tools: vec![],
            error: listed.error,
            needs_auth: listed.needs_auth,
            challenge: listed.challenge,
        };
    }

    let raw = listed
        .result
        .as_ref()
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();

    let tools = raw
        .into_iter()
        .filter_map(|t| {
            let name = t.get("name")?.as_str()?.trim();
            if name.is_empty() {
                return None;
            }
            Some(ConnectorTool {
                name: name.to_string(),
                description: t
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string(),
                input_schema: t
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
            })
        })
        .collect();

    ListToolsOutcome {
        ok: true,
        tools,
        error: None,
        needs_auth: false,
        challenge: None,
    }
}

async fn rpc(
    connector: &ConnectorFull,
    method: &str,
    params: Value,
    options: McpCallOptions<'_>,
) -> RpcResult {
    let host = match host_from_url(&connector.url) {
        Ok(h) => h,
        Err(msg) => {
            return RpcResult {
                ok: false,
                result: None,
                error: Some(msg),
                needs_auth: false,
                challenge: None,
            };
        }
    };

    if let Err(msg) = egress::refuse_if_resolves_private(&host, options.resolver).await {
        return RpcResult {
            ok: false,
            result: None,
            error: Some(msg),
            needs_auth: false,
            challenge: None,
        };
    }

    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    headers.insert(
        "MCP-Protocol-Version",
        HeaderValue::from_static(PROTOCOL_VERSION),
    );
    if let Some(header) = auth_header_for(connector, options.bearer) {
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&header).unwrap_or_else(|_| HeaderValue::from_static("Bearer")),
        );
    }

    let body = json!({
        "jsonrpc": "2.0",
        "id": Uuid::new_v4().to_string(),
        "method": method,
        "params": params,
    })
    .to_string();

    let response = match options.transport.post(&connector.url, headers, body).await {
        Ok(r) => r,
        Err(e) => {
            return RpcResult {
                ok: false,
                result: None,
                error: Some(scrub(connector, &e)),
                needs_auth: false,
                challenge: None,
            };
        }
    };

    if response.status == 401 {
        let challenge = response
            .headers
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        return RpcResult {
            ok: false,
            result: None,
            error: Some("this connector needs authorizing".to_string()),
            needs_auth: true,
            challenge: Some(challenge),
        };
    }

    if !(200..300).contains(&response.status) {
        let snippet = response.body.chars().take(300).collect::<String>();
        return RpcResult {
            ok: false,
            result: None,
            error: Some(scrub(connector, &format!("{}: {snippet}", response.status))),
            needs_auth: false,
            challenge: None,
        };
    }

    let payload = parse_mcp_payload(&response.body);
    let parsed: Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(e) => {
            return RpcResult {
                ok: false,
                result: None,
                error: Some(scrub(connector, &format!("invalid json: {e}"))),
                needs_auth: false,
                challenge: None,
            };
        }
    };

    if let Some(err) = parsed.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("rpc error");
        return RpcResult {
            ok: false,
            result: None,
            error: Some(scrub(connector, msg)),
            needs_auth: false,
            challenge: None,
        };
    }

    RpcResult {
        ok: true,
        result: parsed.get("result").cloned(),
        error: None,
        needs_auth: false,
        challenge: None,
    }
}

fn parse_mcp_payload(text: &str) -> String {
    text.lines()
        .find(|l| l.starts_with("data:"))
        .map(|l| l[5..].trim().to_string())
        .unwrap_or_else(|| text.to_string())
}

fn host_from_url(url: &str) -> Result<String, String> {
    let trimmed = url.trim();
    let rest = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .ok_or_else(|| "that connector's URL is not valid".to_string())?;
    let host = rest
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .trim();
    if host.is_empty() {
        Err("that connector's URL is not valid".to_string())
    } else {
        Ok(host.to_string())
    }
}

fn auth_header_for(connector: &ConnectorFull, bearer: Option<&str>) -> Option<String> {
    if let Some(token) = bearer.filter(|t| !t.is_empty()) {
        return Some(format!("Bearer {token}"));
    }
    connector
        .auth_header
        .as_ref()
        .filter(|h| !h.is_empty())
        .cloned()
}

fn scrub(connector: &ConnectorFull, text: &str) -> String {
    let mut out = text.to_string();
    if let Some(ref auth) = connector.auth_header {
        out = out.split(auth).collect::<Vec<_>>().join("[redacted]");
    }
    model::secrets::redact(&out, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::Resolver;
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct ScriptTransport {
        scripts: Mutex<VecDeque<McpHttpResponse>>,
    }

    #[async_trait]
    impl McpTransport for ScriptTransport {
        async fn post(
            &self,
            _url: &str,
            _headers: HeaderMap,
            _body: String,
        ) -> Result<McpHttpResponse, String> {
            let mut q = self.scripts.lock().unwrap();
            Ok(q.pop_front().unwrap_or(McpHttpResponse {
                status: 500,
                headers: HeaderMap::new(),
                body: "no script".to_string(),
            }))
        }
    }

    struct FakeResolver {
        addrs: Vec<String>,
    }

    #[async_trait]
    impl Resolver for FakeResolver {
        async fn resolve(&self, _host: &str) -> Result<Vec<String>, String> {
            Ok(self.addrs.clone())
        }
    }

    fn connector(url: &str) -> ConnectorFull {
        ConnectorFull {
            id: "c1".to_string(),
            name: "Fake".to_string(),
            url: url.to_string(),
            auth_header: None,
            created_at: "2020-01-01T00:00:00Z".to_string(),
        }
    }

    fn tool_list_body() -> String {
        json!({
            "jsonrpc": "2.0",
            "id": "1",
            "result": {
                "tools": [{
                    "name": "search",
                    "description": "Searches.",
                    "inputSchema": { "type": "object", "properties": { "q": { "type": "string" } } }
                }]
            }
        })
        .to_string()
    }

    #[tokio::test]
    async fn list_tools_parses_json_and_sse() {
        for body in [
            tool_list_body(),
            format!("event: message\ndata: {}\n\n", tool_list_body()),
        ] {
            let transport = ScriptTransport {
                scripts: Mutex::new(VecDeque::from([
                    McpHttpResponse {
                        status: 200,
                        headers: HeaderMap::new(),
                        body: json!({ "jsonrpc": "2.0", "id": "1", "result": {} }).to_string(),
                    },
                    McpHttpResponse {
                        status: 200,
                        headers: HeaderMap::new(),
                        body,
                    },
                ])),
            };
            let resolver = FakeResolver {
                addrs: vec!["93.184.216.34".to_string()],
            };
            let outcome = list_connector_tools(
                &connector("https://fake-mcp.test/mcp"),
                McpCallOptions {
                    transport: &transport,
                    resolver: &resolver,
                    bearer: None,
                },
            )
            .await;
            assert!(outcome.ok, "{:?}", outcome.error);
            assert_eq!(outcome.tools.len(), 1);
            assert_eq!(outcome.tools[0].name, "search");
        }
    }

    #[tokio::test]
    async fn private_resolution_is_refused() {
        let transport = ScriptTransport {
            scripts: Mutex::new(VecDeque::new()),
        };
        let resolver = FakeResolver {
            addrs: vec!["127.0.0.1".to_string()],
        };
        let outcome = list_connector_tools(
            &connector("https://fake-mcp.test/mcp"),
            McpCallOptions {
                transport: &transport,
                resolver: &resolver,
                bearer: None,
            },
        )
        .await;
        assert!(!outcome.ok);
        assert!(
            outcome
                .error
                .unwrap_or_default()
                .contains("inside this network")
        );
    }
}
