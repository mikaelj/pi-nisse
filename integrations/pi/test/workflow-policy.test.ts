/**
 * Milestone 25 — Pi task workflow.
 *
 * These tests pin the frozen Main Pi workflow policy in three places at
 * once so they cannot drift apart:
 *
 *   1. the machine-readable policy fixture (fixtures/workflow-policy.json);
 *   2. the policy text Main Pi follows (skills/nisse/SKILL.md);
 *   3. the tool behavior the policy drives (extensions/nisse.ts).
 *
 * They also execute a bounded-workflow simulation — a mechanical model of
 * Main Pi following the skill — against the real tools with a scripted
 * fake nisse runner. The simulation proves the tool I/O contract alone is
 * sufficient to run the frozen loops (early exit, revision, hard stop at
 * the third attempt, trivial-task skip, feedback semantics), i.e. the
 * integration does not depend on Pi inventing its own review loop.
 *
 * Everything runs offline; no real nisse binary and no LLM is involved.
 */

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import type { ExtensionContext } from "@earendil-works/pi-coding-agent";
import { describe, it } from "node:test";
import {
	type NisseInvocation,
	type NisseProcessResult,
	type NisseRunner,
	buildNisseTools,
	feedbackArgs,
} from "../extensions/nisse.ts";

/* ------------------------------------------------------------------ */
/* Frozen policy fixture                                               */
/* ------------------------------------------------------------------ */

interface LoopPolicy {
	readonly tool: string;
	readonly max_attempts: number;
	readonly exit_condition: string;
	readonly limit_reached: string;
}

interface FeedbackPolicy {
	readonly outcomes: string[];
	readonly known_fixed: string;
	readonly known_valid_but_deferred: string;
	readonly known_invalid: string;
	readonly unknown: string;
}

interface WorkflowPolicy {
	readonly schema_version: number;
	readonly tools: string[];
	readonly severities: string[];
	readonly blocking_severities: string[];
	readonly plan_review_loop: LoopPolicy;
	readonly code_review_loop: LoopPolicy;
	readonly feedback: FeedbackPolicy;
	readonly substantial_workflow: string[];
	readonly trivial_workflow: string[];
	readonly trivial_override: string;
	readonly core_mode_invariant: string;
}

const policyPath = fileURLToPath(new URL("./fixtures/workflow-policy.json", import.meta.url));
const policy = JSON.parse(readFileSync(policyPath, "utf8")) as WorkflowPolicy;

const skillPath = fileURLToPath(new URL("../skills/nisse/SKILL.md", import.meta.url));
const skill = readFileSync(skillPath, "utf8");
/** Skill text with collapsed whitespace, so line breaks cannot hide phrases. */
const flat = skill.replace(/\s+/g, " ");

/* ------------------------------------------------------------------ */
/* Fixture self-validation                                             */
/* ------------------------------------------------------------------ */

