/**
 * Reviewer sessions (plan sections 18, 20, 35, and 36).
 *
 * A session executes exactly one [`ReviewerExecutionSpec`]: explicit
 * instructions, checklist, model profile, thinking level, initial context,
 * and read-only capabilities. Sessions are created per reviewer, never
 * reused, and disposed after completion.
 *
 * Two runtimes exist:
 *
 * - `fake`: the deterministic fake reviewer used to prove the process
 *   boundary (no model is contacted);
 * - `pi`: constrained isolated Pi SDK sessions. The spec is mapped onto an
 *   explicit in-memory agent session — model, thinking level, system
 *   instructions, checklist, read-only tools, and initial context are all
 *   set explicitly, and no Pi resources (Main Pi conversation, project
 *   extensions, skills, prompt templates, AGENTS/context files) are
 *   inherited. Reviewer output is parsed strictly; malformed output gets
 *   exactly one constrained repair attempt; sessions run under the spec's
 *   timeout.
 */

import { resolveFakeModel, resolvePiModel, type HostModelTable } from "./models.ts";
import type { ToolBridge } from "./tools.ts";
import {
  REVIEW_KINDS,
  REVIEWER_TOOLS,
  validateReviewerReport,
  type CandidateProjectFact,
  type ContextEntry,
  type EvidenceRef,
  type ReviewerExecutionSpec,
  type ReviewerReport,
  type ReviewerTool,
} from "./structured.ts";
import type { HostRuntime } from "./types.ts";

/** Stable error codes for reviewer session failures. */
export const SESSION_ERROR_CODES = {
  modelUnresolved: "model_profile_unresolved",
  sdkUnavailable: "sdk_unavailable",
  invalidReport: "invalid_report",
  timeout: "session_timeout",
  cancelled: "review_cancelled",
  session: "session_error",
} as const;

/** A reviewer session failure carrying a stable protocol error code. */
export class ReviewerSessionError extends Error {
  readonly code: string;

  constructor(code: string, message: string) {
    super(message);
    this.name = "ReviewerSessionError";
    this.code = code;
  }
}

/** Per-review execution context handed to a session. */
export interface SessionContext {
  readonly reviewId: string;
  readonly tools: ToolBridge;
  /** Whether the review was cancelled before the session finishes. */
  isCancelled(): boolean;
}

/** One isolated reviewer session. */
export interface ReviewerSession {
  readonly reviewerId: string;
  run(context: SessionContext): Promise<ReviewerReport>;
}

// ---------------------------------------------------------------------------
// Pi session mapping (plan sections 18 and 20)
// ---------------------------------------------------------------------------

/** The resource-isolation contract of every Pi reviewer session. */
export interface AgentSessionIsolation {
  /** Session history is in-memory only and disposed after the review. */
  readonly inMemorySession: true;
  /** No built-in coding tools (read/bash/edit/write/…) are enabled. */
  readonly noBuiltinTools: true;
  /** Project and global Pi extensions are not discovered. */
  readonly noExtensions: true;
  /** Skills are not discovered. */
  readonly noSkills: true;
  /** Prompt templates are not discovered. */
  readonly noPromptTemplates: true;
  /** AGENTS/context files are not discovered. */
  readonly noContextFiles: true;
  /** Themes are not discovered. */
  readonly noThemes: true;
}

/**
 * The fully explicit agent-session configuration derived from one
 * [`ReviewerExecutionSpec`].
 *
 * The configuration is data: the real Pi runtime turns it into an isolated
 * `AgentSession`, and the scripted test runtime records it so the mapping
 * itself stays verifiable without a model.
 */
export interface AgentSessionConfig {
  /** Stable reviewer identity (from the spec, never from the model). */
  readonly reviewerId: string;
  /** Resolved model identifier (`provider/model-id`). */
  readonly model: string;
  /** Optional explicit endpoint base URL from trusted configuration. */
  readonly endpoint?: string;
  /** Explicit thinking level for the session. */
  readonly thinkingLevel: string;
  /** Full system instructions: reviewer instructions, checklist, output contract. */
  readonly systemPrompt: string;
  /** First user message: rendered initial context plus the review task. */
  readonly firstMessage: string;
  /** Names of the read-only tools granted to this reviewer. */
  readonly toolNames: readonly string[];
  /** Frozen isolation guarantees of the session. */
  readonly isolation: AgentSessionIsolation;
}

