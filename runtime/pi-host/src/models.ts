/**
 * Model profile resolution (plan sections 10 and 11).
 *
 * Reviewer sessions never inherit a model. Nisse selects an explicitly
 * configured named model profile, and the host resolves that name to an
 * explicit model identifier before a session runs. In this milestone only
 * the fake runtime performs resolution; real Pi SDK model selection is
 * integrated with reviewer execution in the next milestone.
 */

import type { HostRuntime } from "./types.ts";

/** A model resolved from a named profile. */
export interface ResolvedModel {
  readonly model: string;
  readonly runtime: HostRuntime;
}

/** Validate a model profile name: non-empty, no whitespace. */
export function validateProfileName(name: string): string {
  if (name.length === 0 || /\s/.test(name)) {
    throw new Error(`invalid model profile name \`${name}\``);
  }
  return name;
}

/**
 * Resolve a named model profile for the given runtime.
 *
 * - `fake`: a deterministic local identifier (`fake:<profile>`); no
 *   provider is contacted.
 * - `pi`: real Pi SDK model selection; implemented with isolated reviewer
 *   sessions in the next milestone and deliberately fails closed until
 *   then.
 */
export function resolveModelProfile(profile: string, runtime: HostRuntime): ResolvedModel {
  validateProfileName(profile);
  switch (runtime) {
    case "fake":
      return { model: `fake:${profile}`, runtime };
    case "pi":
      throw new Error(
        `pi runtime model resolution for profile \`${profile}\` is not implemented yet`,
      );
  }
}
