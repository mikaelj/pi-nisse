/**
 * Unit tests for Pi reviewer session execution (plan milestone 12).
 *
 * Covers the spec-to-session mapping, strict report parsing, the single
 * constrained repair attempt, timeouts, and the scripted mock runtime.
 * No model provider is contacted: agent sessions are scripted.
 */

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { createScriptedAgentFactory, parseSessionScript } from "./scripted.ts";
import {
  buildFirstMessage,
  buildRepairPrompt,
  buildReviewerTools,
  buildSystemPrompt,
  extractReportJson,
  parseReviewerReport,
  PiReviewerSession,
  ReviewerSessionError,
  SESSION_ERROR_CODES,
  SessionDeadline,
  type PiSessionDeps,
  type SessionAgentFactory,
} from "./session.ts";
import { ToolBridge } from "./tools.ts";
import type { ContextEntry, ReviewerExecutionSpec } from "./structured.ts";

function contextEntry(text: string): ContextEntry {
  return {
    kind: "symbol_definition",
    title: "function enqueue",
    path: "src/queue.rs",
    symbol: "enqueue",
    line_start: 42,
    line_end: 58,
    text,
    score: 1,
  };
}

function executionSpec(overrides: Partial<ReviewerExecutionSpec> = {}): ReviewerExecutionSpec {
  return {
    reviewer_id: "quality",
    specialization: "general correctness",
    instructions: "Work the checklist; report only evidenced findings.",
    checklist: ["Check error paths.", "Check shared state."],
    model_profile: "standard",
    thinking_level: "medium",
    initial_context: [contextEntry("fn enqueue(&mut self, item: T)")],
    allowed_capabilities: ["project_lookup", "source_lookup"],
    max_lookup_calls: 4,
    max_returned_context_bytes: 65_536,
    timeout_ms: 5_000,
    ...overrides,
  };
}

function validReportJson(): string {
  return JSON.stringify({
    reviewer_id: "whatever-the-model-says",
    summary: "One informational finding.",
    findings: [
      {
        id: "f-1",
        impact_class: "correctness",
        proposed_severity: "info",
        category: "api",
        title: "Missing error handling",
        claim: "enqueue may panic on full queue",
        explanation: "No bound check exists.",
        recommendation: "Return an error when the queue is full.",
        evidence: [{ kind: "source", path: "src/queue.rs", excerpt: "fn enqueue" }],
      },
    ],
    candidate_project_facts: [],
    model_metadata: {},
    duration_ms: 0,
  });
}

const noToolsBridge = new ToolBridge({
  nextId: () => "unused",
  send: () => {},
});

function sessionContext(): Parameters<PiReviewerSession["run"]>[0] {
  return {
    reviewId: "rev-test",
    tools: noToolsBridge,
    isCancelled: () => false,
  };
}

function deps(factory: SessionAgentFactory, models: Map<string, { model: string; endpoint?: string }>): PiSessionDeps {
  return {
    models,
    kind: "plan",
    resolveAgent: () => Promise.resolve(factory),
  };
}

const standardModel = new Map([["standard", { model: "acme/small-local" }]]);

function expectSessionError(error: unknown, code: string): ReviewerSessionError {
  assert.ok(error instanceof ReviewerSessionError, `expected ReviewerSessionError, got ${String(error)}`);
  assert.equal(error.code, code);
  return error;
}

// ---------------------------------------------------------------------------
// Prompt construction
// ---------------------------------------------------------------------------

test("system prompt carries instructions, checklist, and output contract", () => {
  const spec = executionSpec();
  const prompt = buildSystemPrompt(spec);
  assert.ok(prompt.includes("Nisse reviewer `quality`"));
  assert.ok(prompt.includes(spec.instructions));
  assert.ok(prompt.includes("1. Check error paths."));
  assert.ok(prompt.includes("2. Check shared state."));
  assert.ok(prompt.includes("exactly one JSON object"));
  assert.ok(prompt.includes('"proposed_severity"'));
  assert.ok(prompt.includes('"impact_class"'));
  assert.ok(prompt.includes('"evidence"'));
  assert.ok(prompt.includes('"candidate_project_facts"'));
  assert.ok(prompt.includes('"reviewer_id": string  // exactly "quality"'));
});

