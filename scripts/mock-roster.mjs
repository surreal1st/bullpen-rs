#!/usr/bin/env node
/**
 * S0-05 stand-in for the S0-04 server, which does not exist yet. Serves the
 * `dx build --release` web output and answers `/api/roster` with the
 * fixture shape (S0-05's ticket / environment note): three bots, no
 * sections, Jason `busy: true`.
 *
 *   node scripts/mock-roster.mjs [port]
 *
 * The built app's `base_path` (Dioxus.toml) is "bullpen", so its own
 * `index.html` references absolute `/bullpen/...` asset URLs regardless of
 * what path served the page - this serves the SPA shell at `/` and mirrors
 * the built `public/` tree under `/bullpen/*` so both resolve.
 *
 * S1-07a extends this: the real message/conversation routes (S1-06) do not
 * exist yet either, so `crates/client`'s thread/composer is built and shot
 * against fixture routes here instead, matching the real API's shapes so
 * nothing needs to change once S1-06 lands (the final shot against the real
 * server is owed to it - see that ticket's `## Result`):
 *   - `GET /api/bots/:id/conversation` -> `{conversationId, messages:[...]}`,
 *     three messages (user, an assistant markdown reply with inline code,
 *     user).
 *   - `POST /api/bots/:id/messages` -> `text/event-stream`: `{type:"run",
 *     runId}`, three `delta` frames 300ms apart, then `done`.
 */
import { createServer } from "node:http";
import { readFile, stat } from "node:fs/promises";
import { extname, join, normalize } from "node:path";
import { fileURLToPath } from "node:url";

const here = fileURLToPath(new URL(".", import.meta.url));
const PUBLIC_DIR = join(here, "..", "target", "dx", "client", "release", "web", "public");
const PORT = Number(process.argv[2] ?? 4370);

const ROSTER = {
  sections: [],
  bots: [
    {
      id: "arthur",
      name: "Arthur",
      purpose: "Chief of Staff",
      sectionId: null,
      pinned: false,
      hidden: false,
      avatar: null,
      shape: null,
      busy: false,
      unread: 0,
      preview: "Standing by.",
      lastAt: null,
    },
    {
      id: "riley",
      name: "Riley",
      purpose: "Researcher",
      sectionId: null,
      pinned: false,
      hidden: false,
      avatar: null,
      shape: null,
      busy: false,
      unread: 0,
      preview: "Pulled the last five sources.",
      lastAt: null,
    },
    {
      id: "jason",
      name: "Jason",
      purpose: "Analyst",
      sectionId: null,
      pinned: false,
      hidden: false,
      avatar: null,
      shape: "hexagon",
      busy: true,
      unread: 2,
      preview: "Crunching the numbers now…",
      lastAt: null,
    },
  ],
};

/**
 * S1-07a's conversation fixture, keyed by bot id. Timestamps are minutes
 * apart and all "just now", so `thread.rs`'s date/pause divider always
 * shows exactly one "Today HH:MM" mark, on the first message, regardless of
 * when this script happens to run.
 */
const now = Date.now();
const minutesAgo = (n) => new Date(now - n * 60_000).toISOString();

const CONVERSATIONS = {
  arthur: {
    conversationId: "conv-arthur-fixture",
    messages: [
      {
        id: "fixture-1",
        role: "user",
        content: "What's on my plate today?",
        model: null,
        error: null,
        createdAt: minutesAgo(5),
      },
      {
        id: "fixture-2",
        role: "assistant",
        content:
          "Three things, in order:\n\n1. Approve the Quill hire\n2. Review before the next push\n3. Reply to the backup alert\n\nRun `git status` first if you are not sure what is staged.",
        model: "mock/echo",
        error: null,
        createdAt: minutesAgo(4),
      },
      {
        id: "fixture-3",
        role: "user",
        content: "Good, start with the second one.",
        model: null,
        error: null,
        createdAt: minutesAgo(3),
      },
    ],
  },
};

function readBody(req) {
  return new Promise((resolve, reject) => {
    let data = "";
    req.on("data", (chunk) => (data += chunk));
    req.on("end", () => resolve(data));
    req.on("error", reject);
  });
}

/** `POST /api/bots/:id/messages`'s canned run: one `run` frame, three
 * `delta` frames 300ms apart, then `done`. Ignores the request body - this
 * always replies with the same three-part answer regardless of what was
 * typed, since S1-07a only has to prove the stream renders and finalises. */
function streamMockRun(res) {
  res.writeHead(200, {
    "Content-Type": "text/event-stream",
    "Cache-Control": "no-cache",
    Connection: "keep-alive",
  });
  res.write(`data: ${JSON.stringify({ type: "run", runId: `run-${now}` })}\n\n`);
  const parts = ["Sure", " - here is the short version:", " done."];
  let i = 0;
  const step = () => {
    if (i < parts.length) {
      res.write(`data: ${JSON.stringify({ type: "delta", text: parts[i] })}\n\n`);
      i += 1;
      setTimeout(step, 300);
      return;
    }
    res.write(
      `data: ${JSON.stringify({ type: "done", model: "mock/echo", messageId: "fixture-done" })}\n\n`,
    );
    res.end();
  };
  setTimeout(step, 300);
}

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".wasm": "application/wasm",
  ".json": "application/json; charset=utf-8",
};

async function serveFile(res, filePath) {
  try {
    const st = await stat(filePath);
    if (!st.isFile()) throw new Error("not a file");
    const body = await readFile(filePath);
    res.writeHead(200, { "Content-Type": MIME[extname(filePath)] ?? "application/octet-stream" });
    res.end(body);
  } catch {
    res.writeHead(404, { "Content-Type": "text/plain" });
    res.end("not found");
  }
}

const server = createServer(async (req, res) => {
  const url = new URL(req.url, `http://${req.headers.host}`);

  if (url.pathname === "/api/roster") {
    res.writeHead(200, { "Content-Type": "application/json" });
    res.end(JSON.stringify(ROSTER));
    return;
  }

  const conversationMatch = /^\/api\/bots\/([^/]+)\/conversation$/.exec(url.pathname);
  if (conversationMatch && req.method === "GET") {
    const botId = conversationMatch[1];
    const view = CONVERSATIONS[botId] ?? { conversationId: `conv-${botId}-empty`, messages: [] };
    res.writeHead(200, { "Content-Type": "application/json" });
    res.end(JSON.stringify(view));
    return;
  }

  const messagesMatch = /^\/api\/bots\/([^/]+)\/messages$/.exec(url.pathname);
  if (messagesMatch && req.method === "POST") {
    await readBody(req);
    streamMockRun(res);
    return;
  }

  if (url.pathname === "/" || url.pathname === "/index.html") {
    return serveFile(res, join(PUBLIC_DIR, "index.html"));
  }

  // Serve assets and other files directly (base_path is not set, so assets are at /assets/*)
  if (url.pathname.startsWith("/api/")) {
    res.writeHead(404, { "Content-Type": "text/plain" });
    res.end("not found");
    return;
  }

  const rel = normalize(url.pathname);
  if (rel.startsWith("..")) {
    res.writeHead(400).end("bad path");
    return;
  }
  return serveFile(res, join(PUBLIC_DIR, rel.replace(/^\//, "")));
});

server.listen(PORT, "127.0.0.1", () => {
  console.log(`mock-roster serving ${PUBLIC_DIR} on http://127.0.0.1:${PORT}/`);
});
