// AI-RULEZ :: GENERATED FILE — DO NOT EDIT
// Content-Hash: blake3:9ae416958d982d89a53a0d0da4513bb3784fbd53fc748c60251ef4cd22457fba
// Source-Hash: blake3:c85d912de0ec93b88abc5ffe7cb4b0515710852a633cffd5932ed908b143d786
// Schema-Version: v1

/**
 * OpenCode v2 adapter for basemind.
 *
 * To add OpenCode-specific tools or hooks:
 *
 * 1. Create .ai-rulez/opencode/index.js.
 * 2. Default-export { id, setup(ctx) } from that source file. OpenCode v2 does not
 *    run v1 plugins (an exported function returning hooks).
 * 3. If the plugin bundles skills, commands, or agents, call
 *    registerBundledContent(ctx) from ../ai-rulez-content.js in setup.
 * 4. Run ai-rulez generate --plugin --dry-run, then ai-rulez generate --plugin.
 *
 * Keep shared skills, commands, agents, and MCP configuration in their normal
 * .ai-rulez sources. Validate all external input and never interpolate untrusted
 * values into shell commands.
 */
import { registerBundledContent } from "../ai-rulez-content.js"

/** @type {import("@opencode/plugin").Plugin.Plugin} */
export default {
  id: "basemind",
  async setup(ctx) {
    await registerBundledContent(ctx)
  },
}
