//! Roster JSON shape from `/api/roster`. Kept local to `client` rather than
//! `crates/shared` - S0-05 is not allowed to touch `shared` (another
//! builder owns `store`/`server`/`model`), and the real shared type lands
//! whenever S0-04 does.

use serde::{Deserialize, Serialize};

/// Who said a message. Ported from `Message.role` in `shared/types.ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// One message in a conversation, from `GET /api/bots/:id/conversation`.
/// A strict subset of `Message` in `shared/types.ts:101-134` - S1-07a's
/// bubble has no use yet for `attachment`, `reaction` or the guest `botId`
/// (no rooms, no attachments: see the ticket's "SKIP" list), so they are
/// left off rather than carried as unused fields.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Message {
    pub id: String,
    pub role: Role,
    pub content: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

/// `GET /api/bots/:id/conversation`'s response shape. A strict subset of
/// `ConversationView` in `shared/types.ts` - `effectiveModel` etc. are for
/// the off-model badge, out of scope here (see `bubble.rs`'s doc comment).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ConversationView {
    #[serde(rename = "conversationId")]
    pub conversation_id: String,
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Section {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Bot {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub purpose: String,
    #[serde(rename = "sectionId", default)]
    pub section_id: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub shape: Option<String>,
    #[serde(default)]
    pub busy: bool,
    #[serde(default)]
    pub unread: u32,
    #[serde(default)]
    pub preview: Option<String>,
    // S1-07b: fixed from `Option<i64>` - the real server (S1-06) sends an
    // ISO-8601 string here, same as `RoomSummary.last_at` below. S0-05
    // guessed epoch millis before a real server existed to check against;
    // `scripts/mock-roster.mjs`'s own fixture only ever sent `null`, so
    // nothing caught it until a roster fetch here hit a bot with a real
    // `lastAt` and failed outright ("Roster failed to load: invalid type:
    // string ..., expected i64").
    #[serde(rename = "lastAt", default)]
    pub last_at: Option<String>,
    // S2-09b: the model chip's own fields. `model` is `None` for "no pin,
    // runs the platform default"; `effort` always has a value server-side
    // (defaults to "medium" in `crates/store/src/bots.rs::row_to_bot`), kept
    // here as a bare `String` rather than a duplicate enum - the chip only
    // ever compares it against the three literal values.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_effort")]
    pub effort: String,
}

fn default_effort() -> String {
    "medium".to_string()
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct Roster {
    #[serde(default)]
    pub sections: Vec<Section>,
    #[serde(default)]
    pub bots: Vec<Bot>,
}

/// A group chat, as the rail's GROUP CHATS section draws it. Mirrors
/// `shared::RoomSummary` (`crates/shared/src/lib.rs`) - kept local for the
/// same reason `Bot`/`Section` are: this crate does not depend on `shared`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RoomSummary {
    pub id: String,
    pub title: String,
    /// Every bot in the room, in the order Josh picked them. The first id is
    /// the conversation's owner - the bot id a client posts messages to and
    /// reads `/api/bots/:id/conversation?thread=<roomId>` through.
    #[serde(rename = "memberIds")]
    pub member_ids: Vec<String>,
    #[serde(default)]
    pub unread: u32,
    #[serde(default)]
    pub preview: String,
    #[serde(rename = "lastAt", default)]
    pub last_at: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

/// `GET /api/rooms`'s response shape.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct RoomsResponse {
    #[serde(default)]
    pub rooms: Vec<RoomSummary>,
}

/// `POST /api/rooms` and `PATCH /api/rooms/:id`'s success shape.
#[derive(Debug, Clone, Deserialize)]
pub struct RoomResponse {
    pub room: RoomSummary,
}

/// One bot's line for the working indicator, from
/// `GET /api/conversations/:id/working`. Mirrors `shared::working::WorkingBot`.
/// `Serialize` too - `working_bar.rs` re-serialises a fetched list to
/// compare it against the last one it rendered (the TS `JSON.stringify`
/// posture ported byte-for-byte, see that module's doc).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct WorkingBot {
    #[serde(rename = "botId")]
    pub bot_id: String,
    pub name: String,
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(rename = "sectionId", default)]
    pub section_id: Option<String>,
    #[serde(default)]
    pub shape: Option<String>,
    pub activity: String,
    /// Parked on an approval rather than running - a still face, not an
    /// animating one.
    pub waiting: bool,
}

