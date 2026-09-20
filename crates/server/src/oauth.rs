//! S7-03: OAuth 2.1 for MCP connectors — port of `oauth.ts`.

use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use store::{Db, OAuthTokens};
use uuid::Uuid;

use crate::egress::Resolver;

#[derive(Clone, Debug)]
pub struct AsMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub scopes_supported: Option<Vec<String>>,
}

#[derive(Debug)]
pub struct Discovery {
    pub ok: bool,
    pub resource: Option<String>,
    pub metadata: Option<AsMetadata>,
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct FlowStart {
    pub url: String,
    pub state: String,
    pub verifier: String,
}

#[async_trait]
pub trait OAuthHttp: Send + Sync {
    async fn get_json(&self, url: &str) -> Result<(u16, Value), String>;
    async fn post_json(&self, url: &str, body: Value) -> Result<(u16, Value), String>;
    async fn post_form(&self, url: &str, form: &[(&str, String)]) -> Result<(u16, Value), String>;
}

pub struct ReqwestOAuthHttp {
    client: reqwest::Client,
}

impl ReqwestOAuthHttp {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .expect("reqwest oauth client"),
        }
    }
}

impl Default for ReqwestOAuthHttp {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl OAuthHttp for ReqwestOAuthHttp {
    async fn get_json(&self, url: &str) -> Result<(u16, Value), String> {
        let res = self
            .client
            .get(url)
            .header(ACCEPT, "application/json")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = res.status().as_u16();
        let body: Value = res.json().await.map_err(|e| e.to_string())?;
        Ok((status, body))
    }

    async fn post_json(&self, url: &str, body: Value) -> Result<(u16, Value), String> {
        let res = self
            .client
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = res.status().as_u16();
        let payload: Value = res.json().await.map_err(|e| e.to_string())?;
        Ok((status, payload))
    }

