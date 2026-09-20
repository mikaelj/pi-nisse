/**
 * Independent fact verification (plan section 47 step 2).
 *
 * Reviewers may propose project facts, but reviewer output can never
 * create durable knowledge directly. This module is the host side of
 * the independent verification: one isolated non-juror session — always
 * a separate session from the reviewer that proposed the candidate —
 * receives exactly one claim plus the exact evidence excerpts and must
 * answer with exactly one JSON object
 * `{"verdict":"supported"|"contradicted"|"insufficient"}`.
 *
 * The session is fully explicit — model profile and thinking level come
 * from the Rust core's payload, no tools are granted — and its output is
 * parsed strictly with exactly one constrained repair attempt. A failed
 * session never fails the exchange: it completes with the forced
 * `insufficient` verdict plus an error so the Rust core keeps the
 * candidate as pending local state (uncertainty never persists or
 * rejects a fact).
 *
 * The fake runtime answers deterministically from two optional
 * environment variables (set by the Rust tests):
 *
 * - `NISSE_FAKE_FACT_VERIFICATION_VERDICTS`: comma-separated verdicts
 *   consumed in order; missing or exhausted entries answer
 *   `insufficient` (the fail-safe verdict);
 * - `NISSE_FAKE_FACT_VERIFICATION_FAIL`: when truthy, every job
 *   completes with an error (forced `insufficient` with an error).
 */

import { resolvePiModel, type HostModelTable } from "./models.ts";
import {
  FACT_VERIFICATION_VERDICTS,
  isPlainObject,
  type EvidenceRef,
  type FactVerificationRequest,
  type FactVerificationVerdict,
} from "./structured.ts";
import {
  ReviewerSessionError,
  SessionDeadline,
  extractReportJson,
  type AgentSessionConfig,
  type SessionAgent,
  type SessionAgentFactory,
} from "./session.ts";
import type { HostRuntime } from "./types.ts";

/** Stable non-juror identity of the fact-verification session. */
export const FACT_VERIFICATION_SESSION_ID = "fact-verifier";

/** Stable error codes of fact-verification jobs. */
export const FACT_VERIFICATION_ERROR_CODES = {
  modelUnresolved: "model_profile_unresolved",
  invalidVerdict: "invalid_fact_verification_verdict",
  timeout: "session_timeout",
  session: "fact_verification_session_error",
  scriptedFailure: "scripted_fake_failure",
} as const;

/** Dependencies for pi-runtime fact-verification sessions. */
export interface FactVerificationDeps {
  /** Model profiles supplied by the Rust core through `host_config`. */
  readonly models: HostModelTable;
  /** Resolves the agent-session factory (real Pi SDK or scripted mock). */
  readonly resolveAgent: () => Promise<SessionAgentFactory>;
}

/** The outcome of one fact-verification job. */
export interface FactVerificationResult {
  readonly verdict: FactVerificationVerdict;
  /** Present when the verdict was forced to `insufficient` by a
   * failed session. */
  readonly error?: { readonly code: string; readonly message: string };
}

/** Build the system prompt of the fact-verification session. */
export function buildFactVerificationSystemPrompt(): string {
  return [
    "You are the Nisse fact verifier, a constrained non-juror session.",
    "",
    "You receive exactly one claim about a software project and the",
    "exact evidence excerpts a reviewer cited for it. Decide whether",
    "the evidence supports the claim.",
    "",
    "Answer with exactly one JSON object and nothing else — no prose, no",
    "Markdown fences:",
    "",
    '  {"verdict":"supported"}      the excerpts demonstrate the claim is true',
    '  {"verdict":"contradicted"}   the excerpts show the claim is false',
    '  {"verdict":"insufficient"}   the excerpts neither demonstrate nor',
    "                              contradict the claim",
    "",
    "Rules:",
    "- Judge only the supplied excerpts; never use outside knowledge.",
    "- `supported` requires the excerpts themselves to demonstrate the",
    "  claim, not merely be consistent with it.",
    "- When in doubt, answer insufficient; uncertainty never persists",
    "  a fact.",
  ].join("\n");
}

