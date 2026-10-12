/**
 * Tests for model capability resolution (protocol version 2):
 * the shared golden fixture, the real-catalog drift canary, and the
 * `host_config` → `model_capabilities` exchange of the host process
 * (fake runtime: permissive double plus the test-only env override).
 */

import assert from "node:assert/strict";
import { spawn, type ChildProcess } from "node:child_process";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import test from "node:test";

import {
  PI_THINKING_LEVELS,
  permissiveEntry,
  supportedThinkingLevels,
  type CapabilityModel,
} from "./capabilities.ts";
import { THINKING_LEVELS } from "./structured.ts";

interface GoldenFixture {
  order: string[];
  cases: Array<{
    name: string;
    reasoning: boolean;
    thinking_level_map?: Record<string, string | null>;
    expected_supported: string[];
    clamps: Array<[string, string]>;
  }>;
}

const fixture: GoldenFixture = JSON.parse(
  readFileSync(
    fileURLToPath(new URL("../fixtures/thinking-levels.golden.json", import.meta.url)),
    "utf8",
  ),
) as GoldenFixture;

test("the fixture order mirrors pi's EXTENDED_THINKING_LEVELS", () => {
  assert.deepEqual(fixture.order, [...PI_THINKING_LEVELS]);
  assert.deepEqual(fixture.order, [...THINKING_LEVELS]);
});

test("supported-level derivation matches the shared golden fixture", () => {
  assert.ok(fixture.cases.length >= 5, "fixture cases went missing");
  for (const case_ of fixture.cases) {
    const model: CapabilityModel = {
      reasoning: case_.reasoning,
      thinkingLevelMap: case_.thinking_level_map ?? {},
    };
    assert.deepEqual(
      supportedThinkingLevels(model),
      case_.expected_supported,
      `fixture case \`${case_.name}\``,
    );
    // Clamp consistency: every expected clamp target is supported.
    for (const [requested, clamped] of case_.clamps) {
      assert.ok(
        case_.expected_supported.includes(clamped),
        `case \`${case_.name}\`: clamp(${requested}) → ${clamped} must be supported`,
      );
    }
  }
});

test("xhigh and max require an explicit mapping; omitted keys pass through", () => {
  const noMap: CapabilityModel = { reasoning: true };
  assert.deepEqual(supportedThinkingLevels(noMap), [
    "off",
    "minimal",
    "low",
    "medium",
    "high",
  ]);
  const explicitOnly: CapabilityModel = {
    reasoning: true,
    thinkingLevelMap: { low: "low", xhigh: "xhigh", max: null, medium: null },
  };
  // off/minimal/high are omitted (pass-through); medium/max are null
  // (disabled); low/xhigh are explicit.
  assert.deepEqual(supportedThinkingLevels(explicitOnly), [
    "off",
    "minimal",
    "low",
    "high",
    "xhigh",
  ]);
});

test("non-reasoning models support only off", () => {
  assert.deepEqual(supportedThinkingLevels({ reasoning: false }), ["off"]);
  assert.deepEqual(supportedThinkingLevels({}), ["off"]);
});

test("permissive double supports every level", () => {
  assert.deepEqual(permissiveEntry("acme/m").supported_thinking_levels, [
    ...PI_THINKING_LEVELS,
  ]);
});

test("drift canary: the installed SDK catalog still reports zai/glm-5.3 as low/high/max", async () => {
  const sdk = await import("@earendil-works/pi-coding-agent");
  const runtime = await sdk.ModelRuntime.create({ allowModelNetwork: false });
  const model = runtime.getModel("zai", "glm-5.3");
  assert.ok(model, "zai/glm-5.3 is missing from the installed Pi catalog");
  assert.deepEqual(supportedThinkingLevels(model), ["low", "high", "max"]);
});

/** Spawn the host exactly like the Rust core does and speak JSONL. */
function spawnHost(
  runtime: "fake" | "pi",
  extraEnv: NodeJS.ProcessEnv,
): ChildProcess {
  const env: NodeJS.ProcessEnv = { ...process.env, ...extraEnv };
  // Own the capability configuration explicitly: an ambient
  // NISSE_PI_HOST_CAPABILITIES must never leak into tests that did not
  // ask for it.
  if (extraEnv["NISSE_PI_HOST_CAPABILITIES"] === undefined) {
    delete env["NISSE_PI_HOST_CAPABILITIES"];
  }
  return spawn(process.execPath, ["src/main.ts", "--runtime", runtime], {
    env,
    stdio: ["pipe", "pipe", "pipe"],
  });
}

interface Reply {
  protocol_version: number;
  id: string;
  type: string;
  [key: string]: unknown;
}

async function readReplies(child: ChildProcess, count: number): Promise<Reply[]> {
  const replies: Reply[] = [];
  const pending: string[] = [];
  return new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error(`timed out waiting for ${count} replies`)),
      15_000,
    );
    let buffer = "";
    child.stdout!.setEncoding("utf8");
    child.stdout!.on("data", (chunk: string) => {
      buffer += chunk;
      let newline = buffer.indexOf("\n");
      while (newline !== -1) {
        const line = buffer.slice(0, newline);
        buffer = buffer.slice(newline + 1);
        if (line.trim().length > 0) {
          pending.push(line);
        }
        while (pending.length >= count - replies.length && pending.length > 0) {
          replies.push(JSON.parse(pending.shift()!) as Reply);
          if (replies.length === count) {
            clearTimeout(timer);
            resolve(replies);
            return;
          }
        }
        newline = buffer.indexOf("\n");
      }
    });
    child.on("exit", (code) => {
      clearTimeout(timer);
      reject(new Error(`host exited early with code ${code}`));
    });
  });
}

