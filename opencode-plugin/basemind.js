/**
 * basemind plugin for OpenCode (V2 API).
 *
 * Registers the basemind MCP server (`basemind serve`) and the skills shipped
 * with this package. OpenCode discovers the plugin from the `plugins` array in
 * `opencode.json(c)`; the default export is a `Plugin.define({ id, setup })`
 * definition whose `setup` registers domain transforms.
 *
 * The proactive agent-comms notifications live in the sibling `./tui`
 * entrypoint (`tui.js`), which OpenCode loads into the terminal client, so this
 * server-side half stays free of TUI imports.
 */

import { Plugin } from "@opencode/plugin";
import fs from "fs";
import path from "path";
import { fileURLToPath } from "url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// The published package bundles `skills/`; a repo checkout has it at the same
// relative path, so a single resolution works for both. ~keep
const bundledSkillsDir = path.join(__dirname, "skills");
const repoSkillsDir = path.join(__dirname, "..", "skills");
const skillsDir = fs.existsSync(bundledSkillsDir) ? bundledSkillsDir : repoSkillsDir;

/// Strip a leading YAML frontmatter block, returning its fields and the body.
/// The skills use plain scalar keys plus folded (`>-`) descriptions; a tiny
/// parser is enough and avoids pulling a YAML dependency into the plugin.
function parseSkill(text, fallbackId) {
  const meta = {};
  let body = text;
  if (text.startsWith("---")) {
    const end = text.indexOf("\n---", 3);
    if (end !== -1) {
      const raw = text.slice(3, end);
      const lines = raw.split("\n");
      for (let i = 0; i < lines.length; i += 1) {
        const match = /^([A-Za-z0-9_-]+):\s*(.*)$/.exec(lines[i]);
        if (!match) {
          continue;
        }
        const [, key, inline] = match;
        if (inline === ">" || inline === ">-" || inline === "|" || inline === "|-") {
          const block = [];
          while (i + 1 < lines.length && /^\s+\S/.test(lines[i + 1])) {
            block.push(lines[i + 1].trim());
            i += 1;
          }
          meta[key] = inline.startsWith("|") ? block.join("\n") : block.join(" ");
        } else {
          meta[key] = inline.replace(/^["']|["']$/g, "");
        }
      }
      body = text.slice(end + 4).replace(/^\s*\n/, "");
    }
  }
  return {
    id: meta.name ? String(meta.name) : fallbackId,
    name: meta.name ? String(meta.name) : fallbackId,
    description: meta.description ? String(meta.description) : undefined,
    autoinvoke: meta.autoinvoke === "true",
    content: body,
  };
}

/// Enumerate `skills/<id>/SKILL.md`, parse each, and return `Skill.Info`-shaped
/// records for `ctx.skill.transform.editor.add`.
function loadSkills(dir) {
  let entries;
  try {
    entries = fs.readdirSync(dir, { withFileTypes: true });
  } catch {
    return [];
  }
  const skills = [];
  for (const entry of entries) {
    if (!entry.isDirectory()) {
      continue;
    }
    const file = path.join(dir, entry.name, "SKILL.md");
    if (!fs.existsSync(file)) {
      continue;
    }
    const parsed = parseSkill(fs.readFileSync(file, "utf8"), entry.name);
    skills.push({ ...parsed, path: file });
  }
  return skills;
}

export default Plugin.define({
  id: "basemind",
  async setup(ctx) {
    await ctx.mcp.transform((editor) => {
      if (!editor.get("basemind")) {
        editor.set("basemind", { type: "local", command: ["basemind", "serve"] });
      }
    });

    const skills = loadSkills(skillsDir);
    if (skills.length > 0) {
      await ctx.skill.transform((editor) => {
        for (const skill of skills) {
          if (!editor.get(skill.id)) {
            editor.add(skill);
          }
        }
      });
    }
  },
});
