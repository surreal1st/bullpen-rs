//! S12-03: imagegen helpers and generate_image.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use server::imagegen::{
    AspectRatio, DEFAULT_IMAGE_MODEL, GenerateOptions, ImageGenHttp, ReferenceImage,
    decode_data_url, extension_for, generate_image,
};

const PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

fn drew_json() -> String {
    serde_json::json!({
        "choices": [{
            "message": {
                "images": [{ "image_url": { "url": format!("data:image/png;base64,{PNG_BASE64}") } }]
            }
        }]
    })
    .to_string()
}

struct MockHttp {
    response: (u16, String),
    last_body: Arc<Mutex<String>>,
}

impl MockHttp {
    fn ok(body: String) -> Self {
        Self {
            response: (200, body),
            last_body: Arc::new(Mutex::new(String::new())),
        }
    }
}

#[async_trait::async_trait]
impl ImageGenHttp for MockHttp {
    async fn post_chat_completions(
        &self,
        body: &str,
        _timeout: Duration,
    ) -> Result<(u16, String), String> {
        *self.last_body.lock().unwrap() = body.to_string();
        Ok(self.response.clone())
    }
}

fn seed() -> (Arc<Mutex<store::Db>>, tempfile::TempDir) {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(Mutex::new(store::Db::open(":memory:").expect("open")));
    {
        let guard = db.lock().unwrap();
        store::ensure_library_tables(&guard).expect("library");
    }
    (db, temp)
}

#[tokio::test]
async fn stores_image_and_reports_model() {
    let (db, temp) = seed();
    let http = MockHttp::ok(drew_json());
    let outcome = generate_image(
        &db,
        temp.path().to_str().unwrap(),
        "a wrestling card graphic",
        GenerateOptions {
            model: None,
            api_key: Some(Some("test-key")),
            timeout_ms: 90_000,
            aspect_ratio: None,
            references: None,
            bot_id: None,
            http: Some(&http),
        },
    )
    .await;
    assert!(outcome.ok);
    assert_eq!(
        outcome.attachment.as_ref().unwrap().content_type,
        "image/png"
    );
    assert!(outcome.attachment.as_ref().unwrap().bytes > 0);
    assert!(outcome.detail.contains(DEFAULT_IMAGE_MODEL));
}

#[tokio::test]
async fn sends_image_modality() {
    let (db, temp) = seed();
    let http = MockHttp::ok(drew_json());
    generate_image(
        &db,
        temp.path().to_str().unwrap(),
        "x",
        GenerateOptions {
            model: None,
            api_key: Some(Some("k")),
            timeout_ms: 90_000,
            aspect_ratio: None,
            references: None,
            bot_id: None,
            http: Some(&http),
        },
    )
    .await;
    let sent: serde_json::Value = serde_json::from_str(&http.last_body.lock().unwrap()).unwrap();
    assert_eq!(
        sent.get("modalities").and_then(|v| v.as_array()).unwrap(),
        &[serde_json::json!("image"), serde_json::json!("text")]
    );
}

#[tokio::test]
async fn refuses_model_outside_allow_list() {
    let (db, temp) = seed();
    let http = MockHttp::ok(drew_json());
    generate_image(
        &db,
        temp.path().to_str().unwrap(),
        "x",
        GenerateOptions {
            model: Some("openai/gpt-5-image"),
            api_key: Some(Some("k")),
            timeout_ms: 90_000,
            aspect_ratio: None,
            references: None,
            bot_id: None,
            http: Some(&http),
        },
    )
    .await;
    let sent: serde_json::Value = serde_json::from_str(&http.last_body.lock().unwrap()).unwrap();
    assert_eq!(
        sent.get("model").and_then(|v| v.as_str()).unwrap(),
        DEFAULT_IMAGE_MODEL
    );
}

#[tokio::test]
async fn no_key_is_clear() {
    let (db, temp) = seed();
    let outcome = generate_image(
        &db,
        temp.path().to_str().unwrap(),
        "x",
        GenerateOptions {
            model: None,
            api_key: Some(None),
            timeout_ms: 90_000,
            aspect_ratio: None,
            references: None,
            bot_id: None,
            http: None,
        },
    )
    .await;
    assert!(!outcome.ok);
    assert!(outcome.detail.contains("No OpenRouter key"));
}

#[tokio::test]
async fn empty_prompt_does_not_call_out() {
    let (db, temp) = seed();
    let http = MockHttp::ok(drew_json());
    generate_image(
        &db,
        temp.path().to_str().unwrap(),
        "  ",
        GenerateOptions {
            model: None,
            api_key: Some(Some("k")),
            timeout_ms: 90_000,
            aspect_ratio: None,
            references: None,
            bot_id: None,
            http: Some(&http),
        },
    )
    .await;
    assert!(http.last_body.lock().unwrap().is_empty());
}

