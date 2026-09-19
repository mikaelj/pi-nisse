# Nisse

Nisse is a local software-review engine.

Nisse Core exposes four operations:

- `review_plan` — review a plan produced by another agent or human
- `review_code` — review code changes produced by another agent or human
- `query_project` — query indexed project knowledge
- `record_feedback` — record known review outcomes as durable observations

Nisse does not implement the reviewed work itself. It has no Plan mode or
Build mode; workflow policy belongs to coding-agent integrations such as the
primary Pi package.

Status: implementing the frozen plan in [PLAN.md](PLAN.md).

Architecture summary: [docs/architecture-contract.md](docs/architecture-contract.md).

## Configuration

Nisse uses trusted local TOML configuration. Git-tracked `.nisse/**` files
are project data and are never configuration sources.

- user config: `<platform config dir>/nisse/config.toml`
- repository config: `$GIT_COMMON_DIR/nisse/config.toml` (local machine
  state, never committed)

Precedence (highest first): repository config, user config, compiled
defaults. Model endpoints are trusted only when they are (or resolve to)
loopback, RFC1918 private, or IPv6 unique-local addresses, unless
`network.allow_non_private_endpoints` is set explicitly in trusted local
configuration. Model profiles `standard` and `strong` are required before
reviews can run. Run `nisse doctor` to check the local setup.

## Pi review host

Reviewer sessions run in a TypeScript child process (`runtime/pi-host`),
not inside the Rust core. The core spawns the host, drives it with the
frozen JSONL protocol on stdin/stdout, and disposes it after the semantic
work completes — it is not a daemon. Reviewer sessions receive explicit
instructions, checklists, model profiles, thinking levels, context, and
read-only tools; they never inherit the Rust process, Main Pi's
conversation, or arbitrary skills. Protocol details:
[docs/pi-host-protocol.md](docs/pi-host-protocol.md).

Host requirements: Node.js >= 23.6 (runs the TypeScript directly); `npm
install` inside `runtime/pi-host` is only needed for `npm run typecheck`.
