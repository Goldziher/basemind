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

// Agent-comms launcher: resolve the bundled mcp-launch.sh across both install
// modes (npm package vs monorepo dev), mirroring the skills-dir resolution below.
const bundledLauncher = path.join(__dirname, "scripts", "mcp-launch.sh");
const repoLauncher = path.join(__dirname, "..", "scripts", "mcp-launch.sh");
const launcher = fs.existsSync(bundledLauncher) ? bundledLauncher : repoLauncher;

// Per-session high-water mark of the newest message timestamp already surfaced,
// so tool.execute.after polling only reports genuinely new messages once.
let commsHighWaterMicros = 0;

// Run `basemind comms inbox --json` best-effort and time-boxed. Resolves to the
// parsed inbox object, or null on any failure (no daemon, no jq, timeout, parse
// error). Never throws — agent-comms must never break the opencode session.
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

// Condense inbox messages to front-matter lines (subject/from/id) — never bodies,
// to stay token-frugal, matching the session-start / inbox-notify hook contract.
function formatMessages(messages) {
  return messages
    .map((message) => `  • [${message.subject}] from ${message.from} (id: ${message.id})`)
    .join("\n");
}

// Resolve the skills directory across both install modes:
//   - npm install: skills/ sits next to basemind.js inside
//     node_modules/basemind-opencode/ (the prepack hook copies it in).
//   - git+URL / monorepo dev: skills/ lives at the repo root, one level above
//     opencode-plugin/.
// Whichever exists wins; this keeps both install paths working without
// duplicating the dev tree.
const bundledSkillsDir = path.join(__dirname, "skills");
const repoSkillsDir = path.join(__dirname, "..", "skills");
const skillsDir = fs.existsSync(bundledSkillsDir) ? bundledSkillsDir : repoSkillsDir;

// BEST-EFFORT: opencode's event-bus shape and the precise model-facing
// context-injection API are only partially documented. We register an `event`
// handler against the documented bus (session.created, tool.execute.after) and
// surface recent agent-comms front-matter through the most plausible available
// surface — a TUI toast via the injected client when present, with a console
// fallback. If opencode later exposes a first-class additionalContext channel
// for plugins, swap the toast/log surface for it; the polling logic stays.
const hooks = ({ client, directory } = {}) => {
  const root = directory || process.cwd();

  // Surface a short notice to the model/user. Prefer a TUI toast through the
  // injected client; fall back to stderr so the signal is never silently lost.
  const surface = async (message) => {
    try {
      if (client?.tui?.showToast) {
        await client.tui.showToast({ body: { message, variant: "info" } });
        return;
      }
    } catch {
      // toast surface unavailable — fall through to the log fallback.
    }
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

    // Event-bus handler. opencode invokes this for every bus event; we act only
    // on the two we care about. Every branch is fail-open and time-boxed so
    // agent-comms can never block or break a session.
    event: async ({ event } = {}) => {
      if (!event?.type) {
        return;
      }

      // session.created: baseline the high-water mark and surface recent history.
      if (event.type === "session.created") {
        const inbox = await readCommsInbox(root, 8);
        const messages = inbox?.messages ?? [];
        if (messages.length === 0) {
          return;
        }
        commsHighWaterMicros = Math.max(
          commsHighWaterMicros,
          ...messages.map((message) => message.ts_micros ?? 0),
        );
        await surface(
          `agent-comms: ${messages.length} recent message(s). Use room_post / message_get to participate.\n${formatMessages(messages)}`,
        );
        return;
      }

      // tool.execute.after: poll for messages newer than the high-water mark.
      if (event.type === "tool.execute.after") {
        const inbox = await readCommsInbox(root, 30);
        const messages = inbox?.messages ?? [];
        const fresh = messages.filter((message) => (message.ts_micros ?? 0) > commsHighWaterMicros);
        if (fresh.length === 0) {
          return;
        }
        commsHighWaterMicros = Math.max(
          commsHighWaterMicros,
          ...fresh.map((message) => message.ts_micros ?? 0),
        );
        await surface(
          `agent-comms: ${fresh.length} new message(s) since last turn. Reply with room_post {reply_to:<id>} if warranted.\n${formatMessages(fresh)}`,
        );
      }
    },
  };
};

export const BasemindPlugin = async (input) => hooks(input);
export default async (input) => hooks(input);
