/**
 * Nisse Pi review host protocol (plan sections 20 and 21).
 *
 * The Rust core and this host communicate using bidirectional JSONL over
 * stdin/stdout. stdout carries protocol messages only; all logs go to
 * stderr. Every message is one JSON object per line:
 *
 *     { "protocol_version": 1, "id": "...", "type": "...", ...payload }
 *
 * The vocabularies below are the complete frozen sets from plan section 21;
 * every entry is active in this host version. Payload validation of
 * inbound messages is strict — malformed payloads are refused
 * deterministically (`host_error{code="invalid_payload"}`) and unknown
 * type names are `unknown_message`.
 */

/** Protocol version of this host (plan section 21: version 1). */
export const PROTOCOL_VERSION = 1;

/** Message types the Rust core may send to this host, in frozen order. */
export const RUST_TO_HOST_TYPES = [
  "host_config",
  "start_review",
  "start_fact_verification",
  "start_fact_synthesis",
  "start_domain_description",
  "start_finding_equivalence_check",
  "tool_response",
  "cancel",
] as const;

/** Message types this host may send to the Rust core, in frozen order. */
export const HOST_TO_RUST_TYPES = [
  "host_ready",
  "reviewer_started",
  "reviewer_completed",
  "reviewer_failed",
  "tool_request",
  "fact_verification_completed",
  "fact_synthesis_completed",
  "domain_description_completed",
  "finding_equivalence_completed",
  "host_error",
] as const;

export type RustToHostType = (typeof RUST_TO_HOST_TYPES)[number];
export type HostToRustType = (typeof HOST_TO_RUST_TYPES)[number];

/** Stable error codes this host reports through `host_error`. */
export const ERROR_CODES = {
  malformedMessage: "malformed_message",
  unsupportedProtocolVersion: "unsupported_protocol_version",
  unknownMessage: "unknown_message",
  unsupportedMessage: "unsupported_message",
  invalidPayload: "invalid_payload",
  unknownRequest: "unknown_request",
  internal: "internal",
} as const;

/** A decoded inbound message: envelope fields plus the payload object. */
export interface InboundMessage {
  readonly id: string;
  readonly type: RustToHostType;
  readonly payload: Record<string, unknown>;
}

/** Protocol-level decoding failure carrying a stable error code. */
export class ProtocolError extends Error {
  readonly code: string;

  constructor(code: string, message: string) {
    super(message);
    this.name = "ProtocolError";
    this.code = code;
  }
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * Decode one stdin line into an {@link InboundMessage}.
 *
 * Throws {@link ProtocolError} with a stable code when the line is not a
 * valid protocol envelope; the caller reports it as `host_error`.
 */
export function decodeIncomingMessage(line: string): InboundMessage {
  let parsed: unknown;
  try {
    parsed = JSON.parse(line);
  } catch (err) {
    throw new ProtocolError(
      ERROR_CODES.malformedMessage,
      `line is not valid JSON: ${String(err)}`,
    );
  }
  if (!isPlainObject(parsed)) {
    throw new ProtocolError(
      ERROR_CODES.malformedMessage,
      "message is not a JSON object",
    );
  }
  const version = parsed["protocol_version"];
  if (typeof version !== "number" || !Number.isInteger(version) || version < 0) {
    throw new ProtocolError(
      ERROR_CODES.malformedMessage,
      "`protocol_version` is not a non-negative integer",
    );
  }
  if (version !== PROTOCOL_VERSION) {
    throw new ProtocolError(
      ERROR_CODES.unsupportedProtocolVersion,
      `unsupported protocol version ${version}; expected ${PROTOCOL_VERSION}`,
    );
  }
  const id = parsed["id"];
  if (typeof id !== "string" || id.length === 0) {
    throw new ProtocolError(ERROR_CODES.malformedMessage, "`id` is not a non-empty string");
  }
  const type = parsed["type"];
  if (typeof type !== "string") {
    throw new ProtocolError(ERROR_CODES.malformedMessage, "`type` is not a string");
  }
  if (!(RUST_TO_HOST_TYPES as readonly string[]).includes(type)) {
    throw new ProtocolError(ERROR_CODES.unknownMessage, `unknown message type \`${type}\``);
  }

  const payload: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(parsed)) {
    if (key !== "protocol_version" && key !== "id" && key !== "type") {
      payload[key] = value;
    }
  }
  return { id, type: type as RustToHostType, payload };
}

/** Encode one outbound message (envelope plus flattened payload). */
export function encodeMessage(
  type: HostToRustType,
  id: string,
  payload: Record<string, unknown>,
): string {
  return JSON.stringify({
    protocol_version: PROTOCOL_VERSION,
    id,
    type,
    ...payload,
  });
}
