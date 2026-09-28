/**
 * Nisse Main Pi extension (Nisse plan sections 62–63, milestone 24).
 *
 * This extension is a deliberately thin adapter: every tool is one
 * structured subprocess call to the `nisse` CLI using its
 * machine-readable output (`--json` where the frozen CLI offers it).
 * All Nisse business logic — repository discovery, snapshots, routing,
 * juries, finding validation, learning — lives in the Rust core. The
 * extension never interprets, filters, ranks, or rewrites Nisse output;
 * stdout of a successful command is returned verbatim.
 *
 * The nisse binary is resolved from the `NISSE_BIN` environment
 * variable (trusted local configuration) and defaults to `nisse` on
 * PATH. The subprocess timeout defaults to 20 minutes and can be
 * overridden with `NISSE_TIMEOUT_MS` (positive integer milliseconds).
 */

import type { AgentToolResult, ExtensionAPI, ToolDefinition } from "@earendil-works/pi-coding-agent";
import { defineTool } from "@earendil-works/pi-coding-agent";
import { StringEnum } from "@earendil-works/pi-ai";
import { spawn } from "node:child_process";
import crypto from "node:crypto";
import { mkdtempSync, rmSync, statSync, writeFileSync } from "node:fs";
import { isAbsolute } from "node:path";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { TSchema } from "typebox";
import { Type } from "typebox";

/** The frozen Main Pi tool names (Nisse plan section 62). */
export const NISSE_TOOL_NAMES = [
	"nisse_review_plan",
	"nisse_review_code",
	"nisse_review_investigation",
	"nisse_query_project",
	"nisse_feedback",
] as const;

export type NisseToolName = (typeof NISSE_TOOL_NAMES)[number];

/** Environment variable selecting the nisse executable. */
export const NISSE_BIN_ENV = "NISSE_BIN";
/** Environment variable selecting the subprocess timeout (ms). */
export const NISSE_TIMEOUT_ENV = "NISSE_TIMEOUT_MS";

/** Default subprocess timeout: jury reviews run several reviewer sessions. */
export const DEFAULT_TIMEOUT_MS: number = 20 * 60 * 1000;
/** Cap on captured stdout/stderr per stream; Nisse output is bounded JSON. */
export const MAX_OUTPUT_BYTES: number = 16 * 1024 * 1024;

/** Invalid tool input (for example a missing scope value). */
export class NisseInputError extends Error {
	constructor(message: string) {
		super(message);
		this.name = "NisseInputError";
	}
}

/** One nisse CLI invocation. */
export interface NisseInvocation {
	readonly args: readonly string[];
	readonly cwd: string;
	readonly stdin?: string | undefined;
	readonly signal?: AbortSignal | undefined;
}

/** Result of one nisse CLI invocation. */
export interface NisseProcessResult {
	readonly args: readonly string[];
	readonly stdout: string;
	readonly stderr: string;
	readonly exitCode: number | null;
	/** True when the runner killed the process (timeout, abort, output cap). */
	readonly killed: boolean;
	/** Set when the process could not produce a usable result. */
	readonly error?: string | undefined;
}

/** Executes one nisse CLI invocation. */
export type NisseRunner = (invocation: NisseInvocation) => Promise<NisseProcessResult>;

/** Resolve the nisse executable: `NISSE_BIN` when set, else `nisse`. */
export function resolveNisseBin(env: { NISSE_BIN?: string | undefined }): string {
	const configured = env[NISSE_BIN_ENV]?.trim();
	return configured !== undefined && configured !== "" ? configured : "nisse";
}

/** Resolve the subprocess timeout from `NISSE_TIMEOUT_MS` (fallback: default). */
export function resolveTimeoutMs(env: { NISSE_TIMEOUT_MS?: string | undefined }): number {
	const raw = env[NISSE_TIMEOUT_ENV]?.trim();
	if (raw === undefined || raw === "") {
		return DEFAULT_TIMEOUT_MS;
	}
	const parsed = Number.parseInt(raw, 10);
	if (!Number.isFinite(parsed) || parsed <= 0) {
		return DEFAULT_TIMEOUT_MS;
	}
	return parsed;
}

/** Options overriding executable/timeout resolution for one spawn. */
export interface NisseSpawnOptions {
	readonly bin?: string | undefined;
	readonly timeoutMs?: number | undefined;
}

/**
 * Default runner: spawn the nisse CLI and capture its output.
 *
 * The result never rejects; process-level failures (binary not found,
 * timeout, abort, oversized output) are reported through `error`.
 */