test("host_config is answered by model_capabilities echoing the config id (fake: permissive)", async () => {
  const child = spawnHost("fake", {});
  try {
    const repliesP = readReplies(child, 2);
    child.stdin!.write(
      '{"protocol_version":2,"id":"cfg-1","type":"host_config","models":{"standard":{"model":"zai/glm-5.3"}}}\n',
    );
    const replies = await repliesP;
    assert.equal(replies[0]!.type, "host_ready");
    const caps = replies[1]!;
    assert.equal(caps.type, "model_capabilities");
    assert.equal(caps.host_config_id, "cfg-1");
    const profiles = caps.profiles as Record<string, { model: string; supported_thinking_levels: string[] }>;
    assert.deepEqual(profiles["standard"]!.supported_thinking_levels, [...THINKING_LEVELS]);
    assert.deepEqual(caps.unresolved, {});
  } finally {
    child.kill();
  }
});

test("the env override restricts reported levels; malformed overrides fail loudly", async () => {
  const child = spawnHost("fake", { NISSE_PI_HOST_CAPABILITIES: '{"standard":["low","max"]}' });
  try {
    const repliesP = readReplies(child, 2);
    child.stdin!.write(
      '{"protocol_version":2,"id":"cfg-2","type":"host_config","models":{"standard":{"model":"zai/glm-5.3"}}}\n',
    );
    const replies = await repliesP;
    const caps = replies[1]!;
    assert.equal(caps.type, "model_capabilities");
    const profiles = caps.profiles as Record<string, { supported_thinking_levels: string[] }>;
    assert.deepEqual(profiles["standard"]!.supported_thinking_levels, ["low", "max"]);
  } finally {
    child.kill();
  }

  const broken = spawnHost("fake", { NISSE_PI_HOST_CAPABILITIES: '{"standard":[]}' });
  const stdout = await new Promise<string>((resolve, reject) => {
    let buffer = "";
    const timer = setTimeout(() => reject(new Error("host did not exit")), 15_000);
    broken.stdout!.setEncoding("utf8");
    broken.stdout!.on("data", (chunk: string) => (buffer += chunk));
    broken.on("exit", (code) => {
      clearTimeout(timer);
      assert.equal(code, 2);
      resolve(buffer);
    });
    broken.stdin!.write(
      '{"protocol_version":2,"id":"cfg-3","type":"host_config","models":{"standard":{"model":"zai/glm-5.3"}}}\n',
    );
  });
  assert.match(stdout, /host_error/);
  assert.match(stdout, /NISSE_PI_HOST_CAPABILITIES is malformed/);
});

test("the real Pi runtime ignores NISSE_PI_HOST_CAPABILITIES entirely", async () => {
  const child = spawnHost("pi", {
    NISSE_PI_HOST_CAPABILITIES: '{"standard":["off"]}',
  });
  try {
    const repliesP = readReplies(child, 2);
    child.stdin!.write(
      '{"protocol_version":2,"id":"cfg-9","type":"host_config","models":{"standard":{"model":"zai/glm-5.3"}}}\n',
    );
    const replies = await repliesP;
    const caps = replies[1]!;
    assert.equal(caps.type, "model_capabilities");
    const profiles = caps.profiles as Record<string, { supported_thinking_levels: string[] }>;
    // Catalog-derived levels, not the (restrictive) test override: the
    // env var is a fake/scripted-mode knob only.
    assert.deepEqual(profiles["standard"]!.supported_thinking_levels, ["low", "high", "max"]);
  } finally {
    child.kill();
  }
});

test("start_review flows while capability resolution is in flight (no serialization)", async () => {
  const child = spawnHost("fake", {});
  try {
    // host_config and start_review back-to-back: the dispatch loop
    // must enqueue the review without waiting for the capabilities
    // reply, and both replies must arrive.
    const repliesP = readReplies(child, 3);
    child.stdin!.write(
      '{"protocol_version":2,"id":"cfg-7","type":"host_config","models":{"standard":{"model":"zai/glm-5.3"}}}\n',
    );
    child.stdin!.write(
      '{"protocol_version":2,"id":"rev-1","type":"start_review","review_id":"01J8Z","kind":"plan","reviewers":[{"reviewer_id":"quality","specialization":"general","instructions":"work the checklist","checklist":["check error paths"],"model_profile":"standard","thinking_level":"medium","initial_context":[],"allowed_capabilities":["project_lookup"],"max_lookup_calls":1,"max_returned_context_bytes":1024,"timeout_ms":30000}]}\n',
    );
    const replies = await repliesP;
    const types = replies.map((reply) => reply.type);
    // The review session started (and asked its lookup tool) while the
    // capabilities reply was still resolving: nothing serialized.
    assert.ok(types.includes("reviewer_started"), `review started: ${types.join(",")}`);
    const request = replies.find((reply) => reply.type === "tool_request");
    assert.ok(request, `tool_request arrived: ${types.join(",")}`);
    // Answer the lookup, then expect both the capabilities reply and
    // the reviewer completion.
    const finalP = readReplies(child, 2);
    child.stdin!.write(
      `${JSON.stringify({
        protocol_version: 2,
        id: "resp-1",
        type: "tool_response",
        request_id: request.id,
        ok: false,
        error: { code: "tool_failed", message: "no context" },
      })}\n`,
    );
    const finalReplies = await finalP;
    const finalTypes = [...types, ...finalReplies.map((reply) => reply.type)];
    assert.ok(
      finalTypes.includes("model_capabilities"),
      `capabilities arrived: ${finalTypes.join(",")}`,
    );
    assert.ok(
      finalTypes.includes("reviewer_completed") || finalTypes.includes("reviewer_failed"),
      `the review session finished alongside resolution: ${finalTypes.join(",")}`,
    );
  } finally {
    child.kill();
  }
});
