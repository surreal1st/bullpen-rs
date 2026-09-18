use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::StreamExt;
use model::secrets::KeySource;
use model::{
    ContentPart, ImageUrl, MessageContent, ModelEvent, ModelMessage, ModelPort, ModelRequest,
    OpenRouterPort,
};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_rustls::TlsAcceptor;

const ENDPOINT_PATH: &str = "/api/v1/chat/completions";

#[derive(Debug)]
struct PrefixRead {
    received: usize,
    content_length: usize,
}

#[derive(Debug)]
enum ServerReport {
    Served {
        received: usize,
    },
    ClosedAfterStall {
        received: usize,
        content_length: usize,
    },
    HandshakeRejected,
}

struct TlsFixture {
    endpoint: String,
    root_der: Vec<u8>,
    application_bytes: Arc<AtomicUsize>,
    prefix_ready: Option<oneshot::Receiver<PrefixRead>>,
    release: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<ServerReport>,
}

impl TlsFixture {
    async fn spawn(certificate_name: &str, stall_after_prefix: bool) -> Self {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec![certificate_name.to_string()]).unwrap();
        let root_der = cert.der().to_vec();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key)
        .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "https://127.0.0.1:{}{ENDPOINT_PATH}",
            listener.local_addr().unwrap().port()
        );
        let application_bytes = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&application_bytes);
        let (prefix_tx, prefix_ready) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();

        let task = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut stream = match acceptor.accept(socket).await {
                Ok(stream) => stream,
                Err(_) => return ServerReport::HandshakeRejected,
            };

            let mut request = Vec::new();
            let header_end = loop {
                let mut byte = [0u8; 1];
                let read = stream.read(&mut byte).await.unwrap_or(0);
                if read == 0 {
                    return ServerReport::Served {
                        received: observed.load(Ordering::SeqCst),
                    };
                }
                request.push(byte[0]);
                observed.fetch_add(1, Ordering::SeqCst);
                if request.ends_with(b"\r\n\r\n") {
                    break request.len();
                }
            };
            let headers = String::from_utf8_lossy(&request);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                })
                .unwrap();

            if stall_after_prefix {
                let mut byte = [0u8; 1];
                let read = stream.read(&mut byte).await.unwrap_or(0);
                assert_eq!(read, 1, "TLS fixture did not receive a body prefix");
                request.push(byte[0]);
                observed.fetch_add(1, Ordering::SeqCst);
                let received = request.len() - header_end;
                assert!(received > 0 && received < content_length);
                prefix_tx
                    .send(PrefixRead {
                        received,
                        content_length,
                    })
                    .ok();
                let _ = release_rx.await;

                let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
                loop {
                    let mut bytes = [0u8; 16 * 1024];
                    match tokio::time::timeout_at(deadline, stream.read(&mut bytes)).await {
                        Ok(Ok(0)) | Ok(Err(_)) => break,
                        Ok(Ok(read)) => {
                            observed.fetch_add(read, Ordering::SeqCst);
                            request.extend_from_slice(&bytes[..read]);
                        }
                        Err(_) => panic!("cancelled TLS client did not close its connection"),
                    }
                }
                return ServerReport::ClosedAfterStall {
                    received: request.len() - header_end,
                    content_length,
                };
            }

            while request.len() < header_end + content_length {
                let mut bytes = [0u8; 16 * 1024];
                let read = stream.read(&mut bytes).await.unwrap_or(0);
                if read == 0 {
                    break;
                }
                observed.fetch_add(read, Ordering::SeqCst);
                request.extend_from_slice(&bytes[..read]);
            }
            assert_eq!(request.len(), header_end + content_length);
            let body = concat!(
                "data: {\"model\":\"provider/actual\",\"choices\":[{\"delta\":{\"content\":\"tls ok\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
            ServerReport::Served {
                received: content_length,
            }
        });

        Self {
            endpoint,
            root_der,
            application_bytes,
            prefix_ready: stall_after_prefix.then_some(prefix_ready),
            release: stall_after_prefix.then_some(release),
            task,
        }
    }

    async fn finish(self) -> ServerReport {
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .expect("TLS fixture timed out")
            .expect("TLS fixture task panicked")
    }
}

fn image_request(image_bytes: usize) -> ModelRequest {
    ModelRequest {
        model: "vision/model".to_string(),
        messages: vec![ModelMessage {
            role: "user".to_string(),
            content: MessageContent::Parts(vec![ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: format!("data:image/png;base64,{}", "A".repeat(image_bytes)),
                },
            }]),
            tool_calls: None,
            tool_call_id: None,
        }],
        ..Default::default()
    }
}

fn tls_port(endpoint: String, root_der: Vec<u8>) -> OpenRouterPort {
    OpenRouterPort::with_local_https_endpoint(
        KeySource::Inline("fixture-key".to_string()),
        endpoint,
        root_der,
    )
    .unwrap()
}