export function spawnNisse(
	invocation: NisseInvocation,
	options: NisseSpawnOptions = {},
): Promise<NisseProcessResult> {
	const bin = options.bin ?? resolveNisseBin(process.env);
	const timeoutMs = options.timeoutMs ?? resolveTimeoutMs(process.env);

	return new Promise<NisseProcessResult>((resolve) => {
		let child: ReturnType<typeof spawn>;
		try {
			child = spawn(bin, invocation.args, {
				cwd: invocation.cwd,
				stdio: ["pipe", "pipe", "pipe"],
			});
		} catch (error) {
			resolve(failedSpawn(invocation.args, errorMessage(error)));
			return;
		}

		let stdout = "";
		let stderr = "";
		let stdoutBytes = 0;
		let stderrBytes = 0;
		let failure: string | undefined;
		let killedByUs = false;
		let settled = false;

		const capture = (channel: "stdout" | "stderr", chunk: Buffer): void => {
			const text = chunk.toString("utf8");
			if (channel === "stdout") {
				stdout += text;
				stdoutBytes += chunk.length;
			} else {
				stderr += text;
				stderrBytes += chunk.length;
			}
			if (
				failure === undefined &&
				(stdoutBytes > MAX_OUTPUT_BYTES || stderrBytes > MAX_OUTPUT_BYTES)
			) {
				failure = `nisse produced more than ${MAX_OUTPUT_BYTES} bytes of output`;
				killedByUs = true;
				child.kill();
			}
		};
		child.stdout?.on("data", (chunk: Buffer) => capture("stdout", chunk));
		child.stderr?.on("data", (chunk: Buffer) => capture("stderr", chunk));

		if (invocation.stdin !== undefined) {
			// The process may exit before reading all of stdin; an EPIPE
			// here must not crash the extension.
			child.stdin?.on("error", () => {});
			child.stdin?.end(invocation.stdin, "utf8");
		}

		const timer = setTimeout(() => {
			failure = `nisse timed out after ${timeoutMs}ms`;
			killedByUs = true;
			child.kill();
		}, timeoutMs);

		const onAbort = (): void => {
			failure = "nisse was aborted";
			killedByUs = true;
			child.kill();
		};
		invocation.signal?.addEventListener("abort", onAbort, { once: true });

		const finish = (exitCode: number | null): void => {
			if (settled) {
				return;
			}
			settled = true;
			clearTimeout(timer);
			invocation.signal?.removeEventListener("abort", onAbort);
			resolve({
				args: invocation.args,
				stdout,
				stderr,
				exitCode,
				killed: killedByUs,
				...(failure !== undefined && { error: failure }),
			});
		};

		child.on("error", (error) => {
			if (failure === undefined) {
				failure = errorMessage(error);
			}
			finish(null);
		});
		child.on("close", (code) => finish(code));
	});
}

function failedSpawn(args: readonly string[], message: string): NisseProcessResult {
	return {
		args,
		stdout: "",
		stderr: "",
		exitCode: null,
		killed: false,
		error: `failed to start nisse: ${message}`,
	};
}

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

function requiredText(value: string | undefined, field: string): string {
	const trimmed = value?.trim();
	if (trimmed === undefined || trimmed === "") {
		throw new NisseInputError(`\`${field}\` must be a non-empty string`);
	}
	return trimmed;
}

/** CLI arguments for `nisse review plan` (plan text travels over stdin). */
export function reviewPlanArgs(): string[] {
	return ["review", "plan", "--stdin", "--json"];
}

/** Code-review scope input of `nisse_review_code`. */
export interface ReviewCodeScopeInput {
	readonly scope: "working" | "staged" | "commit" | "range" | "branch";
	readonly commit?: string | undefined;
	readonly range?: string | undefined;
	readonly baseRef?: string | undefined;
}

/** CLI arguments for `nisse review code` in the requested scope. */
export interface InvestigationInput {
	readonly task: string;
	readonly result: string;
}

/**
 * Short random token for temp-file uniqueness (cryptographic source).
 */
function randomToken(): string {
	return crypto.randomUUID().replace(/-/g, "").slice(0, 12);
}

/**
 * Resolve one investigation input to CLI args. A leading `@` passes
 * through unchanged (relative values resolve inside the CLI against the
 * session cwd, which matches `base` here); a bare value naming an
 * existing file (resolved against `base`) passes through; literal text
 * is written to a unique temp file inside `dir` and referenced as
 * `@file` (absolute, so the CLI's cwd cannot diverge).
 */
