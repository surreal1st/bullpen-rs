//! Migrations 1..16, SQL text copied verbatim from
//! `bullpen-night/src/server/db.ts` (`MIGRATIONS` array, lines 57-403). Only
//! the runner in `lib.rs` is Rust; every statement here is byte-identical to
//! the TS source so `bullpen.db` opens unchanged.
//!
//! The TS source's own comments misnumber two entries as "12" (a documented
//! bug, see db.ts:313 and :342) - positions below are renumbered 1..16 to
//! match array order, which is what `PRAGMA user_version` actually counts.
//!
//! 17+ are bullpen-rs's own - new features, no TS counterpart to stay
//! byte-identical to.

pub const MIGRATIONS: &[&str] = &[
    // 1
    r#"
  CREATE TABLE bots (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    purpose      TEXT NOT NULL DEFAULT '',
    instructions TEXT NOT NULL DEFAULT '',
    model        TEXT,
    created_at   TEXT NOT NULL,
    archived_at  TEXT
  );

  CREATE TABLE conversations (
    id         TEXT PRIMARY KEY,
    bot_id     TEXT NOT NULL REFERENCES bots(id),
    title      TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL
  );

  CREATE TABLE messages (
    id              TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES conversations(id),
    seq             INTEGER NOT NULL,
    role            TEXT NOT NULL CHECK (role IN ('user','assistant')),
    content         TEXT NOT NULL,
    model           TEXT,
    error           TEXT,
    created_at      TEXT NOT NULL
  );

  CREATE UNIQUE INDEX idx_messages_conv_seq ON messages(conversation_id, seq);
  "#,
    // 2. Slice 13 maintains this. It exists now because the pin rules tighten
    //    for a bot that runs unattended, and a rule that cannot be evaluated
    //    is not a rule.
    r#"ALTER TABLE bots ADD COLUMN has_routine INTEGER NOT NULL DEFAULT 0;"#,
    // 3. Cost is PROVIDER-REPORTED per request, not computed here. OpenRouter
    //    returns an exact `cost` in the final usage frame. The previous
    //    platform logged 24 input tokens for an 11-call run because it
    //    counted its own, which is why none of these numbers are ours.
    r#"
  ALTER TABLE messages ADD COLUMN cost_usd REAL;
  ALTER TABLE messages ADD COLUMN input_tokens INTEGER;
  ALTER TABLE messages ADD COLUMN output_tokens INTEGER;
  ALTER TABLE messages ADD COLUMN cached_tokens INTEGER;

  CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
  );
  "#,
    // 4. Tiered memory. The core rides in every request; the log is searched.
    r#"
  ALTER TABLE bots ADD COLUMN memory_core TEXT NOT NULL DEFAULT '';

  CREATE TABLE memory_log (
    id         TEXT PRIMARY KEY,
    bot_id     TEXT NOT NULL REFERENCES bots(id),
    content    TEXT NOT NULL,
    source     TEXT NOT NULL CHECK (source IN ('bot','josh')),
    created_at TEXT NOT NULL
  );

  CREATE INDEX idx_memory_log_bot ON memory_log(bot_id, created_at DESC);

  CREATE VIRTUAL TABLE memory_fts USING fts5(content, content='memory_log', content_rowid='rowid');

  CREATE TRIGGER memory_log_ai AFTER INSERT ON memory_log BEGIN
    INSERT INTO memory_fts(rowid, content) VALUES (new.rowid, new.content);
  END;

  CREATE TRIGGER memory_log_ad AFTER DELETE ON memory_log BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, content) VALUES ('delete', old.rowid, old.content);
  END;

  CREATE TRIGGER memory_log_au AFTER UPDATE ON memory_log BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, content) VALUES ('delete', old.rowid, old.content);
    INSERT INTO memory_fts(rowid, content) VALUES (new.rowid, new.content);
  END;
  "#,
    // 5. Runs outlive their HTTP request.
    //
    //    Until now a run executed as its SSE body was consumed, so a client
    //    that closed the tab abandoned it. An approval can wait hours and a
    //    routine has no client at all, so the run is now a row and the
    //    stream is a subscriber.
    r#"
  CREATE TABLE runs (
    id              TEXT PRIMARY KEY,
    bot_id          TEXT NOT NULL REFERENCES bots(id),
    conversation_id TEXT NOT NULL REFERENCES conversations(id),
    trigger         TEXT NOT NULL,
    status          TEXT NOT NULL CHECK (status IN ('running','waiting','done','failed')),
    model           TEXT NOT NULL,
    messages        TEXT NOT NULL,
    text            TEXT NOT NULL DEFAULT '',
    cost_usd        REAL NOT NULL DEFAULT 0,
    input_tokens    INTEGER NOT NULL DEFAULT 0,
    output_tokens   INTEGER NOT NULL DEFAULT 0,
    cached_tokens   INTEGER NOT NULL DEFAULT 0,
    steps           INTEGER NOT NULL DEFAULT 0,
    error           TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
  );

  CREATE INDEX idx_runs_status ON runs(status, created_at DESC);

  CREATE TABLE approvals (
    id          TEXT PRIMARY KEY,
    run_id      TEXT NOT NULL REFERENCES runs(id),
    bot_id      TEXT NOT NULL REFERENCES bots(id),
    tool_name   TEXT NOT NULL,
    tool_args   TEXT NOT NULL,
    call_id     TEXT NOT NULL,
    status      TEXT NOT NULL CHECK (status IN ('pending','approved','rejected')),
    created_at  TEXT NOT NULL,
    decided_at  TEXT
  );

  CREATE INDEX idx_approvals_pending ON approvals(status, created_at DESC);

  ALTER TABLE bots ADD COLUMN permissions TEXT NOT NULL DEFAULT '{}';
  "#,
    // 6. Routines: work that happens while nobody is watching.
    r#"
  CREATE TABLE routines (
    id           TEXT PRIMARY KEY,
    bot_id       TEXT NOT NULL REFERENCES bots(id),
    name         TEXT NOT NULL,
    prompt       TEXT NOT NULL,
    schedule     TEXT NOT NULL,
    active       INTEGER NOT NULL DEFAULT 0,
    next_run_at  TEXT,
    last_run_at  TEXT,
    created_at   TEXT NOT NULL
  );

  CREATE INDEX idx_routines_due ON routines(active, next_run_at);

  ALTER TABLE runs ADD COLUMN routine_id TEXT;
  CREATE INDEX idx_runs_routine ON runs(routine_id, created_at DESC);
  "#,
    // 7. Attachments. The file lives on disk; this is the record of it.
    r#"
  CREATE TABLE attachments (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    content_type TEXT NOT NULL,
    bytes        INTEGER NOT NULL,
    created_at   TEXT NOT NULL
  );

  ALTER TABLE messages ADD COLUMN attachment_id TEXT;
  "#,
    // 8. Threads and search.
    //
    //    A bot had exactly one conversation, which is fine with four
    //    messages and useless with four hundred. Search is FTS5 over message
    //    text, kept in step by triggers, the same arrangement the memory log
    //    uses.
    r#"
  ALTER TABLE conversations ADD COLUMN archived_at TEXT;
  ALTER TABLE conversations ADD COLUMN last_at TEXT;

  CREATE VIRTUAL TABLE message_fts USING fts5(content, content='messages', content_rowid='rowid');

  CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO message_fts(rowid, content) VALUES (new.rowid, new.content);
  END;

  CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
    INSERT INTO message_fts(message_fts, rowid, content) VALUES ('delete', old.rowid, old.content);
  END;

  CREATE TRIGGER messages_au AFTER UPDATE ON messages BEGIN
    INSERT INTO message_fts(message_fts, rowid, content) VALUES ('delete', old.rowid, old.content);
    INSERT INTO message_fts(rowid, content) VALUES (new.rowid, new.content);
  END;

  INSERT INTO message_fts(rowid, content) SELECT rowid, content FROM messages;
  "#,
    // 9. Connectors, enabled per bot rather than platform-wide.
    r#"
  CREATE TABLE connectors (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    url         TEXT NOT NULL,
    auth_header TEXT,
    created_at  TEXT NOT NULL
  );

  CREATE TABLE bot_connectors (
    bot_id       TEXT NOT NULL REFERENCES bots(id),
    connector_id TEXT NOT NULL REFERENCES connectors(id),
    PRIMARY KEY (bot_id, connector_id)
  );
  "#,
    // 10. Marketplaces. A URL that serves one manifest; no infrastructure.
    r#"
  CREATE TABLE marketplaces (
    id       TEXT PRIMARY KEY,
    name     TEXT NOT NULL,
    url      TEXT NOT NULL,
    added_at TEXT NOT NULL
  );
  "#,
    // 11. The rail: sections, pinning, hiding, and unread.
    //
    // Sections are seeded from what the roster already says about itself.
    // Every imported bot has a purpose like "SHOOT - Web" or "Business
    // Support", so the part before the dash is the section it already
    // belongs to. Starting everything in Unassigned would mean Josh
    // hand-sorting fifteen bots to reproduce an arrangement the data already
    // describes.
    //
    // Unread is a TIMESTAMP, not a sequence number. `seq` restarts per
    // conversation, so it cannot be compared across a bot's threads, and a
    // bot answering in a second thread would read as already seen.
    r#"
  CREATE TABLE sections (
    id       TEXT PRIMARY KEY,
    name     TEXT NOT NULL,
    position INTEGER NOT NULL DEFAULT 0
  );

  ALTER TABLE bots ADD COLUMN section_id   TEXT REFERENCES sections(id);
  ALTER TABLE bots ADD COLUMN pinned_at    TEXT;
  ALTER TABLE bots ADD COLUMN hidden_at    TEXT;
  ALTER TABLE bots ADD COLUMN avatar       TEXT;
  ALTER TABLE bots ADD COLUMN last_seen_at TEXT;

  INSERT INTO sections (id, name)
  SELECT DISTINCT
    lower(replace(replace(sect, ' ', '-'), '/', '-')),
    sect
  FROM (
    SELECT CASE
             WHEN instr(purpose, ' - ') > 0
               THEN substr(purpose, 1, instr(purpose, ' - ') - 1)
             ELSE purpose
           END AS sect
      FROM bots
     WHERE archived_at IS NULL AND trim(purpose) <> ''
  )
  WHERE trim(sect) <> '';

  UPDATE bots SET section_id = (
    SELECT s.id FROM sections s
     WHERE s.name = CASE
                      WHEN instr(bots.purpose, ' - ') > 0
                        THEN substr(bots.purpose, 1, instr(bots.purpose, ' - ') - 1)
                      ELSE bots.purpose
                    END
  );

  CREATE INDEX bots_section ON bots(section_id);
  "#,
    // 12. OAuth for connectors. Gmail, Calendar and every other hosted MCP
    // server needs this; a static bearer token is not something they issue.
    //
    // Two tables because they have different lifetimes. Credentials last
    // until Josh disconnects; a flow lasts about a minute, between opening
    // the consent screen and the callback coming back, and is deleted the
    // moment it is used.
    r#"
  CREATE TABLE connector_auth (
    connector_id  TEXT PRIMARY KEY REFERENCES connectors(id),
    issuer        TEXT NOT NULL,
    authorize_url TEXT NOT NULL,
    token_url     TEXT NOT NULL,
    resource      TEXT NOT NULL,
    client_id     TEXT NOT NULL,
    client_secret TEXT,
    access_token  TEXT,
    refresh_token TEXT,
    expires_at    TEXT,
    scope         TEXT,
    connected_at  TEXT
  );

  CREATE TABLE oauth_flows (
    state        TEXT PRIMARY KEY,
    connector_id TEXT NOT NULL REFERENCES connectors(id),
    verifier     TEXT NOT NULL,
    created_at   TEXT NOT NULL
  );
  "#,
    // 13. A login, because Bullpen moved off the tailnet onto a public
    // hostname. One password (this is a single-person product), many
    // sessions.
    r#"
  CREATE TABLE auth_settings (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    salt       TEXT NOT NULL,
    hash       TEXT NOT NULL,
    updated_at TEXT NOT NULL
  );

  CREATE TABLE sessions (
    token      TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
  );

  CREATE INDEX idx_sessions_expiry ON sessions(expires_at);
  "#,
    // 14. Which streams a bot has already pinged about.
    //
    // This replaces a JSON file the model wrote into its own sandbox by hand
    // every run - which cost a whole extra model turn (~12s) and went wrong
    // in the ways hand-written state does: written BEFORE the check it was
    // recording, and rewritten with python3, which does not exist in the
    // sandbox.
    //
    // Keyed on the stream START time rather than the channel: the same
    // person live again tomorrow is a new stream and must ping, the same
    // stream an hour later must not. One row per bot per channel is all
    // dedup needs.
    r#"
  CREATE TABLE twitch_pings (
    bot_id            TEXT NOT NULL REFERENCES bots(id),
    login             TEXT NOT NULL,
    stream_started_at TEXT NOT NULL,
    pinged_at         TEXT NOT NULL,
    PRIMARY KEY (bot_id, login)
  );
  "#,
    // 15. Link previews, so a Twitch or YouTube link in a message shows a
    // thumbnail instead of a blue string.
    //
    // Cached because the same stream gets linked repeatedly and every render
    // would otherwise be a round trip to Twitch. A live channel goes stale
    // in minutes and a YouTube title does not, so the TTL is chosen by
    // provider at read time rather than stored here.
    r#"
  CREATE TABLE link_previews (
    url        TEXT PRIMARY KEY,
    provider   TEXT NOT NULL DEFAULT '',
    title      TEXT NOT NULL DEFAULT '',
    subtitle   TEXT NOT NULL DEFAULT '',
    image      TEXT NOT NULL DEFAULT '',
    live       INTEGER NOT NULL DEFAULT 0,
    fetched_at TEXT NOT NULL
  );
  "#,
    // 16. A bot's face shape, so Josh can set it by role.
    //
    // Null means "not chosen", which is NOT the same as the default: an
    // unchosen shape is hashed from the bot's name so a roster imported in
    // one go arrives already distinguishable, the same reason the colours
    // are generated rather than picked.
    r#"ALTER TABLE bots ADD COLUMN shape TEXT;"#,
    // 17. S2-F-07 (F8): a parked approval nobody ever answers used to hold
    //     its `pending` row (and the run's bus/backlog entries) forever, and
    //     a decided row was never pruned either. The sweep that fixes this
    //     needs a third terminal status distinct from `approved`/`rejected`
    //     so Josh's history can tell "he said no" from "he never got to it" -
    //     which means rebuilding the table, since SQLite has no ALTER TABLE
    //     for a CHECK constraint. First bullpen-rs-only migration: 1..16 are
    //     the byte-for-byte TS port, this and everything after is new.
    r#"
  CREATE TABLE approvals_new (
    id          TEXT PRIMARY KEY,
    run_id      TEXT NOT NULL REFERENCES runs(id),
    bot_id      TEXT NOT NULL REFERENCES bots(id),
    tool_name   TEXT NOT NULL,
    tool_args   TEXT NOT NULL,
    call_id     TEXT NOT NULL,
    status      TEXT NOT NULL CHECK (status IN ('pending','approved','rejected','expired')),
    created_at  TEXT NOT NULL,
    decided_at  TEXT
  );

  INSERT INTO approvals_new (id, run_id, bot_id, tool_name, tool_args, call_id, status, created_at, decided_at)
    SELECT id, run_id, bot_id, tool_name, tool_args, call_id, status, created_at, decided_at FROM approvals;

  DROP TABLE approvals;
  ALTER TABLE approvals_new RENAME TO approvals;

  CREATE INDEX idx_approvals_pending ON approvals(status, created_at DESC);
  "#,
    // 18. S3-01: memory tiers, TTL, scope, projects. Adds columns to
    //     memory_log (kind, expires_at, scope, project_id) and creates
    //     projects and project_members tables for scoped memory.
    r#"
  ALTER TABLE memory_log ADD COLUMN kind TEXT NOT NULL DEFAULT 'log';
  ALTER TABLE memory_log ADD COLUMN expires_at TEXT;
  ALTER TABLE memory_log ADD COLUMN scope TEXT NOT NULL DEFAULT 'own';
  ALTER TABLE memory_log ADD COLUMN project_id TEXT;

  CREATE TABLE projects (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    created_at TEXT NOT NULL
  );

  CREATE TABLE project_members (
    project_id TEXT NOT NULL REFERENCES projects(id),
    bot_id     TEXT NOT NULL REFERENCES bots(id),
    PRIMARY KEY (project_id, bot_id)
  );
  "#,
    // 19. S3-F-01b (F5): prevent duplicate project names (case-insensitive).
    r#"
  CREATE UNIQUE INDEX IF NOT EXISTS idx_projects_name ON projects(lower(name));
  "#,
    // 20. S4-01: auto_review_log + judge columns on approvals
    r#"
  CREATE TABLE auto_review_log (
    id          TEXT PRIMARY KEY,
    bot_id      TEXT NOT NULL,
    run_id      TEXT NOT NULL,
    tool_name   TEXT NOT NULL,
    description TEXT NOT NULL,
    verdict     TEXT NOT NULL,
    reason      TEXT NOT NULL,
    decision    TEXT NOT NULL,
    created_at  TEXT NOT NULL
  );

  ALTER TABLE approvals ADD COLUMN judge_verdict TEXT;
  ALTER TABLE approvals ADD COLUMN judge_reason TEXT;
  "#,
    // 21. S6-W-02: per-bot egress policy (mode + allow list), stored as JSON
    // string. Defaults to OFF mode with no allow list.
    r#"ALTER TABLE bots ADD COLUMN egress TEXT NOT NULL DEFAULT '{"mode":"off","allow":[]}';"#,
];
