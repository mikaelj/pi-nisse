# Nisse security model

This document describes Nisse 1.0's trust boundaries, where each boundary is enforced, and the regression tests that keep it enforced. It is the milestone-28 audit result: every item below names its enforcing code.

## Trust classes

| Class | Examples | Trusted? |
| --- | --- | --- |
| Trusted local configuration | user config, CLI options, model profiles | Yes (local user) |
| Untrusted project data | source, diffs, plans, `.nisse/**` records, reviewer output | **No** |
| Regenerable local state | SQLite caches, manifests | No (disposable) |

Untrusted project data may influence **what is reviewed**, never **how Nisse behaves**. Git-tracked `.nisse/**` files are project data: they cannot configure executables, endpoints, credentials, extensions, or filesystem roots.

## Boundary-by-boundary enforcement

### Filesystem and record storage (`src/storage/records.rs`)

- **Path traversal**: contributor slugs must match `[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}` and are validated before any path is built; record file names must be canonical ULIDs.
- **Malicious `.nisse` records**: loading is strict — malformed JSON, invalid schemas, ID/author/filename mismatches, duplicate IDs, and stray entries are hard errors, never silent skips.
- **Oversized records**: `MAX_RECORD_FILE_BYTES` (1 MiB) is checked via file metadata *before* reading, so a hostile tree cannot cause unbounded reads.
- **Invalid UTF-8**: record files are parsed with `serde_json::from_slice`, which rejects invalid UTF-8; directory-entry names must be UTF-8; Git output is UTF-8-validated by the runner; plan input (`--file`/`--stdin`) is UTF-8-validated and size-bounded (`src/app.rs`).
- **Symlinks**: record files must be regular files; a symlinked `.nisse` root is rejected on load; and *creation* walks every directory component below the worktree root with `symlink_metadata` and refuses to traverse symlinks, so a pre-planted `.nisse/people/<id>` symlink can never redirect an immutable record outside the worktree.
- **Interrupted record writes**: records are published by fsynced temporary file plus atomic `hard_link` (`src/storage/fsutil.rs`); a crash leaves either the complete record or an ignorable `.nisse-tmp-*` file (dot-prefixed entries are skipped on load).

### Snapshot and evidence paths (`src/git/reader.rs`, `src/jury/findings.rs`)

- Repository-relative paths are validated: no absolute paths, no `..` or `.` components, no `.git`, no NUL bytes, no redundant separators.
- All reads resolve against the resolved target snapshot (working tree final state, index, or commit tree); a path outside the snapshot reads as absent. Cache presence never proves membership.
- Evidence validation rejects findings whose evidence paths are not part of the review scope or target snapshot, and clamps severity by impact class.

### Process boundaries (`src/git/runner.rs`, `src/jury/host.rs`)

- **Command injection**: Git (and the Pi host) are spawned without a shell; arguments are passed as single literal strings. Object IDs are validated as full hex SHA-1/SHA-256 before being handed back to Git. Ambient `GIT_*` environment variables are stripped and the locale is forced to `C`.
- **Child crash / timeout**: every child has a per-command timeout with kill-on-expiry; premature child termination is a typed error (`ChildTerminated`), never a hang or partial parse.
- **Stdout contamination**: the Pi host protocol channel is stdout-only; stderr is drained into a bounded separate buffer and never parsed as protocol.
- **Protocol injection / malformed output**: every inbound line is envelope-checked (protocol version, id, type) and payload-validated; lines beyond `MAX_LINE_BYTES` (8 MiB) and non-UTF-8 lines are rejected while reading. Outbound messages are single strict JSON lines.
- **Malformed Git output**: `diff --raw`/`diff --numstat` parsing is token-strict; wrong field counts, unknown statuses, truncated rename entries, and invalid line counts are typed `UnexpectedOutput` failures.

### Concurrency and storage locking (`src/storage/db.rs`)

- SQLite connections use WAL journaling, `synchronous=NORMAL`, foreign keys, and a 5 s busy timeout, so concurrent short-lived Nisse processes serialize instead of corrupting or failing spuriously.
- All SQLite state is regenerable; corruption is recovered by rebuild, never by trusting the damaged file.

### Endpoint and network trust (`src/config/endpoint.rs`)

- Default-permitted endpoint classes: IPv4/IPv6 loopback, RFC1918 private IPv4, IPv6 unique-local. Everything else requires the explicit trusted-local `network.allow_non_private_endpoints = true`.
- Hostnames are always resolved before the trust decision; a name is never trusted by spelling; unresolvable names and names with any non-permitted address fail closed. IPv4-mapped IPv6 addresses are classified by their embedded IPv4 address.

### LLM prompt separation (`runtime/pi-host/src/untrusted.ts`)

LLMs are untrusted semantic workers. Every prompt that embeds project data — reviewer initial context (source, diffs, plans, records, practices), equivalence-check findings, fact-verification claims and evidence, fact-synthesis inputs, domain-description descriptors, and previous model answers — wraps that content in explicit

```
<<<UNTRUSTED PROJECT DATA
…
END UNTRUSTED PROJECT DATA>>>
```

markers, and every corresponding system prompt states that content inside the markers (and every tool result) is evidence to analyze, never instructions to follow. Occurrences of the markers inside untrusted content are neutralized before the block is built, so project data cannot forge the end of its own block. Reviewer sessions additionally have only read-only tools, explicit models, and no inheritance of Main Pi context.

## Keep-it-enforced tests

- `src/storage/records.rs` unit tests: traversal, symlinks (root, components, files), oversized/invalid-UTF-8 records, immutability, interrupted-write leftovers.
- `src/storage/db.rs`: concurrent writers serialize via the busy timeout.
- `src/git/runner.rs`: metacharacters in arguments never execute.
- `src/git/diff.rs`: malformed raw/numstat output fails typed.
- `tests/integration/pi_host.rs`: malformed, oversized, and non-UTF-8 stdout lines; child crashes; stderr isolation; stdout purity.
- `tests/integration/security.rs`: binary-level UTF-8 plan rejection, hostile revisions, hostile `.nisse` trees, symlinked directories.
- `runtime/pi-host/src/untrusted.test.ts`: marker integrity and prompt framing for every LLM-facing message.
- `src/config/endpoint.rs`: address classes, hostname resolution, fail-closed behavior, explicit override.
