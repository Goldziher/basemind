# Concepts

* [Harness Interpreter](harness-interpreter.md) - Reads /tmp/basemind-harden-*.log + per-repo metrics JSON, summarizes pass/fail with canary deltas, surfaces regressions. Cheap read-mostly subagent.
* [Mcp Domain Migrator](mcp-domain-migrator.md) - Migrates one basemind MCP domain from flat per-verb tools onto a single mode-dispatched tool plus a CLI subcommand group — mode enum, params, shim, helpers, CLI — without touching shared files.
* [Mcp Tool Author](mcp-tool-author.md) - Adds a mode to one of basemind's 9 MCP domains end-to-end — mode enum variant, params, shim arm, helper body, CLI subcommand, smoke assertion, README row.
* [Rust Perf Engineer](rust-perf-engineer.md) - Reviews diffs touching the basemind scanner / extract / store / index paths for hot-path regressions — allocations, missed memmem opportunities, hashmap churn, parser pool misuse.
* [Scanner Extension](scanner-extension.md) - Adds tree-sitter language coverage to basemind via the language-support skill — sources upstream queries from tree-sitter-\<lang\>/queries/tags.scm, adapts to basemind's Symbol/Call/Import shape.
* [Surface Docs Writer](surface-docs-writer.md) - Rewrites basemind's docs, skills, and plugin manifests against the 9-domain MCP/CLI surface — knows the ai-rulez generation source, the four parallel skill trees, and the hand-synced command mirrors.
