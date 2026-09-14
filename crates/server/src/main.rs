use std::net::SocketAddr;
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let data_dir = std::env::var("BULLPEN_DATA_DIR").unwrap_or_else(|_| "./data".to_string());
    let port: u16 = std::env::var("BULLPEN_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(4370);

    std::fs::create_dir_all(&data_dir).expect("create data dir");
    let db_path = PathBuf::from(&data_dir).join("bullpen.db");
    let db = store::Db::open(db_path.to_str().expect("data dir path is valid utf-8"))
        .expect("open bullpen.db");

    // Says whether the dir is there, never anything from inside the db.
    tracing::info!(%data_dir, "bullpen data dir");

    let app = server::build_app(server::AppState::new(db));

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind port");
    tracing::info!(%addr, "listening");
    axum::serve(listener, app).await.expect("serve");
}