describe("policy fixture", () => {
	it("is the frozen schema version", () => {
		assert.equal(policy.schema_version, 1);
	});

	it("freezes the severity vocabulary and its blocking subset", () => {
		assert.deepEqual(policy.severities, ["blocker", "high", "medium", "low", "info"]);
		assert.deepEqual(policy.blocking_severities, ["blocker", "high"]);
		for (const severity of policy.blocking_severities) {
			assert.ok(policy.severities.includes(severity), `${severity} must be a known severity`);
		}
	});

	it("bounds both review loops at exactly three attempts", () => {
		assert.equal(policy.plan_review_loop.max_attempts, 3);
		assert.equal(policy.code_review_loop.max_attempts, 3);
		for (const loop of [policy.plan_review_loop, policy.code_review_loop]) {
			assert.ok(policy.tools.includes(loop.tool), `${loop.tool} must be a registered tool`);
			assert.match(loop.exit_condition, /no blocker or high findings/);
			assert.ok(loop.limit_reached.length > 0);
		}
		assert.notEqual(policy.plan_review_loop.tool, policy.code_review_loop.tool);
	});

	it("freezes the feedback outcomes and the unknown rule", () => {
		assert.deepEqual(policy.feedback.outcomes, ["fixed", "accepted", "dismissed"]);
		assert.equal(policy.feedback.known_fixed, "fixed");
		assert.equal(policy.feedback.known_valid_but_deferred, "accepted");
		assert.equal(policy.feedback.known_invalid, "dismissed");
		assert.equal(policy.feedback.unknown, "submit nothing");
	});

	it("freezes the substantial and trivial phase sequences", () => {
		assert.deepEqual(policy.substantial_workflow, [
			"understand",
			"draft plan",
			"plan review",
			"implement",
			"test",
			"code review",
			"finish",
		]);
		assert.deepEqual(policy.trivial_workflow, ["implement", "test", "finish"]);
		// The substantial sequence must keep its frozen phase order.
		const order = (name: string) => policy.substantial_workflow.indexOf(name);
		assert.ok(order("plan review") < order("implement"));
		assert.ok(order("implement") < order("code review"));
		assert.equal(policy.substantial_workflow.at(-1), "finish");
		assert.match(
			policy.trivial_override,
			/explicit user instruction/,
			"the trivial override must be the explicit-user-request rule",
		);
	});

	it("freezes exactly the five Main Pi tools", () => {
		assert.deepEqual(policy.tools, [
			"nisse_review_plan",
			"nisse_review_code",
			"nisse_review_investigation",
			"nisse_query_project",
			"nisse_feedback",
		]);
	});
});

/* ------------------------------------------------------------------ */
/* Skill text alignment                                                */
/* ------------------------------------------------------------------ */

describe("skill policy text aligns with the fixture", () => {
	it("declares the nisse skill with a short always-loaded description", () => {
		assert.ok(skill.startsWith("---\n"), "SKILL.md needs YAML frontmatter");
		const frontmatter = skill.slice(4, skill.indexOf("\n---", 4));
		assert.match(frontmatter, /^name:\s*nisse$/m);
		const description = frontmatter.match(/^description:\s*(.+)$/m)?.[1] ?? "";
		assert.ok(description.length > 0, "description must exist");
		assert.ok(
			description.length <= 400,
			`permanent description must stay short (${description.length} > 400 chars)`,
		);
	});

	it("documents every frozen tool by name", () => {
		for (const tool of policy.tools) {
			assert.ok(flat.includes(tool), `skill must document \`${tool}\``);
		}
	});

	it("states both loop limits exactly as frozen", () => {
		for (const [heading, loop] of [
			["Plan-review loop", policy.plan_review_loop],
			["Code-review loop", policy.code_review_loop],
		] as const) {
			const limit = loop.max_attempts;
			assert.ok(
				flat.includes(`${heading} (max ${limit} attempts)`),
				`skill must head the ${heading} with (max ${limit} attempts)`,
			);
			assert.ok(
				flat.includes(`Every \`${loop.tool}\` call — the first included — consumes one of`),
				`skill must define what counts as one ${heading} attempt`,
			);
		}
	});

	it("gates both loops on exactly the blocking severities", () => {
		for (const severity of policy.blocking_severities) {
			assert.ok(flat.includes(severity), `skill must mention \`${severity}\``);
		}
		assert.equal(
			(flat.match(/no blocker\/high findings/g) ?? []).length,
			2,
			"both loops must exit on no blocker/high findings",
		);
	});

	it("keeps both stop-for-user rules", () => {
		assert.ok(
			flat.includes("stop before implementation"),
			"plan loop must stop before implementation at the limit",
		);
		assert.ok(
			flat.includes("do **not** claim the task is complete"),
			"code loop must refuse completion at the limit",
		);
		assert.ok(flat.includes("ask for direction"), "stops must ask the user for direction");
	});

	it("spells out the frozen substantial phase sequence", () => {
		const headings = [
			"### 1. Understand",
			"### 2. Draft a plan",
			"### 3. Plan-review loop",
			"### 4. Implement",
			"### 5. Project tests",
			"### 6. Code-review loop",
			"### 7. Finish",
		];
		for (const heading of headings) {
			assert.ok(skill.includes(heading), `skill must contain \`${heading}\``);
		}
		assert.ok(
			flat.includes("do not invent a different review loop"),
			"skill must forbid inventing a different review loop",
		);
		assert.ok(
			flat.includes("Nisse Core has no task workflow of its own"),
			"skill must state that the workflow lives here, not in Nisse Core",
		);
	});

	it("keeps the trivial path and its explicit-override rule", () => {
		assert.ok(skill.includes("## Trivial tasks"));
		assert.ok(flat.includes("understand → implement → test → finish"));
		assert.ok(flat.includes("unless the user explicitly asks for Nisse"));
		assert.ok(flat.includes("explicit user instruction to use Nisse"));
		assert.ok(flat.includes("explicit request always runs the substantial-task workflow"));
	});

	it("maps feedback outcomes exactly as frozen, including the unknown rule", () => {
		assert.ok(
			flat.includes("`fixed` — the implementation changed specifically to address the finding"),
		);
		assert.ok(flat.includes("`accepted` — the finding is valid but intentionally not fixed now"));
		assert.ok(
			flat.includes("`dismissed` — you investigated and judged it incorrect or not applicable"),
		);
		assert.ok(flat.includes("Unknown outcome — **submit nothing**"));
		assert.ok(flat.includes("idempotent"), "skill must state that submissions are idempotent");
	});

	it("names every severity Main Pi can read from a ReviewResult", () => {
		for (const severity of policy.severities) {
			assert.ok(flat.includes(severity), `skill must name \`${severity}\``);
		}
	});
});

