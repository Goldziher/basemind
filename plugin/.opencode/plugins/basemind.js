// AI-RULEZ :: GENERATED FILE — DO NOT EDIT
// Content-Hash: blake3:9ae416958d982d89a53a0d0da4513bb3784fbd53fc748c60251ef4cd22457fba
// Source-Hash: blake3:fbc7b4d75a7eb6eec0f28a10c0fe7172499c7b07b219d86838a7a4d3593da9ac
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
