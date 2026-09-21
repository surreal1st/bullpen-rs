//! Signed share tokens for bot export links. Port of `share.ts`.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use store::{Db, share as store_share};
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

const SHARE_EXPIRY_DAYS: i64 = 7;

pub fn share_secret() -> String {
    std::env::var("BULLPEN_SHARE_SECRET").unwrap_or_else(|_| "dev-share-secret".to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareToken {
    pub token: String,
    pub bot_id: String,
    pub expires_at: String,
}

fn sign_token(secret: &str, token_without_sig: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(token_without_sig.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

pub fn create_share_token(db: &Db, bot_id: &str) -> rusqlite::Result<ShareToken> {
    let secret = share_secret();
    let now = chrono::Utc::now();
    store_share::purge_expired(db, now)?;
    store_share::delete_for_bot(db, bot_id)?;

    let random_part = uuid::Uuid::new_v4().simple().to_string();
    let token_without_sig = format!("{bot_id}:{random_part}");
    let sig = sign_token(&secret, &token_without_sig);
    let token = format!("{token_without_sig}:{sig}");
    let expires_at = now + chrono::Duration::days(SHARE_EXPIRY_DAYS);

    store_share::insert_token(
        db,
        &uuid::Uuid::new_v4().to_string(),
        bot_id,
        &token,
        &now.to_rfc3339(),
        &expires_at.to_rfc3339(),
    )?;

    Ok(ShareToken {
        token,
        bot_id: bot_id.to_string(),
        expires_at: expires_at.to_rfc3339(),
    })
}

pub fn verify_share_token(db: &Db, token: &str) -> rusqlite::Result<Option<String>> {
    let secret = share_secret();
    let now = chrono::Utc::now();
    store_share::purge_expired(db, now)?;

    let Some((bot_id, stored)) = store_share::row_by_token(db, token)? else {
        return Ok(None);
    };

    let Some(prefix) = token.rsplit_once(':') else {
        return Ok(None);
    };
    let (token_without_sig, presented_sig) = prefix;
    let expected_sig = sign_token(&secret, token_without_sig);
    if !bool::from(expected_sig.as_bytes().ct_eq(presented_sig.as_bytes())) {
        return Ok(None);
    }
    if stored != token {
        return Ok(None);
    }
    Ok(Some(bot_id))
}

pub fn revoke_share_token(db: &Db, bot_id: &str) -> rusqlite::Result<bool> {
    store_share::delete_for_bot_if_any(db, bot_id)
}

pub fn get_share_token(db: &Db, bot_id: &str) -> rusqlite::Result<Option<ShareToken>> {
    let now = chrono::Utc::now();
    store_share::purge_expired(db, now)?;
    let Some((token, expires_at)) = store_share::row_by_bot(db, bot_id)? else {
        return Ok(None);
    };
    Ok(Some(ShareToken {
        token,
        bot_id: bot_id.to_string(),
        expires_at,
    }))
}