test("first message renders review kind and initial context", () => {
  const spec = executionSpec();
  const message = buildFirstMessage("code", spec);
  assert.ok(message.includes("code review"));
  assert.ok(message.includes("quality"));
  assert.ok(message.includes("symbol_definition: function enqueue"));
  assert.ok(message.includes("path: src/queue.rs"));
  assert.ok(message.includes("symbol: enqueue"));
  assert.ok(message.includes("lines: 42-58"));
  assert.ok(message.includes("fn enqueue(&mut self, item: T)"));
  assert.ok(message.trimEnd().endsWith("No other text."));
});

test("first message notes when no initial context exists", () => {
  const spec = executionSpec({ initial_context: [] });
  const message = buildFirstMessage("plan", spec);
  assert.ok(message.includes("(no initial context entries were supplied)"));
});

// ---------------------------------------------------------------------------
// Strict report parsing
// ---------------------------------------------------------------------------

test("extractReportJson accepts exact JSON and single fenced JSON", () => {
  assert.deepEqual(extractReportJson('  {"a":1}  '), { a: 1 });
  assert.deepEqual(extractReportJson("```json\n{\"a\":1}\n\n```"), { a: 1 });
  assert.deepEqual(extractReportJson("```\n{\"a\":1}\n```"), { a: 1 });
});

test("extractReportJson rejects prose, empty text, and junk", () => {
  assert.throws(() => extractReportJson(""));
  assert.throws(() => extractReportJson("Here is my report: {\"a\":1}"));
  assert.throws(() => extractReportJson("{\"a\":1} hope this helps"));
  assert.throws(() => extractReportJson("```"));
});

test("parseReviewerReport validates strictly and stamps the spec identity", () => {
  const spec = executionSpec();
  const report = parseReviewerReport(validReportJson(), spec);
  assert.equal(report.reviewer_id, "quality");
  assert.equal(report.findings.length, 1);
  assert.equal(report.findings[0]?.id, "f-1");

  const broken = JSON.parse(validReportJson()) as Record<string, unknown>;
  delete broken["findings"];
  assert.throws(() => parseReviewerReport(JSON.stringify(broken), spec));
  assert.throws(() => parseReviewerReport("not json", spec));
  const badSeverity = JSON.parse(validReportJson()) as { findings: Array<Record<string, unknown>> };
  badSeverity.findings[0]!["proposed_severity"] = "catastrophic";
  assert.throws(() => parseReviewerReport(JSON.stringify(badSeverity), spec));
});

test("repair prompt quotes the failed answer and reason", () => {
  const spec = executionSpec();
  const prompt = buildRepairPrompt("I cannot do JSON", "answer is not valid JSON: …", spec);
  assert.ok(prompt.includes("Reason: answer is not valid JSON"));
  assert.ok(prompt.includes("<previous_answer>"));
  assert.ok(prompt.includes("I cannot do JSON"));
  assert.ok(prompt.includes('"quality"'));
});

// ---------------------------------------------------------------------------
// Deadline
// ---------------------------------------------------------------------------

test("deadline expires deterministically", async () => {
  const deadline = SessionDeadline.start(5);
  assert.ok(deadline.remainingMs() > 0);
  await new Promise((resolve) => setTimeout(resolve, 15));
  assert.throws(() => deadline.remainingMs(), (err: unknown) => {
    expectSessionError(err, SESSION_ERROR_CODES.timeout);
    return true;
  });
});

// ---------------------------------------------------------------------------
// Session tools
// ---------------------------------------------------------------------------

test("session tools are filtered to granted capabilities", () => {
  const spec = executionSpec({ allowed_capabilities: ["symbol_lookup"] });
  const tools = buildReviewerTools(spec, sessionContext(), SessionDeadline.start(60_000));
  assert.deepEqual(
    tools.map((tool) => tool.name),
    ["symbol_lookup"],
  );
  assert.equal(tools[0]?.parameters.type, "object");
});

// ---------------------------------------------------------------------------
// PiReviewerSession with scripted agents
// ---------------------------------------------------------------------------

