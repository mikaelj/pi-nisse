/**
 * Nisse Pi review host entry point (plan sections 20 and 21).
 *
 * Lifecycle:
 *
 * 1. parse `--runtime fake|pi` (default `pi`) and the fault selector;
 * 2. announce `host_ready` (or perform the scripted fault);
 * 3. serve the JSONL message loop on stdin until EOF or `cancel`;
 * 4. exit when stdin closes.
 *
 * stdout carries protocol messages only — one JSON object per line — and
 * every log line goes to stderr. The host is a child process, not a
 * daemon: it exists only for the duration of the semantic work assigned
 * by the Rust core.
 */

import * as readline from "node:readline";

import { runFindingEquivalenceCheck, type EquivalenceDeps } from "./equivalence.ts";
import {
  runDomainDescription,
  type DomainDescriptionDeps,
} from "./domain.ts";
import {
  runFactVerification,
  type FactVerificationDeps,
} from "./verify.ts";
import {
  runFactSynthesis,
  type FactSynthesisDeps,
} from "./synthesis.ts";
import { createLimiter, type Limiter } from "./limiter.ts";
import {
  decodeIncomingMessage,
  encodeMessage,
  ERROR_CODES,
  PROTOCOL_VERSION,
  ProtocolError,
  type HostToRustType,
} from "./protocol.ts";
import { loadSessionScript, createScriptedAgentFactory } from "./scripted.ts";
import {
  createInvestigationSession,
  createReviewerSession,
  ReviewerSessionError,
  type PiSessionDeps,
  type SessionAgentFactory,
} from "./session.ts";
import { ToolBridge } from "./tools.ts";
import { parseFault, type HostRuntime } from "./types.ts";
import {
  validateCancel,
  validateDomainDescriptionRequest,
  validateFactSynthesisRequest,
  validateFactVerificationRequest,
  validateFindingEquivalenceCheck,
  validateHostConfig,
  validateStartInvestigation,
  THINKING_LEVELS,
  validateStartReview,
  ValidationError,
  type DomainDescriptionRequest,
  type FactSynthesisRequest,
  type FactVerificationRequest,
  type FindingEquivalenceCheck,
  type HostConfig,
  type ReviewerExecutionSpec,
} from "./structured.ts";

interface Options {
  readonly runtime: HostRuntime;
}

function parseArgs(argv: string[]): Options {
  const usage =
    "usage: main.ts [--runtime <fake|pi>]" +
    " (faults are selected with NISSE_PI_HOST_FAULT)";
  let runtime: HostRuntime | undefined;
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === undefined) {
      continue;
    }
    if (arg === "--runtime") {
      const value = argv[index + 1];
      if (value !== "fake" && value !== "pi") {
        process.stderr.write(`${usage}\n`);
        process.exit(2);
      }
      runtime = value;
      index += 1;
      continue;
    }
    process.stderr.write(`unknown argument \`${arg}\`; ${usage}\n`);
    process.exit(2);
  }
  return { runtime: runtime ?? "pi" };
}

const options = parseArgs(process.argv.slice(2));
const fault = (() => {
  try {
    return parseFault(process.env["NISSE_PI_HOST_FAULT"]);
  } catch (err) {
    process.stderr.write(`nisse-pi-host: ${String(err)}\n`);
    process.exit(2);
  }
})();

let idCounter = 0;
const nextId = (): string => `host-${process.pid}-${(idCounter += 1)}`;

/** Protocol writes: the only permitted stdout traffic. */
function send(type: HostToRustType, id: string, payload: Record<string, unknown>): void {
  process.stdout.write(`${encodeMessage(type, id, payload)}\n`);
}

function sendHostError(code: string, message: string): void {
  send("host_error", nextId(), { error: { code, message } });
}

/** Logs go to stderr only (stdout purity). */
function log(message: string): void {
  process.stderr.write(`nisse-pi-host: ${message}\n`);
}

const cancelledReviews = new Set<string>();
const startedReviews = new Set<string>();
const bridge = new ToolBridge({ nextId, send });

/** Model profiles supplied by the Rust core through `host_config`. */
const hostModels = new Map<string, { readonly model: string; readonly endpoint?: string }>();

