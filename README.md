# Nisse

Nisse is a local software-review engine. It reviews implementation plans
and code changes produced by other agents or humans — it never implements
the reviewed work itself.

Status: Nisse 1.0 — the frozen implementation plan
([PLAN.md](PLAN.md)) is complete.

Nisse Core exposes exactly four operations:

| Operation         | Meaning                                                |
| ----------------- | ------------------------------------------------------ |
| `review_plan`     | review a plan produced by another agent or human       |
| `review_code`     | review code changes produced by another agent or human |
| `query_project`   | query indexed project knowledge                        |
| `record_feedback` | record known review outcomes as durable observations   |

The same four operations are served by the CLI, the Pi integration, and
the MCP adapter. Nisse Core is operation-based: it has no Plan mode, no
Build mode, and no task state machine — review-loop policy belongs to
coding-agent integrations, primarily the [Pi package](#pi-integration).

The authoritative specification is the frozen [PLAN.md](PLAN.md); the
concise architecture summary is
[docs/architecture-contract.md](docs/architecture-contract.md).

## Architecture

```
MAIN PI  (or CLI user / MCP client)
   |
   v  extension tools / CLI / MCP — one shared Rust core
+-------------------+
|     NISSE CORE    |   Rust: Git & worktree semantics, snapshots,
|       (Rust)      |   content-addressed source index, SQLite retrieval,
+-------------------+   routing constraints, evidence validation, records,
   |        |        |  learning statistics, persistence
   |        |        +-- durable project records  (.nisse/**, Git-tracked)
   |        +----------- local regenerable caches  ($GIT_DIR/nisse/*.sqlite)
   +-------------------- Git as the source authority
   |
   v  frozen JSONL protocol over stdin/stdout
+-------------------+
|  PI REVIEW HOST   |   TypeScript child process (runtime/pi-host):
|    (TypeScript)   |   isolated Pi SDK reviewer sessions with explicit
+-------------------+   model/instructions/checklist/read-only tools
   |  |  |
   v  v  v
 Pi #1..#4 (jury)  ->  constrained merge (Rust)  ->  ReviewResult -> MAIN PI
```

Key design rules (see [docs/architecture-contract.md](docs/architecture-contract.md)):

- **LLMs are untrusted semantic workers, not orchestrators.** Reviewer
  sessions perform narrow semantic work with strict output schemas; Rust
  owns every deterministic decision and all durable state.
- **Git is the source authority.** Every review targets an exact
  snapshot; cache presence never proves snapshot membership.
- **Generated caches are disposable.** Deleting all SQLite state never
  loses durable project knowledge (it lives in Git-tracked records).
- **The reviewer host is not a daemon.** It is spawned per review batch
  and disposed afterwards; no service runs between commands.

## Installation

Prerequisites:

- **Git** (an installed `git` executable is used as the semantic
  authority for repositories, worktrees, and snapshots);
- **Rust** — the exact toolchain is pinned in
  [rust-toolchain.toml](rust-toolchain.toml) (rustup picks it up
  automatically);
- **Node.js >= 23.6** for the Pi review host (`runtime/pi-host`), which
  runs the TypeScript host directly with node type stripping.

Build and install from source:

```
cargo install --locked --path .
# or just build:  cargo build --release
```

Then install the reviewer-host dependencies once:

```
cd runtime/pi-host
npm install
```

`npm install` provides the TypeScript toolchain and the Pi coding-agent
SDK used by real reviewer sessions. The deterministic fake and scripted
runtimes used by the test suite work without any network or provider.

Finally, verify the local setup:

```
nisse doctor
```

Release builds for macOS (arm64, x86_64), Linux (x86_64, arm64), and
Windows (x86_64) are produced by the release workflow
([.github/workflows/release.yml](.github/workflows/release.yml)) on
version tags; per-commit quality gates run in
[CI](.github/workflows/ci.yml) on all three platforms.

## Configuration

Nisse is configured with trusted local TOML. The Git-tracked `.nisse/**`
tree is project *data* and is never a configuration source.

Layered precedence, highest first (frozen in PLAN.md §11):

1. explicit CLI options (the frozen CLI defines none);
2. repository config: `$GIT_COMMON_DIR/nisse/config.toml` — local
   machine state inside the Git dir, never committed;
3. user config: `~/.nisse/config.toml` on every platform (dotfile
   convention, including macOS; a legacy platform-native config file,
   if present, is still read with a per-run migration notice);
4. compiled defaults.

Example:

```toml
[contributor]
id = "alice"                       # durable-record authorship (user layer only)

[network]
allow_non_private_endpoints = false

[model_profiles.standard]
model = "my-local-model"
endpoint = "http://127.0.0.1:8080/v1"
thinking_level = "medium"

[model_profiles.strong]
model = "my-strong-model"
endpoint = "http://10.0.0.5:8080/v1"

[classifier]                       # optional Needle (see below)
provider = "none"                  # "needle" + endpoint enables it
# endpoint = "http://127.0.0.1:8787"

[embeddings]
provider = "none"

[learning]
enabled = true

[performance]
max_concurrent_reviewers = 4       # 1..=16, default = frozen jury max
```

### Model profiles

Reviewer sessions never inherit Main Pi's active model. Nisse selects
explicitly configured **named model profiles**; no commercial model name
is hard-coded anywhere in the architecture. The required conceptual
profiles are `standard` and `strong` (reviews fail closed until both
exist); `fast` and `strongest` are optional. Each profile names a model,
an endpoint, and an optional thinking level. `nisse doctor` reports the
configured profiles.

### Investigation reviews

`nisse review investigation --task <...> --result <...>` reviews
whether an investigation's claims and conclusions hold up. Each input
accepts literal text, an explicit `@file` reference, or a bare value
naming an existing file. The jury separates claim review (verdicts:
supported / partially supported / contradicted / insufficiently
supported / not applicable / outside scope, each with a confidence tier
from demonstrated to unsupported, plus overstated-conclusion notes)
from coverage review (important areas the investigation missed; gaps
mean incompleteness, never proven findings). Verdict merges are
deterministic and most-skeptical: reviewer disagreement is surfaced,
never averaged away. See [PLAN_INVESTIGATION.md](PLAN_INVESTIGATION.md).

### Directory-creation consent

Nisse never creates a persistent directory (the Git-tracked
`.nisse/**` record tree, the `$GIT_DIR/nisse/` and
`$GIT_COMMON_DIR/nisse/` caches, or `$HOME/.nisse` itself) without
your explicit consent:

- the first time a directory would be created, interactive runs ask
  `Allow? [y]es / [a]lways / [n]o` (prompts go to stderr; `always`
  remembers the canonical path in `[directories] allow_paths`);
- noninteractive runs (subprocess callers, CI) fail closed with a
  typed error naming the path. One exception: when the local caches
  already exist and only the record tree is refused, feedback outcomes
  defer to an append-only sidecar
  (`$GIT_COMMON_DIR/nisse/deferred-observations.jsonl`) and flush once
  consent is granted (`nisse records pending` lists them). On a fresh
  repository — no caches yet — every directory-needing command,
  feedback included, fails closed;
- `NISSE_ALLOW_DIR_CREATION=1` grants blanket consent (tests, CI, or
  restoring the pre-consent behavior) — the repo's `.cargo/config.toml`
  sets it for `cargo test`/`cargo run` in this checkout only;
  `NISSE_INTERACTIVE_CONSENT=1` forces the interactive prompt path for
  scripted/pty-less runs (it grants nothing by itself);
- run `nisse doctor` in a terminal to review and grant consent for
  every directory Nisse would use in the current repository.

Read-only commands (`nisse records pending`, `nisse records list`)
never demand directory creation: with no pending store they simply
report an empty listing.

### Local/LAN trust model

By default Nisse trusts only model/provider endpoints that are — or
resolve to — loopback, RFC1918 private IPv4, IPv6 loopback, or IPv6
unique-local addresses. Hostnames are resolved *before* the trust
decision. Any other endpoint is refused unless
`network.allow_non_private_endpoints = true` is set explicitly in
trusted local configuration. Secrets never live under `.nisse/**`, and
`.nisse/**` can never configure executables, extensions, endpoints,
credentials, or filesystem roots (see
[docs/security.md](docs/security.md)).

## CLI

```
nisse --version

nisse doctor

nisse review plan --file <path> | --stdin [--json]
nisse review code --working | --staged | --commit <oid>
                        | --range <base>..<head> | --branch <base-ref>
                        [--json]
nisse review investigation --task <text|@file|path>
                           --result <text|@file|path> [--json]

nisse query "<query>" [--json]

nisse feedback <review-id> <finding-id> --fixed | --accepted | --dismissed

nisse records list [--kind <observation|fact|lesson>]...
nisse records pending | compact
nisse cache  status | clear
nisse eval   routing | retrieval
nisse mcp
```

- Commands write human-readable text to stdout by default and strict
  machine-readable JSON with `--json` (or over MCP).
- Failures are typed errors on stderr with a non-zero exit; stdout stays
  clean.
- `.nisse/**` is excluded from application code review by default.
- Plan review targets the current working tree; code review supports the
  five frozen snapshot scopes (`working`, `staged`, `commit`, `range`,
  `branch`).

Findings carry a frozen severity (`blocker`, `high`, `medium`, `low`,
`info`) and impact class; high/blocker findings always require concrete
evidence validated against the review snapshot.

## Project knowledge

Durable project knowledge lives in Git-tracked JSON records under
`.nisse/` (see [docs/record-format.md](docs/record-format.md)):

```
.nisse/
  people/<contributor-id>/<ULID>.json   # personal records (observations)
  common/<ULID>.json                    # consolidated facts and lessons
```

Three record kinds, all immutable after creation (new knowledge creates
new records; old records remain for provenance):

- **observation** — something happened (e.g. a review finding was
  fixed);
- **fact** — supported project knowledge, independently verified;
- **lesson** — what repeated observations suggest is useful.

The learning pipeline (see [docs/learning.md](docs/learning.md)) is
deterministic and trust-minimal:

- reviewers may only *propose* facts; Nisse validates them and runs an
  **independent verification session** before anything becomes durable —
  a reviewer can never directly persist a fact;
- known finding outcomes (`fixed` / `accepted` / `dismissed`) become
  immutable observations built by Rust, with idempotent resubmission;
- lessons (routing hints) are derived by Rust from unique observation
  counts under frozen thresholds; LLMs never compute statistics;
- facts whose supporting content disappears from the current snapshot
  are marked **potentially stale**, downranked, and can be revalidated;
- `nisse records compact` consolidates compatible facts/lessons into
  `common/` with independent verification, preserving originals.

`nisse records list` / `nisse records pending` inspect the record tree
and pending local learning state. Pass `--kind observation`, `--kind
fact`, or `--kind lesson` (repeatable) to list only the selected kinds;
observation rows include their finding id so same-category findings of
one review stay distinguishable. Records are plain files: they merge
across branches and survive reviewers, cache loss, and worktree
boundaries.

## Worktrees and generated state

Linked worktrees are first-class. Nisse derives every path from Git
directory semantics (`GIT_DIR`, `GIT_COMMON_DIR`) — never from a `.git`
guess:

| State                | Location                              | Scope      |
| -------------------- | ------------------------------------- | ---------- |
| shared cache         | `$GIT_COMMON_DIR/nisse/cache.sqlite`  | repository |
| worktree cache       | `$GIT_DIR/nisse/worktree.sqlite`      | worktree   |
| durable records      | `<worktree>/.nisse/**`                | Git-tracked |

The shared cache holds the content-addressed source index (parsed
artifacts, symbols, references, FTS, embeddings) and review history;
artifacts are keyed by content (`git:<blob-oid>` or `sha256:<hash>`), so
identical blobs are parsed once and reused across branches and linked
worktrees. Worktree caches hold manifests and path→content membership —
what a worktree sees is always snapshot-specific, so different
same-path content in different worktrees never leaks (tested
explicitly). All SQLite state is regenerable: `nisse cache status` /
`nisse cache clear` report and rebuild it. Indexing is lazy and
incremental; there is no init or refresh command.

## Pi integration

The primary coding-agent experience is the Pi package in
[integrations/pi/](integrations/pi/). It is deliberately thin: four
extension tools (`nisse_review_plan`, `nisse_review_code`,
`nisse_query_project`, `nisse_feedback`) that invoke the `nisse` binary
with structured machine-readable I/O and return its stdout verbatim — no
Nisse business logic is duplicated in TypeScript. The `nisse` binary is
resolved from `PATH` (or `NISSE_BIN`).

Install the package into Pi from a checkout:

```
pi install /absolute/path/to/integrations/pi
# or try it once without installing:
pi -e /absolute/path/to/integrations/pi
```

Use an absolute path — relative paths in Pi settings resolve against
the settings file, not the shell's working directory. Verify with
`pi list`. To share with a team, publish the package to npm (remove
`"private": true` and set a real name in `integrations/pi/package.json`)
or host it in its own git repository; a git source installs at the
repository root, so the nested `integrations/pi` directory is not
discovered in a git install of this repo.

### Main Pi workflow

The [Nisse skill](integrations/pi/skills/nisse/SKILL.md) defines the
bounded workflow policy for substantial tasks:

```
understand -> draft plan
          -> bounded Nisse plan review   (max 3 attempts)
                blocker/high -> revise plan, review again
                still unresolved after 3 -> STOP, ask the user
-> implement -> run project tests
          -> bounded Nisse code review   (max 3 attempts)
                valid blocker/high -> fix, retest, review again
                still unresolved after 3 -> do not claim completion;
                report unresolved findings and ask the user
-> submit known outcomes -> report completion
```

Trivial work (spelling, formatting, comment-only, obvious mechanical
renames, …) may skip Nisse entirely; an explicit user instruction to use
Nisse always overrides the heuristics. Feedback is submitted only for
known outcomes: `fixed`, `accepted` (valid but deferred), or `dismissed`
(invalid); if the outcome is unclear, nothing is submitted. Workflow
policy lives in the skill — never in Nisse Core.

## Needle (optional)

Needle is an optional local helper used for routing classification and
embedding-based reranking. Enable it with the `[classifier]` and/or
`[embeddings]` config sections (`provider = "needle"` plus a trusted
endpoint). Needle never performs substantive review, never mutates
durable state, and never bypasses the deterministic jury constraints
(quality always participates; jury size stays 2–4). Any Needle failure,
timeout, or malformed output degrades to the deterministic
rules-based behavior — reviews work identically without it.

## MCP

`nisse mcp` runs the secondary generic integration: a stdio MCP server
(JSON-RPC, one message per line) exposing the same four core operations
(`review_plan`, `review_code`, `query_project`, `record_feedback`).
stdout carries protocol messages only. See
[integrations/mcp/README.md](integrations/mcp/README.md) for client
configuration.

## Windows and Visual Studio

Windows x86_64 is a supported platform. CI runs the full Rust gate set
(fmt, clippy with warnings denied, all tests) on Windows, including
regressions for paths with spaces, Unicode names, CRLF content, linked
worktrees, the Pi-host child process, and the MCP stdio subprocess.

Visual Studio 2022 / GitHub Copilot connects through the generic MCP
adapter — no Visual Studio-specific behavior exists in Nisse Core. See
[integrations/visual-studio/README.md](integrations/visual-studio/README.md).

## Development

```
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features

cd runtime/pi-host   && npm run typecheck && npm run test
cd integrations/pi   && npm run typecheck && npm run test
```

The default test suite requires no Internet access and no real LLM
provider: reviewer-dependent tests use the deterministic fake runtime of
the Pi host. Evaluation baselines: `nisse eval routing` and
`nisse eval retrieval`.

Further documentation:

- [docs/architecture-contract.md](docs/architecture-contract.md) —
  architecture summary
- [docs/record-format.md](docs/record-format.md) — durable record
  schemas
- [docs/storage.md](docs/storage.md) — local SQLite storage
- [docs/learning.md](docs/learning.md) — learning pipeline
- [docs/security.md](docs/security.md) — trust boundaries and the
  security audit
- [docs/pi-host-protocol.md](docs/pi-host-protocol.md) — the frozen
  Rust↔host JSONL protocol
- [tests/fixtures/README.md](tests/fixtures/README.md) — test fixtures
