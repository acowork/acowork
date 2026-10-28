/**
 * `chatStore.loadAvailableModels` — the shared model inventory.
 *
 * Regression context: `availableModels` / `providerAccounts` feed BOTH
 * composer surfaces (the execution ChatPanel's menus and the Agent
 * Chat-session's model / reasoning buttons — the reasoning control's very
 * existence is resolved from this list via lib/modelCapabilities.ts). The
 * load used to be an effect of the execution composer mounting, so a desktop
 * restart that reconnected to an ALREADY-RUNNING agent — where `alive` is
 * true from the first render and `AgentPage` renders the Chat surface, never
 * the exec one — left the Chat composer with a workspace-only toolbar.
 *
 * The store action is now the single owner and AppLayout triggers it. These
 * tests pin the contract: vault keys → provider accounts + one list entry per
 * configured (model, provider), a capability fetch failure keeping the
 * configured ids AND their previously resolved capabilities (a transient 500
 * must not blank the reasoning control), dedup, single-flight WITH
 * compensation (a `models-added` vault write landing mid-flight forces a
 * re-read — the running load's vault snapshot predates the write),
 * identity-preserving writes (unchanged content reuses the previous
 * references, per the AGENTS.md store rule), and a vault read failure
 * leaving the previous inventory intact.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";

const { mockInvoke, mockFetchProviderModels } = vi.hoisted(() => ({
  mockInvoke: vi.fn(async (..._args: unknown[]): Promise<unknown> => undefined),
  mockFetchProviderModels: vi.fn(
    async (_provider: string): Promise<{ models: unknown[] }> => ({ models: [] }),
  ),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: mockInvoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("../lib/gateway-api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/gateway-api")>()),
  fetchProviderModels: mockFetchProviderModels,
}));
vi.mock("../lib/logger", () => ({
  log: { debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
  setLevel: () => {},
  getLevel: () => "off" as const,
}));

import { useChatStore, __resetModelInventoryFlightForTests } from "./chatStore";
import type { ModelEntry } from "../lib/types";

const ONE_OPENAI_KEY = [
  { provider: "openai", account_id: "acct-1", key_preview: "sk-…", models: ["gpt-4o"] },
];

beforeEach(() => {
  mockInvoke.mockReset();
  mockFetchProviderModels.mockReset();
  mockFetchProviderModels.mockResolvedValue({ models: [] });
  // The single-flight lock is module state; a test that asserts (or fails)
  // mid-flight leaves it held, which would make the NEXT test await an
  // abandoned promise and time out.
  __resetModelInventoryFlightForTests();
  useChatStore.setState({ availableModels: [], providerAccounts: {} });
});

describe("chatStore.loadAvailableModels", () => {
  it("builds the inventory from vault keys + one capability fetch per provider", async () => {
    mockInvoke.mockResolvedValue([
      // Two accounts on the same provider — the capability fetch must be
      // collapsed to one call per provider, but both accounts stay in the menu.
      {
        provider: "openai",
        account_id: "acct-1",
        alias: "work",
        key_preview: "sk-…1",
        models: ["gpt-4o"],
      },
      {
        provider: "openai",
        account_id: "acct-2",
        alias: "private",
        key_preview: "sk-…2",
        models: ["gpt-4o"],
      },
      {
        provider: "anthropic",
        account_id: "acct-3",
        key_preview: "sk-…3",
        default_model: "claude-3-5-sonnet",
        model_capabilities: {
          "claude-3-5-sonnet": { default_reasoning_effort: "high" },
        },
      },
    ]);
    mockFetchProviderModels.mockImplementation(async (provider: string) =>
      provider === "openai"
        ? { models: [{ id: "gpt-4o", tool_call: true, reasoning: true }] }
        : { models: [] },
    );

    await useChatStore.getState().loadAvailableModels();

    const { availableModels, providerAccounts } = useChatStore.getState();
    // toStrictEqual, not toEqual: equal-by-undefined vs missing-key are the
    // same to `toEqual`, and the "no capability" shape IS the contract here.
    expect(availableModels).toStrictEqual([
      { name: "gpt-4o", provider: "openai", tool_call: true, reasoning: true, input_modalities: undefined, default_reasoning_effort: undefined },
      { name: "claude-3-5-sonnet", provider: "anthropic", tool_call: undefined, reasoning: undefined, input_modalities: undefined, default_reasoning_effort: "high" },
    ]);
    expect(mockFetchProviderModels).toHaveBeenCalledTimes(2);
    expect(providerAccounts.openai?.map((a) => a.accountId)).toEqual([
      "acct-1",
      "acct-2",
    ]);
    expect(providerAccounts.anthropic?.[0]?.alias).toBe("anthropic");
  });

  it("keeps a configured model (without capabilities) when its first capability fetch fails", async () => {
    mockInvoke.mockResolvedValue(ONE_OPENAI_KEY);
    mockFetchProviderModels.mockRejectedValue(new Error("503"));

    await useChatStore.getState().loadAvailableModels();

    expect(useChatStore.getState().availableModels).toStrictEqual([
      { name: "gpt-4o", provider: "openai", tool_call: undefined, reasoning: undefined, input_modalities: undefined, default_reasoning_effort: undefined },
    ]);
  });

  it("keeps previously resolved capabilities when a later fetch fails", async () => {
    // A transient provider-side 500 on a refresh must NOT blank the
    // capabilities and make the reasoning control disappear.
    mockInvoke.mockResolvedValue(ONE_OPENAI_KEY);
    mockFetchProviderModels.mockResolvedValue({
      models: [{ id: "gpt-4o", tool_call: true, reasoning: true, input_modalities: ["text", "image"] }],
    });
    await useChatStore.getState().loadAvailableModels();
    expect(useChatStore.getState().availableModels[0].reasoning).toBe(true);

    mockFetchProviderModels.mockRejectedValue(new Error("500"));
    await useChatStore.getState().loadAvailableModels();

    expect(useChatStore.getState().availableModels).toStrictEqual([
      { name: "gpt-4o", provider: "openai", tool_call: true, reasoning: true, input_modalities: ["text", "image"], default_reasoning_effort: undefined },
    ]);
  });

  it("shares one fetch between concurrent callers", async () => {
    mockInvoke.mockResolvedValue(ONE_OPENAI_KEY);

    await Promise.all([
      useChatStore.getState().loadAvailableModels(),
      useChatStore.getState().loadAvailableModels(),
    ]);

    expect(mockFetchProviderModels).toHaveBeenCalledTimes(1);
    expect(useChatStore.getState().availableModels).toHaveLength(1);
    // The single-flight entry is released — a later refresh actually refetches.
    await useChatStore.getState().loadAvailableModels();
    expect(mockFetchProviderModels).toHaveBeenCalledTimes(2);
  });

  it("re-runs the read when a vault write lands while a load is in flight", async () => {
    // The running load's vault snapshot predates the write, so its result
    // would hide the new provider — the `models-added` signal must force one
    // compensating re-read (while a same-tick second call merely coalesces).
    const capabilityFetch = deferred<unknown>();
    mockInvoke.mockResolvedValue(ONE_OPENAI_KEY); // run 1's snapshot: no deepseek yet
    mockFetchProviderModels.mockReturnValue(capabilityFetch.promise); // hang mid-flight

    const first = useChatStore.getState().loadAvailableModels();
    // Let run 1 reach the capability fetch — the vault snapshot is read by now.
    while (mockFetchProviderModels.mock.calls.length === 0) await Promise.resolve();

    // A vault write completes mid-flight and fires the shared signal. The
    // vault now answers with the added provider on any subsequent read.
    mockInvoke.mockResolvedValue([
      ...ONE_OPENAI_KEY,
      { provider: "deepseek", account_id: "acct-9", key_preview: "sk-…9", models: ["deepseek-chat"] },
    ]);
    window.dispatchEvent(new Event("models-added"));
    const handler = useChatStore.getState().loadAvailableModels();
    // Neither the signal handler nor the shared promise started a parallel
    // read — the compensating re-run below is the only second read.
    expect(mockInvoke).toHaveBeenCalledTimes(1);

    capabilityFetch.resolve({ models: [{ id: "gpt-4o", tool_call: true }] });
    await Promise.all([first, handler]);

    expect(mockInvoke).toHaveBeenCalledTimes(2); // the compensating re-read happened
    // Run 1 fetched openai only; the compensating run fetched both providers.
    expect(mockFetchProviderModels).toHaveBeenCalledTimes(3);
    expect(useChatStore.getState().availableModels.map((m) => m.name)).toContain("deepseek-chat");
  });

  it("reuses the previous references when nothing changed", async () => {
    // The load fires on every Gateway connect edge; unchanged content must
    // not hand subscribers a new identity (identity-preserving store rule).
    mockInvoke.mockResolvedValue(ONE_OPENAI_KEY);
    mockFetchProviderModels.mockResolvedValue({ models: [{ id: "gpt-4o", tool_call: true }] });

    await useChatStore.getState().loadAvailableModels();
    const firstModels = useChatStore.getState().availableModels;
    const firstAccounts = useChatStore.getState().providerAccounts;

    await useChatStore.getState().loadAvailableModels();

    expect(useChatStore.getState().availableModels).toBe(firstModels);
    expect(useChatStore.getState().providerAccounts).toBe(firstAccounts);
  });

  it("keeps the previous inventory when the vault read fails", async () => {
    const previous: ModelEntry[] = [{ name: "gpt-4o", provider: "openai" }];
    useChatStore.setState({ availableModels: previous });
    mockInvoke.mockRejectedValue(new Error("not in the desktop shell"));

    await expect(
      useChatStore.getState().loadAvailableModels(),
    ).resolves.toBeUndefined();

    expect(useChatStore.getState().availableModels).toBe(previous);
  });
});

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((res) => {
    resolve = res;
  });
  return { promise, resolve };
}
