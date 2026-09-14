//! S2-06: GET/PUT routes for theme, timezone, rules, models, routing, second opinion.
//! Port of `projects/bullpen-night/src/server/app.ts:2902-3080,4374-4400`.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

use super::parse_body;
use crate::AppState;
use crate::prompt::{DEFAULT_RULES, house_rules, set_house_rules};
use model::judge_pin;
use model::ladder::{
    EscalationKind, get_default_model, get_mid_model, get_premium_model, looks_premium,
    set_default_model, set_mid_model, set_premium_model, set_tier1_model, tier1_models,
};
use model::routing::{get_routing_settings, set_routing_settings};

/// A minimal theme struct for storage and validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Theme {
    pub accent: String,
    pub mine: String,
    pub ground: String,
    pub radius: String,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/theme", get(get_theme).put(put_theme))
        .route("/api/timezone", get(get_timezone).put(put_timezone))
        .route("/api/rules", get(get_rules).put(put_rules))
        .route(
            "/api/default-model",
            get(get_default_model_route).put(put_default_model),
        )
        .route(
            "/api/mid-model",
            get(get_mid_model_route).put(put_mid_model),
        )
        .route(
            "/api/tier1-models",
            get(get_tier1_models_route).put(put_tier1_models),
        )
        .route(
            "/api/premium-model",
            get(get_premium_model_route).put(put_premium_model),
        )
        .route("/api/models", get(get_models_route))
        .route(
            "/api/models/mainstream",
            get(get_mainstream_route).put(put_mainstream_route),
        )
        .route("/api/routing", get(get_routing).put(put_routing))
        .route(
            "/api/second-opinion",
            get(get_second_opinion).put(put_second_opinion),
        )
}

/// Theme GET handler
async fn get_theme(State(state): State<AppState>) -> Result<Response, crate::AppError> {
    let db = state.db();
    let theme = db
        .settings_get("theme")
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_else(|| Theme {
            accent: "seam".to_string(),
            mine: "accent".to_string(),
            ground: "vapor".to_string(),
            radius: "soft".to_string(),
        });
    Ok(Json(json!({ "theme": theme })).into_response())
}

#[derive(Deserialize, Default)]
struct ThemeBody {
    theme: Option<serde_json::Value>,
}