/**
 * Answer every `host_config` with one `model_capabilities` message
 * (protocol version 2): the resolved model and its supported thinking
 * levels per profile, derived from the local Pi model catalog.
 *
 * Resolution for the real Pi runtime is asynchronous; replies are
 * chained so they always arrive in `host_config` order. The
 * deterministic test doubles (fake runtime, scripted sessions) report
 * every profile as supporting all levels — optionally overridden by
 * the test-only `NISSE_PI_HOST_CAPABILITIES` environment variable
 * (active only in fake/scripted modes; malformed there means a broken
 * test setup and fails loudly).
 */
let capabilitiesChain: Promise<void> = Promise.resolve();

function sendModelCapabilities(hostConfigId: string): void {
  // The recovered promise (`.catch` applied *before* the assignment)
  // is what chains forward: a single resolution failure must not
  // poison the chain and silence every later host_config reply.
  void (capabilitiesChain = capabilitiesChain
    .then(async () => {
      const payload = await resolveModelCapabilities();
      send("model_capabilities", nextId(), {
        host_config_id: hostConfigId,
        profiles: Object.fromEntries(
          [...payload.profiles.entries()].map(([name, entry]) => [
            name,
            {
              model: entry.model,
              supported_thinking_levels: [...entry.supported_thinking_levels],
            },
          ]),
        ),
        unresolved: Object.fromEntries(payload.unresolved.entries()),
      });
    })
    .catch((err: unknown) => {
      // Resolution itself failed for an unexpected reason: report it as
      // a protocol-level host error and keep the chain alive for the
      // next host_config (the Rust core fails closed on the missing
      // capabilities reply).
      sendHostError(
        ERROR_CODES.internal,
        `model capabilities could not be resolved: ${
          err instanceof Error ? err.message : String(err)
        }`,
      );
    }));
}

/** Resolve capabilities for the currently configured profiles. */
async function resolveModelCapabilities(): Promise<{
  readonly profiles: Map<string, { readonly model: string; readonly supported_thinking_levels: readonly string[] }>;
  readonly unresolved: Map<string, string>;
}> {
  const scripted = process.env["NISSE_PI_SESSION_SCRIPT"] !== undefined;
  if (options.runtime === "fake" || scripted) {
    return testDoubleCapabilities();
  }
  const { loadModelRuntime } = await import("./pi-agent.ts");
  const { supportedThinkingLevels } = await import("./capabilities.ts");
  const profiles = new Map();
  const unresolved = new Map<string, string>();
  let runtime: Awaited<ReturnType<typeof loadModelRuntime>>;
  try {
    runtime = await loadModelRuntime();
  } catch (err: unknown) {
    const reason = err instanceof Error ? err.message : String(err);
    for (const [name] of hostModels) {
      unresolved.set(name, `the Pi model runtime is unavailable: ${reason}`);
    }
    return { profiles, unresolved };
  }
  for (const [name, profile] of hostModels) {
    const separator = profile.model.indexOf("/");
    const provider = profile.model.slice(0, separator);
    const modelId = profile.model.slice(separator + 1);
    try {
      const model = runtime.getModel(provider, modelId);
      if (model === undefined) {
        unresolved.set(
          name,
          `model \`${profile.model}\` is not registered in the local model catalog`,
        );
        continue;
      }
      profiles.set(name, {
        model: profile.model,
        supported_thinking_levels: supportedThinkingLevels(model),
      });
    } catch (err: unknown) {
      unresolved.set(
        name,
        `model \`${profile.model}\` could not be resolved: ${
          err instanceof Error ? err.message : String(err)
        }`,
      );
    }
  }
  return { profiles, unresolved };
}

