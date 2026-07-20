import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";

const RELEASES_LATEST_URL = "https://github.com/Goldziher/basemind/releases/latest";
const LATEST_REQUEST_TIMEOUT_MS = 10_000;
const VERSION_PATTERN = /^v?(\d+\.\d+\.\d+(?:-rc\.\d+)?)$/;
const LAUNCHER_PATH = path.join(path.dirname(fileURLToPath(import.meta.url)), "mcp-launch.sh");

function latestVersionFromUrl(url) {
  const match = url.match(/\/releases\/tag\/([^/?#]+)$/);
  if (!match) {
    return undefined;
  }

  return VERSION_PATTERN.exec(match[1])?.[1];
}

async function resolveLatestVersion() {
  try {
    const response = await fetch(RELEASES_LATEST_URL, {
      redirect: "follow",
      signal: AbortSignal.timeout(LATEST_REQUEST_TIMEOUT_MS),
    });
    if (!response.ok) {
      throw new Error(`latest release returned HTTP ${response.status}`);
    }

    return latestVersionFromUrl(response.url);
  } catch (error) {
    const detail = error instanceof Error ? error.message : String(error);
    process.stderr.write(`basemind Codex launcher: could not resolve latest release: ${detail}\n`);
    return undefined;
  }
}

function launchEnvironment(version) {
  return version ? { ...process.env, BASEMIND_FORCE_VERSION: version } : process.env;
}

export async function run(serverArgs) {
  const version = await resolveLatestVersion();
  const child = spawn(LAUNCHER_PATH, serverArgs, {
    env: launchEnvironment(version),
    stdio: "inherit",
  });

  await new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("exit", (code, signal) => {
      if (signal) {
        reject(new Error(`launcher terminated by ${signal}`));
        return;
      }

      process.exitCode = code ?? 1;
      resolve();
    });
  });
}
