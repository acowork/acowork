// SPDX-License-Identifier: Apache-2.0
// Copyright the ACowork project contributors.

import { describe, expect, it, vi } from "vitest";
import {
    attachImageToChatComposer,
    insertIntoChatComposer,
    subscribeChatComposerRequests,
    type ChatComposerRequest,
} from "./chatComposerBridge";

describe("chatComposerBridge", () => {
    it("delivers inserted text to the subscriber", () => {
        const handler = vi.fn();
        const off = subscribeChatComposerRequests(handler);
        insertIntoChatComposer("logo.png @ (4, 8)");
        off();
        expect(handler).toHaveBeenCalledWith({ kind: "text", text: "logo.png @ (4, 8)" });
    });

    it("delivers an image as the caller's own File", () => {
        const handler = vi.fn();
        const off = subscribeChatComposerRequests(handler);
        const file = new File(["bytes"], "logo-region.png", { type: "image/png" });
        attachImageToChatComposer(file);
        off();
        const request = handler.mock.calls[0][0] as ChatComposerRequest;
        expect(request.kind).toBe("image");
        expect(request.kind === "image" && request.file).toBe(file);
    });

    it("stops delivering after unsubscribe", () => {
        const handler = vi.fn();
        subscribeChatComposerRequests(handler)();
        insertIntoChatComposer("dropped");
        expect(handler).not.toHaveBeenCalled();
    });

    it("ignores empty payloads", () => {
        const handler = vi.fn();
        const off = subscribeChatComposerRequests(handler);
        insertIntoChatComposer("");
        window.dispatchEvent(new CustomEvent("acowork:chat-composer-request", { detail: { kind: "text", text: "" } }));
        window.dispatchEvent(new CustomEvent("acowork:chat-composer-request", { detail: undefined }));
        off();
        expect(handler).not.toHaveBeenCalled();
    });

    it("fans out to every mounted subscriber", () => {
        // Both composers exist during a surface switch; neither may swallow
        // the other's request.
        const first = vi.fn();
        const second = vi.fn();
        const offFirst = subscribeChatComposerRequests(first);
        const offSecond = subscribeChatComposerRequests(second);
        insertIntoChatComposer("both");
        offFirst();
        offSecond();
        expect(first).toHaveBeenCalledTimes(1);
        expect(second).toHaveBeenCalledTimes(1);
    });
});