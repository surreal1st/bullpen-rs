//! IMPORT-01: `POST /api/import/open/preview` and `POST /api/import/open`.
//! Drives the real HTTP API through `build_app`, never internals - harness
//! copied out of `crates/server/tests/bots.rs` (`open_db`, `fixture_catalog`,
//! `app_with_catalog`, `app_for`, `seed_session` from `common`), same reason
//! that file's own EXPORT-01 block gives for reading a non-JSON body
//! directly rather than through a JSON-only helper.

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request};
use serde_json::{Value, json};
use server::AppState;
use std::sync::Arc;
use store::Db;
use tower::ServiceExt;

mod common;
use common::seed_session;

fn open_db() -> Db {
    Db::open(":memory:").expect("open :memory: db")
}

/// Same fixture the `judge_pin` catalogue tests in `tests/bots.rs` use -
/// anything that needs an ACCEPTED pin needs a catalogue that lists it, or
/// `judge_pin` refuses every model with "OpenRouter does not list this
/// model."
fn fixture_catalog() -> Arc<dyn model::catalog::Catalog> {
    let json = r#"[
        {
            "id": "anthropic/claude-sonnet-5",
            "name": "Claude Sonnet 5",
            "inPerM": 3.0,
            "outPerM": 15.0,
            "contextLength": 200000,
            "supportsTools": true,
            "supportsImages": true,
            "supportsReasoning": false,
            "providerCount": 2,
            "supportsCaching": true
        }
    ]"#;
    Arc::new(model::catalog::FixtureCatalog::from_json(json).expect("parse fixture"))
}

fn app_with_catalog(db: Db, catalog: Arc<dyn model::catalog::Catalog>) -> Router {
    let state = AppState::with_catalog(db, catalog);
    server::build_app(state)
}

fn app_for(db: Db) -> Router {
    let state = AppState::new(db);
    server::build_app(state)
}

async fn post_route(app: &Router, path: &str, session: &str, body: Value) -> (u16, Value) {
    let request = Request::post(path)
        .header("cookie", session)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    (status, value)
}

async fn get_route(app: &Router, path: &str, session: &str) -> (u16, Value) {
    let request = Request::get(path)
        .header("cookie", session)
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    (status, value)
}

/// Reads the raw text of a non-JSON response (the export route's body) -
/// same shape as `tests/bots.rs`'s own `export_route` helper.
async fn export_route(app: &Router, path: &str, session: &str) -> (u16, HeaderMap, String) {
    let request = Request::get(path)
        .header("cookie", session)
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).expect("export body must be valid utf-8");
    (status, headers, text)
}

/* --------------------------------------------------------------- round trip */

/// Bite: the whole point of this ticket. A bot whose name carries a quote
/// and whose purpose carries an embedded newline, pinned to a fixture model,
/// with a multi-paragraph instructions body - exported, then imported on a
/// SECOND fresh `:memory:` db with the same catalogue, and the round trip
/// holds field by field. Without divergence (a) (JSON-decoding a frontmatter
/// value), the quote would arrive with its backslash escapes still literally
/// in the name; without divergence (c) (honouring `model:`), the pin would
/// be silently dropped.
#[tokio::test]
async fn export_then_import_on_a_fresh_db_round_trips_name_purpose_instructions_and_model() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_with_catalog(db, fixture_catalog());

    let tricky_name = "Ada \"Q\" Lovelace";
    let tricky_purpose = "line one\nline two";
    let multi_paragraph_instructions = "First paragraph of instructions.\n\nSecond paragraph with more detail.\n\nThird paragraph.";

    let (status, created) = post_route(
        &app,
        "/api/bots",
        &session,
        json!({
            "name": tricky_name,
            "purpose": tricky_purpose,
            "instructions": multi_paragraph_instructions,
            "model": "anthropic/claude-sonnet-5",
        }),
    )
    .await;
    assert_eq!(status, 201);
    let bot_id = created["bot"]["id"].as_str().unwrap().to_string();

    let (export_status, _headers, exported_text) =
        export_route(&app, &format!("/api/bots/{bot_id}/export"), &session).await;
    assert_eq!(export_status, 200);

    // A second, completely independent app over a second fresh :memory: db,
    // same catalogue - proves this is a real cross-instance round trip, not
    // just re-reading the same row.
    let db2 = open_db();
    let session2 = seed_session(&db2);
    let app2 = app_with_catalog(db2, fixture_catalog());

    let (status, response) = post_route(
        &app2,
        "/api/import/open",
        &session2,
        json!({ "fileName": "export.md", "text": exported_text }),
    )
    .await;
    assert_eq!(status, 201, "import must succeed: {response:?}");
    assert_eq!(response["result"]["name"], tricky_name);

    let (_status, roster) = get_route(&app2, "/api/roster", &session2).await;
    let bots = roster["bots"].as_array().unwrap();
    let imported = bots
        .iter()
        .find(|b| b["name"] == tricky_name)
        .expect("imported bot must be in the roster");
    assert_eq!(imported["name"], tricky_name);
    assert_eq!(imported["purpose"], tricky_purpose);
    assert_eq!(imported["instructions"], multi_paragraph_instructions);
    assert_eq!(imported["model"], "anthropic/claude-sonnet-5");
}

