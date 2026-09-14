use axum::http::StatusCode;
use std::fs;
use tempfile::TempDir;
use tower::ServiceExt;

#[tokio::test]
async fn root_serves_index_html() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let index_html = "<html><body>Test</body></html>";
    fs::write(temp_dir.path().join("index.html"), index_html)
        .expect("write index.html");

    let db_path = temp_dir.path().join("test.db");
    let db = store::Db::open(db_path.to_str().expect("path is valid utf-8"))
        .expect("open db");
    let client_root = temp_dir.path().to_string_lossy().to_string();
    let state = server::AppState::with_client_root(db, client_root);
    let app = server::build_app(state);

    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/")
                .body(axum::body::Body::empty())
                .expect("build request"),
        )
        .await
        .expect("send request");

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    assert_eq!(body, index_html);
}

#[tokio::test]
async fn spa_path_serves_index_html() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let index_html = "<html><body>SPA App</body></html>";
    fs::write(temp_dir.path().join("index.html"), index_html)
        .expect("write index.html");

    let db_path = temp_dir.path().join("test.db");
    let db = store::Db::open(db_path.to_str().expect("path is valid utf-8"))
        .expect("open db");
    let client_root = temp_dir.path().to_string_lossy().to_string();
    let state = server::AppState::with_client_root(db, client_root);
    let app = server::build_app(state);

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/some/spa/path")
                .body(axum::body::Body::empty())
                .expect("build request"),
        )
        .await
        .expect("send request");

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    assert_eq!(body, index_html);
}

#[tokio::test]
async fn api_not_found_returns_json() {
    let temp_dir = TempDir::new().expect("create temp dir");
    fs::write(temp_dir.path().join("index.html"), "<html></html>")
        .expect("write index.html");

    let db_path = temp_dir.path().join("test.db");
    let db = store::Db::open(db_path.to_str().expect("path is valid utf-8"))
        .expect("open db");
    let client_root = temp_dir.path().to_string_lossy().to_string();
    let state = server::AppState::with_client_root(db, client_root);
    let app = server::build_app(state);

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/nope")
                .body(axum::body::Body::empty())
                .expect("build request"),
        )
        .await
        .expect("send request");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let body_str = String::from_utf8(body.to_vec()).expect("body is valid utf-8");
    assert_eq!(body_str, "Not Found");
    assert!(!body_str.contains("<html>"));
}
