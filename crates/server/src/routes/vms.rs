//! S6-W-01: the VM's HTTP surface - what a bot's machine is, wake it, a
//! picture of it, and the desktop itself, proxied same-origin behind the
//! session gate. Plus two admin actions (`GET /api/vms`, `POST
//! /api/vms/hibernate`) TS never had a route for.
//!
//! Ported from TypeScript `vm-routes.ts` (`mountVmRoutes`/`vmView`) by path,
//! where TS has one: `GET /api/bots/:id/vm`, `POST /api/bots/:id/vm/ensure`,
//! `GET /api/bots/:id/vm/thumbnail.png`, `GET /api/bots/:id/vm/view`
//! (redirect) and `ALL /api/bots/:id/vm/view/*` (the HTTP half of the
//! desktop; `vm_proxy::serve` already routes the WebSocket half - see its
//! own module doc for why an upgrade never reaches an axum route at all).
//!
//! 🔴 Every route here is `/api/*`, gated by `auth::require_session`
//! (`lib.rs::build_app` wraps `routes::router()` in it) exactly like every
//! other route in this crate - nothing here is in `auth::OPEN_PATHS`. A VM
//! route reachable without a session would be a remote desktop open to
//! whoever has the URL.
//!
//! 🔴 `ensure_vm`/`refresh_vm`/`hibernate_idle` (`crate::vm`) cannot be
//! called from a route handler directly - see `vm.rs`'s "Send-safe route
//! wrappers" section doc for why (`std::sync::MutexGuard` is never `Send`,
//! and neither is a bare `&Db`, so holding either across the `docker.call()`
//! `.await` those functions need makes the whole handler future `!Send`,
//! which axum's `Handler` trait forbids). Every route below that touches
//! docker goes through `ensure_vm_in`/`refresh_vm_in`/`hibernate_idle_in`
//! instead, and every route gates on `state.vm_enabled` BEFORE reaching any
//! of them - `state.vm_docker` is `vm::DisabledDockerRun` when
//! `BULLPEN_VM` is off regardless (a second, redundant gate), so a route
//! that forgot its own check still could not reach a real container, but
//! nothing here relies on that alone.
//!
//! **No Docker on this workstation.** Every test in
//! `tests/vm_routes.rs` drives these routes against a recording fake
//! `vm::DockerRun`, never a real daemon - nothing here may claim a
//! container actually started; the only proof of that is the meridian smoke
//! test (S6-W-04).

use axum::body::Bytes;
use axum::extract::{Path, Request, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use serde_json::json;
use std::sync::OnceLock;
use std::time::Duration;

use crate::AppState;
use crate::vm::{self, RealFrameCapture};
use crate::vm_proxy;
use store::vms::{VmRow, get_vm, list_vms};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/vms", get(list_all))
        .route("/api/vms/hibernate", post(post_hibernate))
        .route("/api/bots/{id}/vm", get(get_status))
        .route("/api/bots/{id}/vm/ensure", post(post_ensure))
        .route("/api/bots/{id}/vm/thumbnail.png", get(get_thumbnail))
        .route("/api/bots/{id}/vm/view", get(view_redirect))
        .route("/api/bots/{id}/vm/view/{*rest}", any(view_proxy))
}

fn no_such_bot() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"error": "no such bot"}))).into_response()
}

const OFF_DETAIL: &str =
    "Per-bot machines are off on this server. Set BULLPEN_VM=on where they are wanted.";

/// Port of TS `encodeURIComponent`'s one job here: a path SEGMENT. Used only
/// for `viewPath`/the redirect target, both single segments (a bot id),
/// never a full path - `/` is deliberately NOT in the allowed set.
fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Port of TS `vmView` (`vm-routes.ts:53`).
fn vm_view_json(vm: Option<&VmRow>, available: bool, detail: &str) -> serde_json::Value {
    if !available {
        return json!({
            "available": false,
            "state": "unavailable",
            "container": null,
            "cdpPort": null,
            "webPort": null,
            "lastUsedAt": null,
            "detail": if detail.is_empty() { OFF_DETAIL } else { detail },
            "viewPath": null,
        });
    }
    match vm {
        None => json!({
            "available": true,
            "state": "none",
            "container": null,
            "cdpPort": null,
            "webPort": null,
            "lastUsedAt": null,
            "detail": if detail.is_empty() { "No machine yet." } else { detail },
            "viewPath": null,
        }),
        Some(v) => json!({
            "available": true,
            "state": v.state,
            "container": v.container,
            "cdpPort": v.cdp_port,
            "webPort": v.web_port,
            "lastUsedAt": v.last_used_at,
            "detail": detail,
            "viewPath": format!("/api/bots/{}/vm/view/", encode_path_segment(&v.bot_id)),
        }),
    }
}

