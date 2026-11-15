---
type: Decision
title: Adr Immutability
x-ai-rulez:
  kind: rule
  id: adr-immutability
  metadata:
    priority: high
---

# ADR Immutability

- ADRs are `docs/adr/NNNN-<kebab-slug>.md`: one log, numbered from `0000` with no gaps. `docs/adr/README.md` is the index and documents the format; `docs/adr/0000-template.md` is the form to copy.
- Metadata is YAML front matter: `status` (`proposed`, `accepted`, `rejected`, `superseded`, `deprecated`, `withdrawn`) and `date`.
- An ADR may be rewritten freely while its pull request is open. Once merged it is immutable: only status, date and links change, plus plain-fact corrections.
- A changed decision is a new ADR carrying `supersedes: "NNNN"` or `amends: ["NNNN"]`. In a superseded one set only `status: superseded` and `superseded-by: "NNNN"`; an amended one stays `accepted`.
- Never renumber, rename or delete a merged ADR. Every ADR change updates its row in the index.
- adrlint checks the log inside poly (`[lint.markdown.adrlint]` in `poly.toml`): run `poly lint docs/adr` after touching it.
