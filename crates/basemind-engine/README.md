# basemind-engine

The engine behind [basemind](https://github.com/Goldziher/basemind): a local index of a repository's
code (tree-sitter symbols, references, imports), git history, documents and agent memory, and the
synchronous operations that query it.

The crate has no command line, no MCP adapter and no daemon. The `basemind` crate puts those on top of
it; a program that wants the index without them depends on this crate alone.

```toml
[dependencies]
basemind-engine = { version = "0.34", features = ["code-search"] }
```

## Features

| Feature                                                   | Adds                                                              |
| --------------------------------------------------------- | ----------------------------------------------------------------- |
| `code-search`                                             | semantic search over code chunks (embeddings and a vector store)  |
| `documents`, `documents-ocr`, `documents-heic`, `documents-ner` | text extraction from PDFs, Office files, email, HTML, images   |
| `memory`                                                  | the shared memory notes store                                     |
| `code-intel-js`, `code-intel-stack`, `code-intel-python`  | import-resolved navigation (`code-intel` enables all three)       |
| `comms`                                                   | the daemon-hosted code paths (shared outline cache, scan queue)   |
| `test-support`                                            | helpers that tests of dependent crates use                        |

The crate never depends on `rmcp`, `clap` or `hyper`.

## Layout

- `store*`, `index`, `scanner*`, `extract`, `git*`, `config`: the on-disk stores and the scan pipeline.
- `embeddings`, `lance`, `scanner_docs*`: the retrieval layer, behind the features above.
- `engine`: `Workspace`, the caches it reads through and its operations, each failing with an
  `EngineError`.

## License

MIT
