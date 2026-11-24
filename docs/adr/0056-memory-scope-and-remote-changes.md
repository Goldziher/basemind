---
status: proposed
date: 2026-11-24
---

# ADR-0056: Memory scope: a path-keyed store, notes refiled on a remote change

- **Deciders:** the repository owner
- **Related:** ADR-0028 (the repository group)

## Context

Notes are stored in `cache/repos/<repo_group_key>/memory.fjall`, where the key hashes the canonical
path of the repository's main worktree, and each note is filed inside the store under a scope: the
normalised `origin` URL, else `path:<main worktree root>`. Two consequences were never decided:

- adding, removing or changing `origin` changes the scope, and every earlier note is hidden;
- a clone at another path, or a moved repository, has a different store and sees no notes.

Keying the store by the origin scope instead would let clones share notes, but a Fjall database
takes an exclusive directory lock, so two clones of one remote in two processes would contend for
one store, and two diverged clones would have to merge their notes.

## Decision

1. **The store stays keyed by the main worktree's path.** Every worktree of a repository shares it,
   with or without `origin`. A clone or a moved repository has its own store.
2. **The scope inside the store follows the current `origin`.** The first memory call of a writable
   workspace files every note of the store under the current scope (`rescope`). Every scope in a
   path-keyed store belongs to one repository, so none is foreign. When both scopes hold a key, the
   newer note wins, by the rule of the legacy migration, so the result does not depend on the order.
   The merged rows are persisted before the old ones are removed.
3. **The notes' vector rows are owed again** after a refile: they carry the old scope and are
   re-embedded by the next search by meaning.
4. **Moving notes between clones and moved repositories is an explicit act:** `basemind memory
   export` and `basemind memory import`, merging by the same rule. They are not implemented yet.

## Consequences

- No store-layout change and no migration: existing notes stay where they are.
- A release up to 0.34 reads only the notes under its own scope; after a refile the notes it filed
  under another scope are not visible to it until its own scope is current again.
- Clones of one remote keep separate notes until export and import exist.

## Alternatives considered

- **Key the store by the origin scope.** Shares notes across clones, but needs a lock-free or
  multi-process store, a merge of diverged clones, and a migration of every existing store.
