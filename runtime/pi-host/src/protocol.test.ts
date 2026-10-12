/**
 * Unit tests for the host-side protocol schema and validators.
 *
 * Run with: npm test  (node --test, no network access required)
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  decodeIncomingMessage,
  encodeMessage,
  ERROR_CODES,
  HOST_TO_RUST_TYPES,
  ProtocolError,
  PROTOCOL_VERSION,
  RUST_TO_HOST_TYPES,
} from "./protocol.ts";
import {
  validateContextEntry,
  validateReviewerExecutionSpec,
  validateStartReview,
  validateToolArguments,
  ValidationError,
} from "./structured.ts";

test("protocol version is frozen at 2", () => {
  assert.equal(PROTOCOL_VERSION, 2);
});

test("vocabularies match the frozen plan section 21", () => {
  assert.deepEqual(RUST_TO_HOST_TYPES, [
    "host_config",
    "start_review",
    "start_investigation",
    "start_fact_verification",
    "start_fact_synthesis",
    "start_domain_description",
    "start_finding_equivalence_check",
    "tool_response",
    "cancel",
  ]);
  assert.deepEqual(HOST_TO_RUST_TYPES, [
    "host_ready",
    "model_capabilities",
    "reviewer_started",
    "reviewer_completed",
    "reviewer_failed",
    "tool_request",
    "fact_verification_completed",
    "fact_synthesis_completed",
    "domain_description_completed",
    "finding_equivalence_completed",
    "investigation_reviewer_completed",
    "host_error",
  ]);
});

test("decode accepts a well-formed envelope and separates payload fields", () => {
  const message = decodeIncomingMessage(
    '{"protocol_version":2,"id":"m-1","type":"cancel","review_id":"01J8Z"}',
  );
  assert.equal(message.id, "m-1");
  assert.equal(message.type, "cancel");
  assert.deepEqual(message.payload, { review_id: "01J8Z" });
});

test("decode rejects malformed lines with stable codes", () => {
  const cases: Array<[string, string]> = [
    ["", ERROR_CODES.malformedMessage],
    ["not json", ERROR_CODES.malformedMessage],
    ["[1,2]", ERROR_CODES.malformedMessage],
    [
      '{"protocol_version":3,"id":"m-1","type":"cancel"}',
      ERROR_CODES.unsupportedProtocolVersion,
    ],
    ['{"protocol_version":2,"type":"cancel"}', ERROR_CODES.malformedMessage],
    ['{"protocol_version":2,"id":"","type":"cancel"}', ERROR_CODES.malformedMessage],
    ['{"protocol_version":2,"id":"m-1","type":"banana"}', ERROR_CODES.unknownMessage],
  ];
  for (const [line, code] of cases) {
    assert.throws(() => decodeIncomingMessage(line), (err: unknown) => {
      assert.ok(err instanceof ProtocolError);
      assert.equal(err.code, code);
      return true;
    }, `expected ${code} for ${line}`);
  }
});

test("encode produces single-line envelopes with the frozen fields", () => {
  const line = encodeMessage("host_ready", "h-1", { runtime: "fake" });
  assert.ok(!line.includes("\n"));
  const value = JSON.parse(line) as Record<string, unknown>;
  assert.equal(value["protocol_version"], 2);
  assert.equal(value["id"], "h-1");
  assert.equal(value["type"], "host_ready");
  assert.equal(value["runtime"], "fake");
});

function sampleSpec(): Record<string, unknown> {
  return {
    reviewer_id: "quality",
    specialization: "general",
    instructions: "work the checklist",
    checklist: ["check error paths"],
    model_profile: "standard",
    thinking_level: "medium",
    initial_context: [
      {
        kind: "symbol_definition",
        title: "function enqueue",
        path: "src/queue.rs",
        symbol: "enqueue",
        line_start: 42,
        line_end: 58,
        text: "fn enqueue(&mut self, item: T)",
        score: 1.0,
      },
    ],
    allowed_capabilities: ["project_lookup"],
    max_lookup_calls: 8,
    max_returned_context_bytes: 65536,
    timeout_ms: 300000,
  };
}

test("reviewer execution specs validate and reject unknown capabilities", () => {
  const spec = validateReviewerExecutionSpec(sampleSpec());
  assert.equal(spec.reviewer_id, "quality");
  assert.equal(spec.initial_context.length, 1);
  assert.equal(spec.allowed_capabilities[0], "project_lookup");

  const bad = { ...sampleSpec(), allowed_capabilities: ["shell"] };
  assert.throws(() => validateReviewerExecutionSpec(bad), ValidationError);

  const missing = sampleSpec() as Record<string, unknown>;
  delete missing["timeout_ms"];
  assert.throws(() => validateReviewerExecutionSpec(missing), ValidationError);

  const badThinking = { ...sampleSpec(), thinking_level: "extreme" };
  assert.throws(() => validateReviewerExecutionSpec(badThinking), ValidationError);
});

test("start_review validation requires well-formed reviews", () => {
  const payload = {
    review_id: "01J8Z",
    kind: "plan",
    reviewers: [sampleSpec()],
  };
  const review = validateStartReview(payload);
  assert.equal(review.kind, "plan");
  assert.equal(review.reviewers.length, 1);

  assert.throws(() => validateStartReview({ ...payload, kind: "build" }), ValidationError);
  assert.throws(() => validateStartReview({ ...payload, review_id: "" }), ValidationError);
  assert.throws(() => validateStartReview({ ...payload, reviewers: {} }), ValidationError);
});

test("context entries validate strictly", () => {
  const entry = validateContextEntry({
    kind: "practice",
    title: "locks",
    text: "prefer message queues",
    score: 0.5,
  });
  assert.equal(entry.path, undefined);

  // Raw snapshot source served by `source_lookup` is a valid entry kind.
  const source = validateContextEntry({
    kind: "source",
    title: "src/queue.rs",
    path: "src/queue.rs",
    line_start: 1,
    line_end: 3,
    text: "fn enqueue",
    score: 1,
  });
  assert.equal(source.path, "src/queue.rs");

  // The unified diff of a changed path is the primary input of a code
  // review and a valid entry kind.
  const diff = validateContextEntry({
    kind: "diff",
    title: "diff: src/queue.rs (modified)",
    path: "src/queue.rs",
    text: "--- a/src/queue.rs\n+++ b/src/queue.rs",
    score: 1,
  });
  assert.equal(diff.path, "src/queue.rs");

  assert.throws(
    () => validateContextEntry({ kind: "embedding", title: "x", text: "y", score: 1 }),
    ValidationError,
  );
  assert.throws(
    () => validateContextEntry({ kind: "practice", title: "x", text: "y", score: "high" }),
    ValidationError,
  );
});

test("tool arguments follow the frozen per-tool schemas", () => {
  validateToolArguments("project_lookup", { query: "ring buffer" });
  validateToolArguments("symbol_lookup", { symbol: "enqueue" });
  validateToolArguments("relation_lookup", { symbol: "enqueue" });
  validateToolArguments("source_lookup", {
    path: "src/queue.rs",
    range: { line_start: 1, line_end: 9 },
  });

  assert.throws(() => validateToolArguments("project_lookup", {}), ValidationError);
  assert.throws(() => validateToolArguments("symbol_lookup", { query: "x" }), ValidationError);
  assert.throws(
    () => validateToolArguments("source_lookup", { path: "a.rs", range: { line_start: 9, line_end: 1 } }),
    ValidationError,
  );
});