/* ------------------------------------------------------------------ */
/* Tool behavior alignment                                             */
/* ------------------------------------------------------------------ */

describe("tool behavior aligns with the fixture", () => {
	it("registers exactly the five frozen tools in fixture order", () => {
		const tools = buildNisseTools({ run: okRunner });
		assert.deepEqual(
			tools.map((tool) => tool.name),
			policy.tools,
		);
	});

	it("accepts exactly the frozen feedback outcomes", () => {
		const schema = buildNisseTools({ run: okRunner })
			.find((tool) => tool.name === "nisse_feedback")!
			.parameters as unknown as { properties: { outcome: { enum: string[] } } };
		assert.deepEqual(schema.properties.outcome.enum, policy.feedback.outcomes);
		for (const outcome of policy.feedback.outcomes) {
			assert.deepEqual(
				feedbackArgs({ reviewId: "r1", findingId: "f1", outcome: outcome as "fixed" }),
				["feedback", "r1", "f1", `--${outcome}`],
			);
		}
	});

	it("supports the frozen loop tools as JSON review commands", () => {
		const tools = buildNisseTools({ run: okRunner });
		for (const loop of [policy.plan_review_loop, policy.code_review_loop]) {
			const tool = tools.find((candidate) => candidate.name === loop.tool);
			assert.ok(tool, `${loop.tool} must exist`);
			assert.match(tool.description, /ReviewResult JSON/);
		}
	});
});

/* ------------------------------------------------------------------ */
/* Bounded workflow simulation                                         */
/* ------------------------------------------------------------------ */

interface SimFinding {
	readonly id: string;
	readonly severity: string;
}

interface SimReview {
	readonly review_id: string;
	readonly findings: SimFinding[];
}

/** Build a ReviewResult JSON payload with findings of the given severities. */
function reviewJson(reviewId: string, severities: readonly string[]): string {
	return JSON.stringify({
		review_id: reviewId,
		kind: reviewId.startsWith("plan") ? "plan" : "code",
		created_at: "2026-01-01T00:00:00Z",
		snapshot: { mode: "working" },
		summary: "simulated review",
		findings: severities.map((severity, index) => ({
			id: `${reviewId}-f${index}`,
			impact_class: "correctness",
			severity,
			category: "sim",
			title: `simulated ${severity}`,
			claim: "simulated claim",
			explanation: "simulated explanation",
			recommendation: "simulated recommendation",
			evidence: [],
			source_finding_ids: ["s1"],
			reviewer_ids: ["quality"],
			cross_validated: true,
		})),
	});
}

