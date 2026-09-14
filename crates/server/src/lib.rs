//! The axum app. `build_app` returns a `Router` without binding a port; every
//! test drives it through `tower::ServiceExt::oneshot`.

mod auth;
mod routes;

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::{Router, http::StatusCode};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use store::Db;
use tower::Service;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

/// Shared state handed to every route: one guarded connection to `bullpen.db`.
/// A `Mutex` (not a pool) because rusqlite's `Connection` is `Send` but not
/// `Sync` - one query runs at a time, same as the TS server's single
/// `better-sqlite3` handle.
#[derive(Clone)]
pub struct AppState {
    db: Arc<Mutex<Db>>,
    client_root: Arc<String>,
}

impl AppState {
    pub fn new(db: Db) -> Self {
        let client_root =
            std::env::var("BULLPEN_CLIENT_ROOT").unwrap_or_else(|_| "./dist/client".to_string());
        AppState {
            db: Arc::new(Mutex::new(db)),
            client_root: Arc::new(client_root),
        }
    }

    pub fn with_client_root(db: Db, client_root: String) -> Self {
        AppState {
            db: Arc::new(Mutex::new(db)),
            client_root: Arc::new(client_root),
        }
    }
}

/// Fallback for anything the API router didn't match: `/api/*` gets a plain
/// 404 (never SPA html); a path with a file extension (`/assets/a.js`,
/// `/favicon.ico`) is handed to `ServeDir`, which serves it with a real MIME
/// type via `mime_guess` and answers a missing file with a real 404; an
/// extensionless path (an SPA route like `/settings`) gets `index.html` so
/// the client can handle routing.
async fn static_or_spa(State(state): State<AppState>, req: Request) -> Response {
    let path = req.uri().path().to_string();

    // Never fall through to static files or SPA for /api/* paths.
    if path.starts_with("/api") {
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    }

    let has_extension = Path::new(&path).extension().is_some();
    if has_extension {
        let mut serve_dir = ServeDir::new(state.client_root.as_str());
        return serve_dir
            .call(req)
            .await
            .expect("ServeDir::call is infallible")
            .into_response();
    }

    // Extensionless path: serve index.html for SPA routing.
    let index_path = PathBuf::from(state.client_root.as_str()).join("index.html");

    match tokio::fs::read_to_string(&index_path).await {
        Ok(html) => axum::response::Html(html).into_response(),
        Err(e) => {
            tracing::error!("failed to read index.html from {:?}: {}", index_path, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Client bundle not found. Run `dx build` first, or set BULLPEN_CLIENT_ROOT.",
            )
                .into_response()
        }
    }
}

/// Builds the router. No port binding here - `main.rs` reads
/// `BULLPEN_DATA_DIR` / `BULLPEN_PORT`, opens the db, and serves this.
pub fn build_app(state: AppState) -> Router {
    // API routes first (via routes::router())
    // Then a fallback handler that serves static files and index.html for SPA routing
    Router::new()
        .merge(routes::router())
        .fallback(static_or_spa)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
