/**
 * Unit tests for the finding-equivalence check (plan section 38).
 *
 * Run with: npm test  (node --test, no network access required)
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  buildEquivalenceMessage,
  buildEquivalenceSystemPrompt,
  EQUIVALENCE_ERROR_CODES,
  parseEquivalenceVerdict,
  resetFakeEquivalenceState,
  runFindingEquivalenceCheck,
  type EquivalenceDeps,
} from "./equivalence.ts";
import { createScriptedAgentFactory } from "./scripted.ts";
import {
  ValidationError,
  validateFindingEquivalenceCheck,
  type FindingEquivalenceCheck,
} from "./structured.ts";

function sampleFinding(id: string, claim: string): Record<string, unknown> {
  return {
    id,
    impact_class: "correctness",
    proposed_severity: "high",
    category: "logic",
    title: `Finding ${id}`,
    claim,
    explanation: "Because.",
    recommendation: "Fix it.",
    evidence: [
      {
        kind: "source",
        path: "src/a.rs",
        line_start: 1,
        line_end: 3,
        excerpt: "fn a() {}",
      },
    ],
  };
}

function sampleCheck(): FindingEquivalenceCheck {
  return validateFindingEquivalenceCheck({
    check_id: "chk-1",
    timeout_ms: 5_000,
    model_profile: "standard",
    thinking_level: "low",
    finding_a: {
      reviewer_id: "quality",
      finding: sampleFinding("q-1", "Queue drops the last item."),
    },
    finding_b: {
      reviewer_id: "concurrency",
      finding: sampleFinding("c-1", "The ring buffer loses one element."),
    },
  });
}

test("equivalence check payload validates strictly", () => {
  const check = sampleCheck();
  assert.equal(check.check_id, "chk-1");
  assert.equal(check.timeout_ms, 5_000);
  assert.equal(check.finding_a.reviewer_id, "quality");
  assert.equal(check.finding_b.finding.id, "c-1");

  const mutate = (mutator: (payload: Record<string, unknown>) => void): unknown => {
    const payload = {
      check_id: "chk-1",
      timeout_ms: 5_000,
      model_profile: "standard",
      thinking_level: "low",
      finding_a: { reviewer_id: "quality", finding: sampleFinding("q-1", "c") },
      finding_b: { reviewer_id: "concurrency", finding: sampleFinding("c-1", "c") },
    };
    mutator(payload);
    return payload;
  };

  assert.throws(
    () => validateFindingEquivalenceCheck(mutate((p) => void (p["check_id"] = ""))),
    ValidationError,
  );
  assert.throws(
    () => validateFindingEquivalenceCheck(mutate((p) => void (p["timeout_ms"] = 0))),
    ValidationError,
  );
  assert.throws(
    () => validateFindingEquivalenceCheck(mutate((p) => void (p["thinking_level"] = "extreme"))),
    ValidationError,
  );
  assert.throws(
    () => validateFindingEquivalenceCheck(mutate((p) => void delete p["model_profile"])),
    ValidationError,
  );
  assert.throws(
    () =>
      validateFindingEquivalenceCheck(
        mutate((p) => void (p["finding_a"] = { reviewer_id: "", finding: sampleFinding("q", "c") })),
      ),
    ValidationError,
  );
  assert.throws(
    () =>
      validateFindingEquivalenceCheck(
        mutate((p) => {
          const finding = sampleFinding("q-1", "c") as Record<string, unknown>;
          finding["impact_class"] = "availability";
          p["finding_b"] = { reviewer_id: "concurrency", finding };
        }),
      ),
    ValidationError,
  );
});

test("verdict parsing is strict", () => {
  assert.equal(parseEquivalenceVerdict('{"verdict":"same"}'), "same");
  assert.equal(parseEquivalenceVerdict('```json\n{"verdict":"different"}\n```'), "different");
  assert.equal(parseEquivalenceVerdict('  {"verdict":"uncertain"}  '), "uncertain");

  for (const bad of [
    "",
    "same",
    '{"verdict":"same","reason":"x"}',
    '{"reason":"x"}',
    '{"verdict":"maybe"}',
    '{"verdict":1}',
    "{}",
    "[]",
    'prefix {"verdict":"same"}',
  ]) {
    assert.throws(() => parseEquivalenceVerdict(bad), Error, `expected rejection: ${bad}`);
  }
});

test("prompts render both findings and the contract", () => {
  const system = buildEquivalenceSystemPrompt();
  assert.match(system, /non-juror session/);
  assert.match(system, /"verdict":"same"/);

  const message = buildEquivalenceMessage(sampleCheck());
  assert.match(message, /reviewer `quality`, finding id `q-1`/);
  assert.match(message, /reviewer `concurrency`, finding id `c-1`/);
  assert.match(message, /Queue drops the last item\./);
  assert.match(message, /source at src\/a\.rs lines 1-3/);
});

test("fake runtime answers deterministically", async () => {
  const restore = (key: string, value: string | undefined) => {
    if (value === undefined) {
      delete process.env[key];
    } else {
      process.env[key] = value;
    }
  };
  const verdicts = process.env["NISSE_FAKE_EQUIVALENCE_VERDICTS"];
  const fail = process.env["NISSE_FAKE_EQUIVALENCE_FAIL"];

  try {
    // Default: uncertain.
    resetFakeEquivalenceState();
    delete process.env["NISSE_FAKE_EQUIVALENCE_VERDICTS"];
    delete process.env["NISSE_FAKE_EQUIVALENCE_FAIL"];
    assert.deepEqual(await runFindingEquivalenceCheck("fake", sampleCheck()), {
      verdict: "uncertain",
    });

    // Scripted verdicts are consumed in order, then exhausted.
    resetFakeEquivalenceState();
    process.env["NISSE_FAKE_EQUIVALENCE_VERDICTS"] = "same,different";
    assert.equal((await runFindingEquivalenceCheck("fake", sampleCheck())).verdict, "same");
    assert.equal((await runFindingEquivalenceCheck("fake", sampleCheck())).verdict, "different");
    assert.equal((await runFindingEquivalenceCheck("fake", sampleCheck())).verdict, "uncertain");

    // Scripted failure forces uncertain with an error.
    resetFakeEquivalenceState();
    process.env["NISSE_FAKE_EQUIVALENCE_FAIL"] = "1";
    const failed = await runFindingEquivalenceCheck("fake", sampleCheck());
    assert.equal(failed.verdict, "uncertain");
    assert.equal(failed.error?.code, EQUIVALENCE_ERROR_CODES.scriptedFailure);
  } finally {
    restore("NISSE_FAKE_EQUIVALENCE_VERDICTS", verdicts);
    restore("NISSE_FAKE_EQUIVALENCE_FAIL", fail);
    resetFakeEquivalenceState();
  }
});

function scriptedDeps(script: unknown): EquivalenceDeps {
  const factory = createScriptedAgentFactory(
    script as Parameters<typeof createScriptedAgentFactory>[0],
    undefined,
  );
  return {
    models: new Map([["standard", { model: "acme/local-small" }]]),
    resolveAgent: async () => factory,
  };
}

test("pi runtime parses a valid scripted verdict", async () => {
  const deps = scriptedDeps({
    sessions: [{ steps: [{ reply: '{"verdict":"different"}' }] }],
  });
  assert.deepEqual(await runFindingEquivalenceCheck("pi", sampleCheck(), deps), {
    verdict: "different",
  });
});

test("pi runtime repairs one malformed verdict then fails closed", async () => {
  // Malformed first answer, repaired second answer.
  const repaired = scriptedDeps({
    sessions: [
      { steps: [{ reply: "They are the same issue." }, { reply: '{"verdict":"same"}' }] },
    ],
  });
  assert.equal((await runFindingEquivalenceCheck("pi", sampleCheck(), repaired)).verdict, "same");

  // Permanently malformed answers fall back to uncertain with an error.
  const broken = scriptedDeps({
    sessions: [{ steps: [{ reply: "same" }, { reply: '{"verdict":"yes"}' }] }],
  });
  const failed = await runFindingEquivalenceCheck("pi", sampleCheck(), broken);
  assert.equal(failed.verdict, "uncertain");
  assert.equal(failed.error?.code, EQUIVALENCE_ERROR_CODES.invalidVerdict);
});

test("pi runtime fails closed on timeout and unresolved models", async () => {
  // Timeout: the scripted agent delays past the check's deadline.
  const quick = validateFindingEquivalenceCheck({
    check_id: "chk-fast",
    timeout_ms: 100,
    model_profile: "standard",
    thinking_level: "low",
    finding_a: { reviewer_id: "quality", finding: sampleFinding("q-1", "c") },
    finding_b: { reviewer_id: "concurrency", finding: sampleFinding("c-1", "c") },
  });
  const hanging = scriptedDeps({
    sessions: [{ steps: [{ hangMs: 5_000 }] }],
  });
  const timedOut = await runFindingEquivalenceCheck("pi", quick, hanging);
  assert.equal(timedOut.verdict, "uncertain");
  assert.equal(timedOut.error?.code, EQUIVALENCE_ERROR_CODES.timeout);

  // Unresolved model profile: no session is even created.
  const factory = createScriptedAgentFactory({ sessions: [] }, undefined);
  const deps: EquivalenceDeps = {
    models: new Map(),
    resolveAgent: async () => factory,
  };
  const unresolved = await runFindingEquivalenceCheck("pi", sampleCheck(), deps);
  assert.equal(unresolved.verdict, "uncertain");
  assert.equal(unresolved.error?.code, EQUIVALENCE_ERROR_CODES.modelUnresolved);
});
