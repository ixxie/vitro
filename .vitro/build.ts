// Vitro's dogfooding flow (implementation).
//
// Launched by .vitro/flows/build.sh (a bash wrapper that establishes
// the nix-shell). Bun rejects multi-line nix-shell `#!` directives —
// only line 1 may be a shebang in JS/TS — so the wrapper-per-
// interpreter idiom is the standard workaround here.
//
// Runs YOLO inside the cell — full Pi tool access. The cell IS the
// sandbox, so the agent reads/writes/execs freely.
//
// Pi calls OpenRouter via .vitro/pi/models.json. The proxy injects the
// real OPENROUTER_API_KEY at egress; the cell only ever sees a
// placeholder.
//
// Env from `vitro run`:
//   VITRO_CELL, VITRO_BRANCH, VITRO_REPO, VITRO_SERVER
//   VITRO_PARAM_TASK   — task description (vitro run -- task="...")
//   VITRO_PARAM_MODEL  — OpenRouter model id, e.g. "deepseek/deepseek-v4-pro"
//                        (no openrouter/ prefix — that's a Pi-docs artifact;
//                        OpenRouter's catalog uses <vendor>/<name>)

import { mkdirSync, writeFileSync, readFileSync, existsSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { homedir } from "node:os";

const cell = process.env.VITRO_CELL ?? "unknown";
const repo = process.env.VITRO_REPO ?? "vitro";
const repoRoot = `/${repo}`;
const task = process.env.VITRO_PARAM_TASK
  ?? "Read the latest commit log and propose the next small improvement.";
const modelId = process.env.VITRO_PARAM_MODEL ?? "deepseek/deepseek-v4-pro";

console.log(`[vitro/build] cell=${cell} repo=${repo} model=${modelId}`);

// Stage repo's pi config into Pi's expected location, auto-registering
// the runtime model if the user didn't add it to the committed file.
// Mutations stay in the cell — the repo file is read-only here.
const piConfigDir = `${homedir()}/.pi/agent`;
mkdirSync(piConfigDir, { recursive: true });

const repoModelsJson = `${repoRoot}/.vitro/pi/models.json`;
const config: any = existsSync(repoModelsJson)
  ? JSON.parse(readFileSync(repoModelsJson, "utf8"))
  : { providers: {} };

config.providers ??= {};
config.providers.openrouter ??= {
  baseUrl: "https://openrouter.ai/api/v1",
  apiKey: "OPENROUTER_API_KEY",
  api: "openai-completions",
  models: [],
};
const openrouter = config.providers.openrouter;
openrouter.models ??= [];
if (!openrouter.models.some((m: any) => m.id === modelId)) {
  openrouter.models.push({
    id: modelId,
    name: modelId,
    input: ["text"],
    contextWindow: 200000,
    maxTokens: 16384,
  });
}
writeFileSync(`${piConfigDir}/models.json`, JSON.stringify(config, null, 2));

// Pi requires *some* value even though the proxy is what authenticates.
// Anything non-empty works; the proxy rewrites Authorization at egress.
process.env.OPENROUTER_API_KEY = process.env.OPENROUTER_API_KEY ?? "vitro-proxy-placeholder";

const install = spawnSync("bun", ["add", "-g", "@earendil-works/pi-coding-agent"], { stdio: "inherit" });
if (install.status !== 0) {
  console.error("[vitro/build] failed to install pi-coding-agent");
  process.exit(install.status ?? 1);
}

const { createAgentSession, SessionManager, AuthStorage, ModelRegistry } =
  await import("@earendil-works/pi-coding-agent");

const authStorage = AuthStorage.create();
const modelRegistry = ModelRegistry.create(authStorage);

const model = modelRegistry.find("openrouter", modelId);
if (!model) {
  console.error(`[vitro/build] model "${modelId}" not registered after staging — Pi may not have re-read models.json`);
  process.exit(1);
}

console.log(`[vitro/build] task: ${task}\n`);

const { session } = await createAgentSession({
  model,
  authStorage,
  modelRegistry,
  sessionManager: SessionManager.inMemory(),
  cwd: repoRoot,
});

// Stream everything Pi emits so the run is self-documenting, but
// suppress noisy progress events that fire many times per tool call.
session.subscribe((event: any) => {
  const t = event?.type;
  // partials during long tools — emit one line per tool start, skip the rest
  if (t === "tool_execution_update") return;
  // assistant message text deltas → inline stream
  const ame = event?.assistantMessageEvent;
  if (t === "message_update" && ame?.type === "text_delta") {
    process.stdout.write(ame.delta);
    return;
  }
  // tool boundaries — one line each
  if (t === "tool_execution_start") {
    process.stderr.write(`\n[tool ${event.toolName} start] ${JSON.stringify(event.args ?? {}).slice(0, 300)}\n`);
    return;
  }
  if (t === "tool_execution_end") {
    const r = event.result?.content?.[0]?.text ?? JSON.stringify(event.result ?? {});
    const trimmed = typeof r === "string" ? r.trim() : String(r);
    process.stderr.write(`[tool ${event.toolName} end] ${trimmed.slice(0, 200)}${trimmed.length > 200 ? " …" : ""}\n`);
    return;
  }
  // turn boundaries are useful, message_update events without text are noise
  if (t === "turn_start" || t === "turn_end" || t === "agent_start" || t === "agent_end") {
    process.stderr.write(`[${t}]\n`);
  }
});

console.log("[vitro/build] sending prompt …\n");
await session.prompt(task);
console.log("\n[vitro/build] prompt returned");

// Validation loop — re-prompt the agent if cargo check fails.
const maxAttempts = 3;
for (let attempt = 1; attempt <= maxAttempts; attempt++) {
  const check = spawnSync("cargo", ["check", "--quiet"], { cwd: repoRoot, stdio: "pipe" });
  if (check.status === 0) break;
  // spawn failure (e.g. cargo not on PATH) gives status=null, stderr=null
  const detail = check.error
    ? `spawn error: ${check.error.message}`
    : (check.stderr?.toString() ?? `cargo check exited with status ${check.status}`);
  if (attempt === maxAttempts) {
    console.error(`\n[vitro/build] cargo check still failing after retries\n${detail}`);
    process.exit(1);
  }
  await session.followUp(
    `\`cargo check\` failed:\n\`\`\`\n${detail}\n\`\`\`\nFix the errors.`,
  );
}

// Done. The agent's commits are already in grove's bare repo (virtiofs
// mount), so `git fetch vitro && git log vitro/<cell>` from the laptop
// will see them. Pushing to upstream is the human reviewer's call,
// not the flow's.
console.log(`\n[vitro/build] flow finished. \`git fetch vitro && git log vitro/${cell}\``);
process.exit(0);
