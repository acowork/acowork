// @vitest-environment jsdom
/**
 * RunBlockActionRow — the icon row over a Chat execution block (design 27
 * §5.5.5).
 *
 * What this file locks, and why each one is a real failure mode:
 *   1. **icon-only, all four controls, in reading order** — the row replaced a
 *      text pill + a bottom input box; a regression that hides a control behind
 *      hover or drops the label into the text layer is invisible in a snapshot
 *      but loses the affordance;
 *   2. **the drawer's open/closed contract** (`data-state`) — the row's height
 *      must not move the transcript, so what is asserted is the state, not a
 *      class name: restyling the motion must not redden the suite;
 *   3. **typed words survive a pointer leave** — the one famous failure of
 *      hover-revealed fields;
 *   4. **closed means inert** — a clipped field left in the tab order is a focus
 *      trap the human cannot see;
 *   5. **copy says so** — the button wears a check on success and stays silent
 *      on failure (no false "copied");
 *   6. **every callback is the host's** — the row renders state, the host sends
 *      and copies.
 */
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { RunBlockActionRow } from "./RunBlockActionRow";
import { copyText } from "../../lib/clipboard";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

vi.mock("../../lib/clipboard", () => ({
  copyText: vi.fn(async () => true),
}));

// The three run-management controls are separate components with their own
// suites; here they are stubs so what fails or passes is the row's WIRING —
// which props the row hands them, and whether the attachment pipeline that
// lives IN the row works.
const { PlusStub, UsageStub, VisStub, plusProps } = vi.hoisted(() => {
  const captured: { current: Record<string, unknown> } = { current: {} };
  return {
    plusProps: captured,
    PlusStub: (props: Record<string, unknown>) => {
      captured.current = props;
      return <div data-testid="plus-menu" />;
    },
    UsageStub: (props: { sessionId?: string }) => (
      <div data-testid="usage" data-session={props.sessionId} />
    ),
    VisStub: (props: { sessionId?: string }) => (
      <div data-testid="visibility" data-session={props.sessionId} />
    ),
  };
});
vi.mock("./ComposerPlusMenu", () => ({ ComposerPlusMenu: PlusStub }));
vi.mock("./ContextUsageIcon", () => ({ ContextUsageIcon: UsageStub }));
vi.mock("./SessionVisibilityToggle", () => ({ SessionVisibilityToggle: VisStub }));

// The row's usage gate reads context-usage presence from chatStore. The
// fixture has usage for s-1 only — s-2 models the never-opened,
// never-pushed run whose ring would have nothing honest to draw.
const { chatStoreMock } = vi.hoisted(() => ({
  chatStoreMock: {
    agentStates: {
      "ag-1": {
        sessionStates: {
          "s-1": { contextUsage: { usage_percent: 42, total_tokens: 100 } },
          "s-2": {},
        },
      },
    },
  },
}));
vi.mock("../../stores/chatStore", () => ({
  useChatStore: (selector: (state: unknown) => unknown) => selector(chatStoreMock),
}));

const uploadMock = vi.hoisted(() =>
  vi.fn(async (_chat: string, file: File) => ({
    id: `att-${file.name}`,
    filename: file.name,
    mime: file.type || "application/octet-stream",
    size: file.size,
  })),
);
vi.mock("../../stores/chatSessionStore", () => ({
  useChatSessionStore: { getState: () => ({ upload: uploadMock }) },
}));

/** The drawer capsule. `data-state` is the contract; class names are not. */
function drawer(): HTMLElement {
  const el = document.querySelector('[data-run-action="reply"]');
  if (!el) throw new Error("no reply drawer rendered");
  return el as HTMLElement;
}

/**
 * Whether a control wears the accent highlight RIGHT NOW, as opposed to only
 * when hovered.
 *
 * This is the one place the suite reads a class name, and it has to: the
 * highlight is paint, and jsdom computes no Tailwind. What is asserted is a
 * single exact token via `classList` — not the class string — so the accent can
 * be restyled without reddening this file, but a highlight that silently
 * degrades to `hover:` (i.e. one that dies the moment the pointer leaves the
 * button for its own drawer) fails.
 *
 * `classList.contains` is exact-token, which is what makes this distinction
 * possible: `hover:text-[var(--color-accent)]` is a different token from
 * `text-[var(--color-accent)]`.
 */
const ACCENT_TEXT = "text-[var(--color-accent)]";
const ACCENT_BG = "bg-[var(--color-accent)]/12";

