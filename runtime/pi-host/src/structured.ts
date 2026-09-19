/**
 * Strict structured-output validation (plan section 35).
 *
 * Both directions of the host boundary are validated against explicit
 * schemas before use: inbound payloads from the Rust core and outbound
 * reviewer reports to the Rust core. Malformed data is rejected with a
 * clear reason; it never flows onward as guessed state.
 */

import { ERROR_CODES } from "./protocol.ts";

/** Validation failure with a stable protocol error code. */
export class ValidationError extends Error {
  readonly code: string;

  constructor(message: string) {
    super(message);
    this.name = "ValidationError";
    this.code = ERROR_CODES.invalidPayload;
  }
}

export function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export function requireObject(value: unknown, what: string): Record<string, unknown> {
  if (!isPlainObject(value)) {
    throw new ValidationError(`${what} must be a JSON object`);
  }
  return value;
}

export function requireString(object: Record<string, unknown>, field: string): string {
  const value = object[field];
  if (typeof value !== "string") {
    throw new ValidationError(`\`${field}\` must be a string`);
  }
  return value;
}

export function requireNonEmptyString(object: Record<string, unknown>, field: string): string {
  const value = requireString(object, field);
  if (value.length === 0) {
    throw new ValidationError(`\`${field}\` must be non-empty`);
  }
  return value;
}

export function requireOptionalString(
  object: Record<string, unknown>,
  field: string,
): string | undefined {
  const value = optionalField(object, field);
  if (value === undefined) {
    return undefined;
  }
  if (typeof value !== "string") {
    throw new ValidationError(`\`${field}\` must be a string when present`);
  }
  return value;
}

/**
 * Read an optional field, treating JSON `null` the same as absence.
 *
 * The Rust core serializes `Option<T>` as an explicit `null`; the host
 * accepts both spellings for optional fields.
 */
export function optionalField(
  object: Record<string, unknown>,
  field: string,
): unknown {
  const value = object[field];
  return value === null ? undefined : value;
}

export function requireStringArray(object: Record<string, unknown>, field: string): string[] {
  const value = object[field];
  if (!Array.isArray(value)) {
    throw new ValidationError(`\`${field}\` must be an array`);
  }
  return value.map((entry, index) => {
    if (typeof entry !== "string") {
      throw new ValidationError(`\`${field}[${index}]\` must be a string`);
    }
    return entry;
  });
}

export function requireEnum<T extends string>(
  object: Record<string, unknown>,
  field: string,
  allowed: readonly T[],
): T {
  const value = requireNonEmptyString(object, field);
  if (!(allowed as readonly string[]).includes(value)) {
    throw new ValidationError(
      `\`${field}\` must be one of ${allowed.map((a) => `"${a}"`).join(", ")}`,
    );
  }
  return value as T;
}

export function requireNonNegativeInteger(
  object: Record<string, unknown>,
  field: string,
): number {
  const value = object[field];
  if (typeof value !== "number" || !Number.isInteger(value) || value < 0) {
    throw new ValidationError(`\`${field}\` must be a non-negative integer`);
  }
  return value;
}

export function requirePositiveInteger(object: Record<string, unknown>, field: string): number {
  const value = object[field];
  if (typeof value !== "number" || !Number.isInteger(value) || value <= 0) {
    throw new ValidationError(`\`${field}\` must be a positive integer`);
  }
  return value;
}

export function requireNumber(object: Record<string, unknown>, field: string): number {
  const value = object[field];
  if (typeof value !== "number" || !Number.isFinite(value)) {
    throw new ValidationError(`\`${field}\` must be a finite number`);
  }
  return value;
}

/** Frozen value sets shared with the Rust domain types. */
export const CONTEXT_KINDS = [
  "symbol_definition",
  "symbol_reference",
  "source_skeleton",
  "project_fact",
  "project_lesson",
  "practice",
] as const;

export const SEVERITIES = ["info", "low", "medium", "high", "blocker"] as const;

export const IMPACT_CLASSES = [
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
] as const;

export const EVIDENCE_KINDS = ["source", "diff", "plan", "project_fact"] as const;

export const THINKING_LEVELS = ["off", "low", "medium", "high"] as const;

export const REVIEW_KINDS = ["plan", "code"] as const;

/** The read-only reviewer tools (plan section 22). */
export const REVIEWER_TOOLS = [
  "project_lookup",
  "symbol_lookup",
  "source_lookup",
  "relation_lookup",
] as const;

