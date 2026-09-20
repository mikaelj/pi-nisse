/**
 * Tests for constrained dynamic-domain descriptions (plan section 19):
 * strict answer parsing, prompt construction, fake determinism, and the
 * pi-runtime session pipeline through the scripted mock.
 */

import * as assert from "node:assert/strict";
import { afterEach, describe, it } from "node:test";

import {
  DOMAIN_DESCRIPTION_SESSION_ID,
  buildDomainDescriptionMessage,
  buildDomainDescriptionRepairPrompt,
  buildDomainDescriptionSystemPrompt,
  fakeDomainDescription,
  parseDomainDescription,
  runDomainDescription,
} from "./domain.ts";
import type { DomainDescriptionRequest } from "./structured.ts";
import { createScriptedAgentFactory } from "./scripted.ts";
import type { HostModelTable } from "./models.ts";
import type { SessionScript } from "./scripted.ts";

function request(): DomainDescriptionRequest {
  return {
    description_id: "desc-1",
    timeout_ms: 5_000,
    model_profile: "standard",
    thinking_level: "low",
    descriptors: ["Rewrite the audio pipeline", "Queue changes"],
    changed_paths: [],
    languages: ["rust"],
  };
}

function validAnswer(): string {
  return JSON.stringify({
    domain_label: "audio dsp",
    focus_terms: ["sampling rate"],
    risks_to_check: ["denormals"],
    relevant_practice_tags: ["realtime"],
  });
}

/** Assert that an action throws (without asserting a predicate). */
function assertMalformed(action: () => unknown): void {
  let threw = false;
  try {
    action();
  } catch {
    threw = true;
  }
  assert.ok(threw, "expected the answer to be rejected");
}

afterEach(() => {
  delete process.env["NISSE_FAKE_DOMAIN_DESCRIPTION_FAIL"];
});

describe("prompt construction", () => {
  it("system prompt fixes the output contract", () => {
    const prompt = buildDomainDescriptionSystemPrompt();
    assert.ok(prompt.includes("domain_label"));
    assert.ok(prompt.includes("focus_terms"));
    assert.ok(prompt.includes("risks_to_check"));
    assert.ok(prompt.includes("relevant_practice_tags"));
    assert.ok(prompt.includes("Never describe tools"));
  });

  it("message renders descriptors, paths, and languages", () => {
    const message = buildDomainDescriptionMessage(request());
    assert.ok(message.includes("- Rewrite the audio pipeline"));
    assert.ok(message.includes("- Queue changes"));
    assert.ok(message.includes("(none)"), "empty paths render as none");
    assert.ok(message.includes("- rust"));
  });

  it("repair prompt quotes the failing answer", () => {
    const prompt = buildDomainDescriptionRepairPrompt("bad answer", "reason here");
    assert.ok(prompt.includes("reason here"));
    assert.ok(prompt.includes("<previous_answer>"));
    assert.ok(prompt.includes("bad answer"));
  });
});

describe("parseDomainDescription", () => {
  it("accepts exactly the four fields", () => {
    const description = parseDomainDescription(validAnswer());
    assert.equal(description.domain_label, "audio dsp");
    assert.deepEqual(description.focus_terms, ["sampling rate"]);
    assert.deepEqual(description.risks_to_check, ["denormals"]);
    assert.deepEqual(description.relevant_practice_tags, ["realtime"]);
  });

  it("accepts a single fenced answer", () => {
    const fenced = "```json\n" + validAnswer() + "\n```";
    assert.equal(parseDomainDescription(fenced).domain_label, "audio dsp");
  });

  it("rejects malformed answers", () => {
    for (const bad of [
      "",
      "not json",
      "[]",
      JSON.stringify({ domain_label: "x" }),
      JSON.stringify({
        domain_label: "x",
        focus_terms: [],
        risks_to_check: [],
      }),
      JSON.stringify({
        domain_label: "x",
        focus_terms: [],
        risks_to_check: [],
        relevant_practice_tags: [],
        extra: 1,
      }),
      JSON.stringify({
        domain_label: "",
        focus_terms: [],
        risks_to_check: [],
        relevant_practice_tags: [],
      }),
      JSON.stringify({
        domain_label: "x",
        focus_terms: [""],
        risks_to_check: [],
        relevant_practice_tags: [],
      }),
      JSON.stringify({
        domain_label: "x",
        focus_terms: [],
        risks_to_check: new Array(17).fill("risk"),
        relevant_practice_tags: [],
      }),
      JSON.stringify({
        domain_label: "x",
        focus_terms: [],
        risks_to_check: [],
        relevant_practice_tags: [42],
      }),
    ]) {
      assertMalformed(() => parseDomainDescription(bad));
    }
  });
});

