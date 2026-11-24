/**
 * RemoteArtwork — the broken-glyph guard.
 *
 * What this pins (the wake-from-sleep bug): a failed load must render the
 * fallback — never the browser's broken-image glyph — and the next focus
 * (i.e. coming back from suspend) must put the <img> back so the URL is
 * requested again.
 */
import { describe, it, expect } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { RemoteArtwork } from "./RemoteArtwork";

describe("RemoteArtwork", () => {
  it("falls back on a failed load and remounts the image on wake", () => {
    render(<RemoteArtwork src="https://gallery.example/icon.png" fallback={<span>F</span>} />);
    const img = document.querySelector("img");
    expect(img, "a healthy src renders the image").toBeTruthy();

    fireEvent.error(img!);
    expect(document.querySelector("img"), "a failed load must not leave an <img>").toBeNull();
    expect(screen.getByText("F")).toBeTruthy();

    fireEvent(window, new Event("focus"));
    expect(document.querySelector("img"), "wake must retry the URL").toBeTruthy();
  });
});