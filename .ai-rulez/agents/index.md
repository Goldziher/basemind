# Concepts

- [Harness Interpreter](harness-interpreter.md) - Reads /tmp/basemind-harden-*.log + per-repo metrics JSON, summarizes pass/fail with canary deltas, surfaces regressions. Cheap read-mostly subagent.
- [Mcp Tool Author](mcp-tool-author.md) - Adds a tool, or an argument on an existing tool, to basemind's verb-tool MCP surface end-to-end — strict params struct, thin shim, helper body, CLI command, parity row, schema-wire and smoke assertions, README row.
- [Rust Perf Engineer](rust-perf-engineer.md) - Reviews diffs touching the basemind scanner / extract / store / index paths for hot-path regressions — allocations, missed memmem opportunities, hashmap churn, parser pool misuse.
- [Scanner Extension](scanner-extension.md) - Adds tree-sitter language coverage to basemind via the language-support skill — sources upstream queries from tree-sitter-\<lang\>/queries/tags.scm, adapts to basemind's Symbol/Call/Import shape.
- [Surface Docs Writer](surface-docs-writer.md) - Rewrites basemind's docs, skills, hooks and plugin manifests against the verb-tool MCP/CLI surface — knows the ai-rulez generation source, the four parallel skill trees, and the hand-synced command mirrors.