function investigationInputArgs(
	value: string,
	name: string,
	dir: string,
	base: string,
): { args: string[]; tempFile?: string } {
	const looksLikeFile = (candidate: string): boolean => {
		try {
			return statSync(candidate).isFile();
		} catch {
			return false;
		}
	};
	if (value.startsWith("@")) {
		return { args: [`--${name}`, value] };
	}
	const direct = isAbsolute(value) ? value : join(base, value);
	if (looksLikeFile(direct)) {
		return { args: [`--${name}`, direct] };
	}
	const tempFile = join(dir, `nisse-${name}-${randomToken()}.txt`);
	writeFileSync(tempFile, value, "utf8");
	return { args: [`--${name}`, `@${tempFile}`], tempFile };
}

/**
 * Build `review investigation` args with the temp-file lifecycle frozen
 * by PLAN_INVESTIGATION.md: a unique per-invocation temp directory,
 * files fully written before the CLI spawns, and cleanup strictly after
 * the subprocess settles (success, failure, timeout, or abort — the
 * awaited promise).
 */
export async function reviewInvestigationArgs<T>(
	input: InvestigationInput,
	cwd: string,
	buildAndRun: (args: string[]) => Promise<T>,
): Promise<T> {
	const dir = mkdtempSync(join(tmpdir(), "nisse-investigation-"));
	const tempFiles: string[] = [];
	try {
		const task = investigationInputArgs(input.task, "task", dir, cwd);
		const result = investigationInputArgs(input.result, "result", dir, cwd);
		if (task.tempFile !== undefined) {
			tempFiles.push(task.tempFile);
		}
		if (result.tempFile !== undefined) {
			tempFiles.push(result.tempFile);
		}
		// Temp-file paths are absolute; user @/bare paths pass through
		// exactly as the CLI documents them.
		const args = [
			"review",
			"investigation",
			...task.args,
			...result.args,
			"--json",
		];
		return await buildAndRun(args);
	} finally {
		for (const file of tempFiles) {
			try {
				rmSync(file, { force: true });
			} catch {
				// Best-effort cleanup.
			}
		}
		rmSync(dir, { recursive: true, force: true });
	}
}

export function reviewCodeArgs(input: ReviewCodeScopeInput): string[] {
	const args = ["review", "code"];
	switch (input.scope) {
		case "working":
			args.push("--working");
			break;
		case "staged":
			args.push("--staged");
			break;
		case "commit":
			args.push("--commit", requiredText(input.commit, "commit"));
			break;
		case "range":
			args.push("--range", requiredText(input.range, "range"));
			break;
		case "branch":
			args.push("--branch", requiredText(input.baseRef, "baseRef"));
			break;
		default:
			throw new NisseInputError(
				`unknown scope \`${String(input.scope)}\` (working, staged, commit, range, or branch)`,
			);
	}
	args.push("--json");
	return args;
}

/** CLI arguments for `nisse query`. */
export function queryArgs(query: string): string[] {
	return ["query", requiredText(query, "query"), "--json"];
}

/** Known finding outcomes (Nisse plan section 49). */
export type FeedbackOutcome = "fixed" | "accepted" | "dismissed";

/** Feedback input of `nisse_feedback`. */
export interface FeedbackInput {
	readonly reviewId: string;
	readonly findingId: string;
	readonly outcome: FeedbackOutcome;
}

const FEEDBACK_OUTCOMES: readonly FeedbackOutcome[] = ["fixed", "accepted", "dismissed"];

/** CLI arguments for `nisse feedback`. */
export function feedbackArgs(input: FeedbackInput): string[] {
	const reviewId = requiredText(input.reviewId, "reviewId");
	const findingId = requiredText(input.findingId, "findingId");
	const outcome = FEEDBACK_OUTCOMES.find((candidate) => candidate === input.outcome);
	if (outcome === undefined) {
		throw new NisseInputError(
			"`outcome` must be one of fixed, accepted, or dismissed",
		);
	}
	return ["feedback", reviewId, findingId, `--${outcome}`];
}

/** Structured details every Nisse tool attaches to its result. */
export interface NisseToolDetails {
	readonly tool: NisseToolName;
	readonly args: readonly string[];
	readonly exitCode: number | null;
	readonly durationMs: number;
	readonly expectsJson: boolean;
	readonly error?: string | undefined;
}

interface ExecuteContext {
	readonly cwd: string;
	readonly stdin?: string | undefined;
	readonly signal?: AbortSignal | undefined;
}

/**
 * Shared tool body: build arguments, run nisse, validate machine-readable
 * output, and return the successful stdout verbatim. Nisse results are
 * never transformed here.
 */