/** Permissive double plus the test-only env override (loud on misuse). */
function testDoubleCapabilities(): {
  readonly profiles: Map<string, { readonly model: string; readonly supported_thinking_levels: readonly string[] }>;
  readonly unresolved: Map<string, string>;
} {
  const profiles = new Map();
  const unresolved = new Map<string, string>();
  const override = process.env["NISSE_PI_HOST_CAPABILITIES"];
  if (override === undefined) {
    for (const [name, entry] of hostModels) {
      profiles.set(name, { model: entry.model, supported_thinking_levels: [...THINKING_LEVELS] });
    }
    return { profiles, unresolved };
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(override);
  } catch (err: unknown) {
    failLoudOverride(`invalid JSON: ${err instanceof Error ? err.message : String(err)}`);
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    failLoudOverride("expected a JSON object of profile name to level array");
  }
  for (const [name, entry] of hostModels) {
    const levels = (parsed as Record<string, unknown>)[name];
    if (levels === undefined) {
      profiles.set(name, { model: entry.model, supported_thinking_levels: [...THINKING_LEVELS] });
      continue;
    }
    if (!Array.isArray(levels) || levels.length === 0) {
      failLoudOverride(`profile \`${name}\`: expected a non-empty array of levels`);
    }
    for (const level of levels) {
      if (!THINKING_LEVELS.includes(level as (typeof THINKING_LEVELS)[number])) {
        failLoudOverride(`profile \`${name}\`: unknown thinking level \`${String(level)}\``);
      }
    }
    profiles.set(name, { model: entry.model, supported_thinking_levels: levels as string[] });
  }
  return { profiles, unresolved };
}

/** A malformed test override must never be silently ignored. */
function failLoudOverride(detail: string): never {
  const message = `NISSE_PI_HOST_CAPABILITIES is malformed (${detail}); refusing to guess`;
  process.stderr.write(`nisse-pi-host: ${message}\n`);
  sendHostError(ERROR_CODES.internal, message);
  process.exit(2);
}

/** Reviewer-session concurrency bound (plan milestone 29: reviewer
 * concurrency obeys config). The Rust core forwards the configured
 * bound through `host_config`; until one arrives the compiled default
 * (the frozen jury maximum) applies. */
const DEFAULT_MAX_CONCURRENT_REVIEWERS = 4;
let reviewerLimiter: Limiter = createLimiter(DEFAULT_MAX_CONCURRENT_REVIEWERS);

/** Resolve the agent-session factory for the pi runtime (once per host). */
let agentFactoryPromise: Promise<SessionAgentFactory> | undefined;
function resolvePiAgentFactory(): Promise<SessionAgentFactory> {
  agentFactoryPromise ??= (async () => {
    const scriptPath = process.env["NISSE_PI_SESSION_SCRIPT"];
    if (scriptPath !== undefined && scriptPath.length > 0) {
      const script = loadSessionScript(scriptPath);
      const recordPath = process.env["NISSE_PI_SESSION_RECORD"];
      return createScriptedAgentFactory(script, recordPath);
    }
    return (await import("./pi-agent.ts")).piAgentFactory;
  })();
  return agentFactoryPromise;
}

function piSessionDeps(kind: "plan" | "code"): PiSessionDeps {
  return { models: hostModels, kind, resolveAgent: resolvePiAgentFactory };
}

/** Dependencies of pi-runtime equivalence sessions. */
function equivalenceDeps(): EquivalenceDeps {
  return { models: hostModels, resolveAgent: resolvePiAgentFactory };
}

/** Dependencies of pi-runtime domain-description sessions. */
function domainDescriptionDeps(): DomainDescriptionDeps {
  return { models: hostModels, resolveAgent: resolvePiAgentFactory };
}

/** Dependencies of pi-runtime fact-verification sessions. */
function factVerificationDeps(): FactVerificationDeps {
  return { models: hostModels, resolveAgent: resolvePiAgentFactory };
}

/** Dependencies of pi-runtime fact-synthesis sessions. */
function factSynthesisDeps(): FactSynthesisDeps {
  return { models: hostModels, resolveAgent: resolvePiAgentFactory };
}

function isCancelled(reviewId: string): boolean {
  return cancelledReviews.has(reviewId);
}

