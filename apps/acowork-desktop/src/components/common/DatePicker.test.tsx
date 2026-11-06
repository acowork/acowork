//! The `DatePicker` contract — the picker that replaced `<input type="date">`.
//!
//! What is pinned here is everything the native popup did and a hand-rolled one
//! would get wrong:
//!
//!   1. the value it reports is a local calendar day `YYYY-MM-DD` (`""` = 清除),
//!      i.e. exactly the shape the task form stores;
//!   2. it opens on the value's month, and picking / clearing dismisses it;
//!   3. keyboard operation (arrows, PageUp/PageDown, Enter, Escape) works and
//!      **Escape never reaches `window`** — the task dialog closes itself on a
//!      window-level Escape, and dismissing a calendar must not discard a form;
//!   4. a click outside dismisses it, a click inside does not.
//!
//! Visual facts (which is to say, the reason this component exists) are CSS, so
//! they are checked at the source level: the trigger composes the shared field
//! surface and the footer uses the registered button tones. A jsdom render
//! cannot see either.

import { describe, expect, it, vi } from "vitest";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { fireEvent, render, screen } from "@testing-library/react";
import { DatePicker } from "./DatePicker";
import { todayDateField } from "./calendarGrid";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (k: string) => k, i18n: { language: "en" } }),
}));

const __dirname = dirname(fileURLToPath(import.meta.url));

function setup(value = "") {
  const onChange = vi.fn();
  const view = render(<DatePicker value={value} onChange={onChange} ariaLabel="date" />);
  return { ...view, onChange, trigger: () => screen.getByTestId("date-picker-trigger") };
}

/** The day cell for a `YYYY-MM-DD` value (cells carry the ISO day as a name). */
const day = (iso: string) => screen.getByRole("button", { name: iso });

function open(value = "") {
  const ctx = setup(value);
  fireEvent.click(ctx.trigger());
  return ctx;
}

describe("DatePicker trigger", () => {
  it("shows the placeholder while the field is empty, the day once it is set", () => {
    const { trigger } = setup("");
    expect(trigger().textContent).toContain("datePicker.placeholder");
    expect(trigger().getAttribute("aria-expanded")).toBe("false");
  });

  it("shows the stored local calendar day verbatim", () => {
    const { trigger } = setup("2026-11-06");
    expect(trigger().textContent).toContain("2026-11-06");
  });

  it("does not open while disabled", () => {
    const onChange = vi.fn();
    render(<DatePicker value="" onChange={onChange} disabled ariaLabel="date" />);
    fireEvent.click(screen.getByTestId("date-picker-trigger"));
    expect(screen.queryByTestId("date-picker-popup")).toBeNull();
  });

  it("announces itself as a popup trigger", () => {
    const { trigger } = open("2026-11-06");
    expect(trigger().getAttribute("aria-haspopup")).toBe("dialog");
    expect(trigger().getAttribute("aria-expanded")).toBe("true");
  });
});

