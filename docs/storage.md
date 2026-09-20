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
| `text_chunk_fts` | FTS5 index over `text_chunk` (source skeletons) for baseline retrieval |
| `embedding` | Optional cached embedding vectors |
| `review_event` | Local review history (request, snapshot, routing, results) |
| `review_feedback` | Locally recorded finding outcomes |
| `pending_learning` | Learning items awaiting contributor identity or verification |

Worktree cache:

| Table | Contents |
| ------- | -------- |
| `manifest` | Current path → content-key mapping used by lazy refresh |
| `path_content` | Per-path content-key history and metadata; the snapshot-membership authority for retrieval |
| `record_index` | Index of durable `.nisse/**` records for retrieval, including the per-snapshot `[potentially stale]` marking of facts (plan section 48) |
| `record_fts` | FTS5 index over active facts and lessons |
| `practice_fts` | FTS5 index over the bundled practice packs |
| `refresh_state` | Statistics of the last index refresh |
| `meta` | Small key-value markers: the snapshot binding of the last refresh and the indexed practice-catalog version |

FTS5 shadow tables (for example `text_chunk_fts_data`) are SQLite
implementation detail, not part of the contract.

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

## Source index refresh

The source index (plan sections 27–28) is refreshed lazily by normal
operations; no init or refresh command exists. One refresh enumerates
exactly one review snapshot through the snapshot reader — Git decides
membership, so cache presence never proves snapshot membership.

Content keys are a pure function of content, never of paths:

| Key                | Content                                                       |
| ------------------ | ------------------------------------------------------------- |
| `git:<blob-oid>`   | Content that exists as a Git blob (index and commit-tree sides, and clean working-tree files whose bytes hash to their index blob OID) |
| `sha256:<hex>`     | Dirty or untracked working-tree content                        |

Parsed artifacts are cached in the **shared** cache keyed by
`(content_key, language, parser_version)` — identical bytes always
produce the same artifact, which is what lets artifacts be reused across
branches and worktrees. Derived `symbol`, `reference`, and `text_chunk`
skeleton rows are written when an artifact is first parsed.

The **worktree** `manifest` maps the paths of the last refresh; a refresh
reuses artifacts whose keys are already cached (manifest hit or shared
cache hit), parses only uncached content, and prunes manifest rows for
paths absent from the target snapshot. `refresh_state` records the
statistics of the last refresh (`files_seen`, `files_reused`,
`files_parsed`, `files_removed`, duration), and `meta` records the
snapshot descriptor the refresh ran for, so later retrieval can refuse
to answer with membership data of a different snapshot (cache presence
never proves snapshot membership).

## Baseline retrieval

Retrieval (plan sections 30–31) works without embeddings and without
LLMs. Signals: exact symbol lookup, the reference (relation) graph,
FTS5 over source skeletons (`text_chunk_fts`), FTS5 over active durable
facts/lessons (`record_fts`), FTS5 over bundled practices
(`practice_fts`), and changed-symbol/changed-path/module-proximity
boosts. Ranking is deterministic; conflicting facts are marked and
downranked, superseded records are not retrieved as current knowledge,
and every source entry is filtered through the target snapshot's
membership (`path_content`) before it can enter a result. When an
embedding provider is configured, a bounded leading candidate set is
cosine-reranked on top of this baseline; vectors are cached in the
shared `embedding` table with their text stored as `text_chunk` rows
under synthetic `emb:<sha256>` content keys (path sentinel
`(embedding-cache)`, never part of the FTS index), keyed per embedding
model so model or dimension changes never mix geometries. Provider
failures degrade to the unchanged baseline ranking. `nisse query`
refreshes the working-tree snapshot lazily and prints a bounded,
ranked context bundle (`--json` for machine-readable output).

Indexed languages (tree-sitter): C, C++, C#, Dart, Rust, JavaScript,
TypeScript, Python, Bash, Lua, HTML. Files whose content is not valid
UTF-8 are not indexed. Extensionless files are indexed when their first
line carries a recognized Python, bash/sh, or Lua shebang (`env` and
`env -S` forms included); a `sh` shebang indexes as `bash`. A recognized
extension always wins over contradictory content, and files with any
extension are never content-sniffed.
