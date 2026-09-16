//! The wasm32 half of the transport seam ([`super::Transport`]) - today's
//! `gloo-net`/`web-sys` fetch code, moved out of `api.rs` verbatim by
//! S13a-01. Same `gloo_net::http::Request` builder, the same
//! `.credentials(web_sys::RequestCredentials::Include)` for `login`, the
//! same `ReadableStreamDefaultReader` loop for the SSE body
//! (`api.rs::send_message`, before this ticket) - only the address moved.

use super::{Method, RequestSpec, Transport, TransportResponse};
use gloo_net::http::{Request as GlooRequest, Response as GlooResponse};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::ReadableStreamDefaultReader;

/// S6-VM-01: `transport::open_view`'s web half - a new tab at `path`
/// (browser-resolved against the current origin, same as every other
/// relative URL this build already uses), so the click that opens a bot's
/// screen leaves this tab rather than trying to embed a second page's own
/// WebSocket-driven canvas inside this one's DOM. `.ok()`: nothing useful
/// to do with a blocked popup here beyond not opening the screen, the same
/// silent-degrade posture `api.rs::fetch_bot_tools` already takes for a
/// missing route.
pub fn open_view(path: &str) {
    if let Some(window) = web_sys::window() {
        let _ = window.open_with_url_and_target(path, "_blank");
    }
}

pub struct WebTransport;

impl Transport for WebTransport {
    async fn request(spec: RequestSpec) -> Result<TransportResponse, String> {
        let mut builder = match spec.method {
            Method::Get => GlooRequest::get(&spec.url),
            Method::Post => GlooRequest::post(&spec.url),
            Method::Put => GlooRequest::put(&spec.url),
            Method::Patch => GlooRequest::patch(&spec.url),
            Method::Delete => GlooRequest::delete(&spec.url),
        };
        if spec.with_credentials {
            builder = builder.credentials(web_sys::RequestCredentials::Include);
        }
        // Ported from gloo-net's own `RequestBuilder::json` (it does
        // exactly this: set the header, send the serialized text as the
        // body) - reused here as raw bytes since `transport::Request::json`
        // already serialized once in `mod.rs`, and re-serializing a second
        // time would be pointless.
        let request = match spec.body {
            Some(bytes) => {
                let text = String::from_utf8(bytes).map_err(|e| e.to_string())?;
                builder
                    .header("Content-Type", "application/json")
                    .body(text)
                    .map_err(|e| e.to_string())?
            }
            None => builder.build().map_err(|e| e.to_string())?,
        };
        let resp = request.send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        let content_type = resp.headers().get("content-type");
        Ok(TransportResponse {
            status,
            content_type,
            body: WebBody::new(&resp),
        })
    }
}

/// The old `send_message`'s reader loop (`api.rs`, before S13a-01),
/// unchanged: a `web_sys::ReadableStream` read one chunk at a time via
/// `wasm_bindgen_futures::JsFuture`. A response with no body (`resp.body()`
/// is `None` - e.g. a 204) reads as an immediately-empty stream rather than
/// an error, matching how nothing ever called `.body()` on those responses
/// before either.
pub struct WebBody {
    reader: Option<ReadableStreamDefaultReader>,
}

impl WebBody {
    fn new(resp: &GlooResponse) -> Self {
        let reader = resp
            .body()
            .map(|stream| stream.get_reader().unchecked_into());
        Self { reader }
    }

    pub async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, String> {
        let Some(reader) = &self.reader else {
            return Ok(None);
        };
        let result = JsFuture::from(reader.read())
            .await
            .map_err(|e| format!("{e:?}"))?;
        let done = js_sys::Reflect::get(&result, &JsValue::from_str("done"))
            .map_err(|e| format!("{e:?}"))?
            .as_bool()
            .unwrap_or(true);
        if done {
            return Ok(None);
        }
        let value = js_sys::Reflect::get(&result, &JsValue::from_str("value"))
            .map_err(|e| format!("{e:?}"))?;
        Ok(Some(js_sys::Uint8Array::new(&value).to_vec()))
    }
}
