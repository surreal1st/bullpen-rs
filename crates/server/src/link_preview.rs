//! Link previews for Twitch, YouTube, and OpenGraph — port of TS
//! `link-preview.ts`.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::Serialize;
use store::Db;
use url::Url;

use crate::egress::Resolver;
use crate::web_fetch::{FetchOptions, WebFetch, WebFetchPolicy, fetch_for_bot};

const TTL_LIVE_MS: i64 = 3 * 60 * 1000;
const TTL_STATIC_MS: i64 = 24 * 60 * 60 * 1000;

const TWITCH_CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";

pub const IMAGE_TYPES: &[&str] = &[
    "image/jpeg",
    "image/png",
    "image/webp",
    "image/gif",
    "image/avif",
];

pub const MAX_IMAGE_BYTES: usize = 3 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LinkPreview {
    pub url: String,
    pub provider: String,
    pub title: String,
    pub subtitle: String,
    pub image: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live: Option<bool>,
}

pub fn host(url: &Url) -> String {
    url.host_str()
        .unwrap_or("")
        .trim_start_matches("www.")
        .to_lowercase()
}

pub fn youtube_id(url: &Url) -> Option<String> {
    let h = host(url);
    if h == "youtu.be" {
        let id = url.path().trim_start_matches('/').split('/').next()?;
        return (!id.is_empty()).then(|| id.to_string());
    }
    if h != "youtube.com" && h != "m.youtube.com" {
        return None;
    }
    if url.path() == "/watch" {
        return url
            .query_pairs()
            .find(|(k, _)| k == "v")
            .map(|(_, v)| v.into_owned());
    }
    let path = url.path();
    for prefix in ["/shorts/", "/live/", "/embed/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            let id = rest.split('/').next().unwrap_or("");
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }
    None
}

pub fn twitch_login(url: &Url) -> Option<String> {
    if host(url) != "twitch.tv" {
        return None;
    }
    let first = url.path().split('/').find(|s| !s.is_empty())?;
    if ["videos", "directory", "settings", "downloads", "p", "store"].contains(&first) {
        return None;
    }
    if !(3..=25).contains(&first.len()) {
        return None;
    }
    if !first.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some(first.to_lowercase())
}

