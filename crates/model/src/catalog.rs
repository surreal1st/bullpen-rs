//! The OpenRouter model catalog and pin-time validation. Port of
//! `projects/bullpen-night/src/server/catalog.ts`.
//!
//! Every rule here exists because it already cost real time on the previous
//! platform. None of them are hypothetical.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex as AsyncMutex;

use crate::secrets::{KeySource, redact};

/// A model listed in the OpenRouter catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogModel {
    pub id: String,
    pub name: String,
    /// Dollars per million input tokens.
    pub in_per_m: f64,
    /// Dollars per million output tokens.
    pub out_per_m: f64,
    pub context_length: u32,
    pub supports_tools: bool,
    /// Whether the model accepts images, not just text.
    pub supports_images: bool,
    /// Whether OpenRouter lists "reasoning" in this model's supported_parameters.
    pub supports_reasoning: bool,
    /// Number of providers serving it. 1 means shared rate limit.
    pub provider_count: Option<u32>,
    /// True when at least one provider caches the prompt prefix.
    pub supports_caching: Option<bool>,
}

/// The result of checking whether a model is safe to pin to.
#[derive(Debug, Clone, PartialEq)]
pub struct PinVerdict {
    pub ok: bool,
    /// Present when ok is false. Why the pin was refused, in plain language.
    pub refusal: Option<String>,
    /// Present when the pin is allowed but carries a risk worth stating.
    pub warning: Option<String>,
}

impl PinVerdict {
    pub fn ok() -> Self {
        Self {
            ok: true,
            refusal: None,
            warning: None,
        }
    }

    pub fn refusal(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            refusal: Some(msg.into()),
            warning: None,
        }
    }

    pub fn warning(msg: impl Into<String>) -> Self {
        Self {
            ok: true,
            refusal: None,
            warning: Some(msg.into()),
        }
    }
}

/// The catalog interface: list all models, get one, or fetch endpoint details.
#[async_trait::async_trait]
pub trait Catalog: Send + Sync {
    /// All available models.
    async fn list(&self) -> Result<Vec<CatalogModel>, String>;

    /// One model by id, or None if not found.
    async fn get(&self, id: &str) -> Result<Option<CatalogModel>, String>;

    /// Endpoint detail: provider count and caching support.
    async fn detail(&self, id: &str) -> Result<Option<(u32, bool)>, String>;
}

/// A catalog that reads from a JSON fixture file. Used in tests.
pub struct FixtureCatalog {
    models: Arc<Vec<CatalogModel>>,
}

impl FixtureCatalog {
    /// Load from a JSON file. Panics on parse error (acceptable in tests).
    pub fn from_file(path: &str) -> Self {
        let content = std::fs::read_to_string(path).expect("read fixture file");
        let models: Vec<CatalogModel> = serde_json::from_str(&content).expect("parse fixture");
        Self {
            models: Arc::new(models),
        }
    }

    /// Load from inline JSON string (for testing).
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        let models: Vec<CatalogModel> = serde_json::from_str(json)?;
        Ok(Self {
            models: Arc::new(models),
        })
    }
}

#[async_trait::async_trait]
impl Catalog for FixtureCatalog {
    async fn list(&self) -> Result<Vec<CatalogModel>, String> {
        Ok(self.models.as_ref().clone())
    }

    async fn get(&self, id: &str) -> Result<Option<CatalogModel>, String> {
        Ok(self.models.iter().find(|m| m.id == id).cloned())
    }

    async fn detail(&self, _id: &str) -> Result<Option<(u32, bool)>, String> {
        // Fixtures don't have endpoint details; real catalog would fetch from OpenRouter.
        Ok(None)
    }
}

const MODELS_URL: &str = "https://openrouter.ai/api/v1/models";
const ENDPOINTS_URL: &str = "https://openrouter.ai/api/v1/models";
/// Same TTL as `catalog.ts`'s `CACHE_MS` (one hour).
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);

#[derive(Clone)]
struct DetailEntry {
    provider_count: u32,
    supports_caching: bool,
}

struct ListCache {
    at: Instant,
    models: Vec<CatalogModel>,
}

