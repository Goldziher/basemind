# Security Policy

## Supported Versions

Security fixes are applied to the latest release from the default branch.

## Reporting a Vulnerability

Do not open a public issue for a security vulnerability. Use GitHub's
[private vulnerability reporting](https://github.com/Goldziher/basemind/security/advisories/new)
to contact the maintainers privately.

Include the affected version, reproduction steps, impact, and any suggested remediation.

## Downloaded grammars and models

basemind downloads tree-sitter grammar libraries, which it loads into its own process, and model
weights on first use. [`ATTRIBUTIONS.md`](ATTRIBUTIONS.md#grammars-and-models-downloaded-at-runtime)
lists what is verified before each is used and where. For grammars:

- A bundle must match the SHA-256 listed in the manifest of the tree-sitter-language-pack GitHub
  release, fetched over HTTPS by default. `TREE_SITTER_LANGUAGE_PACK_MANIFEST_URL` replaces that
  URL, and the replacement may be `http://` or `file://`, so HTTPS protects only the default
  manifest. The manifest is not signed or pinned, and a library is not hashed again when it is
  loaded from the cache.
- The cache is `<data home>/grammars/tree-sitter-language-pack/v<version>/libs` (`BASEMIND_DATA_HOME`),
  or the same suffix under `TREE_SITTER_LANGUAGE_PACK_CACHE_DIR`. On Unix it must be owned by the
  current user or root and not be group- or other-writable, or it is not used.
- `BASEMIND_GRAMMAR_OFFLINE` stops the downloads basemind makes up front (the bootstrap and
  `basemind lang install`); a grammar loaded on demand for another language is still downloaded.
