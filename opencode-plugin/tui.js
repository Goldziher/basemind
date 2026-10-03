/**
 * basemind CLI (terminal) plugin for OpenCode (V2 API).
 *
 * OpenCode loads this entrypoint from the package's `./tui` export into the
 * terminal client. It restores the V1 plugin's proactive agent-comms
 * notifications: when a session starts and after each successful tool call it
 * reads the basemind agent-comms inbox and surfaces any new messages as a
 * toast, tracking a high-water mark so a message is announced once.
 *
 * This half imports `@opencode/plugin/tui` (TUI context) and therefore must
 * NOT be loaded by the server-side half in `basemind.js`.
 */

import { Plugin } from "@opencode/plugin/tui";
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

let commsHighWaterMicros = 0;

/// Read the agent-comms inbox by re-execing the basemind CLI. Resolves to the
/// parsed JSON, or `null` when the CLI is unavailable or returns nothing.
function readCommsInbox(directory, limit) {
  return new Promise((resolve) => {
    const child = execFile(
      command,
      ["agents", "inbox", "--root", directory, "--json", "--limit", String(limit)],
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
  return messages.map((message) => `  • [${message.subject}] from ${message.from} (id: ${message.id})`).join("\n");
}

export default Plugin.define({
  id: "basemind.comms",
  setup(context) {
    const location = context.location ?? context.data.location.default();
    const directory = location?.directory ?? process.cwd();

    const surface = (message) => {
      context.ui.toast.show({ title: "basemind", message, variant: "info" });
    };

    const announce = (messages) => {
      if (messages.length === 0) {
        return false;
      }
      commsHighWaterMicros = Math.max(commsHighWaterMicros, ...messages.map((message) => message.ts_micros ?? 0));
      return true;
    };

    const onSessionCreated = context.data.on("session.created", async () => {
      const inbox = await readCommsInbox(directory, 8);
      const messages = inbox?.messages ?? [];
      if (!announce(messages)) {
        return;
      }
      surface(
        `agent-comms: ${messages.length} recent message(s). Use agents mode message with message_id to read a body, or mode post with thread, subject, and body to reply.\n${formatMessages(messages)}`,
      );
    });

    const onToolSuccess = context.data.on("session.tool.success", async () => {
      const inbox = await readCommsInbox(directory, 30);
      const messages = inbox?.messages ?? [];
      const fresh = messages.filter((message) => (message.ts_micros ?? 0) > commsHighWaterMicros);
      if (!announce(fresh)) {
        return;
      }
      surface(
        `agent-comms: ${fresh.length} new message(s) since last turn. Reply with agents {mode:"post", thread, subject, body, reply_to:<id>} if warranted.\n${formatMessages(fresh)}`,
      );
    });

    return () => {
      onSessionCreated();
      onToolSuccess();
    };
  },
});
