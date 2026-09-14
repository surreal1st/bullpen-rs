//! S2-06: GET/PUT routes for theme, timezone, rules, models, routing, second opinion.
//! Port of `projects/bullpen-night/src/server/app.ts:2902-3080,4374-4400`.

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

use super::parse_body;
use crate::AppState;
use crate::prompt::{house_rules, set_house_rules};
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
    Ok(Json(json!({ "rules": rules })).into_response())
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

    let rules = match parsed.rules {
        Some(r) => r,
        None => String::new(),
    };

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
    let db = state.db();
    let parsed: ModelBody = parse_body(&body)?;

    let model = match parsed.model {
        Some(m) if !m.trim().is_empty() => m.trim().to_string(),
        _ => return Err(crate::AppError::bad_request("give a model id")),
    };

    // A premium default is refused outright
    if looks_premium(&model) || model == get_premium_model(&db) {
        return Err(crate::AppError::bad_request(
            "That is a premium model, and this is what every unpinned bot and every routine falls \
             back to. It is also what a timer run downgrades TO, so setting it here would defeat \
             the downgrade entirely. Pick a cheap model; premium belongs on Escalate.",
        ));
    }

    let saved = set_default_model(&db, &model);
    Ok(Json(json!({ "model": saved })).into_response())
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
        Err(e) => return Err(crate::AppError::bad_request(format!("catalog error: {e}"))),
    };

    let db = state.db();

    let search = q.q.map(|s| s.to_lowercase()).unwrap_or_default();
    let show_all = q.all.as_deref() == Some("1");

    // For now, all models are considered "mainstream" (would read from settings in full implementation)
    let mainstream_ids = all
        .iter()
        .map(|m| m.id.clone())
        .collect::<std::collections::HashSet<_>>();

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

/// Second opinion GET handler
async fn get_second_opinion(State(state): State<AppState>) -> Result<Response, crate::AppError> {
    let db = state.db();
    let enabled = db
        .settings_get("second_opinion.platform_default")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(false);

    Ok(Json(json!({ "enabled": enabled })).into_response())
}

#[derive(Deserialize, Default)]
struct SecondOpinionBody {
    enabled: Option<bool>,
}

/// Second opinion PUT handler
async fn put_second_opinion(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Response, crate::AppError> {
    let db = state.db();
    let parsed: SecondOpinionBody = parse_body(&body)?;

    let enabled = parsed.enabled.unwrap_or(false);
    db.settings_set("second_opinion.platform_default", &enabled.to_string())?;

    Ok(Json(json!({ "enabled": enabled })).into_response())
}