/** Scripted nisse outputs; an exhausted queue fails the test (boundedness). */
interface WorkflowScript {
	planReviews?: readonly string[];
	codeReviews?: readonly string[];
}

function scriptedRunner(script: WorkflowScript): NisseRunner & { invocations: NisseInvocation[] } {
	const invocations: NisseInvocation[] = [];
	const planQueue = [...(script.planReviews ?? [])];
	const codeQueue = [...(script.codeReviews ?? [])];
	const runner: NisseRunner = async (invocation) => {
		invocations.push(invocation);
		const args = invocation.args;
		const result = (stdout: string): NisseProcessResult => ({
			args,
			stdout,
			stderr: "",
			exitCode: 0,
			killed: false,
		});
		if (args[0] === "review" && args[1] === "plan") {
			const stdout = planQueue.shift();
			assert.ok(
				stdout !== undefined,
				`plan-review attempt exceeded the frozen bound of ${policy.plan_review_loop.max_attempts}`,
			);
			return result(stdout);
		}
		if (args[0] === "review" && args[1] === "code") {
			const stdout = codeQueue.shift();
			assert.ok(
				stdout !== undefined,
				`code-review attempt exceeded the frozen bound of ${policy.code_review_loop.max_attempts}`,
			);
			return result(stdout);
		}
		if (args[0] === "feedback") {
			return result("nisse feedback: recorded (idempotent)");
		}
		throw new Error(`unexpected nisse invocation: ${args.join(" ")}`);
	};
	return Object.assign(runner, { invocations });
}

const okRunner: NisseRunner = async (invocation) => ({
	args: invocation.args,
	stdout: "{}",
	stderr: "",
	exitCode: 0,
	killed: false,
});

type SimulationStatus =
	| "finished"
	| "stopped_before_implementation"
	| "stopped_incomplete";

interface SimulationReport {
	readonly status: SimulationStatus;
	readonly planAttempts: number;
	readonly codeAttempts: number;
	readonly feedback: ReadonlyArray<{
		readonly reviewId: string;
		readonly findingId: string;
		readonly outcome: string;
	}>;
	readonly invocations: readonly NisseInvocation[];
}

interface SimulationOptions {
	readonly substantial: boolean;
	readonly userAskedForNisse?: boolean;
	readonly planText?: string;
	readonly knownOutcomes?: Readonly<Record<string, string>>;
}

function contextStub(): ExtensionContext {
	return { cwd: "/tmp/nisse-project" } as unknown as ExtensionContext;
}

/** Extract the text content of one tool result. */
function resultText(result: { content: ReadonlyArray<{ type: string; text?: string }> }): string {
	return result.content
		.filter((part): part is { type: "text"; text: string } => part.type === "text")
		.map((part) => part.text)
		.join("\n");
}

/**
 * Mechanical model of Main Pi following the skill workflow, driven through
 * the real extension tools against a scripted nisse runner. Test-only: the
 * production extension stays thin and owns no workflow state.
 */
