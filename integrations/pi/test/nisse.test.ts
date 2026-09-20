/**
 * Tests for the Nisse Main Pi extension (Nisse plan section 62,
 * milestone 24): tool schemas, success, failure, JSON parsing, and the
 * thin-adapter invariant. All tests run offline against a fake runner
 * or a fake `nisse` binary; no real Nisse or LLM provider is required.
 */

import assert from "node:assert/strict";
import { chmodSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { after, afterEach, before, describe, it } from "node:test";
import {
	DEFAULT_TIMEOUT_MS,
	MAX_OUTPUT_BYTES,
	NISSE_TOOL_NAMES,
	type NisseInvocation,
	type NisseProcessResult,
	type NisseRunner,
	type NisseToolDetails,
	buildNisseTools,
	default as nisseExtension,
	feedbackArgs,
	queryArgs,
	resolveNisseBin,
	resolveTimeoutMs,
	reviewCodeArgs,
	reviewPlanArgs,
	spawnNisse,
} from "../extensions/nisse.ts";

const CWD = "/tmp/nisse-project";

/** A context stub with only what the tools read (`cwd`). */
function contextStub(cwd: string = CWD): ExtensionContext {
	return { cwd } as unknown as ExtensionContext;
}

/** A recording fake runner with a scripted result. */
function fakeRunner(
	result: Partial<NisseProcessResult> = {},
): NisseRunner & { invocations: NisseInvocation[] } {
	const invocations: NisseInvocation[] = [];
	const runner: NisseRunner = async (invocation) => {
		invocations.push(invocation);
		return {
			args: invocation.args,
			stdout: "",
			stderr: "",
			exitCode: 0,
			killed: false,
			...result,
		};
	};
	return Object.assign(runner, { invocations });
}

function okRunner(stdout: string): NisseRunner & { invocations: NisseInvocation[] } {
	return fakeRunner({ stdout });
}

const REVIEW_JSON = JSON.stringify({
	review_id: "01TESTREVIEW",
	kind: "plan",
	created_at: "2026-01-01T00:00:00Z",
	snapshot: { mode: "working" },
	summary: "looks fine",
	findings: [
		{
			id: "01TESTFINDING",
			impact_class: "correctness",
			severity: "high",
			category: "logic",
			title: "off-by-one",
			claim: "loop reads past the end",
			explanation: "…",
			recommendation: "bound the index",
			evidence: [],
			source_finding_ids: ["a", "b"],
			reviewer_ids: ["quality", "testing"],
			cross_validated: true,
		},
	],
	unknown_future_field: "must survive verbatim passthrough",
});

const QUERY_JSON = JSON.stringify({
	query: "queue boundaries",
	snapshot: { mode: "working" },
	truncated: false,
	text_bytes: 0,
	entries: [],
});

async function toolByName(name: string, runner: NisseRunner) {
	const tools = buildNisseTools({ run: runner });
	const tool = tools.find((candidate) => candidate.name === name);
	assert.ok(tool, `tool ${name} must exist`);
	return tool;
}

/** Run one tool's execute with a fake context. */
async function executeTool(
	name: string,
	runner: NisseRunner,
	params: unknown,
): Promise<{ text: string; details: NisseToolDetails }> {
	const tool = await toolByName(name, runner);
	const result = await tool.execute("call-1", params as never, undefined, undefined, contextStub());
	const text = result.content
		.filter((part): part is { type: "text"; text: string } => part.type === "text")
		.map((part) => part.text)
		.join("\n");
	return { text, details: result.details };
}

describe("tool registration", () => {
	it("registers exactly the four frozen tools", () => {
		const tools: unknown[] = [];
		const pi = {
			registerTool: (tool: unknown) => {
				tools.push(tool);
			},
			// The extension must be thin: no event subscriptions.
			on: () => {
				throw new Error("the nisse extension must not subscribe to events");
			},
		} as unknown as ExtensionAPI;
		nisseExtension(pi);
		const names = tools.map((tool) => (tool as { name: string }).name);
		assert.deepEqual(names, [...NISSE_TOOL_NAMES]);
	});

	it("buildNisseTools returns the four frozen names with metadata", () => {
		const tools = buildNisseTools({ run: okRunner("{}") });
		assert.deepEqual(
			tools.map((tool) => tool.name),
			[...NISSE_TOOL_NAMES],
		);
		for (const tool of tools) {
			assert.ok(tool.label.length > 0, `${tool.name} needs a label`);
			assert.ok(tool.description.length > 0, `${tool.name} needs a description`);
			assert.ok(tool.parameters, `${tool.name} needs parameters`);
		}
	});
});

describe("tool schemas", () => {
	it("nisse_review_plan takes one required plan string", () => {
		const schema = buildNisseTools({ run: okRunner("{}") }).find(
			(tool) => tool.name === "nisse_review_plan",
		)!.parameters as unknown as { properties: Record<string, unknown>; required: string[] };
		assert.deepEqual(Object.keys(schema.properties), ["plan"]);
		assert.deepEqual(schema.required, ["plan"]);
	});

	it("nisse_review_code scope is a required enum with optional scope values", () => {
		const schema = buildNisseTools({ run: okRunner("{}") }).find(
			(tool) => tool.name === "nisse_review_code",
		)!.parameters as unknown as {
			properties: { scope: { enum: string[] }; commit: unknown; range: unknown; baseRef: unknown };
			required: string[];
		};
		assert.deepEqual(schema.properties.scope.enum, ["working", "staged", "commit", "range", "branch"]);
		assert.deepEqual(schema.required, ["scope"]);
		assert.ok("commit" in schema.properties);
		assert.ok("range" in schema.properties);
		assert.ok("baseRef" in schema.properties);
	});

	it("nisse_query_project takes one required query string", () => {
		const schema = buildNisseTools({ run: okRunner("{}") }).find(
			(tool) => tool.name === "nisse_query_project",
		)!.parameters as unknown as { properties: Record<string, unknown>; required: string[] };
		assert.deepEqual(Object.keys(schema.properties), ["query"]);
		assert.deepEqual(schema.required, ["query"]);
	});

	it("nisse_feedback takes ids plus a required outcome enum", () => {
		const schema = buildNisseTools({ run: okRunner("{}") }).find(
			(tool) => tool.name === "nisse_feedback",
		)!.parameters as unknown as {
			properties: { outcome: { enum: string[] } };
			required: string[];
		};
		assert.deepEqual(Object.keys(schema.properties), ["reviewId", "findingId", "outcome"]);
		assert.deepEqual(schema.required, ["reviewId", "findingId", "outcome"]);
		assert.deepEqual(schema.properties.outcome.enum, ["fixed", "accepted", "dismissed"]);
	});

	it("every schema is JSON-serializable", () => {
		for (const tool of buildNisseTools({ run: okRunner("{}") })) {
			assert.doesNotThrow(() => {
				JSON.parse(JSON.stringify(tool.parameters));
			}, tool.name);
		}
	});
});

describe("argument building", () => {
	it("reviewPlanArgs reads the plan over stdin", () => {
		assert.deepEqual(reviewPlanArgs(), ["review", "plan", "--stdin", "--json"]);
	});

	it("reviewCodeArgs maps every scope", () => {
		assert.deepEqual(reviewCodeArgs({ scope: "working" }), [
			"review",
			"code",
			"--working",
			"--json",
		]);
		assert.deepEqual(reviewCodeArgs({ scope: "staged" }), ["review", "code", "--staged", "--json"]);
		assert.deepEqual(reviewCodeArgs({ scope: "commit", commit: "abc123" }), [
			"review",
			"code",
			"--commit",
			"abc123",
			"--json",
		]);
		assert.deepEqual(reviewCodeArgs({ scope: "range", range: "v1..v2" }), [
			"review",
			"code",
			"--range",
			"v1..v2",
			"--json",
		]);
		assert.deepEqual(reviewCodeArgs({ scope: "branch", baseRef: "main" }), [
			"review",
			"code",
			"--branch",
			"main",
			"--json",
		]);
	});

	it("reviewCodeArgs rejects scopes without their value", () => {
		assert.throws(() => reviewCodeArgs({ scope: "commit" }), /commit/);
		assert.throws(() => reviewCodeArgs({ scope: "commit", commit: "  " }), /commit/);
		assert.throws(() => reviewCodeArgs({ scope: "range" }), /range/);
		assert.throws(() => reviewCodeArgs({ scope: "branch", baseRef: "" }), /baseRef/);
	});

	it("queryArgs trims and rejects empty queries", () => {
		assert.deepEqual(queryArgs("  queue boundaries "), ["query", "queue boundaries", "--json"]);
		assert.throws(() => queryArgs("   "), /query/);
	});

	it("feedbackArgs maps every outcome", () => {
		for (const outcome of ["fixed", "accepted", "dismissed"] as const) {
			assert.deepEqual(
				feedbackArgs({ reviewId: "r1", findingId: "f1", outcome }),
				["feedback", "r1", "f1", `--${outcome}`],
			);
		}
		assert.throws(
			() => feedbackArgs({ reviewId: "r1", findingId: "f1", outcome: "maybe" as never }),
			/outcome/,
		);
		assert.throws(() => feedbackArgs({ reviewId: "", findingId: "f1", outcome: "fixed" }), /reviewId/);
		assert.throws(() => feedbackArgs({ reviewId: "r1", findingId: " ", outcome: "fixed" }), /findingId/);
	});
});

describe("success path", () => {
	it("nisse_review_plan pipes the plan over stdin and returns stdout verbatim", async () => {
		const runner = okRunner(REVIEW_JSON);
		const { text, details } = await executeTool(
			"nisse_review_plan",
			runner,
			{ plan: "# Plan\nDo the thing." },
		);
		assert.equal(text, REVIEW_JSON);
		assert.equal(runner.invocations.length, 1);
		const invocation = runner.invocations[0]!;
		assert.deepEqual(invocation.args, ["review", "plan", "--stdin", "--json"]);
		assert.equal(invocation.cwd, CWD);
		assert.equal(invocation.stdin, "# Plan\nDo the thing.");
		assert.equal(details.tool, "nisse_review_plan");
		assert.equal(details.exitCode, 0);
		assert.equal(details.error, undefined);
	});

	it("nisse_review_code maps each scope to its CLI arguments", async () => {
		const cases = [
			[{ scope: "working" }, ["review", "code", "--working", "--json"]],
			[{ scope: "staged" }, ["review", "code", "--staged", "--json"]],
			[{ scope: "commit", commit: "abc" }, ["review", "code", "--commit", "abc", "--json"]],
			[{ scope: "range", range: "v1..v2" }, ["review", "code", "--range", "v1..v2", "--json"]],
			[{ scope: "branch", baseRef: "main" }, ["review", "code", "--branch", "main", "--json"]],
		] as const;
		for (const [params, expectedArgs] of cases) {
			const runner = okRunner(REVIEW_JSON);
			await executeTool("nisse_review_code", runner, params);
			assert.deepEqual(runner.invocations[0]!.args, expectedArgs);
			assert.equal(runner.invocations[0]!.stdin, undefined);
		}
	});

	it("nisse_query_project passes the query and returns the JSON bundle", async () => {
		const runner = okRunner(QUERY_JSON);
		const { text } = await executeTool("nisse_query_project", runner, { query: "queue boundaries" });
		assert.equal(text, QUERY_JSON);
		assert.deepEqual(runner.invocations[0]!.args, ["query", "queue boundaries", "--json"]);
	});

	it("nisse_feedback returns the text report unmodified", async () => {
		const report = "nisse feedback\nreview: r1\nfinding: f1\noutcome: fixed\n";
		const runner = okRunner(report);
		const { text, details } = await executeTool("nisse_feedback", runner, {
			reviewId: "r1",
			findingId: "f1",
			outcome: "fixed",
		});
		// Tool output is the stdout text trimmed of surrounding whitespace;
		// nothing else is added, removed, or interpreted.
		assert.equal(text, "nisse feedback\nreview: r1\nfinding: f1\noutcome: fixed");
		assert.deepEqual(runner.invocations[0]!.args, ["feedback", "r1", "f1", "--fixed"]);
		// Feedback output is plain text: no JSON validation applies.
		assert.equal(details.expectsJson, false);
	});
});

describe("failure path", () => {
	it("non-zero exit reports stderr and sets details.error", async () => {
		const runner = fakeRunner({ exitCode: 1, stderr: "not inside a Git repository\n" });
		const { text, details } = await executeTool(
			"nisse_review_code",
			runner,
			{ scope: "working" },
		);
		assert.match(text, /exited with status 1/);
		assert.match(text, /not inside a Git repository/);
		assert.match(text, /nisse error: /);
		assert.match(String(details.error), /exited with status 1/);
		assert.equal(details.exitCode, 1);
	});

	it("runner errors (spawn failure, timeout) are reported", async () => {
		const runner = fakeRunner({ exitCode: null, error: "nisse timed out after 10ms" });
		const { text, details } = await executeTool("nisse_query_project", runner, { query: "x" });
		assert.match(text, /nisse failed: nisse timed out/);
		assert.equal(details.error, "nisse failed: nisse timed out after 10ms");
	});

	it("invalid tool input is reported without invoking nisse", async () => {
		const runner = okRunner("{}");
		const { text, details } = await executeTool("nisse_review_code", runner, { scope: "commit" });
		assert.match(text, /invalid tool input: `commit`/);
		assert.equal(details.args.length, 0);
		assert.equal(runner.invocations.length, 0);
	});
});

describe("JSON parsing", () => {
	it("JSON commands reject non-JSON stdout", async () => {
		const runner = okRunner("definitely not json");
		const review = await executeTool("nisse_review_plan", runner, { plan: "p" });
		assert.match(review.text, /invalid JSON/);
		const query = await executeTool("nisse_query_project", fakeRunner({ stdout: "{" }), {
			query: "q",
		});
		assert.match(query.text, /invalid JSON/);
	});

	it("any valid JSON passes, including unknown future fields", async () => {
		const runner = okRunner('{"severity":"ultra","future":[1,2,3]}');
		const { text } = await executeTool("nisse_review_plan", runner, { plan: "p" });
		assert.equal(text, '{"severity":"ultra","future":[1,2,3]}');
	});
});

describe("thin adapter invariant", () => {
	it("returns successful stdout verbatim without interpreting findings", async () => {
		const runner = okRunner(REVIEW_JSON);
		const { text } = await executeTool("nisse_review_plan", runner, { plan: "p" });
		assert.equal(text, REVIEW_JSON);
		const parsed = JSON.parse(text) as { unknown_future_field: string };
		assert.equal(parsed.unknown_future_field, "must survive verbatim passthrough");
	});

	it("the extension imports only the Pi SDK, typebox, and node builtins", async () => {
		const sourcePath = fileURLToPath(new URL("../extensions/nisse.ts", import.meta.url));
		const source = readFileSync(sourcePath, "utf8");
		const specs = [...source.matchAll(/from\s+["']([^"']+)["']/g)].map((match) => match[1]!);
		assert.ok(specs.length > 0, "expected imports to be found");
		const allowed = new Set([
			"node:child_process",
			"@earendil-works/pi-ai",
			"@earendil-works/pi-coding-agent",
			"typebox",
		]);
		for (const spec of specs) {
			assert.ok(allowed.has(spec), `unexpected import \`${spec}\`: the extension must stay thin`);
		}
	});
});

describe("configuration resolution", () => {
	it("resolveNisseBin honors NISSE_BIN and falls back to PATH", () => {
		assert.equal(resolveNisseBin({}), "nisse");
		assert.equal(resolveNisseBin({ NISSE_BIN: "  " }), "nisse");
		assert.equal(resolveNisseBin({ NISSE_BIN: " /opt/nisse/nisse " }), "/opt/nisse/nisse");
	});

	it("resolveTimeoutMs honors NISSE_TIMEOUT_MS and falls back to the default", () => {
		assert.equal(resolveTimeoutMs({}), DEFAULT_TIMEOUT_MS);
		assert.equal(resolveTimeoutMs({ NISSE_TIMEOUT_MS: "nope" }), DEFAULT_TIMEOUT_MS);
		assert.equal(resolveTimeoutMs({ NISSE_TIMEOUT_MS: "-5" }), DEFAULT_TIMEOUT_MS);
		assert.equal(resolveTimeoutMs({ NISSE_TIMEOUT_MS: "1500" }), 1500);
	});
});

const isWindows = process.platform === "win32";
const fakeBin = fileURLToPath(new URL("./fake-nisse.mjs", import.meta.url));
let subprocessCwd = "";

describe("subprocess execution (fake nisse binary)", { skip: isWindows }, () => {
	before(() => {
		chmodSync(fakeBin, 0o755);
		subprocessCwd = mkdtempSync(join(tmpdir(), "nisse-pi-"));
	});

	after(() => {
		if (subprocessCwd !== "") {
			rmSync(subprocessCwd, { recursive: true, force: true });
		}
	});

	afterEach(() => {
		delete process.env.FAKE_NISSE_EMIT;
	});

	it("the fake binary is executable", () => {
		chmodSync(fakeBin, 0o755);
	});

	it("spawnNisse runs review plan and delivers stdin", async () => {
		const planText = "# Plan\nstep 1\nstep 2\n";
		const result = await spawnNisse(
			{ args: ["review", "plan", "--stdin", "--json"], cwd: subprocessCwd, stdin: planText },
			{ bin: fakeBin },
		);
		assert.equal(result.error, undefined);
		assert.equal(result.exitCode, 0);
		const parsed = JSON.parse(result.stdout) as { echo_stdin_bytes: number };
		assert.equal(parsed.echo_stdin_bytes, Buffer.byteLength(planText, "utf8"));
	});

	it("spawnNisse captures non-zero exit and stderr", async () => {
		const result = await spawnNisse({ args: ["boom"], cwd: subprocessCwd }, { bin: fakeBin });
		assert.equal(result.exitCode, 2);
		assert.equal(result.error, undefined);
		assert.match(result.stderr, /simulated failure/);
	});

	it("spawnNisse reports a missing binary", async () => {
		const result = await spawnNisse(
			{ args: ["--version"], cwd: subprocessCwd },
			{ bin: "nisse-definitely-not-installed-xyz" },
		);
		assert.equal(result.exitCode, null);
		assert.match(String(result.error), /ENOENT/);
	});

	it("spawnNisse enforces its timeout", async () => {
		const result = await spawnNisse(
			{ args: ["--sleep-forever"], cwd: subprocessCwd },
			{ bin: fakeBin, timeoutMs: 250 },
		);
		assert.equal(result.killed, true);
		assert.match(String(result.error), /timed out after 250ms/);
	});

	it("spawnNisse aborts with its signal", async () => {
		const controller = new AbortController();
		const pending = spawnNisse(
			{ args: ["--sleep-forever"], cwd: subprocessCwd, signal: controller.signal },
			{ bin: fakeBin },
		);
		setTimeout(() => controller.abort(), 100);
		const result = await pending;
		assert.equal(result.killed, true);
		assert.equal(result.error, "nisse was aborted");
	});

	it("tools use the default runner with an injected binary", async () => {
		const tools = buildNisseTools({ bin: fakeBin });
		const planTool = tools.find((tool) => tool.name === "nisse_review_plan")!;
		const result = await planTool.execute(
			"call-1",
			{ plan: "stdin through the default runner" },
			undefined,
			undefined,
			contextStub(subprocessCwd),
		);
		const text = (result.content[0] as { type: "text"; text: string }).text;
		const parsed = JSON.parse(text) as { echo_stdin_bytes: number };
		assert.equal(parsed.echo_stdin_bytes, Buffer.byteLength("stdin through the default runner"));
		assert.equal(result.details.exitCode, 0);
	});

	it("tools surface invalid JSON from the subprocess", async () => {
		process.env.FAKE_NISSE_EMIT = "garbage";
		const tools = buildNisseTools({ bin: fakeBin });
		const queryTool = tools.find((tool) => tool.name === "nisse_query_project")!;
		const result = await queryTool.execute(
			"call-1",
			{ query: "anything" },
			undefined,
			undefined,
			contextStub(subprocessCwd),
		);
		const text = (result.content[0] as { type: "text"; text: string }).text;
		assert.match(text, /invalid JSON/);
		assert.match(String(result.details.error), /invalid JSON/);
	});

	it("NISSE_BIN selects the executable for the default runner", async () => {
		const previous = process.env.NISSE_BIN;
		process.env.NISSE_BIN = fakeBin;
		try {
			const tools = buildNisseTools();
			const feedbackTool = tools.find((tool) => tool.name === "nisse_feedback")!;
			const result = await feedbackTool.execute(
				"call-1",
				{ reviewId: "r1", findingId: "f1", outcome: "accepted" },
				undefined,
				undefined,
				contextStub(subprocessCwd),
			);
			const text = (result.content[0] as { type: "text"; text: string }).text;
			assert.match(text, /review: r1/);
			assert.match(text, /finding: f1/);
		} finally {
			if (previous === undefined) {
				delete process.env.NISSE_BIN;
			} else {
				process.env.NISSE_BIN = previous;
			}
		}
	});
});

describe("output bounds", () => {
	it("advertises bounded capture limits", () => {
		assert.ok(DEFAULT_TIMEOUT_MS > 0);
		assert.ok(MAX_OUTPUT_BYTES > 0);
	});
});
