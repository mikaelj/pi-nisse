/**
 * Model capability resolution (protocol version 2).
 *
 * Every `host_config` is answered with one `model_capabilities` message:
 * for each configured profile, the resolved `provider/model-id` and the
 * thinking levels its Pi catalog entry supports. The levels are derived
 * with pi's own `getSupportedThinkingLevels` semantics:
 *
 * - a non-reasoning model supports only `off`;
 * - a reasoning model supports every pi level whose
 *   `thinkingLevelMap[level]` is not `null`, where `xhigh`/`max`
 *   additionally require the key to be present (`!== undefined`) —
 *   omitted keys pass through for the remaining levels.
 *
 * The derivation is pinned to the shared golden fixture
 * (`fixtures/thinking-levels.golden.json`), which is also consumed by
 * the Rust suite.
 */

import type { ThinkingLevel } from "./structured.ts";

/** pi's `EXTENDED_THINKING_LEVELS` order (weaker → stronger). */
export const PI_THINKING_LEVELS: readonly ThinkingLevel[] = [
  "off",
  "minimal",
  "low",
  "medium",
  "high",
  "xhigh",
  "max",
];

/** The model shape this derivation needs from the Pi catalog. */
export interface CapabilityModel {
  readonly reasoning?: boolean;
  readonly thinkingLevelMap?: Readonly<
    Partial<Record<ThinkingLevel, string | null>>
  >;
}

/** One resolved profile of the `model_capabilities` reply. */
export interface ModelCapabilityEntry {
  readonly model: string;
  readonly supported_thinking_levels: readonly ThinkingLevel[];
}

/** The `model_capabilities` payload. */
export interface ModelCapabilities {
  readonly host_config_id: string;
  readonly profiles: ReadonlyMap<string, ModelCapabilityEntry>;
  readonly unresolved: ReadonlyMap<string, string>;
}

/** Derive the supported thinking levels of one model (pi semantics). */
export function supportedThinkingLevels(model: CapabilityModel): ThinkingLevel[] {
  if (!model.reasoning) {
    return ["off"];
  }
  return PI_THINKING_LEVELS.filter((level) => {
    const mapped = model.thinkingLevelMap?.[level];
    if (mapped === null) {
      return false;
    }
    if (level === "xhigh" || level === "max") {
      return mapped !== undefined;
    }
    return true;
  });
}

/** The permissive test-double entry: every level is supported. */
export function permissiveEntry(model: string): ModelCapabilityEntry {
  return { model, supported_thinking_levels: [...PI_THINKING_LEVELS] };
}
