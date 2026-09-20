# Nisse in Visual Studio (GitHub Copilot)

Nisse integrates with Visual Studio 2022 and GitHub Copilot through the
generic MCP adapter (`nisse mcp`, plan section 67). No Visual
Studio-specific behavior exists in Nisse Core: the IDE connects to the
same four frozen operations the CLI and the Pi extension use.

## The tool surface

Once connected, Copilot can call:

| MCP tool          | Nisse Core operation |
| :---------------- | :------------------- |
| `review_plan`     | review a plan before implementing it |
| `review_code`     | review code changes after implementing (scopes: `working`, `staged`, `commit`, `range`, `branch`) |
| `query_project`   | fast deterministic project-index query (symbols, facts, lessons, practices) |
| `record_feedback` | record a known finding outcome (`fixed`, `accepted`, `dismissed`) |

Nisse is operation-based. It has no Plan/Build mode and owns no task
state machine — the IDE and its agent own the workflow. The intended
bounded loop is the same everywhere:

- plan review, revise on valid blocker/high findings, at most 3
  attempts, then stop for user direction;
- code review after implementation and tests, fix valid blocker/high
  findings, retest, at most 3 attempts, then stop for user direction;
- submit known outcomes with `record_feedback`; submit nothing when
  unclear.

## Prerequisites (Windows x86_64)

1. **Git for Windows** on `PATH`. Nisse shells out to the installed
   `git` executable for all repository, snapshot, and diff semantics.
2. **Node.js >= 23.6** on `PATH`. Reviewer sessions run in the
   TypeScript Pi review host as a `node` child process. (`query_project`
   works without reviewers, but plan/code reviews need it.)
3. **The `nisse` binary** on `PATH`, built from this repository:

   ```
   cargo build --release
   ```

   The resulting `nisse.exe` (in `target\release`) must be reachable
   from Visual Studio's environment.

Verify the installation before connecting:

```
nisse --version
nisse doctor
```

`nisse doctor` checks Git availability, configuration validity, model
profiles, and endpoint trust — it performs no LLM request.

## Connecting Visual Studio

Visual Studio 2022 (17.14 and newer) supports MCP servers over stdio.
Add Nisse to the solution scope with a `.mcp.json` file in the
**solution directory**:

```json
{
  "servers": {
    "nisse": {
      "type": "stdio",
      "command": "nisse",
      "args": ["mcp"],
      "env": {}
    }
  }
}
```

Then:

1. Reopen (or reload) the solution so Visual Studio discovers the file,
   and approve the server when prompted (MCP servers are local
   executables and require explicit trust).
2. The four Nisse tools become available to GitHub Copilot Chat
   (agent mode) in that solution.

The exact discovery/approval UI differs slightly across Visual Studio
versions; the `.mcp.json` format above is the stable part. The MCP
transport is stdio only — stdout carries protocol messages exclusively.

## Running from the right directory

`review_code --working` and `query_project` operate on the working tree
of the repository that contains the server's working directory. Start
Visual Studio with the repository (or solution folder) as the working
context so repository discovery resolves the repo you intend. All
Nisse state is derived from Git:

- shared, regenerable cache: `<repo>\.git\nisse\cache.sqlite`
- per-worktree cache: `<worktree git dir>\nisse\worktree.sqlite`
- durable, committed project knowledge: `<repo>\.nisse\`

## Windows behavior

The Windows support is explicit (plan milestone 27) and covered by CI
on `windows-latest`:

- paths containing spaces, parentheses, and non-ASCII characters are
  supported end to end;
- non-ASCII file names and CRLF file content are indexed and reviewed
  correctly;
- linked worktrees are first-class, including worktree administrative
  directories with spaces in their names; each worktree reviews only
  its own snapshot;
- the MCP stdio stream and the Pi-host JSONL child process both keep
  stdout pure (protocol only) and logs on stderr.

## Trust model

- Nisse makes no ambient network calls for `query_project`, `record
  feedback`, or repository work. Reviewer sessions use explicitly
  configured local model profiles; by default only loopback and
  RFC1918 private endpoints are permitted.
- Configuration lives in trusted local files only:
  `%APPDATA%\nisse\config.toml` (platform-native user config) and
  `<repo>\.git\nisse\config.toml` (repository-local, never committed).
- Git-tracked `.nisse/**` content is project data; it can never
  configure executables, endpoints, or credentials.
- Put no credentials under `.nisse/**`; durable records are committed
  to the repository and shared with everyone who clones it.

## Limitations

- stdio transport only; one operation at a time per server instance;
- the MCP adapter is noninteractive: reviews without a configured
  contributor identity leave learning pending locally instead of
  prompting;
- Copilot, not Nisse, decides when to run reviews — see the bounded
  workflow above.
