// Signs the gate in with the desktop password file, then takes the shot.
// Use it for shots against the SHIPPED server (real db): the password is
// read HERE and reaches the page only through the child env (BULLPEN_SHOT_EVAL),
// never argv.  node scripts/shot-signed-in.mjs <url> <out.png> ["<extra JS run after sign-in>"]
import { readFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
const [url, out, extraEval] = process.argv.slice(2);
const pw = readFileSync(join(process.env.USERPROFILE, ".bullpen", "password.key"), "utf8").replace(/\r?\n$/, "");
if (!pw) { console.error("password.key empty (name only)"); process.exit(1); }
const login = `(async () => {
  const sleep = ms => new Promise(r => setTimeout(r, ms));
  const input = document.querySelector('#gate-password');
  if (!input) return 'no-gate';
  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(input, ${JSON.stringify(pw)});
  input.dispatchEvent(new Event('input', { bubbles: true }));
  const btn = document.querySelector('.gate-button');
  for (let i = 0; i < 50 && btn.disabled; i++) await sleep(100);
  btn.click();
  await sleep(7000);
  ${extraEval ?? ""}
  return 'signed-in';
})()`;
const r = spawnSync("node", ["scripts/shot.mjs", url, out], {
  cwd: fileURLToPath(new URL("..", import.meta.url)),
  env: { ...process.env, BULLPEN_SHOT_EVAL: login, BULLPEN_SHOT_WAIT_MS: "5000" },
  stdio: "inherit",
});
process.exit(r.status ?? 1);