describe("DatePicker popup", () => {
  it("opens on the month of the value and marks the selected day", () => {
    open("2026-11-06");
    expect(screen.getByTestId("date-picker-popup").textContent).toContain("2026-11");
    expect(day("2026-11-06").getAttribute("aria-pressed")).toBe("true");
    expect(day("2026-11-05").getAttribute("aria-pressed")).toBe("false");
  });

  it("opens on today's month when the field is empty", () => {
    open("");
    const today = todayDateField();
    expect(screen.getByTestId("date-picker-popup").textContent).toContain(today.slice(0, 7));
    expect(day(today).getAttribute("aria-current")).toBe("date");
  });

  it("renders a fixed 6×7 grid, including the borrowed adjacent days", () => {
    open("2026-11-06");
    // November 2026 starts on a Sunday, so October 26th leads the grid — the
    // same first row the native popup drew.
    expect(day("2026-10-26").textContent).toBe("26");
    expect(day("2026-12-06").textContent).toBe("6");
  });

  it("reports the picked day as a local calendar day and dismisses itself", () => {
    const { onChange } = open("2026-11-06");
    fireEvent.click(day("2026-11-20"));
    expect(onChange).toHaveBeenCalledWith("2026-11-20");
    expect(screen.queryByTestId("date-picker-popup")).toBeNull();
  });

  it("clears to an empty field", () => {
    const { onChange } = open("2026-11-06");
    fireEvent.click(screen.getByRole("button", { name: "datePicker.clear" }));
    expect(onChange).toHaveBeenCalledWith("");
    expect(screen.queryByTestId("date-picker-popup")).toBeNull();
  });

  it("offers 清除 only when there is something to clear", () => {
    open("");
    expect(
      (screen.getByRole("button", { name: "datePicker.clear" }) as HTMLButtonElement).disabled,
    ).toBe(true);
  });

  it("picks today without walking the grid", () => {
    const { onChange } = open("");
    fireEvent.click(screen.getByRole("button", { name: "datePicker.today" }));
    expect(onChange).toHaveBeenCalledWith(todayDateField());
  });

  it("steps months with the header buttons and follows the year boundary", () => {
    open("2026-12-15");
    expect(screen.getByTestId("date-picker-popup").textContent).toContain("2026-12");
    fireEvent.click(screen.getByRole("button", { name: "datePicker.nextMonth" }));
    expect(screen.getByTestId("date-picker-popup").textContent).toContain("2027-01");
    fireEvent.click(screen.getByRole("button", { name: "datePicker.prevMonth" }));
    fireEvent.click(screen.getByRole("button", { name: "datePicker.prevMonth" }));
    expect(screen.getByTestId("date-picker-popup").textContent).toContain("2026-11");
  });

  it("clamps the day when a month is shorter", () => {
    // 2026-01-31 stepped into February must not become 2026-03-03.
    const { onChange } = open("2026-01-31");
    fireEvent.click(screen.getByRole("button", { name: "datePicker.nextMonth" }));
    expect(screen.getByTestId("date-picker-popup").textContent).toContain("2026-02");
    fireEvent.keyDown(day("2026-02-28"), { key: "Enter" });
    expect(onChange).toHaveBeenCalledWith("2026-02-28");
  });
});

describe("DatePicker dismissal", () => {
  it("closes on a click outside, stays open on a click inside", () => {
    open("2026-11-06");
    fireEvent.mouseDown(day("2026-11-20"));
    expect(screen.queryByTestId("date-picker-popup")).not.toBeNull();
    fireEvent.mouseDown(document.body);
    expect(screen.queryByTestId("date-picker-popup")).toBeNull();
  });

  it("closes on Escape, returns focus to the trigger, and never lets the key reach window", () => {
    // The task dialog closes itself on a window-level Escape; swallowing the
    // calendar must not throw away a half-written task.
    const windowEscape = vi.fn();
    const onWindowKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") windowEscape();
    };
    window.addEventListener("keydown", onWindowKeyDown);
    try {
      const ctx = open("2026-11-06");
      fireEvent.keyDown(day("2026-11-06"), { key: "Escape" });
      expect(screen.queryByTestId("date-picker-popup")).toBeNull();
      expect(windowEscape).not.toHaveBeenCalled();
      expect(document.activeElement).toBe(ctx.trigger());
    } finally {
      window.removeEventListener("keydown", onWindowKeyDown);
    }
  });

  it("reopens on the picked month, not the one it was left on", () => {
    const onChange = vi.fn();
    const view = render(<DatePicker value="2026-11-06" onChange={onChange} ariaLabel="date" />);
    fireEvent.click(screen.getByTestId("date-picker-trigger"));
    fireEvent.click(screen.getByRole("button", { name: "datePicker.nextMonth" }));
    fireEvent.keyDown(day("2026-12-06"), { key: "Escape" });
    view.rerender(<DatePicker value="2026-12-20" onChange={onChange} ariaLabel="date" />);
    fireEvent.click(screen.getByTestId("date-picker-trigger"));
    expect(screen.getByTestId("date-picker-popup").textContent).toContain("2026-12");
    expect(day("2026-12-20").getAttribute("aria-pressed")).toBe("true");
  });
});

