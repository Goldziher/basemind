# Memory

Memory holds durable notes: a decision, a convention or a finding that a later session or another
agent working in the same repository should know. Notes are keyed, tagged and searchable by meaning.

| Question | MCP call | CLI |
|---|---|---|
| Remember this | `memory_save {"key":"decision/auth-tokens","value":"Tokens rotate every 24h.","tags":["auth"]}` | `basemind memory put decision/auth-tokens "Tokens rotate every 24h." --tag auth` |
| Recall by meaning | `memory_search {"query":"token rotation"}` | `basemind memory search "token rotation"` |
| Read one note | `memory_search {"key":"decision/auth-tokens"}` | `basemind memory get decision/auth-tokens` |
| List by key prefix | `memory_search {"prefix":"decision/"}` | `basemind memory list --prefix decision/` |
| Delete a note | none | `basemind memory delete decision/auth-tokens` |

## Contracts

- `memory_save` with an existing `key` replaces the note.
- `memory_search` takes at most one of `query`, `key` and `prefix`; with none it lists every note.
  `tag` filters a search or a listing. A listing shows the first 200 characters of each value and
  pages with `next_cursor`; `key` reads a note whole.
- `visibility` is `group` (the default, shared by every agent of the repository) or `individual`
  (only the agent that saved it).
- Memory is scoped by the normalised git `origin` URL: clones of one repository share notes,
  unrelated repositories do not.
- Both tools need a build with the `memory` feature (the release binary has it); without it the
  call is an error naming the feature. With a daemon running the CLI forwards memory operations to
  it.
