/**
 * Reviewer tool bridge (plan sections 21 and 22).
 *
 * Reviewer sessions may perform bounded supplemental investigation through
 * the four read-only tools. The host turns a tool call into a
 * `tool_request` protocol message; Rust resolves it against the exact
 * target snapshot and answers with a `tool_response`. The bridge enforces
 * per-call timeouts and deterministic cancellation; Rust enforces
 * capability grants, lookup counts, and context budgets.
 */

import { type HostToRustType } from "./protocol.ts";
import {
  validateContextBundle,
  validateToolArguments,
  type ContextBundle,
  type ReviewerTool,
} from "./structured.ts";

/** The outcome of one tool call. */
export type ToolOutcome =
  | { readonly ok: true; readonly bundle: ContextBundle }
  | { readonly ok: false; readonly error: { readonly code: string; readonly message: string } };

interface PendingCall {
  readonly reviewId: string;
  readonly resolve: (outcome: ToolOutcome) => void;
  timer: ReturnType<typeof setTimeout> | undefined;
}

export interface ToolBridgeDeps {
  /** Allocate a fresh protocol message id. */
  nextId(): string;
  /** Encode and write one protocol message to stdout. */
  send(type: HostToRustType, id: string, payload: Record<string, unknown>): void;
}

/** Correlates outbound `tool_request` messages with inbound responses. */
export class ToolBridge {
  private readonly pending = new Map<string, PendingCall>();
  private readonly deps: ToolBridgeDeps;

  constructor(deps: ToolBridgeDeps) {
    this.deps = deps;
  }

  /**
   * Issue one tool call and await Rust's answer.
   *
   * Rejects only for programming errors (invalid arguments); protocol
   * failures resolve as `{ ok: false }` outcomes so reviewer sessions can
   * degrade conservatively.
   */
  call(
    reviewId: string,
    reviewerId: string,
    tool: ReviewerTool,
    args: Record<string, unknown>,
    timeoutMs: number,
  ): Promise<ToolOutcome> {
    validateToolArguments(tool, args);
    const requestId = this.deps.nextId();
    this.deps.send("tool_request", requestId, {
      review_id: reviewId,
      reviewer_id: reviewerId,
      tool,
      arguments: args,
    });
    return new Promise<ToolOutcome>((resolve) => {
      const timer = setTimeout(() => {
        this.pending.delete(requestId);
        resolve({
          ok: false,
          error: {
            code: "tool_timeout",
            message: `\`${tool}\` was not answered within ${timeoutMs}ms`,
          },
        });
      }, timeoutMs);
      // The timer must never keep the process alive on shutdown.
      timer.unref?.();
      this.pending.set(requestId, { reviewId, resolve, timer });
    });
  }

  /** Route one decoded `tool_response` payload to its pending call. */
  handleResponse(payload: unknown): void {
    const object =
      typeof payload === "object" && payload !== null ? (payload as Record<string, unknown>) : {};
    const requestIdRaw = object["request_id"];
    if (typeof requestIdRaw !== "string" || requestIdRaw.length === 0) {
      throw new Error("tool_response payload has no valid `request_id`");
    }
    const pending = this.pending.get(requestIdRaw);
    if (pending === undefined) {
      throw new Error(`tool_response references unknown request \`${requestIdRaw}\``);
    }
    this.pending.delete(requestIdRaw);
    if (pending.timer !== undefined) {
      clearTimeout(pending.timer);
    }
    const ok = object["ok"];
    if (ok === true) {
      pending.resolve({ ok: true, bundle: validateContextBundle(object["bundle"]) });
      return;
    }
    if (ok === false) {
      const error =
        typeof object["error"] === "object" && object["error"] !== null
          ? (object["error"] as Record<string, unknown>)
          : {};
      const code = typeof error["code"] === "string" ? error["code"] : "tool_failed";
      const message =
        typeof error["message"] === "string" ? error["message"] : "tool call failed";
      pending.resolve({ ok: false, error: { code, message } });
      return;
    }
    pending.resolve({
      ok: false,
      error: { code: "malformed_response", message: "`ok` must be a boolean" },
    });
  }

  /** Fail every pending call of one review, or of all reviews. */
  cancel(reviewId: string | undefined): void {
    for (const [requestId, pending] of this.pending) {
      if (reviewId !== undefined && pending.reviewId !== reviewId) {
        continue;
      }
      this.pending.delete(requestId);
      if (pending.timer !== undefined) {
        clearTimeout(pending.timer);
      }
      pending.resolve({
        ok: false,
        error: { code: "cancelled", message: "review cancelled by the Rust core" },
      });
    }
  }
}
