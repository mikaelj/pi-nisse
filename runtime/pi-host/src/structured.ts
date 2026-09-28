/**
 * Strict structured-output validation (plan section 35).
 *
 * Both directions of the host boundary are validated against explicit
 * schemas before use: inbound payloads from the Rust core and outbound
 * reviewer reports to the Rust core. Malformed data is rejected with a
 * clear reason; it never flows onward as guessed state.
 */

import { MAX_CONCURRENT_REVIEWERS_LIMIT } from "./limiter.ts";
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
  "source",
  "plan",
  "diff",
  "project_fact",
  "project_lesson",
  "practice",
  "finding_group",
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

export const EVIDENCE_KINDS = [
  "source",
  "diff",
  "plan",
  "project_fact",
  "investigation",
] as const;

/** Which investigation input an `investigation` evidence ref points at. */
export const INVESTIGATION_INPUT_PARTS = ["task", "result"] as const;

/** Investigation claim verdicts (PLAN_INVESTIGATION.md). */
export const INVESTIGATION_VERDICTS = [
  "supported",
  "partially_supported",
  "contradicted",
  "insufficiently_supported",
  "not_applicable",
  "outside_scope",
] as const;

/** Investigation confidence tiers, in ascending support. */
export const INVESTIGATION_CONFIDENCE_TIERS = [
  "unsupported",
  "merely_possible",
  "plausible",
  "strongly_supported",
  "demonstrated",
] as const;

/** Coverage-gap importance levels. */
export const COVERAGE_IMPORTANCES = ["high", "medium", "low"] as const;

/** Minimum normalized length of an input-span evidence excerpt. */
export const MIN_INPUT_SPAN_EXCERPT_CHARS = 24;

export const THINKING_LEVELS = ["off", "low", "medium", "high"] as const;

export const REVIEW_KINDS = ["plan", "code"] as const;

/** The read-only reviewer tools (plan section 22). */
export const REVIEWER_TOOLS = [
  "project_lookup",
  "symbol_lookup",
  "source_lookup",
  "relation_lookup",
] as const;

/** Verdicts of the narrow finding-equivalence check (plan section 38). */
export const EQUIVALENCE_VERDICTS = ["same", "different", "uncertain"] as const;

export type EquivalenceVerdict = (typeof EQUIVALENCE_VERDICTS)[number];

/** Verdicts of independent fact verification (plan section 47). */
export const FACT_VERIFICATION_VERDICTS = [
  "supported",
  "contradicted",
  "insufficient",
] as const;

export type FactVerificationVerdict = (typeof FACT_VERIFICATION_VERDICTS)[number];

/** Maximum evidence entries of one `start_fact_verification` payload. */
export const MAX_FACT_EVIDENCE_ENTRIES = 16;

/** Maximum fact entries of one `start_fact_synthesis` payload (plan
 * section 54 step 5). */
export const MAX_FACT_SYNTHESIS_FACTS = 16;

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
  /** Which investigation input (investigation evidence only). */
  readonly input_part?: (typeof INVESTIGATION_INPUT_PARTS)[number];
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

export interface InvestigationClaimAssessment {
  readonly claim: string;
  readonly verdict: (typeof INVESTIGATION_VERDICTS)[number];
  readonly confidence: (typeof INVESTIGATION_CONFIDENCE_TIERS)[number];
  readonly overstated?: string;
  readonly task_relevance: string;
  readonly evidence: EvidenceRef[];
  readonly reasoning: string;
}

export interface InvestigationCoverageGap {
  readonly area: string;
  readonly why_expected: string;
  readonly importance: (typeof COVERAGE_IMPORTANCES)[number];
  readonly evidence: EvidenceRef[];
}

/**
 * Per-reviewer investigation report. Reviewer identity and metadata are
 * stamped by the host after validation; the model never supplies them.
 */
