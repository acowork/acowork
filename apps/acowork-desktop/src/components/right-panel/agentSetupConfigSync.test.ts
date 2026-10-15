import { describe, expect, it } from "vitest";
import {
  buildProfilePatchFromServerConfig,
  type ServerConfigEnvelope,
} from "./agentSetupConfigSync";

describe("buildProfilePatchFromServerConfig", () => {
  // The bug this helper exists to fix: the panel's two
  // GET-/config paths (mount + refresh event) used to inline
  // their own cfg-cast + patch-build block, and the two
  // blocks could drift (compressionRatioThreshold was missing
  // from the refresh path in the original implementation).
  // If a new `AgentConfig` field gets added to the wire
  // shape, the panel would silently fail to sync it again.
  // These tests pin the contract so a regression shows up
  // here, not in a UI screenshot.

  it("returns an empty patch for null / undefined / empty data", () => {
    expect(buildProfilePatchFromServerConfig(null)).toEqual({});
    expect(buildProfilePatchFromServerConfig(undefined)).toEqual({});
    expect(buildProfilePatchFromServerConfig({})).toEqual({});
    expect(buildProfilePatchFromServerConfig({ config: null })).toEqual({});
  });

  it("syncs every other AgentConfig field with the same typeof-guard", () => {
    const data: ServerConfigEnvelope = {
      config: {
        max_output_tokens: 16000,
        max_iterations: 500,
        max_sessions: 2000,
        temperature: 0.7,
        context_window: 216000,
        shell_approval_threshold: "auto_approve",
        approval_timeout_secs: 500,
        compression_ratio_threshold: 0.9,
        session_language: "zh-CN",
      },
    };
    expect(buildProfilePatchFromServerConfig(data)).toEqual({
      maxTokens: 16000,
      maxIterations: 500,
      maxSessions: 2000,
      temperature: 0.7,
      contextWindow: 216000,
      shellApprovalThreshold: "auto_approve",
      approvalTimeoutSecs: 500,
      compressionRatioThreshold: 0.9,
      sessionLanguage: "zh-CN",
    });
  });

  it("ignores fields with wrong type rather than coercing", () => {
    // `?? default` patterns used to silently clobber user input
    // when the server returned a wrong type. The new contract:
    // ignore wrong-type fields, let the local cached value stick.
    const data: ServerConfigEnvelope = {
      config: {
        max_output_tokens: "16000" as unknown as number, // wrong type
        temperature: null, // JSON-null = no opinion, same as absent
      },
    };
    const p = buildProfilePatchFromServerConfig(data);
    expect(p.maxTokens).toBeUndefined();
    expect(p.temperature).toBeUndefined();
  });

  it("preserves explicit session_language=null (clear) but ignores undefined", () => {
    const p1 = buildProfilePatchFromServerConfig({
      config: { session_language: null },
    });
    expect(p1.sessionLanguage).toBeNull();

    const p2 = buildProfilePatchFromServerConfig({ config: {} });
    expect(p2.sessionLanguage).toBeUndefined();
  });

  it("passes activeModel / activeProvider through, coercing null → undefined", () => {
    // These are display-only — always taken when present.
    const p = buildProfilePatchFromServerConfig({
      model: "MiniMax-M3",
      provider: "custom-minimax",
    });
    expect(p.activeModel).toBe("MiniMax-M3");
    expect(p.activeProvider).toBe("custom-minimax");

    // JSON-null from the runtime ("no model bound") collapses to
    // undefined so the AgentProfileSettings type (string | undefined,
    // NOT string | null) stays consistent.
    const p2 = buildProfilePatchFromServerConfig({
      model: null,
      provider: null,
    });
    expect(p2.activeModel).toBeUndefined();
    expect(p2.activeProvider).toBeUndefined();
  });

  it("returns a sparse patch when only a subset of fields is present", () => {
    // Mount-time fetch returns whatever is on disk; partial
    // responses are the norm, not the exception.
    const data: ServerConfigEnvelope = { config: { temperature: 0.5 } };
    expect(buildProfilePatchFromServerConfig(data)).toEqual({
      temperature: 0.5,
    });
  });
});