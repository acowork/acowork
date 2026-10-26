import { describe, expect, it } from "vitest";

import {
  distillerForm,
  forgettingForm,
  minutesToSecs,
  parseDormantThreshold,
  parsePositiveInt,
  secsToMinutesInput,
} from "./consolidationPolicy";
import type { DistillerStatus, ForgettingStatus } from "../../lib/types";

const distiller = (over: Partial<DistillerStatus> = {}): DistillerStatus => ({
  enabled: false,
  episode_backlog: 0,
  secs_since_distill: 0,
  interval_secs: 3600,
  accumulation_threshold: 50,
  idle_secs: 1800,
  last_run: null,
  models: [],
  ...over,
});

const forgetting = (over: Partial<ForgettingStatus> = {}): ForgettingStatus => ({
  enabled: false,
  half_life_days: 180,
  dormant_threshold: 0.1,
  archive_days: 90,
  interval_secs: 86400,
  ...over,
});

describe("minutes ↔ seconds", () => {
  it("shows seconds as minutes", () => {
    expect(secsToMinutesInput(3600)).toBe("60");
    expect(secsToMinutesInput(1800)).toBe("30");
    // 90s is not a whole number of minutes; the input must not carry a
    // floating-point tail, and re-saving the rounded value is a no-op.
    expect(secsToMinutesInput(90)).toBe("1.5");
  });

  it("treats a missing or non-positive duration as empty rather than 0", () => {
    expect(secsToMinutesInput(null)).toBe("");
    expect(secsToMinutesInput(undefined)).toBe("");
    expect(secsToMinutesInput(0)).toBe("");
    expect(secsToMinutesInput(Number.NaN)).toBe("");
  });

  it("saves minutes as whole seconds", () => {
    expect(minutesToSecs("60")).toBe(3600);
    expect(minutesToSecs(" 1.5 ")).toBe(90);
    // Round-trips what the input showed.
    expect(minutesToSecs(secsToMinutesInput(90))).toBe(90);
  });

  it("refuses durations the service would reject", () => {
    expect(minutesToSecs("")).toBeNull();
    expect(minutesToSecs("0")).toBeNull();
    expect(minutesToSecs("-5")).toBeNull();
    expect(minutesToSecs("abc")).toBeNull();
  });
});

describe("parsePositiveInt", () => {
  it("accepts whole counts and rounds fractional text", () => {
    expect(parsePositiveInt("50")).toBe(50);
    expect(parsePositiveInt("49.6")).toBe(50);
  });

  it("refuses zero, negatives and junk", () => {
    expect(parsePositiveInt("0")).toBeNull();
    expect(parsePositiveInt("-1")).toBeNull();
    expect(parsePositiveInt("")).toBeNull();
    expect(parsePositiveInt("nope")).toBeNull();
  });
});

describe("parseDormantThreshold", () => {
  it("accepts the service's (0, 1] range", () => {
    expect(parseDormantThreshold("0.1")).toBe(0.1);
    expect(parseDormantThreshold("1")).toBe(1);
  });

  it("refuses values validate_settings would 400 on", () => {
    expect(parseDormantThreshold("0")).toBeNull();
    expect(parseDormantThreshold("1.01")).toBeNull();
    expect(parseDormantThreshold("-0.2")).toBeNull();
    expect(parseDormantThreshold("")).toBeNull();
  });
});

describe("distillerForm", () => {
  it("maps the service's status onto the card's fields", () => {
    expect(distillerForm(distiller({ enabled: true }))).toEqual({
      enabled: true,
      models: [],
      intervalMinutes: "60",
      accumulation: "50",
      idleMinutes: "30",
    });
  });

  it("carries the candidate chain through without inventing entries", () => {
    const form = distillerForm(
      distiller({ models: [{ provider_id: "openai", model_id: "gpt-4o-mini" }] }),
    );
    expect(form.models).toEqual([{ provider_id: "openai", model_id: "gpt-4o-mini" }]);
  });

  // The bug this module exists to stop: an unmounted card used to read the
  // retired `agent_config.json` fields, which are always absent now, so a
  // 60-minute interval and a saved model chain came back as "off"/empty.
  it("never reports a stored policy as disabled just because status is absent", () => {
    expect(distillerForm(null)).toEqual({
      enabled: false,
      models: [],
      intervalMinutes: "",
      accumulation: "",
      idleMinutes: "",
    });
    expect(distillerForm(undefined).enabled).toBe(false);
  });
});

describe("forgettingForm", () => {
  it("maps the service's status onto the card's fields", () => {
    expect(forgettingForm(forgetting({ enabled: true }))).toEqual({
      enabled: true,
      halfLifeDays: "180",
      dormantThreshold: "0.1",
      archiveDays: "90",
    });
  });

  it("trims the f32 tail the service may hand back", () => {
    const form = forgettingForm(forgetting({ dormant_threshold: 0.30000001192092896 }));
    expect(form.dormantThreshold).toBe("0.3");
  });

  it("fails soft on an absent status", () => {
    expect(forgettingForm(null).enabled).toBe(false);
    expect(forgettingForm(undefined).halfLifeDays).toBe("");
  });
});
