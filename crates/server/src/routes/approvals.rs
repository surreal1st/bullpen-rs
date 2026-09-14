//! `GET /api/approvals`, `POST /api/approvals/:id`. Port of
//! `app.ts:4027-4032,4064-4160`. `GET /api/approvals/:id/proposal` is
//! explicitly SKIPPED - it is W5's `propose_tool` diagnostics card, and
//! nothing in this Rust port proposes tools yet.
//!
//! S2-07: `remember` on `POST /api/approvals/:id` (the "always allow" /
//! "never" press) and the four `/api/auto-review/rules` CRUD routes both
//! live here rather than in `crate::rules` itself - a rule only ever
//! exists to answer an approval, and `remember` is the one place that
//! writes both the grid AND a rule from the same press, so the two features
//! share this file the way `app.ts` puts them next to each other
//! (`:4065-4368`). Port of `app.ts:4027-4032,4064-4160,4321-4368`.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::rules::{self, Rule, RuleBehavior};
use crate::{ApiResult, AppError, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/approvals", get(list_approvals))
        .route("/api/approvals/{id}", post(decide))
        .route("/api/auto-review/rules", get(list_rules).post(create_rule))
        .route(
            "/api/auto-review/rules/{id}",
            put(put_rule).delete(delete_rule),
        )
}

/// What one row of `GET /api/approvals` looks like - the fields
/// `test/approvals.test.ts`'s own `pending()` helper reads (`id`,
/// `toolName`, `toolArgs`, `botName`, `trigger`), plus `runId`/`botId` for a
/// client that wants to link back to the run or the bot.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ApprovalView {
    id: String,
    run_id: String,
    bot_id: String,
    bot_name: String,
    tool_name: String,
    tool_args: String,
    created_at: String,
    trigger: Option<String>,
}

impl From<crate::approvals::Approval> for ApprovalView {
    fn from(a: crate::approvals::Approval) -> Self {
        ApprovalView {
            id: a.id,
            run_id: a.run_id,
            bot_id: a.bot_id,
            bot_name: a.bot_name,
            tool_name: a.tool_name,
            tool_args: a.tool_args,
            created_at: a.created_at,
            trigger: a.trigger,
        }
    }
}

async fn list_approvals(State(state): State<AppState>) -> ApiResult<Response> {
    let approvals = {
        let db = state.db();
        crate::approvals::list_pending(&db)?
    };
    let views: Vec<ApprovalView> = approvals.into_iter().map(ApprovalView::from).collect();
    Ok(Json(json!({ "approvals": views })).into_response())
}

#[derive(Deserialize, Default)]
struct DecideBody {
    #[serde(default)]
    approved: bool,
    /// "allow" | "deny" | absent. Port of the TS body's own `remember`
    /// field - see `decide`'s own doc for what pressing it does.
    remember: Option<String>,
}

/// H4's "always allow" / "never" buttons, plus U7's rule on top: a
/// `remember` press writes the grid override it always did AND an
/// auto-review rule in the words of the actual pending call, so Josh sees
/// a sentence he can read, edit or delete afterwards rather than a change
/// to a grid he never looks at this way. Port of `app.ts:4077-4119`.
///
/// Looked up from the PENDING list rather than trusted from the body: the
/// client says which approval, not which bot or tool, so a stale or forged
/// `remember` cannot be pointed at a tool the pending row does not name.
/// `ask_josh` is refused outright - a question is not a permission.
async fn decide(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Response> {
    let parsed: DecideBody = super::parse_body(&body)?;
    let remember = match parsed.remember.as_deref() {
        Some("allow") => Some(true),
        Some("deny") => Some(false),
        _ => None,
    };

    if let Some(allow) = remember {
        let db = state.db();
        let target = crate::approvals::list_pending(&db)?
            .into_iter()
            .find(|a| a.id == id);
        if let Some(target) = target {
            if shared::ask_josh::is_answerable(&target.tool_name) {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "a question cannot be remembered"})),
                )
                    .into_response());
            }

            // Read-then-write, not a fresh object: `set_permissions`
            // replaces the whole stored row, so writing just the one tool
            // would erase every other override Josh had already set.
            let current = crate::permissions::get_permissions(&db, &target.bot_id)?;
            let mut next = current;
            next.insert(
                target.tool_name.clone(),
                if allow {
                    crate::permissions::Decision::Allow
                } else {
                    crate::permissions::Decision::Deny
                },
            );
            crate::permissions::set_permissions(&db, &target.bot_id, &next)?;

            // The rule is the part Josh's screen shows him afterwards - a
            // sentence, not a grid flip. Errors here are logged, not fatal:
            // the grid write above already landed, and a rule that failed
            // to save is not a reason to fail the whole approval.
            let text = rules::describe_call(&target.tool_name, &target.tool_args);
            if let Err(err) = rules::add_rule(
                &db,
                Some(target.bot_id.clone()),
                &text,
                if allow {
                    RuleBehavior::Allow
                } else {
                    RuleBehavior::Never
                },
            ) {
                tracing::error!("approval {id}: failed to write an auto-review rule: {err}");
            }
        }
    }

    let approved = remember.unwrap_or(parsed.approved);
    let ok = state.runs.decide_approval(&id, approved).await;
    Ok(if ok {
        Json(json!({ "ok": true, "approved": approved })).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no such pending approval"})),
        )
            .into_response()
    })
}