    async fn post_form(&self, url: &str, form: &[(&str, String)]) -> Result<(u16, Value), String> {
        let res = self
            .client
            .post(url)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(ACCEPT, "application/json")
            .form(form)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = res.status().as_u16();
        let payload: Value = res.json().await.map_err(|e| e.to_string())?;
        Ok((status, payload))
    }
}

pub fn public_url() -> String {
    std::env::var("BULLPEN_PUBLIC_URL")
        .or_else(|_| std::env::var("PUBLIC_URL"))
        .unwrap_or_else(|_| "https://meridian.tail74afb5.ts.net:8452".to_string())
        .trim_end_matches('/')
        .to_string()
}

pub fn redirect_uri() -> String {
    format!("{}/api/oauth/callback", public_url())
}

pub fn canonical_resource(url: &str) -> String {
    let mut parsed = reqwest::Url::parse(url.trim())
        .unwrap_or_else(|_| reqwest::Url::parse("https://invalid.local/").expect("fallback url"));
    parsed.set_fragment(None);
    parsed.set_query(None);
    let scheme = parsed.scheme().to_ascii_lowercase();
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    let path = parsed.path();
    let mut rebuilt =
        reqwest::Url::parse(&format!("{scheme}://{host}{path}")).unwrap_or(parsed.clone());
    if path == "/" {
        rebuilt.set_path("");
    }
    let text = rebuilt.to_string();
    if text.ends_with('/') && rebuilt.path().is_empty() {
        text.trim_end_matches('/').to_string()
    } else {
        text
    }
}

pub fn resource_metadata_url(header: Option<&str>) -> Option<String> {
    let header = header?;
    let re = regex::Regex::new(r#"(?i)resource_metadata\s*=\s*("([^"]*)"|([^\s,]+))"#).ok()?;
    let caps = re.captures(header)?;
    Some(caps.get(2).or_else(|| caps.get(3))?.as_str().to_string())
}

async fn get_json_checked(
    url: &str,
    http: &dyn OAuthHttp,
    resolver: &dyn Resolver,
) -> Result<Value, String> {
    let host = host_from_url(url)?;
    crate::egress::refuse_if_resolves_private(&host, resolver).await?;
    let (status, body) = http.get_json(url).await?;
    if !(200..300).contains(&status) {
        return Err(model::secrets::redact(
            &format!("{status} from {url}"),
            None,
        ));
    }
    Ok(body)
}

fn host_from_url(url: &str) -> Result<String, String> {
    let trimmed = url.trim();
    let rest = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .ok_or_else(|| format!("that is not a URL: {url}"))?;
    let host = rest
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .trim();
    if host.is_empty() {
        Err(format!("that is not a URL: {url}"))
    } else {
        Ok(host.to_string())
    }
}

fn str_field(body: &Value, key: &str) -> Option<String> {
    body.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

pub async fn discover(
    connector_url: &str,
    www_authenticate: Option<&str>,
    http: &dyn OAuthHttp,
    resolver: &dyn Resolver,
) -> Discovery {
    let resource = canonical_resource(connector_url);
    let advertised = resource_metadata_url(www_authenticate);
    let fallback = format!(
        "{}/.well-known/oauth-protected-resource",
        connector_url.trim_end_matches('/')
    );
    let metadata_url = advertised.as_deref().unwrap_or(&fallback);

    let rs = match get_json_checked(metadata_url, http, resolver).await {
        Ok(v) => v,
        Err(e) => {
            return Discovery {
                ok: false,
                resource: None,
                metadata: None,
                error: Some(if e.contains("published") {
                    e
                } else {
                    "that connector published no resource metadata".to_string()
                }),
            };
        }
    };

    let servers: Vec<String> = rs
        .get("authorization_servers")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let Some(issuer) = servers.first().cloned() else {
        return Discovery {
            ok: false,
            resource: Some(resource),
            metadata: None,
            error: Some("that connector names no authorization server".to_string()),
        };
    };

    let base = issuer.trim_end_matches('/');
    let path = issuer
        .strip_prefix("https://")
        .or_else(|| issuer.strip_prefix("http://"))
        .and_then(|r| r.split_once('/'))
        .map(|(_, p)| p.trim_start_matches('/'))
        .filter(|p| !p.is_empty())
        .map(|p| format!("/{p}"))
        .unwrap_or_default();

    let candidates = [
        format!("{base}/.well-known/oauth-authorization-server{path}"),
        format!("{base}/.well-known/openid-configuration{path}"),
    ];

    for candidate in candidates {
        let Ok(body) = get_json_checked(&candidate, http, resolver).await else {
            continue;
        };
        let Some(authorization_endpoint) = str_field(&body, "authorization_endpoint") else {
            continue;
        };
        let Some(token_endpoint) = str_field(&body, "token_endpoint") else {
            continue;
        };
        let registration_endpoint = str_field(&body, "registration_endpoint");
        let scopes_supported = body
            .get("scopes_supported")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            });

        return Discovery {
            ok: true,
            resource: Some(resource),
            metadata: Some(AsMetadata {
                issuer: str_field(&body, "issuer").unwrap_or(issuer.clone()),
                authorization_endpoint,
                token_endpoint,
                registration_endpoint,
                scopes_supported,
            }),
            error: None,
        };
    }

    Discovery {
        ok: false,
        resource: Some(resource),
        metadata: None,
        error: Some(format!(
            "{issuer} published no usable authorization server metadata"
        )),
    }
}

pub async fn register_client(
    registration_endpoint: &str,
    redirect_uri: &str,
    http: &dyn OAuthHttp,
) -> Result<(String, Option<String>), String> {
    let (status, body) = http
        .post_json(
            registration_endpoint,
            json!({
                "client_name": "Bullpen by Rainmade",
                "redirect_uris": [redirect_uri],
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none",
            }),
        )
        .await?;
    if !(200..300).contains(&status) {
        return Err(model::secrets::redact(
            &format!("registration returned {status}"),
            None,
        ));
    }
    let client_id = str_field(&body, "client_id")
        .ok_or_else(|| "registration returned no client_id".to_string())?;
    Ok((client_id, str_field(&body, "client_secret")))
}

pub fn pkce() -> (String, String) {
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(rand_bytes(48));
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    (verifier, challenge)
}

fn rand_bytes(n: usize) -> Vec<u8> {
    use rand::RngCore;
    let mut buf = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut buf);
    buf
}

pub fn authorization_url(
    metadata: &AsMetadata,
    client_id: &str,
    redirect_uri: &str,
    resource: &str,
    scope: Option<&str>,
) -> FlowStart {
    let (verifier, challenge) = pkce();
    let state = Uuid::new_v4().to_string();
    let mut url = reqwest::Url::parse(&metadata.authorization_endpoint).expect("authorize url");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("resource", resource);
    if let Some(scope) = scope.filter(|s| !s.is_empty()) {
        url.query_pairs_mut().append_pair("scope", scope);
    }
    FlowStart {
        url: url.to_string(),
        state,
        verifier,
    }
}

pub fn token_expired(expires_at: Option<&str>, now_ms: i64) -> bool {
    let Some(expires_at) = expires_at else {
        return false;
    };
    let Ok(at) = chrono::DateTime::parse_from_rfc3339(expires_at) else {
        return false;
    };
    at.timestamp_millis() - 60_000 <= now_ms
}