/** The minimal agent surface a reviewer session needs. */
export interface SessionAgent {
  /** Run one conversation turn to completion. */
  prompt(text: string): Promise<void>;
  /** The final assistant text of the session so far, if any. */
  finalText(): string | undefined;
  /** Abort an in-flight turn (best effort). */
  abort(): Promise<void> | void;
  /** Dispose the session and all resources it holds. */
  dispose(): void;
}

/** A factory creating one agent session from an explicit configuration. */
export type SessionAgentFactory = (
  config: AgentSessionConfig,
  tools: readonly ReviewerToolDef[],
) => Promise<SessionAgent>;

/** Read-only tool description presented to the model. */
export interface ReviewerToolDef {
  readonly name: ReviewerTool;
  readonly description: string;
  readonly parameters: Record<string, unknown>;
  execute(
    args: Record<string, unknown>,
  ): Promise<{ readonly text: string; readonly isError: boolean }>;
}

/** Dependencies for constructing Pi-runtime reviewer sessions. */
export interface PiSessionDeps {
  /** Model profiles supplied by the Rust core through `host_config`. */
  readonly models: HostModelTable;
  /** The kind of review being performed (`plan` or `code`). */
  readonly kind: (typeof REVIEW_KINDS)[number];
  /** Resolves the agent-session factory (real Pi SDK or scripted mock). */
  readonly resolveAgent: () => Promise<SessionAgentFactory>;
}

/** Per-call bound for a single lookup tool call. */
const MAX_TOOL_CALL_MS = 30_000;

// ---------------------------------------------------------------------------
// Prompt construction
// ---------------------------------------------------------------------------

const IMPACT_CLASS_LIST = [
  "correctness",
  "security",
  "data_loss",
  "deadlock",
  "crash",
  "compatibility",
  "performance",
  "test_gap",
  "maintainability",
  "style",
  "other",
].join(" | ");

const SEVERITY_LIST = "info | low | medium | high | blocker";

/** Build the explicit system prompt for one reviewer spec. */
export function buildSystemPrompt(spec: ReviewerExecutionSpec): string {
  const checklist = spec.checklist.map((item, index) => `${index + 1}. ${item}`).join("\n");
  return [
    `You are Nisse reviewer \`${spec.reviewer_id}\` (${spec.specialization}).`,
    "",
    "## Instructions",
    "",
    spec.instructions,
    "",
    "## Checklist",
    "",
    "Work through every checklist item in order. Do not invent your own review methodology:",
    "",
    checklist,
    "",
    "## Read-only investigation tools",
    "",
    "You may use the granted read-only lookup tools listed in the conversation for",
    "supplemental investigation. They resolve against the exact review snapshot only.",
    "You have no other tools. Never claim a tool result you did not receive.",
    "",
    "## Output contract",
    "",
    "Your final answer must be exactly one JSON object and nothing else:",
    "no prose before or after, no Markdown fences. It must validate against this schema:",
    "",
    "{",
    '  "reviewer_id": string  // exactly "' + spec.reviewer_id + '"',
    '  "summary": string      // one-paragraph summary of what you reviewed',
    '  "findings": [          // may be empty when nothing is wrong',
    "    {",
    '      "id": string         // short stable id unique within your report',
    `      "impact_class": one of ${IMPACT_CLASS_LIST}`,
    `      "proposed_severity": one of ${SEVERITY_LIST}`,
    '      "category": string   // short lowercase category',
    '      "title": string      // one-line title',
    '      "claim": string      // the precise claim being made',
    '      "explanation": string // why this matters, referencing evidence',
    '      "recommendation": string // concrete action to resolve it',
    '      "evidence": [        // at least one entry for high/blocker findings',
    "        {",
    '          "kind": "source" | "diff" | "plan" | "project_fact",',
    '          "path": string, "symbol": string, "line_start": number, "line_end": number,',
    '          "content_key": string, "plan_section": string, "record_id": string,',
    "          // all fields above except `kind` are optional,",
    '          "excerpt": string  // short verbatim excerpt supporting the claim',
    "        }",
    "      ]",
    "    }",
    "  ],",
    '  "candidate_project_facts": [ // may be empty',
    "    {",
    '      "subject": string, "claim": string, "tags": string[],',
    '      "evidence": Evidence[], "confidence": number  // 0..1',
    "    }",
    "  ],",
    '  "model_metadata": {}   // you must emit an empty JSON object here',
    '  "duration_ms": 0       // you must emit 0 here',
    "}",
    "",
    "Only report findings supported by the provided context or tool results.",
    "Every finding must cite concrete evidence you actually saw.",
  ].join("\n");
}