#[tokio::test]
async fn words_instead_of_picture() {
    let (db, temp) = seed();
    let body = serde_json::json!({
        "choices": [{ "message": { "content": "I would draw a ring." } }]
    })
    .to_string();
    let http = MockHttp::ok(body);
    let outcome = generate_image(
        &db,
        temp.path().to_str().unwrap(),
        "x",
        GenerateOptions {
            model: None,
            api_key: Some(Some("k")),
            timeout_ms: 90_000,
            aspect_ratio: None,
            references: None,
            bot_id: None,
            http: Some(&http),
        },
    )
    .await;
    assert!(!outcome.ok);
    assert!(outcome.detail.contains("words instead of a picture"));
}

#[tokio::test]
async fn provider_error_status() {
    let (db, temp) = seed();
    let body = serde_json::json!({ "error": { "message": "insufficient credit" } }).to_string();
    let http = MockHttp {
        response: (402, body),
        last_body: Arc::new(Mutex::new(String::new())),
    };
    let outcome = generate_image(
        &db,
        temp.path().to_str().unwrap(),
        "x",
        GenerateOptions {
            model: None,
            api_key: Some(Some("k")),
            timeout_ms: 90_000,
            aspect_ratio: None,
            references: None,
            bot_id: None,
            http: Some(&http),
        },
    )
    .await;
    assert!(!outcome.ok);
    assert!(outcome.detail.contains("402"));
    assert!(outcome.detail.contains("insufficient credit"));
}

#[test]
fn extension_for_content_types() {
    assert_eq!(extension_for("image/jpeg"), "jpg");
    assert_eq!(extension_for("image/png"), "png");
    assert_eq!(extension_for("image/webp"), "webp");
    assert_eq!(extension_for("nonsense"), "png");
    assert_eq!(extension_for(""), "png");
}

#[tokio::test]
async fn jpeg_name_uses_jpg() {
    let (db, temp) = seed();
    let body = serde_json::json!({
        "choices": [{
            "message": {
                "images": [{ "image_url": { "url": "data:image/jpeg;base64,/9j/4AAQSkZJRg==" } }]
            }
        }]
    })
    .to_string();
    let http = MockHttp::ok(body);
    let outcome = generate_image(
        &db,
        temp.path().to_str().unwrap(),
        "a belt",
        GenerateOptions {
            model: None,
            api_key: Some(Some("k")),
            timeout_ms: 90_000,
            aspect_ratio: None,
            references: None,
            bot_id: None,
            http: Some(&http),
        },
    )
    .await;
    assert!(outcome.attachment.as_ref().unwrap().name.ends_with(".jpg"));
}

#[test]
fn decode_data_url_round_trip() {
    let decoded = decode_data_url(&format!("data:image/png;base64,{PNG_BASE64}")).unwrap();
    assert_eq!(decoded.0, "image/png");
    assert!(!decoded.1.is_empty());
    assert!(decode_data_url("https://example.com/a.png").is_none());
    assert!(decode_data_url("data:text/html;base64,PGgxPmhpPC9oMT4=").is_none());
    assert!(decode_data_url("data:image/png;base64,").is_none());
}

#[tokio::test]
async fn landscape_aspect_prefix() {
    let (db, temp) = seed();
    let http = MockHttp::ok(drew_json());
    generate_image(
        &db,
        temp.path().to_str().unwrap(),
        "a wrestling card",
        GenerateOptions {
            model: None,
            api_key: Some(Some("k")),
            timeout_ms: 90_000,
            aspect_ratio: Some(AspectRatio::Landscape),
            references: None,
            bot_id: None,
            http: Some(&http),
        },
    )
    .await;
    let sent: serde_json::Value = serde_json::from_str(&http.last_body.lock().unwrap()).unwrap();
    let text = sent
        .pointer("/messages/0/content")
        .and_then(|c| c.as_array())
        .and_then(|parts| {
            parts
                .iter()
                .find(|p| p.get("type") == Some(&serde_json::json!("text")))
        })
        .and_then(|p| p.get("text"))
        .and_then(|t| t.as_str())
        .unwrap();
    assert!(text.contains("Aspect ratio: 16:9."));
}

#[tokio::test]
async fn reference_images_before_text() {
    let (db, temp) = seed();
    let http = MockHttp::ok(drew_json());
    let refs = [ReferenceImage {
        data_url: format!("data:image/png;base64,{PNG_BASE64}"),
    }];
    generate_image(
        &db,
        temp.path().to_str().unwrap(),
        "draw something like this",
        GenerateOptions {
            model: None,
            api_key: Some(Some("k")),
            timeout_ms: 90_000,
            aspect_ratio: None,
            references: Some(&refs),
            bot_id: None,
            http: Some(&http),
        },
    )
    .await;
    let sent: serde_json::Value = serde_json::from_str(&http.last_body.lock().unwrap()).unwrap();
    let content = sent
        .pointer("/messages/0/content")
        .and_then(|c| c.as_array())
        .unwrap();
    assert_eq!(
        content[0].get("type").and_then(|v| v.as_str()).unwrap(),
        "image_url"
    );
    assert_eq!(
        content[1].get("type").and_then(|v| v.as_str()).unwrap(),
        "text"
    );
}
