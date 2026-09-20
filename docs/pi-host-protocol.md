# Pi Review Host Protocol

The protocol spoken between Nisse Core (Rust) and the Pi review host
(`runtime/pi-host`, TypeScript). Frozen by [PLAN.md](../PLAN.md)
sections 20 and 21; protocol version **1**.

## Transport

- The host is a **child process**, not a daemon. Nisse Core spawns it only
  for the duration of the required semantic work and disposes it after.
- **stdin**: Nisse Core → host, one JSON message per line.
- **stdout**: host → Nisse Core, **protocol messages only**, one JSON
  object per line. Any other stdout traffic is a protocol violation.
- **stderr**: host logs and diagnostics. Never parsed as protocol data;
  captured separately by the Rust side.
- Startup: `node runtime/pi-host/src/main.ts --runtime <fake|pi>`.

## Envelope

Every message (both directions) is a single-line JSON object:

```json
{"protocol_version":1,"id":"<unique>","type":"<message type>", ...payload}
```

- `protocol_version` — must be exactly `1`; mismatches are typed errors /
  `host_error{code="unsupported_protocol_version"}`.
- `id` — unique per sender. The `tool_request` envelope `id` is the
  `request_id` echoed by `tool_response`.
- `type` — one of the frozen vocabulary names below.
- Payload fields are flattened into the same object.

Optional fields may be encoded as JSON `null` (the Rust `Option<T>`
spelling) or omitted; the receiving side treats both as absent.

## Vocabulary (frozen, plan section 21)

Nisse Core → host:

| type | status |
| --- | --- |
| `host_config` | active |
| `start_review` | active |
| `start_fact_verification` | reserved |
| `start_fact_synthesis` | reserved |
| `start_domain_description` | reserved |
| `start_finding_equivalence_check` | reserved |
| `tool_response` | active |
| `cancel` | active |

Host → Nisse Core:

| type | status |
| --- | --- |
| `host_ready` | active |
| `reviewer_started` | active |
| `reviewer_completed` | active |
| `reviewer_failed` | active |
| `tool_request` | active |
| `fact_verification_completed` | reserved |
| `fact_synthesis_completed` | reserved |
| `domain_description_completed` | reserved |
| `finding_equivalence_completed` | reserved |
| `host_error` | active |

Sending a vocabulary-but-reserved message is refused deterministically
(`host_error{code="unsupported_message"}`); unknown type names are
`unknown_message`.

## Active messages

### `host_config` (core → host)

```json
{"models":{"standard":{"model":"acme/local-small","endpoint":"http://127.0.0.1:8080/v1"}}}
```

Resolved model profiles the host may use for reviewer sessions, mapped
from trusted local configuration by the Rust core. `model` is a
`provider/model-id` identifier resolved against the local Pi model
catalog; `endpoint` is optional and, when present, overrides the resolved
model's base URL (endpoint trust was already validated by the core).
Sending `host_config` again replaces the table. Reviewer sessions never
inherit a model: a spec whose `model_profile` is absent from the table
fails closed (`reviewer_failed{code="model_profile_unresolved"}`).

### `host_ready` (host → core)

```json
{"runtime":"fake"}
```

Sent once at startup, before any other message. `runtime` is `fake` or
`pi`.

### `start_review` (core → host)

```json
{"review_id":"<id>","kind":"plan|code","reviewers":[<ReviewerExecutionSpec>]}
```

`ReviewerExecutionSpec` (plan section 18) carries
`reviewer_id`, `specialization`, `instructions`, `checklist`,
`model_profile`, `thinking_level` (`off|low|medium|high`),
`initial_context` (list of `ContextEntry`), `allowed_capabilities`
(subset of the four read-only tools), `max_lookup_calls`,
`max_returned_context_bytes`, and `timeout_ms`. Reviewers execute
independently; the host never alters jury composition or a spec.

For each reviewer the host emits `reviewer_started`, then exactly one of
`reviewer_completed` (payload `report`: strict `ReviewerReport`) or
`reviewer_failed` (payload `error: {code, message}`).

### `tool_request` / `tool_response` (bidirectional)

The four read-only tools (plan section 22) are `project_lookup`,
`symbol_lookup`, `source_lookup`, `relation_lookup`. The host sends:

```json
{"review_id":"<id>","reviewer_id":"<id>","tool":"source_lookup","arguments":{...}}
```

The core answers with the request's envelope id:

```json
{"request_id":"<tool_request id>","ok":true,"bundle":<ContextBundle>}
{"request_id":"<tool_request id>","ok":false,"error":{"code":"...","message":"..."}}
```

The core resolves lookups against the exact target snapshot and enforces
the reviewer's capability grants, lookup count, and returned-context byte
budget; the host enforces per-call timeouts and cancellation. Tool
arguments are strict JSON objects (unknown fields rejected, both sides
validate independently):

| tool | arguments |
| --- | --- |
| `project_lookup` | `{"query": string}` (non-empty, ≤ 8 KiB) |
| `symbol_lookup` | `{"symbol": string}` (non-empty, ≤ 512 B) |
| `relation_lookup` | `{"symbol": string}` (non-empty, ≤ 512 B) |
| `source_lookup` | `{"path": string, "range"?: {"line_start": 1-based line, "line_end": ≥ `line_start`}}` (`path` non-empty, ≤ 4 KiB; `range` inclusive, clamped to the file) |

`source_lookup` returns one `ContextEntry` of kind `source` carrying the
target-snapshot bytes of the selected lines (a `null`/omitted range means
the whole file; a range starting past the last line is a successful empty
answer; non-UTF-8 content fails as `binary_content`).

