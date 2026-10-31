// SPDX-License-Identifier: Apache-2.0
// Copyright the ACowork project contributors.
//
// Subscribing a composer to `lib/chatComposerBridge` requests.
//
// Two composers (the Chat surface's `AgentChatView`, the execution surface's
// `ChatPanel`) need the same three lines: subscribe once, and route the request
// to the LATEST render's closure. Those closures are re-created on every render
// (`insertTextAtCaret` depends on the composer's draft, `uploadFiles` on its
// session), so a subscription that captured the first render's version would
// insert into a stale draft. Re-subscribing on every render instead would open
// a window with no listener — a request arriving then is silently dropped.
//
// Hence the ref, and hence this hook: the rule lives in one tested place
// instead of being re-derived (and re-broken) at two call sites.

import { useEffect, useRef } from "react";
import {
    subscribeChatComposerRequests,
    type ChatComposerRequest,
} from "../lib/chatComposerBridge";

export function useChatComposerBridge(
    handler: (request: ChatComposerRequest) => void,
): void {
    const latest = useRef(handler);
    // No dependency array: the ref tracks the newest closure after every
    // render, and writing it inside an effect keeps render pure.
    useEffect(() => {
        latest.current = handler;
    });
    useEffect(
        () => subscribeChatComposerRequests((request) => latest.current(request)),
        [],
    );
}