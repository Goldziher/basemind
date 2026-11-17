import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";

const LAUNCHER_PATH = path.join(path.dirname(fileURLToPath(import.meta.url)), "mcp-launch.sh");

// The launcher installs the version the plugin manifest beside it pins (.codex-plugin/plugin.json),
// the binary the plugin's skills and hooks were written for. It is a bash script, which Windows
// cannot spawn directly, so it always runs through bash.
export function launchCommand(serverArgs) {
  return { command: "bash", args: [LAUNCHER_PATH, ...serverArgs] };
}

export async function run(serverArgs) {
  const { command, args } = launchCommand(serverArgs);
  const child = spawn(command, args, { stdio: "inherit" });

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
