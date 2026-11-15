# Basemind

Use the bundled public skills for structural code search, git history, documents,
shared memory and scoped agent coordination. The MCP server inherits the caller's
project directory and requires the basemind CLI. Each job is its own tool with an exact
schema (`code_outline`, `code_symbols`, `code_read`, `code_grep`, `code_search`, `code_refs`,
`find_files`, `docs_search`, `git_log`, `git_blame`, `git_diff`, `memory_save`, `memory_search`,
`index_status`, `index_rescan`): an unknown argument is rejected and line numbers are 1-based.
Scans, memory writes and coordination messages require the current task's authorization.
