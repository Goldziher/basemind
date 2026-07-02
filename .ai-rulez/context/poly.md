---
priority: high
---

# poly

poly (polylint) is a single-binary, multi-language linter and formatter. It bundles engines (ruff for Python, oxc for JS/TS/JSON, taplo for TOML, rumdl for Markdown) and delegates to native tools (cargo fmt/clippy, golangci-lint, actionlint, shellcheck, shfmt) when present.

## Commands

- Lint: `poly lint .`
- Check formatting (dry-run): `poly fmt --check .`
- Apply formatting: `poly fmt --fix .`
- Apply lint autofixes: `poly lint --fix .`

## Configuration

Per-repo `poly.toml`. Cache dir `.polylint/` (gitignored).

## Severity

`poly lint` exits non-zero only on error-severity findings; warnings don't fail CI.

## CI

Validation runs via `uses: xberg-io/actions/.github/workflows/reusable-validate.yml@v1`.

Run `poly fmt --check .` and `poly lint .` after changes to verify compliance.
