//! The axum app. `build_app` returns a `Router` without binding a port; every
//! test drives it through `tower::ServiceExt::oneshot`.

mod auth;
mod routes;

use axum::response::{IntoResponse, Response};
use axum::{http::StatusCode, Router};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use store::Db;
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
        let client_root = std::env::var("BULLPEN_CLIENT_ROOT")
            .unwrap_or_else(|_| "./dist/client".to_string());
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

/// A fallback handler for SPA routing: serves index.html for any non-/api path
/// that doesn't exist as a file. This allows the SPA to handle client-side
/// routing.
async fn spa_fallback(
    uri: axum::http::Uri,
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Response {
    // Never fall through to SPA for /api/* paths
    if uri.path().starts_with("/api") {
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    }

    let client_root = state.client_root.as_str();
    let requested_path = uri.path();

    // Try to serve the requested file first (if it's not /)
    if requested_path != "/" {
        let file_path = PathBuf::from(&client_root).join(requested_path.trim_start_matches('/'));
        if file_path.exists() && file_path.is_file() {
            if let Ok(content) = tokio::fs::read(&file_path).await {
                return (StatusCode::OK, content).into_response();
            }
        }
    }

    // Otherwise, serve index.html for SPA routing
    let index_path = PathBuf::from(&client_root).join("index.html");

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
        .fallback(spa_fallback)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
