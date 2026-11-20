#!/usr/bin/env node
const { spawnSync } = require("node:child_process");
const path = require("node:path");
const os = require("node:os");
const fs = require("node:fs");

const binaryName = os.type() === "Windows_NT" ? "basemind.exe" : "basemind";
// BASEMIND_BIN runs another binary, such as a local build; BASEMIND_BINARY is its older name.
const override = process.env.BASEMIND_BIN || process.env.BASEMIND_BINARY;
const binaryPath = override || path.join(__dirname, binaryName);
const packageJsonPath = path.join(__dirname, "..", "package.json");

function ensureBinaryExists() {
  if (fs.existsSync(binaryPath)) {
    return true;
  }

  console.error(`basemind: binary not found at ${binaryPath}. Running install...`);

  const installScriptPath = path.join(__dirname, "..", "install.js");
  if (!fs.existsSync(installScriptPath)) {
    console.error(`basemind: install script not found at ${installScriptPath}`);
    return false;
  }

  const installResult = spawnSync(process.execPath, [installScriptPath], {
    stdio: "inherit",
    cwd: path.dirname(packageJsonPath),
  });

  if (installResult.status === 0 && fs.existsSync(binaryPath)) {
    return true;
  }

  return false;
}

if (!override && !ensureBinaryExists()) {
  console.error(
    `basemind: native binary not found at ${binaryPath}.\n` +
      `The postinstall step that downloads the binary from GitHub releases may have failed.\n` +
      `You can try:\n` +
      `  1. Reinstall: pnpm add -g basemind\n` +
      `  2. Install the binary another way: https://github.com/Goldziher/basemind#install-the-program`,
  );
  process.exit(1);
}

const result = spawnSync(binaryPath, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  console.error(`basemind: failed to spawn binary: ${result.error.message}`);
  process.exit(1);
}
if (result.signal) {
  console.error(`basemind: binary terminated by ${result.signal}`);
  process.exit(1);
}
process.exit(result.status ?? 1);