/// The live OpenRouter catalogue: fetches `GET /models`, caches the whole
/// list for an hour, and maps OpenRouter's raw shape into [`CatalogModel`].
/// Endpoint detail (provider count, caching support) is fetched and cached
/// per id on the same TTL. Port of `createOpenRouterCatalog` in
/// `projects/bullpen-night/src/server/catalog.ts`.
///
/// The key is resolved fresh through `KeySource` on every fetch (never
/// cached itself), and every error that could carry it goes through
/// [`redact`] before it leaves this type - the same posture `OpenRouterPort`
/// takes in `port.rs`.
pub struct OpenRouterCatalog {
    client: reqwest::Client,
    key_source: KeySource,
    list_cache: AsyncMutex<Option<ListCache>>,
    detail_cache: AsyncMutex<HashMap<String, (Instant, DetailEntry)>>,
}

impl OpenRouterCatalog {
    pub fn new(key_source: KeySource) -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .build()
                .expect("client builder failed"),
            key_source,
            list_cache: AsyncMutex::new(None),
            detail_cache: AsyncMutex::new(HashMap::new()),
        }
    }

    async fn fetch_list(&self) -> Result<Vec<CatalogModel>, String> {
        let key = self
            .key_source
            .resolve()
            .ok_or_else(|| "No OpenRouter key configured.".to_string())?;
        let res = self
            .client
            .get(MODELS_URL)
            .bearer_auth(&key)
            .send()
            .await
            .map_err(|e| redact(&e.to_string(), Some(&key)))?;
        if !res.status().is_success() {
            let status = res.status().as_u16();
            return Err(redact(
                &format!("OpenRouter model list returned {status}"),
                Some(&key),
            ));
        }
        let text = res
            .text()
            .await
            .map_err(|e| redact(&e.to_string(), Some(&key)))?;
        parse_models_response(&text).map_err(|e| redact(&e, Some(&key)))
    }
}

#[async_trait::async_trait]
impl Catalog for OpenRouterCatalog {
    async fn list(&self) -> Result<Vec<CatalogModel>, String> {
        {
            let cache = self.list_cache.lock().await;
            if let Some(c) = cache.as_ref()
                && c.at.elapsed() < CACHE_TTL
            {
                return Ok(c.models.clone());
            }
        }
        let models = self.fetch_list().await?;
        let mut cache = self.list_cache.lock().await;
        *cache = Some(ListCache {
            at: Instant::now(),
            models: models.clone(),
        });
        Ok(models)
    }

    async fn get(&self, id: &str) -> Result<Option<CatalogModel>, String> {
        let all = self.list().await?;
        Ok(all.into_iter().find(|m| m.id == id))
    }

    async fn detail(&self, id: &str) -> Result<Option<(u32, bool)>, String> {
        {
            let cache = self.detail_cache.lock().await;
            if let Some((at, entry)) = cache.get(id)
                && at.elapsed() < CACHE_TTL
            {
                return Ok(Some((entry.provider_count, entry.supports_caching)));
            }
        }
        // Endpoint detail is a nice-to-have (provider redundancy warnings),
        // never the reason a picker goes empty - a missing key or a dead
        // upstream here answers `None`, same as `FixtureCatalog`, rather
        // than failing the whole request.
        let Some(key) = self.key_source.resolve() else {
            return Ok(None);
        };
        let url = format!("{ENDPOINTS_URL}/{id}/endpoints");
        let Ok(res) = self.client.get(&url).bearer_auth(&key).send().await else {
            return Ok(None);
        };
        if !res.status().is_success() {
            return Ok(None);
        }
        let Ok(body) = res.json::<RawEndpointsResponse>().await else {
            return Ok(None);
        };
        let endpoints = body.data.map(|d| d.endpoints).unwrap_or_default();
        let entry = DetailEntry {
            provider_count: endpoints.len() as u32,
            supports_caching: endpoints
                .iter()
                .any(|e| e.supports_implicit_caching == Some(true)),
        };
        let mut cache = self.detail_cache.lock().await;
        cache.insert(id.to_string(), (Instant::now(), entry.clone()));
        Ok(Some((entry.provider_count, entry.supports_caching)))
    }
}