/// Bite: `store::create_bot`'s own `slug_for` does NOT refuse a duplicate
/// name - it silently appends `-2` - so without the route's own duplicate
/// check, importing the same export twice would quietly create a second
/// bot instead of refusing. Checked against the roster afterward, not just
/// the 400: a mutation that dropped the check would still create a second
/// row even while somehow keeping the right status code.
#[tokio::test]
async fn importing_an_export_into_the_db_it_came_from_is_refused_as_a_duplicate() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, created) = post_route(
        &app,
        "/api/bots",
        &session,
        json!({ "name": "Trinity", "purpose": "Watches the error log", "instructions": "Be helpful." }),
    )
    .await;
    assert_eq!(status, 201);
    let bot_id = created["bot"]["id"].as_str().unwrap().to_string();

    let (_status, _headers, exported_text) =
        export_route(&app, &format!("/api/bots/{bot_id}/export"), &session).await;

    let (status, response) = post_route(
        &app,
        "/api/import/open",
        &session,
        json!({ "fileName": "trinity.md", "text": exported_text }),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(response["error"], "There is already a bot called Trinity.");

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    assert_eq!(
        roster["bots"].as_array().unwrap().len(),
        1,
        "the duplicate import must not have created a second row"
    );
}

/* -------------------------------------------------------------- detection */

/// Bite: `AGENTS.md` has no frontmatter at all - the whole file becomes the
/// instructions, the format is `"agents-md"`, and since nothing named a
/// `name:`, it is DERIVED from the filename and title-cased (divergence (b)
/// only skips title-casing for an EXPLICIT name; a derived one still gets
/// it, matching the TS exactly).
#[tokio::test]
async fn agents_md_has_no_frontmatter_and_derives_a_title_cased_name_from_the_filename() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let text = "You are a helpful assistant that watches deploys.\n\nBe concise in replies.";
    let (status, response) = post_route(
        &app,
        "/api/import/open",
        &session,
        json!({ "fileName": "agents.md", "text": text }),
    )
    .await;
    assert_eq!(status, 201, "{response:?}");
    assert_eq!(response["result"]["format"], "agents-md");
    assert_eq!(response["result"]["name"], "Agents");

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    let bots = roster["bots"].as_array().unwrap();
    let bot = bots.iter().find(|b| b["name"] == "Agents").unwrap();
    assert_eq!(bot["instructions"], text);
}

/// Bite: `SKILL.md` with `name` + `description` is format `"skill"`, and the
/// explicit `name` is kept VERBATIM (divergence (b)) - not title-cased,
/// unlike the derived-name case above.
#[tokio::test]
async fn skill_md_with_name_and_description_is_format_skill() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let text = "---\nname: deploy watcher\ndescription: watches every deploy\n---\n\nWatch every deploy and report issues.";
    let (status, response) = post_route(
        &app,
        "/api/import/open",
        &session,
        json!({ "fileName": "SKILL.md", "text": text }),
    )
    .await;
    assert_eq!(status, 201, "{response:?}");
    assert_eq!(response["result"]["format"], "skill");
    // Verbatim, not title-cased - "deploy watcher" must NOT become "Deploy Watcher".
    assert_eq!(response["result"]["name"], "deploy watcher");

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    let bots = roster["bots"].as_array().unwrap();
    let bot = bots.iter().find(|b| b["name"] == "deploy watcher").unwrap();
    assert_eq!(bot["purpose"], "watches every deploy");
}

/// IMPORT-01b: `detect_format` checks the BASENAME before it ever looks at
/// the frontmatter - a file literally named `AGENTS.md` is `"agents-md"`
/// even when its own frontmatter carries a `description` (and a `name`)
/// that would otherwise read as a skill. This guards the check ORDER, not
/// an individual branch: a coordinator mutation sweep moved the
/// frontmatter/`description` branch ABOVE the two basename checks, and
/// every existing test stayed green (`--lib` 152, `--test import_open` 13,
/// exit 0) because no prior case combined a recognised filename with real
/// frontmatter - this is that combination, and it is what the mutation
/// changes the answer for.
#[tokio::test]
async fn agents_md_filename_wins_over_frontmatter_with_a_description() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let text = "---\nname: Not A Skill\ndescription: this looks like a skill file, but the filename is AGENTS.md\n---\n\nInstructions here.";
    let (status, response) = post_route(
        &app,
        "/api/import/open/preview",
        &session,
        json!({ "fileName": "AGENTS.md", "text": text }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        response["preview"]["format"], "agents-md",
        "the AGENTS.md filename must win over frontmatter with a description: {response:?}"
    );
}

