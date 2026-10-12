/**
 * Real Pi SDK reviewer sessions (plan section 20).
 *
 * Maps an explicit {@link AgentSessionConfig} onto one isolated Pi SDK
 * `AgentSession`:
 *
 * - in-memory session (never persisted, disposed after the review);
 * - explicit model and thinking level — reviewer sessions never inherit a
 *   model from anywhere;
 * - explicit system instructions (instructions + checklist + output
 *   contract) with every discovery channel disabled: no extensions, no
 *   skills, no prompt templates, no AGENTS/context files, no themes;
 * - no built-in coding tools; the only tools are Nisse's read-only lookup
 *   tools bridged to the Rust core;
 * - in-memory settings (user/project settings are not consulted).
 *
 * The SDK is imported dynamically so the host process still starts and the
 * fake/scripted runtimes still work where the SDK is not installed; real
 * reviewer execution then fails closed with a typed error.
 */

import type {
  AgentSession,
  CreateAgentSessionOptions,
  ModelRuntime,
  ToolDefinition,
} from "@earendil-works/pi-coding-agent";

import {
  ReviewerSessionError,
  SESSION_ERROR_CODES,
  type AgentSessionConfig,
  type SessionAgent,
  type SessionAgentFactory,
  type ReviewerToolDef,
} from "./session.ts";

/** Type-only view of the SDK module (erased at compile time). */
type SdkModule = typeof import("@earendil-works/pi-coding-agent");

let sdkPromise: Promise<SdkModule> | undefined;
let modelRuntimePromise: Promise<ModelRuntime> | undefined;

/** Load the Pi coding-agent SDK once per host process. */
function loadSdk(): Promise<SdkModule> {
  sdkPromise ??= import("@earendil-works/pi-coding-agent").catch((err: unknown) => {
    sdkPromise = undefined;
    throw new Error(
      `failed to load @earendil-works/pi-coding-agent: ${
        err instanceof Error ? err.message : String(err)
      } (install runtime/pi-host dependencies to run real reviewer sessions)`,
    );
  });
  return sdkPromise;
}

/** Create the shared model runtime once (trusted local configuration). */
export function loadModelRuntime(): NonNullable<typeof modelRuntimePromise> {
  modelRuntimePromise ??= loadSdk().then((sdk) =>
    sdk.ModelRuntime.create({ allowModelNetwork: false }),
  );
  return modelRuntimePromise;
}

/** The model type accepted by `createAgentSession`. */
type SessionModel = NonNullable<CreateAgentSessionOptions["model"]>;

/**
 * Resolve the configured `provider/model-id` against the local model
 * catalog and apply an explicit endpoint override when configured.
 */
export async function resolveSessionModel(config: AgentSessionConfig): Promise<SessionModel> {
  const separator = config.model.indexOf("/");
  if (separator <= 0 || separator === config.model.length - 1) {
    throw new ReviewerSessionError(
      SESSION_ERROR_CODES.modelUnresolved,
      `model identifier \`${config.model}\` must be <provider>/<model-id>`,
    );
  }
  const provider = config.model.slice(0, separator);
  const modelId = config.model.slice(separator + 1);
  const runtime = await loadModelRuntime();
  const model = runtime.getModel(provider, modelId);
  if (model === undefined) {
    throw new ReviewerSessionError(
      SESSION_ERROR_CODES.modelUnresolved,
      `model \`${config.model}\` is not registered in the local model catalog`,
    );
  }
  if (config.endpoint === undefined) {
    return model;
  }
  return { ...model, baseUrl: config.endpoint };
}

/** Create the session agent for one reviewer. */
export async function createPiSessionAgent(
  config: AgentSessionConfig,
  tools: readonly ReviewerToolDef[],
): Promise<SessionAgent> {
  const [sdk, model] = await Promise.all([loadSdk(), resolveSessionModel(config)]);

  const settingsManager = sdk.SettingsManager.inMemory();
  const loader = new sdk.DefaultResourceLoader({
    cwd: process.cwd(),
    agentDir: sdk.getAgentDir(),
    settingsManager,
    noExtensions: true,
    noSkills: true,
    noPromptTemplates: true,
    noThemes: true,
    noContextFiles: true,
    systemPrompt: config.systemPrompt,
  });
  await loader.reload();

  const customTools: ToolDefinition[] = tools.map((tool) =>
    sdk.defineTool({
      name: tool.name,
      label: tool.name,
      description: tool.description,
      promptSnippet: `Read-only ${tool.name} resolved against the review snapshot by Nisse.`,
      parameters: tool.parameters as never,
      execute: async (_toolCallId: string, params: never) => {
        const args = params as unknown as Record<string, unknown>;
        const outcome = await tool.execute(args ?? {});
        if (outcome.isError) {
          throw new Error(outcome.text);
        }
        return {
          content: [{ type: "text" as const, text: outcome.text }],
          details: {},
        };
      },
    }),
  );

  // The explicit tool allowlist names only the granted read-only lookup
  // tools: built-in coding tools are not listed and therefore disabled
  // (`noTools: "all"` would filter out custom tools as well).
  const options: CreateAgentSessionOptions = {
    cwd: process.cwd(),
    model,
    thinkingLevel: config.thinkingLevel as NonNullable<CreateAgentSessionOptions["thinkingLevel"]>,
    tools: customTools.map((tool) => tool.name),
    customTools,
    resourceLoader: loader,
    sessionManager: sdk.SessionManager.inMemory(),
    settingsManager,
    modelRuntime: await loadModelRuntime(),
  };
  const { session } = await sdk.createAgentSession(options);
  return new PiSessionAgent(session);
}

/** The real Pi SDK agent factory. */
export const piAgentFactory: SessionAgentFactory = createPiSessionAgent;

/** Adapts a Pi SDK `AgentSession` to the reviewer session surface. */
class PiSessionAgent implements SessionAgent {
  private readonly session: AgentSession;

  constructor(session: AgentSession) {
    this.session = session;
  }

  async prompt(text: string): Promise<void> {
    await this.session.prompt(text, { expandPromptTemplates: false });
  }

  finalText(): string | undefined {
    const messages = this.session.messages;
    for (let index = messages.length - 1; index >= 0; index -= 1) {
      const message = messages[index];
      if (message === undefined) {
        continue;
      }
      if (message.role !== "assistant") {
        // The final assistant message is the last model answer; stop once a
        // user message is reached so older turns never leak into answers.
        if (message.role === "user") {
          return undefined;
        }
        continue;
      }
      const text = message.content
        .filter((part): part is { type: "text"; text: string } => part.type === "text")
        .map((part) => part.text)
        .join("\n")
        .trim();
      return text.length > 0 ? text : undefined;
    }
    return undefined;
  }

  async abort(): Promise<void> {
    await this.session.abort();
  }

  dispose(): void {
    this.session.dispose();
  }
}
