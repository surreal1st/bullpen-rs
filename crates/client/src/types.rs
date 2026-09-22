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

/// RAIL-02: `POST /api/sections`'s success shape.
#[derive(Debug, Clone, Deserialize)]
pub struct SectionField {
    pub section: Section,
}

/// RAIL-02: `PATCH`/`DELETE /api/sections/:id`'s success shape. `DELETE`
/// also carries `bots` (the ticket's own "one call refreshes the rail"), but
/// `settings.rs`'s section manager does not read it - the very next
/// `on_restored` refetch of `/api/roster` is what actually updates the rail,
/// same posture `set_bot_pinned`/`set_bot_hidden` in `api.rs` already take
/// with `patch_rail`'s own echoed roster.
#[derive(Debug, Clone, Deserialize)]
pub struct SectionsField {
    #[serde(default)]
    pub sections: Vec<Section>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Bot {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub purpose: String,
    // EDIT-01: the roster response (`store::roster::list_roster`'s own
    // `SELECT id, name, purpose, instructions, ...`) has always carried
    // this - this struct just never decoded it, since nothing needed it
    // before `edit_bot.rs`'s modal had to prefill an Instructions field
    // from the bot already open. `#[serde(default)]` for the same reason
    // every other field here has it: a hand-built fixture that omits it
    // still decodes.
    #[serde(default)]
    pub instructions: String,
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
    /// S12-04b: `SpeechSynthesisVoice` name for read-aloud; null is device default.
    #[serde(default)]
    pub voice: Option<String>,
    // ARCH-01: `false` for every bot the roster (`GET /api/roster`) ever
    // carries - `store::list_roster` filters `archived_at IS NULL`, so this
    // only ever reads `true` on a row from `GET /api/bots/archived`
    // (`api::fetch_archived_bots`, `settings.rs`'s restore section).
    // `#[serde(default)]` rather than required: the field exists on every
    // real server response, but keeps this struct decoding a hand-built
    // fixture that omits it, same posture as every other field here.
    #[serde(default)]
    pub archived: bool,
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

/// `GET /api/bots/archived`'s response shape - the restore surface's only
/// fetch. Reuses `Bot` rather than a narrower type: every field on it is
/// still meaningful for an archived row (a pinned model, a purpose to show
/// beside the name), and `settings.rs`'s list has no reason to throw any of
/// it away.
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct ArchivedBotsResponse {
    #[serde(default)]
    pub bots: Vec<Bot>,
}

/// RAIL-01: `GET /api/bots/hidden`'s response shape - `settings.rs`'s
/// `HiddenBotsSection`'s only fetch, same reasoning `ArchivedBotsResponse`
/// above already gives for reusing `Bot` rather than a narrower type.
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct HiddenBotsResponse {
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

/// THREADS-01: one of a bot's own conversations, as `threads.rs`'s strip
/// draws it. Mirrors `shared::ThreadSummary` (`crates/shared/src/lib.rs`) -
/// kept local for the same reason `Bot`/`RoomSummary` are: this crate does
/// not depend on `shared`. `kind`/`members` (room vs ordinary chat) are not
/// carried here - `list_threads` already excludes rooms server-side
/// (`crates/store/src/conversations.rs::list_threads`'s own `WHERE ...
/// c.kind != 'room'`), so every entry this type ever decodes is an
/// ordinary chat.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ThreadSummary {
    pub id: String,
    #[serde(rename = "botId")]
    pub bot_id: String,
    #[serde(rename = "botName")]
    pub bot_name: String,
    #[serde(default)]
    pub title: String,
    #[serde(rename = "messageCount", default)]
    pub message_count: i64,
    #[serde(rename = "lastAt", default)]
    pub last_at: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

/// `GET /api/bots/:id/threads`'s response shape.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ThreadsResponse {
    #[serde(default)]
    pub threads: Vec<ThreadSummary>,
}

/// `POST /api/bots/:id/threads`'s success shape.
#[derive(Debug, Clone, Deserialize)]
pub struct ThreadResponse {
    pub thread: ThreadSummary,
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
/// every boot (`app.rs`, ported from `Gate.tsx:67-77`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthStatus {
    pub configured: bool,
    pub signed_in: bool,
    #[serde(default)]
    pub role: Option<String>,
}

/* ---------------------------------------------------------- S11-03: people */

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
#[serde(rename_all = "camelCase")]
pub struct PeopleUser {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub email: Option<String>,
    pub role: String,
    pub ceiling_usd: Option<f64>,
    #[serde(default)]
    pub created_at: String,
    pub archived_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
#[serde(rename_all = "camelCase")]
pub struct InviteSummary {
    pub token: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub expires_at: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct UsersListResponse {
    #[serde(default)]
    pub users: Vec<PeopleUser>,
    #[serde(default)]
    pub invites: Vec<InviteSummary>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MintInviteResponse {
    pub invite: InviteSummary,
    pub url: String,
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

/* ------------------------------------------------------------- SPEND-01 */

/// The account's month-to-date usage, from `GET /api/spend`'s `account`
/// field - null on a credits read failure (see `SpendView::account_readable`).
/// Mirrors `routes/spend.rs::AccountSpend`.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSpend {
    pub total_usage: f64,
}

/// One bot's month-to-date spend row, from `GET /api/spend`'s `bots` array,
/// already sorted highest-first by the server
/// (`spend::spend_by_bot`'s `ORDER BY ... DESC`). A strict subset of
/// `server::spend::BotSpend` - the panel only shows name and dollars, so the
/// token/run counts are left off rather than carried unused.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpendBotRow {
    pub bot_id: String,
    pub bot_name: String,
    pub cost_usd: f64,
    /// COST-01: how many of this bot's assistant messages this month came
    /// back with no provider-reported cost at all. `cost_usd` is the sum of
    /// only what was priced, so this is the count the panel shows beside it
    /// rather than letting the dollar figure quietly understate the bot.
    #[serde(default)]
    pub unpriced_count: i64,
}

/// `GET /api/spend`'s response shape (`routes/spend.rs::GetSpendResponse`).
/// `account_readable` is the flag to trust over `account.is_some()` alone -
/// the ticket's own contract: a credits read failure must render as "we
/// could not read it", never as "$0.00 of $X" (the inverted-flag mutation
/// the ticket names as the one that matters).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpendView {
    pub month: String,
    pub ceiling: f64,
    #[serde(default)]
    pub account: Option<AccountSpend>,
    #[serde(default)]
    pub bots: Vec<SpendBotRow>,
    pub account_readable: bool,
    #[serde(default)]
    pub error: Option<String>,
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

/* ---------------------------------------------------------------- S10-02 */

/// `GET /api/skills`'s list shape - body STRIPPED, `bytes` carrying its
/// length instead (`routes/skills.rs::get_skills`'s own shape, mirroring
/// `store::skills::Skill` minus `body`). The library list renders this and
/// only this; the body is a separate fetch, see `SkillBodyField` below.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillSummary {
    pub id: String,
    pub name: String,
    /// When to use it - the one line the library row shows and the only
    /// thing rendered before a row is opened.
    pub description: String,
    #[serde(default)]
    pub bytes: u64,
    /// "bullpen" | "claude-code" - kept as a plain string, same convention
    /// `Bot.shape`/`RoutingLogEntry.verdict` already use elsewhere in this
    /// crate rather than a duplicate enum for a value only ever compared
    /// against one literal.
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
}

/// `GET /api/skills`'s response shape.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SkillsField {
    #[serde(default)]
    pub skills: Vec<SkillSummary>,
}

/// `GET /api/skills/:name`'s response shape - the ONLY route that ever
/// carries a skill's body. Only `body` is decoded: `settings.rs`'s expanded
/// row already has every other field from the `SkillSummary` it fetched
/// this on behalf of, so there is nothing else here worth a second copy of.
#[derive(Debug, Clone, Deserialize)]
pub struct SkillBodyField {
    pub skill: SkillBodyOnly,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkillBodyOnly {
    #[serde(default)]
    pub body: String,
}

/// Full skill row - body included. Returned by `GET /api/skills/:name` and
/// `PUT /api/skills/:name` (`routes/skills.rs`'s `get_skill`/`put_skill`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub body: String,
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
}

/// `PUT /api/skills/:name`'s success shape.
#[derive(Debug, Clone, Deserialize)]
pub struct SkillPutField {
    pub skill: Skill,
}

/// `GET /api/bots/:id/skills` and `PUT /api/bots/:id/skills/:name`'s shared
/// response shape - the bot's enabled skill NAMES, never full `Skill`/
/// `SkillSummary` rows (`routes/skills.rs`'s own doc on both routes). The PUT
/// route's list is what `edit_bot.rs` must paint a toggle from - never the
/// click that sent the request, see that module's own doc.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct BotSkillNames {
    #[serde(default)]
    pub skills: Vec<String>,
}

/* --------------------------------------------------------------- IMPORT-02 */

/// `POST /api/import/open/preview`'s success shape - the whole parse the
/// server would use to create a bot, before anything is created. Field
/// names mirror `ParsedOpenBot` in `crates/server/src/import_open.rs`
/// exactly, including its one camelCase wire name (`declaredTools`); the
/// server's `format` field (an `OpenFormat` enum there) is read here as a
/// plain `String` ("skill" / "subagent" / "agents-md" / "unknown") since
/// `new_bot.rs` only ever displays it, never branches on it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenPreview {
    pub format: String,
    pub name: String,
    pub purpose: String,
    #[serde(default)]
    pub instructions: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub declared_tools: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// `{"preview": {...}}"`, `POST /api/import/open/preview`'s envelope.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenPreviewResponse {
    pub preview: OpenPreview,
}