async function runReviewer(
  reviewId: string,
  spec: ReviewerExecutionSpec,
  kind: "plan" | "code",
): Promise<void> {
  // Reviewer sessions run concurrently, but never more at once than the
  // configured bound: acquire a slot first, and only then announce the
  // reviewer as started (a queued reviewer has not started yet).
  await reviewerLimiter
    .run(async (): Promise<void> => {
      if (isCancelled(reviewId)) {
        return;
      }
      send("reviewer_started", nextId(), {
        review_id: reviewId,
        reviewer_id: spec.reviewer_id,
      });
      try {
        const session = createReviewerSession(options.runtime, spec, piSessionDeps(kind));
        const report = await session.run({
          reviewId,
          tools: bridge,
          isCancelled: () => isCancelled(reviewId),
        });
        if (isCancelled(reviewId)) {
          return;
        }
        send("reviewer_completed", nextId(), {
          review_id: reviewId,
          reviewer_id: spec.reviewer_id,
          report,
        });
      } catch (err) {
        if (isCancelled(reviewId)) {
          return;
        }
        send("reviewer_failed", nextId(), {
          review_id: reviewId,
          reviewer_id: spec.reviewer_id,
          error: {
            code: err instanceof ReviewerSessionError ? err.code : "session_error",
            message: err instanceof Error ? err.message : String(err),
          },
        });
      }
    })
    .done;
}

async function runInvestigationReviewer(
  reviewId: string,
  spec: ReviewerExecutionSpec,
  task: string,
  result: string,
): Promise<void> {
  await reviewerLimiter
    .run(async (): Promise<void> => {
      if (isCancelled(reviewId)) {
        return;
      }
      send("reviewer_started", nextId(), {
        review_id: reviewId,
        reviewer_id: spec.reviewer_id,
      });
      try {
        const session = createInvestigationSession(options.runtime, spec, {
          models: hostModels,
          task,
          result,
          resolveAgent: resolvePiAgentFactory,
        });
        const report = await session.run({
          reviewId,
          tools: bridge,
          isCancelled: () => isCancelled(reviewId),
        });
        if (isCancelled(reviewId)) {
          return;
        }
        send("investigation_reviewer_completed", nextId(), {
          review_id: reviewId,
          reviewer_id: spec.reviewer_id,
          report,
        });
      } catch (err) {
        if (isCancelled(reviewId)) {
          return;
        }
        send("reviewer_failed", nextId(), {
          review_id: reviewId,
          reviewer_id: spec.reviewer_id,
          error: {
            code: err instanceof ReviewerSessionError ? err.code : "session_error",
            message: err instanceof Error ? err.message : String(err),
          },
        });
      }
    })
    .done;
}

function handleStartInvestigation(payload: unknown): void {
  const investigation = validateStartInvestigation(payload);
  startedReviews.add(investigation.review_id);
  for (const spec of investigation.reviewers) {
    void runInvestigationReviewer(
      investigation.review_id,
      spec,
      investigation.task,
      investigation.result,
    );
  }
}

function handleStartReview(payload: unknown): void {
  const review = validateStartReview(payload);
  if (fault === "malformed-after-ready") {
    // Scripted protocol corruption: emit an unparsable stdout line.
    process.stdout.write("not-json{\n");
    return;
  }
  if (fault === "review-hang") {
    for (const value of review.reviewers) {
      send("reviewer_started", nextId(), {
        review_id: review.review_id,
        reviewer_id: value.reviewer_id,
      });
    }
    return;
  }
  startedReviews.add(review.review_id);
  for (const spec of review.reviewers) {
    void runReviewer(review.review_id, spec, review.kind);
  }
}

function handleCancel(payload: unknown): void {
  const { review_id: reviewId } = validateCancel(payload);
  if (reviewId !== undefined) {
    cancelledReviews.add(reviewId);
    startedReviews.delete(reviewId);
    bridge.cancel(reviewId);
    return;
  }
  for (const pending of startedReviews) {
    cancelledReviews.add(pending);
  }
  startedReviews.clear();
  bridge.cancel(undefined);
}

function handleHostConfig(id: string, payload: unknown): void {
  const config: HostConfig = validateHostConfig(payload);
  hostModels.clear();
  for (const [name, profile] of config.models) {
    hostModels.set(name, profile);
  }
  reviewerLimiter = createLimiter(
    config.maxConcurrentReviewers ?? DEFAULT_MAX_CONCURRENT_REVIEWERS,
  );
  log(
    `host_config: stored ${hostModels.size} model profile(s); ` +
      `max ${config.maxConcurrentReviewers ?? DEFAULT_MAX_CONCURRENT_REVIEWERS} ` +
      `concurrent reviewer session(s)`,
  );
  // Version 2: answer every host_config with the resolved model
  // capabilities. An empty table still gets an (empty) reply so the
  // Rust core's wait always completes.
  sendModelCapabilities(id);
}

