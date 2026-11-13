import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { agentActivity, liveActivityStatus } from "./agentActivity";
import type { SessionStatus } from "./types";

// `check-i18n.mjs` resolves statically analysable `t("a.b")` call sites, and
// its own header says a key handed to `t` through a variable is invisible to
// it. This map is entirely made of those, so a typo here paints a key path in
// the UI and no build step would notice. These tests are that missing check.

/** `t` stands in for the translator by returning its own key (+ params), so a
 *  test asserts WHICH string a status earns, not how it is worded. */
const keyT = ((key: string, options?: Record<string, unknown>) =>
  options && "name" in options ? `${key}:${options.name}` : key) as unknown as Parameters<
  typeof agentActivity
>[1];

const word = (s: SessionStatus | null) => agentActivity(s, keyT);

describe("agentActivity — the keys it emits exist", () => {
  const STATUSES: SessionStatus[] = [
    { status: "idle" },
    { status: "thinking" },
    { status: "llm_awaiting_first_chunk" },
    { status: "llm_streaming", detail: { message_id: null } },
    { status: "tool_executing", detail: { tools: ["memory_recall"] } },
    { status: "tool_executing", detail: { tools: ["http_request"] } },
    { status: "tool_executing", detail: { tools: ["mcp_memory__memory_store"] } },
    { status: "tool_executing", detail: { tools: ["mcp_pm__pm_get_task"] } },
    { status: "tool_executing", detail: { tools: ["mcp_doc__doc_read"] } },
    { status: "tool_executing", detail: { tools: ["mcp_code__code_graph"] } },
    { status: "tool_executing", detail: { tools: ["mcp_sse__anything"] } },
    { status: "tool_executing", detail: { tools: ["some_custom_tool"] } },
    { status: "tool_executing" },
    { status: "tool_executing", detail: {} },
    { status: "waiting_approval", detail: { request_id: "r1" } },
    { status: "paused", detail: { reason: "debug_rewind" as never } },
  ];

  for (const loc of ["en", "zh-CN"]) {
    it(`resolves every word in ${loc}.json`, () => {
      const json = JSON.parse(
        readFileSync(
          resolve(__dirname, "../i18n/locales", `${loc}.json`),
          "utf-8",
        ),
      ) as Record<string, unknown>;
      const at = (path: string): unknown =>
        path
          .split(".")
          .reduce<unknown>(
            (acc, seg) =>
              acc && typeof acc === "object"
                ? (acc as Record<string, unknown>)[seg]
                : undefined,
            json,
          );
      for (const s of STATUSES) {
        const key = word(s).text;
        // The unknown-tool branch appends ":<name>" — that part is a param,
        // not the key.
        const path = key.includes(":") ? key.slice(0, key.indexOf(":")) : key;
        expect(at(path), `status ${JSON.stringify(s)} → ${path}`).toBeTypeOf("string");
      }
    });
  }

  it("covers every built-in tool label, so a named dispatch never falls back", () => {
    const json = JSON.parse(
      readFileSync(resolve(__dirname, "../i18n/locales/en.json"), "utf-8"),
    ) as { tools: Record<string, { running?: string }> };
    const names = Object.keys(json.tools);
    expect(names.length).toBeGreaterThan(10);
    for (const name of names) {
      expect(
        word({ status: "tool_executing", detail: { tools: [name] } }).text,
        name,
      ).toBe(`tools.${name}.running`);
    }
  });
});

describe("agentActivity — which word a status earns", () => {
  it("names the tool a dispatch is waiting on", () => {
    expect(word({ status: "tool_executing", detail: { tools: ["memory_recall"] } })).toEqual({
      text: "tools.memory_recall.running",
      specific: true,
    });
    // Several calls in one dispatch: the first is what is running right now.
    expect(
      word({
        status: "tool_executing",
        detail: { tools: ["file_write", "bash"] },
      }).text,
    ).toBe("tools.file_write.running");
  });

  it("classifies MCP tools by their server — the table has no per-tool entry", () => {
    expect(word({ status: "tool_executing", detail: { tools: ["mcp_memory__memory_store"] } }).text).toBe(
      "chatView.activity.memory",
    );
    expect(word({ status: "tool_executing", detail: { tools: ["mcp_pm__pm_list_tasks"] } }).text).toBe(
      "chatView.activity.project",
    );
    expect(word({ status: "tool_executing", detail: { tools: ["mcp_doc__doc_read"] } }).text).toBe(
      "chatView.activity.docs",
    );
    // A server the platform does not ship is not guessed at — it is named.
    for (const name of ["mcp_code__code_graph", "mcp_sse__ping"]) {
      expect(word({ status: "tool_executing", detail: { tools: [name] } })).toEqual({
        text: `chatView.activity.tool:${name}`,
        specific: true,
      });
    }
  });

  it("falls back when the dispatch names nothing — and says so", () => {
    // The resume-after-approval transition cannot know which calls are still
    // in flight; reusing the previous dispatch's name would be a lie.
    for (const s of [
      { status: "tool_executing" } as SessionStatus,
      { status: "tool_executing", detail: {} } as SessionStatus,
      { status: "tool_executing", detail: { tools: [] } } as SessionStatus,
    ]) {
      expect(word(s)).toEqual({ text: "chatView.activity.working", specific: false });
    }
  });

  it("reads the model phases as work, not as waiting", () => {
    // TTFT wait and reasoning streaming share one word: the split is a
    // provider detail the user cannot act on, and "waiting for the model" is
    // the sentence that made the line look stuck.
    expect(word({ status: "thinking" }).text).toBe("chatView.activity.analyzing");
    expect(word({ status: "llm_awaiting_first_chunk" }).text).toBe(
      "chatView.activity.analyzing",
    );
    expect(word({ status: "llm_streaming", detail: { message_id: null } }).text).toBe(
      "chatView.activity.composing",
    );
    expect(word({ status: "waiting_approval", detail: { request_id: "r" } }).text).toBe(
      "chatView.activity.waitingApproval",
    );
  });

  it("is not specific about a session that is not working", () => {
    // idle / paused / an unknown status: the caller keeps its own sentence,
    // and the banners keep the paused state.
    expect(word({ status: "idle" }).specific).toBe(false);
    expect(word(null).specific).toBe(false);
    expect(
      word({ status: "paused", detail: { reason: "debug_rewind" as never } }).specific,
    ).toBe(false);
  });
});

describe("liveActivityStatus — only speak for the run this turn is", () => {
  const busy: SessionStatus = { status: "thinking" };
  const idle: SessionStatus = { status: "idle" };

  it("returns the one busy run", () => {
    expect(liveActivityStatus([busy])).toBe(busy);
    expect(liveActivityStatus([idle, busy])).toBe(busy);
  });

  it("returns nothing when no run is working — the router-deciding window", () => {
    expect(liveActivityStatus([])).toBeNull();
    expect(liveActivityStatus([idle, null, undefined])).toBeNull();
  });

  it("refuses to pick between several busy runs — a fan-out has no single word", () => {
    const other: SessionStatus = {
      status: "tool_executing",
      detail: { tools: ["bash"] },
    };
    expect(liveActivityStatus([busy, other])).toBeNull();
  });
});
