use std::future::Future;
use std::io::{self, Write};
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

use bytes::Bytes;
use futures::{StreamExt, future::poll_fn};
use http_body_util::Full;
use hyper::body::Body as _;
use hyper::client::conn::http1;
use hyper::{Request, Uri};
use hyper_rustls::HttpsConnectorBuilder;
use serde::Serialize;
use tokio::time::{Instant, timeout, timeout_at};
use tower_service::Service;

use crate::port::{
    BUSY_TRIES, EventStream, MAX_OUTPUT_TOKENS, ModelEvent, ModelRequest, ToolSpec, busy_wait_ms,
    parse_sse_stream_inner, provider_error,
};
use crate::secrets::{KeySource, redact};

const REQUEST_LIMIT: usize = 8 * 1024 * 1024;
pub(crate) const RESPONSE_FRAME_LIMIT: usize = 64 * 1024;
const ERROR_BODY_LIMIT: usize = 64 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(120);

struct CappedWriter {
    bytes: Vec<u8>,
}

impl CappedWriter {
    fn new() -> Self {
        Self { bytes: Vec::new() }
    }
}

impl Write for CappedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("image request exceeds 8 MiB"))?;
        if next > REQUEST_LIMIT {
            return Err(io::Error::other("image request exceeds 8 MiB"));
        }
        if next > self.bytes.capacity() {
            self.bytes
                .try_reserve_exact(next - self.bytes.len())
                .map_err(|_| io::Error::other("could not reserve image request buffer"))?;
        }
        if self.bytes.capacity() < next || self.bytes.capacity() > REQUEST_LIMIT {
            return Err(io::Error::other("image request exceeds 8 MiB"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Serialize)]
struct WireUsage {
    include: bool,
}

#[derive(Serialize)]
struct WireReasoning<'a> {
    effort: &'a str,
}

#[derive(Serialize)]
struct WireFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

#[derive(Serialize)]
struct WireTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunction<'a>,
}

#[derive(Serialize)]
struct WireEnvelope<'a> {
    model: &'a str,
    messages: &'a [crate::port::ModelMessage],
    max_tokens: u32,
    stream: bool,
    usage: WireUsage,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<WireTool<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<WireReasoning<'a>>,
}

fn wire_tools(tools: Option<&[ToolSpec]>) -> Option<Vec<WireTool<'_>>> {
    let tools = tools.filter(|tools| !tools.is_empty())?;
    Some(
        tools
            .iter()
            .map(|tool| WireTool {
                kind: "function",
                function: WireFunction {
                    name: &tool.name,
                    description: &tool.description,
                    parameters: &tool.parameters,
                },
            })
            .collect(),
    )
}

pub(crate) fn serialize(request: &ModelRequest, model: &str) -> Result<Bytes, String> {
    let envelope = WireEnvelope {
        model,
        messages: &request.messages,
        max_tokens: request.max_output_tokens.unwrap_or(MAX_OUTPUT_TOKENS),
        stream: true,
        usage: WireUsage { include: true },
        tools: wire_tools(request.tools.as_deref()),
        reasoning: request.reasoning.as_ref().map(|reasoning| WireReasoning {
            effort: &reasoning.effort,
        }),
    };
    let mut writer = CappedWriter::new();
    serde_json::to_writer(&mut writer, &envelope)
        .map_err(|_| "Could not serialize the screen observation request.".to_string())?;
    Ok(Bytes::from(writer.bytes))
}

fn validate_response_frame(data: Bytes) -> Result<Bytes, String> {
    if data.len() > RESPONSE_FRAME_LIMIT {
        return Err("Screen observation provider sent an oversized response frame.".to_string());
    }
    Ok(data)
}

