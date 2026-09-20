# Nisse Learning

This document describes how Nisse turns review outcomes into durable
project knowledge. The authoritative specification is
[PLAN.md](../PLAN.md) (sections 41–56); where the two disagree,
PLAN.md wins.

See also [record-format.md](record-format.md) for the frozen record
schemas and [storage.md](storage.md) for the local caches.

## Principles

- **LLMs never control learning.** Reviewers and other Pi sessions may
  *propose* knowledge; Rust validates, verifies, counts, and persists
  every durable fact, observation, and lesson (plan section 7).
  LLMs may at most contribute concise human-readable wording, which
  never controls behavior (plan section 51).
- **Observations are the source of truth.** Every statistic — lesson
  support, positives, dismissals — is recomputable from the immutable
  observation records it references (permanent invariant 7). Any stored
  aggregate is a cache.
- **Uncertainty produces less automation.** Without independent
  verification no fact is durable; without a contributor identity no
  record is durable at all — learning stays pending local state (plan
  section 44).

## The learning pipeline

```text
review
  |
  v
fact candidates ── deterministic validation ── independent Pi verification
  |                                                    |
  |                                        SUPPORTED   CONTRADICTED / INSUFFICIENT
  v                                            v                v
durable fact                            .nisse/people/<id>/   pending/rejected
  |
  +── review findings ── known outcome (`nisse feedback`)
                          |
                          v
                 immutable observation records
                          |
                          v
              deterministic lesson maintenance
                          |
              threshold met (5 / 4 / 1)
                          |
                          v
                 immutable lesson record
```

## Fact candidates (milestone 18)

Reviewers may attach `candidate_project_facts` to their reports. A
candidate becomes a durable supported fact only when:

1. **Deterministic validation** passes (plan section 47): confidence
   ≥ 0.90, schema and text bounds, evidence validated against the exact
   target snapshot, at least one source-grounded evidence entry, and
   obvious-secret filtering.
2. **Independent verification** by a fresh Pi session (never the
   proposing reviewer) returns `SUPPORTED` for the exact claim against
   the exact evidence excerpts.
3. A contributor identity is available.

`CONTRADICTED` candidates are rejected; everything else — verifier
failures, `INSUFFICIENT`, missing identity — remains pending local
state in the regenerable shared cache (`nisse records pending`), never
durable project knowledge.

## Observations (milestone 19)

A known outcome for a review finding (`fixed`, `accepted`, or
`dismissed`) creates one immutable observation per reviewer that
produced the finding. Rust constructs every authoritative field from
the stored review, the stored finding, the explicit outcome, and the
snapshot descriptors — LLMs construct no observation identity
(plan section 50).

- Unknown outcomes remain unknown: nothing is inferred from later
  source changes.
- Submissions are idempotent: the same outcome for the same
  (review, finding) pair is a no-op; a *different* outcome is a typed
  conflict, because durable records are immutable.

## Lessons (milestone 20)

Lessons are routing experience derived deterministically from repeated
observation outcomes (plan sections 43 and 51):

1. **Group** observations by their stable descriptors: the finding's
   reviewer and category. Observations of all contributors feed the
   same group; the lesson is authored by the contributor whose
   maintenance pass completed it.
2. **Deduplicate**: statistics count unique original observations.
   Duplicate record IDs and semantically duplicated outcomes (the same
   review/finding outcome recorded again, e.g. after a branch merge)
   count once.
3. **Compute** support, positives (`fixed`/`accepted`), dismissals,
   and the routing conditions in Rust:
   - the group's path module — the longest directory prefix shared by
     every cited path — becomes the hint's single `<module>/**` path
     glob; observations without paths, or paths without a common
     module, yield unconditional hints;
   - the tags shared by every observation become the hint's
     `required_tags_any`;
   - the boost is `0.35 × positives / support`, capped at `0.35`.
4. **Apply the frozen threshold**: `unique ≥ 5`, `positive ≥ 4`,
   `dismissed ≤ 1`. Groups below it create nothing.
5. **Create** the immutable lesson record under
   `.nisse/people/<contributor-id>/` with the deterministic routing
   hint and the sorted unique source observation IDs.

Lesson wording is deterministic in Nisse 1.0 (no LLM participates in
lesson learning). Wording never controls routing: the
[`RulesClassifier`](../src/routing/rules.rs) consumes active lessons
only through their routing hints, which may raise scores but never
suppress a reviewer, and whose total boost per reviewer is capped at
`0.35`.

### Maintenance

Learning maintenance runs at two deterministic points:

- after new observations are recorded by `nisse feedback`;
- as step 21 of every review pipeline (plan section 58).

Maintenance is idempotent and conservative:

- a group already represented by an active lesson referencing exactly
  the same observation set creates nothing;
- a grown group creates a **new** lesson superseding the previous one;
  the original remains for provenance;
- without a contributor identity the deferred learning stays pending
  local state (`nisse records pending`), deduplicated by group;
- storage failures surface as diagnostics and never fail the operation
  that triggered maintenance.

## Feedback outcomes (plan section 49)

| Outcome    | Meaning                                                        |
| ---------- | -------------------------------------------------------------- |
| `fixed`    | implementation changed specifically to address the finding      |
| `accepted` | finding valid but intentionally not fixed now                   |
| `dismissed`| finding investigated and judged incorrect or not applicable     |

Positive outcomes (lesson statistics) are `fixed` and `accepted`.

## What is explicitly not here yet

Knowledge consolidation into `.nisse/common/` — semantic fact merging,
conflict handling, and lesson merging (plan sections 52–56) — is a
later milestone; `nisse records compact` remains unimplemented. Fact
staleness detection (plan section 48) is likewise future work.
