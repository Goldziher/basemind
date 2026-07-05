# basemind docs site

The [basemind.ai](https://basemind.ai) documentation site — built with
[Astro Starlight](https://starlight.astro.build) and deployed to GitHub Pages by
`.github/workflows/docs.yaml`.

## Develop

```bash
npm install
npm run dev      # local dev server at http://localhost:4321
npm run build    # static build into ./dist
npm run preview  # serve the built site
```

## Layout

- `src/content/docs/**` — the documentation pages (Markdown / MDX).
- `astro.config.mjs` — site config, sidebar, and the `starlight-llms-txt` plugin
  (generates `/llms.txt`, `/llms-small.txt`, `/llms-full.txt`).
- `src/styles/custom.css` — brand theme.
- `public/CNAME` — the custom domain (`basemind.ai`).

Content is sourced from the repo `README.md`, `docs/ARCHITECTURE.md`, and the `skills/` SKILL files —
keep it in sync when those change.
