# @nisse/pi-integration

Thin Main Pi integration for [Nisse](../../), the local software-review engine: five `nisse_*` extension tools over the `nisse` CLI plus the bounded Nisse review-workflow skill. Install from the repository root: `pi install ./integrations/pi` (local dir package; the `nisse` binary must be on `PATH`).

## Tools

| Tool | Purpose |
| --- | --- |
| `nisse_review_plan` | Jury review of an implementation plan (plan text in, `ReviewResult` JSON out). |
| `nisse_review_code` | Jury review of code changes for one snapshot scope (`working` default; `staged`, `commit`, `range`, `branch`). |
| `nisse_review_investigation` | Claim verdicts plus coverage review of investigation results. |
| `nisse_query_project` | Deterministic index query (symbols, facts, lessons) — no LLM, no jury. |
| `nisse_feedback` | Record a known outcome (`fixed` / `accepted` / `dismissed`) for one finding. |

The extension never interprets or rewrites Nisse output; successful stdout is returned verbatim.

## Configuration

- **`NISSE_BIN`** (environment of the process that launches pi): the `nisse` executable; defaults to `nisse` on `PATH`.
- **`NISSE_TIMEOUT_MS`** (environment of the process that launches pi): the subprocess timeout for every tool call; defaults to 20 minutes. A shell `export` inside a pi session cannot reach the extension's process — use the per-call parameter instead.
- **`timeout_ms`** (optional parameter on the three review tools, `60000..7200000`): per-call subprocess timeout. This is the lever an in-session agent can actually use; invalid values fall back to the environment default.
- **`performance.reviewer_timeout_ms`** (`~/.nisse/config.toml`, `60000..3600000`, default `300000`): Nisse's internal per-reviewer-session wall-clock budget; the frozen single retry reuses the same budget. Tool calls cannot change it.

### Which timeout did I hit?

- Tool error `nisse timed out after Nms` → the **wrapper**; retry with a higher `timeout_ms`.
- Findings or a typed failure mentioning `session_timeout`, or `only N valid independent reviewer report(s) exist` → the **internal session budget**; raise `reviewer_timeout_ms` in the user config.

When raising the internal budget, keep the wrapper above the worst-case wall clock: `wrapper ≳ 2 × reviewer_timeout_ms + ~5 minutes`.

## Tests

```
npm run typecheck && npm test
```