#[tokio::test]
async fn local_https_image_request_uses_explicit_trust_root() {
    let fixture = TlsFixture::spawn("127.0.0.1", false).await;
    let port = tls_port(fixture.endpoint.clone(), fixture.root_der.clone());
    let events = tokio::time::timeout(
        Duration::from_secs(5),
        port.stream(image_request(16)).collect::<Vec<_>>(),
    )
    .await
    .expect("trusted TLS request timed out");

    assert!(
        events
            .iter()
            .any(|event| matches!(event, ModelEvent::Delta { text } if text == "tls ok"))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ModelEvent::Done { .. }))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ModelEvent::Error { .. }))
    );
    assert!(fixture.application_bytes.load(Ordering::SeqCst) > 0);
    match fixture.finish().await {
        ServerReport::Served { received } => assert!(received > 0),
        report => panic!("unexpected TLS fixture report: {report:?}"),
    }
}

#[tokio::test]
async fn local_https_text_request_uses_the_same_explicit_trust_root() {
    let fixture = TlsFixture::spawn("127.0.0.1", false).await;
    let port = tls_port(fixture.endpoint.clone(), fixture.root_der.clone());
    let events = tokio::time::timeout(
        Duration::from_secs(5),
        port.stream(ModelRequest {
            model: "vision/model".to_string(),
            messages: vec![ModelMessage::user("TLS warm-up")],
            ..Default::default()
        })
        .collect::<Vec<_>>(),
    )
    .await
    .expect("trusted TLS text request timed out");

    assert!(
        events
            .iter()
            .any(|event| matches!(event, ModelEvent::Delta { text } if text == "tls ok"))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ModelEvent::Error { .. }))
    );
    match fixture.finish().await {
        ServerReport::Served { received } => assert!(received > 0),
        report => panic!("unexpected TLS fixture report: {report:?}"),
    }
}

#[tokio::test]
async fn cancelling_owned_tls_request_closes_while_application_reader_is_stalled() {
    let mut fixture = TlsFixture::spawn("127.0.0.1", true).await;
    let port = tls_port(fixture.endpoint.clone(), fixture.root_der.clone());
    let request = image_request(4 * 1024 * 1024);
    let client = tokio::spawn(async move { port.stream(request).collect::<Vec<_>>().await });

    let prefix = tokio::time::timeout(Duration::from_secs(5), fixture.prefix_ready.take().unwrap())
        .await
        .expect("TLS application reader did not receive a body prefix")
        .expect("TLS prefix signal dropped");
    assert!(prefix.received > 0);
    assert!(prefix.received < prefix.content_length);

    client.abort();
    let cancelled = client
        .await
        .expect_err("TLS image request completed before cancellation");
    assert!(cancelled.is_cancelled());
    fixture.release.take().unwrap().send(()).ok();

    match fixture.finish().await {
        ServerReport::ClosedAfterStall {
            received,
            content_length,
        } => {
            assert!(received > 0);
            assert!(received <= content_length);
        }
        report => panic!("unexpected TLS fixture report: {report:?}"),
    }
}

#[tokio::test]
async fn untrusted_tls_certificate_fails_before_application_data() {
    let fixture = TlsFixture::spawn("127.0.0.1", false).await;
    let unrelated = generate_simple_self_signed(vec!["127.0.0.1".to_string()]).unwrap();
    let port = tls_port(fixture.endpoint.clone(), unrelated.cert.der().to_vec());
    let events = tokio::time::timeout(
        Duration::from_secs(5),
        port.stream(image_request(16)).collect::<Vec<_>>(),
    )
    .await
    .expect("untrusted TLS request timed out");

    assert!(
        events
            .iter()
            .any(|event| matches!(event, ModelEvent::Error { .. }))
    );
    assert_eq!(fixture.application_bytes.load(Ordering::SeqCst), 0);
    assert!(matches!(
        fixture.finish().await,
        ServerReport::HandshakeRejected
    ));
}

#[tokio::test]
async fn wrong_tls_hostname_fails_before_application_data() {
    let fixture = TlsFixture::spawn("localhost", false).await;
    let port = tls_port(fixture.endpoint.clone(), fixture.root_der.clone());
    let events = tokio::time::timeout(
        Duration::from_secs(5),
        port.stream(image_request(16)).collect::<Vec<_>>(),
    )
    .await
    .expect("wrong-hostname TLS request timed out");

    assert!(
        events
            .iter()
            .any(|event| matches!(event, ModelEvent::Error { .. }))
    );
    assert_eq!(fixture.application_bytes.load(Ordering::SeqCst), 0);
    assert!(matches!(
        fixture.finish().await,
        ServerReport::HandshakeRejected
    ));
}

#[test]
fn local_https_constructor_refuses_external_destination() {
    let root = generate_simple_self_signed(vec!["example.com".to_string()])
        .unwrap()
        .cert
        .der()
        .to_vec();
    assert!(
        OpenRouterPort::with_local_https_endpoint(
            KeySource::Inline("fixture-key".to_string()),
            "https://example.com/api/v1/chat/completions",
            root,
        )
        .is_err()
    );
}
