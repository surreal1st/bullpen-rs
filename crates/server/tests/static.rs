use axum::http::StatusCode;
use std::fs;
use tempfile::TempDir;
use tower::ServiceExt;

#[tokio::test]
async fn root_serves_index_html() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let index_html = "<html><body>Test</body></html>";
    fs::write(temp_dir.path().join("index.html"), index_html).expect("write index.html");

    let db_path = temp_dir.path().join("test.db");
    let db = store::Db::open(db_path.to_str().expect("path is valid utf-8")).expect("open db");
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
    fs::write(temp_dir.path().join("index.html"), index_html).expect("write index.html");

    let db_path = temp_dir.path().join("test.db");
    let db = store::Db::open(db_path.to_str().expect("path is valid utf-8")).expect("open db");
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
    fs::write(temp_dir.path().join("index.html"), "<html></html>").expect("write index.html");

    let db_path = temp_dir.path().join("test.db");
    let db = store::Db::open(db_path.to_str().expect("path is valid utf-8")).expect("open db");
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

fn build_static_app(client_root: &std::path::Path) -> axum::Router {
    let db_path = client_root.join("test.db");
    let db = store::Db::open(db_path.to_str().expect("path is valid utf-8")).expect("open db");
    let state = server::AppState::with_client_root(db, client_root.to_string_lossy().to_string());
    server::build_app(state)
}

#[tokio::test]
async fn assets_are_served_with_real_mime_types() {
    let temp_dir = TempDir::new().expect("create temp dir");
    fs::write(temp_dir.path().join("index.html"), "<html></html>").expect("write index.html");
    let assets_dir = temp_dir.path().join("assets");
    fs::create_dir_all(&assets_dir).expect("create assets dir");
    fs::write(assets_dir.join("a.js"), "console.log(1);").expect("write a.js");
    fs::write(assets_dir.join("b.wasm"), [0u8, 1, 2, 3]).expect("write b.wasm");
    fs::write(assets_dir.join("c.css"), "body{}").expect("write c.css");

    let app = build_static_app(temp_dir.path());

    for (path, expected_prefix) in [
        ("/assets/a.js", "text/javascript"),
        ("/assets/b.wasm", "application/wasm"),
        ("/assets/c.css", "text/css"),
    ] {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::OK, "path: {path}");
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .expect("content-type header present")
            .to_str()
            .expect("content-type is valid utf-8");
        assert!(
            content_type.starts_with(expected_prefix)
                // .js may come back as either registered JS mime type.
                || (path.ends_with(".js") && content_type.starts_with("application/javascript")),
            "path {path}: expected content-type starting with {expected_prefix}, got {content_type}"
        );
    }
}

#[tokio::test]
async fn missing_asset_returns_404_not_html() {
    let temp_dir = TempDir::new().expect("create temp dir");
    fs::write(temp_dir.path().join("index.html"), "<html></html>").expect("write index.html");
    let assets_dir = temp_dir.path().join("assets");
    fs::create_dir_all(&assets_dir).expect("create assets dir");

    let app = build_static_app(temp_dir.path());

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/assets/missing.js")
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
    assert!(!body_str.contains("<html>"));
}
