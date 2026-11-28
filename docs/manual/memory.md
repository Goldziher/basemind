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
| Export the notes to a file | none | `basemind memory export --output notes.jsonl` |
| Merge an export file into the repository | none | `basemind memory import notes.jsonl` |
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
- `basemind memory export` writes the notes of one tier of the current scope as JSON lines, to stdout
  or `--output`: a header line (`format`, `version`, `visibility`, `notes`) then one note per line
  (`key`, `value`, `tags`, `created_at`, `updated_at`), ordered by key, so the same notes always
  export to the same bytes. `--prefix` and `--tag` narrow it; `--individual` exports the per-agent
  tier. An export holds at most 8 MiB of notes: past that it is refused, and a larger store is
  exported in parts with `--prefix` or `--tag`.
- `basemind memory import <file>` (`-` reads stdin) merges such a file into the same tier by key.
  The note with the later `updated_at` wins and the existing note stays on a tie; `--overwrite`
  takes the file's note, `--skip` keeps the existing one, and the two exclude each other. A replaced
  note keeps the earlier `created_at` and takes the file's `updated_at`. The file is checked whole
  before anything is written, and a bad one writes nothing: it must carry the header of a format and
  version this binary reads, a note count that matches the header, no duplicate or empty key, no
  negative timestamp, and notes within the limits of a save; it is at most 8 MiB and 100000 notes.
  A file of one tier is refused by the other. Each note it writes records `memory import <file name>`
  as its provenance, and its vector is rebuilt by the next search by meaning. Importing the same
  file again changes nothing. Both commands run in the daemon when one is up.
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