/** Render one initial context entry as a labeled text block. */
export function renderContextEntry(entry: ContextEntry, index: number): string {
  const location: string[] = [];
  if (entry.path !== undefined) {
    location.push(`path: ${entry.path}`);
  }
  if (entry.symbol !== undefined) {
    location.push(`symbol: ${entry.symbol}`);
  }
  if (entry.line_start !== undefined || entry.line_end !== undefined) {
    const start = entry.line_start ?? "?";
    const end = entry.line_end ?? "?";
    location.push(`lines: ${start}-${end}`);
  }
  if (entry.record_id !== undefined) {
    location.push(`record: ${entry.record_id}`);
  }
  if (entry.practice_id !== undefined) {
    location.push(`practice: ${entry.practice_id}`);
  }
  const header = `[${index}] ${entry.kind}: ${entry.title}`;
  const body = location.length > 0 ? `${header}\n  ${location.join("  ")}` : header;
  return `${body}\n  ${entry.text.replace(/\n/g, "\n  ")}`;
}

/** Build the first user message: the review task plus initial context. */
export function buildFirstMessage(
  kind: (typeof REVIEW_KINDS)[number],
  spec: ReviewerExecutionSpec,
): string {
  const lines: string[] = [
    `Review task: ${kind} review. Reviewer role: ${spec.reviewer_id} (${spec.specialization}).`,
    "",
    "Initial context below was pre-selected by Nisse as the strongest starting evidence.",
    "It is authoritative review input; investigate further only with the read-only tools.",
    "",
  ];
  if (spec.initial_context.length === 0) {
    lines.push("(no initial context entries were supplied)");
  } else {
    spec.initial_context.forEach((entry, index) => {
      lines.push(renderContextEntry(entry, index + 1));
      lines.push("");
    });
  }
  lines.push(
    "Work through your checklist now and respond with exactly one JSON object",
    "matching the output contract in your instructions. No other text.",
  );
  return lines.join("\n");
}

// ---------------------------------------------------------------------------
// Strict report parsing (plan section 35)
// ---------------------------------------------------------------------------

/**
 * Extract and parse the report JSON from a model answer.
 *
 * Strict: the text must be exactly one JSON value, optionally wrapped in a
 * single Markdown code fence. Prose, trailing junk, and non-JSON text are
 * malformed and trigger the constrained repair path.
 */
export function extractReportJson(text: string): unknown {
  let trimmed = text.trim();
  if (trimmed.startsWith("```")) {
    const firstNewline = trimmed.indexOf("\n");
    if (firstNewline === -1) {
      throw new Error("fenced block never ends");
    }
    trimmed = trimmed.slice(firstNewline + 1);
    const closing = trimmed.lastIndexOf("```");
    if (closing !== -1) {
      trimmed = trimmed.slice(0, closing);
    }
    trimmed = trimmed.trim();
  }
  if (trimmed.length === 0) {
    throw new Error("answer is empty");
  }
  try {
    return JSON.parse(trimmed);
  } catch (err) {
    throw new Error(`answer is not exactly one JSON value: ${String(err)}`);
  }
}