export type ReviewerTool = (typeof REVIEWER_TOOLS)[number];

export interface ContextEntry {
  readonly kind: (typeof CONTEXT_KINDS)[number];
  readonly title: string;
  readonly path?: string;
  readonly symbol?: string;
  readonly line_start?: number;
  readonly line_end?: number;
  readonly record_id?: string;
  readonly practice_id?: string;
  readonly text: string;
  readonly score: number;
}

export interface ContextBundle {
  readonly entries: ContextEntry[];
  readonly truncated: boolean;
  readonly text_bytes: number;
}

export interface EvidenceRef {
  readonly kind: (typeof EVIDENCE_KINDS)[number];
  readonly path?: string;
  readonly symbol?: string;
  readonly line_start?: number;
  readonly line_end?: number;
  readonly content_key?: string;
  readonly plan_section?: string;
  readonly record_id?: string;
  readonly excerpt: string;
}

export interface ReviewerFinding {
  readonly id: string;
  readonly impact_class: (typeof IMPACT_CLASSES)[number];
  readonly proposed_severity: (typeof SEVERITIES)[number];
  readonly category: string;
  readonly title: string;
  readonly claim: string;
  readonly explanation: string;
  readonly recommendation: string;
  readonly evidence: EvidenceRef[];
}

export interface CandidateProjectFact {
  readonly subject: string;
  readonly claim: string;
  readonly tags: string[];
  readonly evidence: EvidenceRef[];
  readonly confidence: number;
}

export interface ReviewerReport {
  readonly reviewer_id: string;
  readonly summary: string;
  readonly findings: ReviewerFinding[];
  readonly candidate_project_facts: CandidateProjectFact[];
  readonly model_metadata: unknown;
  readonly duration_ms: number;
}

export interface ReviewerExecutionSpec {
  readonly reviewer_id: string;
  readonly specialization: string;
  readonly instructions: string;
  readonly checklist: string[];
  readonly model_profile: string;
  readonly thinking_level: (typeof THINKING_LEVELS)[number];
  readonly initial_context: ContextEntry[];
  readonly allowed_capabilities: ReviewerTool[];
  readonly max_lookup_calls: number;
  readonly max_returned_context_bytes: number;
  readonly timeout_ms: number;
}

export interface StartReview {
  readonly review_id: string;
  readonly kind: (typeof REVIEW_KINDS)[number];
  readonly reviewers: ReviewerExecutionSpec[];
}

/** A model profile entry from the Rust core's `host_config` message. */
export interface HostModelProfilePayload {
  readonly model: string;
  readonly endpoint?: string;
}

/** The `host_config` payload: resolved model profiles by name. */
export interface HostConfig {
  readonly models: ReadonlyMap<string, HostModelProfilePayload>;
}

export function validateContextEntry(value: unknown): ContextEntry {
  const object = requireObject(value, "context entry");
  const kind = requireEnum(object, "kind", CONTEXT_KINDS);
  const title = requireNonEmptyString(object, "title");
  const text = requireString(object, "text");
  const score = requireNumber(object, "score");
  const path = requireOptionalString(object, "path");
  const symbol = requireOptionalString(object, "symbol");
  const recordId = requireOptionalString(object, "record_id");
  const practiceId = requireOptionalString(object, "practice_id");
  const lineStart = optionalField(object, "line_start");
  const lineEnd = optionalField(object, "line_end");
  if (
    lineStart !== undefined &&
    (typeof lineStart !== "number" || !Number.isInteger(lineStart) || lineStart < 1)
  ) {
    throw new ValidationError("`line_start` must be a positive integer when present");
  }
  if (
    lineEnd !== undefined &&
    (typeof lineEnd !== "number" || !Number.isInteger(lineEnd) || lineEnd < 1)
  ) {
    throw new ValidationError("`line_end` must be a positive integer when present");
  }
  const entry: ContextEntry = {
    kind,
    title,
    text,
    score,
    ...(path !== undefined ? { path } : {}),
    ...(symbol !== undefined ? { symbol } : {}),
    ...(lineStart !== undefined ? { line_start: lineStart } : {}),
    ...(lineEnd !== undefined ? { line_end: lineEnd } : {}),
    ...(recordId !== undefined ? { record_id: recordId } : {}),
    ...(practiceId !== undefined ? { practice_id: practiceId } : {}),
  };
  return entry;
}

