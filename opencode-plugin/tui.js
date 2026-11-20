/**
 * basemind CLI (terminal) plugin for OpenCode (V2 API).
 *
 * OpenCode loads this entrypoint from the package's `./tui` export into the
 * terminal client. It restores the V1 plugin's proactive agent-comms
 * notifications: when a session starts and after each successful tool call it
 * asks the daemon of basemind for an announcing read of the agent inbox and
 * surfaces any unread messages as a toast. The daemon records what it has
 * announced, so a message is announced once and stays unread until the agent
 * reads it with agent_inbox. At startup it also shows, once, the line from
 * `./binary-check.js` when the `basemind` binary on PATH is missing or from
 * another minor version.
 *
 * This half imports `@opencode/plugin/tui` (TUI context) and therefore must
 * NOT be loaded by the server-side half in `basemind.js`.
 */

import { Plugin } from "@opencode/plugin/tui";
import { checkBinary, packageVersion } from "./binary-check.js";
import { execFile } from "child_process";
import fs from "fs";
import path from "path";
import { fileURLToPath } from "url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// Prefer the bundled launcher (harness releases copy it next to the plugin); a
// repo checkout has the shared one a level up; otherwise fall back to the
// `basemind` binary on PATH, so comms notifications also work for an install
// from npm where no launcher is bundled. ~keep
const bundledLauncher = path.join(__dirname, "scripts", "mcp-launch.sh");
const repoLauncher = path.join(__dirname, "..", "scripts", "mcp-launch.sh");
const command = fs.existsSync(bundledLauncher)
  ? bundledLauncher
  : fs.existsSync(repoLauncher)
    ? repoLauncher
    : "basemind";

/// Announce unread agent-comms messages by re-execing the basemind CLI: front
/// matter only, nothing marked read. Resolves to the parsed JSON, or `null`
/// when the CLI is unavailable or returns nothing.
function announceCommsInbox(directory, limit) {
  return new Promise((resolve) => {
    const child = execFile(
      command,
      ["agent", "inbox", "--announce", "--root", directory, "--json", "--limit", String(limit)],
      { timeout: 6000, cwd: directory },
      (error, stdout) => {
        if (error || !stdout) {
          resolve(null);
          return;
        }
        try {
          resolve(JSON.parse(stdout));
        } catch {
          resolve(null);
        }
      },
    );
    child.on("error", () => resolve(null));
  });
}

function formatMessages(messages) {
  return messages
    .map(
      (message) => `  - from ${message.from} in ${message.subject} (${message.id}, ${message.body_bytes ?? 0} bytes)`,
    )
    .join("\n");
}

export default Plugin.define({
  id: "basemind.comms",
  setup(context) {
    const location = context.location ?? context.data.location.default();
    const directory = location?.directory ?? process.cwd();

    const surface = (message, variant = "info") => {
      context.ui.toast.show({ title: "basemind", message, variant });
    };

    checkBinary({ expected: packageVersion() }).then((binary) => {
      if (binary.message) {
        surface(binary.message, binary.status === "missing" ? "error" : "warning");
      }
    });

    const announce = async () => {
      const inbox = await announceCommsInbox(directory, 10);
      const messages = inbox?.messages ?? [];
      if (messages.length === 0) {
        return;
      }
      const more = inbox?.more ? "\nThere is more unread than listed." : "";
      surface(
        `agent-comms: ${messages.length} unread message(s). Read them with agent_inbox (bodies inline; {"id":"<id>"} reads one whole message); reply with agent_post. The agent tools are deferred: find them by tool search.\n${formatMessages(messages)}${more}`,
      );
    };

    const onSessionCreated = context.data.on("session.created", announce);
    const onToolSuccess = context.data.on("session.tool.success", announce);

    return () => {
      onSessionCreated();
      onToolSuccess();
    };
  },
});
