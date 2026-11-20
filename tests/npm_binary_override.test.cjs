// The npm wrapper runs the binary BASEMIND_BIN names, or BASEMIND_BINARY when that is unset, instead
// of the one its installer downloads.
const assert = require("node:assert/strict");
const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { test } = require("node:test");

const WRAPPER = path.join(__dirname, "..", "npm-package", "bin", "basemind.js");
const unix = { skip: os.type() === "Windows_NT" && "the stand-in binaries are shell scripts" };

function standIn(root, name) {
  const binary = path.join(root, name);
  fs.writeFileSync(binary, `#!/bin/sh\nprintf '%s %s' ${name} "$*"\n`, { mode: 0o755 });
  return binary;
}

function run(pins) {
  const env = { ...process.env };
  delete env.BASEMIND_BIN;
  delete env.BASEMIND_BINARY;
  const result = spawnSync(process.execPath, [WRAPPER, "status", "--json"], {
    env: { ...env, ...pins },
    encoding: "utf8",
  });
  assert.equal(result.status, 0, result.stderr);
  return result.stdout;
}

function withRoot(body) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "basemind-npm-override-"));
  try {
    body(root);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
}

test("BASEMIND_BIN runs its binary with the arguments", unix, () => {
  withRoot((root) => {
    assert.equal(run({ BASEMIND_BIN: standIn(root, "new") }), "new status --json");
  });
});

test("BASEMIND_BINARY runs its binary when BASEMIND_BIN is unset", unix, () => {
  withRoot((root) => {
    assert.equal(run({ BASEMIND_BINARY: standIn(root, "old") }), "old status --json");
  });
});

test("BASEMIND_BIN wins over BASEMIND_BINARY", unix, () => {
  withRoot((root) => {
    const pins = { BASEMIND_BIN: standIn(root, "new"), BASEMIND_BINARY: standIn(root, "old") };
    assert.equal(run(pins), "new status --json");
  });
});
