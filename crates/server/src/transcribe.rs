//! Push-to-talk transcription — port of `transcribe.ts`.

use std::time::Duration;

use model::{KeySource, ModelUsage, redact};

pub const DEFAULT_TRANSCRIBE_MODEL: &str = "google/gemini-2.5-flash-lite";
pub const MAX_AUDIO_BYTES: usize = 4 * 1024 * 1024;

const ENDPOINT: &str = "https://openrouter.ai/api/v1/chat/completions";

const INSTRUCTION: &str = "Transcribe exactly what was said. Output only the words, with normal punctuation. If there is no speech, output nothing.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    Webm,
    Wav,
    Mp3,
    Ogg,
}

impl AudioFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Webm => "webm",
            Self::Wav => "wav",
            Self::Mp3 => "mp3",
            Self::Ogg => "ogg",
        }
    }
}

pub fn format_for(mime_type: &str) -> Option<AudioFormat> {
    let base = mime_type.split(';').next()?.trim().to_lowercase();
    match base.as_str() {
        "audio/webm" => Some(AudioFormat::Webm),
        "audio/wav" | "audio/x-wav" | "audio/wave" => Some(AudioFormat::Wav),
        "audio/mpeg" | "audio/mp3" => Some(AudioFormat::Mp3),
        "audio/ogg" => Some(AudioFormat::Ogg),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct TranscribeOutcome {
    pub ok: bool,
    pub text: String,
    pub detail: String,
    pub usage: Option<ModelUsage>,
}

#[async_trait::async_trait]
pub trait TranscribeHttp: Send + Sync {
    async fn post_chat_completions(
        &self,
        body: &str,
        timeout: Duration,
    ) -> Result<(u16, String), String>;
}

pub struct ReqwestTranscribe {
    client: reqwest::Client,
    api_key: String,
}

#[async_trait::async_trait]
impl TranscribeHttp for ReqwestTranscribe {
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

pub struct TranscribeOptions<'a> {
    pub model: Option<&'a str>,
    pub api_key: Option<Option<&'a str>>,
    pub timeout_ms: u64,
    pub http: Option<&'a dyn TranscribeHttp>,
}

pub async fn transcribe(
    audio: &[u8],
    mime_type: &str,
    options: TranscribeOptions<'_>,
) -> TranscribeOutcome {
    if audio.is_empty() {
        return TranscribeOutcome {
            ok: false,
            text: String::new(),
            detail: "Nothing was recorded.".into(),
            usage: None,
        };
    }

    if audio.len() > MAX_AUDIO_BYTES {
        let mb = format!("{:.1}", audio.len() as f64 / 1024.0 / 1024.0);
        return TranscribeOutcome {
            ok: false,
            text: String::new(),
            detail: format!("That recording is {mb} MB. The limit is 4 MB."),
            usage: None,
        };
    }

    let Some(format) = format_for(mime_type) else {
        let label = if mime_type.trim().is_empty() {
            "unknown"
        } else {
            mime_type.trim()
        };
        return TranscribeOutcome {
            ok: false,
            text: String::new(),
            detail: format!("Unsupported audio type: {label}."),
            usage: None,
        };
    };

    let key = match options.api_key {
        None => KeySource::Env.resolve(),
        Some(None) => None,
        Some(Some(k)) => Some(k.to_string()),
    };
    let Some(key) = key.filter(|k| !k.is_empty()) else {
        return TranscribeOutcome {
            ok: false,
            text: String::new(),
            detail: redact("No OpenRouter key configured.", None),
            usage: None,
        };
    };
    let key_for_redact = key.clone();

    let model = options.model.unwrap_or(DEFAULT_TRANSCRIBE_MODEL);
    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD.encode(audio);

    let body = serde_json::json!({
        "model": model,
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "input_audio",
                    "input_audio": { "data": data, "format": format.as_str() }
                },
                { "type": "text", "text": INSTRUCTION }
            ]
        }],
        "stream": false,
        "max_tokens": 400,
        "usage": { "include": true }
    });
    let body_str = body.to_string();

    let timeout = Duration::from_millis(options.timeout_ms.max(1));
    let live = ReqwestTranscribe {
        client: reqwest::Client::new(),
        api_key: key,
    };

    let http_result = match options.http {
        Some(h) => h.post_chat_completions(&body_str, timeout).await,
        None => live.post_chat_completions(&body_str, timeout).await,
    };

    if let Err(err) = &http_result {
        return TranscribeOutcome {
            ok: false,
            text: String::new(),
            detail: redact(
                &format!("The transcription did not come back: {err}"),
                Some(key_for_redact.as_str()),
            ),
            usage: None,
        };
    }

    let (status, response_text) = http_result.expect("checked Err above");
    let body_json: serde_json::Value =
        serde_json::from_str(&response_text).unwrap_or(serde_json::json!({}));

    if status != 200 {
        let said = body_json
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("");
        let detail = if said.is_empty() {
            format!("OpenRouter returned {status}")
        } else {
            said.to_string()
        };
        return TranscribeOutcome {
            ok: false,
            text: String::new(),
            detail: redact(&detail, Some(key_for_redact.as_str())),
            usage: None,
        };
    }

    let text = body_json
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    let usage = body_json.get("usage").and_then(|u| {
        let cost = u.get("cost").and_then(|c| c.as_f64())?;
        Some(ModelUsage {
            cost_usd: cost,
            input_tokens: u.get("prompt_tokens").and_then(|t| t.as_u64()).unwrap_or(0) as u32,
            output_tokens: u
                .get("completion_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(0) as u32,
            cached_tokens: u
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(0) as u32,
            cost_known: true,
        })
    });

    TranscribeOutcome {
        ok: true,
        text,
        detail: format!("Transcribed with {model}."),
        usage,
    }
}
