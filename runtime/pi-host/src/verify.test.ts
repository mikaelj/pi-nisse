/**
 * Unit tests for independent fact verification (plan section 47).
 *
 * Run with: npm test  (node --test, no network access required)
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  buildFactVerificationMessage,
  buildFactVerificationSystemPrompt,
  FACT_VERIFICATION_ERROR_CODES,
  parseFactVerificationVerdict,
  resetFakeFactVerificationState,
  runFactVerification,
  type FactVerificationDeps,
} from "./verify.ts";
import { createScriptedAgentFactory } from "./scripted.ts";
import {
  ValidationError,
  validateFactVerificationRequest,
  type FactVerificationRequest,
} from "./structured.ts";

function sampleRequest(claim: string): FactVerificationRequest {
  return validateFactVerificationRequest({
    verification_id: "ver-1",
    timeout_ms: 5_000,
    model_profile: "standard",
    thinking_level: "medium",
    claim,
    evidence: [
      {
        kind: "source",
        path: "src/queue.rs",
        line_start: 1,
        line_end: 3,
        excerpt: "pub struct Queue { slots: [u8; 32] }",
      },
    ],
  });
}

test("fact verification payload validates strictly", () => {
  const request = sampleRequest("The sensor queue is bounded to 32 slots.");
  assert.equal(request.verification_id, "ver-1");
  assert.equal(request.timeout_ms, 5_000);
  assert.equal(request.thinking_level, "medium");
  assert.equal(request.evidence.length, 1);
  assert.equal(request.evidence[0]?.path, "src/queue.rs");

  const mutate = (mutator: (payload: Record<string, unknown>) => void): unknown => {
    const payload = {
      verification_id: "ver-1",
      timeout_ms: 5_000,
      model_profile: "standard",
      thinking_level: "medium",
      claim: "c",
      evidence: [{ kind: "source", excerpt: "fn a() {}" }],
    };
    mutator(payload);
    return payload;
  };

  assert.throws(
    () => validateFactVerificationRequest(mutate((p) => void (p["verification_id"] = ""))),
    ValidationError,
  );
  assert.throws(
    () => validateFactVerificationRequest(mutate((p) => void (p["timeout_ms"] = 0))),
    ValidationError,
  );
  assert.throws(
    () => validateFactVerificationRequest(mutate((p) => void (p["thinking_level"] = "extreme"))),
    ValidationError,
  );
  assert.throws(
    () => validateFactVerificationRequest(mutate((p) => void delete p["claim"])),
    ValidationError,
  );
  assert.throws(
    () => validateFactVerificationRequest(mutate((p) => void (p["claim"] = ""))),
    ValidationError,
  );
  assert.throws(
    () => validateFactVerificationRequest(mutate((p) => void (p["evidence"] = []))),
    ValidationError,
  );
  assert.throws(
    () =>
      validateFactVerificationRequest(
        mutate((p) => {
          p["evidence"] = Array.from({ length: 17 }, () => ({ kind: "source", excerpt: "x" }));
        }),
      ),
    ValidationError,
  );
  assert.throws(
    () =>
      validateFactVerificationRequest(
        mutate((p) => {
          p["evidence"] = [{ kind: "snippet", excerpt: "x" }];
        }),
      ),
    ValidationError,
  );
});

test("verdict parsing is strict", () => {
  assert.equal(parseFactVerificationVerdict('{"verdict":"supported"}'), "supported");
  assert.equal(
    parseFactVerificationVerdict('```json\n{"verdict":"contradicted"}\n```'),
    "contradicted",
  );
  assert.equal(parseFactVerificationVerdict('  {"verdict":"insufficient"}  '), "insufficient");

  for (const bad of [
    "",
    "supported",
    '{"verdict":"supported","reason":"x"}',
    '{"reason":"x"}',
    '{"verdict":"uncertain"}',
    '{"verdict":"maybe"}',
    '{"verdict":1}',
    "{}",
    "[]",
    'prefix {"verdict":"supported"}',
  ]) {
    assert.throws(() => parseFactVerificationVerdict(bad), Error, `expected rejection: ${bad}`);
  }
});

test("prompts render the exact claim and evidence", () => {
  const system = buildFactVerificationSystemPrompt();
  assert.match(system, /non-juror session/);
  assert.match(system, /"verdict":"supported"/);
  assert.match(system, /never use outside knowledge/);

  const message = buildFactVerificationMessage(
    sampleRequest("The sensor queue is bounded to 32 slots."),
  );
  assert.match(message, /Claim:\n  The sensor queue is bounded to 32 slots\./);
  assert.match(message, /source at src\/queue\.rs lines 1-3/);
  assert.match(message, /pub struct Queue \{ slots: \[u8; 32\] \}/);
});

test("fake runtime answers deterministically", async () => {
  const restore = (key: string, value: string | undefined) => {
    if (value === undefined) {
      delete process.env[key];
    } else {
      process.env[key] = value;
    }
  };
  const verdicts = process.env["NISSE_FAKE_FACT_VERIFICATION_VERDICTS"];
  const fail = process.env["NISSE_FAKE_FACT_VERIFICATION_FAIL"];

  try {
    // Default: insufficient (the fail-safe verdict).
    resetFakeFactVerificationState();
    delete process.env["NISSE_FAKE_FACT_VERIFICATION_VERDICTS"];
    delete process.env["NISSE_FAKE_FACT_VERIFICATION_FAIL"];
    assert.deepEqual(await runFactVerification("fake", sampleRequest("c")), {
      verdict: "insufficient",
    });

    // Scripted verdicts are consumed in order, then exhausted.
    resetFakeFactVerificationState();
    process.env["NISSE_FAKE_FACT_VERIFICATION_VERDICTS"] = "supported,contradicted";
    assert.equal((await runFactVerification("fake", sampleRequest("c"))).verdict, "supported");
    assert.equal(
      (await runFactVerification("fake", sampleRequest("c"))).verdict,
      "contradicted",
    );
    assert.equal((await runFactVerification("fake", sampleRequest("c"))).verdict, "insufficient");

    // Unknown scripted entries fall back to insufficient.
    resetFakeFactVerificationState();
    process.env["NISSE_FAKE_FACT_VERIFICATION_VERDICTS"] = "maybe";
    assert.equal((await runFactVerification("fake", sampleRequest("c"))).verdict, "insufficient");

    // Scripted failure forces insufficient with an error.
    resetFakeFactVerificationState();
    process.env["NISSE_FAKE_FACT_VERIFICATION_FAIL"] = "1";
    const failed = await runFactVerification("fake", sampleRequest("c"));
    assert.equal(failed.verdict, "insufficient");
    assert.equal(failed.error?.code, FACT_VERIFICATION_ERROR_CODES.scriptedFailure);
  } finally {
    restore("NISSE_FAKE_FACT_VERIFICATION_VERDICTS", verdicts);
    restore("NISSE_FAKE_FACT_VERIFICATION_FAIL", fail);
    resetFakeFactVerificationState();
  }
});

function scriptedDeps(script: unknown): FactVerificationDeps {
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
    sessions: [{ steps: [{ reply: '{"verdict":"contradicted"}' }] }],
  });
  assert.deepEqual(await runFactVerification("pi", sampleRequest("c"), deps), {
    verdict: "contradicted",
  });
});

test("pi runtime repairs one malformed verdict then fails closed", async () => {
  // Malformed first answer, repaired second answer.
  const repaired = scriptedDeps({
    sessions: [
      { steps: [{ reply: "The claim holds." }, { reply: '{"verdict":"supported"}' }] },
    ],
  });
  assert.equal((await runFactVerification("pi", sampleRequest("c"), repaired)).verdict, "supported");

  // Permanently malformed answers fall back to insufficient with an error.
  const broken = scriptedDeps({
    sessions: [{ steps: [{ reply: "supported" }, { reply: '{"verdict":"yes"}' }] }],
  });
  const failed = await runFactVerification("pi", sampleRequest("c"), broken);
  assert.equal(failed.verdict, "insufficient");
  assert.equal(failed.error?.code, FACT_VERIFICATION_ERROR_CODES.invalidVerdict);
});

test("pi runtime fails closed on timeout and unresolved models", async () => {
  // Timeout: the scripted agent delays past the job's deadline.
  const quick = validateFactVerificationRequest({
    verification_id: "ver-fast",
    timeout_ms: 100,
    model_profile: "standard",
    thinking_level: "medium",
    claim: "c",
    evidence: [{ kind: "source", excerpt: "fn a() {}" }],
  });
  const hanging = scriptedDeps({
    sessions: [{ steps: [{ hangMs: 5_000 }] }],
  });
  const timedOut = await runFactVerification("pi", quick, hanging);
  assert.equal(timedOut.verdict, "insufficient");
  assert.equal(timedOut.error?.code, FACT_VERIFICATION_ERROR_CODES.timeout);

  // Unresolved model profile: no session is even created.
  const factory = createScriptedAgentFactory({ sessions: [] }, undefined);
  const deps: FactVerificationDeps = {
    models: new Map(),
    resolveAgent: async () => factory,
  };
  const unresolved = await runFactVerification("pi", sampleRequest("c"), deps);
  assert.equal(unresolved.verdict, "insufficient");
  assert.equal(unresolved.error?.code, FACT_VERIFICATION_ERROR_CODES.modelUnresolved);
});