describe("fake runtime", () => {
  it("derives a deterministic description from the descriptors", () => {
    const first = fakeDomainDescription(request());
    const second = fakeDomainDescription(request());
    assert.deepEqual(first, second);
    assert.equal(first.domain_label, "audio pipeline");
    assert.ok(first.focus_terms.includes("rust code"));
  });

  it("falls back to an unspecified label without descriptors", () => {
    const description = fakeDomainDescription({
      ...request(),
      descriptors: [],
      languages: [],
    });
    assert.equal(description.domain_label, "unspecified domain");
    assert.deepEqual(description.focus_terms, []);
  });

  it("completes with an error when the fault is enabled", async () => {
    process.env["NISSE_FAKE_DOMAIN_DESCRIPTION_FAIL"] = "1";
    const result = await runDomainDescription("fake", request());
    assert.equal(result.description, undefined);
    assert.equal(result.error?.code, "scripted_fake_failure");
  });
});

describe("pi runtime (scripted agent)", () => {
  const models: HostModelTable = new Map([
    ["standard", { model: "acme/local-small" }],
  ]);

  function scripted(script: SessionScript, recordPath?: string) {
    return runDomainDescription("pi", request(), {
      models,
      resolveAgent: () => Promise.resolve(createScriptedAgentFactory(script, recordPath)),
    });
  }

  it("runs an isolated tool-less session and returns the description", async () => {
    const recordPath = await import("node:fs").then((fs) =>
      fs.mkdtempSync("nisse-domain-record-") + "/record.jsonl",
    );
    const result = await scripted(
      { sessions: [{ steps: [{ reply: validAnswer() }] }] },
      recordPath,
    );
    assert.deepEqual(result.error, undefined);
    assert.equal(result.description?.domain_label, "audio dsp");

    const { readFileSync, rmSync } = await import("node:fs");
    const lines = readFileSync(recordPath, "utf8")
      .split("\n")
      .filter((line) => line.trim().length > 0)
      .map((line) => JSON.parse(line) as Record<string, unknown>);
    rmSync(recordPath);
    assert.equal(lines.length, 1);
    const recorded = lines[0];
    assert.ok(recorded !== undefined, "one session was recorded");
    const config = recorded["config"] as Record<string, unknown>;
    assert.equal(config["reviewerId"], DOMAIN_DESCRIPTION_SESSION_ID);
    assert.equal(config["model"], "acme/local-small");
    assert.equal(config["thinkingLevel"], "low");
    assert.deepEqual(config["toolNames"], []);
    assert.equal((config["isolation"] as Record<string, unknown>)["noBuiltinTools"], true);
    const prompts = recorded["prompts"] as string[];
    assert.ok(prompts[0]?.includes("Rewrite the audio pipeline"));
  });

  it("repairs exactly once and fails closed after a second bad answer", async () => {
    const result = await scripted({
      sessions: [{ steps: [{ reply: "garbage" }, { reply: validAnswer() }] }],
    });
    assert.equal(result.description?.domain_label, "audio dsp");

    const failed = await scripted({
      sessions: [{ steps: [{ reply: "garbage" }, { reply: "still garbage" }] }],
    });
    assert.equal(failed.description, undefined);
    assert.equal(failed.error?.code, "invalid_domain_description");
  });

  it("fails closed when the model profile was never supplied", async () => {
    const result = await runDomainDescription("pi", request(), {
      models: new Map(),
      resolveAgent: () =>
        Promise.resolve(createScriptedAgentFactory({ sessions: [] }, undefined)),
    });
    assert.equal(result.description, undefined);
    assert.equal(result.error?.code, "model_profile_unresolved");
  });
});
