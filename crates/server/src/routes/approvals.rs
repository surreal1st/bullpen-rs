//! `GET /api/approvals`, `POST /api/approvals/:id`. Port of
//! `app.ts:4027-4032,4064-4160`. `GET /api/approvals/:id/proposal` is
//! explicitly SKIPPED - it is W5's `propose_tool` diagnostics card, and
//! nothing in this Rust port proposes tools yet.
//!
//! `remember` (the "always allow"/"never" press that also writes an
//! auto-review rule) is S2-07's deliverable, blocked on this ticket - the
//! body here only reads `approved`, same shape the TS route falls back to
//! when `remember` is absent.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{ApiResult, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/approvals", get(list_approvals))
        .route("/api/approvals/{id}", post(decide))
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
}

async fn decide(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Response> {
    let parsed: DecideBody = super::parse_body(&body)?;
    let ok = state.runs.decide_approval(&id, parsed.approved).await;
    Ok(if ok {
        Json(json!({ "ok": true, "approved": parsed.approved })).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no such pending approval"})),
        )
            .into_response()
    })
}
