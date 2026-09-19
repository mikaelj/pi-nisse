/**
 * Model profile resolution (plan sections 10, 11, and 18).
 *
 * Reviewer sessions never inherit a model. Nisse selects an explicitly
 * configured named model profile; the Rust core resolves those names from
 * trusted local configuration and forwards them to this host through
 * `host_config`. The host maps a spec's `model_profile` name to an explicit
 * model identifier before a session runs.
 */

import type { HostRuntime } from "./types.ts";

/** A model resolved from a named profile. */
export interface ResolvedModel {
  readonly model: string;
  readonly runtime: HostRuntime;
}

/**
 * A model profile entry from the Rust core's `host_config`.
 *
 * `model` is a `provider/model-id` identifier resolved by the trusted
 * Nisse configuration. `endpoint` is an optional explicit base URL
 * (already endpoint-trust-validated by the Rust core).
 */
export interface HostModelProfile {
  readonly model: string;
  readonly endpoint?: string;
}

/** Model profiles currently known to this host (from `host_config`). */
export type HostModelTable = ReadonlyMap<string, HostModelProfile>;

/** Validate a model profile name: non-empty, no whitespace. */
export function validateProfileName(name: string): string {
  if (name.length === 0 || /\s/.test(name)) {
    throw new Error(`invalid model profile name \`${name}\``);
  }
  return name;
}

/** Resolve a named model profile for the fake runtime (no provider). */
export function resolveFakeModel(profile: string): ResolvedModel {
  validateProfileName(profile);
  return { model: `fake:${profile}`, runtime: "fake" };
}

/**
 * Resolve a named model profile against the host's model table.
 *
 * Throws when the profile was never supplied through `host_config`:
 * reviewer sessions fail closed instead of inheriting any model.
 */
export function resolvePiModel(table: HostModelTable, profile: string): HostModelProfile {
  validateProfileName(profile);
  const resolved = table.get(profile);
  if (resolved === undefined) {
    const known = [...table.keys()].sort().join(", ");
    throw new Error(
      `model profile \`${profile}\` was not supplied by host_config` +
        (known.length > 0 ? ` (known profiles: ${known})` : " (no profiles were supplied)"),
    );
  }
  return resolved;
}
