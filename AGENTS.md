# AGENTS.md

## Nisse — software-review engine

Nisse reviews plans, code changes, and investigation results produced by you (the agent) or by a human. It never writes or fixes source itself. Use it for substantial work; skip it for trivial changes.

### Tools

| Tool | When |
| --- | --- |
| `nisse_review_plan` | after drafting an implementation plan, before writing code |
| `nisse_review_code` | after implementing and passing the project's tests |
| `nisse_review_investigation` | when research/investigation conclusions will drive action (claim verdicts + coverage gaps; no fix loop) |
| `nisse_query_project` | to retrieve indexed project knowledge mid-task |
| `nisse_feedback` | after each review round, once per finding whose outcome you actually know |

### Substantial vs trivial

Substantial: architecture changes, multi-module interactions, concurrency, persistence, schema, networking, auth/security, compatibility, migrations, complex bug fixes, public API changes. Trivial: spelling, formatting, comment-only edits, obvious mechanical renames. An explicit user instruction to use Nisse always overrides these heuristics.

### Bounded workflow (frozen)

Plan loop: `nisse_review_plan` → fix valid blocker/high findings → review again; max 3 attempts — if unresolved blocker/high findings remain, stop and ask the user. Then implement, run the project's tests, and run the code loop with `nisse_review_code` under the same bounds; do not claim completion with unresolved blocker/high findings. Feedback outcomes: `fixed` (implementation changed and tests pass), `accepted` (valid but intentionally deferred), `dismissed` (investigated and judged incorrect) — submit nothing when the outcome is unclear; outcomes are immutable attestations, and a conflicting resubmission fails with a typed conflict.

### Reading results

Findings carry `id`, `severity` (blocker / high / medium / low / info), `claim`, and evidence. Blocker/high findings always cite concrete evidence validated against the review snapshot. Use `review_id` + `findings[].id` for feedback; short unambiguous prefixes of both ids are accepted.

### First use (directory-creation consent)

Nisse asks the user before creating any persistent directory (`.nisse/**`, the cache directories under `$GIT_DIR`, `$HOME/.nisse`). Noninteractive runs fail closed with a typed `directory creation requires consent` error, or defer feedback outcomes to `nisse records pending`. Remedy: have the user run `nisse doctor` once per repository, or add the path to `[directories] allow_paths` in `$HOME/.nisse/config.toml`, or set `NISSE_ALLOW_DIR_CREATION=1` (CI/tests). Never change local consent configuration on the user's behalf.

### Config paths

User config: `$HOME/.nisse/config.toml` (mode 0600; dotfile on every platform). Repo config: `.nisse/config.toml` — note `.nisse/**` is project data and can never configure executables, endpoints, credentials, or filesystem roots. Model profiles (`standard`, `strong`) live in user config; reviews never inherit your model.

### Docs convention

Markdown prose is one paragraph per line — no manual hard-wrap. Code fences, tables, list hanging indentation, intentional hard breaks, and reference-link definitions keep their explicit structure. PLAN.md and PLAN_INVESTIGATION.md are frozen contracts and are never reflowed.

### When Nisse fails

- `directory creation requires consent` → see "First use" above.
- `model profile not resolved` → run `nisse doctor`.
- Review looks stale or fails oddly → ask the user to run `nisse doctor`; the binary is resolved from `PATH` or `NISSE_BIN`.