/// `GET /api/conversations/:id/working`'s response shape.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct WorkingResponse {
    #[serde(default)]
    pub working: Vec<WorkingBot>,
}

/// One pending approval: a run parked mid-step on a gated tool call.
/// `GET /api/approvals`'s row shape (`crates/server/src/routes/approvals.rs`'s
/// `ApprovalView`), a strict subset of `PendingApproval` in
/// `shared/types.ts` - no `routineName` field, since bullpen-rs's `trigger`
/// column is a bare kind ("chat"/"routine"/"webhook"/"goal") and nothing in
/// this port has routines of their own yet (see `approvals.rs`'s `cause_of`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingApproval {
    pub id: String,
    pub run_id: String,
    pub bot_id: String,
    pub bot_name: String,
    pub tool_name: String,
    pub tool_args: String,
    pub created_at: String,
    #[serde(default)]
    pub trigger: Option<String>,
    /// S4-05: set only when the auto-review judge (not the grid's plain
    /// ask) produced this approval - null for an ordinary grid "ask" and
    /// for anything predating migration 20. See `judge.rs`'s Design doc.
    #[serde(default)]
    pub judge_verdict: Option<String>,
    #[serde(default)]
    pub judge_reason: Option<String>,
}

/// `GET /api/approvals`'s response shape.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ApprovalsResponse {
    #[serde(default)]
    pub approvals: Vec<PendingApproval>,
}

/// One open question a bot asked without parking its run
/// (`ask_josh { wait: false }`, S2-08's default). `GET /api/questions`'s row
/// shape (`crates/server/src/routes/questions.rs`) - no `botName`: that
/// route never sends one, so `questions.rs`'s pane renders the name its
/// caller already knows (the open conversation's own bot) instead of a
/// second copy of it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenQuestion {
    pub id: String,
    pub bot_id: String,
    pub conversation_id: String,
    pub question: String,
    #[serde(default)]
    pub options: Vec<String>,
    pub asked_at: String,
}

/// `GET /api/questions`'s response shape.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct QuestionsResponse {
    #[serde(default)]
    pub questions: Vec<OpenQuestion>,
}

/// `GET /api/auth/status`'s response shape - the gate's first question on
/// every boot (`app.rs`, ported from `Gate.tsx:67-77`). `role` is not
/// carried: this client has no member-vs-owner distinction yet (S5b's
/// invites, `role` in the TS response, are out of scope for S1-F-11).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthStatus {
    pub configured: bool,
    pub signed_in: bool,
}

/* -------------------------------------------------------------- S6-VM-01 */

/// `GET`/`POST .../vm[/ensure]`'s response shape
/// (`crates/server/src/routes/vms.rs::vm_view_json`) - the fields
/// `vm_card.rs`'s pure `blank_line`/`state_word` functions and its render
/// switch on. `container`/`cdpPort`/`webPort`/`lastUsedAt` are part of the
/// wire shape (the ticket's own Endpoints list) but nothing on the card
/// reads them - narrowed the same way `Message` narrows `shared/types.ts`'s
/// fuller shape (see that type's own doc), and the same fields the TS
/// `VmCard.tsx`'s own local `VmState` interface keeps.
///
/// `PartialEq` (not just `Eq`-able by hand) is what lets `vm_card.rs` skip
/// a `Signal::set` when a poll's answer is byte-identical to the one
/// already shown - rule 3 from the ticket ("the same answer as last time
/// must not re-render the card"), same `peek()`-then-compare idiom
/// `thread.rs`'s own conversation fetch already uses.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VmState {
    pub available: bool,
    pub state: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub view_path: Option<String>,
}

/* ---------------------------------------------------------- S2-09b: settings */