/** Execute one equivalence check job: a completed result always answers
 * the check, with `uncertain` plus an error when the session failed. */
async function runEquivalenceJob(check: FindingEquivalenceCheck): Promise<void> {
  try {
    const result = await runFindingEquivalenceCheck(options.runtime, check, equivalenceDeps());
    send("finding_equivalence_completed", nextId(), {
      check_id: check.check_id,
      verdict: result.verdict,
      ...(result.error !== undefined ? { error: result.error } : {}),
    });
  } catch (err) {
    // Unexpected internal failures are protocol-level host errors.
    sendHostError(
      ERROR_CODES.internal,
      `equivalence check \`${check.check_id}\` failed unexpectedly: ${
        err instanceof Error ? err.message : String(err)
      }`,
    );
  }
}

/** Execute one domain-description job: a completed result always answers
 * the job, with an error (and no description) when the session failed. */
async function runDomainDescriptionJob(request: DomainDescriptionRequest): Promise<void> {
  try {
    const result = await runDomainDescription(
      options.runtime,
      request,
      domainDescriptionDeps(),
    );
    send("domain_description_completed", nextId(), {
      description_id: request.description_id,
      ...(result.description !== undefined ? { description: result.description } : {}),
      ...(result.error !== undefined ? { error: result.error } : {}),
    });
  } catch (err) {
    // Unexpected internal failures are protocol-level host errors.
    sendHostError(
      ERROR_CODES.internal,
      `domain description \`${request.description_id}\` failed unexpectedly: ${
        err instanceof Error ? err.message : String(err)
      }`,
    );
  }
}

/** Execute one fact-verification job: a completed result always answers
 * the job, with `insufficient` plus an error when the session failed. */
async function runFactVerificationJob(request: FactVerificationRequest): Promise<void> {
  try {
    const result = await runFactVerification(options.runtime, request, factVerificationDeps());
    send("fact_verification_completed", nextId(), {
      verification_id: request.verification_id,
      verdict: result.verdict,
      ...(result.error !== undefined ? { error: result.error } : {}),
    });
  } catch (err) {
    // Unexpected internal failures are protocol-level host errors.
    sendHostError(
      ERROR_CODES.internal,
      `fact verification \`${request.verification_id}\` failed unexpectedly: ${
        err instanceof Error ? err.message : String(err)
      }`,
    );
  }
}

/** Execute one fact-synthesis job: a completed result always answers
 * the job — with a candidate, with an explicit `cannot_merge` refusal,
 * or with an error when the session failed. */
async function runFactSynthesisJob(request: FactSynthesisRequest): Promise<void> {
  try {
    const result = await runFactSynthesis(options.runtime, request, factSynthesisDeps());
    switch (result.kind) {
      case "merged":
        send("fact_synthesis_completed", nextId(), {
          synthesis_id: request.synthesis_id,
          candidate: result.candidate,
        });
        return;
      case "cannot_merge":
        send("fact_synthesis_completed", nextId(), {
          synthesis_id: request.synthesis_id,
          cannot_merge: true,
        });
        return;
      case "failed":
        send("fact_synthesis_completed", nextId(), {
          synthesis_id: request.synthesis_id,
          error: result.error,
        });
        return;
    }
  } catch (err) {
    // Unexpected internal failures are protocol-level host errors.
    sendHostError(
      ERROR_CODES.internal,
      `fact synthesis \`${request.synthesis_id}\` failed unexpectedly: ${
        err instanceof Error ? err.message : String(err)
      }`,
    );
  }
}

