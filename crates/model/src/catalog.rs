//! The OpenRouter model catalog and pin-time validation. Port of
//! `projects/bullpen-night/src/server/catalog.ts`.
//!
//! Every rule here exists because it already cost real time on the previous
//! platform. None of them are hypothetical.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

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
        if provider_count <= 1 {
            if has_routine {
                return PinVerdict::refusal(
                    "Only one provider serves this model, so its rate limit is other people's traffic. \
                     A scheduled run will die on a 429 at some point and you will not be watching. \
                     Pin a model with more than one provider.",
                );
            }
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