pub fn links_in(text: &str, limit: usize) -> Vec<String> {
    let re = regex::Regex::new(r#"https?://[^\s<>"')\]]+"#).expect("link regex");
    let mut found = std::collections::HashSet::new();
    for cap in re.captures_iter(text) {
        let m = cap.get(0).map(|m| m.as_str()).unwrap_or("");
        let cleaned = m.trim_end_matches(|c: char| ".,;:!?".contains(c));
        if let Ok(url) = Url::parse(cleaned)
            && (url.scheme() == "http" || url.scheme() == "https")
        {
            found.insert(url.to_string());
        }
        if found.len() >= limit {
            break;
        }
    }
    found.into_iter().collect()
}

fn cached(db: &Db, url: &str, now: i64) -> Option<LinkPreview> {
    let row: Option<(String, String, String, String, i64, String)> = db
        .conn()
        .query_row(
            "SELECT provider, title, subtitle, image, live, fetched_at FROM link_previews WHERE url = ?1",
            [url],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .ok();

    let (provider, title, subtitle, image, live, fetched_at) = row?;
    let fetched_ms = chrono::DateTime::parse_from_rfc3339(&fetched_at)
        .ok()
        .map(|dt| dt.timestamp_millis())?;
    let age = now - fetched_ms;
    let ttl = if provider == "Twitch" {
        TTL_LIVE_MS
    } else {
        TTL_STATIC_MS
    };
    if age > ttl {
        return None;
    }

    Some(LinkPreview {
        url: url.to_string(),
        provider,
        title,
        subtitle,
        image,
        live: Some(live == 1),
    })
}

fn remember(db: &Db, preview: &LinkPreview, now: i64) {
    let fetched_at = chrono::DateTime::from_timestamp_millis(now)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    let live = if preview.live == Some(true) { 1 } else { 0 };
    let _ = db.conn().execute(
        "INSERT INTO link_previews (url, provider, title, subtitle, image, live, fetched_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(url) DO UPDATE SET
           provider = excluded.provider, title = excluded.title, subtitle = excluded.subtitle,
           image = excluded.image, live = excluded.live, fetched_at = excluded.fetched_at",
        rusqlite::params![
            preview.url,
            preview.provider,
            preview.title,
            preview.subtitle,
            preview.image,
            live,
            fetched_at,
        ],
    );
}

async fn twitch_preview(login: &str, url: &str, fetch: &dyn WebFetch) -> Option<LinkPreview> {
    let body = serde_json::json!({
        "query": "query($login:String!){user(login:$login){displayName profileImageURL(width:300) description stream{title type viewersCount previewImageURL(width:640,height:360) game{name}}}}",
        "variables": { "login": login },
    });
    let res = fetch
        .post_json(
            "https://gql.twitch.tv/gql",
            &[
                ("Client-ID", TWITCH_CLIENT_ID),
                ("Content-Type", "application/json"),
            ],
            &body.to_string(),
            8000,
        )
        .await
        .ok()?;
    if !(200..300).contains(&res.status) {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_slice(&res.body).ok()?;
    let user = parsed.pointer("/data/user")?;
    if user.is_null() {
        return None;
    }
    let display = user
        .get("displayName")
        .and_then(|v| v.as_str())
        .unwrap_or(login);
    let stream = user.get("stream");
    let live = stream.and_then(|s| s.get("type")).and_then(|t| t.as_str()) == Some("live");
    let title = if live {
        stream
            .and_then(|s| s.get("title"))
            .and_then(|t| t.as_str())
            .unwrap_or(display)
            .to_string()
    } else {
        display.to_string()
    };
    let subtitle = if live {
        let game = stream
            .and_then(|s| s.pointer("/game/name"))
            .and_then(|g| g.as_str())
            .unwrap_or("live");
        let viewers = stream
            .and_then(|s| s.get("viewersCount"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        format!("{display} · {game} · {viewers} watching")
    } else {
        user.get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("Offline")
            .to_string()
    };
    let image = if live {
        stream
            .and_then(|s| s.get("previewImageURL"))
            .and_then(|i| i.as_str())
            .unwrap_or("")
    } else {
        user.get("profileImageURL")
            .and_then(|i| i.as_str())
            .unwrap_or("")
    };
    Some(LinkPreview {
        url: url.to_string(),
        provider: "Twitch".to_string(),
        title,
        subtitle,
        image: image.to_string(),
        live: Some(live),
    })
}

async fn youtube_preview(id: &str, url: &str, fetch: &dyn WebFetch) -> Option<LinkPreview> {
    let watch = format!("https://www.youtube.com/watch?v={id}");
    let encoded_watch: String = url::form_urlencoded::byte_serialize(watch.as_bytes()).collect();
    let endpoint = format!("https://www.youtube.com/oembed?url={encoded_watch}&format=json");
    let res = fetch.get(&endpoint, 8000).await.ok()?;
    if !(200..300).contains(&res.status) {
        return None;
    }
    let body: serde_json::Value = serde_json::from_slice(&res.body).ok()?;
    Some(LinkPreview {
        url: url.to_string(),
        provider: "YouTube".to_string(),
        title: body
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("YouTube")
            .to_string(),
        subtitle: body
            .get("author_name")
            .and_then(|a| a.as_str())
            .unwrap_or("")
            .to_string(),
        image: body
            .get("thumbnail_url")
            .and_then(|t| t.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg")),
        live: None,
    })
}

fn meta(html: &str, property: &str) -> String {
    let patterns = [
        format!(
            r#"(?i)<meta[^>]+(?:property|name)=["']{property}["'][^>]+content=["']([^"']*)["']"#
        ),
        format!(
            r#"(?i)<meta[^>]+content=["']([^"']*)["'][^>]+(?:property|name)=["']{property}["']"#
        ),
    ];
    for pat in patterns {
        if let Ok(re) = regex::Regex::new(&pat)
            && let Some(cap) = re.captures(html)
            && let Some(m) = cap.get(1)
        {
            return decode_entities(m.as_str());
        }
    }
    String::new()
}

fn decode_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#039;", "'")
        .replace("&apos;", "'")
}

async fn generic_preview(
    db: &Arc<Mutex<Db>>,
    url: &str,
    resolver: &dyn Resolver,
    fetch: &dyn WebFetch,
) -> Option<LinkPreview> {
    let policy = {
        let guard = db_guard(db);
        WebFetchPolicy::from_db(&guard)
    };
    let outcome = fetch_for_bot(
        &policy,
        url,
        resolver,
        fetch,
        FetchOptions {
            max_bytes: Some(200_000),
            timeout_ms: Some(15_000),
            binary: false,
            _marker: std::marker::PhantomData,
        },
    )
    .await;
    if outcome.error.is_some() || outcome.text.is_none() {
        return None;
    }
    let content_type = outcome.content_type.unwrap_or_default();
    if !content_type.contains("html") {
        return None;
    }
    let head = outcome.text.unwrap_or_default();
    let head = if head.len() > 200_000 {
        head[..200_000].to_string()
    } else {
        head
    };
    let title = meta(&head, "og:title");
    let title = if title.is_empty() {
        regex::Regex::new(r"(?i)<title[^>]*>([^<]*)</title>")
            .ok()
            .and_then(|re| re.captures(&head))
            .and_then(|c| c.get(1))
            .map(|m| decode_entities(m.as_str()))
            .unwrap_or_default()
    } else {
        title
    };
    let image = meta(&head, "og:image");
    if title.is_empty() && image.is_empty() {
        return None;
    }
    let provider = meta(&head, "og:site_name");
    let provider = if provider.is_empty() {
        Url::parse(url).ok().map(|u| host(&u)).unwrap_or_default()
    } else {
        provider
    };
    Some(LinkPreview {
        url: url.to_string(),
        provider,
        title: decode_entities(&title).trim().to_string(),
        subtitle: meta(&head, "og:description").chars().take(200).collect(),
        image,
        live: None,
    })
}

pub struct PreviewOptions<'a> {
    pub fetch: &'a dyn WebFetch,
    pub resolver: &'a dyn Resolver,
    pub now: Option<i64>,
}

fn db_guard(db: &Arc<Mutex<Db>>) -> MutexGuard<'_, Db> {
    db.lock().unwrap_or_else(PoisonError::into_inner)
}

pub async fn preview_for(
    db: &Arc<Mutex<Db>>,
    raw_url: &str,
    options: PreviewOptions<'_>,
) -> Option<LinkPreview> {
    let now = options
        .now
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
    let url = Url::parse(raw_url).ok()?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return None;
    }
    let key = url.to_string();
    if let Some(hit) = {
        let guard = db_guard(db);
        cached(&guard, &key, now)
    } {
        return Some(hit);
    }

    let preview = async {
        let login = twitch_login(&url);
        let video = youtube_id(&url);
        if let Some(login) = login {
            twitch_preview(&login, &key, options.fetch).await
        } else if let Some(id) = video {
            youtube_preview(&id, &key, options.fetch).await
        } else {
            generic_preview(db, &key, options.resolver, options.fetch).await
        }
    }
    .await;

    if let Some(ref p) = preview {
        let guard = db_guard(db);
        remember(&guard, p, now);
    }
    preview
}

pub fn image_allowed(raw: &str) -> bool {
    Url::parse(raw)
        .map(|u| u.scheme() == "https")
        .unwrap_or(false)
}
