// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { DocumentChip } from "./DocumentChip";

/** The chip is the only place an upload's weight is stated, and the in-flight
 *  case is the one users watch on a slow link: it must read `sent / total`,
 *  not the total alone (a frozen number and a frozen spinner look identical). */
describe("DocumentChip upload progress", () => {
  it("states sent / total while uploading", () => {
    render(
      <DocumentChip
        filename="big.pdf"
        format="pdf"
        size={5 * 1024 * 1024}
        sentBytes={1024 * 1024}
        status="uploading"
      />,
    );
    expect(screen.getByText("1.0 MB / 5.0 MB")).toBeTruthy();
  });

  it("drops the sent half once the upload has landed", () => {
    render(
      <DocumentChip
        filename="big.pdf"
        format="pdf"
        size={5 * 1024 * 1024}
        sentBytes={5 * 1024 * 1024}
        status="success"
      />,
    );
    expect(screen.getByText("5.0 MB")).toBeTruthy();
  });

  it("falls back to the plain size when nothing has been reported yet", () => {
    render(
      <DocumentChip
        filename="big.pdf"
        format="pdf"
        size={5 * 1024 * 1024}
        status="uploading"
      />,
    );
    expect(screen.getByText("5.0 MB")).toBeTruthy();
  });
});