test("valid answer completes with host-stamped identity and metadata", async () => {
  const factory: SessionAgentFactory = async (config) => ({
    prompt: async (text) => {
      assert.ok(text.includes("plan review"));
      assert.ok(config.systemPrompt.includes("Nisse reviewer `quality`"));
    },
    finalText: () => validReportJson(),
    abort: () => {},
    dispose: () => {},
  });
  const session = new PiReviewerSession(executionSpec(), deps(factory, standardModel));
  const report = await session.run(sessionContext());
  assert.equal(report.reviewer_id, "quality");
  assert.ok(
    typeof report.duration_ms === "number" && report.duration_ms >= 0,
    "duration is host-measured",
  );
  const metadata = report.model_metadata as Record<string, unknown>;
  assert.equal(metadata["runtime"], "pi");
  assert.equal(metadata["model"], "acme/small-local");
  assert.equal(metadata["thinking_level"], "medium");
  assert.equal(metadata["turns"], 1);
  assert.equal(metadata["repaired"], undefined);
});

test("malformed answer is repaired exactly once and marked repaired", async () => {
  const prompts: string[] = [];
  const factory: SessionAgentFactory = async () => ({
    prompt: async (text) => {
      prompts.push(text);
    },
    finalText: () => (prompts.length === 1 ? "sorry, I cannot" : validReportJson()),
    abort: () => {},
    dispose: () => {},
  });
  const session = new PiReviewerSession(executionSpec(), deps(factory, standardModel));
  const report = await session.run(sessionContext());
  assert.equal(prompts.length, 2, "exactly one repair turn");
  assert.ok(prompts[1]?.includes("Your previous answer was not a valid review report"));
  const metadata = report.model_metadata as Record<string, unknown>;
  assert.equal(metadata["turns"], 2);
  assert.equal(metadata["repaired"], true);
});

test("malformed answer after repair fails with invalid_report", async () => {
  const factory: SessionAgentFactory = async () => ({
    prompt: async () => {},
    finalText: () => "still not json",
    abort: () => {},
    dispose: () => {},
  });
  const session = new PiReviewerSession(executionSpec(), deps(factory, standardModel));
  await assert.rejects(
    () => session.run(sessionContext()),
    (err: unknown) => {
      expectSessionError(err, SESSION_ERROR_CODES.invalidReport);
      return true;
    },
  );
});

test("missing assistant text is repaired, not fatal", async () => {
  const answers: Array<string | undefined> = [undefined, validReportJson()];
  const factory: SessionAgentFactory = async () => ({
    prompt: async () => {},
    finalText: () => answers.shift(),
    abort: () => {},
    dispose: () => {},
  });
  const session = new PiReviewerSession(executionSpec(), deps(factory, standardModel));
  const report = await session.run(sessionContext());
  assert.equal(report.reviewer_id, "quality");
  const metadata = report.model_metadata as Record<string, unknown>;
  assert.equal(metadata["repaired"], true);
});

test("hanging session times out with session_timeout", async () => {
  const factory: SessionAgentFactory = async () => ({
    prompt: async () => {
      await new Promise((resolve) => setTimeout(resolve, 10_000));
    },
    finalText: () => undefined,
    abort: () => {},
    dispose: () => {},
  });
  const spec = executionSpec({ timeout_ms: 50 });
  const session = new PiReviewerSession(spec, deps(factory, standardModel));
  await assert.rejects(
    () => session.run(sessionContext()),
    (err: unknown) => {
      expectSessionError(err, SESSION_ERROR_CODES.timeout);
      return true;
    },
  );
});

test("unresolved model profile fails closed with model_profile_unresolved", async () => {
  const factory: SessionAgentFactory = async () => {
    throw new Error("factory must not be reached");
  };
  const empty = new Map<string, { model: string; endpoint?: string }>();
  const session = new PiReviewerSession(executionSpec(), deps(factory, empty));
  await assert.rejects(
    () => session.run(sessionContext()),
    (err: unknown) => {
      expectSessionError(err, SESSION_ERROR_CODES.modelUnresolved);
      return true;
    },
  );
});