fn vm_row_json(v: &VmRow) -> serde_json::Value {
    json!({
        "botId": v.bot_id,
        "container": v.container,
        "cdpPort": v.cdp_port,
        "webPort": v.web_port,
        "state": v.state,
        "lastUsedAt": v.last_used_at,
    })
}

/// `GET /api/vms` - every bot's machine, admin-level. Never touches docker
/// (`store::vms::list_vms` is a plain read) - gated on `vm_enabled` anyway,
/// same "off means nothing is exposed" posture `vm_view_json` already takes
/// for a single bot, so a disabled server's stored rows are never leaked
/// just because listing itself needs no docker call to answer.
async fn list_all(State(state): State<AppState>) -> Response {
    if !state.vm_enabled {
        return Json(json!({ "enabled": false, "vms": [] })).into_response();
    }
    let rows = {
        let db = state.db();
        list_vms(&db)
    };
    match rows {
        Ok(vms) => Json(json!({
            "enabled": true,
            "vms": vms.iter().map(vm_row_json).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => crate::AppError::from(e).into_response(),
    }
}

/// `POST /api/vms/hibernate` - the idle sweep, triggerable by hand. Same
/// "manual trigger" precedent `POST /api/goals/tick`/`POST /api/routines/tick`
/// already use for their own background schedulers.
async fn post_hibernate(State(state): State<AppState>) -> Response {
    if !state.vm_enabled {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": OFF_DETAIL })),
        )
            .into_response();
    }
    let db = state.db_handle();
    match vm::hibernate_idle_in(&db, state.vm_docker.clone(), &state.vm_config).await {
        Ok(stopped) => Json(json!({ "stopped": stopped })).into_response(),
        Err(e) => crate::AppError::from(e).into_response(),
    }
}

/// `GET /api/bots/:id/vm` - port of TS `app.get("/api/bots/:id/vm", ...)`
/// (`vm-routes.ts:99`). Settles a "starting" row against docker on read
/// (`refresh_vm_in`); any other state costs no docker call, so looking never
/// starts anything - matches `ensure_vm`'s own "looking is not using".
async fn get_status(State(state): State<AppState>, Path(bot_id): Path<String>) -> Response {
    let bot = {
        let db = state.db();
        store::get_bot(&db, &bot_id)
    };
    match bot {
        Err(e) => crate::AppError::from(e).into_response(),
        Ok(None) => no_such_bot(),
        Ok(Some(_)) => {
            if !state.vm_enabled {
                return Json(vm_view_json(None, false, "")).into_response();
            }
            let db = state.db_handle();
            match vm::refresh_vm_in(&db, state.vm_docker.clone(), &bot_id).await {
                Ok(vm) => Json(vm_view_json(vm.as_ref(), true, "")).into_response(),
                Err(e) => crate::AppError::from(e).into_response(),
            }
        }
    }
}

/// `POST /api/bots/:id/vm/ensure` - port of TS `app.post(".../vm/ensure",
/// ...)` (`vm-routes.ts:108`). Create it, or wake it - the one call the
/// card and every tool go through.
async fn post_ensure(State(state): State<AppState>, Path(bot_id): Path<String>) -> Response {
    let bot = {
        let db = state.db();
        store::get_bot(&db, &bot_id)
    };
    let bot = match bot {
        Err(e) => return crate::AppError::from(e).into_response(),
        Ok(None) => return no_such_bot(),
        Ok(Some(b)) => b,
    };
    if !state.vm_enabled {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(vm_view_json(None, false, "")),
        )
            .into_response();
    }
    let db = state.db_handle();
    let outcome = match vm::ensure_vm_in(
        &db,
        state.vm_docker.clone(),
        &bot_id,
        &bot.name,
        &state.vm_config,
    )
    .await
    {
        Ok(o) => o,
        Err(e) => return crate::AppError::from(e).into_response(),
    };
    let status = if outcome.ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(vm_view_json(outcome.vm.as_ref(), true, &outcome.detail)),
    )
        .into_response()
}