/// `{"model": "..."}"`, the shape every plain model GET/PUT
/// (`/api/default-model`, `/api/mid-model`, `/api/premium-model`) answers
/// with on success.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelField {
    pub model: String,
}

/// `{"error": "..."}"`, a refused model PUT (a premium pin, an empty id).
#[derive(Debug, Clone, Deserialize)]
pub struct ModelError {
    pub error: String,
}

/// The two models an unpinned bot climbs through on the way to Escalate.
/// Ported from `General.tsx`'s `Tier1Kind` union - kept as plain field
/// access (`get`/`kind` strings) rather than an enum, since every caller
/// already has the kind as the bare string the server itself uses
/// ("code"/"reason"/"vision").
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct Tier1Models {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub vision: String,
}

impl Tier1Models {
    pub fn get(&self, kind: &str) -> &str {
        match kind {
            "code" => &self.code,
            "reason" => &self.reason,
            "vision" => &self.vision,
            _ => "",
        }
    }
}

/// `GET /api/tier1-models`'s response shape.
#[derive(Debug, Clone, Deserialize)]
pub struct Tier1Response {
    pub models: Tier1Models,
}

/// One row of the routing card's "Last 20 routings" table. Mirrors
/// `model::routing::RoutingLogEntry` on the wire.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutingLogEntry {
    pub id: String,
    pub created_at: String,
    pub verdict: String,
    pub model: String,
}

/// `GET`/`PUT /api/routing`'s response shape.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RoutingState {
    pub enabled: bool,
    pub text: String,
    #[serde(default)]
    pub log: Vec<RoutingLogEntry>,
}

/// One row of the Auto review card's "Last 20 judgements" table. Mirrors
/// `store::auto_review::LogEntry` on the wire - a strict subset (no
/// `bot_id`/`run_id`/`tool_name`: the card has nothing to link them to
/// yet, same reasoning as `RoutingLogEntry` above).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoReviewLogEntry {
    pub id: String,
    pub created_at: String,
    pub description: String,
    pub verdict: String,
    pub decision: String,
}

/// `GET`/`PUT /api/auto-review/judge`'s response shape.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct AutoReviewState {
    pub enabled: bool,
}

/// `GET /api/auto-review/log`'s response shape.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct AutoReviewLogResponse {
    #[serde(default)]
    pub entries: Vec<AutoReviewLogEntry>,
}

/// `GET`/`PUT /api/rules`'s response shape.
#[derive(Debug, Clone, Deserialize)]
pub struct RulesField {
    pub rules: String,
}

/// One row of `GET /api/models`. A strict subset of `settings.rs::ModelInfo`:
/// the picker draws price, provider support and the mainstream tag, with no
/// use yet for `contextLength`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntry {
    pub id: String,
    pub name: String,
    pub in_per_m: f64,
    pub out_per_m: f64,
    #[serde(default)]
    pub supports_tools: bool,
    #[serde(default)]
    pub batch_only: bool,
    #[serde(default)]
    pub mainstream: bool,
}

/// `GET /api/models`'s response shape.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ModelsResponse {
    #[serde(default)]
    pub models: Vec<CatalogEntry>,
    #[serde(default)]
    pub total: usize,
    #[serde(rename = "mainstreamTotal", default)]
    pub mainstream_total: usize,
}

/// `GET`/`PUT /api/bots/:id/permissions`'s response shape. Decisions are
/// kept as bare strings ("allow"/"ask"/"deny") rather than a duplicate enum
/// of `server::permissions::Decision` - this crate does not depend on
/// `server`, and the grid only ever compares against the three literals.
#[derive(Debug, Clone, Deserialize)]
pub struct PermissionsField {
    pub permissions: std::collections::HashMap<String, String>,
}

/// One bot-written tool from `GET /api/bot-tools` (W5, not yet built on the
/// Rust server - `api::fetch_bot_tools` degrades to an empty list on a 404,
/// same as the TS original's `.catch(() => setMade([]))`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MadeTool {
    pub name: String,
    pub bot_name: String,
    pub description: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct BotToolsField {
    #[serde(default)]
    pub tools: Vec<MadeTool>,
}

