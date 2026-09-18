//! RAIL-02: section CRUD - port of `projects/bullpen-night/src/server/
//! app.ts:2314-2332`'s three routes. `GET /api/roster` already returns
//! `store::list_sections` (see `routes/mod.rs::roster`), so creating,
//! renaming or deleting a section is visible there on the very next fetch
//! without this file touching that route at all - same reasoning
//! `routes/bots.rs`'s `patch_rail` gives for answering with the fresh
//! roster rather than a client having to guess when to re-fetch.
//!
//! Moving a bot between sections is NOT here - it is `sectionId` on the
//! existing `PATCH /api/bots/:id/rail` (`routes/bots.rs`), same as pin and
//! hide before it. See that file's RAIL-01 doc comment for why pin/hide/
//! move/avatar/shape all share one route rather than a fifth round trip for
//! one context menu, and its RAIL-02 doc comment for the `sectionId`
//! handling itself.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{patch, post};
use axum::{Json, Router};
use serde_json::json;

use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/sections", post(create_section))
        .route(
            "/api/sections/{id}",
            patch(rename_section).delete(delete_section),
        )
}

/// Reads the body the same forgiving way `create_bot`/`archive_bot`
/// (`routes/bots.rs`) do: a body that fails to parse at all, or is not a
/// JSON object, becomes `{}` rather than a 400 - matching the TS route's
/// own `c.req.json().catch(() => ({}))`. `{}` has no `name`, so a genuinely
/// malformed body still ends in the same refusal an explicitly empty name
/// gets, not a different error shape.
fn name_from_body(body: &axum::body::Bytes) -> String {
    let parsed: serde_json::Value = serde_json::from_slice(body).unwrap_or_else(|_| json!({}));
    parsed
        .as_object()
        .and_then(|obj| obj.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// RAIL-02: `POST /api/sections` - port of `app.ts:2314-2317`.
async fn create_section(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Response, crate::AppError> {
    let name = name_from_body(&body);
    let db = state.db();
    match store::create_section(&db, &name)? {
        Some(section) => {
            Ok((StatusCode::CREATED, Json(json!({ "section": section }))).into_response())
        }
        None => Err(crate::AppError::bad_request("Give the section a name.")),
    }
}

/// RAIL-02: `PATCH /api/sections/:id` - port of `app.ts:2319-2323`. `false`
/// from `store::rename_section` covers both "no such id" and "empty name" -
/// the same single check the TS route makes, and the same combined message
/// (matched exactly, per the ticket) it answers with rather than telling
/// the two apart.
async fn rename_section(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, crate::AppError> {
    let name = name_from_body(&body);
    let db = state.db();
    if !store::rename_section(&db, &id, &name)? {
        return Err(crate::AppError::bad_request(
            "no such section, or the name was empty",
        ));
    }
    let sections = store::list_sections(&db)?;
    Ok(Json(json!({ "sections": sections })).into_response())
}

/// RAIL-02: `DELETE /api/sections/:id` - port of `app.ts:2326-2331`. 🔴 The
/// bots that were in this section are NOT gone: `store::delete_section`
/// already moved every one of them to Unassigned before the row itself was
/// removed. This route's own job is just to hand back the two lists a
/// client needs to redraw the rail in one call - `sections` (this one
/// missing) and `bots` (all present, one more of them now `sectionId:
/// null`).
async fn delete_section(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, crate::AppError> {
    let db = state.db();
    if !store::delete_section(&db, &id)? {
        return Err(crate::AppError::not_found("no such section"));
    }
    let sections = store::list_sections(&db)?;
    let bots = store::list_roster(&db)?;
    Ok(Json(json!({ "sections": sections, "bots": bots })).into_response())
}
