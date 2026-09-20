/**
 * Narrow finding-equivalence checks (plan section 38).
 *
 * Consolidation may only group findings that a constrained semantic
 * check judged `same`. This module is the host side of that check: one
 * isolated non-juror session receives exactly two findings (with their
 * reviewer provenance) and must answer with exactly one JSON object
 * `{"verdict":"same"|"different"|"uncertain"}`.
 *
 * The session is fully explicit — model profile and thinking level come
 * from the Rust core's payload, no tools are granted — and its output is
 * parsed strictly with exactly one constrained repair attempt. A failed
 * check never fails the batch: it completes with the forced `uncertain`
 * verdict plus an error so the Rust core can record a diagnostic and
 * leave the findings ungrouped (uncertainty produces less automation,
 * never guessed grouping).
 *
 * The fake runtime answers deterministically from two optional
 * environment variables (set by the Rust tests):
 *
 * - `NISSE_FAKE_EQUIVALENCE_VERDICTS`: comma-separated verdicts
 *   consumed in order; missing or exhausted entries answer `uncertain`;
 * - `NISSE_FAKE_EQUIVALENCE_FAIL`: when truthy, every check fails with
 *   a scripted error (forced `uncertain` with an error).
 */

import { resolvePiModel, type HostModelTable } from "./models.ts";
import {
  EQUIVALENCE_VERDICTS,
  isPlainObject,
  type EquivalenceFinding,
  type EquivalenceVerdict,
  type FindingEquivalenceCheck,
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

/** Stable non-juror identity of the equivalence session. */
export const EQUIVALENCE_SESSION_ID = "finding-equivalence";

/** Stable error codes of equivalence checks. */
export const EQUIVALENCE_ERROR_CODES = {
  modelUnresolved: "model_profile_unresolved",
  invalidVerdict: "invalid_equivalence_verdict",
  timeout: "session_timeout",
  session: "equivalence_session_error",
  scriptedFailure: "scripted_fake_failure",
} as const;

/** Dependencies for pi-runtime equivalence sessions. */
export interface EquivalenceDeps {
  /** Model profiles supplied by the Rust core through `host_config`. */
  readonly models: HostModelTable;
  /** Resolves the agent-session factory (real Pi SDK or scripted mock). */
  readonly resolveAgent: () => Promise<SessionAgentFactory>;
}

/** The outcome of one equivalence check. */
export interface EquivalenceResult {
  readonly verdict: EquivalenceVerdict;
  /** Present when the verdict fell back to `uncertain`. */
  readonly error?: { readonly code: string; readonly message: string };
}

/** Build the system prompt of the equivalence session. */
export function buildEquivalenceSystemPrompt(): string {
  return [
    "You are the Nisse finding-equivalence checker, a constrained non-juror session.",
    "",
    "You receive exactly two review findings that were produced",
    "independently by different reviewers of the same change. Decide",
    "whether they describe the same underlying issue.",
    "",
    "Answer with exactly one JSON object and nothing else — no prose, no",
    "Markdown fences:",
    "",
    '  {"verdict":"same"}       both findings describe the same underlying issue;',
    "                          fixing one would also resolve the other",
    '  {"verdict":"different"}  they describe different issues',
    '  {"verdict":"uncertain"}  you cannot decide',
    "",
    "Rules:",
    "- Different severities, wording, or evidence excerpts do not alone",
    "  make two findings different; compare the underlying issue.",
    "- Findings about different locations or different impact classes are",
    "  normally different.",
    "- When in doubt, answer uncertain; uncertainty never merges findings.",
  ].join("\n");
}

/** Render one finding of the comparison. */
function renderFinding(label: string, entry: EquivalenceFinding): string {
  const finding = entry.finding;
  const lines: string[] = [
    `${label} (reviewer \`${entry.reviewer_id}\`, finding id \`${finding.id}\`):`,
    `impact_class: ${finding.impact_class}`,
    `category: ${finding.category}`,
    `severity: ${finding.proposed_severity}`,
    `title: ${finding.title}`,
    `claim: ${finding.claim}`,
    `explanation: ${finding.explanation}`,
    `recommendation: ${finding.recommendation}`,
    "evidence:",
  ];
  if (finding.evidence.length === 0) {
    lines.push("  (none)");
  } else {
    for (const evidence of finding.evidence) {
      const location: string[] = [evidence.kind];
      if (evidence.path !== undefined) {
        location.push(`at ${evidence.path}`);
      }
      if (evidence.line_start !== undefined || evidence.line_end !== undefined) {
        location.push(`lines ${evidence.line_start ?? "?"}-${evidence.line_end ?? "?"}`);
      }
      lines.push(`  - ${location.join(" ")}: ${evidence.excerpt}`);
    }
  }
  return lines.join("\n");
}

/** Build the first (and usually only) user message of the session. */
export function buildEquivalenceMessage(check: FindingEquivalenceCheck): string {
  return [
    "Decide whether the following two findings describe the same",
    "underlying issue. Answer with exactly one JSON object",
    '{"verdict":"same"|"different"|"uncertain"}.',
    "",
    renderFinding("Finding A", check.finding_a),
    "",
    renderFinding("Finding B", check.finding_b),
  ].join("\n");
}

/** Build the constrained repair prompt for one malformed answer. */
export function buildEquivalenceRepairPrompt(answer: string, reason: string): string {
  return [
    "Your previous answer was not a valid equivalence verdict:",
    "",
    `Reason: ${reason}`,
    "",
    "Your previous answer was:",
    "",
    "<previous_answer>",
    answer,
    "</previous_answer>",
    "",
    'Respond again with exactly one JSON object {"verdict":"same"} or',
    '{"verdict":"different"} or {"verdict":"uncertain"}. No other text.',
  ].join("\n");
}

/**
 * Parse one model answer as a strict equivalence verdict.
 *
 * The answer must be exactly one JSON object with the single field
 * `verdict`; anything else is malformed and triggers the repair path.
 */
export function parseEquivalenceVerdict(answer: string): EquivalenceVerdict {
  const parsed = extractReportJson(answer);
  if (!isPlainObject(parsed)) {
    throw new Error("verdict answer is not a JSON object");
  }
  const keys = Object.keys(parsed);
  if (keys.length !== 1 || keys[0] !== "verdict") {
    throw new Error('verdict answer must have exactly one field `verdict`');
  }
  const verdict = parsed["verdict"];
  if (
    typeof verdict !== "string" ||
    !(EQUIVALENCE_VERDICTS as readonly string[]).includes(verdict)
  ) {
    throw new Error(
      `\`verdict\` must be one of ${EQUIVALENCE_VERDICTS.map((v) => `"${v}"`).join("|")}, ` +
        `got ${JSON.stringify(verdict)}`,
    );
  }
  return verdict as EquivalenceVerdict;
}

// ---------------------------------------------------------------------------
// Fake runtime
// ---------------------------------------------------------------------------

/** Consumed-in-order verdicts of the fake runtime (`undefined` = not
 * loaded from the environment yet). */
let fakeVerdicts: string[] | undefined;

function nextFakeVerdict(): EquivalenceVerdict {
  fakeVerdicts ??= (process.env["NISSE_FAKE_EQUIVALENCE_VERDICTS"] ?? "")
    .split(",")
    .map((entry) => entry.trim())
    .filter((entry) => entry.length > 0);
  const next = fakeVerdicts.shift();
  if (next !== undefined && (EQUIVALENCE_VERDICTS as readonly string[]).includes(next)) {
    return next as EquivalenceVerdict;
  }
  return "uncertain";
}

/** Reset the scripted fake state so a new environment is honored
 * (tests). */
export function resetFakeEquivalenceState(): void {
  fakeVerdicts = undefined;
}

function fakeEquivalenceFailureEnabled(): boolean {
  const raw = (process.env["NISSE_FAKE_EQUIVALENCE_FAIL"] ?? "").trim().toLowerCase();
  return raw === "1" || raw === "true" || raw === "yes";
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/** Run one equivalence check with the selected runtime. */
export async function runFindingEquivalenceCheck(
  runtime: HostRuntime,
  check: FindingEquivalenceCheck,
  deps?: EquivalenceDeps,
): Promise<EquivalenceResult> {
  switch (runtime) {
    case "fake":
      return runFakeEquivalenceCheck();
    case "pi":
      if (deps === undefined) {
        throw new Error("pi runtime equivalence checks require session dependencies");
      }
      return runPiEquivalenceCheck(check, deps);
  }
}

function runFakeEquivalenceCheck(): EquivalenceResult {
  if (fakeEquivalenceFailureEnabled()) {
    return {
      verdict: "uncertain",
      error: {
        code: EQUIVALENCE_ERROR_CODES.scriptedFailure,
        message: "scripted fake equivalence failure",
      },
    };
  }
  return { verdict: nextFakeVerdict() };
}

async function runPiEquivalenceCheck(
  check: FindingEquivalenceCheck,
  deps: EquivalenceDeps,
): Promise<EquivalenceResult> {
  const failed = (code: string, message: string): EquivalenceResult => ({
    verdict: "uncertain",
    error: { code, message },
  });

  let profile;
  try {
    profile = resolvePiModel(deps.models, check.model_profile);
  } catch (err) {
    return failed(
      EQUIVALENCE_ERROR_CODES.modelUnresolved,
      err instanceof Error ? err.message : String(err),
    );
  }

  const deadline = SessionDeadline.start(check.timeout_ms);
  const config: AgentSessionConfig = {
    reviewerId: EQUIVALENCE_SESSION_ID,
    model: profile.model,
    ...(profile.endpoint !== undefined ? { endpoint: profile.endpoint } : {}),
    thinkingLevel: check.thinking_level,
    systemPrompt: buildEquivalenceSystemPrompt(),
    firstMessage: buildEquivalenceMessage(check),
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
      EQUIVALENCE_ERROR_CODES.session,
      `creating the equivalence agent session failed: ${
        err instanceof Error ? err.message : String(err)
      }`,
    );
  }

  try {
    await deadline.race(agent.prompt(config.firstMessage), "awaiting the verdict");
    let firstAnswer = "";
    try {
      firstAnswer = agent.finalText() ?? "";
      return { verdict: parseEquivalenceVerdict(firstAnswer) };
    } catch (err) {
      if (err instanceof ReviewerSessionError) {
        throw err;
      }
      const reason = err instanceof Error ? err.message : String(err);
      // Exactly one constrained repair attempt (plan section 35).
      await deadline.race(
        agent.prompt(buildEquivalenceRepairPrompt(firstAnswer, reason)),
        "awaiting the repaired verdict",
      );
      try {
        const secondAnswer = agent.finalText() ?? "";
        return { verdict: parseEquivalenceVerdict(secondAnswer) };
      } catch (secondErr) {
        const secondReason = secondErr instanceof Error ? secondErr.message : String(secondErr);
        return failed(
          EQUIVALENCE_ERROR_CODES.invalidVerdict,
          `verdict answer never became valid (first: ${reason}; after repair: ${secondReason})`,
        );
      }
    }
  } catch (err) {
    if (err instanceof ReviewerSessionError) {
      return failed(err.code, err.message);
    }
    return failed(
      EQUIVALENCE_ERROR_CODES.session,
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