export function validateContextBundle(value: unknown): ContextBundle {
  const object = requireObject(value, "context bundle");
  const entries = object["entries"];
  if (!Array.isArray(entries)) {
    throw new ValidationError("`entries` must be an array");
  }
  const truncated = object["truncated"];
  if (typeof truncated !== "boolean") {
    throw new ValidationError("`truncated` must be a boolean");
  }
  const textBytes = requireNonNegativeInteger(object, "text_bytes");
  return {
    entries: entries.map(validateContextEntry),
    truncated,
    text_bytes: textBytes,
  };
}

export function validateEvidenceRef(value: unknown): EvidenceRef {
  const object = requireObject(value, "evidence reference");
  const kind = requireEnum(object, "kind", EVIDENCE_KINDS);
  const excerpt = requireString(object, "excerpt");
  const optional = {
    path: requireOptionalString(object, "path"),
    symbol: requireOptionalString(object, "symbol"),
    content_key: requireOptionalString(object, "content_key"),
    plan_section: requireOptionalString(object, "plan_section"),
    record_id: requireOptionalString(object, "record_id"),
  };
  const lineStart = optionalField(object, "line_start");
  const lineEnd = optionalField(object, "line_end");
  if (
    lineStart !== undefined &&
    (typeof lineStart !== "number" || !Number.isInteger(lineStart) || lineStart < 1)
  ) {
    throw new ValidationError("`line_start` must be a positive integer when present");
  }
  if (
    lineEnd !== undefined &&
    (typeof lineEnd !== "number" || !Number.isInteger(lineEnd) || lineEnd < 1)
  ) {
    throw new ValidationError("`line_end` must be a positive integer when present");
  }
  const evidence: EvidenceRef = {
    kind,
    excerpt,
    ...(optional.path !== undefined ? { path: optional.path } : {}),
    ...(optional.symbol !== undefined ? { symbol: optional.symbol } : {}),
    ...(optional.content_key !== undefined ? { content_key: optional.content_key } : {}),
    ...(optional.plan_section !== undefined ? { plan_section: optional.plan_section } : {}),
    ...(optional.record_id !== undefined ? { record_id: optional.record_id } : {}),
    ...(lineStart !== undefined ? { line_start: lineStart } : {}),
    ...(lineEnd !== undefined ? { line_end: lineEnd } : {}),
  };
  return evidence;
}

export function validateReviewerFinding(value: unknown): ReviewerFinding {
  const object = requireObject(value, "reviewer finding");
  const evidence = object["evidence"];
  if (!Array.isArray(evidence)) {
    throw new ValidationError("`evidence` must be an array");
  }
  return {
    id: requireNonEmptyString(object, "id"),
    impact_class: requireEnum(object, "impact_class", IMPACT_CLASSES),
    proposed_severity: requireEnum(object, "proposed_severity", SEVERITIES),
    category: requireNonEmptyString(object, "category"),
    title: requireNonEmptyString(object, "title"),
    claim: requireNonEmptyString(object, "claim"),
    explanation: requireString(object, "explanation"),
    recommendation: requireString(object, "recommendation"),
    evidence: evidence.map(validateEvidenceRef),
  };
}

export function validateCandidateProjectFact(value: unknown): CandidateProjectFact {
  const object = requireObject(value, "candidate project fact");
  const evidence = object["evidence"];
  if (!Array.isArray(evidence)) {
    throw new ValidationError("`evidence` must be an array");
  }
  return {
    subject: requireNonEmptyString(object, "subject"),
    claim: requireNonEmptyString(object, "claim"),
    tags: requireStringArray(object, "tags"),
    evidence: evidence.map(validateEvidenceRef),
    confidence: requireNumber(object, "confidence"),
  };
}

export function validateReviewerReport(value: unknown): ReviewerReport {
  const object = requireObject(value, "reviewer report");
  const findings = object["findings"];
  if (!Array.isArray(findings)) {
    throw new ValidationError("`findings` must be an array");
  }
  const facts = object["candidate_project_facts"];
  if (!Array.isArray(facts)) {
    throw new ValidationError("`candidate_project_facts` must be an array");
  }
  return {
    reviewer_id: requireNonEmptyString(object, "reviewer_id"),
    summary: requireString(object, "summary"),
    findings: findings.map(validateReviewerFinding),
    candidate_project_facts: facts.map(validateCandidateProjectFact),
    model_metadata: object["model_metadata"] ?? null,
    duration_ms: requireNonNegativeInteger(object, "duration_ms"),
  };
}

