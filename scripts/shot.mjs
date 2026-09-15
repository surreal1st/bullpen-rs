#!/usr/bin/env node
/**
 * Takes a picture of the running app.
 *
 * This exists because 263 tests passed all night while the conversation pane
 * rendered as a giant rounded ellipse with the messages laid out sideways. A
 * suite that never renders a page cannot see a broken page, so rendering it has
 * to be one command rather than something improvised each time.
 *
 *   node scripts/shot.mjs [url] [outfile]
 *
 * Defaults to the live instance on meridian's tailnet address. Use the ADDRESS,
 * not the hostname: Chrome force-upgrades http on a .ts.net name (HSTS preload)
 * and the handshake against the plain port hangs.
 *
 * 🔴 Shots that need the fake port (BULLPEN_FAKE_PORT=1) require a DEBUG server
 * build: `cargo build -p server` produces the binary at `target/debug/bullpen.exe`.
 * Release builds (`cargo build --release`) strip the fake port entirely and will
 * ignore the environment variable.
 *
 * 🔴 Driven over the DevTools protocol with a REAL wait, not `--screenshot`.
 * The one-shot flags take the picture at the load event, before the app has
 * fetched its roster and conversation, and `--virtual-time-budget` waits for
 * network idle - which never comes now that the page holds the events stream
 * (E7) open; it crashed chrome-headless-shell outright. So: launch with a
 * debugging port, wait `BULLPEN_SHOT_WAIT_MS` (default 6000) of wall-clock
 * time for the second-stage fetches, optionally run `BULLPEN_SHOT_EVAL` in the
 * page first (a JS expression, to open a tab or type into the composer), then
 * capture. Node 24's built-in WebSocket is enough; no package is needed.
 */
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, readdirSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { clearTimeout, setTimeout } from "node:timers";

const URL_ARG = process.argv[2] ?? "http://127.0.0.1:4380/";
const OUT = process.argv[3] ?? join(process.cwd(), "shot.png");
const WAIT_MS = Number(process.env["BULLPEN_SHOT_WAIT_MS"] ?? 6000);
const EVAL = process.env["BULLPEN_SHOT_EVAL"];
const SIZE = process.env["BULLPEN_SHOT_SIZE"] ?? "1600,1000";
// Optional: after EVAL runs, navigate here and wait again before capturing.
// Used for cookie-gated pages that have no gate of their own to sign into.

/** Playwright ships a headless shell; no need for a second browser download. */
function findChrome() {
  const fromEnv = process.env["BULLPEN_CHROME"];
  if (fromEnv !== undefined && existsSync(fromEnv)) return fromEnv;

  const root = join(
    process.env["LOCALAPPDATA"] ?? join(process.env["HOME"] ?? "", ".cache"),
    "ms-playwright",
  );
  if (!existsSync(root)) return null;

  for (const dir of readdirSync(root)) {
    for (const candidate of [
      join(root, dir, "chrome-headless-shell-win64", "chrome-headless-shell.exe"),
      join(root, dir, "chrome-win", "chrome.exe"),
      join(root, dir, "chrome-linux", "chrome"),
    ]) {
      if (existsSync(candidate)) return candidate;
    }
  }
  return null;
}

const chrome = findChrome();
if (chrome === null) {
  console.error("No headless chrome found. Set BULLPEN_CHROME to one, or npx playwright install.");
  process.exit(2);
}

// A fresh profile every run. A reused one leaves a lock behind after a killed
// run and the next launch silently waits forever.
const profile = mkdtempSync(join(tmpdir(), "bullpen-shot-"));

// Port 0: Chrome picks a free one and prints it, so two shots at once (two
// agents, two previews) never collide.
const child = spawn(
  chrome,
  [
    "--headless",
    "--disable-gpu",
    "--no-sandbox",
    `--user-data-dir=${profile}`,
    `--window-size=${SIZE}`,
    "--remote-debugging-port=0",
    "about:blank",
  ],
  { stdio: ["ignore", "ignore", "pipe"] },
);

let stderr = "";
child.stderr.on("data", (chunk) => (stderr += String(chunk)));

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const deadline = setTimeout(() => {
  child.kill();
  console.error(`timed out loading ${URL_ARG}`);
  process.exit(1);
}, 60_000);

async function debugPort() {
  for (let i = 0; i < 80; i++) {
    const m = /DevTools listening on ws:\/\/127\.0\.0\.1:(\d+)\//.exec(stderr);
    if (m) return Number(m[1]);
    await sleep(125);
  }
  throw new Error(`chrome never announced a devtools port:\n${stderr.split("\n").slice(-8).join("\n")}`);
}

async function pageTarget(port) {
  for (let i = 0; i < 40; i++) {
    try {
      const targets = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
      const page = targets.find((t) => t.type === "page");
      if (page) return page;
    } catch {
      // Not up yet.
    }
    await sleep(250);
  }
  throw new Error("chrome devtools endpoint listed no page");
}

function connect(wsUrl) {
  return new Promise((resolve, reject) => {
    const ws = new globalThis.WebSocket(wsUrl);
    ws.addEventListener("open", () => resolve(ws));
    ws.addEventListener("error", (e) => reject(new Error(`devtools socket failed: ${String(e)}`)));
  });
}

function cdp(ws) {
  let id = 0;
  const pending = new Map();
  ws.addEventListener("message", (event) => {
    const msg = JSON.parse(String(event.data));
    if (msg.id !== undefined && pending.has(msg.id)) {
      pending.get(msg.id)(msg);
      pending.delete(msg.id);
    }
  });
  return (method, params = {}) =>
    new Promise((resolve) => {
      const thisId = ++id;
      pending.set(thisId, resolve);
      ws.send(JSON.stringify({ id: thisId, method, params }));
    });
}

try {
  const port = await debugPort();
  const target = await pageTarget(port);
  const ws = await connect(target.webSocketDebuggerUrl);
  const send = cdp(ws);
  await send("Page.enable");
  await send("Runtime.enable");
  await send("Page.navigate", { url: URL_ARG });
  await sleep(WAIT_MS);

  if (EVAL !== undefined && EVAL !== "") {
    const res = await send("Runtime.evaluate", { expression: EVAL, awaitPromise: true });
    if (res.result?.exceptionDetails) {
      throw new Error(`BULLPEN_SHOT_EVAL threw: ${JSON.stringify(res.result.exceptionDetails)}`);
    }
    // Let React paint whatever the expression changed.
    await sleep(600);
  }

  // A second navigation on the SAME origin, after EVAL has signed in. The
  // VM screen (`/api/bots/:id/vm/view/`) is cookie-gated and has no sign-in
  // gate of its own, so shooting it cold captures {"error":"Sign in to
  // Bullpen."} - which is what happened the first time it was shot.
  const thenUrl = process.env["BULLPEN_SHOT_THEN_URL"];
  if (thenUrl !== undefined && thenUrl !== "") {
    await send("Page.navigate", { url: thenUrl });
    await sleep(Number(process.env["BULLPEN_SHOT_THEN_WAIT_MS"] ?? WAIT_MS));
  }

  const shot = await send("Page.captureScreenshot", { format: "png" });
  writeFileSync(OUT, Buffer.from(shot.result.data, "base64"));
  ws.close();
  console.log(`${OUT}  ${statSync(OUT).size} bytes  (${URL_ARG})`);
} catch (err) {
  console.error(err instanceof Error ? err.message : String(err));
  process.exitCode = 1;
} finally {
  clearTimeout(deadline);
  child.kill();
}