/** Render one evidence excerpt of the comparison. */
function renderEvidence(evidence: EvidenceRef, index: number): string {
  const location: string[] = [evidence.kind];
  if (evidence.path !== undefined) {
    location.push(`at ${evidence.path}`);
  }
  if (evidence.line_start !== undefined || evidence.line_end !== undefined) {
    location.push(`lines ${evidence.line_start ?? "?"}-${evidence.line_end ?? "?"}`);
  }
  return `  ${index + 1}. ${location.join(" ")}: ${evidence.excerpt}`;
}

/** Build the first (and usually only) user message of the session. */
export function buildFactVerificationMessage(request: FactVerificationRequest): string {
  return [
    "Decide whether the following evidence supports the exact claim.",
    'Answer with exactly one JSON object {"verdict":"supported"} or',
    '{"verdict":"contradicted"} or {"verdict":"insufficient"}.',
    "",
    "Claim:",
    `  ${request.claim}`,
    "",
    "Evidence excerpts:",
    ...request.evidence.map(renderEvidence),
  ].join("\n");
}

/** Build the constrained repair prompt for one malformed answer. */
export function buildFactVerificationRepairPrompt(answer: string, reason: string): string {
  return [
    "Your previous answer was not a valid verification verdict:",
    "",
    `Reason: ${reason}`,
    "",
    "Your previous answer was:",
    "",
    "<previous_answer>",
    answer,
    "</previous_answer>",
    "",
    'Respond again with exactly one JSON object {"verdict":"supported"} or',
    '{"verdict":"contradicted"} or {"verdict":"insufficient"}. No other text.',
  ].join("\n");
}

/**
 * Parse one model answer as a strict verification verdict.
 *
 * The answer must be exactly one JSON object with the single field
 * `verdict`; anything else is malformed and triggers the repair path.
 */
export function parseFactVerificationVerdict(answer: string): FactVerificationVerdict {
  const parsed = extractReportJson(answer);
  if (!isPlainObject(parsed)) {
    throw new Error("verdict answer is not a JSON object");
  }
  const keys = Object.keys(parsed);
  if (keys.length !== 1 || keys[0] !== "verdict") {
    throw new Error("verdict answer must have exactly one field `verdict`");
  }
  const verdict = parsed["verdict"];
  if (
    typeof verdict !== "string" ||
    !(FACT_VERIFICATION_VERDICTS as readonly string[]).includes(verdict)
  ) {
    throw new Error(
      `\`verdict\` must be one of ` +
        `${FACT_VERIFICATION_VERDICTS.map((v) => `"${v}"`).join("|")}, ` +
        `got ${JSON.stringify(verdict)}`,
    );
  }
  return verdict as FactVerificationVerdict;
}

// ---------------------------------------------------------------------------
// Fake runtime
// ---------------------------------------------------------------------------

/** Consumed-in-order verdicts of the fake runtime (`undefined` = not
 * loaded from the environment yet). */
let fakeVerdicts: string[] | undefined;

function nextFakeVerdict(): FactVerificationVerdict {
  fakeVerdicts ??= (process.env["NISSE_FAKE_FACT_VERIFICATION_VERDICTS"] ?? "")
    .split(",")
    .map((entry) => entry.trim())
    .filter((entry) => entry.length > 0);
  const next = fakeVerdicts.shift();
  if (next !== undefined && (FACT_VERIFICATION_VERDICTS as readonly string[]).includes(next)) {
    return next as FactVerificationVerdict;
  }
  return "insufficient";
}

/** Reset the scripted fake state so a new environment is honored
 * (tests). */
export function resetFakeFactVerificationState(): void {
  fakeVerdicts = undefined;
}

