//! Stripe Issuing purchasing — port of `purchasing.ts`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use chrono::{DateTime, Datelike, Utc};
use hmac::{Hmac, Mac};
use model::ToolSpec;
use regex::Regex;
use serde::Serialize;
use serde_json::json;
use sha2::Sha256;
use store::Db;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::permissions::Decision;
use crate::settings_secrets::{decrypt_for_storage, encrypt_for_storage};
use crate::tools::purchase;

const KEY_SECRET: &str = "stripe.secret_key";
const KEY_WEBHOOK_SECRET: &str = "stripe.webhook_secret";
const KEY_CARDHOLDER: &str = "stripe.cardholder_id";
const KEY_ALLOW_LIVE: &str = "stripe.allow_live";

pub const STRIPE_REPLAY_TOLERANCE_SECONDS: u64 = 5 * 60;

pub fn ensure_purchasing_schema(db: &Db) -> rusqlite::Result<()> {
    let columns: Vec<String> = db
        .conn()
        .prepare("PRAGMA table_info(bots)")?
        .query_map([], |row| row.get(1))?
        .filter_map(Result::ok)
        .collect();
    let has = |name: &str| columns.iter().any(|c| c == name);
    if !has("can_buy") {
        db.conn().execute(
            "ALTER TABLE bots ADD COLUMN can_buy INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !has("monthly_limit_usd") {
        db.conn()
            .execute("ALTER TABLE bots ADD COLUMN monthly_limit_usd REAL", [])?;
    }
    if !has("card_id") {
        db.conn()
            .execute("ALTER TABLE bots ADD COLUMN card_id TEXT", [])?;
    }
    if !has("card_last4") {
        db.conn()
            .execute("ALTER TABLE bots ADD COLUMN card_last4 TEXT", [])?;
    }
    db.conn().execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS purchases (
          id                       TEXT PRIMARY KEY,
          bot_id                   TEXT NOT NULL,
          merchant                 TEXT NOT NULL,
          amount_usd               REAL NOT NULL,
          reason                   TEXT NOT NULL,
          status                   TEXT NOT NULL DEFAULT 'pending',
          stripe_authorization_id  TEXT,
          settled_amount_usd       REAL,
          settled_at               TEXT,
          approved_at              TEXT NOT NULL,
          created_at               TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_purchases_bot ON purchases(bot_id, created_at);
        CREATE INDEX IF NOT EXISTS idx_purchases_auth ON purchases(stripe_authorization_id);
        "#,
    )?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BotPurchasing {
    pub can_buy: bool,
    pub monthly_limit_usd: Option<f64>,
    pub card_id: Option<String>,
    pub card_last4: Option<String>,
}

type BotPurchasingRow = (i64, Option<f64>, Option<String>, Option<String>);

pub fn get_bot_purchasing(db: &Db, bot_id: &str) -> BotPurchasing {
    let row: Option<BotPurchasingRow> = db
        .conn()
        .query_row(
            "SELECT can_buy, monthly_limit_usd, card_id, card_last4 FROM bots WHERE id = ?1",
            [bot_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .ok();
    match row {
        Some((can_buy, limit, card_id, last4)) => BotPurchasing {
            can_buy: can_buy == 1,
            monthly_limit_usd: limit,
            card_id,
            card_last4: last4,
        },
        None => BotPurchasing {
            can_buy: false,
            monthly_limit_usd: None,
            card_id: None,
            card_last4: None,
        },
    }
}

pub fn set_bot_purchasing(
    db: &Db,
    bot_id: &str,
    can_buy: Option<bool>,
    monthly_limit_usd: Option<Option<f64>>,
) -> rusqlite::Result<BotPurchasing> {
    let current = get_bot_purchasing(db, bot_id);
    let can_buy = can_buy.unwrap_or(current.can_buy);
    let monthly_limit_usd = match monthly_limit_usd {
        None => current.monthly_limit_usd,
        Some(None) => None,
        Some(Some(v)) if v.is_finite() && v >= 0.0 => Some(v),
        Some(Some(_)) => current.monthly_limit_usd,
    };
    db.conn().execute(
        "UPDATE bots SET can_buy = ?1, monthly_limit_usd = ?2 WHERE id = ?3",
        rusqlite::params![i32::from(can_buy), monthly_limit_usd, bot_id],
    )?;
    Ok(get_bot_purchasing(db, bot_id))
}

fn set_bot_card(db: &Db, bot_id: &str, card_id: &str, last4: &str) -> rusqlite::Result<()> {
    db.conn().execute(
        "UPDATE bots SET card_id = ?1, card_last4 = ?2 WHERE id = ?3",
        rusqlite::params![card_id, last4, bot_id],
    )?;
    Ok(())
}

fn get_setting(db: &Db, key: &str) -> Option<String> {
    db.settings_get(key).ok().flatten()
}

fn put_setting(db: &Db, key: &str, value: &str) -> rusqlite::Result<()> {
    db.settings_set(key, value)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StripeKeyKind {
    Test,
    Live,
    Invalid,
}

pub fn classify_stripe_key(raw: &str) -> StripeKeyKind {
    let key = raw.trim();
    static TEST: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static LIVE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let test = TEST.get_or_init(|| Regex::new(r"^sk_test_[A-Za-z0-9]+$").expect("test key regex"));
    let live = LIVE.get_or_init(|| Regex::new(r"^sk_live_[A-Za-z0-9]+$").expect("live key regex"));
    if test.is_match(key) {
        StripeKeyKind::Test
    } else if live.is_match(key) {
        StripeKeyKind::Live
    } else {
        StripeKeyKind::Invalid
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct KeyGateResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn validate_stripe_key(raw: &str, allow_live: bool) -> KeyGateResult {
    let kind = classify_stripe_key(raw);
    match kind {
        StripeKeyKind::Invalid => KeyGateResult {
            ok: false,
            error: Some(
                r#"That is not a Stripe secret key - it should start "sk_test_" or "sk_live_"."#
                    .to_string(),
            ),
        },
        StripeKeyKind::Live if !allow_live => KeyGateResult {
            ok: false,
            error: Some(
                r#"A live key is refused until "Allow live keys" is switched on. Test mode (sk_test_...) always works."#
                    .to_string(),
            ),
        },
        _ => KeyGateResult {
            ok: true,
            error: None,
        },
    }
}

pub fn is_allow_live(db: &Db) -> bool {
    get_setting(db, KEY_ALLOW_LIVE).as_deref() == Some("1")
}

pub fn set_allow_live(db: &Db, allow: bool) -> rusqlite::Result<()> {
    put_setting(db, KEY_ALLOW_LIVE, if allow { "1" } else { "0" })
}

pub struct StripeConfig {
    pub secret_key: Option<String>,
    pub webhook_secret: Option<String>,
    pub cardholder_id: Option<String>,
    pub allow_live: bool,
}

pub fn get_stripe_config(db: &Db) -> StripeConfig {
    let secret_enc = get_setting(db, KEY_SECRET);
    let webhook_enc = get_setting(db, KEY_WEBHOOK_SECRET);
    let cardholder_enc = get_setting(db, KEY_CARDHOLDER);
    StripeConfig {
        secret_key: secret_enc
            .as_deref()
            .and_then(|enc| decrypt_for_storage(db, enc)),
        webhook_secret: webhook_enc
            .as_deref()
            .and_then(|enc| decrypt_for_storage(db, enc)),
        cardholder_id: cardholder_enc
            .as_deref()
            .and_then(|enc| decrypt_for_storage(db, enc)),
        allow_live: is_allow_live(db),
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PurchasingStatus {
    pub has_secret_key: bool,
    pub key_kind: Option<StripeKeyKind>,
    pub has_webhook_secret: bool,
    pub has_cardholder_id: bool,
    pub allow_live: bool,
}

pub fn purchasing_status(db: &Db) -> PurchasingStatus {
    let config = get_stripe_config(db);
    PurchasingStatus {
        has_secret_key: config.secret_key.is_some(),
        key_kind: config.secret_key.as_deref().map(classify_stripe_key),
        has_webhook_secret: config.webhook_secret.is_some(),
        has_cardholder_id: config.cardholder_id.as_ref().is_some_and(|s| !s.is_empty()),
        allow_live: config.allow_live,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SaveKeysResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<PurchasingStatus>,
}

pub fn save_stripe_keys(
    db: &Db,
    secret_key: Option<&str>,
    webhook_secret: Option<&str>,
    cardholder_id: Option<&str>,
) -> SaveKeysResult {
    if let Some(raw) = secret_key.map(str::trim).filter(|s| !s.is_empty()) {
        let gate = validate_stripe_key(raw, is_allow_live(db));
        if !gate.ok {
            return SaveKeysResult {
                ok: false,
                error: gate
                    .error
                    .or_else(|| Some("That key was refused.".to_string())),
                status: None,
            };
        }
        match encrypt_for_storage(db, raw) {
            Ok(enc) => {
                if put_setting(db, KEY_SECRET, &enc).is_err() {
                    return SaveKeysResult {
                        ok: false,
                        error: Some("Could not save the secret key.".to_string()),
                        status: None,
                    };
                }
            }
            Err(e) => {
                return SaveKeysResult {
                    ok: false,
                    error: Some(e),
                    status: None,
                };
            }
        }
    }
    if let Some(raw) = webhook_secret.map(str::trim).filter(|s| !s.is_empty())
        && let Ok(enc) = encrypt_for_storage(db, raw)
    {
        let _ = put_setting(db, KEY_WEBHOOK_SECRET, &enc);
    }
    if let Some(raw) = cardholder_id.map(str::trim).filter(|s| !s.is_empty())
        && let Ok(enc) = encrypt_for_storage(db, raw)
    {
        let _ = put_setting(db, KEY_CARDHOLDER, &enc);
    }
    SaveKeysResult {
        ok: true,
        error: None,
        status: Some(purchasing_status(db)),
    }
}

#[derive(Debug, Clone)]
pub struct StripeResponse {
    pub ok: bool,
    pub status: u16,
    pub body: serde_json::Value,
}

#[async_trait]
pub trait StripeHttp: Send + Sync {
    async fn post_form(
        &self,
        secret_key: &str,
        path: &str,
        form: &HashMap<String, String>,
    ) -> StripeResponse;
}

pub struct ReqwestStripeHttp {
    client: reqwest::Client,
}

impl ReqwestStripeHttp {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("stripe http client"),
        }
    }
}

impl Default for ReqwestStripeHttp {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl StripeHttp for ReqwestStripeHttp {
    async fn post_form(
        &self,
        secret_key: &str,
        path: &str,
        form: &HashMap<String, String>,
    ) -> StripeResponse {
        let body: Vec<(String, String)> =
            form.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        match self
            .client
            .post(format!("https://api.stripe.com/v1/{path}"))
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {secret_key}"),
            )
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .form(&body)
            .send()
            .await
        {
            Ok(res) => {
                let status = res.status().as_u16();
                let body: serde_json::Value = res.json().await.unwrap_or(json!({}));
                StripeResponse {
                    ok: (200..300).contains(&status),
                    status,
                    body,
                }
            }
            Err(err) => StripeResponse {
                ok: false,
                status: 0,
                body: json!({ "error": { "message": err.to_string() } }),
            },
        }
    }
}

fn stripe_error_message(response: &StripeResponse) -> String {
    if let Some(msg) = response
        .body
        .pointer("/error/message")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        return msg.to_string();
    }
    format!(
        "Stripe refused the request ({}).",
        if response.status == 0 {
            "no response".to_string()
        } else {
            response.status.to_string()
        }
    )
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateCardResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last4: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn db_guard(db: &Arc<Mutex<Db>>) -> MutexGuard<'_, Db> {
    db.lock().unwrap_or_else(PoisonError::into_inner)
}

pub async fn create_issuing_card(
    db: &Arc<Mutex<Db>>,
    bot_id: &str,
    stripe: &dyn StripeHttp,
) -> CreateCardResult {
    let (secret_key, cardholder_id, limit) = {
        let guard = db_guard(db);
        if store::get_bot(&guard, bot_id).ok().flatten().is_none() {
            return CreateCardResult {
                ok: false,
                card_id: None,
                last4: None,
                error: Some("No such bot.".to_string()),
            };
        }
        let config = get_stripe_config(&guard);
        let purchasing = get_bot_purchasing(&guard, bot_id);
        (
            config.secret_key,
            config.cardholder_id,
            purchasing.monthly_limit_usd,
        )
    };
    let Some(secret_key) = secret_key else {
        return CreateCardResult {
            ok: false,
            card_id: None,
            last4: None,
            error: Some(
                "Add a Stripe secret key in Settings > Computer > Purchasing first.".to_string(),
            ),
        };
    };
    if cardholder_id.as_deref().unwrap_or("").is_empty() {
        return CreateCardResult {
            ok: false,
            card_id: None,
            last4: None,
            error: Some(
                "Add a cardholder id in Settings > Computer > Purchasing first.".to_string(),
            ),
        };
    }
    let Some(limit) = limit.filter(|l| *l > 0.0) else {
        return CreateCardResult {
            ok: false,
            card_id: None,
            last4: None,
            error: Some("Set a monthly limit for this bot before creating a card.".to_string()),
        };
    };
    let mut form = HashMap::new();
    form.insert("cardholder".to_string(), cardholder_id.unwrap_or_default());
    form.insert("currency".to_string(), "usd".to_string());
    form.insert("type".to_string(), "virtual".to_string());
    form.insert("status".to_string(), "active".to_string());
    form.insert(
        "spending_controls[spending_limits][0][amount]".to_string(),
        format!("{}", (limit * 100.0).round() as i64),
    );
    form.insert(
        "spending_controls[spending_limits][0][interval]".to_string(),
        "monthly".to_string(),
    );
    let response = stripe.post_form(&secret_key, "issuing/cards", &form).await;
    if !response.ok {
        return CreateCardResult {
            ok: false,
            card_id: None,
            last4: None,
            error: Some(stripe_error_message(&response)),
        };
    }
    let card_id = response
        .body
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if card_id.is_empty() {
        return CreateCardResult {
            ok: false,
            card_id: None,
            last4: None,
            error: Some("Stripe did not return a card id.".to_string()),
        };
    }
    let last4 = response
        .body
        .get("last4")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if set_bot_card(&db_guard(db), bot_id, card_id, last4).is_err() {
        return CreateCardResult {
            ok: false,
            card_id: None,
            last4: None,
            error: Some("Could not save the card on this bot.".to_string()),
        };
    }
    CreateCardResult {
        ok: true,
        card_id: Some(card_id.to_string()),
        last4: Some(last4.to_string()),
        error: None,
    }
}

fn month_start_iso(now: DateTime<Utc>) -> String {
    format!("{}-{:02}-01T00:00:00Z", now.year(), now.month())
}

pub fn monthly_spent_usd(db: &Db, bot_id: &str, now: DateTime<Utc>) -> f64 {
    db.conn()
        .query_row(
            "SELECT COALESCE(SUM(CASE WHEN status = 'settled' THEN settled_amount_usd ELSE amount_usd END), 0)
             FROM purchases
             WHERE bot_id = ?1 AND status != 'declined' AND created_at >= ?2",
            rusqlite::params![bot_id, month_start_iso(now)],
            |row| row.get(0),
        )
        .unwrap_or(0.0)
}

#[derive(Debug, Clone, PartialEq)]
pub struct LimitCheck {
    pub exceeds: bool,
    pub limit_usd: f64,
    pub spent_usd: f64,
}

pub fn purchase_would_exceed_limit(
    db: &Db,
    bot_id: &str,
    amount_usd: f64,
    now: DateTime<Utc>,
) -> LimitCheck {
    let purchasing = get_bot_purchasing(db, bot_id);
    let limit_usd = purchasing.monthly_limit_usd.unwrap_or(0.0);
    if limit_usd <= 0.0 {
        return LimitCheck {
            exceeds: true,
            limit_usd: 0.0,
            spent_usd: 0.0,
        };
    }
    let spent_usd = monthly_spent_usd(db, bot_id, now);
    LimitCheck {
        exceeds: spent_usd + amount_usd > limit_usd,
        limit_usd,
        spent_usd,
    }
}

struct PurchaseArgs {
    merchant: String,
    amount_usd: f64,
    reason: String,
}

fn parse_purchase_args(args: &str) -> Result<PurchaseArgs, String> {
    let input: serde_json::Value = if args.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(args).map_err(|_| "Give valid JSON arguments.".to_string())?
    };
    let merchant = input
        .get("merchant")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let amount_usd = input.get("amount_usd").and_then(|v| v.as_f64());
    let reason = input
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if merchant.is_empty() {
        return Err("Give a merchant name.".to_string());
    }
    let Some(amount_usd) = amount_usd.filter(|a| a.is_finite() && *a > 0.0) else {
        return Err("Give a positive amount_usd.".to_string());
    };
    if reason.is_empty() {
        return Err("Say why you want to buy this.".to_string());
    }
    Ok(PurchaseArgs {
        merchant,
        amount_usd,
        reason,
    })
}

pub fn run_purchase_tool(db: &Db, bot_id: &str, args: &str) -> String {
    let parsed = match parse_purchase_args(args) {
        Ok(p) => p,
        Err(msg) => return format!("Refused: {msg}"),
    };
    let check = purchase_would_exceed_limit(db, bot_id, parsed.amount_usd, Utc::now());
    if check.exceeds {
        return if check.limit_usd <= 0.0 {
            "Refused: no monthly limit is set for you, so purchasing is off. Josh was not asked."
                .to_string()
        } else {
            format!(
                "Refused: ${:.2} at {} would bring this month's total to ${:.2}, over your ${:.2} monthly limit. Josh was not asked.",
                parsed.amount_usd,
                parsed.merchant,
                check.spent_usd + parsed.amount_usd,
                check.limit_usd
            )
        };
    }
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    if db
        .conn()
        .execute(
            "INSERT INTO purchases (id, bot_id, merchant, amount_usd, reason, status, approved_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?6)",
            rusqlite::params![
                id,
                bot_id,
                parsed.merchant,
                parsed.amount_usd,
                parsed.reason,
                now,
            ],
        )
        .is_err()
    {
        return "Refused: could not record the purchase.".to_string();
    }
    format!(
        "Approved by Josh. ${:.2} at {} is on file - Stripe will approve the real charge only if it matches (merchant, amount within 5%, your card).",
        parsed.amount_usd, parsed.merchant
    )
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PurchasePublic {
    pub id: String,
    pub merchant: String,
    pub amount_usd: f64,
    pub status: String,
    pub settled_amount_usd: Option<f64>,
    pub created_at: String,
}

pub fn list_purchases(db: &Db, bot_id: &str) -> Vec<PurchasePublic> {
    let Ok(mut stmt) = db.conn().prepare(
        "SELECT id, merchant, amount_usd, status, settled_amount_usd, created_at
         FROM purchases WHERE bot_id = ?1 ORDER BY created_at DESC LIMIT 100",
    ) else {
        return Vec::new();
    };
    stmt.query_map([bot_id], |row| {
        Ok(PurchasePublic {
            id: row.get(0)?,
            merchant: row.get(1)?,
            amount_usd: row.get(2)?,
            status: row.get(3)?,
            settled_amount_usd: row.get(4)?,
            created_at: row.get(5)?,
        })
    })
    .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
    .unwrap_or_default()
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PurchasingSummary {
    #[serde(flatten)]
    pub bot: BotPurchasing,
    pub spent_usd: f64,
    pub purchases: Vec<PurchasePublic>,
}

pub fn purchasing_summary(db: &Db, bot_id: &str) -> PurchasingSummary {
    let bot = get_bot_purchasing(db, bot_id);
    PurchasingSummary {
        spent_usd: monthly_spent_usd(db, bot_id, Utc::now()),
        purchases: list_purchases(db, bot_id),
        bot,
    }
}

pub fn purchase_specs_for_bot(db: &Db, bot_id: &str) -> Vec<ToolSpec> {
    let cfg = get_bot_purchasing(db, bot_id);
    if cfg.can_buy
        && cfg
            .monthly_limit_usd
            .is_some_and(|l| l.is_finite() && l > 0.0)
    {
        vec![purchase::spec()]
    } else {
        vec![]
    }
}

pub fn before_ask_purchase(db: &Db, bot_id: &str, args: &str) -> Decision {
    let input: serde_json::Value = if args.trim().is_empty() {
        json!({})
    } else {
        match serde_json::from_str(args) {
            Ok(v) => v,
            Err(_) => return Decision::Ask,
        }
    };
    let amount_usd = input.get("amount_usd").and_then(|v| v.as_f64());
    let Some(amount_usd) = amount_usd.filter(|a| a.is_finite() && *a > 0.0) else {
        return Decision::Ask;
    };
    let check = purchase_would_exceed_limit(db, bot_id, amount_usd, Utc::now());
    if check.exceeds {
        Decision::Allow
    } else {
        Decision::Ask
    }
}

pub fn verify_stripe_signature(
    secret: &str,
    raw_body: &str,
    header: Option<&str>,
    tolerance_seconds: u64,
    now: u64,
) -> bool {
    let presented = header.unwrap_or("");
    if presented.is_empty() {
        return false;
    }
    let mut timestamp = String::new();
    let mut v1 = String::new();
    for part in presented.split(',') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key.trim() {
            "t" => timestamp = value.trim().to_string(),
            "v1" => v1 = value.trim().to_string(),
            _ => {}
        }
    }
    if timestamp.is_empty() || v1.is_empty() {
        return false;
    }
    let Ok(as_number) = timestamp.parse::<u64>() else {
        return false;
    };
    if now.abs_diff(as_number) > tolerance_seconds {
        return false;
    }
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key size");
    mac.update(format!("{timestamp}.{raw_body}").as_bytes());
    let expected = hex::encode(mac.finalize().into_bytes());
    expected.as_bytes().ct_eq(v1.as_bytes()).into()
}

struct PurchaseRow {
    id: String,
    merchant: String,
    amount_usd: f64,
}

pub fn find_matching_pending_purchase(
    db: &Db,
    bot_id: &str,
    merchant_name: &str,
    amount_usd: f64,
) -> Option<String> {
    let Ok(mut stmt) = db.conn().prepare(
        "SELECT id, merchant, amount_usd FROM purchases
         WHERE bot_id = ?1 AND status = 'pending' ORDER BY created_at ASC",
    ) else {
        return None;
    };
    let lower_incoming = merchant_name.trim().to_lowercase();
    let rows: Vec<PurchaseRow> = stmt
        .query_map([bot_id], |row| {
            Ok(PurchaseRow {
                id: row.get(0)?,
                merchant: row.get(1)?,
                amount_usd: row.get(2)?,
            })
        })
        .and_then(|iter| iter.collect::<Result<Vec<_>, _>>())
        .unwrap_or_default();
    for row in rows {
        let lower_approved = row.merchant.trim().to_lowercase();
        let merchant_matches =
            !lower_approved.is_empty() && lower_incoming.contains(&lower_approved);
        let tolerance = row.amount_usd * 0.05;
        let amount_matches = (row.amount_usd - amount_usd).abs() <= tolerance;
        if merchant_matches && amount_matches {
            return Some(row.id);
        }
    }
    None
}

fn bot_id_for_card(db: &Db, card_id: &str) -> Option<String> {
    if card_id.is_empty() {
        return None;
    }
    db.conn()
        .query_row("SELECT id FROM bots WHERE card_id = ?1", [card_id], |row| {
            row.get(0)
        })
        .ok()
}

async fn handle_authorization_request(
    db: &Arc<Mutex<Db>>,
    secret_key: Option<&str>,
    authorization: &serde_json::Value,
    stripe: &dyn StripeHttp,
) -> bool {
    let auth_id = authorization
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let card_id = authorization
        .pointer("/card/id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let merchant_name = authorization
        .pointer("/merchant_data/name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let amount = authorization
        .get("amount")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let amount_usd = amount.abs() / 100.0;

    let bot_id = {
        let guard = db_guard(db);
        bot_id_for_card(&guard, card_id)
    };
    let Some(bot_id) = bot_id else {
        if let (Some(key), true) = (secret_key, !auth_id.is_empty()) {
            let _ = stripe
                .post_form(
                    key,
                    &format!("issuing/authorizations/{auth_id}/decline"),
                    &HashMap::new(),
                )
                .await;
        }
        return false;
    };

    let matched = {
        let guard = db_guard(db);
        find_matching_pending_purchase(&guard, &bot_id, merchant_name, amount_usd)
    };
    if let Some(purchase_id) = &matched {
        let _ = db_guard(db).conn().execute(
            "UPDATE purchases SET status = 'authorized', stripe_authorization_id = ?1 WHERE id = ?2",
            rusqlite::params![auth_id, purchase_id],
        );
    }
    if let Some(key) = secret_key.filter(|_| !auth_id.is_empty()) {
        let path = if matched.is_some() {
            format!("issuing/authorizations/{auth_id}/approve")
        } else {
            format!("issuing/authorizations/{auth_id}/decline")
        };
        let _ = stripe.post_form(key, &path, &HashMap::new()).await;
    }
    matched.is_some()
}

fn handle_transaction_created(db: &Db, transaction: &serde_json::Value) {
    let auth_id = transaction
        .get("authorization")
        .and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.get("id").and_then(|id| id.as_str()).map(str::to_string))
        })
        .unwrap_or_default();
    if auth_id.is_empty() {
        return;
    }
    let row_id: Option<String> = db
        .conn()
        .query_row(
            "SELECT id FROM purchases WHERE stripe_authorization_id = ?1",
            [&auth_id],
            |row| row.get(0),
        )
        .ok();
    let Some(row_id) = row_id else {
        return;
    };
    let amount = transaction
        .get("amount")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let amount_usd = amount.abs() / 100.0;
    let now = Utc::now().to_rfc3339();
    let _ = db.conn().execute(
        "UPDATE purchases SET status = 'settled', settled_amount_usd = ?1, settled_at = ?2 WHERE id = ?3",
        rusqlite::params![amount_usd, now, row_id],
    );
}

pub struct WebhookOutcome {
    pub handled: bool,
    pub approved: Option<bool>,
}

pub async fn handle_stripe_webhook(
    db: &Arc<Mutex<Db>>,
    secret_key: Option<&str>,
    payload: &serde_json::Value,
    stripe: &dyn StripeHttp,
) -> WebhookOutcome {
    let event_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let object = payload
        .pointer("/data/object")
        .cloned()
        .unwrap_or(json!({}));
    if event_type == "issuing_authorization.request" {
        let approved = handle_authorization_request(db, secret_key, &object, stripe).await;
        return WebhookOutcome {
            handled: true,
            approved: Some(approved),
        };
    }
    if event_type == "issuing_transaction.created" {
        handle_transaction_created(&db_guard(db), &object);
        return WebhookOutcome {
            handled: true,
            approved: None,
        };
    }
    WebhookOutcome {
        handled: false,
        approved: None,
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
