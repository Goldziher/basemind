;; section: symbols
;;
;; Markdown / Obsidian headings become navigable symbols (kind `heading`), so `outline` and
;; `search_symbols` work over a notes vault the same way they do over source code. The captured
;; name is the `heading_content` inline node; heading depth is implicit in document (line) order.

;; ATX headings: `# Title` … `###### Title`.
(atx_heading
  heading_content: (inline) @symbol.name) @symbol.heading

;; Setext headings: a paragraph underlined by `===` (h1) or `---` (h2).
(setext_heading
  heading_content: (paragraph (inline) @symbol.name)) @symbol.heading

;; section: calls
;;
;; Obsidian's reference graph — wikilinks (`[[Note]]`, `[[Note#Heading|alias]]`, `![[Embed]]`),
;; standard note links (`[text](Note.md)`), and `#tags` (inline + frontmatter) — is NOT modeled by
;; the tree-sitter-markdown grammar: basemind runs only the block grammar, and these all live inside
;; opaque `inline` nodes. They are extracted by a dedicated fence-aware byte-scan in `extract/l2.rs`
;; (`markdown_references`) and surfaced as calls, so `find_references "Note"` returns a note's
;; backlinks and `find_references "#tag"` returns the notes sharing a tag. This section is
;; intentionally empty.
