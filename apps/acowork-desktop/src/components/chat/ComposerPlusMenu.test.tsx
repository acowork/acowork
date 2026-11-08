// @vitest-environment jsdom
/**
 * The composer `+` menu — what only this component can get wrong.
 *
 * The switch *semantics* (narrow-never-widen, deny bookkeeping) live in
 * `lib/toolInventory.ts` and are pinned there; the persistence contract lives
 * in `stores/toolPolicyStore.ts`. This file pins the navigation and the wiring:
 * four level-1 entries, drill-down into a server's tools, the attachment row
 * handing off to the picker, and a switch reaching the store.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));
vi.mock("../../lib/logger", () => ({
  log: { warn: vi.fn(), debug: vi.fn(), info: vi.fn(), error: vi.fn() },
  setLevel: vi.fn(),
  getLevel: vi.fn(),
}));
// The skill level is another extracted component with its own store; keep this
// test to the menu shell.
vi.mock("../skills/SkillMenuList", () => ({
  SkillMenuList: ({ onDone }: { onDone?: () => void }) => (
    <div>
      <span>skill-list</span>
      <button type="button" onClick={() => onDone?.()}>
        pick-skill
      </button>
    </div>
  ),
}));

import { ComposerPlusMenu } from "./ComposerPlusMenu";
import { CircularProgressIcon } from "./CircularProgressIcon";
import { useToolPolicyStore } from "../../stores/toolPolicyStore";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";

const TOOLS = {
  tools: [
    { name: "read_file", enabled: true },
    { name: "shell", enabled: false },
  ],
  mcp_servers_defs: [
    { name: "playwright", transport: "stdio", command: "npx", args: [], active: true },
  ],
};

// The real `GET /mcp-tools` envelope: `{ agent_id, servers }`. A stub that
// returns the bare `servers` map here matches the old, wrong reader and lets
// the "no tool drill-down" bug pass its own component test.
const MCP_TOOLS = {
  agent_id: "agent-1",
  servers: { playwright: [{ name: "navigate", enabled: true }] },
};

function stubFetch(putStatus = 200) {
  const puts: unknown[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string, init?: RequestInit) => {
      const ok = (payload: unknown) =>
        Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve(payload) });
      if (url.endsWith("/agents/agent-1/tools")) return ok(TOOLS);
      if (url.endsWith("/agents/agent-1/mcp-tools")) return ok(MCP_TOOLS);
      if (url.includes("/sessions/s-1/tool-policy")) {
        if (init?.method === "PUT") {
          puts.push(JSON.parse(String(init.body)));
          return Promise.resolve({
            ok: putStatus === 200,
            status: putStatus,
            json: () => Promise.resolve(JSON.parse(String(init.body))),
          });
        }
        return ok({ tool_policy: null });
      }
      return ok({});
    }),
  );
  return puts;
}

function renderMenu(onPickAttachments = vi.fn()) {
  render(
    <ComposerPlusMenu readOnly={false} onPickAttachments={onPickAttachments} />,
  );
  fireEvent.click(screen.getByLabelText("chatPanel.plusMenu"));
  return onPickAttachments;
}

describe("ComposerPlusMenu", () => {
  beforeEach(() => {
    // Real stores, seeded: the menu reads the agent / active session from them
    // and `useAgentStore()` is called without a selector, which a call-through
    // mock cannot satisfy.
    useAgentStore.setState({ selectedAgentId: "agent-1" });
    useChatStore.setState({
      agentStates: { "agent-1": { activeSessionId: "s-1" } },
    } as never);
    useToolPolicyStore.setState({
      inventory: {},
      inventoryLoading: {},
      inventoryError: {},
      policies: {},
      inFlight: {},
    });
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("opens on four level-1 entries", () => {
    stubFetch();
    renderMenu();
    expect(screen.getByText("chatPanel.plusSkills")).toBeTruthy();
    expect(screen.getByText("chatPanel.plusMcp")).toBeTruthy();
    expect(screen.getByText("chatPanel.plusBuiltinTools")).toBeTruthy();
    expect(screen.getByText("chatPanel.plusAttachments")).toBeTruthy();
  });

  it("hands attachments off to the picker and closes", () => {
    stubFetch();
    const onPick = renderMenu();
    fireEvent.click(screen.getByText("chatPanel.plusAttachments"));
    expect(onPick).toHaveBeenCalledTimes(1);
    expect(screen.queryByText("chatPanel.plusSkills")).toBeNull();
  });

  it("drills into skills and closes on a pick", () => {
    stubFetch();
    renderMenu();
    fireEvent.click(screen.getByText("chatPanel.plusSkills"));
    fireEvent.click(screen.getByText("pick-skill"));
    expect(screen.queryByText("skill-list")).toBeNull();
  });

  it("switches a builtin tool off through the store", async () => {
    const puts = stubFetch();
    renderMenu();
    fireEvent.click(screen.getByText("chatPanel.plusBuiltinTools"));

    // `read_file` is agent-enabled, `shell` is not.
    const switches = await screen.findAllByRole("switch");
    expect(switches).toHaveLength(2);
    expect((switches[0] as HTMLInputElement).checked).toBe(true);
    expect((switches[1] as HTMLInputElement).disabled).toBe(true);

    fireEvent.click(switches[0]);
    await waitFor(() => expect(puts).toHaveLength(1));
    expect(puts[0]).toEqual({
      tool_policy: {
        disabled_builtin_tools: ["read_file"],
        disabled_mcp_servers: [],
        // The wire shape is Rust's `BTreeMap`, not the UI's identity list.
        disabled_mcp_tools: {},
      },
    });
  });

  it("holds the switches until the session policy has been read", async () => {
    // A GET that never settles: the read is in flight for the whole test.
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        const ok = (payload: unknown) =>
          Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve(payload) });
        if (url.endsWith("/agents/agent-1/tools")) return ok(TOOLS);
        if (url.endsWith("/agents/agent-1/mcp-tools")) return ok(MCP_TOOLS);
        if (url.includes("/tool-policy")) return new Promise(() => {});
        return ok({});
      }),
    );
    renderMenu();
    fireEvent.click(screen.getByText("chatPanel.plusBuiltinTools"));

    const switches = await screen.findAllByRole("switch");
    // Every switch is held and the reason is on screen: a toggle would be a
    // whole-policy PUT computed from a value that never arrived.
    expect(switches.every((s) => (s as HTMLInputElement).disabled)).toBe(true);
    expect(screen.getByText("chatPanel.plusLoading")).toBeTruthy();
  });

  it("drills from a server into its tools", async () => {
    stubFetch();
    renderMenu();
    fireEvent.click(screen.getByText("chatPanel.plusMcp"));

    // The server row: its switch plus a drill into the tool level.
    const serverSwitch = await screen.findByRole("switch");
    expect((serverSwitch as HTMLInputElement).checked).toBe(true);

    // The drill target is the ROW, not a sub-control. This is the gesture the
    // settings panel trains users to make and the one the hover tint promises;
    // pinning the row is what keeps "clicked it, nothing happened" from
    // coming back as a passing test.
    fireEvent.click(screen.getByRole("button", { name: "chatPanel.plusShowTools" }));

    await waitFor(() => expect(screen.getByText("navigate")).toBeTruthy());
    // The server switch is gone; the tool's is the only one left.
    expect(screen.getAllByRole("switch")).toHaveLength(1);
  });

  it("flipping the server switch does not navigate into its tools", async () => {
    const puts = stubFetch();
    renderMenu();
    fireEvent.click(screen.getByText("chatPanel.plusMcp"));
    const serverSwitch = await screen.findByRole("switch");

    fireEvent.click(serverSwitch);

    // The switch owns its own click; navigation is the row's. Without the
    // stopPropagation both fire, and turning a server off also throws the user
    // into the tool list of a server they just closed.
    await waitFor(() => expect(puts.length).toBeGreaterThan(0));
    expect(screen.queryByText("navigate")).toBeNull();
  });

  it("does not offer level 2 for an agent-disabled server", async () => {
    stubFetch();
    useToolPolicyStore.setState((s) => ({
      ...s,
      inventory: {
        "agent-1": {
          agentId: "agent-1",
          builtin: [],
          mcpServers: [
            {
              name: "pm",
              agentEnabled: false,
              tools: [
                {
                  server: "pm",
                  tool: "list_tasks",
                  prefixedName: "pm__list_tasks",
                  agentEnabled: false,
                },
              ],
            },
          ],
        },
      },
    }));
    renderMenu();
    fireEvent.click(screen.getByText("chatPanel.plusMcp"));
    // Not a drill target: the box it would open holds nothing reachable.
    expect(screen.queryByRole("button", { name: "chatPanel.plusShowTools" })).toBeNull();
    // The switch is still there — the row is inert, not missing.
    expect(await screen.findByRole("switch")).toBeTruthy();
  });

  it("shows the reset affordance only when the group denies something", async () => {
    stubFetch();
    renderMenu();
    fireEvent.click(screen.getByText("chatPanel.plusBuiltinTools"));
    const reset = await screen.findByLabelText("chatPanel.followAgentDefault");
    expect((reset as HTMLButtonElement).disabled).toBe(true);

    await waitFor(() => expect(useToolPolicyStore.getState().inventory["agent-1"]).toBeTruthy());
    useToolPolicyStore.setState((s) => ({
      ...s,
      policies: {
        ...s.policies,
        "agent-1/s-1": {
          policy: {
            disabled_builtin_tools: ["read_file"],
            disabled_mcp_servers: [],
            disabled_mcp_tools: {},
          },
          loaded: true,
          loading: false,
          saving: false,
          error: null,
          revision: 0,
        },
      },
    }));
    await waitFor(() =>
      expect(
        (screen.getByLabelText("chatPanel.followAgentDefault") as HTMLButtonElement)
          .disabled,
      ).toBe(false),
    );
  });
});

/**
 * The trigger's mark sits between the usage ring and the rest of the row.
 *
 * Pinned as a *relationship*, not as a number, because both ends of the range
 * are real regressions that have already been shipped once:
 *
 *   - lucide `Plus` at the cluster's 16 → ~10.7px of ink: users read the `+`
 *     as missing ("太小了，不显眼").
 *   - the cross full-bleed to the ring's 16px / 2px stroke → the loudest glyph
 *     on the strip, which reads as abrupt ("有点大的过头了，突兀").
 *
 * So the invariant is a band, measured in ink — the span each glyph actually
 * paints — not in box size. lucide gives every glyph the same 24-unit box and
 * fills it unevenly, so two glyphs with the same box can paint very different
 * marks; box size is exactly the thing that hid the original bug.
 */
