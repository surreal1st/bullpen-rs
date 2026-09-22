//! OpenRouter image generation — port of `imagegen.ts`.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use model::KeySource;
use store::{Attachment, Db, StoreAttachmentInput, store_attachment};

use crate::scope::scope_for_bot;

pub const ENDPOINT: &str = "https://openrouter.ai/api/v1/chat/completions";

pub const DEFAULT_IMAGE_MODEL: &str = "google/gemini-3.1-flash-lite-image";

const ALLOWED: [&str; 3] = [
    "google/gemini-3.1-flash-lite-image",
    "google/gemini-2.5-flash-image",
    "google/gemini-3.1-flash-image",
];

#[derive(Debug, Clone)]
pub struct ImageOutcome {
    pub ok: bool,
    pub attachment: Option<Attachment>,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AspectRatio {
    Square,
    Landscape,
    Portrait,
}

#[async_trait::async_trait]
pub trait ImageGenHttp: Send + Sync {
    async fn post_chat_completions(
        &self,
        body: &str,
        timeout: Duration,
    ) -> Result<(u16, String), String>;
}

pub struct ReqwestImageGen {
    client: reqwest::Client,
    api_key: String,
}

#[async_trait::async_trait]
impl ImageGenHttp for ReqwestImageGen {
    async fn post_chat_completions(
        &self,
        body: &str,
        timeout: Duration,
    ) -> Result<(u16, String), String> {
        let res = self
            .client
            .post(ENDPOINT)
            .header("authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .body(body.to_string())
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = res.status().as_u16();
        let text = res.text().await.map_err(|e| e.to_string())?;
        Ok((status, text))
    }
}

pub struct GenerateOptions<'a> {
    pub model: Option<&'a str>,
    /// `None` → read key from env/file; `Some(None)` → no key; `Some(Some(k))` → test key.
    pub api_key: Option<Option<&'a str>>,
    pub timeout_ms: u64,
    pub aspect_ratio: Option<AspectRatio>,
    pub references: Option<&'a [ReferenceImage]>,
    pub bot_id: Option<&'a str>,
    pub http: Option<&'a dyn ImageGenHttp>,
}

pub struct ReferenceImage {
    pub data_url: String,
}

pub fn decode_data_url(url: &str) -> Option<(String, Vec<u8>)> {
    let rest = url.strip_prefix("data:")?;
    let (header, payload) = rest.split_once(',')?;
    if !header.ends_with(";base64") {
        return None;
    }
    let content_type = header.strip_suffix(";base64")?.to_string();
    if !content_type.starts_with("image/") {
        return None;
    }
    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD
        .decode(payload.as_bytes())
        .ok()?;
    if data.is_empty() {
        return None;
    }
    Some((content_type, data))
}

pub fn data_url_for(bytes: &[u8], content_type: &str) -> String {
    use base64::Engine;
    format!(
        "data:{content_type};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

pub fn extension_for(content_type: &str) -> String {
    let subtype = content_type.split('/').nth(1).unwrap_or("png");
    let cleaned = subtype.split('+').next().unwrap_or("png");
    if cleaned == "jpeg" {
        "jpg".to_string()
    } else {
        let filtered: String = cleaned
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect();
        if filtered.is_empty() {
            "png".to_string()
        } else {
            filtered
        }
    }
}

fn lock_db(db: &Arc<Mutex<Db>>) -> std::sync::MutexGuard<'_, Db> {
    db.lock().unwrap_or_else(PoisonError::into_inner)
}

fn file_name_from_prompt(prompt: &str, content_type: &str) -> String {
    let stem: String = prompt
        .chars()
        .take(40)
        .filter(|c| c.is_ascii_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
        .collect();
    let stem = stem.trim();
    let base = if stem.is_empty() { "image" } else { stem };
    format!("{base}.{}", extension_for(content_type))
}

pub async fn generate_image(
    db: &Arc<Mutex<Db>>,
    data_dir: &str,
    prompt: &str,
    options: GenerateOptions<'_>,
) -> ImageOutcome {
    let clean = prompt.trim();
    if clean.is_empty() {
        return ImageOutcome {
            ok: false,
            attachment: None,
            detail: "Nothing was drawn: the description was empty.".into(),
        };
    }

    let key = match options.api_key {
        None => KeySource::Env.resolve(),
        Some(None) => None,
        Some(Some(k)) => Some(k.to_string()),
    };
    let Some(key) = key.filter(|k| !k.is_empty()) else {
        return ImageOutcome {
            ok: false,
            attachment: None,
            detail: "No OpenRouter key is configured, so nothing was drawn.".into(),
        };
    };

    let model = options
        .model
        .filter(|m| ALLOWED.contains(m))
        .unwrap_or(DEFAULT_IMAGE_MODEL);

    let aspect_prefix = match options.aspect_ratio {
        Some(AspectRatio::Landscape) => "Aspect ratio: 16:9. ",
        Some(AspectRatio::Portrait) => "Aspect ratio: 9:16. ",
        Some(AspectRatio::Square) => "Aspect ratio: 1:1. ",
        None => "",
    };

    let mut content_parts: Vec<serde_json::Value> = Vec::new();
    if let Some(refs) = options.references {
        for reference in refs {
            content_parts.push(serde_json::json!({
                "type": "image_url",
                "image_url": { "url": reference.data_url }
            }));
        }
    }
    content_parts.push(serde_json::json!({
        "type": "text",
        "text": format!("{aspect_prefix}{clean}"),
    }));

    let body = serde_json::json!({
        "model": model,
        "modalities": ["image", "text"],
        "messages": [{ "role": "user", "content": content_parts }],
    });
    let body_str = body.to_string();

    let timeout = Duration::from_millis(options.timeout_ms.max(1));
    let http_client = options.http;
    let live = ReqwestImageGen {
        client: reqwest::Client::new(),
        api_key: key,
    };

    let (status, response_text) = match http_client {
        Some(h) => h.post_chat_completions(&body_str, timeout).await,
        None => live.post_chat_completions(&body_str, timeout).await,
    }
    .unwrap_or_else(|err| {
        return ImageOutcome {
            ok: false,
            attachment: None,
            detail: format!("The image did not come back: {err}"),
        };
    });

    let body_json: serde_json::Value =
        serde_json::from_str(&response_text).unwrap_or(serde_json::json!({}));

    if status != 200 {
        let msg = body_json
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("");
        return ImageOutcome {
            ok: false,
            attachment: None,
            detail: format!("The image model answered {status}: {msg}")
                .trim()
                .to_string(),
        };
    }

    let url = body_json
        .pointer("/choices/0/message/images/0/image_url/url")
        .and_then(|v| v.as_str());

    let Some(url) = url else {
        let said = body_json
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let detail = if said.is_empty() {
            "The model returned no image and said nothing.".to_string()
        } else {
            format!(
                "The model answered with words instead of a picture: {}",
                said.chars().take(300).collect::<String>()
            )
        };
        return ImageOutcome {
            ok: false,
            attachment: None,
            detail,
        };
    };

    let Some((content_type, data)) = decode_data_url(url) else {
        return ImageOutcome {
            ok: false,
            attachment: None,
            detail: "The model returned something that was not an image.".into(),
        };
    };

    let name = file_name_from_prompt(clean, &content_type);
    let guard = lock_db(db);
    let user_id = options
        .bot_id
        .and_then(|bid| scope_for_bot(&guard, bid).ok())
        .map(|s| s.user_id);

    match store_attachment(
        &guard,
        data_dir,
        StoreAttachmentInput {
            name: &name,
            content_type: &content_type,
            data: &data,
            bot_id: options.bot_id,
            kind: None,
            user_id: user_id.as_deref(),
        },
    ) {
        Ok(attachment) => ImageOutcome {
            ok: true,
            attachment: Some(attachment),
            detail: format!("Drew it with {model}."),
        },
        Err(store::StoreAttachmentError::Empty) => ImageOutcome {
            ok: false,
            attachment: None,
            detail: "That file is empty.".into(),
        },
        Err(store::StoreAttachmentError::TooLarge { .. }) => ImageOutcome {
            ok: false,
            attachment: None,
            detail: "That file exceeds the attachment size limit.".into(),
        },
        Err(store::StoreAttachmentError::Store(_)) => ImageOutcome {
            ok: false,
            attachment: None,
            detail: "The picture could not be saved.".into(),
        },
    }
}

pub fn is_image_content_type(content_type: &str) -> bool {
    content_type.to_lowercase().starts_with("image/")
}