/// `PATCH /api/bots/:id`'s success shape (`crates/server/src/routes/bots.rs`).
#[derive(Debug, Clone, Deserialize)]
pub struct BotPatchResponse {
    pub bot: Bot,
}

/* ------------------------------------------------------------- S3-05: memory */

/// One entry in a bot's own memory log or the shared log. `GET
/// /api/bots/:id/memory` and `GET /api/memory/shared` share this row shape
/// (`crates/server/src/routes/memory.rs`'s `LogEntryResponse`).
///
/// 🔴 `kind`/`expires_at` are NOT sent by the server today - the route's
/// `LogEntryResponse` carries only `id`/`content`/`source`/`createdAt`, and
/// `store::memory::recent_log`/`search_log` do not even select the
/// `kind`/`expires_at` columns from `memory_log` (they exist in the schema -
/// see `store::memory::note`'s INSERT - just never read back out). Kept
/// `Option` here, `#[serde(default)]`, so the kind badge and "expires in…"
/// text in `memory_editor.rs` light up the moment a server ticket adds
/// them, with no second client change. Until then every entry renders with
/// no badge and no TTL text - see this ticket's `## Result` note for the
/// exact server change needed.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryEntry {
    pub id: String,
    pub content: String,
    pub source: String,
    pub created_at: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
}

/// `GET /api/bots/:id/memory`'s response shape.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryView {
    pub core: String,
    pub tokens: i64,
    pub budget: i64,
    pub over_budget: bool,
    pub entries: i64,
    #[serde(default)]
    pub log: Vec<MemoryEntry>,
}

/// `PUT /api/bots/:id/memory/core`'s response shape.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoreStatus {
    pub core: String,
    pub tokens: i64,
    pub budget: i64,
    pub over_budget: bool,
    pub entries: i64,
}

/// `POST /api/bots/:id/memory` and `POST /api/bots/:id/memory/notes`'s
/// success shape.
#[derive(Debug, Clone, Deserialize)]
pub struct MemoryEntryField {
    pub entry: MemoryEntry,
}

/// `GET`/`PUT /api/shared-core`'s response shape.
#[derive(Debug, Clone, Deserialize)]
pub struct SharedCoreField {
    pub core: String,
}

/// One project bots can be added to for scoped memory. `GET`/`POST
/// /api/projects`'s row shape.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSummary {
    pub id: String,
    pub name: String,
    pub created_at: String,
}

/// `GET /api/projects`'s response shape.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ProjectsField {
    #[serde(default)]
    pub projects: Vec<ProjectSummary>,
}

/// `POST /api/projects`'s success shape.
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectField {
    pub project: ProjectSummary,
}

/// `GET /api/memory/shared`'s response shape.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SharedLogField {
    #[serde(default)]
    pub log: Vec<MemoryEntry>,
}

/* --------------------------------------------------------- S5-05: routines */