async function simulateTask(script: WorkflowScript, options: SimulationOptions): Promise<SimulationReport> {
	const runner = scriptedRunner(script);
	const tools = buildNisseTools({ run: runner });
	const planTool = tools.find((tool) => tool.name === policy.plan_review_loop.tool)!;
	const codeTool = tools.find((tool) => tool.name === policy.code_review_loop.tool)!;
	const feedbackTool = tools.find((tool) => tool.name === "nisse_feedback")!;

	const isBlocking = (finding: SimFinding): boolean =>
		policy.blocking_severities.includes(finding.severity);

	const runReview = async (
		tool: (typeof tools)[number],
		params: unknown,
	): Promise<SimReview> => {
		const result = await tool.execute("sim", params as never, undefined, undefined, contextStub());
		return JSON.parse(resultText(result)) as SimReview;
	};

	const useNisse = options.substantial || options.userAskedForNisse === true;
	const feedback: {
		reviewId: string;
		findingId: string;
		outcome: string;
	}[] = [];
	let planAttempts = 0;
	let codeAttempts = 0;

	// Phase 1–2 (understand, draft plan): no Nisse calls.
	// Phase 3: bounded plan-review loop.
	if (useNisse) {
		let resolved = false;
		for (let attempt = 1; attempt <= policy.plan_review_loop.max_attempts; attempt += 1) {
			planAttempts = attempt;
			const review = await runReview(planTool, { plan: options.planText ?? "# plan" });
			if (!review.findings.some(isBlocking)) {
				resolved = true;
				break;
			}
			// Revise the plan for valid blocker/high findings; review again.
		}
		if (!resolved) {
			// Stop before implementation; ask the user for direction.
			return {
				status: "stopped_before_implementation",
				planAttempts,
				codeAttempts,
				feedback,
				invocations: runner.invocations,
			};
		}
	}

	// Phase 4–5 (implement, project tests): no Nisse calls.
	// Phase 6: bounded code-review loop.
	if (useNisse) {
		let resolved = false;
		for (let attempt = 1; attempt <= policy.code_review_loop.max_attempts; attempt += 1) {
			codeAttempts = attempt;
			const review = await runReview(codeTool, { scope: "working" });
			if (!review.findings.some(isBlocking)) {
				resolved = true;
				break;
			}
			for (const finding of review.findings) {
				const outcome = options.knownOutcomes?.[finding.id];
				if (outcome !== undefined) {
					await feedbackTool.execute(
						"sim",
						{ reviewId: review.review_id, findingId: finding.id, outcome: outcome as "fixed" },
						undefined,
						undefined,
						contextStub(),
					);
					feedback.push({ reviewId: review.review_id, findingId: finding.id, outcome });
				}
				// Unknown outcome: submit nothing.
			}
			// Fix valid findings, retest, review again.
		}
		if (!resolved) {
			// Do not claim completion; ask the user for direction.
			return {
				status: "stopped_incomplete",
				planAttempts,
				codeAttempts,
				feedback,
				invocations: runner.invocations,
			};
		}
	}

	// Phase 7: finish.
	return { status: "finished", planAttempts, codeAttempts, feedback, invocations: runner.invocations };
}

