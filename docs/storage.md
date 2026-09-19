# Nisse Local Storage

This document describes Nisse's regenerable local SQLite storage. The
authoritative specification is [PLAN.md](../PLAN.md) (sections 24 and 29);
where the two disagree, PLAN.md wins.

Durable project records (`.nisse/**`) are **not** SQLite and are not
described here; see [record-format.md](record-format.md).

## Purpose and guarantees

All SQLite state is a **disposable local cache**. It holds the source
index, retrieval data, embeddings, local review history, and pending
learning. It is never authoritative shared memory:

- Losing or deleting it never destroys durable project knowledge, which
  lives exclusively in Git-tracked `.nisse/**` records (permanent
  invariant 4).
- Everything in it can be rebuilt from the Git repository and the
  `.nisse/**` records.
- No init or refresh command is required; databases are created on first
  use.

## Locations

Locations are derived from Git directory semantics — never from a
`.git`-relative guess — so linked worktrees are first-class:

| Database | Path | Scope |
| -------- | ---- | ----- |
| Shared cache | `$GIT_COMMON_DIR/nisse/cache.sqlite` | All worktrees of the repository |
| Worktree cache | `$GIT_DIR/nisse/worktree.sqlite` | One worktree only |

In a linked worktree, `$GIT_DIR` is `<main>/.git/worktrees/<name>` while
`$GIT_COMMON_DIR` is `<main>/.git`, so every worktree shares one physical
shared cache while keeping a private worktree database.

## Tables

Shared cache:

| Table | Contents |
| ----- | -------- |
| `content_artifact` | Content-addressed file artifacts (`git:<oid>` / `sha256:<hash>` keys) |
| `parsed_artifact` | Tree-sitter parse results keyed by content, language, and parser version |
| `symbol` | Definitions extracted from parsed artifacts |
| `reference` | Identifier references extracted from parsed artifacts |
| `text_chunk` | Text chunks used for retrieval |
| `embedding` | Optional cached embedding vectors |
| `review_event` | Local review history (request, snapshot, routing, results) |
| `review_feedback` | Locally recorded finding outcomes |
| `pending_learning` | Learning items awaiting contributor identity or verification |

Worktree cache:

| Table | Contents |
| ------- | -------- |
| `manifest` | Current path → content-key mapping used by lazy refresh |
| `path_content` | Per-path content-key history and metadata |
| `record_index` | Index of durable `.nisse/**` records for retrieval |
| `refresh_state` | Statistics of the last index refresh |

## Schema versioning and migrations

Each database records its schema version in the SQLite `user_version`
pragma. Schema evolution is forward-only: each migration is one
transactional batch of SQL plus the `user_version` bump. Opening a
database applies all pending migrations automatically.

A database whose `user_version` is **newer** than this Nisse supports was
written by a newer Nisse. It is never opened and never deleted by this
version; the operation fails with a typed error asking for an upgrade.

## Opening, corruption policy, and recovery

All subsystems open caches through one code path:

- **Open** (`open_or_recover`): creates missing databases, applies pragmas
  (WAL journaling, busy timeout, foreign keys), and migrates the schema
  forward. If the file exists but is corrupt (SQLite reports
  not-a-database or corruption), it is **deleted together with its
  journal sidecars and rebuilt empty** — cache content is regenerable, so
  conservative failure behavior here means automatic, *reported*
  recovery, not a wedged cache. Recovery is always surfaced, never
  silent.
- **Status** (`nisse cache status`): purely diagnostic. It never creates,
  migrates, or recovers a database. Corrupt or too-new caches are
  reported as problems (non-zero exit) with the suggestion to run
  `nisse cache clear`.
- **Clear** (`nisse cache clear`): deletes both databases of the current
  worktree (including `-wal`/`-shm`/`-journal` sidecars) and recreates
  them empty at the current schema version. It works regardless of
  database content, because the old file is removed first.

Concurrent short-lived Nisse processes coordinate through the SQLite busy
timeout; the shared cache is the only database two worktrees open
together.