async function executeNisse(
	tool: NisseToolName,
	buildArgs: () => string[],
	run: NisseRunner,
	context: ExecuteContext,
	expectsJson: boolean,
): Promise<AgentToolResult<NisseToolDetails>> {
	const startedAt = Date.now();

	let args: string[];
	try {
		args = buildArgs();
	} catch (error) {
		if (error instanceof NisseInputError) {
			return failure(tool, [], startedAt, expectsJson, `invalid tool input: ${error.message}`);
		}
		throw error;
	}

	const result = await run({
		args,
		cwd: context.cwd,
		stdin: context.stdin,
		signal: context.signal,
	});

	if (result.error !== undefined) {
		return failure(
			tool,
			args,
			startedAt,
			expectsJson,
			`nisse failed: ${result.error}${stderrSuffix(result.stderr)}`,
			result.exitCode,
		);
	}
	if (result.exitCode !== 0) {
		return failure(
			tool,
			args,
			startedAt,
			expectsJson,
			`nisse exited with status ${result.exitCode}${stderrSuffix(result.stderr)}`,
			result.exitCode,
		);
	}
	if (expectsJson) {
		try {
			JSON.parse(result.stdout);
		} catch {
			return failure(
				tool,
				args,
				startedAt,
				expectsJson,
				"nisse emitted invalid JSON on stdout",
				result.exitCode,
			);
		}
	}

	return {
		content: [{ type: "text", text: result.stdout.trim() }],
		details: {
			tool,
			args,
			exitCode: result.exitCode,
			durationMs: Date.now() - startedAt,
			expectsJson,
		},
	};
}

function failure(
	tool: NisseToolName,
	args: readonly string[],
	startedAt: number,
	expectsJson: boolean,
	message: string,
	exitCode: number | null = null,
): AgentToolResult<NisseToolDetails> {
	return {
		content: [{ type: "text", text: `nisse error: ${message}` }],
		details: {
			tool,
			args,
			exitCode,
			durationMs: Date.now() - startedAt,
			expectsJson,
			error: message,
		},
	};
}

function stderrSuffix(stderr: string): string {
	const trimmed = stderr.trim();
	return trimmed === "" ? "" : `\n${trimmed}`;
}

const ReviewPlanParams = Type.Object({
	plan: Type.String({
		description:
			"Complete text of the implementation plan to review. Nisse reviews it against the current project snapshot.",
	}),
});

const ReviewCodeParams = Type.Object({
	scope: StringEnum(["working", "staged", "commit", "range", "branch"] as const, {
		description:
			"Which changes to review. working: HEAD vs final working tree (staged + unstaged + untracked). staged: HEAD vs index. commit: one commit vs its first parent (requires `commit`). range: `<base>..<head>` trees (requires `range`). branch: merge-base(baseRef, HEAD) vs HEAD (requires `baseRef`).",
	}),
	commit: Type.Optional(
		Type.String({ description: "Commit-ish to review (required when scope is `commit`)." }),
	),
	range: Type.Optional(
		Type.String({ description: "Range `<base>..<head>` (required when scope is `range`)." }),
	),
	baseRef: Type.Optional(
		Type.String({
			description: "Base ref for the merge-base comparison (required when scope is `branch`).",
		}),
	),
});

const QueryParams = Type.Object({
	query: Type.String({
		description:
			"Free-text project query: a symbol name, topic, path, or question about the indexed project.",
	}),
});

const FeedbackParams = Type.Object({
	reviewId: Type.String({
		description: "Review identifier from the ReviewResult (`review_id`).",
	}),
	findingId: Type.String({
		description: "Finding identifier from the ReviewResult (`findings[].id`).",
	}),
	outcome: StringEnum(["fixed", "accepted", "dismissed"] as const, {
		description:
			"fixed: the implementation changed specifically to address the finding. accepted: valid but intentionally not fixed now. dismissed: investigated and judged incorrect or not applicable. Submit nothing when the outcome is unclear.",
	}),
});

/** Dependencies injected for tests; defaults spawn the real nisse CLI. */
export interface NisseToolDependencies {
	readonly run?: NisseRunner | undefined;
	readonly bin?: string | undefined;
	readonly timeoutMs?: number | undefined;
}

/** A Nisse tool registration. */
export type NisseToolRegistration = ToolDefinition<TSchema, NisseToolDetails>;

