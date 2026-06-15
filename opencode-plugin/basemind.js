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

import fs from "fs";
import path from "path";
import { fileURLToPath } from "url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

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
