# Plan: Wire pi's thinkingLevelMap into Nisse (protocol v2 `model_capabilities`)

Repo: `~/code/nisse/pi-main` (Rust core + `runtime/pi-host` TypeScript host, version-locked in one repository). Revision 2 after plan review round 1.

## Problem

- Nisse's `ThinkingLevel` enum (`src/domain/model.rs`) is `off | low | medium | high`, but pi's vocabulary is `off | minimal | low | medium | high | xhigh | max` (`EXTENDED_THINKING_LEVELS`, verified in the installed SDK `runtime/pi-host/node_modules/@earendil-works/pi-coding-agent@0.85.1`). Levels such as `max` — which `zai/glm-5.3` supports — cannot be expressed in Nisse config.
- Per-model support comes from pi's `thinkingLevelMap` (e.g. `zai/glm-5.3`: `low/high/max` supported; `off`/`minimal`/`medium`/`xhigh` map to `null`). Nisse knows nothing about it: a reviewer whose catalog default is `medium` runs against `zai/glm-5.3` and pi silently clamps at the transport layer. Nisse can create specs whose (model, thinking_level) pair is invalid.
- The config field `[model_profiles.*].thinking_level` is currently advisory only: `HostModelProfile::from_config` drops it (`src/jury/protocol.rs`), and every session uses the reviewer catalog's or job options' default level. Setting `thinking_level = "max"` is a config parse error; setting `"high"` changes nothing.

## Goals

1. Nisse obtains each configured model's supported thinking levels from pi at runtime (source of truth: pi's model catalog `thinkingLevelMap`, resolved through the same `ModelRuntime` the host already uses for sessions) — no stale hard-coded copy of pi's catalog.
2. Nisse never creates an invalid (model, thinking_level) pair: every session spec/payload level is validated and, when unsupported, deterministically clamped using pi's own clamp semantics, with a visible diagnostic.
3. `[model_profiles.*].thinking_level` becomes meaningful: when set, it overrides catalog/option defaults for sessions on that profile (precedence: explicit per-request level > profile override > catalog default), then clamping applies.
4. Nisse's `ThinkingLevel` vocabulary matches pi's exactly, on both sides of the host protocol.

## Non-goals

- Changing pi itself; changing reviewer-catalog defaults; per-reviewer level overrides in config.
- `nisse doctor` model-capability validation (doctor runs offline, must not spawn the node host; review-time is the enforcement point).
- Editing PLAN.md (frozen). Aligning the `integrations/pi` SDK pin (^0.86.1) with `runtime/pi-host` (^0.85.1) — noted as future work; this change targets the installed 0.85.1 shapes, which are identical for the symbols used (`EXTENDED_THINKING_LEVELS`, `clampThinkingLevel`, `getSupportedThinkingLevels`, `thinkingLevelMap` typing — verified by reading the installed package).

## Design

### 1. Extend `ThinkingLevel` (src/domain/model.rs)

- Variants: `Off, Minimal, Low, Medium, High, Xhigh, Max` (serde lowercase; `Display` matches). Superset of today's values — config, catalog, and existing wire traffic stay parse-compatible.
- `pub const PI_THINKING_LEVEL_ORDER: [ThinkingLevel; 7]` mirroring pi's `EXTENDED_THINKING_LEVELS` order (`off, minimal, low, medium, high, xhigh, max`).
- `pub fn clamp_to_supported(requested, supported: &BTreeSet<ThinkingLevel>) -> Option<ThinkingLevel>` reimplementing pi's `clampThinkingLevel` exactly: exact match → keep; else scan at-or-above the requested position in `PI_THINKING_LEVEL_ORDER`, then below it, else first supported. **Empty `supported` returns `None`** and every caller fails closed (see policy below); no unspecified behavior anywhere.

### 2. Host protocol v2 (`model_capabilities`)

- `PROTOCOL_VERSION` 1 → 2 on both sides (`src/jury/host.rs` / `runtime/pi-host/src/protocol.ts`). Rust and TS ship together; mismatch remains the existing typed error.
- New host→core message `model_capabilities`, added to `HOST_TO_RUST_TYPES` and the Rust decoder vocabulary. The host sends it immediately after processing every `host_config` (including replacements). Payload:

```json
{"profiles":{"standard":{"model":"zai/glm-5.3","supported_thinking_levels":["low","high","max"]}},"unresolved":{"fast":"model `x/y` is not registered in the local model catalog"}}
```

