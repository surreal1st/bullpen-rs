// VM smoke test against a RUNNING bullpen-rs server (default: meridian HTTPS :8452).
//
// Walks the path the API actually advertises, which is the only thing that
// has ever caught this class of bug: a unit test makes the same routing
// assumption the handler does. Before 9bc393a the advertised `viewPath`
// returned "Not Found".
//
//   node scripts/vm-smoke.mjs [baseUrl] [botId]
//
// The password is read HERE from %USERPROFILE%\.bullpen\password.key and
// only ever travels in a request body. Neither it nor the session token is
// printed - the token's LENGTH is, so a broken login is still diagnosable.
//
// It checks more than the status code on purpose: S6-W-06 was a 200 whose
// body was raw gzip with the header that said so stripped, so the API looked
// perfect and the browser showed mojibake. See the viewPath step.
//
// The verdict is the EXIT CODE. It never calls process.exit() mid-request:
// on Windows that aborts node (libuv assert) and the exit code stops meaning
// anything. Failures throw, main catches, and the loop drains.
import { readFileSync } from "node:fs";
import { join } from "node:path";

const base = (process.argv[2] ?? "https://meridian.tail74afb5.ts.net:8452").replace(/\/$/, "");
const botId = process.argv[3];

const short = (s, n = 400) => (s.length > n ? `${s.slice(0, n)}… [${s.length} bytes]` : s);

async function step(name, fn) {
  console.log(`\n== ${name}`);
  return await fn();
}

async function main() {
  if (!botId) throw new Error("Pass an explicit test bot ID: node scripts/vm-smoke.mjs [baseUrl] [botId]");
  const pw = readFileSync(join(process.env.USERPROFILE, ".bullpen", "password.key"), "utf8").replace(/\r?\n$/, "");
  if (!pw) throw new Error("password.key is empty (name only, never the value)");

  const token = await step("POST /api/auth/login", async () => {
    const r = await fetch(`${base}/api/auth/login`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ password: pw }),
    });
    const text = await r.text();
    console.log(`status ${r.status}`);
    if (r.status !== 200) {
      console.log(`body ${short(text)}`);
      throw new Error(`login answered ${r.status}`);
    }
    const t = JSON.parse(text).token ?? "";
    console.log(`ok, token length ${t.length}`);
    if (!t) throw new Error("login returned no token");
    return t;
  });

  const auth = { authorization: `Bearer ${token}` };

  await step(`GET /api/bots/${botId}/vm  (state BEFORE ensure)`, async () => {
    const r = await fetch(`${base}/api/bots/${botId}/vm`, { headers: auth });
    console.log(`status ${r.status}`);
    console.log(short(await r.text()));
  });

  const ensured = await step(`POST /api/bots/${botId}/vm/ensure`, async () => {
    const r = await fetch(`${base}/api/bots/${botId}/vm/ensure`, { method: "POST", headers: auth });
    const text = await r.text();
    console.log(`status ${r.status}`);
    console.log(short(text));
    if (r.status !== 200) throw new Error("ensure did not return 200 - the VM did not come up");
    return JSON.parse(text);
  });

  const viewPath = ensured.viewPath;
  if (!viewPath) throw new Error("ensure returned no viewPath - nothing to walk");

  // `accept-encoding: gzip` on purpose - it is what every browser sends, and
  // it is the request shape that exposed S6-W-06. `fetch` would send it
  // anyway; saying so here is the point of the test.
  await step(`GET ${viewPath}  (the advertised path, verbatim, accept-encoding: gzip)`, async () => {
    const r = await fetch(`${base}${viewPath}`, {
      headers: { ...auth, "accept-encoding": "gzip" },
      redirect: "manual",
    });
    const encoding = r.headers.get("content-encoding");
    console.log(`status ${r.status}`);
    console.log(`content-type ${r.headers.get("content-type") ?? "(none)"}`);
    console.log(`content-encoding ${encoding ?? "(none)"}`);
    if (r.status === 404) throw new Error("REGRESSION: the path this API advertises is a 404 again");
    if (r.status >= 400) throw new Error(`viewPath answered ${r.status}`);

    // 🔴 The bug a status code cannot see (S6-W-06). The proxy relays the
    // container's gzip body without decompressing it; if it also drops the
    // header that says so, the browser renders the compressed bytes as text
    // and the VM screen is a window of mojibake. A 200 looks perfect here.
    //
    // `fetch` decompresses for us when the header is present, so the check
    // has to be: did the header survive, and do the bytes we were handed
    // still look compressed?
    const raw = Buffer.from(await r.arrayBuffer());
    const looksGzip = raw[0] === 0x1f && raw[1] === 0x8b;
    console.log(`first bytes ${raw.subarray(0, 4).toString("hex")}  len ${raw.length}`);
    console.log(short(raw.toString("utf8"), 200));
    if (looksGzip && encoding === null) {
      throw new Error(
        "REGRESSION: a gzip body with no content-encoding header - every browser will render this as text",
      );
    }
    if (!raw.toString("utf8", 0, 200).toLowerCase().includes("<!doctype") && !looksGzip) {
      throw new Error("the VM screen did not answer with HTML - look at the bytes above");
    }
  });

  console.log("\nSMOKE OK");
}

try {
  await main();
} catch (e) {
  console.error(`\nSMOKE FAILED: ${e.message}`);
  process.exitCode = 1;
}
