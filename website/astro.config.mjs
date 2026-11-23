// @ts-check
import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";
import starlightLinksValidator from "starlight-links-validator";
import starlightLlmsTxt from "starlight-llms-txt";

export default defineConfig({
  site: "https://basemind.ai",
  integrations: [
    starlight({
      title: "basemind",
      description:
        "Coding agents ask a local index of your code, git history, documents and memory over MCP " +
        "or the CLI, get paths and lines instead of reading files, and coordinate in shared threads.",
      logo: {
        src: "./src/assets/logo.svg",
        alt: "basemind",
        replacesTitle: true,
      },
      favicon: "/favicon.svg",
      customCss: ["./src/styles/custom.css"],
      social: [{ icon: "github", label: "GitHub", href: "https://github.com/Goldziher/basemind" }],
      editLink: {
        baseUrl: "https://github.com/Goldziher/basemind/edit/main/website/",
      },
      head: [
        {
          tag: "link",
          attrs: { rel: "apple-touch-icon", href: "/apple-touch-icon.png" },
        },
        {
          tag: "link",
          attrs: { rel: "icon", type: "image/png", sizes: "32x32", href: "/favicon-32.png" },
        },
        {
          tag: "meta",
          attrs: { property: "og:image", content: "https://basemind.ai/og.png" },
        },
        {
          tag: "meta",
          attrs: { name: "twitter:card", content: "summary_large_image" },
        },
        {
          tag: "meta",
          attrs: { name: "twitter:image", content: "https://basemind.ai/og.png" },
        },
      ],
      plugins: [
        // Fails the build on a broken internal link or anchor.
        starlightLinksValidator(),
        starlightLlmsTxt({
          exclude: [
            "reference/cli",
            "reference/configuration",
            "reference/architecture",
            "reference/performance",
            "reference/memory",
            "reference/evaluation",
          ],
          promote: ["index*", "start/**", "concepts/**"],
          minify: { collapseCodeBlocks: true },
          details:
            "Operating rule: basemind first, shell/grep/git fallback — prefer its MCP tools " +
            "over reading files, over grep, and over naked git. Curated project map: " +
            "https://github.com/Goldziher/basemind/blob/main/llms.txt",
        }),
      ],
      sidebar: [
        {
          label: "Start here",
          items: [
            { label: "Introduction", slug: "start/introduction" },
            { label: "Installation", slug: "start/installation" },
            { label: "Quickstart", slug: "start/quickstart" },
            { label: "Upgrading", slug: "start/upgrading" },
            { label: "Data and uninstall", slug: "start/uninstall" },
          ],
        },
        {
          label: "Concepts",
          items: [
            { label: "How it works", slug: "concepts/how-it-works" },
            { label: "Token economy", slug: "concepts/token-economy" },
            { label: "Shared memory", slug: "concepts/memory" },
            { label: "Agent comms", slug: "concepts/agent-comms" },
          ],
        },
        {
          label: "Capabilities",
          items: [
            { label: "Code intelligence", slug: "capabilities/code-intelligence" },
            { label: "Git intelligence", slug: "capabilities/git-intelligence" },
            { label: "Document search", slug: "capabilities/document-search" },
            { label: "Semantic code search", slug: "capabilities/code-search" },
          ],
        },
        {
          label: "Reference",
          items: [
            { label: "MCP tools", slug: "reference/mcp-tools" },
            { label: "CLI", slug: "reference/cli" },
            { label: "Configuration", slug: "reference/configuration" },
            { label: "Architecture", slug: "reference/architecture" },
            { label: "Performance", slug: "reference/performance" },
            { label: "Memory and resources", slug: "reference/memory" },
            { label: "Evaluation", slug: "reference/evaluation" },
          ],
        },
      ],
    }),
  ],
});