/// Mirror of the case above: `skill.md` with NO frontmatter at all is still
/// `"skill"` - the basename check must win over the `!front.had ->
/// "agents-md"` fallback too, not only over a frontmatter `description`
/// branch. Without the basename checks running first, a `skill.md` file
/// with no frontmatter block would fall through to `"agents-md"` instead.
#[tokio::test]
async fn skill_md_filename_wins_even_with_no_frontmatter_at_all() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let text = "Just plain instructions, no frontmatter block at all.";
    let (status, response) = post_route(
        &app,
        "/api/import/open/preview",
        &session,
        json!({ "fileName": "skill.md", "text": text }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        response["preview"]["format"], "skill",
        "the skill.md filename must win even with no frontmatter at all: {response:?}"
    );
}

/// Bite: the same frontmatter shape, but WITH `tools:`, is format
/// `"subagent"` - checked via preview (which carries `declaredTools` and
/// the warnings list; the create route's response envelope does not). The
/// filename here is deliberately NOT `skill.md`/`SKILL.md`: `detect_format`
/// checks the basename FIRST, before frontmatter, so a literal `skill.md`
/// filename would short-circuit to `"skill"` regardless of `tools:` - this
/// test exercises the frontmatter-driven branch the way a real
/// `.claude/agents/*.md` file would.
#[tokio::test]
async fn subagent_frontmatter_with_tools_reports_format_subagent_and_declared_tools() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let text = "---\nname: Deploy Watcher\ndescription: watches deploys\ntools: [browse, read_page]\n---\n\nWatch every deploy.";
    let (status, response) = post_route(
        &app,
        "/api/import/open/preview",
        &session,
        json!({ "fileName": ".claude/agents/deploy-watcher.md", "text": text }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["preview"]["format"], "subagent");
    assert_eq!(
        response["preview"]["declaredTools"],
        json!(["browse", "read_page"])
    );
    let warnings = response["preview"]["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap_or("").contains("browse, read_page")),
        "the tools warning must name the declared tools: {warnings:?}"
    );
}

/// Bite: frontmatter present, but neither `description` nor a recognised
/// filename - format `"unknown"`, with the not-recognised warning present.
#[tokio::test]
async fn frontmatter_with_no_description_and_an_unrecognised_filename_is_unknown() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let text = "---\ntools: [browse]\n---\n\nSome instructions with no description at all.";
    let (status, response) = post_route(
        &app,
        "/api/import/open/preview",
        &session,
        json!({ "fileName": "random-thing.txt", "text": text }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["preview"]["format"], "unknown");
    let warnings = response["preview"]["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|w| w
            .as_str()
            .unwrap_or("")
            .contains("the format was not recognised")),
        "{warnings:?}"
    );
}

/// IMPORT-01a/F1: a `fileName` with no `.md`-derivable `name:` in its
/// frontmatter falls to `file_name_to_name`, which used to slice a `str` at
/// `len - 3` before proving those were the last three bytes of `.md`. A
/// filename ending in a 4-byte UTF-8 character (an emoji) lands that offset
/// INSIDE the character, which panics rather than answering 400 - reachable
/// straight from an untrusted request body, since nothing about this
/// filename is otherwise malformed. Bite: this must be a graceful 201/400,
/// never a panic that kills the connection.
#[tokio::test]
async fn a_filename_ending_in_a_multibyte_character_does_not_panic() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, response) = post_route(
        &app,
        "/api/import/open",
        &session,
        json!({ "fileName": "bot\u{1F600}", "text": "hello" }),
    )
    .await;
    assert_eq!(status, 201, "{response:?}");
}

/* ---------------------------------------------------------------- refusals */

/// Bite: empty (or whitespace-only, once trimmed) instructions is a 400 with
/// the exact ticket-specified message, and nothing is created.
#[tokio::test]
async fn empty_instructions_is_400_and_nothing_created() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let text = "---\nname: Empty Bot\ndescription: nothing here\n---\n\n   \n\n";
    let (status, response) = post_route(
        &app,
        "/api/import/open",
        &session,
        json!({ "fileName": "empty.md", "text": text }),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(response["error"], "There are no instructions in that file.");
    assert!(response["warnings"].is_array());

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    assert!(roster["bots"].as_array().unwrap().is_empty());
}

