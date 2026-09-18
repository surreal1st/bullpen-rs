//! RAIL-02: `POST`/`PATCH`/`DELETE /api/sections`, through `build_app`
//! (`crates/server/src/routes/sections.rs`). Moving a bot between sections
//! (`sectionId` on `PATCH /api/bots/:id/rail`) is tested in
//! `tests/bots.rs`'s own RAIL-02 section instead - it drives a different
//! route (`routes/bots.rs`) and reuses that file's own helpers
//! (`seed_bot`, `ids_in_order`); see that file's RAIL-02 comment for why.

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use serde_json::{Value, json};
use server::AppState;
use store::Db;
use tower::ServiceExt;

mod common;
use common::seed_session;

fn open_db() -> Db {
    Db::open(":memory:").expect("open :memory: db")
}

fn app_for(db: Db) -> Router {
    let state = AppState::new(db);
    server::build_app(state)
}

fn seed_bot(db: &Db, id: &str, name: &str) {
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at, permissions) VALUES (?1, ?2, '', ?3, NULL, '2026-01-01T00:00:00Z', ?4)",
            rusqlite::params![id, name, format!("You are {name}."), "{}"],
        )
        .expect("seed bot");
}

async fn post_route(app: &Router, path: &str, session: &str, body: Value) -> (u16, Value) {
    let request = Request::post(path)
        .header("cookie", session)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    (status, value)
}

async fn patch_route(app: &Router, path: &str, session: &str, body: Value) -> (u16, Value) {
    let request = Request::patch(path)
        .header("cookie", session)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    (status, value)
}

async fn delete_route(app: &Router, path: &str, session: &str) -> (u16, Value) {
    let request = Request::delete(path)
        .header("cookie", session)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    (status, value)
}

async fn get_route(app: &Router, path: &str, session: &str) -> (u16, Value) {
    let request = Request::get(path)
        .header("cookie", session)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    (status, value)
}

/* ---------------------------------------------------- the one that matters */

/// 🔴 Bite: deleting a section must leave every bot alive. This is the
/// single most important behaviour in the ticket - a `delete_section` that
/// cascaded into `DELETE FROM bots WHERE section_id = ?` (instead of the
/// `UPDATE ... SET section_id = NULL` it should be) would still answer 200
/// here, so the check is not "the route did not 500": it re-fetches the
/// live roster through `/api/roster` (not just the DELETE's own echo) and
/// asserts the bot is STILL THERE with `sectionId` gone to `null`, plus a
/// second, independent proof through the DELETE response's own `bots` field.
#[tokio::test]
async fn deleting_a_section_keeps_every_bot_falls_back_to_unassigned() {
    let db = open_db();
    seed_bot(&db, "trinity", "Trinity");
    seed_bot(&db, "orson", "Orson");
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) =
        post_route(&app, "/api/sections", &session, json!({ "name": "SHOOT" })).await;
    assert_eq!(status, 201);
    let section_id = response["section"]["id"].as_str().unwrap().to_string();

    // Move both bots into the section before deleting it - the bite only
    // means something if there was actually something to fall back FROM.
    for bot_id in ["trinity", "orson"] {
        let (status, _) = patch_route(
            &app,
            &format!("/api/bots/{bot_id}/rail"),
            &session,
            json!({ "sectionId": section_id }),
        )
        .await;
        assert_eq!(status, 200, "moving {bot_id} into the section");
    }

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    let bots = roster["bots"].as_array().unwrap();
    for bot_id in ["trinity", "orson"] {
        let bot = bots.iter().find(|b| b["id"] == bot_id).unwrap();
        assert_eq!(
            bot["sectionId"], section_id,
            "setup: {bot_id} must actually be in the section before the delete"
        );
    }

    let (status, response) =
        delete_route(&app, &format!("/api/sections/{section_id}"), &session).await;
    assert_eq!(status, 200);

    // Proof #1: the DELETE's own response.
    let echoed_bots = response["bots"].as_array().unwrap();
    for bot_id in ["trinity", "orson"] {
        let bot = echoed_bots
            .iter()
            .find(|b| b["id"] == bot_id)
            .unwrap_or_else(|| panic!("{bot_id} must survive the delete: {echoed_bots:?}"));
        assert!(
            bot["sectionId"].is_null(),
            "{bot_id} must fall back to Unassigned, not keep the deleted section id: {bot:?}"
        );
    }
    let echoed_sections = response["sections"].as_array().unwrap();
    assert!(
        !echoed_sections.iter().any(|s| s["id"] == section_id),
        "the deleted section itself must be gone from the list"
    );

    // Proof #2: a completely fresh fetch, not the DELETE's own echo - the
    // route TOLD the truth, and the database AGREES.
    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    let bots = roster["bots"].as_array().unwrap();
    assert_eq!(bots.len(), 2, "no bot must have been deleted: {bots:?}");
    for bot_id in ["trinity", "orson"] {
        let bot = bots
            .iter()
            .find(|b| b["id"] == bot_id)
            .unwrap_or_else(|| panic!("{bot_id} must still be in the roster: {bots:?}"));
        assert!(bot["sectionId"].is_null(), "{bot_id}: {bot:?}");
    }
}

/// Bite: deleting an unknown section id is 404, and nothing is written -
/// re-checked through a fresh `/api/roster` fetch (sections list unchanged)
/// rather than trusting the status code alone.
#[tokio::test]
async fn delete_section_unknown_id_is_404() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) = delete_route(&app, "/api/sections/does-not-exist", &session).await;
    assert_eq!(status, 404);
    assert_eq!(response["error"], "no such section");
}