describe("ComposerPlusMenu trigger glyph", () => {
  /**
   * Width of what an SVG glyph actually paints, in CSS pixels.
   *
   * For a ring: `2 * (r + stroke/2)`. For a stroked path: the centreline
   * extent plus one cap radius at each end. Both are computed in user units
   * and then scaled by `width / viewBox-width` — without that step a lucide
   * glyph reads as full-size no matter how small it renders, which is the
   * measurement error that let the original 14px `+` look fine on paper.
   */
  function inkSpan(svg: SVGSVGElement): number {
    const viewBox = (svg.getAttribute("viewBox") ?? "0 0 24 24").split(/\s+/);
    const scale = Number(svg.getAttribute("width")) / Number(viewBox[2]);

    const circle = svg.querySelector("circle");
    if (circle) {
      const r = Number(circle.getAttribute("r"));
      const stroke = Number(circle.getAttribute("stroke-width"));
      return 2 * (r + stroke / 2) * scale;
    }

    const stroke = Number(svg.getAttribute("stroke-width"));
    const cap = svg.getAttribute("stroke-linecap") === "round" ? stroke / 2 : 0;
    // The horizontal arm is the `h` command (`M2.5 8h11`).
    const horizontal = /h([-\d.]+)/.exec(String(svg.querySelector("path")?.getAttribute("d")));
    return (Number(horizontal?.[1]) + 2 * cap) * scale;
  }

  function strokeWeight(svg: SVGSVGElement): number {
    // The weight lives on the painted shape in the ring (`<circle>` carries
    // its own stroke-width) and on the root in the cross — read whichever
    // element actually holds it rather than assuming where the author put it.
    const painted = svg.querySelector("circle, path");
    return Number(painted?.getAttribute("stroke-width") ?? svg.getAttribute("stroke-width"));
  }

  function plusGlyph(): SVGSVGElement {
    const { container } = render(
      <ComposerPlusMenu readOnly={false} onPickAttachments={vi.fn()} />,
    );
    return container.querySelector("button svg") as SVGSVGElement;
  }

  function ringGlyph(): SVGSVGElement {
    const { container } = render(<CircularProgressIcon usagePercent={50} />);
    return container.querySelector("svg") as SVGSVGElement;
  }

  it("stays quieter than the usage ring — it must not be the loudest mark on the row", () => {
    expect(inkSpan(plusGlyph())).toBeLessThan(inkSpan(ringGlyph()));
    expect(strokeWeight(plusGlyph())).toBeLessThan(strokeWeight(ringGlyph()));
  });

  it("stays loud enough to read beside the ring", () => {
    // The floor is what the shipped 14px lucide cross broke: it painted 0.58
    // of the ring's span and the button read as absent.
    expect(inkSpan(plusGlyph())).toBeGreaterThanOrEqual(0.7 * inkSpan(ringGlyph()));
  });

  it("keeps the shared icon-only button box, so the toolbar row does not move", () => {
    const { container } = render(
      <ComposerPlusMenu readOnly={false} onPickAttachments={vi.fn()} />,
    );
    // Only the glyph changed. The 28x28 click target, and with it the fold
    // engine's `[data-toolbar-btn]` measurement, must not.
    expect(container.querySelector("button")?.className).toMatch(/h-7 w-7/);
  });
});
