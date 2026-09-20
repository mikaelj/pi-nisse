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

## Fact staleness (milestone 21)

Code-derived facts retain content provenance: when the fact pipeline
persists a supported fact, Rust stamps every source evidence entry
with the content key of the exact snapshot content it was verified
against. A reviewer never supplies its own provenance.

Against every target snapshot, each active supported fact is assessed
deterministically (plan section 48):

- a cited path absent from the snapshot — deleted content — makes the
  fact potentially stale;
- a recorded content key that no longer matches the snapshot content
  — modified content — makes it potentially stale;
- keyless evidence (written before provenance stamping) falls back to
  excerpt containment.

Potentially stale facts are never silently current truth (permanent
invariant 22): retrieval marks them `[potentially stale]` and
downranks them to the weight of conflicting facts — both in `nisse
query` and in reviewer initial context. The marking lives in the
regenerable record index and is recomputed on every record sync
because it is snapshot-dependent, never part of the immutable record.

Stale facts stay eligible for **targeted revalidation**: one fresh,
independent Pi verification session judges the exact claim against
evidence rebuilt from the *current* snapshot content and answers
`supported`, `contradicted`, or `insufficient`:

| Verdict        | Durable effect                                                            |
| -------------- | ------------------------------------------------------------------------- |
| `supported`    | new supported fact record, provenance refreshed, supersedes the old one   |
| `contradicted` | new fact record in state `unresolved_conflict`, supersedes the old one    |
| `insufficient` | nothing updated; the attempt stays pending local state                    |

The old record always remains for provenance. When no current source
support can be rebuilt at all, revalidation is deterministically
`insufficient` without a session. Without a contributor identity no
durable record is created (plan section 44).

## Feedback outcomes (plan section 49)

| Outcome    | Meaning                                                        |
| ---------- | -------------------------------------------------------------- |
| `fixed`    | implementation changed specifically to address the finding      |
| `accepted` | finding valid but intentionally not fixed now                   |
| `dismissed`| finding investigated and judged incorrect or not applicable     |

Positive outcomes (lesson statistics) are `fixed` and `accepted`.

## Knowledge consolidation (milestone 22)

`nisse records compact` compacts the *semantic* duplication of durable
knowledge into `.nisse/common/` without losing provenance (plan
sections 52–56). One operation applies to one contributor's records
and to many contributors' records alike: authorship affects only the
provenance of the created records, never the semantics. Observations
are never compacted away — they remain the authoritative raw learning
data forever (plan section 53). Repeated runs are idempotent, and the
original records always remain stored; only the *active* (non-
superseded) set shrinks.

### Fact consolidation

```text
active supported facts
  |
  +-- same normalized (subject, text) ------> deterministic exact duplicates
  |                                            |
  |                                            v
  |                          representative statement verbatim + union(tags,
  |                          evidence), one independent verification
  |
  +-- distinct statements, same subject -----> semantic group
                                               |
                                     fresh Pi synthesis session
                                               |
                          MERGED <candidate>          CANNOT_MERGE
                               |                         |
                     independent verifier          conflict handling
                               |                         |
                          SUPPORTED --> common      per-claim verification
                          fact under .nisse/common  against current evidence
                                                        |
                          exactly one SUPPORTED, all others CONTRADICTED
                                     |                        |
                                     v                        v
                          resolved common fact      unresolved conflict
                          (winner verbatim)         (originals stay separate)
```

Rules frozen by the plan:

- **Verification always gates durability** (permanent invariant 10):
  even the deterministic exact-duplicate candidate is independently
  verified before it becomes a common fact.
- **Synthesis may only narrow.** The synthesis session produces *the
  narrowest statement supported by the supplied facts and their
  evidence* or answers `CANNOT_MERGE`; it is never instructed to
  invent a compromise (plan sections 54–55).
- **The verifier can only reject.** A `CONTRADICTED` merged candidate
  never becomes knowledge; an `INSUFFICIENT` verdict (or a failed
  session) leaves the whole group untouched — uncertainty produces
  less automation, never guessed truth.
- **Conflicts are preserved.** Resolution happens only when the
  current evidence clearly supports exactly one claim (one
  `supported`, every other `contradicted`); the winning formulation is
  carried **verbatim**. Otherwise the conflicting facts remain
  separate and active, and nothing is written.
- **Provenance is explicit.** Every common fact records all source
  record IDs in `source_record_ids` and supersedes them (`supersedes`);
  the originals stay on disk for provenance.

Grouping is deterministic (normalized statement, normalized subject;
groups are bounded at 16 members). Optional Needle/embedding
similarity may only widen the semantic groups; the baseline never
needs it.

### Lesson consolidation

Compatible lessons — active lessons whose unique source observations
form one `(reviewer, category)` group — merge with **no LLM at all**:

1. union and deduplicate `source_observation_ids`;
2. reload the unique original observations;
3. recompute the statistics from those unique observations — never
   `merged_support = support_A + support_B`, overlapping evidence is
   counted once (permanent invariant 7);
4. recompute the deterministic routing hint and wording;
5. create the common lesson superseding the originals (which remain
   stored together with every observation).

If the recomputed statistics miss the frozen lesson threshold, the
group creates nothing.

### Reporting and identity

`nisse records compact` prints one deterministic report (per-group
relation and outcome, diagnostics, and a summary line). Creating
durable records requires a contributor identity: interactive use may
prompt for one on first use (plan section 44); noninteractive use
fails explicitly, and nothing is written — the consolidation output
is re-derivable at any time.
