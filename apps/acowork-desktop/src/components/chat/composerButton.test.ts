import { describe, it, expect } from "vitest";
import { composerButtonState, type ComposerButtonInput } from "./composerButton";

const base: ComposerButtonInput = {
  sending: false,
  hasDraft: false,
  hasUploading: false,
  readOnly: false,
  connected: true,
};

const state = (over: Partial<ComposerButtonInput>) => composerButtonState({ ...base, ...over });

describe("composerButtonState — one-step send", () => {
  it("idle: sends, and an empty composer is a live silent no-op", () => {
    expect(state({})).toEqual({ mode: "send", disabled: false, labelKey: "sendMessage" });
    expect(state({ hasDraft: true })).toEqual({
      mode: "send",
      disabled: false,
      labelKey: "sendMessage",
    });
  });

  it("running with a draft: SENDS and interrupts (the one-step send)", () => {
    expect(state({ sending: true, hasDraft: true })).toEqual({
      mode: "send",
      disabled: false,
      labelKey: "sendAndInterrupt",
    });
  });

  it("running with an empty composer: stops", () => {
    expect(state({ sending: true })).toEqual({ mode: "stop", disabled: false, labelKey: "stop" });
  });

  // Regression: the stop affordance must survive the conditions that gate
  // sending. A dropped connection or an in-flight upload used to disable the
  // button wholesale, taking away the only control that can halt a turn.
  it("stop stays live while disconnected or uploading", () => {
    expect(state({ sending: true, connected: false }).disabled).toBe(false);
    expect(state({ sending: true, hasUploading: true }).disabled).toBe(false);
    expect(state({ sending: true, connected: false, hasUploading: true }).disabled).toBe(false);
  });

  it("send is gated by upload and connection state", () => {
    expect(state({ hasDraft: true, hasUploading: true }).disabled).toBe(true);
    expect(state({ hasDraft: true, connected: false }).disabled).toBe(true);
    expect(state({ sending: true, hasDraft: true, hasUploading: true }).disabled).toBe(true);
    expect(state({ sending: true, hasDraft: true, connected: false }).disabled).toBe(true);
  });

  it("a read-only session can neither send nor stop", () => {
    expect(state({ readOnly: true, hasDraft: true }).disabled).toBe(true);
    expect(state({ readOnly: true, sending: true }).disabled).toBe(true);
  });
});