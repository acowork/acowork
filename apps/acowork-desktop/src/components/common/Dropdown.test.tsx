/**
 * Dropdown — the themed select's visual + interaction contract.
 *
 * Two halves:
 *  - source/class assertions for the accent treatment (jsdom does not resolve
 *    Tailwind arbitrary values such as `bg-[var(--color-accent)]/15`, so the
 *    class strings ARE the spec — same approach as menuItemSelected.test.tsx);
 *  - rendered-DOM assertions for the popup, keyboard and the hidden native
 *    carrier that keeps every existing form/event/a11y contract intact.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { Dropdown } from "./Dropdown";
import {
  DROPDOWN_ITEM_SELECTED_CLASS,
  DROPDOWN_TRIGGER_ACTIVE_CLASS,
  DROPDOWN_TRIGGER_CLASS,
  DROPDOWN_TRIGGER_SMALL_CLASS,
} from "./dropdownTheme";
import { FIELD_SURFACE_CLASS } from "./StyledInput";
import { listRowSelected } from "../../lib/ui-styles";

const OPTIONS = [
  { value: "a", label: "Alpha" },
  { value: "b", label: "Beta" },
  { value: "c", label: "Gamma" },
];

const __dirname = dirname(fileURLToPath(import.meta.url));
const readSource = (rel: string) => readFileSync(resolve(__dirname, rel), "utf8");

describe("dropdown theme", () => {
  it("the trigger IS the input field surface — accent border on focus", () => {
    expect(DROPDOWN_TRIGGER_CLASS).toContain("focus:border-[var(--color-accent)]");
    expect(FIELD_SURFACE_CLASS).toContain("focus:border-[var(--color-accent)]");
    expect(DROPDOWN_TRIGGER_CLASS).not.toContain("zinc-400");
    expect(DROPDOWN_TRIGGER_CLASS).not.toContain("focus:ring-zinc");
  });

  it("both size variants share one colour treatment — geometry only differs", () => {
    expect(DROPDOWN_TRIGGER_SMALL_CLASS).not.toContain("focus:border-zinc");
    expect(DROPDOWN_TRIGGER_SMALL_CLASS).not.toContain("zinc");
    expect(DROPDOWN_TRIGGER_SMALL_CLASS).not.toContain("text-text-");
    expect(DROPDOWN_TRIGGER_SMALL_CLASS).not.toContain("bg-");
    expect(DROPDOWN_TRIGGER_SMALL_CLASS).toContain("h-7");
    const src = readSource("./Dropdown.tsx");
    expect(src).not.toContain("focus:border-zinc-400");
    expect(src).not.toContain("focus:ring-zinc-400");
    expect(src).not.toContain("bg-dropdown-bg");
  });

  it("the accent-active recipe has exactly one source", () => {
    // A second literal copy in ui-styles.ts used to drift from the theme.
    expect(readSource("../../lib/ui-styles.ts")).not.toContain(
      "export const toolbarButtonActive",
    );
    expect(DROPDOWN_TRIGGER_ACTIVE_CLASS).toContain(listRowSelected);
  });

  it("a selected row wears the menu's accent wash, not a neutral grey", () => {
    expect(DROPDOWN_ITEM_SELECTED_CLASS).toBe(listRowSelected);
    expect(DROPDOWN_ITEM_SELECTED_CLASS).toMatch(/bg-\[var\(--color-accent\)\]/);
    expect(DROPDOWN_ITEM_SELECTED_CLASS).not.toContain("zinc");
    expect(DROPDOWN_TRIGGER_ACTIVE_CLASS).toContain("bg-[var(--color-accent)]");
  });

  it("the option list is ours (portal + app scrollbar), not WebView2's", () => {
    const src = readSource("./Dropdown.tsx");
    expect(src).toContain("createPortal");
    expect(src).toContain("OverlayScrollbar");
    expect(src).toContain('role="listbox"');
    expect(src).toContain('role="option"');
  });

  it("every dropdown-like surface consumes the shared theme", () => {
    const consumers = [
      "../../components/common/ToolbarDropdown.tsx",
      "../../components/layout/titleBarMenu.ts",
      "../../views/pm/pmPopover.tsx",
      "../../components/settings/UrlComboBox.tsx",
    ];
    for (const rel of consumers) {
      const src = readSource(rel);
      expect(src, `${rel} must import dropdownTheme`).toContain("dropdownTheme");
    }
  });
});

describe("Dropdown behaviour", () => {
  it("keeps the hidden native combobox + option list for form/event contracts", () => {
    render(<Dropdown value="b" onChange={() => {}} options={OPTIONS} />);
    const select = screen.getByRole("combobox") as HTMLSelectElement;
    expect(Array.from(select.querySelectorAll("option")).map((o) => o.value)).toEqual([
      "a",
      "b",
      "c",
    ]);
    expect(select.value).toBe("b");
    expect(select.tabIndex).toBe(-1);
  });

  it("opens an accent-bordered trigger and marks the current row with an accent check", () => {
    render(<Dropdown value="b" onChange={() => {}} options={OPTIONS} aria-label="Pick" />);
    const trigger = screen.getByTestId("dropdown-trigger") as HTMLElement;
    expect(trigger.className).toContain("focus:border-[var(--color-accent)]");
    fireEvent.click(trigger);
    expect(trigger.getAttribute("aria-expanded")).toBe("true");
    expect(trigger.style.borderColor).toBe("var(--color-accent)");

    const rows = screen.getAllByRole("option");
    expect(rows).toHaveLength(3);
    expect(rows[1].getAttribute("aria-selected")).toBe("true");
    expect(rows[1].className).toMatch(/bg-\[var\(--color-accent\)\]/);
    expect(rows[1].querySelector("svg")).not.toBeNull();
  });

  it("picks a row by click, fires onChange and closes", () => {
    const onChange = vi.fn();
    render(<Dropdown value="a" onChange={onChange} options={OPTIONS} />);
    fireEvent.click(screen.getByTestId("dropdown-trigger"));
    fireEvent.click(screen.getByRole("option", { name: "Gamma" }));
    expect(onChange).toHaveBeenCalledWith("c");
    expect(screen.queryByTestId("dropdown-menu")).toBeNull();
  });

  it("supports arrow keys + Enter and closes on Escape", () => {
    const onChange = vi.fn();
    render(<Dropdown value="a" onChange={onChange} options={OPTIONS} />);
    const trigger = screen.getByTestId("dropdown-trigger");
    fireEvent.keyDown(trigger, { key: "ArrowDown" });
    const menu = screen.getByTestId("dropdown-menu");
    fireEvent.keyDown(menu, { key: "ArrowDown" });
    fireEvent.keyDown(menu, { key: "Enter" });
    expect(onChange).toHaveBeenCalledWith("b");

    fireEvent.click(trigger);
    fireEvent.keyDown(screen.getByTestId("dropdown-menu"), { key: "Escape" });
    expect(screen.queryByTestId("dropdown-menu")).toBeNull();
  });

  it("still fires onChange from the native carrier (legacy change contract)", () => {
    const onChange = vi.fn();
    render(<Dropdown value="a" onChange={onChange} options={OPTIONS} />);
    fireEvent.change(screen.getByRole("combobox"), { target: { value: "c" } });
    expect(onChange).toHaveBeenCalledWith("c");
  });

  it("shows a non-selectable placeholder on the trigger but not in the menu", () => {
    render(
      <Dropdown
        value=""
        onChange={() => {}}
        options={OPTIONS}
        placeholder={{ value: "", label: "Choose one" }}
      />,
    );
    expect(screen.getByTestId("dropdown-trigger").textContent).toContain("Choose one");
    fireEvent.click(screen.getByTestId("dropdown-trigger"));
    expect(screen.getAllByRole("option")).toHaveLength(3);
  });

  it("a selectable placeholder IS a real row", () => {
    render(
      <Dropdown
        value=""
        onChange={() => {}}
        options={OPTIONS}
        placeholder={{ value: "", label: "Choose one", selectable: true }}
      />,
    );
    fireEvent.click(screen.getByTestId("dropdown-trigger"));
    const rows = screen.getAllByRole("option");
    expect(rows).toHaveLength(4);
    expect(rows[0].getAttribute("aria-selected")).toBe("true");
  });

  it("applies style/title to the visible trigger, never to the hidden carrier", () => {
    render(
      <Dropdown
        value="a"
        onChange={() => {}}
        options={OPTIONS}
        title="Pick one"
        style={{ maxWidth: 120 }}
      />,
    );
    const trigger = screen.getByTestId("dropdown-trigger") as HTMLElement;
    const carrier = screen.getByRole("combobox") as HTMLSelectElement;
    expect(trigger.title).toBe("Pick one");
    expect(trigger.style.maxWidth).toBe("120px");
    expect(carrier.title).toBe("");
    expect(carrier.style.maxWidth).toBe("");
    // The sr-only invariants are load-bearing and must survive a caller's style.
    expect(carrier.style.position).toBe("absolute");
    expect(carrier.style.pointerEvents).toBe("none");
  });

  it("a disabled trigger cannot open the menu", () => {
    render(<Dropdown value="a" onChange={() => {}} options={OPTIONS} disabled />);
    fireEvent.click(screen.getByTestId("dropdown-trigger"));
    expect(screen.queryByTestId("dropdown-menu")).toBeNull();
  });

  it("closes on an outside mousedown", () => {
    render(
      <>
        <span data-testid="outside">outside</span>
        <Dropdown value="a" onChange={() => {}} options={OPTIONS} />
      </>,
    );
    fireEvent.click(screen.getByTestId("dropdown-trigger"));
    expect(screen.getByTestId("dropdown-menu")).toBeTruthy();
    fireEvent.mouseDown(screen.getByTestId("outside"));
    expect(screen.queryByTestId("dropdown-menu")).toBeNull();
  });

  it("mouse hover does not steal focus from the keyboard cursor", () => {
    render(<Dropdown value="a" onChange={() => {}} options={OPTIONS} />);
    fireEvent.keyDown(screen.getByTestId("dropdown-trigger"), { key: "ArrowDown" });
    fireEvent.keyDown(screen.getByTestId("dropdown-menu"), { key: "ArrowDown" });
    expect(document.activeElement?.textContent).toContain("Beta");
    fireEvent.mouseEnter(screen.getAllByRole("option")[2]);
    expect(document.activeElement?.textContent).toContain("Beta");
  });
});
