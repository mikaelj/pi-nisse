# Nisse Durable Record Format

This document describes Nisse's durable project records. The
authoritative specification is [PLAN.md](../PLAN.md) (sections 39–56);
where the two disagree, PLAN.md wins.

## Purpose and guarantees

Durable records are Nisse's shared project knowledge: what happened
(observations), what supported evidence says about the project (facts),
and what repeated observations suggest is useful (lessons).

- Records are **immutable after creation**. New knowledge creates new
  records; old records remain for provenance. There is no record
  update, only `supersedes`.
- Records are **Git-tracked project data**. They live in the worktree,
  merge like any other file, and are shared through normal Git
  transport. They never configure Nisse (see
  [security](../PLAN.md) section 10).
- Records are **independent of SQLite**. The local caches are
  regenerable; deleting them never touches records (permanent
  invariant 4).
- Records are **strict JSON**. Unknown fields, unknown enum variants,
  and schema violations are hard errors — never silently repaired.

## Layout

```text
.nisse/
  people/<contributor-id>/<ULID>.json
  common/<ULID>.json
```

- `people/<contributor-id>/` holds one contributor's records. They are
  directly retrievable and never require promotion. Contributor IDs
  match `[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}`.
- `common/` holds consolidated facts and lessons. Observations remain
  contributor records forever (plan sections 45 and 53); storing an
  observation under `common/` is an error.
- File names are the record's own ULID. A record whose file name,
  author, and contributor directory disagree is invalid.
- Record IDs are ULIDs, unique across the whole `.nisse` tree.
- Timestamps are UTC RFC3339 strings. The schema version of Nisse 1.0
  records is `1`.

Every record is a JSON object with a `kind` tag selecting its schema:

```json
{
  "schema_version": 1,
  "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
  "kind": "observation",
  "author": "alice",
  "created_at": "2026-01-19T12:00:00.000Z",
  "...": "kind-specific fields"
}
```

## Kinds

### `observation`

Something that happened. The primary — and in Nisse 1.0 the only —
observation type is `review_outcome`, created by `nisse feedback` from
a stored review, stored finding, and explicit outcome. Full LLM
transcripts are never persisted as observations.

| Field              | Meaning                                        |
| ------------------ | ---------------------------------------------- |
| `observation_type` | `review_outcome`                                |
| `review_id`        | Review the outcome belongs to (ULID)           |
| `finding_id`       | Finding the outcome applies to                 |
| `reviewer_id`      | Reviewer that produced the finding             |
| `outcome`          | `fixed` \| `accepted` \| `dismissed`           |
| `impact_class`     | Impact class carried from the finding          |
| `category`         | Machine-readable category carried from the finding |
| `languages`        | Languages the finding touched                  |
| `tags`             | Topic tags                                     |
| `paths`            | Repository-relative paths (validated)          |
| `snapshot`         | Snapshot descriptor of the review              |

### `fact`

Supported project knowledge. Facts are only ever created after
deterministic validation plus independent verification (plan section
47); a reviewer can propose facts but never persist them. Updated
knowledge supersedes the old record.

| Field                | Meaning                                       |
| -------------------- | --------------------------------------------- |
| `subject`            | Stable subject phrase                         |
| `text`               | The fact statement                            |
| `tags`               | Topic tags                                    |
| `state`              | `supported` \| `unresolved_conflict`          |
| `evidence`           | Evidence references (at least one, required)  |
| `source_review_id`   | Review that proposed the fact, if any         |
| `source_record_ids`  | Records this fact was derived from            |
| `supersedes`         | Record IDs this record replaces               |

Only `supported` facts are treated as normal project truth. Facts in
`unresolved_conflict` state are surfaced as conflicts, never silently
resolved.

### `lesson`

A useful project-specific pattern suggested by repeated observations.
Routing statistics are computed deterministically by the Rust core from
`source_observation_ids`; any stored aggregates are caches.

| Field                    | Meaning                                        |
| ------------------------ | ---------------------------------------------- |
| `subject` / `text` / `tags` | As for facts                                |
| `routing_hint`           | Deterministic routing hint (below)             |
| `source_observation_ids` | Observations the lesson derives from (≥ 1)     |
| `supersedes`             | Record IDs this record replaces                |

`routing_hint`:

| Field               | Meaning                                              |
| ------------------- | ---------------------------------------------------- |
| `reviewer_id`       | Reviewer the lesson boosts                           |
| `path_globs`        | Path globs limiting the boost                        |
| `required_tags_any` | Tags limiting the boost (any-of)                     |
| `boost`             | Score boost, `0.0..=0.35`. Lessons may raise scores but never suppress a reviewer |

## Validation

Structural validation (implemented in `nisse::domain::records`)
enforces, among other rules:

- `schema_version == 1`; `id` is a canonical ULID; `author` is a valid
  contributor slug; `created_at` is UTC RFC3339.
- All referenced IDs (`review_id`, `supersedes`, `source_record_ids`,
  `source_observation_ids`) are canonical ULIDs, unique within their
  list, and never self-referential.
- Paths and path globs are safe repository-relative paths: no
  absolute paths, `..`, or drive prefixes.
- Snapshot object IDs are lowercase hex (40 or 64 digits).
- Text fields are non-empty and bounded (e.g. subjects ≤ 256 bytes,
  bodies ≤ 8 KiB, excerpts ≤ 4 KiB); list sizes are bounded.
- Facts carry at least one evidence reference; lessons carry at least
  one source observation; lesson boosts stay within `0.0..=0.35`.

Structural validation proves a record is well-formed, not that its
claims are semantically true. Semantic trust requires the verification
pipelines of plan sections 47–48.

## Storage semantics

- **Atomic, immutable creation**: records are written to a temporary
  file in the destination directory, fsynced, then published with a
  create-if-absent link operation. An interrupted write leaves an
  ignorable `.nisse-tmp-*` file, never a partial record. Creating an
  existing record ID fails; existing files are never overwritten.
- **Strict loading**: every `.json` file under `people/*/` and
  `common/` must parse, validate, and match its location. Dot-prefixed
  entries (temporary files, OS droppings) are ignored; everything else
  invalid is a hard error naming the offending path.
- **Merge safety**: every record lives at a path derived from its own
  identity, so records created on different branches or worktrees never
  collide. Two-branch merges and multi-worktree use are supported by
  construction; duplicate IDs across the tree are rejected.
- **Supersedes**: superseding never mutates the old record. Consumers
  compute the set of superseded IDs (for marking, downranking, or
  filtering) from the records themselves.

## Contributor identity

Author IDs refer to contributors. Contributor identity is trusted
local configuration (platform-native user config), never stored under
`.nisse/**`. When interactive use first needs an identity and none
exists, Nisse asks `What should Nisse call you?` and saves the answer
locally. Noninteractive runs never wait for input: without an identity,
learning that needs an author stays pending locally instead
(plan section 44).
