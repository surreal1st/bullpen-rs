// VM smoke test against a RUNNING bullpen-rs server (default: meridian :4380).
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
// The verdict is the EXIT CODE. It never calls process.exit() mid-request:
// on Windows that aborts node (libuv assert) and the exit code stops meaning
// anything. Failures throw, main catches, and the loop drains.
import { readFileSync } from "node:fs";
import { join } from "node:path";

const base = (process.argv[2] ?? "http://100.119.100.103:4380").replace(/\/$/, "");
const botId = process.argv[3] ?? "dora";

const short = (s, n = 400) => (s.length > n ? `${s.slice(0, n)}… [${s.length} bytes]` : s);

async function step(name, fn) {
  console.log(`\n== ${name}`);
  return await fn();
}

async function main() {
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

  await step(`GET ${viewPath}  (the advertised path, verbatim)`, async () => {
    const r = await fetch(`${base}${viewPath}`, { headers: auth, redirect: "manual" });
    console.log(`status ${r.status}`);
    console.log(`content-type ${r.headers.get("content-type") ?? "(none)"}`);
    console.log(short(await r.text(), 300));
    if (r.status === 404) throw new Error("REGRESSION: the path this API advertises is a 404 again");
    if (r.status >= 400) throw new Error(`viewPath answered ${r.status}`);
  });

  console.log("\nSMOKE OK");
}

try {
  await main();
} catch (e) {
  console.error(`\nSMOKE FAILED: ${e.message}`);
  process.exitCode = 1;
}
