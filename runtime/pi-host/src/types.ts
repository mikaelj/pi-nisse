/**
 * Host-local shared types.
 *
 * Small enough to live beside their users; kept in one module so the
 * runtime selection and protocol-facing names stay consistent.
 */

/** Reviewer runtime selected on the command line (`--runtime`). */
export type HostRuntime = "fake" | "pi";

/** Fault injection modes for protocol tests (see docs/pi-host-protocol.md). */
export const FAULTS = [
  "startup-hang",
  "crash-on-start",
  "wrong-version",
  "ready-then-exit",
  "stderr-noise",
  "malformed-after-ready",
  "review-hang",
  "oversized-line",
  "binary-stdout",
] as const;

export type Fault = (typeof FAULTS)[number];

export function parseFault(value: string | undefined): Fault | undefined {
  if (value === undefined || value === "") {
    return undefined;
  }
  if ((FAULTS as readonly string[]).includes(value)) {
    return value as Fault;
  }
  throw new Error(
    `unknown NISSE_PI_HOST_FAULT \`${value}\`; expected one of ${FAULTS.join(", ")}`,
  );
}
