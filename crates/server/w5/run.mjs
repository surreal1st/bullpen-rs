#!/usr/bin/env node
/**
 * S10-09: thin CLI so bullpen-rs can call W5 logic from bullpen-night
 * without vendoring 1300 lines of TypeScript. Requires:
 *   - `BULLPEN_NIGHT_ROOT` (defaults to ../../bullpen-night from this file)
 *   - bullpen-night's `node_modules` (typescript, tsx)
 *
 * stdin: one JSON object per invocation
 * stdout: one JSON object (or { "error": "..." } on failure)
 */
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { readFileSync } from "node:fs";
import { createInterface } from "node:readline";

const here = dirname(fileURLToPath(import.meta.url));
const nightRoot =
  process.env.BULLPEN_NIGHT_ROOT ?? join(here, "../../../../bullpen-night");
const require = createRequire(join(nightRoot, "package.json"));

async function loadBotTools() {
  const mod = await import(join(nightRoot, "src/server/bot-tools.ts"));
  return mod;
}

function sandboxStub() {
  return {
    check: () => Promise.resolve({ ok: true, detail: "fake" }),
    exec: () =>
      Promise.resolve({
        stdout: "",
        stderr: "",
        exitCode: 0,
        timedOut: false,
        truncated: false,
      }),
  };
}

function openMemoryDb() {
  const { openDb } = require(join(nightRoot, "src/server/db.ts"));
  return openDb(":memory:");
}

async function main() {
  const op = process.argv[2];
  if (!op) {
    process.stdout.write(JSON.stringify({ error: "missing op" }));
    process.exit(1);
  }

  let payload = {};
  const rl = createInterface({ input: process.stdin });
  const lines = [];
  for await (const line of rl) lines.push(line);
  const text = lines.join("\n").trim();
  if (text) {
    try {
      payload = JSON.parse(text);
    } catch {
      process.stdout.write(JSON.stringify({ error: "invalid json on stdin" }));
      process.exit(1);
    }
  }

  const botTools = await loadBotTools();

  try {
    if (op === "guard") {
      const source = String(payload.source ?? "");
      const verdict = botTools.guardSource(source);
      process.stdout.write(JSON.stringify(verdict));
      return;
    }

    if (op === "prepare") {
      const { openDb } = await import(join(nightRoot, "src/server/db.ts"));
      const dbPath = payload.dbPath;
      const db = dbPath ? openDb(dbPath) : openMemoryDb();
      botTools.ensureBotToolTables(db);
      const deps = {
        db,
        botId: String(payload.botId ?? ""),
        sandbox: sandboxStub(),
        ...(payload.timeoutMs ? { timeoutMs: payload.timeoutMs } : {}),
      };
      const proposal = await botTools.prepareProposal(
        deps,
        String(payload.dataDir ?? ""),
        payload.args ?? {},
      );
      if (payload.persistPath && dbPath) {
        // proposal already written by prepareProposal into dbPath
      } else if (!dbPath && payload.dbPath === undefined) {
        // in-memory only — caller must not rely on row persistence
      }
      process.stdout.write(JSON.stringify({ proposal }));
      return;
    }

    if (op === "prepare-persist") {
      const dbPath = String(payload.dbPath ?? "");
      const { openDb } = await import(join(nightRoot, "src/server/db.ts"));
      const db = openDb(dbPath);
      botTools.ensureBotToolTables(db);
      const deps = {
        db,
        botId: String(payload.botId ?? ""),
        sandbox: sandboxStub(),
      };
      const proposal = await botTools.prepareProposal(
        deps,
        String(payload.dataDir ?? ""),
        payload.args ?? {},
      );
      process.stdout.write(JSON.stringify({ proposal }));
      return;
    }

    if (op === "describe") {
      const proposal = payload.proposal;
      const text = botTools.describeProposal(proposal);
      process.stdout.write(JSON.stringify({ text }));
      return;
    }

    if (op === "run-tool") {
      const dbPath = String(payload.dbPath ?? "");
      const { openDb } = await import(join(nightRoot, "src/server/db.ts"));
      const db = openDb(dbPath);
      botTools.ensureBotToolTables(db);
      const deps = {
        db,
        botId: String(payload.botId ?? ""),
        sandbox: sandboxStub(),
      };
      const result = await botTools.runBotTool(
        deps,
        String(payload.name ?? ""),
        payload.args ?? {},
      );
      process.stdout.write(JSON.stringify({ result }));
      return;
    }

    if (op === "approved-specs") {
      const dbPath = String(payload.dbPath ?? "");
      const { openDb } = await import(join(nightRoot, "src/server/db.ts"));
      const db = openDb(dbPath);
      botTools.ensureBotToolTables(db);
      const specs = botTools.approvedToolSpecs(db);
      process.stdout.write(JSON.stringify({ specs }));
      return;
    }

    process.stdout.write(JSON.stringify({ error: `unknown op ${op}` }));
    process.exit(1);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    process.stdout.write(JSON.stringify({ error: message }));
    process.exit(1);
  }
}

main();