export interface InvestigationReport {
  /** Identity of the reviewer that produced this report (host-stamped). */
  readonly reviewer_id: string;
  readonly summary: string;
  readonly claim_assessments: InvestigationClaimAssessment[];
  readonly coverage_gaps: InvestigationCoverageGap[];
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

/** One finding of an equivalence check, with reviewer provenance. */
export interface EquivalenceFinding {
  readonly reviewer_id: string;
  readonly finding: ReviewerFinding;
}

/** The `start_finding_equivalence_check` payload (plan section 38). */
export interface FindingEquivalenceCheck {
  readonly check_id: string;
  readonly timeout_ms: number;
  readonly model_profile: string;
  readonly thinking_level: (typeof THINKING_LEVELS)[number];
  readonly finding_a: EquivalenceFinding;
  readonly finding_b: EquivalenceFinding;
}

/** A model profile entry from the Rust core's `host_config` message. */
export interface HostModelProfilePayload {
  readonly model: string;
  readonly endpoint?: string;
}

/** The constrained dynamic domain description (plan section 19). */
export interface DynamicDomainDescription {
  readonly domain_label: string;
  readonly focus_terms: readonly string[];
  readonly risks_to_check: readonly string[];
  readonly relevant_practice_tags: readonly string[];
}

/** Maximum entries per list of a dynamic domain description. */
export const MAX_DOMAIN_DESCRIPTION_TERMS = 16;

/** Maximum entries per request list (`descriptors`, `changed_paths`,
 * `languages`) of a `start_domain_description` payload. */
export const MAX_DOMAIN_REQUEST_TERMS = 64;

/** Maximum bytes of one list entry or of the domain label. */
const MAX_DOMAIN_DESCRIPTION_TERM_BYTES = 256;
const MAX_DOMAIN_LABEL_BYTES = 256;

/** The `start_domain_description` payload (plan section 19). */
export interface DomainDescriptionRequest {
  readonly description_id: string;
  readonly timeout_ms: number;
  readonly model_profile: string;
  readonly thinking_level: (typeof THINKING_LEVELS)[number];
  readonly descriptors: readonly string[];
  readonly changed_paths: readonly string[];
  readonly languages: readonly string[];
}

/** The `host_config` payload: resolved model profiles by name, plus the
 * optional reviewer-concurrency bound (plan milestone 29: reviewer
 * concurrency obeys config). */
export interface HostConfig {
  readonly models: ReadonlyMap<string, HostModelProfilePayload>;
  readonly maxConcurrentReviewers: number | undefined;
}

/** The `start_fact_verification` payload (plan section 47 step 2):
 * one exact claim plus the exact evidence excerpts a fresh verifier
 * session must judge. */
export interface FactVerificationRequest {
  readonly verification_id: string;
  readonly timeout_ms: number;
  readonly model_profile: string;
  readonly thinking_level: (typeof THINKING_LEVELS)[number];
  readonly claim: string;
  readonly evidence: readonly EvidenceRef[];
}

/** Validate a `start_fact_verification` payload. */
export function validateFactVerificationRequest(value: unknown): FactVerificationRequest {
  const object = requireObject(value, "start_fact_verification payload");
  const evidence = object["evidence"];
  if (!Array.isArray(evidence)) {
    throw new ValidationError("`evidence` must be an array");
  }
  if (evidence.length === 0) {
    throw new ValidationError("`evidence` must not be empty");
  }
  if (evidence.length > MAX_FACT_EVIDENCE_ENTRIES) {
    throw new ValidationError(`\`evidence\` exceeds ${MAX_FACT_EVIDENCE_ENTRIES} entries`);
  }
  return {
    verification_id: requireNonEmptyString(object, "verification_id"),
    timeout_ms: requirePositiveInteger(object, "timeout_ms"),
    model_profile: requireNonEmptyString(object, "model_profile"),
    thinking_level: requireEnum(object, "thinking_level", THINKING_LEVELS),
    claim: requireNonEmptyString(object, "claim"),
    evidence: evidence.map(validateEvidenceRef),
  };
}

/** One recorded fact of a `start_fact_synthesis` payload (plan section
 * 54 step 5): its exact statement plus the evidence excerpts backing
 * it. */
export interface FactSynthesisInput {
  readonly text: string;
  readonly evidence: readonly EvidenceRef[];
}

/** The `start_fact_synthesis` payload: the subject and the recorded
 * facts one fresh synthesis session must try to merge into the
 * narrowest statement supported by the supplied facts and their
 * evidence. */
export interface FactSynthesisRequest {
  readonly synthesis_id: string;
  readonly timeout_ms: number;
  readonly model_profile: string;
  readonly thinking_level: (typeof THINKING_LEVELS)[number];
  readonly subject: string;
  readonly facts: readonly FactSynthesisInput[];
}

/** Validate one `facts[]` entry of a synthesis request. */
function validateFactSynthesisInput(value: unknown): FactSynthesisInput {
  const object = requireObject(value, "facts[] entry");
  const evidence = object["evidence"];
  if (!Array.isArray(evidence)) {
    throw new ValidationError("`facts[].evidence` must be an array");
  }
  if (evidence.length === 0) {
    throw new ValidationError("`facts[].evidence` must not be empty");
  }
  if (evidence.length > MAX_FACT_EVIDENCE_ENTRIES) {
    throw new ValidationError(
      `\`facts[].evidence\` exceeds ${MAX_FACT_EVIDENCE_ENTRIES} entries`,
    );
  }
  return {
    text: requireNonEmptyString(object, "text"),
    evidence: evidence.map(validateEvidenceRef),
  };
}

/** Validate a `start_fact_synthesis` payload. */
export function validateFactSynthesisRequest(value: unknown): FactSynthesisRequest {
  const object = requireObject(value, "start_fact_synthesis payload");
  const facts = object["facts"];
  if (!Array.isArray(facts)) {
    throw new ValidationError("`facts` must be an array");
  }
  if (facts.length === 0) {
    throw new ValidationError("`facts` must not be empty");
  }
  if (facts.length > MAX_FACT_SYNTHESIS_FACTS) {
    throw new ValidationError(`\`facts\` exceeds ${MAX_FACT_SYNTHESIS_FACTS} entries`);
  }
  return {
    synthesis_id: requireNonEmptyString(object, "synthesis_id"),
    timeout_ms: requirePositiveInteger(object, "timeout_ms"),
    model_profile: requireNonEmptyString(object, "model_profile"),
    thinking_level: requireEnum(object, "thinking_level", THINKING_LEVELS),
    subject: requireNonEmptyString(object, "subject"),
    facts: facts.map(validateFactSynthesisInput),
  };
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
  const inputPart =
    object["input_part"] === undefined
      ? undefined
      : requireEnum(object, "input_part", INVESTIGATION_INPUT_PARTS);
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
    ...(inputPart !== undefined ? { input_part: inputPart } : {}),
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

/** Validate one investigation evidence ref (input-span discipline). */
export function validateInvestigationEvidence(value: unknown): EvidenceRef {
  const evidence = validateEvidenceRef(value);
  if (evidence.kind !== "investigation") {
    throw new ValidationError(
      "investigation evidence must have kind `investigation`",
    );
  }
  if (evidence.input_part === undefined) {
    throw new ValidationError("investigation evidence requires `input_part`");
  }
  if (evidence.excerpt.trim().length < MIN_INPUT_SPAN_EXCERPT_CHARS) {
    throw new ValidationError(
      `investigation evidence excerpt is shorter than ${MIN_INPUT_SPAN_EXCERPT_CHARS} characters`,
    );
  }
  return evidence;
}

/** Validate one investigation claim assessment. */
export function validateInvestigationClaimAssessment(
  value: unknown,
): InvestigationClaimAssessment {
  const object = requireObject(value, "claim assessment");
  const evidence = object["evidence"];
  if (!Array.isArray(evidence)) {
    throw new ValidationError("`evidence` must be an array");
  }
  const assessment: InvestigationClaimAssessment = {
    claim: requireNonEmptyString(object, "claim"),
    verdict: requireEnum(object, "verdict", INVESTIGATION_VERDICTS),
    confidence: requireEnum(object, "confidence", INVESTIGATION_CONFIDENCE_TIERS),
    task_relevance: requireNonEmptyString(object, "task_relevance"),
    reasoning: requireString(object, "reasoning"),
    evidence: evidence.map(validateInvestigationEvidence),
    ...(object["overstated"] !== undefined
      ? { overstated: requireNonEmptyString(object, "overstated") }
      : {}),
  };
  return assessment;
}

/** Validate one investigation coverage gap. */
export function validateInvestigationCoverageGap(
  value: unknown,
): InvestigationCoverageGap {
  const object = requireObject(value, "coverage gap");
  const evidence = object["evidence"];
  if (!Array.isArray(evidence)) {
    throw new ValidationError("`evidence` must be an array");
  }
  return {
    area: requireNonEmptyString(object, "area"),
    why_expected: requireNonEmptyString(object, "why_expected"),
    importance: requireEnum(object, "importance", COVERAGE_IMPORTANCES),
    evidence: evidence.map(validateInvestigationEvidence),
  };
}

/** Validate one investigation report (strict; one-repair policy upstream). */
export function validateInvestigationReport(
  value: unknown,
): Omit<InvestigationReport, "reviewer_id"> {
  const object = requireObject(value, "investigation report");
  const claims = object["claim_assessments"];
  if (!Array.isArray(claims)) {
    throw new ValidationError("`claim_assessments` must be an array");
  }
  const gaps = object["coverage_gaps"];
  if (!Array.isArray(gaps)) {
    throw new ValidationError("`coverage_gaps` must be an array");
  }
  const facts = object["candidate_project_facts"];
  if (!Array.isArray(facts)) {
    throw new ValidationError("`candidate_project_facts` must be an array");
  }
  return {
    summary: requireNonEmptyString(object, "summary"),
    claim_assessments: claims.map(validateInvestigationClaimAssessment),
    coverage_gaps: gaps.map(validateInvestigationCoverageGap),
    candidate_project_facts: facts.map(validateCandidateProjectFact),
    model_metadata: {},
    duration_ms: 0,
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

export interface StartInvestigation {
  readonly review_id: string;
  readonly task: string;
  readonly result: string;
  readonly reviewers: ReviewerExecutionSpec[];
}

/** Validate one `start_investigation` payload (PLAN_INVESTIGATION.md). */
export function validateStartInvestigation(value: unknown): StartInvestigation {
  const object = requireObject(value, "start investigation payload");
  const reviewers = object["reviewers"];
  if (!Array.isArray(reviewers)) {
    throw new ValidationError("`reviewers` must be an array");
  }
  if (reviewers.length === 0) {
    throw new ValidationError("`reviewers` must not be empty");
  }
  return {
    review_id: requireNonEmptyString(object, "review_id"),
    task: requireNonEmptyString(object, "task"),
    result: requireNonEmptyString(object, "result"),
    reviewers: reviewers.map(validateReviewerExecutionSpec),
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

/** Validate one finding of an equivalence check. */
export function validateEquivalenceFinding(value: unknown): EquivalenceFinding {
  const object = requireObject(value, "equivalence finding");
  const finding = requireObject(object["finding"], "equivalence finding `finding`");
  return {
    reviewer_id: requireNonEmptyString(object, "reviewer_id"),
    finding: validateReviewerFinding(finding),
  };
}

/** Validate a `start_finding_equivalence_check` payload. */
export function validateFindingEquivalenceCheck(value: unknown): FindingEquivalenceCheck {
  const object = requireObject(value, "start_finding_equivalence_check payload");
  const findingA = requireObject(object["finding_a"], "`finding_a`");
  const findingB = requireObject(object["finding_b"], "`finding_b`");
  return {
    check_id: requireNonEmptyString(object, "check_id"),
    timeout_ms: requirePositiveInteger(object, "timeout_ms"),
    model_profile: requireNonEmptyString(object, "model_profile"),
    thinking_level: requireEnum(object, "thinking_level", THINKING_LEVELS),
    finding_a: validateEquivalenceFinding(findingA),
    finding_b: validateEquivalenceFinding(findingB),
  };
}

function requireBoundedTermList(
  object: Record<string, unknown>,
  field: string,
  maxEntries: number,
): string[] {
  const list = requireStringArray(object, field);
  if (list.length > maxEntries) {
    throw new ValidationError(`\`${field}\` exceeds ${maxEntries} entries`);
  }
  for (const entry of list) {
    if (entry.length === 0 || entry.length > MAX_DOMAIN_DESCRIPTION_TERM_BYTES) {
      throw new ValidationError(
        `\`${field}\` entries must be non-empty and at most ` +
          `${MAX_DOMAIN_DESCRIPTION_TERM_BYTES} bytes`,
      );
    }
  }
  return list;
}

/** Validate a `start_domain_description` payload (plan section 19). */
export function validateDomainDescriptionRequest(value: unknown): DomainDescriptionRequest {
  const object = requireObject(value, "start_domain_description payload");
  return {
    description_id: requireNonEmptyString(object, "description_id"),
    timeout_ms: requirePositiveInteger(object, "timeout_ms"),
    model_profile: requireNonEmptyString(object, "model_profile"),
    thinking_level: requireEnum(object, "thinking_level", THINKING_LEVELS),
    descriptors: requireBoundedTermList(object, "descriptors", MAX_DOMAIN_REQUEST_TERMS),
    changed_paths: requireBoundedTermList(object, "changed_paths", MAX_DOMAIN_REQUEST_TERMS),
    languages: requireBoundedTermList(object, "languages", MAX_DOMAIN_REQUEST_TERMS),
  };
}

/** Validate a session answer as a strict dynamic domain description. */
export function validateDynamicDomainDescription(value: unknown): DynamicDomainDescription {
  const object = requireObject(value, "dynamic domain description");
  const label = requireNonEmptyString(object, "domain_label");
  if (label.length > MAX_DOMAIN_LABEL_BYTES) {
    throw new ValidationError(
      `\`domain_label\` must be at most ${MAX_DOMAIN_LABEL_BYTES} bytes`,
    );
  }
  return {
    domain_label: label,
    focus_terms: requireBoundedTermList(object, "focus_terms", MAX_DOMAIN_DESCRIPTION_TERMS),
    risks_to_check: requireBoundedTermList(
      object,
      "risks_to_check",
      MAX_DOMAIN_DESCRIPTION_TERMS,
    ),
    relevant_practice_tags: requireBoundedTermList(
      object,
      "relevant_practice_tags",
      MAX_DOMAIN_DESCRIPTION_TERMS,
    ),
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
  const rawMax = object["max_concurrent_reviewers"];
  let maxConcurrentReviewers: number | undefined;
  if (rawMax !== undefined) {
    if (
      typeof rawMax !== "number" ||
      !Number.isInteger(rawMax) ||
      rawMax < 1 ||
      rawMax > MAX_CONCURRENT_REVIEWERS_LIMIT
    ) {
      throw new ValidationError(
        "`max_concurrent_reviewers` must be an integer between 1 and " +
          `${MAX_CONCURRENT_REVIEWERS_LIMIT}`,
      );
    }
    maxConcurrentReviewers = rawMax;
  }
  return { models: table, maxConcurrentReviewers };
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