describe("DatePicker keyboard", () => {
  const focusedIso = () => (document.activeElement as HTMLElement | null)?.getAttribute("aria-label");

  it("hands the keyboard to the selected day when it opens", () => {
    open("2026-11-06");
    expect(focusedIso()).toBe("2026-11-06");
  });

  it("walks days and weeks with the arrow keys", () => {
    open("2026-11-06");
    fireEvent.keyDown(day("2026-11-06"), { key: "ArrowRight" });
    expect(focusedIso()).toBe("2026-11-07");
    fireEvent.keyDown(day("2026-11-07"), { key: "ArrowDown" });
    expect(focusedIso()).toBe("2026-11-14");
    fireEvent.keyDown(day("2026-11-14"), { key: "ArrowLeft" });
    fireEvent.keyDown(day("2026-11-13"), { key: "ArrowUp" });
    expect(focusedIso()).toBe("2026-11-06");
  });

  it("crosses a month boundary with the arrows, taking the grid with it", () => {
    open("2026-11-30");
    fireEvent.keyDown(day("2026-11-30"), { key: "ArrowRight" });
    expect(focusedIso()).toBe("2026-12-01");
    expect(screen.getByTestId("date-picker-popup").textContent).toContain("2026-12");
  });

  it("walks months with PageUp / PageDown", () => {
    open("2026-11-06");
    fireEvent.keyDown(day("2026-11-06"), { key: "PageDown" });
    expect(focusedIso()).toBe("2026-12-06");
    expect(screen.getByTestId("date-picker-popup").textContent).toContain("2026-12");
    fireEvent.keyDown(day("2026-12-06"), { key: "PageUp" });
    expect(focusedIso()).toBe("2026-11-06");
  });

  it("leaves Enter alone outside the grid — the month arrows own their key", () => {
    // The arrows are ordinary buttons: Enter on 下个月 must not be read as
    // "pick the focused day" just because it happened inside the popup.
    const { onChange } = open("2026-11-06");
    fireEvent.keyDown(screen.getByRole("button", { name: "datePicker.nextMonth" }), {
      key: "Enter",
    });
    expect(onChange).not.toHaveBeenCalled();
    expect(screen.getByTestId("date-picker-popup")).toBeTruthy();
  });

  it("picks the focused day with Enter", () => {
    const { onChange } = open("2026-11-06");
    fireEvent.keyDown(day("2026-11-06"), { key: "ArrowRight" });
    fireEvent.keyDown(day("2026-11-07"), { key: "Enter" });
    expect(onChange).toHaveBeenCalledWith("2026-11-07");
  });
});

describe("DatePicker reuses the design system", () => {
  const source = readFileSync(resolve(__dirname, "DatePicker.tsx"), "utf8");

  it("takes its surface from the shared field constant, not a copied class string", () => {
    // Composition, not a duplicated string: the trigger and `StyledInput` must
    // change together or the defensively-copied copy drifts silently.
    expect(source).toContain("FIELD_SURFACE_CLASS");
    expect(source).not.toContain("border-input-border");
  });

  it("sits on the same surface as the text fields beside it", () => {
    const { trigger } = setup("2026-11-06");
    const classes = trigger().className;
    expect(classes).toContain("border-input-border");
    expect(classes).toContain("bg-input-bg");
    expect(classes).toContain("px-3");
    expect(classes).toContain("py-[var(--ui-input-py)]");
  });

  it("dresses its footer in the registered button tones", () => {
    open("2026-11-06");
    const clear = screen.getByRole("button", { name: "datePicker.clear" });
    const today = screen.getByRole("button", { name: "datePicker.today" });
    expect(clear.className).toContain("btn-solid");
    expect(today.className).toContain("btn-accent");
    for (const button of [clear, today]) {
      // One padding token for both, which is what keeps the fill behind the
      // label instead of hanging off it.
      expect(button.className).toContain("py-[var(--ui-btn-py)]");
      expect(button.className).toContain("px-3");
    }
  });

  it("sizes its own month arrows instead of inheriting a platform control", () => {
    // The oversized arrows were the reported defect; a bounded icon button is
    // the fix, so the fixed box is the contract.
    open("2026-11-06");
    for (const name of ["datePicker.prevMonth", "datePicker.nextMonth"]) {
      const button = screen.getByRole("button", { name });
      expect(button.className).toContain("h-7");
      expect(button.className).toContain("w-7");
      expect(button.querySelector("svg")?.getAttribute("class")).toContain("h-3.5");
    }
  });

  it("renders no native date input — nothing for WebView2 to draw for us", () => {
    const { container } = setup("2026-11-06");
    expect(container.querySelector("input")).toBeNull();
    expect(screen.getByTestId("date-picker-trigger").tagName).toBe("BUTTON");
  });
});