- Host-side resolution (real pi runtime, `runtime/pi-host/src/pi-agent.ts` + `main.ts`): for each configured profile, split `provider/model-id`, resolve via the shared `ModelRuntime` (same one `resolveSessionModel` uses), and derive supported levels with a faithful TS port of pi's `getSupportedThinkingLevels`: non-reasoning model → `["off"]`; reasoning model → every pi level whose `thinkingLevelMap[level] !== null`, where `xhigh`/`max` additionally require the key to be present (`!== undefined`). Endpoint overrides do not change levels (they only swap `baseUrl`). Unresolvable model or SDK load failure → entry in `unresolved` with a clear reason; the host never guesses.
- **Concurrency semantics (unchanged loop):** the `rl.on("line")` dispatch loop stays synchronous; `handleHostConfig` fires the (async, real-runtime) resolution without blocking the loop and sends `model_capabilities` when it resolves. `start_*` handlers keep enqueuing into the limiter exactly as today — no serialization, no deadlock. Ordering is guaranteed on the Rust side: Rust blocks on `wait_model_capabilities` before sending any `start_*`.
- Deterministic test doubles: `--runtime fake` and scripted sessions (`NISSE_PI_SESSION_SCRIPT`) never resolve real models, so they report every configured profile as supporting all seven levels. Test-only env `NISSE_PI_HOST_CAPABILITIES` (JSON object: profile name → non-empty array of the seven known level strings) overrides the reported map — **active only in fake/scripted modes; the real runtime ignores it entirely (never parsed)**. Malformed override in an active mode (bad JSON, unknown level, or empty array) fails loudly: stderr message + `host_error{code="internal"}` + exit, because a silently ignored test override would weaken every clamp test.
- Rust side (`src/jury/protocol.rs`, `src/jury/host.rs`): `ModelCapabilitiesPayload { profiles: BTreeMap<String, ModelCapabilityEntry { model: String, supported_thinking_levels: BTreeSet<ThinkingLevel> }>, unresolved: BTreeMap<String, String> }` (`deny_unknown_fields` like sibling payloads; unknown level strings on the wire are typed protocol errors via serde). New `HostEvent::ModelCapabilities` variant handled by the shared event pump; `HostProcess` stashes the latest capabilities; `send_host_config` resets the stash so every (re)sent table gets a fresh reply. `pub fn wait_model_capabilities(&mut self, timeout: Duration) -> Result<ModelCapabilitiesPayload, HostError>` modeled on `wait_ready` (timeout = `HOST_READY_TIMEOUT`, 20 s; resolution is local and fast). Events arriving during the wait route to their existing waiters/pumps as today.

### 3. Thinking policy in Rust (new `src/jury/thinking.rs`)

```rust
pub struct ThinkingPolicy { entries: BTreeMap<String, PolicyEntry> }
pub struct PolicyEntry { model: String, supported: BTreeSet<ThinkingLevel>, override_level: Option<ThinkingLevel> }

impl ThinkingPolicy {
    pub fn build(caps: &ModelCapabilitiesPayload, profiles: &BTreeMap<String, ModelProfile>) -> Result<Self, String>;
    pub fn resolve(&self, profile: &str, explicit: Option<ThinkingLevel>, default: ThinkingLevel)
        -> Result<(ThinkingLevel, Option<String>), String>;
}
```

