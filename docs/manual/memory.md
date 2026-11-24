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
| Remove every note of the repository | none | `basemind memory clear --yes` |
| Find where the notes are kept | none | `basemind cache stats` |

## Contracts

- `memory_save` with an existing `key` replaces the note.
- Notes are plain text on local disk, in the repository group directory `basemind cache stats`
  names. Never save a secret, a token or a credential in one.
- `basemind memory clear` removes every note of the repository, of every tier and agent, and
  empties and purges the vector store of every embedding model. It asks for confirmation on a
  terminal and refuses without `--yes` anywhere else. With the daemon running it runs in the daemon;
  while another basemind process holds the workspace's index and no daemon runs, it is refused.
- `basemind memory delete` removes the note and its vector rows from the store of every embedding
  model the repository has used, and purges those stores, so no earlier version of them still holds
  the note.
- `memory_search` takes at most one of `query`, `key` and `prefix`; with none it lists every note.
  `tags` filters a search or a listing: a note carrying any of them passes. A listing shows the first 200 characters of each value and
  pages with `next_cursor`; `key` reads a note whole.
- A `query` search answers the notes within a relevance floor (squared L2 distance `1.0` between
  unit vectors, a cosine similarity of one half); the reply's `notice` says how many nearer notes it
  left out, and an unrelated query returns no hit.
- When no embedding model can embed the query (none downloaded, offline), `query` matches words
  instead and the reply carries `"lexical": true`: a note is a hit when at least half of the
  query's words each begin a word of its key, value or tags, and its `distance` is the share of
  the words it lacks.
- `visibility` is `group` (the default, shared by every agent of the repository) or `individual`
  (only the agent that saved it).
- Notes live in one store per repository, keyed by the path of its main worktree, and inside it by
  a scope: the normalised `origin` URL, or `path:<main worktree>` without one. Every worktree of a
  repository shares its notes, with or without an `origin`. A clone at another path and a moved
  repository have their own store and do not see the notes of the first. Adding, removing or
  changing `origin` keeps the notes: the first memory call of a session files every note of the
  store under the current scope, the newer one winning when both scopes hold a key, and a
  search by meaning embeds them again. `basemind manual data` says where the store lives.
- Both tools need a build with the `memory` feature (the release binary has it); without it the
  call is an error naming the feature. With a daemon running the CLI forwards memory operations to
  it.
