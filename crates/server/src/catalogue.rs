//! Marketplace catalogue — templates on disk, bot directory fetch, and card
//! shapes for the HTTP API. Port of slices of
//! `projects/bullpen-night/src/server/catalogue.ts` + `app.ts` templates route.

use crate::import_open::parse_open_bot;
use crate::marketplace::Offering;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use store::Db;

pub const BOT_DIRECTORY_URL: &str = "https://botdirectory.ai/api/bots.json";
const CACHE_MS: u64 = 60 * 60 * 1000;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceCard {
    pub kind: String,
    pub name: String,
    pub purpose: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub integrations: Vec<String>,
    pub source_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_access: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub does: Option<Vec<String>>,
    pub installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_template: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template_id: Option<String>,
    /// Bot install payload — never sent to the client on list routes; used
    /// server-side when resolving `POST /api/marketplace/install`.
    #[serde(skip)]
    pub instructions: Option<String>,
    /// MCP endpoint for built-in connectors — server-side install resolution.
    #[serde(skip)]
    pub connector_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawBuiltInPlugin {
    kind: String,
    name: String,
    purpose: String,
    url: String,
    category: String,
    integrations: Vec<String>,
    #[serde(rename = "sourceName")]
    source_name: String,
    #[serde(default)]
    open_access: Option<bool>,
    #[serde(default)]
    does: Option<Vec<String>>,
}

static BUILT_IN_PLUGINS: OnceLock<Vec<MarketplaceCard>> = OnceLock::new();

/// Built-in plugin shelf — port of `BUILT_IN_PLUGINS` in bullpen-night
/// `catalogue.ts` (data in `data/built_in_plugins.json`).
pub fn built_in_plugin_cards() -> Vec<MarketplaceCard> {
    BUILT_IN_PLUGINS
        .get_or_init(|| {
            let raw: Vec<RawBuiltInPlugin> =
                serde_json::from_str(include_str!("../data/built_in_plugins.json"))
                    .expect("built_in_plugins.json must parse");
            raw.into_iter()
                .map(|p| MarketplaceCard {
                    kind: p.kind,
                    name: p.name,
                    purpose: p.purpose,
                    category: Some(p.category),
                    integrations: p.integrations,
                    source_name: p.source_name,
                    detail_url: None,
                    unavailable: None,
                    open_access: p.open_access,
                    does: p.does,
                    installed: false,
                    is_template: None,
                    template_id: None,
                    instructions: None,
                    connector_url: Some(p.url),
                })
                .collect()
        })
        .clone()
}

pub fn templates_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("BULLPEN_TEMPLATES_DIR") {
        return PathBuf::from(dir);
    }
    PathBuf::from("templates")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

fn first_sentence(text: &str) -> String {
    let stop = text
        .find(['.', '!', '?'])
        .map(|i| i + 1)
        .unwrap_or(text.len());
    text.chars()
        .take(stop.min(300))
        .collect::<String>()
        .trim()
        .to_string()
}

/// Turns botdirectory JSON into installable cards.
pub fn read_directory(payload: &serde_json::Value, source_name: &str) -> Vec<MarketplaceCard> {
    let Some(bots) = payload.get("bots").and_then(|v| v.as_array()) else {
        return vec![];
    };

    let mut cards = Vec::new();
    for raw in bots.iter().take(2000) {
        let Some(bot) = raw.as_object() else {
            continue;
        };
        let name = bot
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.chars().take(80).collect::<String>())
            .unwrap_or_default()
            .trim()
            .to_string();
        if name.is_empty() {
            continue;
        }
        let prompt = bot
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(|s| s.chars().take(200_000).collect::<String>())
            .unwrap_or_default();
        let prompt_trim = prompt.trim();
        let description = bot
            .get("description")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.chars().take(300).collect::<String>())
            .unwrap_or_else(|| first_sentence(&prompt));
        let integrations: Vec<String> = bot
            .get("integrations")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .take(8)
                    .collect()
            })
            .unwrap_or_default();
        let category = bot
            .get("category")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "Other".to_string());
        let detail_url = bot
            .get("detailUrl")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        cards.push(MarketplaceCard {
            kind: "bot".to_string(),
            name: name.clone(),
            purpose: description,
            category: Some(category),
            integrations,
            source_name: source_name.to_string(),
            detail_url,
            unavailable: if prompt_trim.is_empty() {
                Some(
                    "This one is share-only: the directory lists it but does not publish its prompt."
                        .to_string(),
                )
            } else {
                None
            },
            open_access: None,
            does: None,
            installed: false,
            is_template: None,
            template_id: None,
            instructions: if prompt_trim.is_empty() {
                None
            } else {
                Some(prompt)
            },
            connector_url: None,
        });
    }
    cards
}

fn read_template_file(path: &Path, template_id: &str) -> Option<MarketplaceCard> {
    let content = std::fs::read_to_string(path).ok()?;
    let parsed = parse_open_bot(path.file_name()?.to_str().unwrap_or("bot.md"), &content);
    Some(MarketplaceCard {
        kind: "bot".to_string(),
        name: parsed.name,
        purpose: parsed.purpose,
        category: Some("Starter".to_string()),
        integrations: vec![],
        source_name: "Bullpen".to_string(),
        detail_url: None,
        unavailable: None,
        open_access: None,
        does: None,
        installed: false,
        is_template: Some(true),
        template_id: Some(template_id.to_string()),
        instructions: Some(parsed.instructions),
        connector_url: None,
    })
}

