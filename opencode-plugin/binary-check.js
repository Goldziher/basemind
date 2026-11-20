/**
 * Checks the `basemind` binary on PATH against this package's version.
 *
 * Both OpenCode halves run `basemind` from PATH: the server half registers
 * `basemind serve` and the terminal half asks it for the agent inbox. The
 * skills name the tools of this package's version, so a binary from another
 * minor version answers "unknown tool", and a missing one gives a server that
 * never starts. This module has no OpenCode imports, so either half can load it.
 */

import { execFile } from "child_process";
import fs from "fs";
import path from "path";
import { fileURLToPath } from "url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

/** Milliseconds `basemind --version` may take before the binary counts as unusable. */
export const VERSION_TIMEOUT_MS = 5000;

/** The version of this package, read from its `package.json`. */
export function packageVersion() {
  return JSON.parse(fs.readFileSync(path.join(__dirname, "package.json"), "utf8")).version;
}

/** The `major.minor` of a version string (`0.33.1` and `0.33.0-rc.2` give `0.33`), or `null`. */
export function versionMinor(version) {
  return /^(\d+\.\d+)\./u.exec(version)?.[1] ?? null;
}

function installCommands(version) {
  return `\`pnpm add -g basemind@${version}\` (or \`pip install basemind==${version}\`)`;
}

/** The exit status `cmd.exe` gives a command it cannot find on PATH. */
const CMD_NOT_FOUND = 9009;

/**
 * How `basemind --version` is spawned on `platform`. Windows goes through the
 * shell: a `pnpm add -g` install puts `basemind.cmd` and `basemind.ps1` shims on
 * PATH, which only a shell resolves. The command line is fixed; no input reaches it.
 */
export function versionInvocation(platform) {
  const shell = platform === "win32";
  return { file: shell ? "basemind --version" : "basemind", args: shell ? [] : ["--version"], shell };
}

/** Whether a failed spawn means there is no `basemind` on PATH. */
export function isNotFound(error, shell) {
  return error?.code === "ENOENT" || (shell && error?.code === CMD_NOT_FOUND);
}

function runVersion(env, timeoutMs) {
  const { file, args, shell } = versionInvocation(process.platform);
  // A read-only probe: a binary that ignores SIGTERM must not keep setup waiting past the timeout.
  const options = { env, timeout: timeoutMs, killSignal: "SIGKILL", shell };
  return new Promise((resolve) => {
    execFile(file, args, options, (error, stdout) => {
      resolve({ error, notFound: isNotFound(error, shell), stdout: String(stdout ?? "") });
    });
  });
}

/**
 * Run `basemind --version` from PATH and compare its minor version with
 * `expected`. Never rejects. Resolves to `{ status, message }`:
 *
 * - `"ok"`: the binary's minor version matches; `message` is `null`.
 * - `"missing"`: there is no `basemind` on PATH, or it cannot report a
 *   version; the MCP server must not be registered.
 * - `"mismatch"`: the binary is from another minor version; it still runs,
 *   but its tools may not match the skills.
 *
 * `message` is the one line to show the user, naming the install command.
 */
export async function checkBinary({ expected, env = process.env, timeoutMs = VERSION_TIMEOUT_MS }) {
  const install = installCommands(expected);
  const { error, notFound, stdout } = await runVersion(env, timeoutMs);
  if (notFound) {
    return {
      status: "missing",
      message: `basemind: no \`basemind\` binary on PATH, so the basemind MCP server is not registered. Install it with ${install}, then restart OpenCode.`,
    };
  }
  if (error) {
    const reason = error.killed ? `no answer within ${timeoutMs} ms` : error.message.split("\n")[0];
    return {
      status: "missing",
      message: `basemind: \`basemind --version\` failed (${reason}), so the basemind MCP server is not registered. Reinstall it with ${install}, then restart OpenCode.`,
    };
  }
  const found = /^basemind\s+(\S+)/u.exec(stdout.trim())?.[1] ?? null;
  if (found !== null && versionMinor(found) === versionMinor(expected)) {
    return { status: "ok", message: null };
  }
  return {
    status: "mismatch",
    message: `basemind: the \`basemind\` binary on PATH reports ${found ?? "no version"}, but basemind-opencode ${expected} needs ${versionMinor(expected)}.x, so its tools may not match the skills. Upgrade it with ${install}, then restart OpenCode.`,
  };
}