/** Build the four frozen Nisse tools (Nisse plan section 62). */
export function buildNisseTools(deps: NisseToolDependencies = {}): NisseToolRegistration[] {
	const run: NisseRunner =
		deps.run ??
		((invocation) => spawnNisse(invocation, { bin: deps.bin, timeoutMs: deps.timeoutMs }));

	return [
		defineTool({
			name: "nisse_review_plan",
			label: "Nisse plan review",
			description:
				"Run a Nisse reviewer-jury review of an implementation plan before implementing it. Pass the complete plan text. Returns Nisse ReviewResult JSON: review_id, summary, findings[] with id, severity (blocker|high|medium|low|info), impact_class, title, claim, explanation, recommendation, evidence. Follow the nisse skill workflow for blocker/high findings.",
			promptSnippet: "Review an implementation plan with the Nisse jury before implementing.",
			parameters: ReviewPlanParams,
			async execute(_toolCallId, params, signal, _onUpdate, ctx) {
				return executeNisse(
					"nisse_review_plan",
					reviewPlanArgs,
					run,
					{ cwd: ctx.cwd, stdin: params.plan, signal },
					true,
				);
			},
		}),
		defineTool({
			name: "nisse_review_code",
			label: "Nisse code review",
			description:
				"Run a Nisse reviewer-jury review of code changes after implementing and passing project tests. Default scope `working` reviews HEAD vs the final working tree. Returns ReviewResult JSON like nisse_review_plan; use review_id and findings[].id for feedback.",
			promptSnippet: "Review code changes with the Nisse jury after tests pass.",
			parameters: ReviewCodeParams,
			async execute(_toolCallId, params, signal, _onUpdate, ctx) {
				return executeNisse(
					"nisse_review_code",
					() => reviewCodeArgs(params),
					run,
					{ cwd: ctx.cwd, signal },
					true,
				);
			},
		}),
		defineTool({
			name: "nisse_review_investigation",
			label: "Nisse investigation review",
			description:
				"Run a Nisse reviewer-jury review of investigation results (PLAN_INVESTIGATION.md): claim review (supported / partially supported / contradicted / insufficiently supported / not applicable / outside scope, with confidence tiers) plus coverage review against the task. Use after an investigation/research task produced conclusions that will drive action. Returns InvestigationResult JSON; claim ids and coverage-gap ids are stable within this review.",
			promptSnippet:
				"Test whether investigation results hold up: claim scrutiny plus coverage review.",
			parameters: Type.Object({
				task: Type.String({
					description:
						"The effective task the investigation was supposed to answer (literal text, or a path — a leading `@` forces file interpretation).",
				}),
				result: Type.String({
					description:
						"The investigation output to review (literal text, or a path — a leading `@` forces file interpretation).",
				}),
			}),
			async execute(_toolCallId, params, signal, _onUpdate, ctx) {
				return reviewInvestigationArgs(params, ctx.cwd, (args) =>
					executeNisse(
						"nisse_review_investigation",
						() => args,
						run,
						{ cwd: ctx.cwd, signal },
						true,
					),
				);
			},
		}),
		defineTool({
			name: "nisse_query_project",
			label: "Nisse project query",
			description:
				"Query Nisse's deterministic project index for the current working-tree snapshot: source symbols and relations, project facts, lessons, and built-in practices. No LLM review runs; useful for investigating the project and checking finding evidence. Returns a JSON context bundle.",
			promptSnippet: "Query indexed project knowledge (symbols, facts, lessons).",
			parameters: QueryParams,
			async execute(_toolCallId, params, signal, _onUpdate, ctx) {
				return executeNisse(
					"nisse_query_project",
					() => queryArgs(params.query),
					run,
					{ cwd: ctx.cwd, signal },
					true,
				);
			},
		}),
		defineTool({
			name: "nisse_feedback",
			label: "Nisse feedback",
			description:
				"Record Main Pi's known outcome for one Nisse finding: fixed, accepted, or dismissed (see the nisse skill). Submit only outcomes you actually know; submit nothing when unclear. Repeated submissions are idempotent.",
			promptSnippet: "Record a known outcome (fixed/accepted/dismissed) for a Nisse finding.",
			parameters: FeedbackParams,
			async execute(_toolCallId, params, signal, _onUpdate, ctx) {
				return executeNisse(
					"nisse_feedback",
					() => feedbackArgs(params),
					run,
					{ cwd: ctx.cwd, signal },
					false,
				);
			},
		}),
	];
}

/**
 * Extension entry point: register the four Nisse tools.
 *
 * The extension owns no state and subscribes to no events; the nisse
 * skill (skills/nisse/SKILL.md) owns the bounded workflow policy.
 */
export default function nisseExtension(pi: ExtensionAPI): void {
	for (const tool of buildNisseTools()) {
		pi.registerTool(tool);
	}
}
