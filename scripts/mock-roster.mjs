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
