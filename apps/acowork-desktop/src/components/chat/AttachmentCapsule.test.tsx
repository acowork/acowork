// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { AttachmentCapsule, formatAttachmentSize } from "./AttachmentCapsule";

describe("AttachmentCapsule", () => {
  it("renders a decorative span, never a button, without onClick", () => {
    render(<AttachmentCapsule title="report.pdf">report.pdf</AttachmentCapsule>);
    expect(screen.queryByRole("button")).toBeNull();
    expect(screen.getByTitle("report.pdf").tagName).toBe("SPAN");
  });

  it("renders a real button when it can be clicked", () => {
    const onClick = vi.fn();
    render(
      <AttachmentCapsule onClick={onClick} title="open report.pdf">
        report.pdf
      </AttachmentCapsule>,
    );
    const button = screen.getByRole("button", { name: "open report.pdf" });
    button.click();
    expect(onClick).toHaveBeenCalledTimes(1);
  });

  it("is inert while pending, whatever the caller passed", () => {
    const onClick = vi.fn();
    render(
      <AttachmentCapsule onClick={onClick} pending title="awaiting server">
        report.pdf
      </AttachmentCapsule>,
    );
    const capsule = screen.getByTitle("awaiting server");
    expect(capsule.tagName).toBe("SPAN");
    expect(capsule.className).toContain("opacity-60");
    expect(capsule.className).not.toContain("cursor-pointer");
  });

  it("carries the shared frame: border, hover and truncation live here, not in callers", () => {
    render(
      <AttachmentCapsule onClick={() => {}} title="x">
        payload
      </AttachmentCapsule>,
    );
    const className = screen.getByTitle("x").className;
    expect(className).toContain("rounded-md");
    expect(className).toContain("border-border-outer");
    expect(className).toContain("hover:bg-zinc-100");
  });

  it("renders the trailing control inside the frame", () => {
    render(
      <AttachmentCapsule
        title="x"
        trailing={
          <button type="button" aria-label="remove">
            ×
          </button>
        }
      >
        payload
      </AttachmentCapsule>,
    );
    expect(screen.getByRole("button", { name: "remove" })).toBeTruthy();
  });
});

describe("formatAttachmentSize", () => {
  it("uses one decimal above 1 KB and never a locale", () => {
    expect(formatAttachmentSize(900)).toBe("900 B");
    expect(formatAttachmentSize(12800)).toBe("12.5 KB");
    expect(formatAttachmentSize(3.4 * 1024 * 1024)).toBe("3.4 MB");
  });
});