describe("bounded workflow simulation", () => {
	it("finishes after one clean plan review and one clean code review", async () => {
		const report = await simulateTask(
			{ planReviews: [reviewJson("plan-1", [])], codeReviews: [reviewJson("code-1", [])] },
			{ substantial: true },
		);
		assert.equal(report.status, "finished");
		assert.equal(report.planAttempts, 1);
		assert.equal(report.codeAttempts, 1);
		assert.deepEqual(report.feedback, []);
		assert.equal(report.invocations.length, 2);
	});

	it("exits the plan loop early once blocker/high findings are resolved", async () => {
		const report = await simulateTask(
			{
				planReviews: [reviewJson("plan-1", ["high"]), reviewJson("plan-2", ["medium"])],
				codeReviews: [reviewJson("code-1", [])],
			},
			{ substantial: true },
		);
		// medium does not block: the second attempt exits the loop.
		assert.equal(report.status, "finished");
		assert.equal(report.planAttempts, 2);
		assert.equal(report.codeAttempts, 1);
	});

	it("stops before implementation after three unresolved plan reviews", async () => {
		const report = await simulateTask(
			{
				planReviews: [
					reviewJson("plan-1", ["high"]),
					reviewJson("plan-2", ["blocker"]),
					reviewJson("plan-3", ["high", "medium"]),
				],
				codeReviews: [reviewJson("code-1", [])],
			},
			{ substantial: true },
		);
		assert.equal(report.status, "stopped_before_implementation");
		assert.equal(report.planAttempts, policy.plan_review_loop.max_attempts);
		assert.equal(report.codeAttempts, 0);
		// Implementation never began: no code review was ever requested.
		const requestedCodeReview = report.invocations.some(
			(invocation) => invocation.args[0] === "review" && invocation.args[1] === "code",
		);
		assert.equal(requestedCodeReview, false, "a stopped plan loop must not trigger code review");
	});

	it("stops incomplete after three unresolved code reviews without finishing", async () => {
		const report = await simulateTask(
			{
				planReviews: [reviewJson("plan-1", [])],
				codeReviews: [
					reviewJson("code-1", ["blocker"]),
					reviewJson("code-2", ["high"]),
					reviewJson("code-3", ["blocker"]),
				],
			},
			{ substantial: true, knownOutcomes: { "code-1-f0": "accepted" } },
		);
		assert.equal(report.status, "stopped_incomplete");
		assert.equal(report.planAttempts, 1);
		assert.equal(report.codeAttempts, policy.code_review_loop.max_attempts);
	});

	it("submits feedback only for known outcomes", async () => {
		const report = await simulateTask(
			{
				planReviews: [reviewJson("plan-1", [])],
				codeReviews: [reviewJson("code-1", ["high", "low"]), reviewJson("code-2", [])],
			},
			{
				substantial: true,
				knownOutcomes: { "code-1-f0": "fixed" },
			},
		);
		assert.equal(report.status, "finished");
		assert.equal(report.codeAttempts, 2);
		assert.deepEqual(report.feedback, [
			{ reviewId: "code-1", findingId: "code-1-f0", outcome: "fixed" },
		]);
		// The fixed finding produced exactly one feedback CLI call.
		const feedbackCalls = report.invocations.filter(
			(invocation) => invocation.args[0] === "feedback",
		);
		assert.deepEqual(feedbackCalls.map((invocation) => invocation.args), [
			["feedback", "code-1", "code-1-f0", "--fixed"],
		]);
	});

	it("reviews use machine-readable output and pipe the plan over stdin", async () => {
		const planText = "# Simulated plan\n1. do the thing";
		const report = await simulateTask(
			{ planReviews: [reviewJson("plan-1", [])], codeReviews: [reviewJson("code-1", [])] },
			{ substantial: true, planText },
		);
		const planCall = report.invocations.find(
			(invocation) => invocation.args[0] === "review" && invocation.args[1] === "plan",
		);
		assert.ok(planCall);
		assert.ok(planCall.args.includes("--json"));
		assert.equal(planCall.stdin, planText);
		const codeCall = report.invocations.find(
			(invocation) => invocation.args[0] === "review" && invocation.args[1] === "code",
		);
		assert.ok(codeCall);
		assert.ok(codeCall.args.includes("--json"));
		// Plan review always happens before code review.
		assert.ok(
			report.invocations.findIndex((invocation) => invocation.args[1] === "plan") <
				report.invocations.findIndex((invocation) => invocation.args[1] === "code"),
		);
	});

	it("skips every Nisse tool for a trivial task", async () => {
		const report = await simulateTask({}, { substantial: false });
		assert.equal(report.status, "finished");
		assert.equal(report.planAttempts, 0);
		assert.equal(report.codeAttempts, 0);
		assert.equal(report.invocations.length, 0);
	});

	it("runs the full workflow for a trivial task when the user explicitly asks for Nisse", async () => {
		const report = await simulateTask(
			{ planReviews: [reviewJson("plan-1", [])], codeReviews: [reviewJson("code-1", [])] },
			{ substantial: false, userAskedForNisse: true },
		);
		assert.equal(report.status, "finished");
		assert.equal(report.planAttempts, 1);
		assert.equal(report.codeAttempts, 1);
		assert.equal(report.invocations.length, 2);
	});
});
