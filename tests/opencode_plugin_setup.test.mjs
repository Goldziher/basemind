// The OpenCode server half registers `basemind serve` only when a usable `basemind` is on PATH, prints
// the binary check's line once otherwise, and leaves a user's own `basemind` MCP entry alone; the
// terminal half shows that line as one toast. `@opencode/plugin` is stubbed so both halves load
// without OpenCode installed.
import assert from "node:assert/strict";
import fs from "node:fs";
import { registerHooks } from "node:module";
import os from "node:os";
import path from "node:path";
import { test } from "node:test";
import { setTimeout as sleep } from "node:timers/promises";

registerHooks({
  resolve(specifier, context, nextResolve) {
    if (specifier === "@opencode/plugin" || specifier === "@opencode/plugin/tui") {
      return { url: "data:text/javascript,export const Plugin = { define: (plugin) => plugin };", shortCircuit: true };
    }
    return nextResolve(specifier, context);
  },
});

const { default: plugin } = await import("../opencode-plugin/basemind.js");
const { default: tui } = await import("../opencode-plugin/tui.js");
const { packageVersion } = await import("../opencode-plugin/binary-check.js");

const posix = { skip: process.platform === "win32" && "the stub binary is a POSIX shell script" };
const version = packageVersion();
const minor = /^(\d+\.\d+)\./u.exec(version)[1];
const install = `\`pnpm add -g basemind@${version}\` (or \`pip install basemind==${version}\`)`;

function editor(entries) {
  return {
    get: (name) => entries.get(name),
    set: (name, config) => entries.set(name, config),
    add: (skill) => entries.set(skill.id, skill),
  };
}

function stubPath(script) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "basemind-opencode-setup-"));
  if (script !== null) {
    fs.writeFileSync(path.join(dir, "basemind"), `#!/bin/sh\n${script}\n`, { mode: 0o755 });
  }
  return dir;
}

async function setup(script, existing = new Map()) {
  const dir = stubPath(script);
  const savedPath = process.env.PATH;
  const savedError = console.error;
  const errors = [];
  const mcp = existing;
  process.env.PATH = dir;
  console.error = (line) => errors.push(line);
  try {
    await plugin.setup({
      mcp: { transform: async (apply) => apply(editor(mcp)) },
      skill: { transform: async (apply) => apply(editor(new Map())) },
    });
  } finally {
    process.env.PATH = savedPath;
    console.error = savedError;
    fs.rmSync(dir, { recursive: true, force: true });
  }
  return { mcp: Object.fromEntries(mcp), errors };
}

const served = { basemind: { type: "local", command: ["basemind", "serve"] } };

test("no binary on PATH registers no server and prints one install line", posix, async () => {
  assert.deepEqual(await setup(null), {
    mcp: {},
    errors: [
      `basemind: no \`basemind\` binary on PATH, so the basemind MCP server is not registered. Install it with ${install}, then restart OpenCode.`,
    ],
  });
});

test("a binary from another minor version is registered with one upgrade line", posix, async () => {
  assert.deepEqual(await setup("echo 'basemind 0.0.1'"), {
    mcp: served,
    errors: [
      `basemind: the \`basemind\` binary on PATH reports 0.0.1, but basemind-opencode ${version} needs ${minor}.x, so its tools may not match the skills. Upgrade it with ${install}, then restart OpenCode.`,
    ],
  });
});

test("a binary of the package's version is registered silently", posix, async () => {
  assert.deepEqual(await setup(`echo 'basemind ${version}'`), { mcp: served, errors: [] });
});

test("a user's own basemind entry is kept and nothing is printed", posix, async () => {
  const own = { type: "local", command: ["/opt/basemind", "serve"] };
  assert.deepEqual(await setup(null, new Map([["basemind", own]])), { mcp: { basemind: own }, errors: [] });
});

// Wait for the first toast: up to the check's own timeout when one is expected, a second otherwise.
async function firstToast(shown, expected) {
  const deadline = Date.now() + (expected ? 6000 : 1000);
  while (shown.length === 0 && Date.now() < deadline) {
    await sleep(20);
  }
}

async function toasts(script, expected) {
  const dir = stubPath(script);
  const savedPath = process.env.PATH;
  const shown = [];
  process.env.PATH = dir;
  try {
    const cleanup = tui.setup({
      location: { directory: dir },
      ui: { toast: { show: (toast) => shown.push(toast) } },
      data: { on: () => () => {} },
    });
    await firstToast(shown, expected);
    cleanup();
  } finally {
    process.env.PATH = savedPath;
    fs.rmSync(dir, { recursive: true, force: true });
  }
  return shown;
}

test("the terminal half shows a missing binary as one error toast", posix, async () => {
  assert.deepEqual(await toasts(null, true), [
    {
      title: "basemind",
      message: `basemind: no \`basemind\` binary on PATH, so the basemind MCP server is not registered. Install it with ${install}, then restart OpenCode.`,
      variant: "error",
    },
  ]);
});

test("the terminal half shows another minor version as one warning toast", posix, async () => {
  assert.deepEqual(await toasts("echo 'basemind 0.0.1'", true), [
    {
      title: "basemind",
      message: `basemind: the \`basemind\` binary on PATH reports 0.0.1, but basemind-opencode ${version} needs ${minor}.x, so its tools may not match the skills. Upgrade it with ${install}, then restart OpenCode.`,
      variant: "warning",
    },
  ]);
});

test("the terminal half shows nothing for the package's version", posix, async () => {
  assert.deepEqual(await toasts(`echo 'basemind ${version}'`, false), []);
});
