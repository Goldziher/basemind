# Network access

basemind indexes and answers locally, but it downloads some files on first use: its own binary
(through an installer or the plugin launcher), tree-sitter grammars and Hugging Face models. This
page lists every outbound host, what triggers each fetch, and how to redirect it or turn it off.
Nothing else leaves the machine: the daemon listens on a local socket or named pipe, and on loopback
only for the HTTP front-end (`BASEMIND_ALLOW_HTTP`). Git history is read from the local repository;
basemind never fetches.

## Outbound hosts

| What | Host | Triggered by | Redirect | Turn off |
|---|---|---|---|---|
| Plugin launcher binary | `github.com/Goldziher/basemind/releases/download/` (redirects to GitHub's release asset storage) | First launch of a plugin version with no matching binary in `~/.cache/basemind/versions/` | `BASEMIND_RELEASE_BASE_URL` | `BASEMIND_OFFLINE=1`, or `BASEMIND_BIN` |
| Launcher release-window fallback | `github.com/Goldziher/basemind/releases/latest`, `api.github.com/repos/Goldziher/basemind/releases` | The pinned release cannot be downloaded, and no mirror is set | none | `BASEMIND_RELEASE_BASE_URL`, `BASEMIND_OFFLINE=1`, or `BASEMIND_BIN` |
| npm package binary | `github.com/Goldziher/basemind/releases/download/` | `postinstall` (`npm-package/install.js`), and the first run of `bin/basemind.js` when the binary is missing | none | install the program another way |
| pip package binary | `github.com/Goldziher/basemind/releases/download/` | The first run of the `basemind` wrapper for a version | none | `BASEMIND_BINARY` |
| Tree-sitter grammars | `github.com/xberg-io/tree-sitter-language-pack/releases/download/` (the manifest `parsers.json`, then the bundles it names) | The first scan, `basemind lang install`, a file in a language whose grammar is not cached, a `[languages.<name>] preload` | `TREE_SITTER_LANGUAGE_PACK_MANIFEST_URL` (`https://` or `file://`) | `BASEMIND_OFFLINE=1`, or `BASEMIND_GRAMMAR_OFFLINE=1` |
| Embedding models | `huggingface.co` (redirects to Hugging Face's file storage) | The first document, memory or semantic code-search embedding with a preset that is not cached | `HF_ENDPOINT` | `BASEMIND_OFFLINE=1`, or `HF_HUB_OFFLINE=1` |
| Reranker model | `huggingface.co` | The first reranked search with a preset that is not cached | `HF_ENDPOINT` | `BASEMIND_OFFLINE=1`, or `HF_HUB_OFFLINE=1` |
| Tokenizer | `huggingface.co` | The first real token count (`basemind admin tokens`, savings telemetry) in a build with the `tokenizer` feature | `HF_ENDPOINT` | `BASEMIND_OFFLINE=1`, or `HF_HUB_OFFLINE=1` |

The launcher downloads with `curl` (else `wget`) and the pip wrapper with Python's `urllib`; both
honour `HTTPS_PROXY`, `HTTP_PROXY` and `NO_PROXY`. The npm installer does not read the proxy
variables yet. The Hugging Face client reads `HTTPS_PROXY` too.

Models are cached under `HF_HUB_CACHE` (else `$HF_HOME/hub`, else the platform's Hugging Face
cache) and grammars under `TREE_SITTER_LANGUAGE_PACK_CACHE_DIR` (else the data home). Each is
downloaded once per cache.

## Offline mode

`BASEMIND_OFFLINE=1` turns every download above off, except the npm and pip installers, which run
before basemind does. `1`, `true`, `yes` or `on` turns it on and `0`, `false`, `no` or `off` leaves it
off, in any case; any other value stops basemind and the launcher with an error naming the variable. basemind exports `HF_HUB_OFFLINE=1` to the Hugging Face client, for its own
process and the daemon it starts. A grammar, model or binary missing from its cache is then an error
that names it:

- the launcher names the release archive and where it expects the binary, and runs a cached binary
  of the pinned minor when it has one;
- a scan or `basemind lang install` names the missing grammars and the cache to copy them into;
- an embedding or a rerank names the model file and its repository, and the document or code-search
  tier reports itself unavailable while the rest of the index works.

To prepare a machine for offline use, run basemind once online with the same cache locations, or
copy the caches (`~/.cache/basemind/versions/`, the grammar cache that `basemind lang list` prints,
and the Hugging Face cache) from a machine that has them. A mirror of the release downloads is
described in the [README](../README.md#1-as-a-plugin-recommended).

## Compiled in but unused

The release build enables lancedb's `remote` feature, which links an HTTP and gRPC client (`tonic`,
`arrow-flight`, `reqwest`). Nothing in basemind calls it: lancedb 0.41 does not compile its error
type without that feature. The vector store is always opened on a local path.