/**
 * Parse and validate one model answer as a strict `ReviewerReport`.
 *
 * Reviewer identity is stamped from the execution spec afterwards: the
 * model is an untrusted semantic worker and never defines who reviewed.
 */
export function parseReviewerReport(answer: string, spec: ReviewerExecutionSpec): ReviewerReport {
  const parsed = extractReportJson(answer);
  const report = validateReviewerReport(parsed);
  return { ...report, reviewer_id: spec.reviewer_id };
}

/** Build the constrained repair prompt for one malformed answer. */
export function buildRepairPrompt(answer: string, reason: string, spec: ReviewerExecutionSpec): string {
  return [
    "Your previous answer was not a valid review report:",
    "",
    `Reason: ${reason}`,
    "",
    "Your previous answer was:",
    "",
    "<previous_answer>",
    answer,
    "</previous_answer>",
    "",
    "Respond again with exactly one JSON object conforming to the output contract",
    `in your instructions (reviewer_id exactly "${spec.reviewer_id}", model_metadata {},`,
    "duration_ms 0). Output the JSON object only — no prose, no fences.",
  ].join("\n");
}

// ---------------------------------------------------------------------------
// Deadline helper
// ---------------------------------------------------------------------------

/** A session-scoped deadline derived from `timeout_ms`. */
export class SessionDeadline {
  private readonly expiresAt: number;

  private constructor(timeoutMs: number) {
    this.expiresAt = Date.now() + timeoutMs;
  }

  /** Start a deadline of `timeoutMs` milliseconds. */
  static start(timeoutMs: number): SessionDeadline {
    return new SessionDeadline(timeoutMs);
  }

  /** Remaining milliseconds; fails the session once the deadline passed. */
  remainingMs(): number {
    const remaining = this.expiresAt - Date.now();
    if (remaining <= 0) {
      throw new ReviewerSessionError(
        SESSION_ERROR_CODES.timeout,
        "reviewer session exceeded its timeout budget",
      );
    }
    return remaining;
  }

  /** Wait for `promise`, failing with `session_timeout` past the deadline. */
  async race<T>(promise: Promise<T>, what: string): Promise<T> {
    const remaining = this.remainingMs();
    let timer: ReturnType<typeof setTimeout> | undefined;
    const expired = new Promise<never>((resolve) => {
      timer = setTimeout(
        () =>
          resolve(
            Promise.reject(
              new ReviewerSessionError(
                SESSION_ERROR_CODES.timeout,
                `reviewer session timed out while ${what}`,
              ),
            ) as never,
          ),
        remaining,
      );
      timer.unref?.();
    });
    try {
      return await Promise.race([promise, expired]);
    } finally {
      if (timer !== undefined) {
        clearTimeout(timer);
      }
    }
  }
}

// ---------------------------------------------------------------------------
// Read-only session tools (plan section 22)
// ---------------------------------------------------------------------------

function requireStringArg(
  args: Record<string, unknown>,
  field: string,
): string {
  const value = args[field];
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`tool argument \`${field}\` must be a non-empty string`);
  }
  return value;
}

const TOOL_DESCRIPTIONS: Record<ReviewerTool, string> = {
  project_lookup:
    "Search the reviewed project snapshot. Input: {query: string}. " +
    "Returns a bounded ranked context bundle.",
  symbol_lookup:
    "Look up a symbol definition and metadata in the reviewed snapshot. " +
    "Input: {symbol: string}.",
  source_lookup:
    "Read source from the reviewed snapshot only. Input: {path: string, " +
    "range?: {line_start: number, line_end: number}}.",
  relation_lookup:
    "Look up known bounded relationships of a symbol in the reviewed snapshot. " +
    "Input: {symbol: string}.",
};

/**
 * Build the read-only session tools granted to a reviewer.
 *
 * Only capabilities from `spec.allowed_capabilities` are created; tool calls
 * are bridged to the Rust core, which resolves them against the exact
 * target snapshot.
 */