/// Pure over the response body: parses OpenRouter's `GET /models` JSON into
/// our shape with no network involved, so the mapping itself - dollar
/// pricing scaled to per-million, `tools`/`reasoning` read off
/// `supported_parameters`, `image` read off `architecture.input_modalities`
/// - is unit-testable against a captured response.
fn parse_models_response(body: &str) -> Result<Vec<CatalogModel>, String> {
    let parsed: RawModelsResponse = serde_json::from_str(body).map_err(|e| e.to_string())?;
    Ok(parsed.data.into_iter().map(map_raw_model).collect())
}

fn map_raw_model(m: RawModel) -> CatalogModel {
    let in_per_m = m
        .pricing
        .as_ref()
        .and_then(|p| p.prompt.as_deref())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0)
        * 1_000_000.0;
    let out_per_m = m
        .pricing
        .as_ref()
        .and_then(|p| p.completion.as_deref())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0)
        * 1_000_000.0;
    let params = m.supported_parameters.unwrap_or_default();
    let modalities = m
        .architecture
        .and_then(|a| a.input_modalities)
        .unwrap_or_default();
    CatalogModel {
        id: m.id,
        name: m.name,
        in_per_m,
        out_per_m,
        context_length: m.context_length.unwrap_or(0),
        supports_tools: params.iter().any(|p| p == "tools"),
        supports_images: modalities.iter().any(|m| m == "image"),
        supports_reasoning: params.iter().any(|p| p == "reasoning"),
        provider_count: None,
        supports_caching: None,
    }
}

#[derive(Debug, Deserialize, Default)]
struct RawModelsResponse {
    #[serde(default)]
    data: Vec<RawModel>,
}

#[derive(Debug, Deserialize)]
struct RawModel {
    id: String,
    name: String,
    #[serde(default)]
    context_length: Option<u32>,
    #[serde(default)]
    pricing: Option<RawPricing>,
    #[serde(default)]
    supported_parameters: Option<Vec<String>>,
    #[serde(default)]
    architecture: Option<RawArchitecture>,
}

#[derive(Debug, Deserialize, Default)]
struct RawPricing {
    prompt: Option<String>,
    completion: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct RawArchitecture {
    input_modalities: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Default)]
struct RawEndpointsResponse {
    data: Option<RawEndpointsData>,
}

#[derive(Debug, Deserialize, Default)]
struct RawEndpointsData {
    #[serde(default)]
    endpoints: Vec<RawEndpoint>,
}

#[derive(Debug, Deserialize)]
struct RawEndpoint {
    #[serde(default)]
    supports_implicit_caching: Option<bool>,
}

/// Decides whether a bot may be pinned to a model.
///
/// `has_routine` tightens every rule, because an unattended run has nobody
/// watching it fail.
pub async fn judge_pin(catalog: &dyn Catalog, model_id: &str, has_routine: bool) -> PinVerdict {
    // 1. Batch-only slugs. OpenRouter lists 72 of these RIGHT NEXT TO the real
    //    model, with identical fields and identical supported_parameters. Nothing
    //    in the catalog marks them. The suffix is the only signal, which is
    //    exactly why 13 of 15 bots on the previous platform died instantly on
    //    `anthropic/claude-opus-5:batch` and it looked like a routine bug.
    if model_id.ends_with(":batch") {
        return PinVerdict::refusal(
            "This is the batch-only version of the model. Every run fails immediately with a 404, \
             because it can only be reached through the Batch API. Pin the model without the :batch suffix.",
        );
    }

    let model = match catalog.get(model_id).await {
        Ok(Some(m)) => m,
        Ok(None) => {
            return PinVerdict::refusal("OpenRouter does not list this model. Check the id.");
        }
        Err(e) => return PinVerdict::refusal(format!("Catalog error: {e}")),
    };

    // 2. Free tiers are rate limited against a shared daily quota, so they are
    //    fine to try by hand and hopeless on a timer.
    if model_id.ends_with(":free") {
        if has_routine {
            return PinVerdict::refusal(
                "Free models share a small daily quota across all of OpenRouter, so a scheduled run \
                 will fail unpredictably. Pin a paid model on any bot that has a routine.",
            );
        }
        return PinVerdict::warning("Free tier. Rate limited against a shared daily quota.");
    }

    // 3. Provider redundancy. A model served by one provider inherits that
    //    provider's rate limit, which is other people's traffic. This is what
    //    killed both scheduled runs of qwen3.7-flash with
    //    `429 limit_source: upstream_provider_shared_pool`.
    if let Ok(Some((provider_count, _))) = catalog.detail(model_id).await {
        if provider_count <= 1 && has_routine {
            return PinVerdict::refusal(
                "Only one provider serves this model, so its rate limit is other people's traffic. \
                 A scheduled run will die on a 429 at some point and you will not be watching. \
                 Pin a model with more than one provider.",
            );
        } else if provider_count <= 1 {
            return PinVerdict::warning(
                "Only one provider serves this model. Expect rate limits under load.",
            );
        }
    }

    if !model.supports_tools {
        return PinVerdict::warning(
            "This model cannot call tools, so connectors will not work on this bot.",
        );
    }

    PinVerdict::ok()
}