// ---- U7: auto-review rules CRUD ----
//
// Named `/api/auto-review/rules`, not `/api/rules` - that path already
// belongs to Settings' free-text "Rules for every bot" box, a different
// feature (one prose blob folded into every prompt) that would either
// collide with this or be mistaken for it. `botId` is a query param rather
// than a path segment because a rule can apply to "every bot" (`bot_id:
// None`) - there is no single bot's URL to hang a global rule off of. Port
// of `app.ts:4331-4367`.

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RuleView {
    id: String,
    bot_id: Option<String>,
    text: String,
    behavior: RuleBehavior,
    created_at: String,
    hits: i64,
}

impl From<Rule> for RuleView {
    fn from(r: Rule) -> Self {
        RuleView {
            id: r.id,
            bot_id: r.bot_id,
            text: r.text,
            behavior: r.behavior,
            created_at: r.created_at,
            hits: r.hits,
        }
    }
}

#[derive(Deserialize, Default)]
struct RulesQuery {
    #[serde(rename = "botId")]
    bot_id: Option<String>,
}

async fn list_rules(
    State(state): State<AppState>,
    Query(query): Query<RulesQuery>,
) -> ApiResult<Response> {
    let db = state.db();
    let rules = match query.bot_id.filter(|b| !b.is_empty()) {
        None => rules::list_global_rules(&db)?,
        Some(bot_id) => {
            let Some(_bot) = store::get_bot(&db, &bot_id)? else {
                return Ok(
                    (StatusCode::NOT_FOUND, Json(json!({"error": "no such bot"}))).into_response(),
                );
            };
            rules::list_rules_for(&db, &bot_id)?
        }
    };
    let views: Vec<RuleView> = rules.into_iter().map(RuleView::from).collect();
    Ok(Json(json!({ "rules": views })).into_response())
}

#[derive(Deserialize, Default)]
struct CreateRuleBody {
    #[serde(rename = "botId")]
    bot_id: Option<String>,
    #[serde(default)]
    text: String,
    behavior: Option<String>,
}

async fn create_rule(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> ApiResult<Response> {
    let parsed: CreateRuleBody = super::parse_body(&body)?;
    let db = state.db();

    if let Some(bot_id) = &parsed.bot_id
        && store::get_bot(&db, bot_id)?.is_none()
    {
        return Ok((StatusCode::NOT_FOUND, Json(json!({"error": "no such bot"}))).into_response());
    }
    let behavior = parsed
        .behavior
        .as_deref()
        .and_then(RuleBehavior::parse)
        .unwrap_or(RuleBehavior::Ask);

    match rules::add_rule(&db, parsed.bot_id, &parsed.text, behavior) {
        Ok(rule) => Ok((
            StatusCode::CREATED,
            Json(json!({ "rule": RuleView::from(rule) })),
        )
            .into_response()),
        Err(message) => Ok(AppError::bad_request(message).into_response()),
    }
}

#[derive(Deserialize, Default)]
struct PatchRuleBody {
    text: Option<String>,
    behavior: Option<String>,
}

async fn put_rule(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Response> {
    let parsed: PatchRuleBody = super::parse_body(&body)?;
    let behavior = parsed.behavior.as_deref().and_then(RuleBehavior::parse);
    let db = state.db();

    match rules::update_rule(&db, &id, parsed.text, behavior) {
        Ok(Some(rule)) => Ok(Json(json!({ "rule": RuleView::from(rule) })).into_response()),
        Ok(None) => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no such rule"})),
        )
            .into_response()),
        Err(message) => Ok(AppError::bad_request(message).into_response()),
    }
}

async fn delete_rule(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    let db = state.db();
    let gone = rules::delete_rule(&db, &id)?;
    Ok(if gone {
        Json(json!({ "ok": true })).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no such rule"})),
        )
            .into_response()
    })
}