export function buildReviewerTools(
  spec: ReviewerExecutionSpec,
  context: SessionContext,
  deadline: SessionDeadline,
): ReviewerToolDef[] {
  const tools: ReviewerToolDef[] = [];
  for (const name of REVIEWER_TOOLS) {
    if (!spec.allowed_capabilities.includes(name)) {
      continue;
    }
    const parameters = toolParameters(name);
    tools.push({
      name,
      description: TOOL_DESCRIPTIONS[name],
      parameters,
      execute: async (args) => {
        deadline.remainingMs();
        if (context.isCancelled()) {
          return { text: "error: review cancelled", isError: true };
        }
        const callArgs =
          name === "source_lookup"
            ? {
                path: requireStringArg(args, "path"),
                ...(args["range"] !== undefined ? { range: args["range"] } : {}),
              }
            : name === "project_lookup"
              ? { query: requireStringArg(args, "query") }
              : { symbol: requireStringArg(args, "symbol") };
        const timeoutMs = Math.min(deadline.remainingMs(), MAX_TOOL_CALL_MS);
        const outcome = await context.tools.call(
          context.reviewId,
          spec.reviewer_id,
          name,
          callArgs,
          timeoutMs,
        );
        if (context.isCancelled()) {
          return { text: "error: review cancelled", isError: true };
        }
        if (outcome.ok) {
          return { text: JSON.stringify(outcome.bundle), isError: false };
        }
        return {
          text: `error ${outcome.error.code}: ${outcome.error.message}`,
          isError: true,
        };
      },
    });
  }
  return tools;
}

function toolParameters(name: ReviewerTool): Record<string, unknown> {
  const stringProperty = (description: string) => ({ type: "string", description });
  switch (name) {
    case "project_lookup":
      return {
        type: "object",
        properties: { query: stringProperty("What to search for") },
        required: ["query"],
        additionalProperties: false,
      };
    case "symbol_lookup":
    case "relation_lookup":
      return {
        type: "object",
        properties: { symbol: stringProperty("Symbol name, e.g. a function or type") },
        required: ["symbol"],
        additionalProperties: false,
      };
    case "source_lookup":
      return {
        type: "object",
        properties: {
          path: stringProperty("Repository-relative file path"),
          range: {
            type: "object",
            description: "Optional inclusive line range",
            properties: {
              line_start: { type: "integer", minimum: 1 },
              line_end: { type: "integer", minimum: 1 },
            },
            required: ["line_start", "line_end"],
            additionalProperties: false,
          },
        },
        required: ["path"],
        additionalProperties: false,
      };
  }
}

// ---------------------------------------------------------------------------
// Fake runtime (unchanged deterministic behavior)
// ---------------------------------------------------------------------------

/**
 * Scripted fake candidate project facts (plan section 46).
 *
 * Deterministic, env-controlled so Rust tests can exercise the whole
 * fact-candidate pipeline offline:
 *
 * - `NISSE_FAKE_REVIEWER_FACT_CLAIM`: when set to non-empty text, every
 *   fake reviewer proposes exactly one candidate fact carrying it;
 * - `NISSE_FAKE_REVIEWER_FACT_SUBJECT`: subject override (default
 *   `fake fact subject`);
 * - `NISSE_FAKE_REVIEWER_FACT_CONFIDENCE`: confidence override
 *   (default `0.95`).
 *
 * The candidate's evidence is derived from the first source-ish initial
 * context entry with a path, so it validates against the snapshot.
 */
