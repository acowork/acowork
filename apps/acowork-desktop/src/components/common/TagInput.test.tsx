//! ADR-104: interaction contract of the shared chip input.
//!
//! These tests pin the four things that would otherwise be re-derived by every
//! caller, and that a dialog would get subtly wrong:
//!
//!   1. Enter / comma / blur all commit, and Enter never submits the form
//!   2. a comma-separated paste splits into chips (tail stays editable text)
//!   3. Backspace on an empty input removes the last chip
//!   4. policy is the caller's: the component reports the raw token and lets
//!      the caller decide, so a rejected token leaves no chip behind
//!
//! What is *not* tested here: label validity. That rule belongs to
//! `lib/manifestRules.ts` and is pinned by its own suite.

import { describe, it, expect, vi } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { TagInput } from "./TagInput";

function setup(initial: string[] = []) {
  const onAdd = vi.fn();
  const onRemove = vi.fn();
  const view = render(
    <TagInput
      value={initial}
      onAdd={onAdd}
      onRemove={onRemove}
      placeholder="add a label"
      ariaLabel="labels"
      removeLabel="Remove label"
    />,
  );
  return { ...view, onAdd, onRemove, input: screen.getByLabelText("labels") };
}

describe("TagInput", () => {
  it("renders one chip per value entry", () => {
    setup(["project-management", "agile"]);
    expect(screen.getByText("project-management")).toBeTruthy();
    expect(screen.getByText("agile")).toBeTruthy();
  });

  it("commits on Enter and clears the input", () => {
    const { input, onAdd } = setup();
    fireEvent.change(input, { target: { value: "agile" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onAdd).toHaveBeenCalledWith("agile");
    expect((input as HTMLInputElement).value).toBe("");
  });

  it("does not bubble Enter into a surrounding form submit", () => {
    const onSubmit = vi.fn();
    const onAdd = vi.fn();
    render(
      <form onSubmit={onSubmit}>
        <TagInput value={[]} onAdd={onAdd} onRemove={() => {}} ariaLabel="labels" />
      </form>,
    );
    const input = screen.getByLabelText("labels");
    fireEvent.change(input, { target: { value: "x" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onAdd).toHaveBeenCalledWith("x");
    expect(onSubmit).not.toHaveBeenCalled();
  });

  it("splits a comma-separated paste into chips and keeps the tail editable", () => {
    const { input, onAdd } = setup();
    fireEvent.change(input, { target: { value: "a-b, c-d,ef" } });
    expect(onAdd.mock.calls.map((c) => c[0])).toEqual(["a-b", "c-d"]);
    expect((input as HTMLInputElement).value).toBe("ef");
  });

  it("commits on blur so a typed-but-not-entered chip is not lost", () => {
    const { input, onAdd } = setup();
    fireEvent.change(input, { target: { value: "agile" } });
    fireEvent.blur(input);
    expect(onAdd).toHaveBeenCalledWith("agile");
  });

  it("removes the last chip on Backspace when the input is empty", () => {
    const { input, onRemove } = setup(["a-b", "c-d"]);
    fireEvent.keyDown(input, { key: "Backspace" });
    expect(onRemove).toHaveBeenCalledWith(1);
  });

  it("does not remove a chip on Backspace while text is present", () => {
    const { input, onRemove } = setup(["a-b"]);
    fireEvent.change(input, { target: { value: "z" } });
    fireEvent.keyDown(input, { key: "Backspace" });
    expect(onRemove).not.toHaveBeenCalled();
  });

  it("removes by index, so duplicate chips stay individually removable", () => {
    const { onRemove } = setup(["a-b", "a-b"]);
    fireEvent.click(screen.getAllByLabelText("Remove label a-b")[1]);
    expect(onRemove).toHaveBeenCalledWith(1);
  });

  it("reports a rejected token and leaves no chip behind when the caller ignores it", () => {
    // The caller here accepts nothing — the "invalid label" path.
    const { input, onAdd } = setup([]);
    fireEvent.change(input, { target: { value: "Not Valid" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onAdd).toHaveBeenCalledWith("Not Valid");
    // No chip rendered, input cleared: the caller's error message is the only
    // feedback, which is exactly the split this component promises.
    expect(screen.queryByText("Not Valid")).toBeNull();
    expect((input as HTMLInputElement).value).toBe("");
  });

  it("ignores empty commits", () => {
    const { input, onAdd } = setup([]);
    fireEvent.change(input, { target: { value: "   " } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onAdd).not.toHaveBeenCalled();
  });

  it("refuses to add or remove while disabled", () => {
    const onAdd = vi.fn();
    const onRemove = vi.fn();
    render(
      <TagInput
        value={["a-b"]}
        onAdd={onAdd}
        onRemove={onRemove}
        disabled
        ariaLabel="labels"
        removeLabel="Remove label"
      />,
    );
    fireEvent.click(screen.getByLabelText("Remove label a-b"));
    expect(onRemove).not.toHaveBeenCalled();
    const input = screen.getByLabelText("labels") as HTMLInputElement;
    expect(input.disabled).toBe(true);
  });
});