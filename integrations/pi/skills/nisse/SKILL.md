---
name: nisse
description: Nisse local software-review engine. Use for substantial coding tasks: draft a plan, run a bounded Nisse plan review (max 3 attempts), implement, test, run a bounded Nisse code review (max 3 attempts), fix valid blocker/high findings, and submit known finding outcomes.
---

# Nisse — Local Software Review

Nisse reviews plans and code that **you** (Main Pi) produce. Nisse never
writes or fixes source itself; it returns structured `ReviewResult` JSON
and you act on it. Nisse Core has no task workflow of its own — this
skill defines your workflow around its four tools:

| Tool | Use |
| --- | --- |
| `nisse_review_plan` | Review an implementation plan (plan text in, findings out). |
| `nisse_review_code` | Review code changes for one snapshot scope (`working`, `staged`, `commit`, `range`, `branch`). |
| `nisse_query_project` | Query the indexed project (symbols, facts, lessons, practices). Deterministic, no jury. |
| `nisse_feedback` | Record a known outcome (`fixed` / `accepted` / `dismissed`) for one finding. |

Prerequisites: the `nisse` binary on `PATH` (overridable with the
`NISSE_BIN` environment variable). When reviews fail oddly, ask the user
to run `nisse doctor`.

## Substantial vs. trivial work

Use Nisse for tasks involving any of: architecture changes; multiple
interacting files/modules; concurrency; realtime behavior; embedded
constraints; persistence; database schema; networking/protocols;
authentication/security; compatibility; public API changes; migrations;
complex bug fixes; substantial refactors; performance-sensitive
behavior; important project-specific constraints.

You may skip automatic Nisse review for: spelling; formatting;
comments only; obvious mechanical renames; trivial generated changes;
clearly non-risky one-line edits.

An explicit user instruction to use Nisse always overrides these
heuristics.

## Workflow for substantial tasks

The frozen phase sequence — do not reorder, skip, or unbound these
phases, and do not invent a different review loop:

    understand
      → draft plan
      → plan-review loop     (max 3 attempts; stop for the user if unresolved)
      → implement
      → project tests
      → code-review loop     (max 3 attempts; stop for the user if unresolved)
      → finish

These bounds are the Nisse workflow contract for Main Pi. The loops
above are the only review loops; follow them exactly.

### 1. Understand

Investigate the task and project until you understand the requested
change. Nisse does not write the plan for you.

### 2. Draft a plan

Write a concrete implementation plan specific enough to review: files
and modules, interfaces, data flow, compatibility constraints, tests,
migration strategy, error handling, rollout implications.

### 3. Plan-review loop (max 3 attempts)

Every `nisse_review_plan` call — the first included — consumes one of
the three attempts.

1. Call `nisse_review_plan` with the complete plan text.
2. Inspect the findings, focusing on severity `blocker` and `high`.
3. For each valid blocker/high finding: revise the plan to address it.
4. Call `nisse_review_plan` again with the revised plan.

Exit the loop when the review has **no blocker/high findings**.
Medium/low/info findings do not block — incorporate them when useful.

If blocker/high findings remain after the **third** attempt: **stop
before implementation**, explain the unresolved findings to the user,
and ask for direction. Do not continue autonomously into
implementation.

### 4. Implement

Implement the reviewed plan yourself. You remain the coding agent;
Nisse does not modify source.

### 5. Project tests

Run the project's appropriate tests/checks **before** requesting code
review. If basic tests fail, fix them first.

### 6. Code-review loop (max 3 attempts)

Every `nisse_review_code` call — the first included — consumes one of
the three attempts.

1. Call `nisse_review_code` (default scope `working`: HEAD vs the final
   working tree; use `staged` to review only the index, `commit`,
   `range`, or `branch` for history).
2. Investigate each blocker/high finding. If valid, fix it.
3. Run the relevant project tests again.
4. Record known outcomes with `nisse_feedback` (see below).
5. Call `nisse_review_code` again.

Exit the loop when the review has **no blocker/high findings**. Treat
medium/low/info findings by relevance and scope; they do not block
completion.

If blocker/high findings remain after the **third** attempt: do **not**
claim the task is complete; report the unresolved findings to the user
and ask for direction.

### 7. Finish

Report the task complete only when: implementation tests pass, no
unresolved blocker/high finding remains, and both bounded review loops
finished successfully. Anything else — including either loop hitting
its attempt limit — means the task is **not** complete and the user
must hear about the unresolved findings.

## Trivial tasks

For trivial work (see the guidance above) you may work without Nisse:
understand → implement → test → finish. Do not invoke any Nisse tool
for a trivial task **unless the user explicitly asks for Nisse** — an
explicit request always runs the substantial-task workflow above.

## Feedback semantics

After a code-review loop, submit one `nisse_feedback` call per finding
whose outcome you actually know:

- `fixed` — the implementation changed specifically to address the finding.
- `accepted` — the finding is valid but intentionally not fixed now.
- `dismissed` — you investigated and judged it incorrect or not applicable.
- Unknown outcome — **submit nothing**. Do not infer an outcome merely
  from later source changes.

Repeated submissions for the same finding are idempotent.

## Reading a ReviewResult

```json
{
  "review_id": "01J…",
  "kind": "plan" | "code",
  "created_at": "…",
  "snapshot": { "mode": "…" },
  "summary": "…",
  "findings": [
    {
      "id": "01J…",
      "impact_class": "correctness" | "security" | …,
      "severity": "blocker" | "high" | "medium" | "low" | "info",
      "category": "…",
      "title": "…",
      "claim": "…",
      "explanation": "…",
      "recommendation": "…",
      "evidence": [{ "kind": "source", "path": "…", "excerpt": "…" }],
      "cross_validated": true
    }
  ]
}
```

Blocker/high findings always carry concrete evidence. Judge validity
before acting; when unsure, use `nisse_query_project` or read the cited
evidence. Use `review_id` + `findings[].id` for feedback.