- `build` never aborts on problems of profiles a job may not use (a typo'd optional `fast` profile must not break `standard`/`strong` reviews). Instead it records per-profile problems (unresolved / missing from capabilities / empty supported set); `resolve(profile, ...)` **fails closed with a reason naming the profile** when the profile it is asked for has a problem — before any session starts. A `warnings()` accessor lists problems of all other configured profiles so callers surface them as diagnostics. Invalid pairs remain impossible by construction: every session's level passes through `resolve` for exactly the profile it will run on.
- `resolve`: requested = `explicit.or(profile override).unwrap_or(default)`; clamp via `clamp_to_supported` (empty set already rejected for this profile by the per-profile gate; the `None` case errors, never guesses); on adjustment, return a note like `thinking level medium adjusted to high for model zai/glm-5.3 (profile standard)`. The returned level is guaranteed supported.
- **Precedence classification (rule):** `explicit` is reserved for genuine per-request user input. Verified by grep: no Rust or MCP call site passes a user-supplied per-request thinking level today (`src/mcp` exposes none), so **every enumerated payload site below passes its current level as `default`**, and the profile override therefore applies uniformly to reviews, investigation, and all fact jobs.
- **Every host spawn site waits for capabilities and builds the policy** (complete enumeration, verified by grep):

| Spawn site | Payload sites covered (policy applied) |
| --- | --- |
| `src/jury/review.rs:377` (plan review) | `DomainDescriptionPayload` (~389), `execution_spec()` specs → `StartReviewPayload` (451) |
| `src/jury/review.rs:1116` (code review) | `DomainDescriptionPayload` (~1132), `execution_spec()` specs → `StartReviewPayload` (1197), consolidation: `FindingEquivalenceCheckPayload` (`src/jury/synthesis.rs:168`), synthesis `StartReviewPayload` (`synthesis.rs:203` via `synthesizer_spec`, level from `ConsolidationOptions.thinking_level`) |
| `src/jury/investigation.rs:486` | `DomainDescriptionPayload` (~496), `StartInvestigationPayload` (~550) |
| `src/app.rs:771` (`records compact`) | `FactSynthesisPayload` (`src/learning/consolidation.rs:695`), `FactVerificationPayload` (`consolidation.rs:1104`, `src/learning/facts.rs:564`, `src/learning/staleness.rs:468`) |

  The policy threads from each spawn site into the builders (review options, `ConsolidationOptions`, investigation options carry `Arc<ThinkingPolicy>` or a `&ThinkingPolicy` parameter — implementation picks the least invasive that keeps every construction site covered). Per the classification rule above, each site's existing level value (reviewer-catalog default or job-option default such as `ConsolidationOptions.thinking_level`) is passed as `default`; the profile override wins over all of them. Clamp notes go to user-visible diagnostics where a channel exists (`diagnostics` vec in reviews; investigation notes); internal fact jobs log via `tracing::warn`. Implementation re-greps `Payload {` constructions to confirm no straggler sends a raw level.
- **Re-sent `host_config` rule:** any site that (re)sends `host_config` must follow with `wait_model_capabilities` and rebuild the `ThinkingPolicy` before the next `start_*`. Verified by grep: all four spawn sites above send `host_config` exactly once, immediately after `wait_ready`; no mid-flow resend exists today — the rule exists so future sites cannot violate it (a unit test asserts two `host_config` sends yield two capabilities replies and the second is consumed).
- Flows that run without a host (`host: None` in learning jobs) never start sessions and need no policy.

### 4. Host-side spec vocabulary (runtime/pi-host/src/structured.ts)

- `THINKING_LEVELS` (line 199) extends from 4 to the 7-value list; every `requireEnum(..., THINKING_LEVELS)` site (execution specs, payloads, and the new capabilities validator) accepts exactly the seven values. Rust serde and TS validators stay in lockstep; both reject unknown level strings as typed errors.

### 5. Config semantics + docs

- `thinking_level` in `[model_profiles.*]` accepts all seven values and overrides catalog/option defaults for sessions on that profile. `HostModelProfile` on the wire still carries only model+endpoint — session levels stay explicit per execution spec (plan-section-18 invariant preserved).
- Documentation updates (complete list): `docs/pi-host-protocol.md` (version 2, `model_capabilities`, derivation semantics, permissive fake/scripted double, `NISSE_PI_HOST_CAPABILITIES` override and its loud-failure rule); the `src/jury/protocol.rs` module doc ("frozen at 1" → version 2, vocabulary now includes `model_capabilities`); the `src/config/mod.rs` doc example (`thinking_level = "low"` stays valid; mention override semantics); grep-sweep for remaining "frozen at 1"/"protocol version 1" prose in both trees and update.

## Tests

- **Shared golden fixture** `runtime/pi-host/fixtures/thinking-levels.golden.json`: cases of `{model: {reasoning, thinkingLevelMap}, expected_supported, requested, expected_clamp}` including the `zai/glm-5.3` shape (`medium→high`, `off→low`), non-reasoning (`["off"]`), xhigh/max present/absent, no-map pass-through, plus the level-order array. Consumed by both `cargo test` (via `include_str!` + serde) and `npm test` — one source of truth for the two reimplementations. (The installed SDK does not export `clampThinkingLevel`/`EXTENDED_THINKING_LEVELS` from its public types — verified — so clamp fidelity is pinned by the golden fixture; the TS drift canary below covers derivation.)
- Rust unit (`src/domain/model.rs`): serde round-trip all seven levels; rejection of unknown/case-mismatched values; clamp via the golden fixture.
- Rust unit (`src/jury/thinking.rs`): override precedence (explicit > profile override > default) including a consolidation-style case proving the override wins when the site's level arrives as `default`; clamp note text; per-profile fail-closed reasons (unresolved, missing, empty set) naming the profile; unused-profile problems surface only via `warnings()` and never abort.
- Rust integration (`tests/integration/`): existing fake-runtime suites keep passing (permissive capabilities). New cases with `NISSE_PI_HOST_CAPABILITIES` restricting a profile: review completes, diagnostics contain the clamp note, and the scripted recorder (`NISSE_PI_SESSION_RECORD`) shows the clamped level; unresolvable-model capabilities fail closed with the surfaced reason; malformed override exits loudly.
- Rust host tests (`tests/integration/pi_host.rs` doubles): a host that sends `host_ready` but never `model_capabilities` yields the typed timeout error (tested with a short explicit timeout parameter on `wait_model_capabilities` — the parameter is how production passes `HOST_READY_TIMEOUT`; no env knob needed); two `host_config` sends yield two capabilities replies and the second is consumed after the stash reset.
- TS unit (`runtime/pi-host/src/*.test.ts`): supported-levels derivation from the golden fixture and against the installed SDK's real catalog (drift canary: `zai/glm-5.3` → `["low","high","max"]`); `model_capabilities` emission after `host_config` in fake mode; env override honored and ignored in real mode; interleave test (host_config resolution in flight while `start_review` + `tool_response` flow) proving the loop does not serialize; all seven levels round-trip through every `requireEnum(..., THINKING_LEVELS)` validator; protocol version 2 round-trip and wrong-version rejection.
- Full suites: `cargo test`, `cd runtime/pi-host && npm run typecheck && npm test`.

## Rollout

- Rebuild the release binary and install to `~/.local/bin/nisse` (current install location).
- Update `~/.nisse/config.toml`: both profiles to `thinking_level = "max"` (valid for `zai/glm-5.3` and now effective), refresh the comment to list all seven values and note that unsupported levels are clamped with a diagnostic.
- Verify with `nisse doctor` + a smoke review in this repo.