/// `GET /api/bots/:id/vm/thumbnail.png` - port of TS
/// (`vm-routes.ts:125`). 🔴 NEVER creates a machine (bite (a)): a bot with
/// no `vms` row - whether it never had one, or VMs are simply off - gets a
/// plain 404, not a silently provisioned container.
async fn get_thumbnail(State(state): State<AppState>, Path(bot_id): Path<String>) -> Response {
    let bot = {
        let db = state.db();
        store::get_bot(&db, &bot_id)
    };
    match bot {
        Err(e) => return crate::AppError::from(e).into_response(),
        Ok(None) => return no_such_bot(),
        Ok(Some(_)) => {}
    }

    let vm = if state.vm_enabled {
        let db = state.db();
        get_vm(&db, &bot_id).unwrap_or(None)
    } else {
        None
    };
    let Some(vm) = vm else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "no machine"}))).into_response();
    };
    if vm.state == "stopped" {
        return (StatusCode::CONFLICT, Json(json!({"error": "asleep"}))).into_response();
    }

    let now_ms = chrono::Utc::now().timestamp_millis();
    let capture = RealFrameCapture;
    match vm::thumbnail(&vm.container, &state.vm_config, &capture, now_ms).await {
        Some(png) => {
            let mut response = (StatusCode::OK, png).into_response();
            response
                .headers_mut()
                .insert(CONTENT_TYPE, HeaderValue::from_static("image/png"));
            // The server caches for five seconds (`vm::thumbnail`'s own TTL);
            // the browser must not cache at all, or a "live" thumbnail
            // freezes on the first frame it ever drew.
            response
                .headers_mut()
                .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "no frame"})),
        )
            .into_response(),
    }
}

/// `GET /api/bots/:id/vm/view` - port of TS's redirect
/// (`vm-routes.ts:146`). No trailing slash and selkies derives its
/// WebSocket URL from the directory part of the page path, so a client that
/// loaded the desktop from the wrong path gets a black screen that never
/// connects - see `vm_proxy::viewer_target`'s own doc.
async fn view_redirect(Path(bot_id): Path<String>) -> Redirect {
    Redirect::to(&format!(
        "/api/bots/{}/vm/view/",
        encode_path_segment(&bot_id)
    ))
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

/// The session cookie is dropped deliberately: it means nothing inside the
/// container, and forwarding it would write Josh's session token into the
/// container's own access log. Port of TS `stripHeaders` (`vm-routes.ts:196`).
fn strip_request_headers(source: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in source.iter() {
        if matches!(
            name.as_str(),
            "cookie" | "authorization" | "host" | "connection" | "upgrade" | "content-length"
        ) {
            continue;
        }
        out.insert(name.clone(), value.clone());
    }
    out
}

const MAX_PROXY_BODY_BYTES: usize = 16 * 1024 * 1024;

/// `ALL /api/bots/:id/vm/view/*` - the HTTP half of the desktop, proxied
/// same-origin behind the session (`vm_proxy::serve`/`attach_vm_proxy`
/// already carries the WebSocket half - a plain GET/POST here never reaches
/// an `Upgrade` request at all, see `vm_proxy`'s own module doc for why an
/// upgrade is intercepted before it ever reaches an axum route). Port of TS
/// `app.all(".../vm/view/*", ...)` (`vm-routes.ts:156`).
///
/// 🔴 NEVER creates a machine (bite (a), same as `get_thumbnail`): no `vms`
/// row is a plain 404.
async fn view_proxy(
    State(state): State<AppState>,
    Path((bot_id, rest)): Path<(String, String)>,
    req: Request,
) -> Response {
    let bot = {
        let db = state.db();
        store::get_bot(&db, &bot_id)
    };
    let bot = match bot {
        Err(e) => return crate::AppError::from(e).into_response(),
        Ok(None) => return no_such_bot(),
        Ok(Some(b)) => b,
    };

    let vm = if state.vm_enabled {
        let db = state.db();
        get_vm(&db, &bot_id).unwrap_or(None)
    } else {
        None
    };
    let Some(vm) = vm else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "no machine"}))).into_response();
    };

    // Someone watching the screen is someone using the machine, or the idle
    // sweep hibernates a desktop out from under them while they look at it.
    {
        let db = state.db();
        let _ = vm::touch_vm(&db, &bot_id);
    }

    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let upstream_url = format!("http://127.0.0.1:{}/{}{}", vm.web_port, rest, query);

    let method = req.method().clone();
    let headers = strip_request_headers(req.headers());
    let body_bytes = axum::body::to_bytes(req.into_body(), MAX_PROXY_BODY_BYTES)
        .await
        .unwrap_or_else(|_| Bytes::new());

    let mut builder = http_client()
        .request(method, &upstream_url)
        .timeout(Duration::from_secs(30));
    for (name, value) in headers.iter() {
        builder = builder.header(name, value);
    }
    if !body_bytes.is_empty() {
        builder = builder.body(body_bytes.to_vec());
    }

    match builder.send().await {
        Ok(upstream) => {
            let status = upstream.status();
            let proxied_headers = vm_proxy::proxy_response_headers(upstream.headers());
            let bytes = upstream.bytes().await.unwrap_or_else(|_| Bytes::new());
            let mut response = (
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                bytes,
            )
                .into_response();
            *response.headers_mut() = proxied_headers;
            response
        }
        // A sentence, not a stack: a machine that is booting refuses
        // connections for a good twenty seconds and that is an ordinary
        // state, not a fault.
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("{}'s machine is not answering yet.", bot.name),
        )
            .into_response(),
    }
}
