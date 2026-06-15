#!/usr/bin/env node
const { spawnSync } = require("node:child_process");
const path = require("node:path");
const os = require("node:os");
const fs = require("node:fs");

const binaryName = os.type() === "Windows_NT" ? "basemind.exe" : "basemind";
const binaryPath = path.join(__dirname, binaryName);

if (!fs.existsSync(binaryPath)) {
  console.error(
    `basemind: native binary not found at ${binaryPath}.\n` +
      `The postinstall step that downloads the binary from GitHub releases may have failed.\n` +
      `Reinstall with: npm install -g basemind`,
  );
  process.exit(1);
}

const result = spawnSync(binaryPath, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  console.error(`basemind: failed to spawn binary: ${result.error.message}`);
  process.exit(1);
}
process.exit(result.status ?? 0);
