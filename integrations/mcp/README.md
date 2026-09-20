# Nisse MCP Integration

Nisse is a local software-review engine. This directory documents its
generic [Model Context Protocol](https://modelcontextprotocol.io)
integration: any MCP client — IDEs, editors, agents, test harnesses —
can run Nisse plan/code reviews, project queries, and feedback without
a Pi-specific extension.

MCP is the *secondary* integration. The primary user experience is the
Pi package (`integrations/pi/`); MCP exists for everything else
(including Visual Studio / GitHub Copilot, see `integrations/visual-studio/`).

## Running the server

```
nisse mcp
```

- **Transport**: stdio only. Newline-delimited JSON-RPC on stdin and
  stdout, one message per line, per the MCP stdio transport.
- **Protocol versions**: the server negotiates the MCP protocol
  version during `initialize` (it supports the versions of the
  official Rust MCP SDK).
- **stdout purity**: stdout carries protocol messages only. All logs
  go to stderr (`RUST_LOG` controls verbosity). Reviews spawn isolated
  reviewer sessions in a child process; none of their output ever
  reaches the server's stdout.
- **Working directory**: the server reviews the repository containing
  its working directory, exactly like the `nisse` CLI. Point your
  client's cwd at the project you want reviewed.
- **No daemon**: `nisse mcp` runs for the duration of the client
  connection and exits when stdin closes. There is no background
  service.

### Client configuration

Most MCP clients accept a command entry like:

```json
{
  "mcpServers": {
    "nisse": {
      "command": "/path/to/nisse",
      "args": ["mcp"],
      "cwd": "/path/to/your/project"
    }
  }
}
```

## Tools

The four tools are the frozen Nisse Core operations (PLAN.md sections
1, 61, 67). The MCP adapter is a thin layer over the same in-process
core operations the CLI uses — the Pi extension reaches the same code
through the CLI — so all three integrations behave identically
(permanent invariant 25).

| Tool             | Input                                                                 | Output                                       |
| :--------------- | :-------------------------------------------------------------------- | :------------------------------------------- |
| `review_plan`    | `plan` (string)                                                       | `ReviewResult` JSON                          |
| `review_code`    | `scope` (`working`\|`staged`\|`commit`\|`range`\|`branch`) plus `commit`, `range`, or `baseRef` when the scope needs one | `ReviewResult` JSON |
| `query_project`  | `query` (string)                                                      | context-bundle JSON (same as `nisse query --json`) |
| `record_feedback`| `reviewId`, `findingId`, `outcome` (`fixed`\|`accepted`\|`dismissed`)  | feedback report JSON                         |

### `review_plan`

Reviews an implementation plan against the current working-tree
snapshot. Runs LLM reviewer sessions through the configured review
host and may take minutes. The result carries `review_id`,
`findings[]` (each with `id`, `severity`, `impact_class`, `claim`,
`evidence`, `reviewer_ids`, `cross_validated`), and a `summary`.
Address valid blocker/high findings before implementing; use
`record_feedback` to submit known outcomes.

### `review_code`

Reviews code changes for one of the five frozen snapshot scopes:

- `working` — HEAD against the final working-tree state (staged +
  unstaged + untracked non-ignored);
- `staged` — HEAD against the index;
- `commit` — one commit against its first parent (requires `commit`);
- `range` — `<base>..<head>` trees (requires `range`);
- `branch` — merge-base(`baseRef`, HEAD) against HEAD (requires
  `baseRef`).

The result format matches `review_plan`. Run your project's tests
before requesting a code review.

### `query_project`

Fast, deterministic, LLM-free retrieval over the current working-tree
snapshot: source symbols and skeletons, relations, project facts,
lessons, and built-in practices. Useful for investigating the project
and checking finding evidence.

### `record_feedback`

Records a known outcome for one finding of a stored review:

- `fixed` — the implementation changed specifically to address the
  finding;
- `accepted` — valid but intentionally not fixed now;
- `dismissed` — investigated and judged incorrect or not applicable.

Submit only outcomes you actually know. Repeated submissions are
idempotent; durable outcomes are immutable. Successful submissions
create immutable observation records under `.nisse/` (Git-tracked
project knowledge, PLAN.md sections 39–50).

## Error semantics

- Structurally invalid input (blank or missing required fields, a
  scope without its revision) is rejected with a JSON-RPC
  `-32602 invalid_params` error before any Nisse work runs.
- An operation that ran and failed — for example a review that could
  not gather two valid reviewer reports — returns a successful
  response whose tool result has `isError: true` and carries Nisse's
  typed error message (the same text the CLI prints to stderr).
- The server survives tool failures; subsequent requests are served
  normally.

## Semantics and limits

- **Noninteractive**: no tool ever prompts. Without a configured
  contributor identity (`[contributor] id` in the platform-native user
  config), feedback outcomes are retained as pending local state and
  the tool reports the explicit failure; reviews leave learning
  pending. See PLAN.md section 44.
- **Serialized operations**: the server executes one Nisse operation
  at a time in arrival order; requests are never executed
  concurrently against the same repository state.
- **Trust model**: reviewer model endpoints come from trusted local
  configuration only; Git-tracked `.nisse/**` files cannot configure
  commands, endpoints, or credentials (PLAN.md section 10).
- **Read-only reviewing**: Nisse never modifies application source
  (permanent invariant 23). Reviews persist only regenerable local
  caches and, for feedback, durable `.nisse/` records.
- **Optional providers**: Needle classification/embeddings, when
  configured locally, improve routing and retrieval; every provider
  failure degrades to the deterministic rules-only behavior.

## Related

- `PLAN.md` — the frozen implementation contract (sections 61, 67,
  and milestone 26 cover this adapter).
- `docs/architecture-contract.md` — architecture overview.
- `integrations/pi/` — the primary Pi integration.
- Windows and Visual Studio / GitHub Copilot guidance arrives with
  Windows support (PLAN.md milestone 27) and will live in
  `integrations/visual-studio/`.