function dispatch(message: ReturnType<typeof decodeIncomingMessage>): void {
  switch (message.type) {
    case "host_config":
      handleHostConfig(message.id, message.payload);
      return;
    case "start_review":
      handleStartReview(message.payload);
      return;
    case "start_investigation":
      handleStartInvestigation(message.payload);
      return;
    case "start_finding_equivalence_check": {
      const check = validateFindingEquivalenceCheck(message.payload);
      void runEquivalenceJob(check);
      return;
    }
    case "start_domain_description": {
      const request = validateDomainDescriptionRequest(message.payload);
      void runDomainDescriptionJob(request);
      return;
    }
    case "start_fact_verification": {
      const request = validateFactVerificationRequest(message.payload);
      void runFactVerificationJob(request);
      return;
    }
    case "start_fact_synthesis": {
      const request = validateFactSynthesisRequest(message.payload);
      void runFactSynthesisJob(request);
      return;
    }
    case "tool_response":
      bridge.handleResponse(message.payload);
      return;
    case "cancel":
      handleCancel(message.payload);
      return;
    default:
      // Defensive: every frozen vocabulary entry is handled above, so
      // this arm only guards future vocabulary extensions — it fails
      // deterministically instead of guessing.
      sendHostError(
        ERROR_CODES.unsupportedMessage,
        `\`${message.type}\` is defined by the protocol but not handled by this host version`,
      );
  }
}

process.on("uncaughtException", (err) => {
  log(`uncaught exception: ${err instanceof Error ? err.stack : String(err)}`);
  sendHostError(ERROR_CODES.internal, "uncaught exception in the pi review host");
  process.exit(1);
});

// Startup faults -----------------------------------------------------------

if (fault === "crash-on-start") {
  log("fault crash-on-start: exiting before startup completes");
  process.exit(3);
}

if (fault === "oversized-line") {
  // Scripted protocol-bound violation: a stdout line beyond the frozen
  // 8 MiB protocol bound. The Rust reader must reject it without
  // buffering it. Exit only after the write flushed so the reader sees
  // the full oversized line, not a truncated one.
  log("fault oversized-line: writing a line beyond the protocol bound");
  process.stdout.write(`${"x".repeat(9 * 1024 * 1024)}\n`, () => {
    process.exit(0);
  });
} else if (fault === "binary-stdout") {
  // Scripted protocol-bound violation: stdout bytes that are not valid
  // UTF-8. The Rust reader must fail closed.
  log("fault binary-stdout: writing non-UTF-8 bytes to stdout");
  process.stdout.write(Buffer.from([0x22, 0xff, 0xfe, 0x22, 0x0a]), () => {
    process.exit(0);
  });
}

if (fault === "startup-hang") {
  log("fault startup-hang: host_ready will never be sent");
}

if (fault === "wrong-version") {
  // Scripted protocol corruption: announce readiness with a future
  // protocol version. The Rust core must reject it.
  process.stdout.write(
    `${JSON.stringify({
      protocol_version: PROTOCOL_VERSION + 98,
      id: "fault-ready",
      type: "host_ready",
      runtime: options.runtime,
    })}\n`,
  );
  log("fault wrong-version: sent host_ready with a wrong protocol version");
} else if (fault !== "startup-hang") {
  send("host_ready", nextId(), { runtime: options.runtime });
  if (fault === "stderr-noise") {
    log("fault stderr-noise: regular diagnostics on stderr");
    log("fault stderr-noise: stdout still carries protocol messages only");
  }
}

if (fault === "ready-then-exit") {
  log("fault ready-then-exit: exiting after host_ready");
  process.exit(0);
}

// Message loop -------------------------------------------------------------

const silenced = fault === "startup-hang" || fault === "wrong-version";
const rl = readline.createInterface({ input: process.stdin, terminal: false });

rl.on("line", (line: string) => {
  if (line.trim().length === 0) {
    sendHostError(ERROR_CODES.malformedMessage, "empty protocol line");
    return;
  }
  try {
    const message = decodeIncomingMessage(line);
    if (silenced) {
      // Scripted hang: decode (and log) but never respond.
      log(`fault: ignoring inbound \`${message.type}\` message`);
      return;
    }
    dispatch(message);
  } catch (err) {
    if (err instanceof ProtocolError || err instanceof ValidationError) {
      sendHostError(err.code, err.message);
      return;
    }
    sendHostError(
      ERROR_CODES.internal,
      `failed to handle message: ${err instanceof Error ? err.message : String(err)}`,
    );
  }
});

rl.on("close", () => {
  // stdin closed: the Rust core is shutting the host down.
  log("stdin closed; shutting down");
  bridge.cancel(undefined);
  process.exit(0);
});