/// `POST /api/import/open`'s 201 body - `crates/server/src/routes/
/// import.rs`'s own response literal, `{"result": {"ok", "botId", "name",
/// "format", "warnings"}}`, NOT a full bot row (unlike `BotPatchResponse`
/// above). Only `bot_id` is read (`api::import_open_bot`'s own doc on why it
/// re-fetches the roster rather than fabricating a `Bot` from these partial
/// fields), so only `bot_id` is typed here - `ok`/`name`/`format`/
/// `warnings` are real fields on the wire but nothing in this client reads
/// them, and a `#[derive(Deserialize)]` struct is not `deny_unknown_fields`,
/// so leaving them off costs nothing and does not fail decoding.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenImportResult {
    pub bot_id: String,
}

/// `{"result": {...}}"`, `POST /api/import/open`'s success envelope.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenImportResponse {
    pub result: OpenImportResult,
}

/// The 400 body BOTH import routes can answer with - unlike `ModelError`
/// above, which only ever carries `error`, `crates/server/src/routes/
/// import.rs`'s two early refusals (no instructions, a duplicate name) carry
/// `warnings` alongside it, and the model-pin refusal and the "no file
/// content" 400 carry `error` alone - `#[serde(default)]` on `warnings`
/// covers that case with an empty list rather than a decode failure.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenImportError {
    pub error: String,
    #[serde(default)]
    pub warnings: Vec<String>,
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

/* ----------------------------------------------------------- S11-07: away */

/// W6: `GET /api/away` — one row per bot with something to catch up on.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AwayBotRow {
    pub bot_id: String,
    pub bot_name: String,
    pub unread: i64,
    pub last_unread_line: String,
    pub questions: i64,
    pub approvals: i64,
    pub stopped_routines: Vec<String>,
}

/// W6: `GET /api/away`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AwayPayload {
    pub show: bool,
    #[serde(default)]
    pub gap_hours: Option<f64>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub bots: Option<Vec<AwayBotRow>>,
    #[serde(default)]
    pub generated_at: Option<String>,
}

/* -------------------------------------------------------- S11-08: attention */

/// `GET /api/attention` — same shape as `store::attention::Attention`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct Attention {
    pub approvals: i64,
    pub unread: i64,
    pub total: i64,
}