fn raw_env_value_is_nonempty(value: Option<std::ffi::OsString>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

fn proxy_is_configured() -> bool {
    [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ]
    .iter()
    .any(|name| raw_env_value_is_nonempty(std::env::var_os(name)))
}

struct HeaderOutcome<T, C> {
    response: T,
    connection: Option<Pin<Box<C>>>,
    terminal: Option<Result<(), ()>>,
}

async fn race_response_and_connection<R, C, T, RE, CE>(
    mut response: Pin<Box<R>>,
    mut connection: Pin<Box<C>>,
) -> Result<HeaderOutcome<T, C>, ()>
where
    R: Future<Output = Result<T, RE>>,
    C: Future<Output = Result<(), CE>>,
{
    tokio::select! {
        biased;
        result = &mut response => result
            .map(|response| HeaderOutcome {
                response,
                connection: Some(connection),
                terminal: None,
            })
            .map_err(|_| ()),
        result = &mut connection => {
            let terminal = result.map_err(|_| ());
            drop(connection);
            response
                .await
                .map(|response| HeaderOutcome {
                    response,
                    connection: None,
                    terminal: Some(terminal),
                })
                .map_err(|_| ())
        }
    }
}

fn poll_connection_after_body_pending<C, CE>(
    cx: &mut std::task::Context<'_>,
    connection: &mut Option<Pin<Box<C>>>,
    connection_result: &mut Option<Result<(), ()>>,
    terminal: &mut bool,
) -> Poll<Option<Result<Bytes, String>>>
where
    C: Future<Output = Result<(), CE>>,
{
    if let Some(result) = connection_result.take() {
        *terminal = true;
        return match result {
            Ok(()) => Poll::Ready(None),
            Err(()) => Poll::Ready(Some(Err(
                "Screen observation connection failed.".to_string()
            ))),
        };
    }
    let connection_poll = connection
        .as_mut()
        .map_or(Poll::Pending, |connection| connection.as_mut().poll(cx));
    if let Poll::Ready(result) = connection_poll {
        drop(connection.take());
        *connection_result = Some(result.map_err(|_| ()));
        cx.waker().wake_by_ref();
    }
    Poll::Pending
}
pub(crate) fn stream(
    request: ModelRequest,
    key_source: KeySource,
    endpoint: String,
    allow_http: bool,
) -> EventStream {
    Box::pin(async_stream::stream! {
        if !allow_http && proxy_is_configured() {
            yield ModelEvent::Error {
                message: "Screen observation delivery does not support configured HTTP proxies.".to_string(),
                status: None,
            };
            return;
        }

        let uri: Uri = match endpoint.parse() {
            Ok(uri) => uri,
            Err(_) => {
                yield ModelEvent::Error { message: "Invalid screen observation endpoint.".to_string(), status: None };
                return;
            }
        };
        let scheme_allowed = uri.scheme_str() == Some("https")
            || (allow_http && uri.scheme_str() == Some("http"));
        if !scheme_allowed {
            yield ModelEvent::Error { message: "Screen observation endpoint must use HTTPS.".to_string(), status: None };
            return;
        }
        let host = match uri
            .authority()
            .and_then(|authority| hyper::header::HeaderValue::from_str(authority.as_str()).ok())
        {
            Some(host) => host,
            None => {
                yield ModelEvent::Error { message: "Invalid screen observation endpoint authority.".to_string(), status: None };
                return;
            }
        };
        let origin: Uri = match uri
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/")
            .parse()
        {
            Ok(origin) => origin,
            Err(_) => {
                yield ModelEvent::Error { message: "Invalid screen observation request target.".to_string(), status: None };
                return;
            }
        };

        let serialized = match serialize(&request, &request.model) {
            Ok(bytes) => bytes,
            Err(message) => {
                yield ModelEvent::Error { message, status: None };
                return;
            }
        };

        let mut connector = if allow_http {
            HttpsConnectorBuilder::new()
                .with_webpki_roots()
                .https_or_http()
                .enable_http1()
                .build()
        } else {
            HttpsConnectorBuilder::new()
                .with_webpki_roots()
                .https_only()
                .enable_http1()
                .build()
        };
        let mut last_busy = None;

        for attempt in 0..BUSY_TRIES {
            let key = match key_source.resolve() {
                Some(key) => key,
                None => {
                    yield ModelEvent::Error {
                        message: redact(
                            &format!(
                                "No OpenRouter key. Set {} to a file outside the repo, or {}.",
                                crate::secrets::KEY_FILE_VAR,
                                crate::secrets::KEY_VAR
                            ),
                            None,
                        ),
                        status: None,
                    };
                    return;
                }
            };
            let deadline = Instant::now() + ATTEMPT_TIMEOUT;
            let io = match timeout(CONNECT_TIMEOUT, async {
                poll_fn(|cx| connector.poll_ready(cx)).await?;
                connector.call(uri.clone()).await
            })
            .await
            {
                Ok(Ok(io)) => io,
                Ok(Err(_)) | Err(_) => {
                    yield ModelEvent::Error {
                        message: "Could not connect to the screen observation provider.".to_string(),
                        status: None,
                    };
                    return;
                }
            };

            let mut builder = http1::Builder::new();
            builder.read_buf_exact_size(Some(RESPONSE_FRAME_LIMIT));
            let (mut sender, connection) = match timeout_at(
                deadline,
                builder.handshake::<_, Full<Bytes>>(io),
            )
            .await
            {
                Ok(Ok(parts)) => parts,
                Ok(Err(_)) | Err(_) => {
                    yield ModelEvent::Error { message: "Could not establish the screen observation connection.".to_string(), status: None };
                    return;
                }
            };
            let authorization = match format!("Bearer {key}").parse::<hyper::header::HeaderValue>() {
                Ok(value) => value,
                Err(_) => {
                    yield ModelEvent::Error { message: "Invalid model authorization.".to_string(), status: None };
                    return;
                }
            };
            let outbound = match Request::post(origin.clone())
                .header(hyper::header::AUTHORIZATION, authorization)
                .header(hyper::header::HOST, host.clone())
                .header("HTTP-Referer", "https://rainmade.io")
                .header("X-Title", "Bullpen")
                .header(hyper::header::CONTENT_TYPE, "application/json")
                .header(hyper::header::CONTENT_LENGTH, serialized.len())
                .body(Full::new(serialized.clone()))
            {
                Ok(request) => request,
                Err(_) => {
                    yield ModelEvent::Error { message: "Could not build the screen observation request.".to_string(), status: None };
                    return;
                }
            };

            let outcome = {
                let response = Box::pin(sender.send_request(outbound));
                let connection = Box::pin(connection);
                match timeout_at(
                    deadline,
                    race_response_and_connection(response, connection),
                )
                .await
                {
                    Ok(Ok(outcome)) => outcome,
                    Ok(Err(())) | Err(_) => {
                        yield ModelEvent::Error { message: "Screen observation request failed before a response.".to_string(), status: None };
                        return;
                    }
                }
            };
            drop(sender);

            let status = outcome.response.status();
            let retry_after = outcome
                .response
                .headers()
                .get(hyper::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let mut incoming = outcome.response.into_body();
            let mut connection = outcome.connection;
            let mut connection_result = outcome.terminal;
            let mut terminal = false;
            let byte_stream = futures::stream::poll_fn(move |cx| {
                if terminal {
                    return Poll::Ready(None);
                }
                match Pin::new(&mut incoming).poll_frame(cx) {
                    Poll::Ready(Some(Ok(frame))) => {
                        if let Ok(data) = frame.into_data() {
                            let copied = validate_response_frame(data);
                            if copied.is_err() {
                                terminal = true;
                            }
                            return Poll::Ready(Some(copied));
                        }
                    }
                    Poll::Ready(Some(Err(_))) => {
                        terminal = true;
                        return Poll::Ready(Some(Err(
                            "Screen observation response failed.".to_string(),
                        )));
                    }
                    Poll::Ready(None) => {
                        terminal = true;
                        return Poll::Ready(None);
                    }
                    Poll::Pending => {}
                }
                poll_connection_after_body_pending(
                    cx,
                    &mut connection,
                    &mut connection_result,
                    &mut terminal,
                )
            });
            let mut byte_stream = Box::pin(byte_stream);

            if status.as_u16() == 429 {
                let mut body = Vec::new();
                loop {
                    let next = match timeout_at(deadline, byte_stream.next()).await {
                        Ok(next) => next,
                        Err(_) => {
                            yield ModelEvent::Error { message: "Screen observation request timed out.".to_string(), status: None };
                            return;
                        }
                    };
                    let Some(chunk) = next else { break; };
                    let chunk = match chunk {
                        Ok(chunk) => chunk,
                        Err(_) => break,
                    };
                    if body.len().saturating_add(chunk.len()) > ERROR_BODY_LIMIT {
                        yield ModelEvent::Error { message: "The model provider rejected the screen observation without exposing its payload.".to_string(), status: Some(429) };
                        return;
                    }
                    body.extend_from_slice(&chunk);
                }
                drop(byte_stream);
                let text = String::from_utf8_lossy(&body).into_owned();
                last_busy = Some(text.clone());
                if attempt + 1 < BUSY_TRIES {
                    let wait = busy_wait_ms(429, &text, retry_after.as_deref());
                    tokio::time::sleep(Duration::from_millis(wait)).await;
                    continue;
                }
                break;
            }

            if status.is_redirection() {
                drop(byte_stream);
                yield ModelEvent::Error {
                    message: "The screen observation provider returned a redirect, which is not followed.".to_string(),
                    status: Some(status.as_u16()),
                };
                return;
            }
            if !status.is_success() {
                let mut body = Vec::new();
                while let Ok(Some(Ok(chunk))) = timeout_at(deadline, byte_stream.next()).await {
                    if body.len().saturating_add(chunk.len()) > ERROR_BODY_LIMIT {
                        break;
                    }
                    body.extend_from_slice(&chunk);
                }
                drop(byte_stream);
                let message = provider_error(
                    &String::from_utf8_lossy(&body),
                    Some(&key),
                    true,
                );
                yield ModelEvent::Error { status: Some(status.as_u16()), message };
                return;
            }

            let mut events = parse_sse_stream_inner(
                byte_stream,
                request.model.clone(),
                Some(key),
                None,
                true,
                true,
            );
            loop {
                match timeout_at(deadline, events.next()).await {
                    Ok(Some(event)) => yield event,
                    Ok(None) => return,
                    Err(_) => {
                        yield ModelEvent::Error { message: "Screen observation request timed out.".to_string(), status: None };
                        return;
                    }
                }
            }
        }

        let upstream = last_busy.unwrap_or_else(|| "OpenRouter returned 429".to_string());
        let message = provider_error(&upstream, key_source.resolve().as_deref(), true);
        yield ModelEvent::Error { status: Some(429), message };
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port::{ContentPart, ImageUrl, MessageContent, ModelMessage};

    fn image_request(url: String) -> ModelRequest {
        ModelRequest {
            model: "vision/model".to_string(),
            messages: vec![ModelMessage {
                role: "user".to_string(),
                content: MessageContent::Parts(vec![ContentPart::ImageUrl {
                    image_url: ImageUrl { url },
                }]),
                tool_calls: None,
                tool_call_id: None,
            }],
            ..Default::default()
        }
    }

    struct QueuesHeadersThenCompletes {
        response: Option<tokio::sync::oneshot::Sender<Result<&'static str, &'static str>>>,
        dropped: Option<tokio::sync::oneshot::Sender<()>>,
        polled: bool,
    }

    impl Future for QueuesHeadersThenCompletes {
        type Output = Result<(), &'static str>;

        fn poll(mut self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
            assert!(!self.polled, "completed connection future was polled again");
            self.polled = true;
            self.response
                .take()
                .expect("response sender present")
                .send(Ok("queued headers"))
                .map_err(|_| "response receiver dropped")?;
            Poll::Ready(Ok(()))
        }
    }

    impl Drop for QueuesHeadersThenCompletes {
        fn drop(&mut self) {
            if let Some(dropped) = self.dropped.take() {
                let _ = dropped.send(());
            }
        }
    }

    #[tokio::test]
    async fn connection_first_header_race_preserves_terminal_without_repoll() {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let response = Box::pin(async move {
            let response = response_rx.await.map_err(|_| "response sender dropped")??;
            dropped_rx
                .await
                .map_err(|_| "connection drop acknowledgement missing")?;
            Ok::<_, &'static str>(response)
        });
        let connection = Box::pin(QueuesHeadersThenCompletes {
            response: Some(response_tx),
            dropped: Some(dropped_tx),
            polled: false,
        });

        let outcome = timeout(
            Duration::from_secs(1),
            race_response_and_connection(response, connection),
        )
        .await
        .expect("header race timed out")
        .expect("header race failed");
        assert_eq!(outcome.response, "queued headers");
        assert!(outcome.connection.is_none());
        assert!(matches!(outcome.terminal, Some(Ok(()))));

        let mut connection = outcome.connection;
        let mut connection_result = outcome.terminal;
        let mut terminal = false;
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(matches!(
            poll_connection_after_body_pending(
                &mut cx,
                &mut connection,
                &mut connection_result,
                &mut terminal,
            ),
            Poll::Ready(None)
        ));
        assert!(terminal);
        assert!(connection_result.is_none());
    }

    #[test]
    fn capped_writer_handles_existing_spare_capacity() {
        let mut bytes = Vec::with_capacity(6 * 1024 * 1024);
        bytes.resize(5 * 1024 * 1024, 0);
        let mut writer = CappedWriter { bytes };
        writer.write_all(&vec![0; 2 * 1024 * 1024]).unwrap();
        assert_eq!(writer.bytes.len(), 7 * 1024 * 1024);
        assert!(writer.bytes.capacity() <= REQUEST_LIMIT);
    }
    #[test]
    fn capped_writer_never_grows_capacity_past_eight_mib() {
        let mut writer = CappedWriter::new();
        writer.write_all(&vec![0; 5_243_080]).unwrap();
        assert_eq!(writer.bytes.len(), 5_243_080);
        assert!(writer.bytes.capacity() <= REQUEST_LIMIT);
        writer
            .write_all(&vec![0; REQUEST_LIMIT - writer.bytes.len()])
            .unwrap();
        assert_eq!(writer.bytes.len(), REQUEST_LIMIT);
        assert!(writer.bytes.capacity() <= REQUEST_LIMIT);
        assert!(writer.write_all(&[0]).is_err());
        assert_eq!(writer.bytes.len(), REQUEST_LIMIT);
        assert!(writer.bytes.capacity() <= REQUEST_LIMIT);
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_proxy_value_is_nonempty() {
        use std::os::unix::ffi::OsStringExt;

        assert!(raw_env_value_is_nonempty(Some(
            std::ffi::OsString::from_vec(vec![0xff]),
        )));
    }

    #[cfg(windows)]
    #[test]
    fn non_unicode_proxy_value_is_nonempty() {
        use std::os::windows::ffi::OsStringExt;

        assert!(raw_env_value_is_nonempty(Some(
            std::ffi::OsString::from_wide(&[0xd800]),
        )));
    }

    #[test]
    fn oversized_response_frame_is_refused_before_clone() {
        let frame = Bytes::from(vec![0; RESPONSE_FRAME_LIMIT + 1]);
        assert_eq!(frame.len(), RESPONSE_FRAME_LIMIT + 1);
        assert_eq!(
            validate_response_frame(frame).unwrap_err(),
            "Screen observation provider sent an oversized response frame."
        );
        let accepted = Bytes::from_static(b"bounded");
        let accepted_pointer = accepted.as_ptr();
        let accepted = validate_response_frame(accepted).unwrap();
        assert_eq!(accepted.as_ptr(), accepted_pointer);
    }

    #[tokio::test]
    async fn pending_line_limit_is_checked_segment_by_segment() {
        let mut chunks = vec![
            Ok(vec![b':'; 64 * 1024]),
            Ok(vec![b':'; 64 * 1024]),
            Ok(vec![b':'; 64 * 1024]),
            Ok(vec![b':'; 63 * 1024]),
        ];
        let mut boundary = vec![b':'; 512];
        boundary.push(b'\n');
        boundary.extend(std::iter::repeat_n(b':', 1024));
        chunks.push(Ok(boundary));
        chunks.push(Ok(b"\ndata: [DONE]\n".to_vec()));
        let mut events = parse_sse_stream_inner(
            futures::stream::iter(chunks),
            "vision/model".to_string(),
            None,
            None,
            true,
            true,
        );
        let events: Vec<_> = events.by_ref().collect().await;
        assert!(matches!(events.last(), Some(ModelEvent::Done { .. })));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ModelEvent::Error { .. }))
        );
    }

    #[test]
    fn borrowed_serializer_preserves_image_wire_shape() {
        let request = image_request("data:image/png;base64,AAAA".to_string());
        let actual: serde_json::Value =
            serde_json::from_slice(&serialize(&request, &request.model).unwrap()).unwrap();
        let expected = crate::port::build_body(&request, &request.model);
        assert_eq!(actual, expected);
    }

    #[test]
    fn borrowed_serializer_refuses_before_crossing_eight_mib() {
        let request = image_request(format!(
            "data:image/png;base64,{}",
            "A".repeat(REQUEST_LIMIT)
        ));
        assert_eq!(
            serialize(&request, &request.model).unwrap_err(),
            "Could not serialize the screen observation request."
        );
    }
}
