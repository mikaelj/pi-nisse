# Nisse Architecture Contract

This document is a concise summary of the Nisse 1.0 architecture. The authoritative specification is [PLAN.md](../PLAN.md). Where the two disagree, PLAN.md wins.

## Purpose

Nisse is a local software-review engine. It reviews plans and code changes produced by other agents or humans. It never implements the reviewed work.

Nisse Core exposes exactly five operations:

| Operation              | Meaning                                  |
| ---------------------- | ------------------------------------------ |
| `review_plan`          | Review a plan produced by another agent  |
| `review_code`          | Review code changes produced by another  |
| `review_investigation` | Review an investigation's claims         |
| `query_project`        | Query indexed project knowledge          |
| `record_feedback`      | Record known review outcomes             |

Nisse Core has no Plan mode, Build mode, or task state machine. Workflow policy (review loops, attempt limits, escalation to the user) belongs to coding-agent integrations, primarily the Pi package.

## Components

```
MAIN PI
   |
   v (Pi extension tools / CLI / MCP — same core)
+------------------+
|   NISSE CORE     |  Rust executable: Git, snapshots, diffs, indexing,
|     (Rust)       |  retrieval, records, routing constraints, validation,
+------------------+  synthesis constraints, persistence.
   |          \
   |           +-- durable project records (.nisse/**, Git-tracked)
   |           +-- local regenerable state ($GIT_COMMON_DIR/nisse/,
   |                                            $GIT_DIR/nisse/)
   v
 PI REVIEW HOST    TypeScript child process: isolated Pi SDK reviewer
   |               sessions, narrow semantic jobs, strict schemas.
   v
JuryPlan (quality + 1..3 specialists, max 4) -> constrained merge -> ReviewResult
```

- **Pi review host** (`runtime/pi-host`): a child process, not a daemon. It performs only the semantic work the Rust core assigns. Reviewer sessions are isolated: explicit model, thinking level, instructions, tools, and context; they do not inherit Main Pi's conversation, extensions, or skills.
- **Primary Pi package** (`integrations/pi`): thin extension plus skill that owns the bounded plan-review and code-review workflows.
- **Needle adapter** (optional): classification and embeddings only. Needle failure degrades to deterministic behavior.
- **MCP adapter**: generic stdio integration calling the same core APIs.

## Central design rule

LLMs are untrusted semantic workers, not orchestrators.

- Every decision that can reasonably be made deterministically is made in the Rust core.
- LLM outputs use strict schemas; malformed output causes less automation, not guessed state.
- LLMs never directly mutate durable project knowledge. Fact persistence requires independent verification.
- Synthesis may only consolidate existing validated findings; it can never invent findings or raise severity.
- The Git snapshot is authoritative; all source access resolves against the target snapshot. Cache presence never proves snapshot membership.
- Reviewer source access is read-only and budget-bounded.
- Uncertainty is represented explicitly and produces less automation.

## Review pipeline

`review plan` executes the frozen pipeline of PLAN.md section 58 against the working-tree snapshot (section 25): lazy index refresh, deterministic request descriptors from the plan, preliminary fact/lesson retrieval, rules-based routing, deterministic jury construction (`quality` always, 2..=4 jurors, dynamic-domain-expert fallback), bounded initial reviewer contexts led by the plan itself (section 23), independent reviewer execution over the Pi host with the one-retry policy and the two-valid-reports minimum (section 36), finding/evidence validation against the snapshot, consolidation, and the local review event in the shared cache (section 57). The dynamic fallback's domain description is a constrained, validated semantic input only; when its session fails, the static expert template is used unchanged.

## Storage

- **Durable shared knowledge**: Git-tracked `.nisse/**` immutable JSON records (observations, facts, lessons) with ULID IDs, UTC RFC3339 timestamps, and schema version 1. Records preserve provenance; new knowledge creates new records rather than mutating old ones. Details: [record-format.md](record-format.md).
- **Local generated state**: SQLite under `$GIT_COMMON_DIR/nisse/` (shared cache) and `$GIT_DIR/nisse/` (worktree state). Fully regenerable; its loss never destroys durable knowledge. Details: [storage.md](storage.md).
- **Trust**: `.nisse/**` is project data and can never configure commands, endpoints, credentials, or extensions. Runtime/provider configuration is local trusted TOML configuration with frozen precedence: repository-local `$GIT_COMMON_DIR/nisse/config.toml`, then the user config at `$HOME/.nisse/config.toml` (dotfile convention on every platform; the legacy platform-native path is read with a migration notice for one release), then compiled defaults. Default model endpoints are loopback/ private only; hostnames are resolved before the trust decision, and `network.allow_non_private_endpoints` is the explicit override. `nisse doctor` reports Git availability, configuration validity, model profiles, endpoint trust, and optional classifier status.

## CLI surface (frozen)

```
nisse --version
nisse doctor
nisse review plan --file <path> | --stdin [--json]
nisse review code --working | --staged | --commit <oid> | --range <base>..<head> | --branch <base-ref> [--json]
nisse query "<query>" [--json]
nisse feedback <review-id> <finding-id> (--fixed | --accepted | --dismissed)
nisse records list [--kind <observation|fact|lesson>]... [--oneline] [--limit <N>] [--json]
nisse cache status | clear
nisse eval routing | retrieval
nisse mcp
```

stdout is reserved for machine-readable output; logs and diagnostics go to stderr.

## Implementation languages and stack

- **Nisse Core**: Rust (edition 2024), tokio, clap, serde, thiserror/anyhow, tracing, rusqlite (bundled SQLite), tree-sitter, official Rust MCP SDK.
- **Pi review host / Pi package**: TypeScript using the official Pi SDK.
- **Git**: the installed `git` executable is the semantic authority.

## Milestone discipline

Implementation proceeds one frozen milestone at a time, committed as `nisse: complete milestone NN - <name>`. See PLAN.md for milestones, invariants (section 71), and acceptance criteria.