export function fakeCandidateFacts(
  contextEntries: readonly ContextEntry[],
): CandidateProjectFact[] {
  const claim = process.env["NISSE_FAKE_REVIEWER_FACT_CLAIM"];
  if (claim === undefined || claim.trim().length === 0) {
    return [];
  }
  const subject = process.env["NISSE_FAKE_REVIEWER_FACT_SUBJECT"] ?? "fake fact subject";
  const rawConfidence = Number(process.env["NISSE_FAKE_REVIEWER_FACT_CONFIDENCE"] ?? "0.95");
  const confidence = Number.isFinite(rawConfidence) ? rawConfidence : 0.95;
  const entry =
    contextEntries.find(
      (candidate) =>
        (candidate.kind === "source" ||
          candidate.kind === "source_skeleton" ||
          candidate.kind === "symbol_definition") &&
        candidate.path !== undefined,
    ) ?? contextEntries.find((candidate) => candidate.path !== undefined);
  const evidence: EvidenceRef =
    entry === undefined
      ? { kind: "source", excerpt: "fake runtime: no initial context entries were supplied" }
      : {
          kind: "source",
          ...(entry.path !== undefined ? { path: entry.path } : {}),
          ...(entry.symbol !== undefined ? { symbol: entry.symbol } : {}),
          ...(entry.line_start !== undefined ? { line_start: entry.line_start } : {}),
          ...(entry.line_end !== undefined ? { line_end: entry.line_end } : {}),
          excerpt: Array.from(entry.text).slice(0, 800).join(""),
        };
  return [
    {
      subject,
      claim,
      tags: ["fake"],
      evidence: [evidence],
      confidence,
    },
  ];
}

/**
 * Deterministic fake reviewer.
 *
 * Behavior is fully determined by the execution spec and two optional
 * environment variables (set by the Rust tests):
 *
 * - `NISSE_FAKE_REVIEWER_FAIL_FOR`: comma-separated reviewer ids whose
 *   sessions fail with a scripted error (every attempt);
 * - `NISSE_FAKE_REVIEWER_FAIL_ONCE_FOR`: comma-separated reviewer ids
 *   whose first session attempt fails (to exercise the retry policy);
 * - `NISSE_FAKE_REVIEWER_ATTEMPTS_FILE`: path where every session attempt
 *   appends one JSON line `{reviewer_id, attempt}` so tests can prove how
 *   often a reviewer actually ran.
 *
 * Tool use: when the spec grants `project_lookup` and at least one lookup
 * call, the session performs exactly one bounded lookup and reflects the
 * outcome in its report summary.
 */
export class FakeReviewerSession implements ReviewerSession {
  readonly reviewerId: string;
  private readonly spec: ReviewerExecutionSpec;
  private readonly attempt: number;

  constructor(spec: ReviewerExecutionSpec, attempt = 1) {
    this.spec = spec;
    this.reviewerId = spec.reviewer_id;
    this.attempt = attempt;
  }

  async run(context: SessionContext): Promise<ReviewerReport> {
    const started = Date.now();
    const attemptsFile = process.env["NISSE_FAKE_REVIEWER_ATTEMPTS_FILE"];
    if (attemptsFile !== undefined && attemptsFile.length > 0) {
      const { appendFileSync } = await import("node:fs");
      appendFileSync(
        attemptsFile,
        `${JSON.stringify({ reviewer_id: this.spec.reviewer_id, attempt: this.attempt })}\n`,
      );
    }
    if (failList("NISSE_FAKE_REVIEWER_FAIL_FOR").includes(this.spec.reviewer_id)) {
      throw new Error(`scripted fake failure for reviewer \`${this.spec.reviewer_id}\``);
    }
    if (
      this.attempt === 1 &&
      failList("NISSE_FAKE_REVIEWER_FAIL_ONCE_FOR").includes(this.spec.reviewer_id)
    ) {
      throw new Error(
        `scripted first-attempt fake failure for reviewer \`${this.spec.reviewer_id}\``,
      );
    }

    let toolsUsed = 0;
    let toolEntries = -1;
    let toolError: string | undefined;
    if (
      this.spec.allowed_capabilities.includes("project_lookup") &&
      this.spec.max_lookup_calls >= 1
    ) {
      toolsUsed = 1;
      const timeoutMs = Math.min(this.spec.timeout_ms, 10_000);
      const outcome = await context.tools.call(
        context.reviewId,
        this.spec.reviewer_id,
        "project_lookup",
        { query: "nisse fake runtime probe" },
        timeoutMs,
      );
      if (outcome.ok) {
        toolEntries = outcome.bundle.entries.length;
      } else {
        toolError = `${outcome.error.code}: ${outcome.error.message}`;
      }
    }

    const summary =
      `fake review by ${this.spec.reviewer_id}: ` +
      `checklist=${this.spec.checklist.length} ` +
      `context=${this.spec.initial_context.length} ` +
      `tools=${toolsUsed} ` +
      `tool_entries=${toolEntries}` +
      (toolError !== undefined ? ` tool_error=${toolError}` : "") +
      ` attempt=${this.attempt}`;

    const evidence = fakeEvidence(this.spec.initial_context);
    const report: ReviewerReport = {
      reviewer_id: this.spec.reviewer_id,
      summary,
      findings: [
        {
          id: "fake-1",
          impact_class: "correctness",
          proposed_severity: "info",
          category: "fake",
          title: `Fake finding from ${this.spec.reviewer_id}`,
          claim: "The deterministic fake runtime always reports one informational finding.",
          explanation:
            "This finding exists to exercise the protocol boundary; it carries no review judgment.",
          recommendation: "Ignore this finding; it is generated by the fake reviewer runtime.",
          evidence: [evidence],
        },
      ],
      candidate_project_facts: fakeCandidateFacts(this.spec.initial_context),
      model_metadata: {
        runtime: "fake",
        model: resolveFakeModel(this.spec.model_profile).model,
        thinking_level: this.spec.thinking_level,
      },
      duration_ms: Math.max(0, Date.now() - started),
    };
    if (context.isCancelled()) {
      throw new Error("review cancelled before completion");
    }
    return report;
  }
}

