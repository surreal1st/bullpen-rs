//! S12-04: transcribe helpers and POST /api/transcribe.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use server::transcribe::{
    AudioFormat, DEFAULT_TRANSCRIBE_MODEL, MAX_AUDIO_BYTES, TranscribeHttp, TranscribeOptions,
    format_for, transcribe,
};
use server::{AppState, build_app};
use tower::ServiceExt;

const FAKE_KEY: &str = "sk-or-v1-test-key-for-transcribe-only";

struct MockHttp {
    response: (u16, String),
    last_body: Arc<Mutex<String>>,
}

#[async_trait::async_trait]
impl TranscribeHttp for MockHttp {
    async fn post_chat_completions(
        &self,
        body: &str,
        _timeout: Duration,
    ) -> Result<(u16, String), String> {
        *self.last_body.lock().unwrap() = body.to_string();
        Ok(self.response.clone())
    }
}

fn said_json() -> String {
    serde_json::json!({
        "choices": [{ "message": { "content": "hello from the mic" } }]
    })
    .to_string()
}

#[test]
fn format_for_maps_recorder_mimes() {
    assert_eq!(format_for("audio/webm"), Some(AudioFormat::Webm));
    assert_eq!(
        format_for("audio/webm;codecs=opus"),
        Some(AudioFormat::Webm)
    );
    assert_eq!(format_for("audio/ogg"), Some(AudioFormat::Ogg));
    assert_eq!(format_for("audio/wav"), Some(AudioFormat::Wav));
    assert_eq!(format_for("audio/mpeg"), Some(AudioFormat::Mp3));
    assert!(format_for("text/plain").is_none());
    assert!(format_for("video/mp4").is_none());
    assert!(format_for("").is_none());
}

#[tokio::test]
async fn sends_input_audio_and_instruction() {
    let http = MockHttp {
        response: (200, said_json()),
        last_body: Arc::new(Mutex::new(String::new())),
    };
    transcribe(
        b"fake audio bytes",
        "audio/webm;codecs=opus",
        TranscribeOptions {
            model: None,
            api_key: Some(Some(FAKE_KEY)),
            timeout_ms: 30_000,
            http: Some(&http),
        },
    )
    .await;
    let sent: serde_json::Value = serde_json::from_str(&http.last_body.lock().unwrap()).unwrap();
    assert_eq!(
        sent.get("model").and_then(|v| v.as_str()).unwrap(),
        DEFAULT_TRANSCRIBE_MODEL
    );
    let parts = sent
        .pointer("/messages/0/content")
        .and_then(|c| c.as_array())
        .unwrap();
    assert_eq!(parts[0]["type"], "input_audio");
    assert_eq!(parts[0]["input_audio"]["format"], "webm");
    assert_eq!(parts[1]["type"], "text");
}

#[tokio::test]
async fn happy_path_returns_text() {
    let http = MockHttp {
        response: (200, said_json()),
        last_body: Arc::new(Mutex::new(String::new())),
    };
    let outcome = transcribe(
        b"fake audio bytes",
        "audio/webm",
        TranscribeOptions {
            model: None,
            api_key: Some(Some(FAKE_KEY)),
            timeout_ms: 30_000,
            http: Some(&http),
        },
    )
    .await;
    assert!(outcome.ok);
    assert_eq!(outcome.text, "hello from the mic");
}

#[tokio::test]
async fn oversized_clip_never_calls_out() {
    let http = MockHttp {
        response: (200, said_json()),
        last_body: Arc::new(Mutex::new(String::new())),
    };
    let oversized = vec![0u8; MAX_AUDIO_BYTES + 1];
    let outcome = transcribe(
        &oversized,
        "audio/webm",
        TranscribeOptions {
            model: None,
            api_key: Some(Some(FAKE_KEY)),
            timeout_ms: 30_000,
            http: Some(&http),
        },
    )
    .await;
    assert!(!outcome.ok);
    assert!(outcome.detail.contains("4.0 MB"));
    assert!(http.last_body.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unsupported_mime_never_calls_out() {
    let http = MockHttp {
        response: (200, said_json()),
        last_body: Arc::new(Mutex::new(String::new())),
    };
    let outcome = transcribe(
        b"x",
        "video/mp4",
        TranscribeOptions {
            model: None,
            api_key: Some(Some(FAKE_KEY)),
            timeout_ms: 30_000,
            http: Some(&http),
        },
    )
    .await;
    assert!(!outcome.ok);
    assert!(outcome.detail.contains("Unsupported audio type"));
    assert!(http.last_body.lock().unwrap().is_empty());
}

#[tokio::test]
async fn redacts_upstream_error() {
    let body = serde_json::json!({ "error": { "message": format!("bad key: Bearer {FAKE_KEY}") } })
        .to_string();
    let http = MockHttp {
        response: (401, body),
        last_body: Arc::new(Mutex::new(String::new())),
    };
    let outcome = transcribe(
        b"x",
        "audio/webm",
        TranscribeOptions {
            model: None,
            api_key: Some(Some(FAKE_KEY)),
            timeout_ms: 30_000,
            http: Some(&http),
        },
    )
    .await;
    assert!(!outcome.ok);
    assert!(outcome.detail.contains("[redacted]"));
    assert!(!outcome.detail.contains(FAKE_KEY));
}

fn app_with_session() -> (axum::Router, String) {
    let db = store::Db::open(":memory:").expect("open");
    store::set_password(&db, "owner-password-long-enough").expect("password");
    let token = store::create_session(&db).expect("session");
    let state = AppState::new(db);
    (build_app(state), token)
}

#[tokio::test]
async fn post_returns_413_over_cap_without_model() {
    let (app, token) = app_with_session();
    let res = app
        .oneshot(
            Request::post("/api/transcribe")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "audio/webm")
                .body(Body::from(vec![0u8; MAX_AUDIO_BYTES + 1]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["limitBytes"], MAX_AUDIO_BYTES);
}

#[tokio::test]
async fn post_returns_415_for_non_audio() {
    let (app, token) = app_with_session();
    let res = app
        .oneshot(
            Request::post("/api/transcribe")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "text/plain")
                .body(Body::from("not audio"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}