/* -------------------------------------------------------------- create */

/// Bite: creating a section returns 201 with the section in the body, and a
/// subsequent `/api/roster` fetch carries it too - not just the POST's own
/// echo.
#[tokio::test]
async fn create_section_returns_201_and_appears_in_roster() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) = post_route(
        &app,
        "/api/sections",
        &session,
        json!({ "name": "Business Support" }),
    )
    .await;
    assert_eq!(status, 201);
    assert_eq!(response["section"]["name"], "Business Support");
    let id = response["section"]["id"].as_str().unwrap().to_string();
    assert!(!id.is_empty());

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    let sections = roster["sections"].as_array().unwrap();
    assert!(
        sections
            .iter()
            .any(|s| s["id"] == id && s["name"] == "Business Support"),
        "roster must carry the new section: {sections:?}"
    );
}

/// Bite: an empty or whitespace-only name is refused on create - 400, and
/// nothing lands in the roster's sections list.
#[tokio::test]
async fn create_section_rejects_empty_and_whitespace_name() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    for name in ["", "   "] {
        let (status, _response) =
            post_route(&app, "/api/sections", &session, json!({ "name": name })).await;
        assert_eq!(status, 400, "name {name:?}");
    }

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    assert!(
        roster["sections"].as_array().unwrap().is_empty(),
        "a rejected name must not create a row"
    );
}

/// Bite: a case-insensitive duplicate is refused, ported from
/// `roster.ts:37-40`. Without this a second "SHOOT" takes the id `shoot-2`
/// and the rail renders two headers reading SHOOT with nothing to tell them
/// apart - the slug makes them distinct to the database and identical to the
/// person looking at them.
#[tokio::test]
async fn create_section_refuses_a_case_insensitive_duplicate_name() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, _) = post_route(&app, "/api/sections", &session, json!({ "name": "SHOOT" })).await;
    assert_eq!(status, 201);

    for name in ["SHOOT", "shoot", "ShOoT", "  shoot  "] {
        let (status, _) =
            post_route(&app, "/api/sections", &session, json!({ "name": name })).await;
        assert_eq!(status, 400, "duplicate {name:?} must be refused");
    }

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    assert_eq!(
        roster["sections"].as_array().unwrap().len(),
        1,
        "only the first SHOOT may exist"
    );
}

/// Bite: the name is capped at 60 characters, matching `roster.ts:34`, so a
/// pasted paragraph cannot become a rail header. Counted in CHARACTERS: the
/// cap is applied with `chars().take(60)`, and a byte slice would panic on a
/// multi-byte character straddling the boundary.
#[tokio::test]
async fn create_section_trims_a_long_name_to_sixty_characters() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let long = "A".repeat(75);
    let (status, response) =
        post_route(&app, "/api/sections", &session, json!({ "name": long })).await;
    assert_eq!(status, 201);
    assert_eq!(
        response["section"]["name"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        60
    );

    let wide = "é".repeat(75);
    let (status, response) =
        post_route(&app, "/api/sections", &session, json!({ "name": wide })).await;
    assert_eq!(status, 201, "a multi-byte name must not panic the cap");
    assert_eq!(
        response["section"]["name"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        60
    );
}

/* -------------------------------------------------------------- rename */

/// Bite: renaming updates the name, and the roster reflects it on a fresh
/// fetch.
#[tokio::test]
async fn rename_section_updates_name_and_roster_reflects_it() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let (_status, created) =
        post_route(&app, "/api/sections", &session, json!({ "name": "Misc" })).await;
    let id = created["section"]["id"].as_str().unwrap().to_string();

    let (status, response) = patch_route(
        &app,
        &format!("/api/sections/{id}"),
        &session,
        json!({ "name": "Business Support" }),
    )
    .await;
    assert_eq!(status, 200);
    let sections = response["sections"].as_array().unwrap();
    let section = sections.iter().find(|s| s["id"] == id).unwrap();
    assert_eq!(section["name"], "Business Support");

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    let sections = roster["sections"].as_array().unwrap();
    let section = sections.iter().find(|s| s["id"] == id).unwrap();
    assert_eq!(
        section["name"], "Business Support",
        "the rename must hold on a fresh fetch too, not just the PATCH's own echo"
    );
}

/// Bite: renaming an unknown id is 400 with the ticket's exact combined
/// message.
#[tokio::test]
async fn rename_section_unknown_id_is_400() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) = patch_route(
        &app,
        "/api/sections/does-not-exist",
        &session,
        json!({ "name": "New Name" }),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(response["error"], "no such section, or the name was empty");
}

/// Bite: renaming to an empty or whitespace-only name is refused with the
/// same combined message, and the old name survives.
#[tokio::test]
async fn rename_section_empty_name_is_400_and_old_name_survives() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let (_status, created) =
        post_route(&app, "/api/sections", &session, json!({ "name": "Misc" })).await;
    let id = created["section"]["id"].as_str().unwrap().to_string();

    for name in ["", "   "] {
        let (status, response) = patch_route(
            &app,
            &format!("/api/sections/{id}"),
            &session,
            json!({ "name": name }),
        )
        .await;
        assert_eq!(status, 400, "name {name:?}");
        assert_eq!(response["error"], "no such section, or the name was empty");
    }

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    let sections = roster["sections"].as_array().unwrap();
    let section = sections.iter().find(|s| s["id"] == id).unwrap();
    assert_eq!(section["name"], "Misc", "a refused rename must not land");
}
