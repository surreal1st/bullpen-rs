//! Marketplace HTTP routes — port of `app.ts` marketplace section (templates +
//! install first; plugins shelf fills in S10-07).

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use model::judge_pin;
use serde_json::{Value, json};
use std::collections::HashSet;

use crate::AppState;
use crate::catalogue::{
    BOT_DIRECTORY_URL, MarketplaceCard, card_to_offering, fetch_bot_cards, template_cards,
    templates_dir,
};
use crate::marketplace::{describe_install, install_bot, install_open_markdown};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/marketplace/plugins", get(list_plugins))
        .route("/api/marketplace/bots", get(list_bots))
        .route("/api/marketplace/templates", get(list_templates))
        .route("/api/marketplace/install", post(install_card))
}

async fn list_plugins() -> impl IntoResponse {
    Json(json!({
        "ok": true,
        "cards": [],
    }))
}

async fn list_bots(State(state): State<AppState>) -> impl IntoResponse {
    let url = std::env::var("BULLPEN_BOT_DIRECTORY_URL")
        .unwrap_or_else(|_| BOT_DIRECTORY_URL.to_string());
    let client = reqwest::Client::new();
    let (ok, mut cards, error) = fetch_bot_cards(&client, &url).await;
    let installed = installed_bot_names(state);
    for card in &mut cards {
        card.installed = installed.contains(&card.name.to_lowercase());
    }
    Json(json!({
        "ok": ok,
        "cards": cards,
        "error": error,
    }))
}

async fn list_templates(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, crate::AppError> {
    let mut cards = {
        let db = state.db();
        template_cards(&db)?
    };
    let installed = installed_bot_names(state);
    for card in &mut cards {
        card.installed = installed.contains(&card.name.to_lowercase());
    }
    Ok(Json(json!({ "ok": true, "cards": cards })))
}

fn installed_bot_names(state: AppState) -> HashSet<String> {
    let db = state.db();
    store::list_roster(&db)
        .unwrap_or_default()
        .into_iter()
        .map(|b| b.name.to_lowercase())
        .collect()
}

async fn install_card(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, crate::AppError> {
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let wanted = parsed
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let kind = parsed.get("kind").and_then(|v| v.as_str()).unwrap_or("bot");

    if kind == "connector" {
        return Ok((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": "connector install is not available yet" })),
        )
            .into_response());
    }

    let pool = resolve_bot_pool(&state).await?;
    let Some(card) = pool.into_iter().find(|c| c.name == wanted) else {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such card" })),
        )
            .into_response());
    };

    if card.unavailable.is_some() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": card.unavailable })),
        )
            .into_response());
    }

    let Some(offering) = resolve_offering(&state, &card)? else {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "that card cannot be installed" })),
        )
            .into_response());
    };

    let template_markdown = if let Some(template_id) = &card.template_id {
        let path = templates_dir().join(format!("{template_id}.md"));
        if path.is_file() {
            Some(std::fs::read_to_string(&path).map_err(|e| {
                crate::AppError::bad_request(format!("could not read template file: {e}"))
            })?)
        } else {
            None
        }
    } else {
        None
    };

    if let Some(text) = &template_markdown {
        let parsed = crate::import_open::parse_open_bot("template.md", text);
        if let Some(model) = &parsed.model {
            let verdict = judge_pin(state.catalog.as_ref(), model, false).await;
            if !verdict.ok {
                return Err(crate::AppError::bad_request(
                    verdict
                        .refusal
                        .unwrap_or_else(|| "that model cannot be pinned".to_string()),
                ));
            }
        }
    } else if let Some(template_id) = &card.template_id {
        let model = {
            let db = state.db();
            store::get_bot(&db, template_id)?.and_then(|b| b.model)
        };
        if let Some(model) = model {
            let verdict = judge_pin(state.catalog.as_ref(), &model, false).await;
            if !verdict.ok {
                return Err(crate::AppError::bad_request(
                    verdict
                        .refusal
                        .unwrap_or_else(|| "that model cannot be pinned".to_string()),
                ));
            }
        }
    }

    let db = state.db();
    let result = if let Some(text) = template_markdown {
        install_open_markdown(&db, "template.md", &text)?
    } else {
        install_bot(&db, &offering)?
    };
    if !result.ok {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": result.error })),
        )
            .into_response());
    }

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "ok": true,
            "installed": result.what,
            "id": result.id,
            "grants": describe_install(&offering),
        })),
    )
        .into_response())
}

async fn resolve_bot_pool(state: &AppState) -> Result<Vec<MarketplaceCard>, crate::AppError> {
    let mut pool = {
        let db = state.db();
        template_cards(&db)?
    };
    let url = std::env::var("BULLPEN_BOT_DIRECTORY_URL")
        .unwrap_or_else(|_| BOT_DIRECTORY_URL.to_string());
    let client = reqwest::Client::new();
    let (_ok, bot_cards, _err) = fetch_bot_cards(&client, &url).await;
    pool.extend(bot_cards);
    Ok(pool)
}

fn resolve_offering(
    state: &AppState,
    card: &MarketplaceCard,
) -> Result<Option<crate::marketplace::Offering>, crate::AppError> {
    if let Some(offering) = card_to_offering(card) {
        return Ok(Some(offering));
    }
    if let Some(template_id) = &card.template_id {
        let db = state.db();
        if let Some(bot) = store::get_bot(&db, template_id)? {
            return Ok(Some(crate::marketplace::Offering::Bot {
                name: bot.name,
                purpose: bot.purpose,
                instructions: bot.instructions,
            }));
        }
    }
    Ok(None)
}