async fn token_request(
    token_endpoint: &str,
    mut form: Vec<(String, String)>,
    client_secret: Option<&str>,
    http: &dyn OAuthHttp,
) -> Result<OAuthTokens, String> {
    if let Some(secret) = client_secret.filter(|s| !s.is_empty()) {
        form.push(("client_secret".to_string(), secret.to_string()));
    }
    let refs: Vec<(&str, String)> = form.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
    let (status, payload) = http.post_form(token_endpoint, &refs).await?;
    if !(200..300).contains(&status) {
        let described = str_field(&payload, "error_description")
            .or_else(|| str_field(&payload, "error"))
            .unwrap_or_else(|| status.to_string());
        return Err(scrub_form(
            &model::secrets::redact(&described, None),
            &form,
            client_secret,
        ));
    }
    let access_token = str_field(&payload, "access_token")
        .ok_or_else(|| "no access_token came back".to_string())?;
    let expires_in = payload.get("expires_in").and_then(|v| v.as_f64());
    let expires_at = expires_in.map(|secs| {
        (chrono::Utc::now() + chrono::Duration::milliseconds((secs * 1000.0) as i64)).to_rfc3339()
    });
    Ok(OAuthTokens {
        access_token,
        refresh_token: str_field(&payload, "refresh_token"),
        expires_at,
        scope: str_field(&payload, "scope"),
    })
}

fn scrub_form(text: &str, form: &[(String, String)], client_secret: Option<&str>) -> String {
    let mut out = text.to_string();
    for (key, value) in form {
        if ["code", "code_verifier", "refresh_token"].contains(&key.as_str()) && !value.is_empty() {
            out = out.split(value).collect::<Vec<_>>().join("[redacted]");
        }
    }
    if let Some(secret) = client_secret.filter(|s| !s.is_empty()) {
        out = out.split(secret).collect::<Vec<_>>().join("[redacted]");
    }
    out
}

pub async fn exchange_code(
    auth: &store::ConnectorAuth,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    http: &dyn OAuthHttp,
) -> Result<OAuthTokens, String> {
    token_request(
        &auth.token_url,
        vec![
            ("grant_type".to_string(), "authorization_code".to_string()),
            ("code".to_string(), code.to_string()),
            ("redirect_uri".to_string(), redirect_uri.to_string()),
            ("client_id".to_string(), auth.client_id.clone()),
            ("code_verifier".to_string(), verifier.to_string()),
            ("resource".to_string(), auth.resource.clone()),
        ],
        auth.client_secret.as_deref(),
        http,
    )
    .await
}

pub async fn refresh_tokens(
    auth: &store::ConnectorAuth,
    http: &dyn OAuthHttp,
) -> Result<OAuthTokens, String> {
    let refresh_token = auth
        .refresh_token
        .as_deref()
        .ok_or_else(|| "no refresh token".to_string())?;
    token_request(
        &auth.token_url,
        vec![
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("refresh_token".to_string(), refresh_token.to_string()),
            ("client_id".to_string(), auth.client_id.clone()),
            ("resource".to_string(), auth.resource.clone()),
        ],
        auth.client_secret.as_deref(),
        http,
    )
    .await
}

pub async fn bearer_for(db: &Db, connector_id: &str, http: &dyn OAuthHttp) -> Option<String> {
    let auth = store::get_auth(db, connector_id).ok().flatten()?;
    let access = auth.access_token.as_deref()?;
    if !token_expired(
        auth.expires_at.as_deref(),
        chrono::Utc::now().timestamp_millis(),
    ) {
        return Some(access.to_string());
    }
    let renewed = refresh_tokens(&auth, http).await.ok()?;
    store::put_tokens(db, connector_id, &renewed).ok()?;
    Some(renewed.access_token)
}

pub fn oauth_callback_page(title: &str, detail: &str) -> String {
    let detail = html_escape(detail);
    let title = html_escape(title);
    format!(
        "<!doctype html>
<html><head><meta charset=\"utf-8\"><title>{title}</title>
<style>
  body {{ background:#16181d; color:#e6e8ec; font:16px/1.6 system-ui,sans-serif;
         display:grid; place-items:center; height:100vh; margin:0; }}
  main {{ max-width:30rem; padding:2rem; text-align:center; }}
  h1 {{ font-size:1.3rem; margin:0 0 .5rem; letter-spacing:-.02em; }}
  p {{ color:#8b9099; margin:0; }}
</style></head>
<body><main><h1>{title}</h1><p>{detail}</p></main></body></html>"
    )
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
