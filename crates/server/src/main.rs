use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use model::{EventStream, ModelEvent, ModelPort, ModelRequest};

/// S1-07b: `BULLPEN_FAKE_PORT=1` swaps the real OpenRouter port for one
/// that waits `DELAY` then replies - this environment has no OpenRouter
/// key, and the working bar (`crates/client/src/working_bar.rs`) needs a
/// REAL run in flight against THIS server to screenshot, since
/// `GET /api/conversations/:id/working` only exists here, not in
/// `scripts/mock-roster.mjs`. A local type rather than `model::fake`, whose
/// `FakePort` replays instantly with no delay and is `cfg(test)`-gated -
/// this touches only this one file, nothing in `crates/model`.
struct DelayedFakePort {
    reply: &'static str,
    delay: Duration,
}

impl ModelPort for DelayedFakePort {
    fn stream(&self, _request: ModelRequest) -> EventStream {
        let reply = self.reply;
        let delay = self.delay;
        Box::pin(async_stream::stream! {
            tokio::time::sleep(delay).await;
            yield ModelEvent::Delta { text: reply.to_string() };
            yield ModelEvent::Done {
                model: "fake/delayed".to_string(),
                usage: None,
                finish_reason: Some("stop".to_string()),
            };
        })
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let data_dir = std::env::var("BULLPEN_DATA_DIR").unwrap_or_else(|_| "./data".to_string());
    let port: u16 = std::env::var("BULLPEN_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(4380);

    std::fs::create_dir_all(&data_dir).expect("create data dir");
    let db_path = PathBuf::from(&data_dir).join("bullpen.db");
    let db = store::Db::open(db_path.to_str().expect("data dir path is valid utf-8"))
        .expect("open bullpen.db");

    // Says whether the dir is there, never anything from inside the db.
    tracing::info!(%data_dir, "bullpen data dir");

    let app = if std::env::var("BULLPEN_FAKE_PORT").as_deref() == Ok("1") {
        tracing::info!("BULLPEN_FAKE_PORT=1: model calls are scripted, no OpenRouter key needed");
        let fake_port: Arc<dyn ModelPort> = Arc::new(DelayedFakePort {
            reply: "Working on it - give me a moment.",
            delay: Duration::from_secs(4),
        });
        server::build_app(server::AppState::with_port(db, fake_port))
    } else {
        server::build_app(server::AppState::new(db))
    };

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind port");
    tracing::info!(%addr, "listening");
    axum::serve(listener, app).await.expect("serve");
}