/// Theme PUT handler
async fn put_theme(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Response, crate::AppError> {
    let db = state.db();
    let parsed: ThemeBody = parse_body(&body)?;

    // Normalize and validate the theme
    let theme = match parsed.theme {
        Some(v) => {
            if let Ok(t) = serde_json::from_value::<Theme>(v) {
                t
            } else {
                Theme {
                    accent: "seam".to_string(),
                    mine: "accent".to_string(),
                    ground: "vapor".to_string(),
                    radius: "soft".to_string(),
                }
            }
        }
        None => Theme {
            accent: "seam".to_string(),
            mine: "accent".to_string(),
            ground: "vapor".to_string(),
            radius: "soft".to_string(),
        },
    };

    let json = serde_json::to_string(&theme)?;
    db.settings_set("theme", &json)?;

    Ok(Json(json!({ "theme": theme })).into_response())
}

/// Timezone GET handler
async fn get_timezone(State(state): State<AppState>) -> Result<Response, crate::AppError> {
    let db = state.db();
    let tz = db
        .settings_get("timezone")
        .ok()
        .flatten()
        .unwrap_or_else(|| "auto".to_string());
    Ok(Json(json!({ "timezone": tz })).into_response())
}

#[derive(Deserialize, Default)]
struct TimezoneBody {
    timezone: Option<String>,
}

/// Timezone PUT handler
async fn put_timezone(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Response, crate::AppError> {
    let db = state.db();
    let parsed: TimezoneBody = parse_body(&body)?;

    let tz = match parsed.timezone {
        Some(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => "auto".to_string(),
    };

    db.settings_set("timezone", &tz)?;

    Ok(Json(json!({ "timezone": tz })).into_response())
}

/// House rules GET handler
async fn get_rules(State(state): State<AppState>) -> Result<Response, crate::AppError> {
    let db = state.db();
    let rules = house_rules(&db);
    Ok(Json(json!({ "rules": rules, "fallback": DEFAULT_RULES })).into_response())
}

#[derive(Deserialize, Default)]
struct RulesBody {
    rules: Option<String>,
}

/// House rules PUT handler
async fn put_rules(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Response, crate::AppError> {
    let db = state.db();
    let parsed: RulesBody = parse_body(&body)?;

    let rules = parsed.rules.unwrap_or_default();

    let saved = set_house_rules(&db, &rules);
    Ok(Json(json!({ "rules": saved })).into_response())
}

/// Default model GET handler
async fn get_default_model_route(
    State(state): State<AppState>,
) -> Result<Response, crate::AppError> {
    let db = state.db();
    let model = get_default_model(&db);
    Ok(Json(json!({ "model": model })).into_response())
}

#[derive(Deserialize, Default)]
struct ModelBody {
    model: Option<String>,
}

/// Default model PUT handler
async fn put_default_model(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Response, crate::AppError> {
    let parsed: ModelBody = parse_body(&body)?;

    let model = match parsed.model {
        Some(m) if !m.trim().is_empty() => m.trim().to_string(),
        _ => return Err(crate::AppError::bad_request("give a model id")),
    };

    // A premium default is refused outright. Locked scope before the catalog
    // await below - a `MutexGuard` cannot cross it (the handler future has
    // to stay `Send`).
    {
        let db = state.db();
        refuse_if_premium(&db, &model)?;
    }

    // F2/D9: judged as a ROUTINE pin, the strictest rule there is
    // (`app.ts:2988`) - this is the model every unpinned bot and every timer
    // run falls back to, so a `:batch` slug here would 404 unattended.
    let verdict = judge_pin(state.catalog.as_ref(), &model, true).await;
    if !verdict.ok {
        return Err(crate::AppError::bad_request(
            verdict
                .refusal
                .unwrap_or_else(|| "that model cannot be pinned".to_string()),
        ));
    }

    let db = state.db();
    let saved = set_default_model(&db, &model);
    Ok(Json(json!({ "model": saved })).into_response())
}

/// The premium-pin refusal `/api/default-model` uses above, pulled out so
/// `routes/bots.rs`'s per-bot model PATCH (S2-09b) gives byte-identical text
/// rather than a second copy that can drift.
pub(crate) fn refuse_if_premium(db: &store::Db, model: &str) -> Result<(), crate::AppError> {
    if looks_premium(model) || model == get_premium_model(db) {
        return Err(crate::AppError::bad_request(
            "That is a premium model, and this is what every unpinned bot and every routine falls \
             back to. It is also what a timer run downgrades TO, so setting it here would defeat \
             the downgrade entirely. Pick a cheap model; premium belongs on Escalate.",
        ));
    }
    Ok(())
}

/// Mid model GET handler
async fn get_mid_model_route(State(state): State<AppState>) -> Result<Response, crate::AppError> {
    let db = state.db();
    let model = get_mid_model(&db);
    Ok(Json(json!({ "model": model })).into_response())
}

/// Mid model PUT handler
async fn put_mid_model(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Response, crate::AppError> {
    let db = state.db();
    let parsed: ModelBody = parse_body(&body)?;

    let model = match parsed.model {
        Some(m) if !m.trim().is_empty() => m.trim().to_string(),
        _ => return Err(crate::AppError::bad_request("give a model id")),
    };

    let saved = set_mid_model(&db, &model);
    Ok(Json(json!({ "model": saved })).into_response())
}

/// Tier1 models GET handler
async fn get_tier1_models_route(
    State(state): State<AppState>,
) -> Result<Response, crate::AppError> {
    let db = state.db();
    let models = tier1_models(&db);
    Ok(Json(json!({
        "models": models,
        "kinds": ["code", "reason", "vision"]
    }))
    .into_response())
}

#[derive(Deserialize, Default)]
struct Tier1Body {
    kind: Option<String>,
    model: Option<String>,
}

/// Tier1 models PUT handler
async fn put_tier1_models(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Response, crate::AppError> {
    let db = state.db();
    let parsed: Tier1Body = parse_body(&body)?;

    let kind_str = match parsed.kind {
        Some(k) => k,
        None => return Err(crate::AppError::bad_request("kind is required")),
    };

    let kind = match kind_str.as_str() {
        "code" => EscalationKind::Code,
        "reason" => EscalationKind::Reason,
        "vision" => EscalationKind::Vision,
        _ => {
            return Err(crate::AppError::bad_request(
                "kind must be one of: code, reason, vision",
            ));
        }
    };

    let model = match parsed.model {
        Some(m) if !m.trim().is_empty() => m.trim().to_string(),
        _ => return Err(crate::AppError::bad_request("give a model id")),
    };

    let saved = set_tier1_model(&db, kind, &model);
    Ok(Json(json!({ "kind": kind_str, "model": saved })).into_response())
}

/// Premium model GET handler
async fn get_premium_model_route(
    State(state): State<AppState>,
) -> Result<Response, crate::AppError> {
    let db = state.db();
    let model = get_premium_model(&db);
    Ok(Json(json!({ "model": model })).into_response())
}

/// Premium model PUT handler
async fn put_premium_model(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Response, crate::AppError> {
    let db = state.db();
    let parsed: ModelBody = parse_body(&body)?;

    let model = match parsed.model {
        Some(m) if !m.trim().is_empty() => m.trim().to_string(),
        _ => return Err(crate::AppError::bad_request("give a model id")),
    };

    let saved = set_premium_model(&db, &model);
    Ok(Json(json!({ "model": saved })).into_response())
}

#[derive(Deserialize, Default)]
struct ModelsQuery {
    q: Option<String>,
    all: Option<String>,
}

#[derive(Serialize)]
struct ModelInfo {
    id: String,
    name: String,
    #[serde(rename = "inPerM")]
    in_per_m: f64,
    #[serde(rename = "outPerM")]
    out_per_m: f64,
    #[serde(rename = "contextLength")]
    context_length: u32,
    #[serde(rename = "supportsTools")]
    supports_tools: bool,
    #[serde(rename = "batchOnly")]
    batch_only: bool,
    mainstream: bool,
}

#[derive(Serialize)]
struct ModelsResponse {
    models: Vec<ModelInfo>,
    total: usize,
    #[serde(rename = "mainstreamTotal")]
    mainstream_total: usize,
    #[serde(rename = "defaultModel")]
    default_model: String,
}

/// Models search handler
async fn get_models_route(
    State(state): State<AppState>,
    Query(q): Query<ModelsQuery>,
) -> Result<Response, crate::AppError> {
    let catalog = Arc::clone(&state.catalog);
    let all = match catalog.list().await {
        Ok(models) => models,
        Err(e) => return Ok(catalog_error_response(&e)),
    };

    let db = state.db();

    let search = q.q.map(|s| s.to_lowercase()).unwrap_or_default();
    let show_all = q.all.as_deref() == Some("1");

    // F12: the curated list Josh actually configured (`models.mainstream`),
    // not "every model the catalogue happens to list" - that made `all=1`
    // a no-op, since `mainstream`/`mainstreamTotal` were always the full set.
    let mainstream_ids: std::collections::HashSet<String> =
        get_mainstream_ids(&db).into_iter().collect();

    let matched_all: Vec<_> = if search.is_empty() {
        all
    } else {
        all.into_iter()
            .filter(|m| {
                m.id.to_lowercase().contains(&search) || m.name.to_lowercase().contains(&search)
            })
            .collect()
    };

    let matched_mainstream: Vec<_> = matched_all
        .iter()
        .filter(|m| mainstream_ids.contains(&m.id))
        .cloned()
        .collect();

    let scoped = if show_all {
        &matched_all
    } else {
        &matched_mainstream
    };
    let total = matched_all.len();
    let mainstream_total = matched_mainstream.len();

    let models: Vec<ModelInfo> = scoped
        .iter()
        .take(60)
        .map(|m| ModelInfo {
            id: m.id.clone(),
            name: m.name.clone(),
            in_per_m: m.in_per_m,
            out_per_m: m.out_per_m,
            context_length: m.context_length,
            supports_tools: m.supports_tools,
            batch_only: m.id.ends_with(":batch"),
            mainstream: mainstream_ids.contains(&m.id),
        })
        .collect();

    let default = get_default_model(&db);

    Ok(Json(ModelsResponse {
        models,
        total,
        mainstream_total,
        default_model: default,
    })
    .into_response())
}

/// F11: an upstream catalogue failure is "the catalogue is down", not "you
/// sent something wrong" - `app.ts:1419` answers 502, and `redact`s the
/// message since an OpenRouter error body can carry the key or the request
/// URL. No key is threaded through to this call site, so this only strips
/// `sk-or-...`-shaped tokens (the "ours or not" half `redact`'s own doc
/// describes) - still the right shape of answer, never the raw upstream text.
fn catalog_error_response(err: &str) -> Response {
    let message = model::redact(err, None);
    (StatusCode::BAD_GATEWAY, Json(json!({ "error": message }))).into_response()
}

/* ------------------------------------------------------------------------ */
/* F12: the curated "mainstream" model list                                  */
/* ------------------------------------------------------------------------ */

const MAINSTREAM_KEY: &str = "models.mainstream";

struct MainstreamModel {
    id: &'static str,
    label: &'static str,
}

/// The default fifteen. Port of `DEFAULT_MAINSTREAM`
/// (`projects/bullpen-night/src/server/mainstream.ts:32-48`).
const DEFAULT_MAINSTREAM: &[MainstreamModel] = &[
    MainstreamModel {
        id: "anthropic/claude-fable-5.1",
        label: "Fable 5.1",
    },
    MainstreamModel {
        id: "anthropic/claude-opus-5",
        label: "Opus 5",
    },
    MainstreamModel {
        id: "anthropic/claude-sonnet-5",
        label: "Sonnet 5",
    },
    MainstreamModel {
        id: "anthropic/claude-haiku-4.5",
        label: "Haiku 4.5",
    },
    MainstreamModel {
        id: "openai/gpt-6-astra",
        label: "GPT-6 Astra",
    },
    MainstreamModel {
        id: "openai/gpt-5.6-terra",
        label: "GPT-5.6 Terra",
    },
    MainstreamModel {
        id: "openai/gpt-5.6-sol",
        label: "GPT-5.6 Sol",
    },
    MainstreamModel {
        id: "openai/gpt-5.6-luna",
        label: "GPT-5.6 Luna",
    },
    MainstreamModel {
        id: "openai/gpt-oss-120b",
        label: "GPT-OSS 120B",
    },
    MainstreamModel {
        id: "google/gemini-3.1-pro-preview",
        label: "Gemini 3.1 Pro",
    },
    MainstreamModel {
        id: "google/gemini-3.8-flash",
        label: "Gemini 3.8 Flash",
    },
    MainstreamModel {
        id: "google/gemini-3.5-flash-lite",
        label: "Gemini 3.5 Flash-Lite",
    },
    MainstreamModel {
        id: "google/gemini-2.5-flash-lite",
        label: "Gemini 2.5 Flash-Lite",
    },
    MainstreamModel {
        id: "x-ai/grok-4.6",
        label: "Grok 4.6",
    },
    MainstreamModel {
        id: "x-ai/grok-build-0.1",
        label: "Grok Build 0.1",
    },
];

fn default_mainstream_ids() -> Vec<String> {
    DEFAULT_MAINSTREAM
        .iter()
        .map(|m| m.id.to_string())
        .collect()
}

/// The ids Josh currently has configured, or the default fifteen if he has
/// never touched it. Port of `getMainstreamIds` (`mainstream.ts:53`).
fn get_mainstream_ids(db: &store::Db) -> Vec<String> {
    let raw = match db.settings_get(MAINSTREAM_KEY) {
        Ok(Some(v)) => v,
        _ => return default_mainstream_ids(),
    };
    match serde_json::from_str::<Vec<String>>(&raw) {
        Ok(ids) if !ids.is_empty() => ids,
        _ => default_mainstream_ids(),
    }
}

/// Port of `setMainstreamIds` (`mainstream.ts:69`).
fn set_mainstream_ids(db: &store::Db, ids: &[String]) -> Result<(), crate::AppError> {
    let json = serde_json::to_string(ids)?;
    db.settings_set(MAINSTREAM_KEY, &json)?;
    Ok(())
}

/// The curated label for an id, or its slug with the vendor prefix
/// stripped. Port of `labelFor` (`mainstream.ts:77`).
fn label_for(id: &str) -> String {
    if let Some(known) = DEFAULT_MAINSTREAM.iter().find(|m| m.id == id) {
        return known.label.to_string();
    }
    match id.split_once('/') {
        Some((_, rest)) => rest.to_string(),
        None => id.to_string(),
    }
}

#[derive(Serialize)]
struct MainstreamEntry {
    id: String,
    label: String,
}

#[derive(Serialize)]
struct MainstreamResponse {
    ids: Vec<String>,
    models: Vec<MainstreamEntry>,
}

fn mainstream_response(ids: Vec<String>) -> MainstreamResponse {
    let models = ids
        .iter()
        .map(|id| MainstreamEntry {
            id: id.clone(),
            label: label_for(id),
        })
        .collect();
    MainstreamResponse { ids, models }
}

/// `GET /api/models/mainstream` - the curated list itself, independent of
/// the catalogue search above. Answers even when OpenRouter is unreachable,
/// since it is just a settings row (`app.ts:1429`).
async fn get_mainstream_route(State(state): State<AppState>) -> Result<Response, crate::AppError> {
    let db = state.db();
    let ids = get_mainstream_ids(&db);
    Ok(Json(mainstream_response(ids)).into_response())
}

#[derive(Deserialize, Default)]
struct MainstreamBody {
    ids: Option<Vec<serde_json::Value>>,
}

/// `PUT /api/models/mainstream` - every id checked against the catalogue,
/// all-or-nothing, same as `validateMainstreamIds` (`mainstream.ts:98`).
async fn put_mainstream_route(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Response, crate::AppError> {
    let parsed: MainstreamBody = parse_body(&body)?;
    let raw = match parsed.ids {
        Some(v) => v,
        None => {
            return Err(crate::AppError::bad_request(
                "ids must be a list of model ids",
            ));
        }
    };

    let mut ids: Vec<String> = Vec::new();
    for v in raw {
        if let serde_json::Value::String(s) = v {
            let s = s.trim().to_string();
            if !s.is_empty() && !ids.contains(&s) {
                ids.push(s);
            }
        }
    }
    if ids.is_empty() {
        return Err(crate::AppError::bad_request(
            "at least one model is required",
        ));
    }

    for id in &ids {
        // Same batch-variant trap `judge_pin` refuses at pin time: sits
        // right beside the real model in OpenRouter's list.
        if id.ends_with(":batch") {
            return Err(crate::AppError::bad_request(format!(
                "{id} is the batch-only version of the model. It can only be reached through the Batch API."
            )));
        }
        // A rolling alias points at whatever OpenRouter currently calls
        // that, which a curated, stable list exists to avoid.
        if id.contains('~') {
            return Err(crate::AppError::bad_request(format!(
                "{id} is a rolling alias, not a pinned model id."
            )));
        }
    }

    for id in &ids {
        match state.catalog.get(id).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                return Err(crate::AppError::bad_request(format!(
                    "OpenRouter does not list {id}. Check the id."
                )));
            }
            Err(e) => return Ok(catalog_error_response(&e)),
        }
    }

    set_mainstream_ids(&state.db(), &ids)?;
    Ok(Json(mainstream_response(ids)).into_response())
}

/// Routing GET handler
async fn get_routing(State(state): State<AppState>) -> Result<Response, crate::AppError> {
    let db = state.db();
    let settings = get_routing_settings(&db)?;
    let log = model::routing::list_routing_log(&db, 20)?;
    Ok(Json(json!({
        "enabled": settings.enabled,
        "text": settings.text,
        "log": log,
    }))
    .into_response())
}

#[derive(Deserialize, Default)]
struct RoutingBody {
    enabled: Option<bool>,
    text: Option<String>,
}

/// Routing PUT handler
async fn put_routing(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Response, crate::AppError> {
    let db = state.db();
    let parsed: RoutingBody = parse_body(&body)?;

    let settings = set_routing_settings(&db, parsed.enabled, parsed.text)?;
    let log = model::routing::list_routing_log(&db, 20)?;

    Ok(Json(json!({
        "enabled": settings.enabled,
        "text": settings.text,
        "log": log,
    }))
    .into_response())
}

/// F9: TS's key, values, and default (`second-opinion.ts:59-78`) - "the
/// switch here is the OFF switch", so a fresh db (no row at all) reads
/// `true`, and a "1"/"0" value written by a real TS `bullpen.db` parses
/// correctly here instead of `"1".parse::<bool>()` erring.
const SECOND_OPINION_KEY: &str = "models.secondOpinionDefault";

/// Second opinion GET handler
async fn get_second_opinion(State(state): State<AppState>) -> Result<Response, crate::AppError> {
    let db = state.db();
    let enabled = db
        .settings_get(SECOND_OPINION_KEY)
        .ok()
        .flatten()
        .map(|v| v == "1")
        .unwrap_or(true);

    Ok(Json(json!({ "enabled": enabled })).into_response())
}

#[derive(Deserialize, Default)]
struct SecondOpinionBody {
    enabled: Option<serde_json::Value>,
}

/// Second opinion PUT handler. F9: a missing or non-boolean `enabled` is a
/// 400, matching `app.ts:4396` - it must not silently store `false`.
async fn put_second_opinion(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Response, crate::AppError> {
    let db = state.db();
    let parsed: SecondOpinionBody = parse_body(&body)?;

    let enabled = match parsed.enabled {
        Some(serde_json::Value::Bool(b)) => b,
        _ => return Err(crate::AppError::bad_request("enabled must be a boolean")),
    };
    db.settings_set(SECOND_OPINION_KEY, if enabled { "1" } else { "0" })?;

    Ok(Json(json!({ "enabled": enabled })).into_response())
}
