/**
 * Checks the `basemind` binary on PATH against this package's version.
 *
 * Both OpenCode halves run `basemind` from PATH: the server half registers
 * `basemind serve` and the terminal half asks it for the agent inbox. The
 * skills name the tools of this package's version, so a binary from another
 * minor version answers "unknown tool", and a missing one gives a server that
 * never starts. This module has no OpenCode imports, so either half can load it.
 */

import { execFile, spawn } from "child_process";
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

/**
 * How a probe that outlived its timeout is stopped on `platform`: the
 * `taskkill` command for its process tree on Windows, where the probe is
 * `cmd.exe` running the `basemind.cmd` shim and killing `cmd.exe` alone leaves
 * the CLI it started running; `null` elsewhere, where the probe leads its own
 * process group and the whole group is killed: the npm wrapper waits on a
 * native child that inherits the output pipes, and killing the wrapper alone
 * would leave `execFile` waiting for them to close.
 */
export function treeKillInvocation(platform, pid) {
  return platform === "win32" ? { file: "taskkill", args: ["/pid", String(pid), "/T", "/F"] } : null;
}

// A read-only probe: a binary that ignores SIGTERM must not keep setup waiting past the timeout.
function stopProbe(child, platform, env) {
  const tree = treeKillInvocation(platform, child.pid);
  if (tree === null) {
    try {
      process.kill(-child.pid, "SIGKILL");
    } catch {
      child.kill("SIGKILL");
    }
    return;
  }
  execFile(tree.file, tree.args, { env }, (error) => {
    if (error) {
      child.kill("SIGKILL");
    }
  });
}

// `execFile` does not pass `detached` on, so the probe is spawned directly and reports what
// `execFile` would: an error whose `code` is the spawn error code or the non-zero exit status.
function runVersion(env, timeoutMs, platform) {
  const { file, args, shell } = versionInvocation(platform);
  return new Promise((resolve) => {
    let timedOut = false;
    let stdout = "";
    let spawnError = null;
    const child = spawn(file, args, { env, shell, detached: platform !== "win32", stdio: ["ignore", "pipe", "pipe"] });
    child.stdout.on("data", (chunk) => {
      if (stdout.length < 4096) {
        stdout += chunk;
      }
    });
    child.stderr.resume();
    child.on("error", (error) => {
      spawnError = error;
    });
    child.on("close", (code) => {
      clearTimeout(timer);
      const error =
        spawnError ??
        (code === 0 ? null : Object.assign(new Error(`Command failed: ${[file, ...args].join(" ")}`), { code }));
      resolve({ error, timedOut, notFound: isNotFound(error, shell), stdout });
    });
    const timer = setTimeout(() => {
      timedOut = true;
      stopProbe(child, platform, env);
    }, timeoutMs);
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
 * `platform` picks how the probe is spawned and stopped (`versionInvocation`,
 * `treeKillInvocation`); it defaults to this process's.
 */
export async function checkBinary({
  expected,
  env = process.env,
  timeoutMs = VERSION_TIMEOUT_MS,
  platform = process.platform,
}) {
  const install = installCommands(expected);
  const { error, timedOut, notFound, stdout } = await runVersion(env, timeoutMs, platform);
  if (notFound) {
    return {
      status: "missing",
      message: `basemind: no \`basemind\` binary on PATH, so the basemind MCP server is not registered. Install it with ${install}, then restart OpenCode.`,
    };
  }
  if (error) {
    const reason = timedOut ? `no answer within ${timeoutMs} ms` : error.message.split("\n")[0];
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