/// Bite: a `model:` the catalogue does not list refuses the WHOLE request -
/// checked against the roster (empty), not just the status code, so a
/// mutation that judged the pin AFTER the insert cannot pass.
#[tokio::test]
async fn unknown_model_pin_is_400_and_nothing_created() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_with_catalog(db, fixture_catalog());

    let text = "---\nname: Bad Model Bot\ndescription: x\nmodel: unknown/does-not-exist\n---\n\nDo the thing.";
    let (status, response) = post_route(
        &app,
        "/api/import/open",
        &session,
        json!({ "fileName": "bad-model.md", "text": text }),
    )
    .await;
    assert_eq!(status, 400);
    assert!(
        response["error"]
            .as_str()
            .unwrap_or("")
            .contains("does not list this model")
    );

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    assert!(
        roster["bots"].as_array().unwrap().is_empty(),
        "a refused pin must not create the row at all"
    );
}

/// Bite: preview parses and returns, but creates nothing - the roster stays
/// empty afterward.
#[tokio::test]
async fn preview_creates_nothing() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let text = "---\nname: Preview Only\ndescription: just previewing\n---\n\nSome instructions.";
    let (status, response) = post_route(
        &app,
        "/api/import/open/preview",
        &session,
        json!({ "fileName": "preview.md", "text": text }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["preview"]["name"], "Preview Only");

    let (_status, roster) = get_route(&app, "/api/roster", &session).await;
    assert!(
        roster["bots"].as_array().unwrap().is_empty(),
        "a preview must never create a row"
    );
}

/// Bite: blank `text` (missing key, or present but whitespace-only) is a
/// flat 400 "no file content was sent" on BOTH routes, checked before
/// anything else runs.
#[tokio::test]
async fn blank_text_is_400_on_both_routes() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    for path in ["/api/import/open/preview", "/api/import/open"] {
        let (status, response) = post_route(
            &app,
            path,
            &session,
            json!({ "fileName": "x.md", "text": "   " }),
        )
        .await;
        assert_eq!(status, 400, "path={path}");
        assert_eq!(response["error"], "no file content was sent", "path={path}");

        // A body with no `text` key at all must answer the same way.
        let (status, response) =
            post_route(&app, path, &session, json!({ "fileName": "x.md" })).await;
        assert_eq!(status, 400, "path={path} (missing text key)");
        assert_eq!(response["error"], "no file content was sent", "path={path}");
    }
}

/// Bite: a name that survives slugging as nothing (punctuation only) still
/// imports - `store::slug_base` falls back to `"bot"`, matching
/// `create_bot_name_with_no_alnum_becomes_bot` in `tests/bots.rs` for the
/// ordinary create route.
#[tokio::test]
async fn a_punctuation_only_name_still_imports_with_slug_bot() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let text = "---\nname: \"!!!\"\ndescription: punctuation name\n---\n\nSome instructions.";
    let (status, response) = post_route(
        &app,
        "/api/import/open",
        &session,
        json!({ "fileName": "x.md", "text": text }),
    )
    .await;
    assert_eq!(status, 201, "{response:?}");
    assert_eq!(response["result"]["botId"], "bot");
    assert_eq!(response["result"]["name"], "!!!");
}

/* --------------------------------------------------------- trailing warning */

/// Bite: the trailing platform-defaults warning is conditional on whether a
/// model was honoured (divergence (c) changed what is true here) - no
/// `model:` in the file gets the TS's original two-clause warning; a
/// pinned import gets the narrower one-clause warning instead.
#[tokio::test]
async fn trailing_warning_names_model_only_when_no_model_was_in_the_file() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_with_catalog(db, fixture_catalog());

    let (_status, no_model_response) = post_route(
        &app,
        "/api/import/open",
        &session,
        json!({
            "fileName": "no-model.md",
            "text": "---\nname: No Model Bot\ndescription: x\n---\n\nDo the thing.",
        }),
    )
    .await;
    let warnings = no_model_response["result"]["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|w| w.as_str().unwrap_or("")
            == "model and permissions were not in the file, so it starts on the platform defaults"),
        "{warnings:?}"
    );

    let (_status, pinned_response) = post_route(
        &app,
        "/api/import/open",
        &session,
        json!({
            "fileName": "pinned.md",
            "text": "---\nname: Pinned Bot\ndescription: x\nmodel: anthropic/claude-sonnet-5\n---\n\nDo the thing.",
        }),
    )
    .await;
    let warnings = pinned_response["result"]["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|w| w.as_str().unwrap_or("")
            == "permissions were not in the file, so it starts on the platform defaults"),
        "{warnings:?}"
    );
    assert!(
        !warnings
            .iter()
            .any(|w| w.as_str().unwrap_or("").contains("model and permissions")),
        "a pinned import must not claim the model was defaulted too: {warnings:?}"
    );
}