test("unavailable agent runtime fails closed with sdk_unavailable", async () => {
  const deps: PiSessionDeps = {
    models: standardModel,
    kind: "plan",
    resolveAgent: () => Promise.reject(new Error("module not found")),
  };
  const session = new PiReviewerSession(executionSpec(), deps);
  await assert.rejects(
    () => session.run(sessionContext()),
    (err: unknown) => {
      expectSessionError(err, SESSION_ERROR_CODES.sdkUnavailable);
      return true;
    },
  );
});

// ---------------------------------------------------------------------------
// Scripted mock runtime
// ---------------------------------------------------------------------------

test("session scripts parse strictly", () => {
  const script = parseSessionScript(
    JSON.stringify({ sessions: [{ steps: [{ reply: "{}" }, { error: "boom" }, { hangMs: 100 }] }] }),
  );
  assert.equal(script.sessions.length, 1);
  assert.equal(script.sessions[0]?.steps.length, 3);

  for (const bad of [
    "not json",
    "{}",
    '{"sessions":{}}',
    '{"sessions":[{"steps":[{"reply":1}]}]}',
    '{"sessions":[{"steps":[{"reply":"a","error":"b"}]}]}',
    '{"sessions":[{"steps":[{"hangMs":0}]}]}',
    '{"sessions":[{"steps":[{}]}]}',
  ]) {
    assert.throws(
      () => parseSessionScript(bad),
      (err: unknown) => err instanceof Error,
      `expected rejection for ${bad}`,
    );
  }
});

test("scripted factory records session configs and prompts", async () => {
  const dir = mkdtempSync(join(tmpdir(), "nisse-scripted-"));
  try {
    const recordPath = join(dir, "record.jsonl");
    const script = parseSessionScript(
      JSON.stringify({ sessions: [{ steps: [{ reply: validReportJson() }] }] }),
    );
    const factory = createScriptedAgentFactory(script, recordPath);
    const session = new PiReviewerSession(executionSpec(), deps(factory, standardModel));
    const report = await session.run(sessionContext());
    assert.equal(report.reviewer_id, "quality");

    const lines = readFileSync(recordPath, "utf8").trim().split("\n");
    assert.equal(lines.length, 1);
    const recorded = JSON.parse(lines[0]!) as {
      config: {
        reviewerId: string;
        model: string;
        thinkingLevel: string;
        toolNames: string[];
        isolation: Record<string, boolean>;
      };
      prompts: string[];
    };
    assert.equal(recorded.config.reviewerId, "quality");
    assert.equal(recorded.config.model, "acme/small-local");
    assert.equal(recorded.config.thinkingLevel, "medium");
    assert.deepEqual(recorded.config.toolNames, ["project_lookup", "source_lookup"]);
    assert.equal(recorded.config.isolation["inMemorySession"], true);
    assert.equal(recorded.config.isolation["noBuiltinTools"], true);
    assert.equal(recorded.config.isolation["noExtensions"], true);
    assert.equal(recorded.config.isolation["noSkills"], true);
    assert.equal(recorded.config.isolation["noPromptTemplates"], true);
    assert.equal(recorded.config.isolation["noContextFiles"], true);
    assert.equal(recorded.prompts.length, 1);
    assert.ok(recorded.prompts[0]?.includes("plan review"));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("scripted factory fails when the script is exhausted", async () => {
  const script = parseSessionScript(JSON.stringify({ sessions: [] }));
  const factory = createScriptedAgentFactory(script, undefined);
  const session = new PiReviewerSession(executionSpec(), deps(factory, standardModel));
  await assert.rejects(() => session.run(sessionContext()), /script/i);
});

test("scripted step errors fail the session (turn errors are not repairs)", async () => {
  const script = parseSessionScript(
    JSON.stringify({ sessions: [{ steps: [{ error: "model exploded" }, { reply: validReportJson() }] }] }),
  );
  const factory = createScriptedAgentFactory(script, undefined);
  const session = new PiReviewerSession(executionSpec(), deps(factory, standardModel));
  // A failing turn is a session failure, not malformed output: the only
  // repair path is for answers that parse/validate incorrectly. Rust's
  // retry policy handles failed reviewers.
  await assert.rejects(
    () => session.run(sessionContext()),
    /model exploded/,
  );
});