/// One routine, from `GET/POST /api/routines`, `PATCH /api/routines/:id` and
/// `POST /api/routines/:id/active` (`crates/server/src/routes/routines.rs`,
/// mirroring `store::Routine`'s camelCase wire shape). Still a strict subset -
/// `tools`/`conditions`/`secondOpinion` are left off entirely (serde ignores
/// the extra JSON fields rather than erroring); `kind`/`tool`/`toolArgs`/
/// `hasHook`/`hookKind`/`hookEvents`/`hookMatch` were the S5b tool-kind and
/// hook UI this doc used to call "left off" too - S5b-07 adds them, see the
/// note below.
///
/// S5-F-02 (F5): `schedule_text` is now a REAL computed description
/// (`crate::schedule::describe_schedule`, injected server-side by
/// `routine_wire_json` on `list`/`create`/`patch`) - `routines_editor.rs`
/// shows this instead of the raw `schedule` field, which as of F3 holds
/// the TS JSON object (`{"kind":"interval","minutes":15}`), not a phrase,
/// so this client has no use for it and does not carry it at all (serde
/// ignores the extra JSON field). `#[serde(default)]` because
/// `POST /:id/active`'s response (owned by S5-F-01, untouched here) does
/// not send `scheduleText` yet - that response's `Routine` value is
/// discarded by `routines_editor.rs::toggle_active` anyway (it always
/// re-fetches the list right after), so an empty default there is
/// harmless rather than a hard deserialize failure.
///
/// S5b-07 adds the tool-kind and hook fields the doc above once called out
/// as skipped: `kind`/`tool`/`tool_args` (a "tool" routine runs a tool
/// instead of just prompting, `crates/server/src/routines.rs::fire_routine_tool`)
/// and `has_hook`/`hook_kind`/`hook_events`/`hook_match` (a routine that
/// fires from an inbound webhook rather than its own schedule,
/// `crates/server/src/routes/hooks.rs`). `tools`/`conditions`/
/// `second_opinion` stay off this struct - this ticket's UI has no surface
/// for a bot-permission list or an AND-group of webhook conditions, and
/// serde ignores the extra JSON fields on the way in, same as it always has
/// for `hasHook` etc. before this ticket added them here.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Routine {
    pub id: String,
    pub bot_id: String,
    pub bot_name: String,
    pub name: String,
    pub prompt: String,
    #[serde(default)]
    pub schedule_text: String,
    pub active: bool,
    pub next_run_at: Option<String>,
    pub last_run_at: Option<String>,
    pub paused_reason: Option<String>,
    pub last_error: Option<String>,
    pub failures: i32,
    /// "prompt" | "tool" - `#[serde(default)]` covers a server response
    /// from before this ticket landed (there is none live, but the same
    /// belt-and-suspenders posture `effort`/`schedule_text` above already
    /// take), never a value this client invents on its own.
    #[serde(default = "default_routine_kind")]
    pub kind: String,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub tool_args: Option<String>,
    /// Whether a webhook secret exists - the secret itself is NEVER sent
    /// back over this field or any other (`routes/hooks.rs`'s own doc: shown
    /// once, at mint time, and never again). See `routines_editor.rs`'s
    /// minted-secret UI for the one place a secret ever reaches this client.
    #[serde(default)]
    pub has_hook: bool,
    #[serde(default = "default_hook_kind")]
    pub hook_kind: String,
    #[serde(default)]
    pub hook_events: Option<Vec<String>>,
    #[serde(default)]
    pub hook_match: Option<String>,
}

fn default_routine_kind() -> String {
    "prompt".to_string()
}

fn default_hook_kind() -> String {
    "raw".to_string()
}

/// One row of `GET /api/routines/:id/runs` and `GET /api/goals/:id/runs`
/// (max 20, newest first) - the two routes answer the byte-identical shape
/// (`store::RoutineRun`/`store::goals::GoalRun`), so S5b-07's goals runs
/// panel reuses this type rather than adding a duplicate.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutineRun {
    pub id: String,
    pub status: String,
    pub text: String,
    pub error: Option<String>,
    pub cost_usd: f64,
    pub created_at: String,
}

/* ----------------------------------------------------------- S5b-07: goals */

/// One entry in a goal's log - a reflection the bot wrote, a status note (an
/// edit here or the scheduler's own "status -> X"), or a weekly report line.
/// Mirrors `store::goals::GoalLogEntry`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalLogEntry {
    pub at: String,
    pub kind: String,
    pub text: String,
}

/// One goal, from `GET/POST /api/goals` and `PATCH /api/goals/:id`
/// (`crates/server/src/routes/goals.rs`, mirroring `store::goals::Goal`'s
/// camelCase wire shape).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Goal {
    pub id: String,
    pub bot_id: String,
    pub bot_name: String,
    pub objective: String,
    pub done_when: String,
    pub status: String,
    pub budget_tokens: Option<i64>,
    pub spent_tokens: i64,
    pub budget_until: Option<String>,
    pub plan: String,
    #[serde(default)]
    pub log: Vec<GoalLogEntry>,
    pub reason: Option<String>,
    pub next_session_at: Option<String>,
    pub last_session_at: Option<String>,
    pub last_report_at: Option<String>,
    pub created_at: String,
}