function isLit(el: HTMLElement): boolean {
  return el.classList.contains(ACCENT_TEXT) && el.classList.contains(ACCENT_BG);
}

function renderRow(props: Partial<React.ComponentProps<typeof RunBlockActionRow>> = {}) {
  return render(
    <RunBlockActionRow
      runName="Fix the login redirect"
      onJump={vi.fn()}
      onReference={vi.fn()}
      copyPayload="# Fix the login redirect"
      onSend={vi.fn(async () => {})}
      {...props}
    />,
  );
}

describe("RunBlockActionRow", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("offers the four actions as icons, in reading order", () => {
    renderRow();
    const labels = screen
      .getAllByRole("button")
      .map((b) => b.getAttribute("aria-label"));
    // Four controls: the drill, the reference, the copy, the message button that
    // reveals the drawer (the send button inside it is inert while closed).
    // Nothing in the row renders as text: an icon row that grows a label is a
    // toolbar.
    expect(labels).toEqual([
      "chatView.viewRun",
      "chatView.block.actionReference",
      "chatView.block.actionCopy",
      "chatView.block.replyPlaceholder",
    ]);
    expect(screen.queryByText("chatView.viewRun")).toBeNull();
  });

  it("carries the session name on the drill's tooltip", () => {
    renderRow();
    expect(
      screen.getByRole("button", { name: "chatView.viewRun" }).getAttribute("title"),
    ).toBe("Fix the login redirect");
  });

  it("renders only the actions the surface handed it", () => {
    renderRow({ onJump: undefined, onReference: undefined, onSend: undefined });
    expect(
      screen.getByRole("button", { name: "chatView.block.actionCopy" }),
    ).toBeTruthy();
    expect(
      screen.queryByRole("button", { name: "chatView.viewRun" }),
    ).toBeNull();
    expect(document.querySelector('[data-run-action="reply"]')).toBeNull();
  });

  it("calls the host's drill and reference handlers", () => {
    const onJump = vi.fn();
    const onReference = vi.fn();
    renderRow({ onJump, onReference });
    fireEvent.click(screen.getByRole("button", { name: "chatView.viewRun" }));
    fireEvent.click(
      screen.getByRole("button", { name: "chatView.block.actionReference" }),
    );
    expect(onJump).toHaveBeenCalledTimes(1);
    expect(onReference).toHaveBeenCalledTimes(1);
  });

  it("opens the drawer on hover and keeps it open while a draft holds", async () => {
    renderRow();
    expect(drawer().getAttribute("data-state")).toBe("closed");
    fireEvent.mouseEnter(drawer().parentElement as HTMLElement);
    expect(drawer().getAttribute("data-state")).toBe("open");
    const box = screen.getByRole("textbox");
    fireEvent.change(box, { target: { value: "keep typing" } });
    fireEvent.mouseLeave(drawer().parentElement as HTMLElement);
    await waitFor(() => expect(drawer().getAttribute("data-state")).toBe("open"));
  });

  it("closes an empty drawer after the pointer leaves", async () => {
    renderRow();
    const row = drawer().parentElement as HTMLElement;
    fireEvent.mouseEnter(row);
    fireEvent.mouseLeave(row);
    await waitFor(() =>
      expect(drawer().getAttribute("data-state")).toBe("closed"),
    );
  });

  it("takes the close back when the pointer returns during the grace period", async () => {
    renderRow();
    const row = drawer().parentElement as HTMLElement;
    fireEvent.mouseEnter(row);
    fireEvent.mouseLeave(row);
    // Back inside before the grace elapses: a cursor crossing the row must not
    // strobe the field.
    fireEvent.mouseEnter(row);
    await new Promise((r) => setTimeout(r, 150));
    expect(drawer().getAttribute("data-state")).toBe("open");
  });

  it("keeps a closed drawer out of the tab order", () => {
    renderRow();
    expect(drawer().getAttribute("data-state")).toBe("closed");
    // `aria-hidden` + `inert`: the field exists but is not reachable or readable
    // while the capsule is clipped.
    expect(screen.queryByRole("textbox")).toBeNull();
  });

  it("opens on the message button (the pointer-free entry)", async () => {
    renderRow();
    fireEvent.click(
      screen.getByRole("button", { name: "chatView.block.replyPlaceholder" }),
    );
    await waitFor(() =>
      expect(document.activeElement).toBe(screen.getByRole("textbox")),
    );
    expect(drawer().getAttribute("data-state")).toBe("open");
  });

  it("opens and focuses on the locate signal's rising edge", async () => {
    const { rerender } = renderRow();
    fireEvent.mouseLeave(drawer().parentElement as HTMLElement);
    rerender(
      <RunBlockActionRow
        runName="run"
        copyPayload="x"
        onSend={vi.fn(async () => {})}
        focusSignal
      />,
    );
    await waitFor(() =>
      expect(document.activeElement).toBe(screen.getByRole("textbox")),
    );
  });

  it("sends on Enter and keeps the drawer for the next follow-up", async () => {
    const onSend = vi.fn(async () => {});
    renderRow({ onSend });
    fireEvent.mouseEnter(drawer().parentElement as HTMLElement);
    const box = screen.getByRole("textbox") as HTMLTextAreaElement;
    fireEvent.change(box, { target: { value: "  continue please  " } });
    fireEvent.keyDown(box, { key: "Enter" });
    await waitFor(() =>
      expect(onSend).toHaveBeenCalledWith("continue please", []),
    );
    expect(box.value).toBe("");
    expect(drawer().getAttribute("data-state")).toBe("open");
  });

  it("refuses to send an empty or unattachable draft", async () => {
    const onSend = vi.fn(async () => {});
    renderRow({ onSend, canSend: false });
    fireEvent.mouseEnter(drawer().parentElement as HTMLElement);
    const box = screen.getByRole("textbox");
    fireEvent.change(box, { target: { value: "no round to address" } });
    fireEvent.keyDown(box, { key: "Enter" });
    expect(onSend).not.toHaveBeenCalled();
  });

  it("wears a check after a copy and reports nothing when the copy fails", async () => {
    renderRow();
    fireEvent.click(
      screen.getByRole("button", { name: "chatView.block.actionCopy" }),
    );
    await waitFor(() =>
      expect(
        screen.getByRole("button", { name: "chatView.block.actionCopy" })
          .getAttribute("title"),
      ).toBe("chatView.block.copied"),
    );
    expect(copyText).toHaveBeenCalledWith("# Fix the login redirect");
  });

  it("does not claim a copy the clipboard refused", async () => {
    vi.mocked(copyText).mockResolvedValueOnce(false);
    renderRow();
    fireEvent.click(
      screen.getByRole("button", { name: "chatView.block.actionCopy" }),
    );
    await waitFor(() => expect(copyText).toHaveBeenCalled());
    expect(
      screen.getByRole("button", { name: "chatView.block.actionCopy" })
        .getAttribute("title"),
    ).toBe("chatView.block.actionCopy");
  });

  it("disables the copy button when there is nothing to copy", () => {
    renderRow({ copyPayload: "" });
    const button = screen.getByRole("button", {
      name: "chatView.block.actionCopy",
    }) as HTMLButtonElement;
    expect(button.disabled).toBe(true);
  });

  it("lights the message button for as long as its drawer is open", async () => {
    renderRow();
    const row = drawer().parentElement as HTMLElement;
    const reply = () =>
      screen.getByRole("button", {
        name: "chatView.block.replyPlaceholder",
      }) as HTMLButtonElement;

    expect(isLit(reply())).toBe(false);
    fireEvent.mouseEnter(row);
    expect(drawer().getAttribute("data-state")).toBe("open");
    // The drawer hangs off the button, so the button owns the highlight while
    // the field is open — including once the pointer has left the button itself
    // to type in the field (the CSS `:hover` the row used to rely on is gone by
    // then, which is exactly the flicker this locks out).
    expect(isLit(reply())).toBe(true);

    // Closed means dark again: the tint's lifetime is the drawer's.
    fireEvent.mouseLeave(row);
    await waitFor(() =>
      expect(drawer().getAttribute("data-state")).toBe("closed"),
    );
    expect(isLit(reply())).toBe(false);
  });

  it("keeps the message button lit after a latch lets go of the pointer", async () => {
    renderRow();
    fireEvent.click(
      screen.getByRole("button", { name: "chatView.block.replyPlaceholder" }),
    );
    await waitFor(() =>
      expect(document.activeElement).toBe(screen.getByRole("textbox")),
    );
    const reply = screen.getByRole("button", {
      name: "chatView.block.replyPlaceholder",
    });
    // Latched open with the caret in the field: no pointer involved at all.
    expect(isLit(reply)).toBe(true);
  });

  describe("run-management controls (the composer's +, usage, visibility)", () => {
    /** The row without the block's run address — the pre-existing shape. */
    it("renders none of the three without the run's agent/session address", () => {
      renderRow();
      expect(screen.queryByTestId("plus-menu")).toBeNull();
      expect(screen.queryByTestId("usage")).toBeNull();
      expect(screen.queryByTestId("visibility")).toBeNull();
    });

    it("addresses all three at the RUN, not at the surface's selection", () => {
      renderRow({ agentId: "ag-1", sessionId: "s-1", chatSessionId: "c1" });
      expect(screen.getByTestId("usage").getAttribute("data-session")).toBe("s-1");
      expect(screen.getByTestId("visibility").getAttribute("data-session")).toBe("s-1");
      expect(plusProps.current.agentId).toBe("ag-1");
      expect(plusProps.current.sessionId).toBe("s-1");
      // The block variant hides the Skills row (it rides the exec composer's
      // pending-send state, which this drawer does not have), opens down, and
      // portals the panel out of the transcript's clipping scroll container.
      expect(plusProps.current.showSkills).toBe(false);
      expect(plusProps.current.placement).toBe("bottom");
      expect(plusProps.current.portal).toBe(true);
    });

    it("gates the + menu on the answerer, not on the viewer", () => {
      renderRow({ agentId: "ag-1", sessionId: "s-1", chatSessionId: "c1", canManage: false });
      expect(plusProps.current.readOnly).toBe(true);
    });

    it("hides the usage ring when the store has no context usage for the run", () => {
      // A never-opened, never-pushed run would draw the ring at 0% — a lie.
      renderRow({ agentId: "ag-1", sessionId: "s-2", chatSessionId: "c1" });
      expect(screen.queryByTestId("usage")).toBeNull();
      // The other two stay: their endpoints read AND write persisted
      // session meta, so they work on a closed run.
      expect(screen.getByTestId("plus-menu")).toBeTruthy();
      expect(screen.getByTestId("visibility")).toBeTruthy();
    });

    it("uploads files picked via the + menu into the chat session, chips them in the drawer, and sends their ids with the next continuation", async () => {
      const onSend = vi.fn(async () => {});
      renderRow({ onSend, agentId: "ag-1", sessionId: "s-1", chatSessionId: "c1" });
      // The menu's Attachments row drives the row's hidden file input.
      const pick = plusProps.current.onPickAttachments as () => void;
      const input = document.querySelector('input[type="file"]') as HTMLInputElement;
      pick();
      fireEvent.change(input, {
        target: { files: [new File(["pdf bytes"], "spec.pdf", { type: "application/pdf" })] },
      });
      await waitFor(() =>
        expect(uploadMock).toHaveBeenCalledWith("c1", expect.any(File)),
      );
      expect(document.querySelector('[data-run-attachment="att-spec.pdf"]')).not.toBeNull();

      fireEvent.mouseEnter(drawer().parentElement as HTMLElement);
      const box = screen.getByRole("textbox") as HTMLTextAreaElement;
      fireEvent.change(box, { target: { value: "see the spec" } });
      fireEvent.keyDown(box, { key: "Enter" });
      await waitFor(() =>
        expect(onSend).toHaveBeenCalledWith("see the spec", ["att-spec.pdf"]),
      );
      // The file rode THAT send — it is not implicit context for the next one.
      expect(document.querySelector('[data-run-attachment="att-spec.pdf"]')).toBeNull();
    });

    it("drops a pending attachment before sending when its chip is removed", async () => {
      const onSend = vi.fn(async () => {});
      renderRow({ onSend, agentId: "ag-1", sessionId: "s-1", chatSessionId: "c1" });
      const input = document.querySelector('input[type="file"]') as HTMLInputElement;
      fireEvent.change(input, {
        target: { files: [new File(["x"], "gone.txt", { type: "text/plain" })] },
      });
      await waitFor(() =>
        expect(document.querySelector('[data-run-attachment="att-gone.txt"]')).not.toBeNull(),
      );
      // The chips live inside the drawer capsule, which is `aria-hidden` while
      // closed — open it before the remove button is reachable by role.
      fireEvent.mouseEnter(drawer().parentElement as HTMLElement);
      fireEvent.click(screen.getByRole("button", { name: "chatPanel.removeAttachment" }));
      expect(document.querySelector('[data-run-attachment="att-gone.txt"]')).toBeNull();

      fireEvent.mouseEnter(drawer().parentElement as HTMLElement);
      const box = screen.getByRole("textbox") as HTMLTextAreaElement;
      fireEvent.change(box, { target: { value: "just text" } });
      fireEvent.keyDown(box, { key: "Enter" });
      await waitFor(() => expect(onSend).toHaveBeenCalledWith("just text", []));
    });
  });
});