function fakeFactVerificationFailureEnabled(): boolean {
  const raw = (process.env["NISSE_FAKE_FACT_VERIFICATION_FAIL"] ?? "").trim().toLowerCase();
  return raw === "1" || raw === "true" || raw === "yes";
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/** Run one fact-verification job with the selected runtime. */
export async function runFactVerification(
  runtime: HostRuntime,
  request: FactVerificationRequest,
  deps?: FactVerificationDeps,
): Promise<FactVerificationResult> {
  switch (runtime) {
    case "fake":
      return runFakeFactVerification();
    case "pi":
      if (deps === undefined) {
        throw new Error("pi runtime fact verification requires session dependencies");
      }
      return runPiFactVerification(request, deps);
  }
}

function runFakeFactVerification(): FactVerificationResult {
  if (fakeFactVerificationFailureEnabled()) {
    return {
      verdict: "insufficient",
      error: {
        code: FACT_VERIFICATION_ERROR_CODES.scriptedFailure,
        message: "scripted fake fact-verification failure",
      },
    };
  }
  return { verdict: nextFakeVerdict() };
}

async function runPiFactVerification(
  request: FactVerificationRequest,
  deps: FactVerificationDeps,
): Promise<FactVerificationResult> {
  const failed = (code: string, message: string): FactVerificationResult => ({
    verdict: "insufficient",
    error: { code, message },
  });

  let profile;
  try {
    profile = resolvePiModel(deps.models, request.model_profile);
  } catch (err) {
    return failed(
      FACT_VERIFICATION_ERROR_CODES.modelUnresolved,
      err instanceof Error ? err.message : String(err),
    );
  }

  const deadline = SessionDeadline.start(request.timeout_ms);
  const config: AgentSessionConfig = {
    reviewerId: FACT_VERIFICATION_SESSION_ID,
    model: profile.model,
    ...(profile.endpoint !== undefined ? { endpoint: profile.endpoint } : {}),
    thinkingLevel: request.thinking_level,
    systemPrompt: buildFactVerificationSystemPrompt(),
    firstMessage: buildFactVerificationMessage(request),
    toolNames: [],
    isolation: {
      inMemorySession: true,
      noBuiltinTools: true,
      noExtensions: true,
      noSkills: true,
      noPromptTemplates: true,
      noContextFiles: true,
      noThemes: true,
    },
  };

  let agent: SessionAgent;
  try {
    const factory = await deps.resolveAgent();
    agent = await factory(config, []);
  } catch (err) {
    return failed(
      FACT_VERIFICATION_ERROR_CODES.session,
      `creating the fact-verification agent session failed: ${
        err instanceof Error ? err.message : String(err)
      }`,
    );
  }

  try {
    await deadline.race(agent.prompt(config.firstMessage), "awaiting the verdict");
    let firstAnswer = "";
    try {
      firstAnswer = agent.finalText() ?? "";
      return { verdict: parseFactVerificationVerdict(firstAnswer) };
    } catch (err) {
      if (err instanceof ReviewerSessionError) {
        throw err;
      }
      const reason = err instanceof Error ? err.message : String(err);
      // Exactly one constrained repair attempt (plan section 35).
      await deadline.race(
        agent.prompt(buildFactVerificationRepairPrompt(firstAnswer, reason)),
        "awaiting the repaired verdict",
      );
      try {
        const secondAnswer = agent.finalText() ?? "";
        return { verdict: parseFactVerificationVerdict(secondAnswer) };
      } catch (secondErr) {
        const secondReason = secondErr instanceof Error ? secondErr.message : String(secondErr);
        return failed(
          FACT_VERIFICATION_ERROR_CODES.invalidVerdict,
          `verdict answer never became valid (first: ${reason}; after repair: ${secondReason})`,
        );
      }
    }
  } catch (err) {
    if (err instanceof ReviewerSessionError) {
      return failed(err.code, err.message);
    }
    return failed(
      FACT_VERIFICATION_ERROR_CODES.session,
      err instanceof Error ? err.message : String(err),
    );
  } finally {
    try {
      await agent.abort();
    } catch {
      // Aborting a finished or failed agent is best effort.
    }
    agent.dispose();
  }
}
