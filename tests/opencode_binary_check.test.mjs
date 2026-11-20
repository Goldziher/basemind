// The OpenCode plugin checks the `basemind` on PATH before registering its MCP server: a missing or
// unusable binary is not registered, one from another minor version is, and each names the install
// command in one line.
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { test } from "node:test";

import {
  checkBinary,
  isNotFound,
  packageVersion,
  versionInvocation,
  versionMinor,
} from "../opencode-plugin/binary-check.js";

const posix = { skip: process.platform === "win32" && "the stub binary is a POSIX shell script" };

function pathWith(script) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "basemind-opencode-bin-"));
  if (script !== null) {
    fs.writeFileSync(path.join(dir, "basemind"), `#!/bin/sh\n${script}\n`, { mode: 0o755 });
  }
  return { dir, env: { ...process.env, PATH: dir } };
}

async function check(script, options = {}) {
  const { dir, env } = pathWith(script);
  try {
    return await checkBinary({ expected: "0.33.0", env, ...options });
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

const install = "`pnpm add -g basemind@0.33.0` (or `pip install basemind==0.33.0`)";

test("the expected version is the package's own", () => {
  const manifest = JSON.parse(fs.readFileSync(new URL("../opencode-plugin/package.json", import.meta.url)));
  assert.equal(packageVersion(), manifest.version);
});

for (const [version, minor] of [
  ["0.33.0", "0.33"],
  ["0.33.12", "0.33"],
  ["1.4.0-rc.2", "1.4"],
  ["garbage", null],
]) {
  test(`the minor version of ${version} is ${minor}`, () => {
    assert.equal(versionMinor(version), minor);
  });
}

test("no binary on PATH is reported as missing with the install command", posix, async () => {
  assert.deepEqual(await check(null), {
    status: "missing",
    message: `basemind: no \`basemind\` binary on PATH, so the basemind MCP server is not registered. Install it with ${install}, then restart OpenCode.`,
  });
});

test("a binary from another minor version is reported with the upgrade command", posix, async () => {
  assert.deepEqual(await check("echo 'basemind 0.32.4'"), {
    status: "mismatch",
    message: `basemind: the \`basemind\` binary on PATH reports 0.32.4, but basemind-opencode 0.33.0 needs 0.33.x, so its tools may not match the skills. Upgrade it with ${install}, then restart OpenCode.`,
  });
});

test("a binary that prints no version is reported as a mismatch", posix, async () => {
  assert.deepEqual(await check("echo 'something else'"), {
    status: "mismatch",
    message: `basemind: the \`basemind\` binary on PATH reports no version, but basemind-opencode 0.33.0 needs 0.33.x, so its tools may not match the skills. Upgrade it with ${install}, then restart OpenCode.`,
  });
});

for (const version of ["0.33.0", "0.33.7"]) {
  test(`a ${version} binary matches a 0.33.0 package`, posix, async () => {
    assert.deepEqual(await check(`echo 'basemind ${version}'`), { status: "ok", message: null });
  });
}

test("a binary that exits non-zero is not registered", posix, async () => {
  assert.deepEqual(await check("exit 3"), {
    status: "missing",
    message: `basemind: \`basemind --version\` failed (Command failed: basemind --version), so the basemind MCP server is not registered. Reinstall it with ${install}, then restart OpenCode.`,
  });
});

test("a binary that hangs is stopped and not registered", posix, async () => {
  assert.deepEqual(await check("exec /bin/sleep 10", { timeoutMs: 200 }), {
    status: "missing",
    message: `basemind: \`basemind --version\` failed (no answer within 200 ms), so the basemind MCP server is not registered. Reinstall it with ${install}, then restart OpenCode.`,
  });
});

test("a binary that ignores SIGTERM is killed at the timeout and not registered", posix, async () => {
  assert.deepEqual(await check("trap '' TERM\nexec /bin/sleep 10", { timeoutMs: 200 }), {
    status: "missing",
    message: `basemind: \`basemind --version\` failed (no answer within 200 ms), so the basemind MCP server is not registered. Reinstall it with ${install}, then restart OpenCode.`,
  });
});

for (const [platform, invocation] of [
  ["win32", { file: "basemind --version", args: [], shell: true }],
  ["linux", { file: "basemind", args: ["--version"], shell: false }],
  ["darwin", { file: "basemind", args: ["--version"], shell: false }],
]) {
  test(`on ${platform} the version probe runs ${invocation.shell ? "through" : "without"} a shell`, () => {
    assert.deepEqual(versionInvocation(platform), invocation);
  });
}

for (const [error, shell, notFound] of [
  [{ code: "ENOENT" }, false, true],
  [{ code: "ENOENT" }, true, true],
  [{ code: 9009 }, true, true],
  [{ code: 9009 }, false, false],
  [{ code: 1 }, true, false],
  [null, true, false],
]) {
  test(`a spawn error ${JSON.stringify(error)} with shell ${shell} is ${notFound ? "" : "not "}a missing binary`, () => {
    assert.equal(isNotFound(error, shell), notFound);
  });
}