#[cfg(test)]
mod openrouter_catalog_tests {
    use super::*;

    /// Trimmed but real-shaped capture of an OpenRouter `GET /models`
    /// response - the fields `map_raw_model` reads, nothing else. No
    /// network: this drives the pure mapping function directly.
    const CAPTURED_MODELS_RESPONSE: &str = r#"{
        "data": [
            {
                "id": "anthropic/claude-sonnet-5",
                "name": "Claude Sonnet 5",
                "context_length": 200000,
                "pricing": { "prompt": "0.000003", "completion": "0.000015" },
                "supported_parameters": ["tools", "reasoning", "temperature"],
                "architecture": { "input_modalities": ["text", "image"] }
            },
            {
                "id": "anthropic/claude-opus-5:batch",
                "name": "Claude Opus 5 (batch)",
                "context_length": 200000,
                "pricing": { "prompt": "0.0000075", "completion": "0.0000375" },
                "supported_parameters": ["tools"],
                "architecture": { "input_modalities": ["text"] }
            },
            {
                "id": "some/no-pricing-model",
                "name": "No Pricing Model"
            }
        ]
    }"#;

    #[test]
    fn maps_a_captured_openrouter_models_response() {
        let models = parse_models_response(CAPTURED_MODELS_RESPONSE).expect("parse");
        assert_eq!(models.len(), 3);

        let sonnet = &models[0];
        assert_eq!(sonnet.id, "anthropic/claude-sonnet-5");
        assert_eq!(sonnet.name, "Claude Sonnet 5");
        assert!((sonnet.in_per_m - 3.0).abs() < 1e-9, "{}", sonnet.in_per_m);
        assert!(
            (sonnet.out_per_m - 15.0).abs() < 1e-9,
            "{}",
            sonnet.out_per_m
        );
        assert_eq!(sonnet.context_length, 200_000);
        assert!(sonnet.supports_tools);
        assert!(sonnet.supports_images);
        assert!(sonnet.supports_reasoning);
        // The live fetch never sets these - `detail()` fills them in
        // separately, matching TS's `providerCount: null, supportsCaching: null`.
        assert_eq!(sonnet.provider_count, None);
        assert_eq!(sonnet.supports_caching, None);

        let batch = &models[1];
        assert_eq!(batch.id, "anthropic/claude-opus-5:batch");
        assert!(!batch.supports_reasoning);
        assert!(!batch.supports_images);

        // A model with no `pricing`/`supported_parameters`/`architecture` at
        // all must not panic the mapper - it reads as free and tool-less.
        let bare = &models[2];
        assert_eq!(bare.in_per_m, 0.0);
        assert_eq!(bare.out_per_m, 0.0);
        assert_eq!(bare.context_length, 0);
        assert!(!bare.supports_tools);
        assert!(!bare.supports_images);
    }

    #[test]
    fn parse_models_response_rejects_invalid_json() {
        assert!(parse_models_response("not json").is_err());
    }
}
