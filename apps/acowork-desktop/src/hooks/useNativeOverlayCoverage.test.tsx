/**
 * `useNativeOverlayCoverage` — the guard that keeps an open dialog visible.
 *
 * Why this exists at all: a preview is a *native* child webview, composited
 * above the HTML layer. No `z-index` can put a React portal on top of it, so
 * when a modal opens over the file panel the webview has to be hidden or the
 * dialog is invisible and its clicks land in the page.
 *
 * Portals are created in a dozen components, so detection is a hit test against
 * `document.elementFromPoint` rather than instrumentation of each overlay. The
 * tests pin the three outcomes that matter: our own anchor is not "covered", a
 * foreign element over the rect is, and a disabled host never probes.
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render } from "@testing-library/react";
import { useRef } from "react";
import { useNativeOverlayCoverage } from "./useNativeOverlayCoverage";

function Probe({ enabled }: { enabled: boolean }) {
    const ref = useRef<HTMLDivElement | null>(null);
    const covered = useNativeOverlayCoverage(ref, enabled);
    return (
        <div ref={ref} data-testid="anchor" data-covered={String(covered)}>
            anchor
        </div>
    );
}

describe("useNativeOverlayCoverage", () => {
    let rectSpy: ReturnType<typeof vi.spyOn>;
    // jsdom implements no hit testing at all, so the probe is installed per test
    // rather than spied on.
    let hitSpy: ReturnType<typeof vi.fn>;

    beforeEach(() => {
        rectSpy = vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockReturnValue({
            x: 0,
            y: 0,
            left: 0,
            top: 0,
            width: 200,
            height: 100,
            right: 200,
            bottom: 100,
            toJSON: () => ({}),
        } as DOMRect);
        hitSpy = vi.fn(() => null);
        (document as unknown as { elementFromPoint: unknown }).elementFromPoint = hitSpy;
    });

    afterEach(() => {
        rectSpy.mockRestore();
        delete (document as unknown as { elementFromPoint?: unknown }).elementFromPoint;
    });

    it("reports uncovered when the topmost element is the preview anchor itself", () => {
        const view = render(<Probe enabled />);
        // `elementFromPoint` returning null (nothing above) must not mean covered.
        expect(view.getByTestId("anchor").dataset.covered).toBe("false");
    });

    it("reports covered when a foreign element sits over the rect", () => {
        const dialog = document.createElement("div");
        document.body.appendChild(dialog);
        hitSpy.mockImplementation(() => dialog);

        const view = render(<Probe enabled />);

        expect(view.getByTestId("anchor").dataset.covered).toBe("true");
        dialog.remove();
    });

    it("does not probe at all while disabled", () => {
        const view = render(<Probe enabled={false} />);

        expect(hitSpy).not.toHaveBeenCalled();
        expect(view.getByTestId("anchor").dataset.covered).toBe("false");
    });
});