Failure codes served by the core: `unknown_review`, `unknown_reviewer`,
`capability_denied` (tool not in the reviewer's `allowed_capabilities`),
`lookup_budget_exhausted` (`max_lookup_calls` spent),
`context_budget_exhausted` (`max_returned_context_bytes` spent),
`invalid_arguments`, `path_not_in_snapshot` (well-formed path that is not
part of the target snapshot), `binary_content`, `retrieval_failed`,
`git_failed`. A request whose arguments fail the schema consumes no
lookup; every dispatched lookup consumes one call and its returned text
bytes count against the byte budget regardless of outcome. Bundles are
trimmed weakest-entry-first into the remaining budget (marked
`truncated`), never enlarged.

### `cancel` (core → host)

`{"review_id":"<id>"}` cancels one review; `{}` cancels everything. The
host aborts pending sessions and tool calls; there is no acknowledgment
message. The core then closes stdin (or kills the child).

### `host_error` (host → core)

```json
{"error":{"code":"<stable code>","message":"<human text>"}}
```

Codes include `malformed_message`, `unsupported_protocol_version`,
`unknown_message`, `unsupported_message`, `invalid_payload`,
`unknown_request`, and `internal`.

## Lifecycle

1. Nisse Core spawns the host and waits for `host_ready` (bounded timeout).
2. The core sends `start_review`; the host starts each reviewer session
   (`reviewer_started`) and serves their tool calls through
   `tool_request`/`tool_response`.
3. Each session ends in `reviewer_completed` or `reviewer_failed`.
4. The core may `cancel` at any time.
5. stdin close is the shutdown signal: the host exits `0`.

## Failure behavior

- Every inbound line is envelope-checked and payload-validated on both
  sides; malformed input is a typed error (`MalformedLine`,
  `UnsupportedVersion`, `InvalidPayload`, …) or `host_error`, never
  guessed state.
- The Rust reader caps lines at 8 MiB and rejects invalid UTF-8.
- Child crash (unexpected exit) is reported as a typed `ChildTerminated`
  error with the exit status; dropping the client kills and reaps the
  child.
- The fake runtime never contacts a network endpoint.

## Fake runtime and fault injection

`--runtime fake` selects the deterministic fake reviewer runtime used by
the protocol tests. It honors the spec's capabilities (issues exactly one
`project_lookup` when granted), produces a fixed informational finding,
and reflects tool outcomes in its report summary.

Environment variable `NISSE_PI_HOST_FAULT` scripts protocol faults for
testing: `startup-hang`, `crash-on-start`, `wrong-version`,
`ready-then-exit`, `stderr-noise`, `malformed-after-ready`, `review-hang`.

Fake reviewer faults:

- `NISSE_FAKE_REVIEWER_FAIL_FOR=<id>,<id>`: every session of the listed
  reviewers fails;
- `NISSE_FAKE_REVIEWER_FAIL_ONCE_FOR=<id>,<id>`: only the first session
  attempt of the listed reviewers fails (exercises the retry policy);
- `NISSE_FAKE_REVIEWER_ATTEMPTS_FILE=<path>`: every session appends one
  JSON line `{reviewer_id, attempt}` so tests can prove how often a
  reviewer actually ran.

## Pi reviewer runtime

`--runtime pi` executes constrained isolated Pi SDK sessions (plan
section 20). Each `ReviewerExecutionSpec` is mapped onto one session
with:

- **explicit model** resolved from `host_config` (endpoint override
  applied when configured) — never inherited from Main Pi or settings;
- **explicit thinking level** from the spec;
- **explicit system instructions**: reviewer instructions, the ordered
  checklist, and the strict output contract;
- **explicit tools**: only the spec's granted read-only lookup tools,
  bridged to the Rust core; no built-in coding tools;
- **explicit initial context**: the review task plus the rendered context
  entries as the first user message;
- **isolation**: in-memory session only; extensions, skills, prompt
templates, AGENTS/context files, and themes are not discovered; settings
are in-memory.

Reviewer output is parsed strictly (`ReviewerReport` schema). Malformed
output receives exactly **one constrained repair** prompt; if the repair
is still invalid the reviewer fails
(`reviewer_failed{code="invalid_report"}`). Sessions run under the
spec's `timeout_ms` (`reviewer_failed{code="session_timeout"}`). The
host stamps reviewer identity, duration, and model metadata itself — the
model's own claims about them are never trusted.

The Pi SDK is imported dynamically: where `runtime/pi-host` dependencies
are not installed, the fake and scripted runtimes still work and real
reviewer execution fails closed with
`reviewer_failed{code="sdk_unavailable"}`.

### Scripted mock (tests)

For offline tests of the full pi pipeline, the pi runtime can run against
scripted agent sessions instead of the SDK:

- `NISSE_PI_SESSION_SCRIPT=<path>`: JSON file
  `{"sessions":[{"steps":[{"reply":string}|{"error":string}|{"hangMs":number}]}]}`;
  one session per reviewer session, one step per conversation turn;
- `NISSE_PI_SESSION_RECORD=<path>`: every completed session appends one
  JSON line `{config, prompts}` describing the exact session Nisse built
  (model, endpoint, thinking level, system prompt, tools, isolation flags).

### Retry policy (Rust core)

A failed reviewer may be retried **once** by the Rust core
(`nisse::jury::execution::execute_review`); the retry reuses the identical
`ReviewerExecutionSpec` — model, reviewer, checklist, and capabilities are
never silently changed.
