import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const BOOTSTRAP_MARKER = "basemind:using-basemind bootstrap for pi";

const extensionDir = dirname(fileURLToPath(import.meta.url));
const packageRoot = resolve(extensionDir, "../..");
const skillsDir = resolve(packageRoot, "skills");
const bootstrapSkillPath = resolve(skillsDir, "basemind", "SKILL.md");

let cachedBootstrap: string | null | undefined;

export default function basemindPiExtension(pi: ExtensionAPI) {
  let injectBootstrap = true;

  pi.on("resources_discover", async () => ({
    skillPaths: [skillsDir],
  }));

  pi.on("session_start", async () => {
    injectBootstrap = true;
  });

  pi.on("session_compact", async () => {
    injectBootstrap = true;
  });

  pi.on("agent_end", async () => {
    injectBootstrap = false;
  });

  pi.on("context", async (event) => {
    if (!injectBootstrap) return;
    if (event.messages.some(messageContainsBootstrap)) return;

    const bootstrap = getBootstrapContent();
    if (!bootstrap) return;

    const bootstrapMessage = {
      role: "user" as const,
      content: [{ type: "text" as const, text: bootstrap }],
      timestamp: Date.now(),
    };

    const insertAt = firstNonCompactionSummaryIndex(event.messages);
    return {
      messages: [...event.messages.slice(0, insertAt), bootstrapMessage, ...event.messages.slice(insertAt)],
    };
  });
}

function getBootstrapContent(): string | null {
  if (cachedBootstrap !== undefined) return cachedBootstrap;

  try {
    const skillContent = readFileSync(bootstrapSkillPath, "utf8");
    const body = stripFrontmatter(skillContent);
    cachedBootstrap = `<EXTREMELY_IMPORTANT>
${BOOTSTRAP_MARKER}

This project ships basemind — an indexed tree-sitter code map + git context layer.
The basemind skill is included below and already loaded for this pi session; follow
it now and do not try to load it again.

${body}

${piToolMapping()}
</EXTREMELY_IMPORTANT>`;
    return cachedBootstrap;
  } catch {
    cachedBootstrap = null;
    return null;
  }
}

function stripFrontmatter(content: string): string {
  const match = content.match(/^---\n[\s\S]*?\n---\n([\s\S]*)$/);
  return (match ? match[1] : content).trim();
}

function piToolMapping(): string {
  return `## pi tool mapping

pi has no native MCP, so basemind's MCP tools are not exposed in this session. Use
the \`basemind\` CLI through pi's \`bash\` tool instead — it shares the same on-disk
index and the same capabilities:

- Outline a file: \`basemind query outline <path>\`
- Find a symbol: \`basemind query symbol <name>\`
- Find call sites: \`basemind query references <name>\`
- Find callers: \`basemind query callers <path> <name>\`
- Regex over content: \`basemind query grep <pattern>\`
- Git history / blame: \`basemind git recent-changes\`, \`basemind git blame-file <path>\`
- Re-index after edits: \`basemind rescan [path]\`

Run \`basemind scan\` once if the index is missing. Prefer these over re-reading or
grepping files. If a pi MCP extension is installed, basemind can also be registered
as a stdio server (\`basemind serve\`) via \`<cwd>/.pi/mcp.json\`.`;
}

function messageContainsBootstrap(message: unknown): boolean {
  const content = (message as { content?: unknown }).content;
  if (typeof content === "string") return content.includes(BOOTSTRAP_MARKER);
  if (!Array.isArray(content)) return false;
  return content.some((part) => {
    return (
      part &&
      typeof part === "object" &&
      (part as { type?: unknown }).type === "text" &&
      typeof (part as { text?: unknown }).text === "string" &&
      (part as { text: string }).text.includes(BOOTSTRAP_MARKER)
    );
  });
}

function firstNonCompactionSummaryIndex(messages: unknown[]): number {
  let index = 0;
  while ((messages[index] as { role?: unknown } | undefined)?.role === "compactionSummary") {
    index += 1;
  }
  return index;
}
