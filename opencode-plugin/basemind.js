/**
 * basemind plugin for OpenCode.ai
 *
 * Registers the basemind MCP server (`basemind serve`) and the skills
 * directory shipped with the repo. OpenCode discovers the plugin via the
 * `plugin` array in `opencode.json`; the function exported here is called
 * once at startup with the live client + directory and returns a config
 * hook that mutates OpenCode's resolved config in place.
 *
 * Exported as both the default and a named export so OpenCode picks it up
 * regardless of which convention its plugin loader resolves first.
 */

import { execFile } from "child_process";
import fs from "fs";
import path from "path";
import { fileURLToPath } from "url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

const bundledLauncher = path.join(__dirname, "scripts", "mcp-launch.sh");
const repoLauncher = path.join(__dirname, "..", "scripts", "mcp-launch.sh");
const launcher = fs.existsSync(bundledLauncher) ? bundledLauncher : repoLauncher;

let commsHighWaterMicros = 0;

function readCommsInbox(directory, limit) {
  return new Promise((resolve) => {
    const child = execFile(
      launcher,
      ["comms", "inbox", "--root", directory, "--json", "--limit", String(limit)],
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

const bundledSkillsDir = path.join(__dirname, "skills");
const repoSkillsDir = path.join(__dirname, "..", "skills");
const skillsDir = fs.existsSync(bundledSkillsDir) ? bundledSkillsDir : repoSkillsDir;

const hooks = ({ client, directory } = {}) => {
  const root = directory || process.cwd();

  const surface = async (message) => {
    try {
      if (client?.tui?.showToast) {
        await client.tui.showToast({ body: { message, variant: "info" } });
        return;
      }
    } catch {}
    // eslint-disable-next-line no-console
    console.error(`[basemind] ${message}`);
  };

  return {
    config: async (config) => {
      config.skills = config.skills || {};
      config.skills.paths = config.skills.paths || [];
      if (!config.skills.paths.includes(skillsDir)) {
        config.skills.paths.push(skillsDir);
      }

      config.mcp = config.mcp || {};
      if (!config.mcp.basemind) {
        config.mcp.basemind = {
          type: "local",
          command: ["basemind", "serve"],
          enabled: true,
        };
      }
    },

    event: async ({ event } = {}) => {
      if (!event?.type) {
        return;
      }

      if (event.type === "session.created") {
        const inbox = await readCommsInbox(root, 8);
        const messages = inbox?.messages ?? [];
        if (messages.length === 0) {
          return;
        }
        commsHighWaterMicros = Math.max(commsHighWaterMicros, ...messages.map((message) => message.ts_micros ?? 0));
        await surface(
          `agent-comms: ${messages.length} recent message(s). Use thread_post / message_get to participate.\n${formatMessages(messages)}`,
        );
        return;
      }

      if (event.type === "tool.execute.after") {
        const inbox = await readCommsInbox(root, 30);
        const messages = inbox?.messages ?? [];
        const fresh = messages.filter((message) => (message.ts_micros ?? 0) > commsHighWaterMicros);
        if (fresh.length === 0) {
          return;
        }
        commsHighWaterMicros = Math.max(commsHighWaterMicros, ...fresh.map((message) => message.ts_micros ?? 0));
        await surface(
          `agent-comms: ${fresh.length} new message(s) since last turn. Reply with thread_post {reply_to:<id>} if warranted.\n${formatMessages(fresh)}`,
        );
      }
    },
  };
};

export const BasemindPlugin = async (input) => hooks(input);
export default async (input) => hooks(input);
