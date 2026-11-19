import { beforeEach, describe, expect, it } from "vitest";
import { useLayoutStore } from "./layoutStore";

describe("layoutStore hosted app navigation", () => {
    beforeEach(() => {
        useLayoutStore.setState({
            hostedAppId: null,
            openHostedAppRequestSeq: 0,
            openHostedAppRequestId: null,
            navViewRequestSeq: 0,
            navViewRequest: null,
            hostedAppsEpoch: 0,
        });
    });

    it("requestOpenHostedApp switches to app view and records id", () => {
        useLayoutStore.getState().requestOpenHostedApp("figcraft");
        const s = useLayoutStore.getState();
        expect(s.hostedAppId).toBe("figcraft");
        expect(s.openHostedAppRequestId).toBe("figcraft");
        expect(s.openHostedAppRequestSeq).toBe(1);
        expect(s.navViewRequest).toBe("app");
        expect(s.navViewRequestSeq).toBe(1);
    });

    it("requestNavView to a built-in clears hostedAppId", () => {
        useLayoutStore.getState().requestOpenHostedApp("figcraft");
        useLayoutStore.getState().requestNavView("chat");
        expect(useLayoutStore.getState().hostedAppId).toBeNull();
        expect(useLayoutStore.getState().navViewRequest).toBe("chat");
    });

    it("bumpHostedAppsEpoch increments", () => {
        useLayoutStore.getState().bumpHostedAppsEpoch();
        expect(useLayoutStore.getState().hostedAppsEpoch).toBe(1);
    });
});
