/**
 * Unit tests of the fact-synthesis narrow job (plan section 54 step 5):
 * strict answer parsing, prompt shape, and the deterministic fake
 * runtime driven by the test environment variables.
 */

import assert from "node:assert/strict";
import { afterEach, describe, it } from "node:test";

import {
  buildFactSynthesisMessage,
  buildFactSynthesisRepairPrompt,
  buildFactSynthesisSystemPrompt,
  FACT_SYNTHESIS_SESSION_ID,
  MAX_CANDIDATE_BYTES,
  parseFactSynthesisAnswer,
  resetFakeFactSynthesisState,
  runFactSynthesis,
} from "./synthesis.ts";
import type { FactSynthesisRequest } from "./structured.ts";

function request(overrides: Partial<FactSynthesisRequest> = {}): FactSynthesisRequest {
  return {
    synthesis_id: "syn-1",
    timeout_ms: 30_000,
    model_profile: "standard",
    thinking_level: "medium",
    subject: "queue capacity",
    facts: [
      {
        text: "The sensor queue is bounded to 32 slots.",
        evidence: [
          {
            kind: "source",
            path: "src/queue.rs",
            line_start: 1,
            line_end: 3,
            excerpt: "pub struct Queue {",
          },
        ],
      },
      {
        text: "The queue capacity is guarded by a mutex.",
        evidence: [
          {
            kind: "source",
            path: "src/queue.rs",
            line_start: 5,
            line_end: 7,
            excerpt: "let guard = self.mutex.lock();",
          },
        ],
      },
    ],
    ...overrides,
  };
}

function setEnv(entries: Record<string, string | undefined>): void {
  for (const [key, value] of Object.entries(entries)) {
    if (value === undefined) {
      delete process.env[key];
    } else {
      process.env[key] = value;
    }
  }
}

afterEach(() => {
  setEnv({
    NISSE_FAKE_FACT_SYNTHESIS_RESULTS: undefined,
    NISSE_FAKE_FACT_SYNTHESIS_FAIL: undefined,
  });
  resetFakeFactSynthesisState();
});

describe("fact synthesis answer parsing", () => {
  it("accepts CANNOT_MERGE exactly", () => {
    assert.deepEqual(parseFactSynthesisAnswer("CANNOT_MERGE"), { kind: "cannot_merge" });
    // Whitespace around the answer is tolerated.
    assert.deepEqual(parseFactSynthesisAnswer("  CANNOT_MERGE\n"), { kind: "cannot_merge" });
  });

  it("accepts MERGED with a candidate statement", () => {
    assert.deepEqual(parseFactSynthesisAnswer("MERGED The queue is bounded and guarded."), {
      kind: "merged",
      candidate: "The queue is bounded and guarded.",
    });
    assert.deepEqual(parseFactSynthesisAnswer("  MERGED   trimmed statement  "), {
      kind: "merged",
      candidate: "trimmed statement",
    });
  });

  it("rejects malformed answers", () => {
    for (const bad of [
      "",
      "MERGED",
      "MERGED ",
      "merged the queue is bounded.",
      "The queue is bounded.",
      "MERGED one\nMERGED two",
      "MERGED one\nextra line",
      "cannot_merge",
    ]) {
      assert.throws(
        () => parseFactSynthesisAnswer(bad),
        Error,
        `answer: ${JSON.stringify(bad)}`,
      );
    }
  });

  it("rejects oversized candidates", () => {
    const oversized = `MERGED ${"x".repeat(MAX_CANDIDATE_BYTES + 1)}`;
    assert.throws(() => parseFactSynthesisAnswer(oversized), /exceeds/);
    // Exactly at the bound is accepted.
    const bounded = `MERGED ${"x".repeat(MAX_CANDIDATE_BYTES)}`;
    assert.equal(parseFactSynthesisAnswer(bounded).kind, "merged");
  });
});

describe("fact synthesis prompts", () => {
  it("system prompt states the frozen contract", () => {
    const prompt = buildFactSynthesisSystemPrompt();
    assert.match(prompt, /narrowest\s+statement/);
    assert.match(prompt, /MERGED <the narrowest supported statement>/);
    assert.match(prompt, /CANNOT_MERGE/);
    assert.match(prompt, /never invent a compromise/i);
  });

  it("message carries the subject, every fact, and every excerpt", () => {
    const message = buildFactSynthesisMessage(request());
    assert.match(message, /Subject: queue capacity/);
    assert.match(message, /Fact 1:/);
    assert.match(message, /The sensor queue is bounded to 32 slots\./);
    assert.match(message, /Fact 2:/);
    assert.match(message, /pub struct Queue \{/);
    assert.match(message, /lines 5-7/);
  });

  it("repair prompt echoes the previous answer and the reason", () => {
    const prompt = buildFactSynthesisRepairPrompt("MERGED one\nMERGED two", "must be one line");
    assert.match(prompt, /<previous_answer>/);
    assert.match(prompt, /MERGED one/);
    assert.match(prompt, /Reason: must be one line/);
    assert.match(prompt, /CANNOT_MERGE/);
  });

  it("session identity is stable and non-juror", () => {
    assert.equal(FACT_SYNTHESIS_SESSION_ID, "fact-synthesizer");
  });
});

describe("fact synthesis fake runtime", () => {
  it("scripts merges in order", async () => {
    setEnv({
      NISSE_FAKE_FACT_SYNTHESIS_RESULTS:
        "merge:The queue is bounded to 32 slots and guarded by a mutex.,cannot_merge",
    });
    assert.deepEqual(await runFactSynthesis("fake", request()), {
      kind: "merged",
      candidate: "The queue is bounded to 32 slots and guarded by a mutex.",
    });
    assert.deepEqual(await runFactSynthesis("fake", request()), { kind: "cannot_merge" });
  });

  it("refuses when the script is missing, exhausted, or malformed", async () => {
    setEnv({ NISSE_FAKE_FACT_SYNTHESIS_RESULTS: undefined });
    assert.deepEqual(await runFactSynthesis("fake", request()), { kind: "cannot_merge" });

    setEnv({ NISSE_FAKE_FACT_SYNTHESIS_RESULTS: "merge:" });
    assert.deepEqual(await runFactSynthesis("fake", request()), { kind: "cannot_merge" });

    setEnv({ NISSE_FAKE_FACT_SYNTHESIS_RESULTS: "banana" });
    assert.deepEqual(await runFactSynthesis("fake", request()), { kind: "cannot_merge" });
  });

  it("fails with a scripted error when requested", async () => {
    setEnv({ NISSE_FAKE_FACT_SYNTHESIS_FAIL: "1" });
    const result = await runFactSynthesis("fake", request());
    assert.equal(result.kind, "failed");
    if (result.kind === "failed") {
      assert.equal(result.error.code, "scripted_fake_failure");
    }
  });
});
