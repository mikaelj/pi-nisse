/**
 * Unit tests for the investigation structured validators and session
 * prompt/parse machinery (PLAN_INVESTIGATION.md).
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  MIN_INPUT_SPAN_EXCERPT_CHARS,
  ValidationError,
  validateInvestigationClaimAssessment,
  validateInvestigationCoverageGap,
  validateInvestigationEvidence,
  validateInvestigationReport,
  validateStartInvestigation,
} from "./structured.ts";
import {
  buildInvestigationFirstMessage,
  buildInvestigationRepairPrompt,
  buildInvestigationSystemPrompt,
  parseInvestigationReport,
} from "./session.ts";

const LONG = "a".repeat(MIN_INPUT_SPAN_EXCERPT_CHARS + 4);

function validClaim() {
  return {
    claim: "the module greets users",
    verdict: "supported",
    confidence: "plausible",
    task_relevance: "the task asks about greetings",
    reasoning: "the source shows it",
    evidence: [
      {
        kind: "investigation",
        input_part: "result",
        excerpt: LONG,
      },
    ],
  };
}

function validGap() {
  return {
    area: "unicode handling",
    why_expected: "the task mentions user input",
    importance: "medium",
    evidence: [
      {
        kind: "investigation",
        input_part: "task",
        excerpt: LONG,
      },
    ],
  };
}

test("start_investigation validation requires well-formed reviewers", () => {
  const base = {
    review_id: "01J8Z",
    task: "the task",
    result: "the result",
    reviewers: [
      {
        reviewer_id: "claim-skeptic",
        specialization: "s",
        instructions: "i",
        checklist: ["c"],
        model_profile: "standard",
        thinking_level: "medium" as const,
        initial_context: [],
        allowed_capabilities: ["project_lookup"],
        max_lookup_calls: 4,
        max_returned_context_bytes: 1024,
        timeout_ms: 120000,
      },
    ],
  };
  assert.deepEqual(validateStartInvestigation(base).review_id, "01J8Z");
  assert.throws(
    () => validateStartInvestigation({ ...base, reviewers: [] }),
    ValidationError,
  );
  assert.throws(
    () => validateStartInvestigation({ ...base, task: "" }),
    ValidationError,
  );
});

test("investigation evidence enforces kind, input_part, and excerpt length", () => {
  assert.equal(validateInvestigationEvidence({ kind: "investigation", input_part: "task", excerpt: LONG }).kind, "investigation");
  assert.throws(
    () => validateInvestigationEvidence({ kind: "source", input_part: "task", excerpt: LONG }),
    ValidationError,
  );
  assert.throws(
    () => validateInvestigationEvidence({ kind: "investigation", excerpt: LONG }),
    ValidationError,
  );
  assert.throws(
    () =>
      validateInvestigationEvidence({
        kind: "investigation",
        input_part: "task",
        excerpt: "short",
      }),
    ValidationError,
  );
});

test("claim assessments reject bad verdicts and confidence tiers", () => {
  const claim = validClaim();
  assert.equal(
    validateInvestigationClaimAssessment(claim).verdict,
    "supported",
  );
  assert.throws(
    () =>
      validateInvestigationClaimAssessment({ ...claim, verdict: "fine" }),
    ValidationError,
  );
  assert.throws(
    () =>
      validateInvestigationClaimAssessment({ ...claim, confidence: "total" }),
    ValidationError,
  );
  // Empty evidence passes host validation; the Rust pipeline clamps
  // unanchored/evidence-free assessments to insufficiently_supported.
  assert.equal(
    validateInvestigationClaimAssessment({ ...claim, evidence: [] }).verdict,
    "supported",
  );
});

test("coverage gaps use strict investigation evidence", () => {
  assert.equal(validateInvestigationCoverageGap(validGap()).area, "unicode handling");
  assert.throws(
    () =>
      validateInvestigationCoverageGap({
        ...validGap(),
        evidence: [{ kind: "source", excerpt: LONG }],
      }),
    ValidationError,
  );
});

test("investigation report validation accepts and stamps nothing", () => {
  const report = {
    summary: "s",
    claim_assessments: [validClaim()],
    coverage_gaps: [validGap()],
    candidate_project_facts: [],
    model_metadata: {},
    duration_ms: 0,
  };
  const validated = validateInvestigationReport(report);
  assert.equal(validated.summary, "s");
  assert.throws(
    () => validateInvestigationReport({ ...report, summary: "" }),
    ValidationError,
  );
});

test("investigation parse stamps reviewer identity from the spec", () => {
  const spec = {
    reviewer_id: "claim-skeptic",
    specialization: "s",
    instructions: "i",
    checklist: ["c"],
    model_profile: "standard",
    thinking_level: "medium" as const,
    initial_context: [],
    allowed_capabilities: [],
    max_lookup_calls: 1,
    max_returned_context_bytes: 1,
    timeout_ms: 1000,
  };
  const report = {
    summary: "s",
    claim_assessments: [validClaim()],
    coverage_gaps: [],
    candidate_project_facts: [],
    model_metadata: {},
    duration_ms: 3,
  };
  const parsed = parseInvestigationReport(JSON.stringify(report), spec);
  assert.equal(parsed.reviewer_id, "claim-skeptic");
});

test("investigation system prompt and first message separate untrusted inputs", () => {
  const spec = {
    reviewer_id: "claim-skeptic",
    specialization: "s",
    instructions: "falsify",
    checklist: ["check evidence"],
    model_profile: "standard",
    thinking_level: "medium" as const,
    initial_context: [],
    allowed_capabilities: [],
    max_lookup_calls: 1,
    max_returned_context_bytes: 1,
    timeout_ms: 1000,
  };
  const system = buildInvestigationSystemPrompt(spec);
  assert.ok(system.includes("falsify"));
  assert.ok(system.includes("UNTRUSTED input"));
  const first = buildInvestigationFirstMessage(spec, "the task text", "the result text");
  assert.ok(first.includes("the task text"));
  assert.ok(first.includes("the result text"));
  assert.ok(first.includes("untrusted"));
  const repair = buildInvestigationRepairPrompt("bad", "not JSON", spec);
  assert.ok(repair.includes("not JSON"));
});