pub fn bundled_template_cards() -> Vec<MarketplaceCard> {
    let dir = templates_dir();
    let Ok(read_dir) = std::fs::read_dir(&dir) else {
        return vec![];
    };
    let mut cards = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if let Some(card) = read_template_file(&path, stem) {
            cards.push(card);
        }
    }
    cards.sort_by(|a, b| a.name.cmp(&b.name));
    cards
}

pub fn user_template_cards(db: &Db) -> rusqlite::Result<Vec<MarketplaceCard>> {
    let mut stmt = db.conn().prepare(
        "SELECT id, name, purpose FROM bots WHERE is_template = 1 AND archived_at IS NULL",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut cards = Vec::new();
    for row in rows {
        let (id, name, purpose) = row?;
        cards.push(MarketplaceCard {
            kind: "bot".to_string(),
            name,
            purpose,
            category: Some("Saved".to_string()),
            integrations: vec![],
            source_name: "Your templates".to_string(),
            detail_url: None,
            unavailable: None,
            open_access: None,
            does: None,
            installed: false,
            is_template: Some(true),
            template_id: Some(id),
            instructions: None,
            connector_url: None,
        });
    }
    Ok(cards)
}

pub fn template_cards(db: &Db) -> rusqlite::Result<Vec<MarketplaceCard>> {
    let mut cards = bundled_template_cards();
    cards.extend(user_template_cards(db)?);
    Ok(cards)
}

pub fn card_to_offering(card: &MarketplaceCard) -> Option<Offering> {
    if card.unavailable.is_some() {
        return None;
    }
    match card.kind.as_str() {
        "connector" => {
            let url = card
                .connector_url
                .clone()
                .or_else(|| card.detail_url.clone())
                .unwrap_or_default();
            Some(Offering::Connector {
                name: card.name.clone(),
                purpose: card.purpose.clone(),
                url,
            })
        }
        "bot" => {
            let instructions = card.instructions.clone().or_else(|| {
                card.template_id.as_ref().and_then(|id| {
                    let path = templates_dir().join(format!("{id}.md"));
                    read_template_file(&path, id).and_then(|c| c.instructions)
                })
            })?;
            if instructions.trim().is_empty() {
                return None;
            }
            Some(Offering::Bot {
                name: card.name.clone(),
                purpose: card.purpose.clone(),
                instructions,
            })
        }
        _ => None,
    }
}

struct BotCache {
    at_ms: u64,
    cards: Vec<MarketplaceCard>,
}

static BOT_CACHE: OnceLock<Mutex<Option<BotCache>>> = OnceLock::new();

fn bot_cache() -> &'static Mutex<Option<BotCache>> {
    BOT_CACHE.get_or_init(|| Mutex::new(None))
}

/// Fetch botdirectory cards. Uses an in-memory cache like the TS server.
pub async fn fetch_bot_cards(
    client: &reqwest::Client,
    url: &str,
) -> (bool, Vec<MarketplaceCard>, Option<String>) {
    if url.contains("no-network") {
        return (true, vec![], None);
    }
    {
        let guard = bot_cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cache) = guard.as_ref()
            && now_ms().saturating_sub(cache.at_ms) < CACHE_MS
        {
            return (true, cache.cards.clone(), None);
        }
    }

    let response = client
        .get(url)
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(30))
        .send()
        .await;

    match response {
        Ok(res) if res.status().is_success() => {
            let body: serde_json::Value = match res.json().await {
                Ok(v) => v,
                Err(err) => {
                    return stale_or_error(&format!("could not read the directory: {err}"));
                }
            };
            let cards = read_directory(&body, "botdirectory.ai");
            let mut guard = bot_cache().lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(BotCache {
                at_ms: now_ms(),
                cards: cards.clone(),
            });
            (true, cards, None)
        }
        Ok(res) => stale_or_error(&format!("the directory returned {}", res.status())),
        Err(err) => stale_or_error(&err.to_string()),
    }
}

fn stale_or_error(message: &str) -> (bool, Vec<MarketplaceCard>, Option<String>) {
    let guard = bot_cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cache) = guard.as_ref() {
        return (true, cache.cards.clone(), None);
    }
    (false, vec![], Some(message.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_plugins_carry_gmail_endpoint() {
        let gmail = built_in_plugin_cards()
            .into_iter()
            .find(|c| c.name == "Gmail")
            .expect("Gmail card");
        assert_eq!(gmail.kind, "connector");
        assert_eq!(
            gmail.connector_url.as_deref(),
            Some("https://gmailmcp.googleapis.com/mcp/v1")
        );
        assert!(
            built_in_plugin_cards()
                .iter()
                .all(|c| c.kind == "connector")
        );
    }

    #[test]
    fn read_directory_drops_blank_names() {
        let cards = read_directory(
            &serde_json::json!({ "bots": [{ "name": "  ", "prompt": "hi" }] }),
            "test",
        );
        assert!(cards.is_empty());
    }
}
