//! The axum app. `build_app` returns a `Router` without binding a port; every
//! test drives it through `tower::ServiceExt::oneshot`.

mod auth;
mod routes;

use axum::Router;
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
}

impl AppState {
    pub fn new(db: Db) -> Self {
        AppState {
            db: Arc::new(Mutex::new(db)),
        }
    }
}

/// Builds the router. No port binding here - `main.rs` reads
/// `BULLPEN_DATA_DIR` / `BULLPEN_PORT`, opens the db, and serves this.
pub fn build_app(state: AppState) -> Router {
    routes::router()
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
