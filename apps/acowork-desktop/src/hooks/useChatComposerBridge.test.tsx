// @vitest-environment jsdom
/**
 * The composer-side half of the workspace→chat bridge.
 *
 * Two properties are the whole reason this hook exists, and both are silent
 * failures when they break:
 *
 *   1. a request is handled by the LATEST render's closure — the composer's
 *      draft and caret are re-read every render, so a stale closure would
 *      insert into text the user can no longer see;
 *   2. the subscription is installed once — re-subscribing per render opens a
 *      window with no listener, and a request that lands there is dropped.
 */
import { describe, expect, it, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { useChatComposerBridge } from "./useChatComposerBridge";
import {
    attachImageToChatComposer,
    insertIntoChatComposer,
} from "../lib/chatComposerBridge";

describe("useChatComposerBridge", () => {
    it("delivers inserted text to the mounted composer", () => {
        const handler = vi.fn();
        renderHook(() => useChatComposerBridge(handler));
        act(() => insertIntoChatComposer("logo.png @ (1, 2)"));
        expect(handler).toHaveBeenCalledWith({ kind: "text", text: "logo.png @ (1, 2)" });
    });

    it("uses the newest closure after a re-render", () => {
        const first = vi.fn();
        const second = vi.fn();
        let draft = "old";
        const { rerender } = renderHook(() =>
            useChatComposerBridge(() => (draft === "old" ? first : second)()),
        );
        draft = "new";
        rerender();
        act(() => insertIntoChatComposer("second"));
        expect(first).not.toHaveBeenCalled();
        expect(second).toHaveBeenCalledTimes(1);
    });

    it("stops listening on unmount", () => {
        const handler = vi.fn();
        const { unmount } = renderHook(() => useChatComposerBridge(handler));
        unmount();
        act(() => insertIntoChatComposer("after unmount"));
        expect(handler).not.toHaveBeenCalled();
    });

    it("passes an image request through as the caller's File", () => {
        const handler = vi.fn();
        renderHook(() => useChatComposerBridge(handler));
        const file = new File(["png"], "crop.png", { type: "image/png" });
        act(() => attachImageToChatComposer(file));
        expect(handler).toHaveBeenCalledWith({ kind: "image", file });
    });
});