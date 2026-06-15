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

import path from "path";
import { fileURLToPath } from "url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, "..");
const skillsDir = path.join(repoRoot, "skills");

const hooks = () => ({
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
});

export const BasemindPlugin = async () => hooks();
export default async () => hooks();
