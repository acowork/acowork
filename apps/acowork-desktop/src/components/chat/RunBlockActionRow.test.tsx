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

/** The drawer capsule. `data-state` is the contract; class names are not. */
function drawer(): HTMLElement {
  const el = document.querySelector('[data-run-action="reply"]');
  if (!el) throw new Error("no reply drawer rendered");
  return el as HTMLElement;
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
      expect(onSend).toHaveBeenCalledWith("continue please"),
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
});