// ---------------------------------------------------------------------------
// Pi runtime (real constrained sessions; plan sections 20 and 35)
// ---------------------------------------------------------------------------

/** Records why an answer turn failed, for repair and failure reporting. */
interface TurnFailure {
  readonly reason: string;
  readonly answer: string;
}

/** Isolated Pi SDK reviewer session driven by one execution spec. */
export class PiReviewerSession implements ReviewerSession {
  readonly reviewerId: string;
  private readonly spec: ReviewerExecutionSpec;
  private readonly deps: PiSessionDeps;

  constructor(spec: ReviewerExecutionSpec, deps: PiSessionDeps) {
    this.spec = spec;
    this.deps = deps;
    this.reviewerId = spec.reviewer_id;
  }

  async run(context: SessionContext): Promise<ReviewerReport> {
    const started = Date.now();
    const profile = this.resolveProfile();
    const deadline = SessionDeadline.start(this.spec.timeout_ms);

    const factory = await this.resolveFactory();
    const config: AgentSessionConfig = {
      reviewerId: this.spec.reviewer_id,
      model: profile.model,
      ...(profile.endpoint !== undefined ? { endpoint: profile.endpoint } : {}),
      thinkingLevel: this.spec.thinking_level,
      systemPrompt: buildSystemPrompt(this.spec),
      firstMessage: buildFirstMessage(this.deps.kind, this.spec),
      toolNames: this.spec.allowed_capabilities.slice(),
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

    const tools = buildReviewerTools(this.spec, context, deadline);
    let agent;
    try {
      agent = await factory(config, tools);
    } catch (err) {
      if (err instanceof ReviewerSessionError) {
        throw err;
      }
      throw new ReviewerSessionError(
        SESSION_ERROR_CODES.sdkUnavailable,
        `creating the reviewer agent session failed: ${
          err instanceof Error ? err.message : String(err)
        }`,
      );
    }
    let turns = 0;
    let repaired = false;
    try {
      // Turn 1: the review itself.
      await deadline.race(agent.prompt(config.firstMessage), "awaiting the review answer");
      turns += 1;
      let report: ReviewerReport;
      let firstAnswer = "";
      try {
        firstAnswer = this.collectAnswer(agent, 1).answer;
        report = parseReviewerReport(firstAnswer, this.spec);
      } catch (err) {
        if (err instanceof ReviewerSessionError) {
          // Timeouts and other session-level failures are not repairable.
          throw err;
        }
        // Exactly one constrained repair attempt (plan section 35).
        const failure: TurnFailure = {
          reason: err instanceof Error ? err.message : String(err),
          answer: firstAnswer,
        };
        repaired = true;
        turns += 1;
        await deadline.race(
          agent.prompt(buildRepairPrompt(failure.answer, failure.reason, this.spec)),
          "awaiting the repaired answer",
        );
        const second = this.collectAnswer(agent, 2);
        try {
          report = parseReviewerReport(second.answer, this.spec);
        } catch (secondErr) {
          const reason = secondErr instanceof Error ? secondErr.message : String(secondErr);
          throw new ReviewerSessionError(
            SESSION_ERROR_CODES.invalidReport,
            `reviewer answer never became a valid report (first: ${failure.reason}; ` +
              `after repair: ${reason})`,
          );
        }
      }

      if (context.isCancelled()) {
        throw new ReviewerSessionError(SESSION_ERROR_CODES.cancelled, "review was cancelled");
      }

      // Host-authoritative metadata: identity, duration, and model facts are
      // stamped deterministically; the model's own claims are never trusted.
      const durationMs = Math.max(0, Date.now() - started);
      return {
        ...report,
        reviewer_id: this.spec.reviewer_id,
        duration_ms: durationMs,
        model_metadata: {
          runtime: "pi",
          model: profile.model,
          ...(profile.endpoint !== undefined ? { endpoint: profile.endpoint } : {}),
          thinking_level: this.spec.thinking_level,
          turns,
          ...(repaired ? { repaired: true } : {}),
        },
      };
    } finally {
      try {
        await agent.abort();
      } catch {
        // Aborting a finished or failed agent is best effort.
      }
      agent.dispose();
    }
  }

  private resolveProfile() {
    try {
      return resolvePiModel(this.deps.models, this.spec.model_profile);
    } catch (err) {
      throw new ReviewerSessionError(
        SESSION_ERROR_CODES.modelUnresolved,
        err instanceof Error ? err.message : String(err),
      );
    }
  }

  private async resolveFactory(): Promise<SessionAgentFactory> {
    try {
      return await this.deps.resolveAgent();
    } catch (err) {
      throw new ReviewerSessionError(
        SESSION_ERROR_CODES.sdkUnavailable,
        `the pi reviewer runtime is unavailable: ${err instanceof Error ? err.message : String(err)}`,
      );
    }
  }

  private collectAnswer(agent: SessionAgent, turn: number): { readonly answer: string } {
    const answer = agent.finalText();
    if (answer === undefined) {
      throw new Error(`turn ${turn} produced no assistant text`);
    }
    return { answer };
  }
}

/** Attempt counters for the fake runtime's fail-once fault. */
const fakeAttempts = new Map<string, number>();

/** Create the session implementation for the selected runtime. */
export function createReviewerSession(
  runtime: HostRuntime,
  spec: ReviewerExecutionSpec,
  deps?: PiSessionDeps,
): ReviewerSession {
  switch (runtime) {
    case "fake":
      {
        const attempt = (fakeAttempts.get(spec.reviewer_id) ?? 0) + 1;
        fakeAttempts.set(spec.reviewer_id, attempt);
        return new FakeReviewerSession(spec, attempt);
      }
    case "pi":
      if (deps === undefined) {
        throw new Error("pi runtime sessions require session dependencies");
      }
      return new PiReviewerSession(spec, deps);
  }
}

function failList(variable: string): string[] {
  const raw = process.env[variable] ?? "";
  return raw
    .split(",")
    .map((entry) => entry.trim())
    .filter((entry) => entry.length > 0);
}

function fakeEvidence(context: ContextEntry[]): EvidenceRef {
  const first = context[0];
  if (first === undefined) {
    return {
      kind: "source",
      excerpt: "fake runtime: no initial context entries were supplied",
    };
  }
  return {
    kind: "source",
    ...(first.path !== undefined ? { path: first.path } : {}),
    ...(first.symbol !== undefined ? { symbol: first.symbol } : {}),
    ...(first.line_start !== undefined ? { line_start: first.line_start } : {}),
    ...(first.line_end !== undefined ? { line_end: first.line_end } : {}),
    excerpt: first.text,
  };
}
