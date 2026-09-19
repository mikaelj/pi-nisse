/**
 * Scripted Pi reviewer sessions (test mock).
 *
 * The pi reviewer runtime normally creates real isolated Pi SDK sessions.
 * Tests need the full session pipeline — spec mapping, strict report
 * parsing, the single repair attempt, timeouts — without contacting any
 * model provider. The scripted agent factory provides that: it consumes
 * scripted steps from a JSON file and records the exact agent-session
 * configuration Nisse derived from the execution spec.
 *
 * Environment variables (set by the Rust core or the test runner):
 *
 * - `NISSE_PI_SESSION_SCRIPT`: path to a JSON file
 *   `{ "sessions": [{ "steps": [ { "reply": string } | { "error": string }
 *     | { "hangMs": number } ] }] }`. One session entry is consumed per
 *   reviewer session; one step per conversation turn.
 * - `NISSE_PI_SESSION_RECORD`: path where every completed session appends
 *   one JSON line `{ config, prompts }` describing the session Nisse built.
 */

import { appendFileSync, readFileSync } from "node:fs";

import {
  ReviewerSessionError,
  SESSION_ERROR_CODES,
  type AgentSessionConfig,
  type SessionAgent,
  type SessionAgentFactory,
} from "./session.ts";
import type { ReviewerToolDef } from "./session.ts";

/** One scripted conversation step. */
export type ScriptedStep =
  | { readonly reply: string }
  | { readonly error: string }
  | { readonly hangMs: number };

/** One scripted reviewer session. */
export interface ScriptedSession {
  readonly steps: readonly ScriptedStep[];
}

/** A full session script. */
export interface SessionScript {
  readonly sessions: readonly ScriptedSession[];
}

/** A recorded scripted session (one JSON line in the record file). */
export interface RecordedSession {
  readonly config: AgentSessionConfig;
  readonly prompts: readonly string[];
}

class ScriptValidationError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ScriptValidationError";
  }
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function validateStep(value: unknown, index: number): ScriptedStep {
  if (!isPlainObject(value)) {
    throw new ScriptValidationError(`sessions[].steps[${index}] must be an object`);
  }
  const keys = Object.keys(value).sort();
  if (keys.length !== 1) {
    throw new ScriptValidationError(
      `sessions[].steps[${index}] must have exactly one of "reply", "error", "hangMs"`,
    );
  }
  const key = keys[0];
  if (key === undefined) {
    throw new ScriptValidationError(`sessions[].steps[${index}] is empty`);
  }
  const raw = value[key];
  if (key === "hangMs") {
    if (typeof raw !== "number" || !Number.isInteger(raw) || raw <= 0) {
      throw new ScriptValidationError(
        `sessions[].steps[${index}].hangMs must be a positive integer`,
      );
    }
    return { hangMs: raw };
  }
  if (typeof raw !== "string" || raw.length === 0) {
    throw new ScriptValidationError(`sessions[].steps[${index}].${key} must be a non-empty string`);
  }
  return key === "reply" ? { reply: raw } : { error: raw };
}

/** Parse and validate a session script document. */
export function parseSessionScript(text: string): SessionScript {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch (err) {
    throw new ScriptValidationError(`session script is not valid JSON: ${String(err)}`);
  }
  if (!isPlainObject(parsed) || !Array.isArray(parsed["sessions"])) {
    throw new ScriptValidationError('session script must be { "sessions": [...] }');
  }
  const sessions = (parsed["sessions"] as unknown[]).map((session, sessionIndex) => {
    if (!isPlainObject(session) || !Array.isArray(session["steps"])) {
      throw new ScriptValidationError(
        `sessions[${sessionIndex}] must be { "steps": [...] }`,
      );
    }
    const steps = (session["steps"] as unknown[]).map((step, stepIndex) =>
      validateStep(step, stepIndex),
    );
    return { steps };
  });
  return { sessions };
}

/** Read a session script from a file path (used by the host entry point). */
export function loadSessionScript(path: string): SessionScript {
  return parseSessionScript(readFileSync(path, "utf8"));
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => {
    const timer = setTimeout(resolve, ms);
    timer.unref?.();
  });
}

/**
 * Create a scripted agent factory.
 *
 * Every session consumes the next scripted entry; when the script runs out
 * the session fails deterministically. Completed sessions append their
 * configuration and prompts to `recordPath` (one JSON line each) so tests
 * can assert exactly what Nisse mapped onto the agent session.
 */
export function createScriptedAgentFactory(
  script: SessionScript,
  recordPath: string | undefined,
): SessionAgentFactory {
  let nextSession = 0;
  return async (config: AgentSessionConfig, _tools: readonly ReviewerToolDef[]) => {
    void _tools;
    const session = script.sessions[nextSession];
    nextSession += 1;
    if (session === undefined) {
      throw new ReviewerSessionError(
        SESSION_ERROR_CODES.session,
        `session script exhausted after ${script.sessions.length} session(s)`,
      );
    }
    return new ScriptedAgent(config, session.steps, recordPath);
  };
}

class ScriptedAgent implements SessionAgent {
  private readonly config: AgentSessionConfig;
  private readonly steps: readonly ScriptedStep[];
  private readonly recordPath: string | undefined;
  private readonly prompts: string[] = [];
  private nextStep = 0;
  private lastReply: string | undefined;
  private recorded = false;

  constructor(
    config: AgentSessionConfig,
    steps: readonly ScriptedStep[],
    recordPath: string | undefined,
  ) {
    this.config = config;
    this.steps = steps;
    this.recordPath = recordPath;
  }

  async prompt(text: string): Promise<void> {
    this.prompts.push(text);
    const step = this.steps[this.nextStep];
    this.nextStep += 1;
    if (step === undefined) {
      throw new Error(`scripted session has no step for prompt ${this.prompts.length}`);
    }
    if ("hangMs" in step) {
      await sleep(step.hangMs);
      return;
    }
    if ("error" in step) {
      throw new Error(step.error);
    }
    this.lastReply = step.reply;
  }

  finalText(): string | undefined {
    return this.lastReply;
  }

  abort(): void {
    // Scripted agents have nothing to abort; the session deadline races.
  }

  dispose(): void {
    if (this.recorded || this.recordPath === undefined) {
      return;
    }
    this.recorded = true;
    const record: RecordedSession = {
      config: this.config,
      prompts: this.prompts,
    };
    appendFileSync(this.recordPath, `${JSON.stringify(record)}\n`);
  }
}
