//! The axum app. `build_app` returns a `Router` without binding a port; every
//! test drives it through `tower::ServiceExt::oneshot`.