export function validateReviewerExecutionSpec(value: unknown): ReviewerExecutionSpec {
  const object = requireObject(value, "reviewer execution spec");
  const context = object["initial_context"];
  if (!Array.isArray(context)) {
    throw new ValidationError("`initial_context` must be an array");
  }
  const capabilities = requireStringArray(object, "allowed_capabilities");
  for (const capability of capabilities) {
    if (!(REVIEWER_TOOLS as readonly string[]).includes(capability)) {
      throw new ValidationError(
        `\`allowed_capabilities\` contains unknown capability \`${capability}\``,
      );
    }
  }
  return {
    reviewer_id: requireNonEmptyString(object, "reviewer_id"),
    specialization: requireString(object, "specialization"),
    instructions: requireString(object, "instructions"),
    checklist: requireStringArray(object, "checklist"),
    model_profile: requireNonEmptyString(object, "model_profile"),
    thinking_level: requireEnum(object, "thinking_level", THINKING_LEVELS),
    initial_context: context.map(validateContextEntry),
    allowed_capabilities: capabilities as ReviewerTool[],
    max_lookup_calls: requireNonNegativeInteger(object, "max_lookup_calls"),
    max_returned_context_bytes: requireNonNegativeInteger(
      object,
      "max_returned_context_bytes",
    ),
    timeout_ms: requirePositiveInteger(object, "timeout_ms"),
  };
}

export function validateStartReview(value: unknown): StartReview {
  const object = requireObject(value, "start_review payload");
  const reviewers = object["reviewers"];
  if (!Array.isArray(reviewers)) {
    throw new ValidationError("`reviewers` must be an array");
  }
  return {
    review_id: requireNonEmptyString(object, "review_id"),
    kind: requireEnum(object, "kind", REVIEW_KINDS),
    reviewers: reviewers.map(validateReviewerExecutionSpec),
  };
}

/** Validate a `host_config` payload (resolved model profiles). */
export function validateHostConfig(value: unknown): HostConfig {
  const object = requireObject(value, "host_config payload");
  const models = object["models"];
  if (!isPlainObject(models)) {
    throw new ValidationError("`models` must be a JSON object");
  }
  const table = new Map<string, HostModelProfilePayload>();
  for (const [name, entry] of Object.entries(models)) {
    if (name.length === 0) {
      throw new ValidationError("model profile names must be non-empty");
    }
    const profile = requireObject(entry, `models.${name}`);
    const model = requireNonEmptyString(profile, "model");
    const endpoint = requireOptionalString(profile, "endpoint");
    table.set(name, endpoint !== undefined ? { model, endpoint } : { model });
  }
  return { models: table };
}

export function validateCancel(value: unknown): { review_id?: string } {
  const object = requireObject(value, "cancel payload");
  const reviewId = requireOptionalString(object, "review_id");
  if (reviewId === undefined) {
    return {};
  }
  return { review_id: reviewId };
}

/** Validate the arguments of one tool invocation (plan section 22). */
export function validateToolArguments(
  tool: ReviewerTool,
  value: unknown,
): Record<string, unknown> {
  const object = requireObject(value, `\`${tool}\` arguments`);
  const requireQuery = () => requireNonEmptyString(object, "query");
  const requireSymbol = () => requireNonEmptyString(object, "symbol");
  switch (tool) {
    case "project_lookup":
      requireQuery();
      break;
    case "symbol_lookup":
    case "relation_lookup":
      requireSymbol();
      break;
    case "source_lookup": {
      requireNonEmptyString(object, "path");
      const range = object["range"];
      if (range !== undefined) {
        const rangeObject = requireObject(range, "`range`");
        const start = rangeObject["line_start"];
        const end = rangeObject["line_end"];
        if (
          typeof start !== "number" ||
          !Number.isInteger(start) ||
          start < 1 ||
          typeof end !== "number" ||
          !Number.isInteger(end) ||
          end < 1 ||
          end < start
        ) {
          throw new ValidationError(
            "`range` must provide `line_start`/`line_end` positive integers in order",
          );
        }
      }
      break;
    }
  }
  return object;
}
