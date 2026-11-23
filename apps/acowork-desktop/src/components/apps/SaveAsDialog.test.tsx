// @vitest-environment jsdom
/**
 * `SaveAsDialog` — the host-side target picker a hosted app's Save As asks for
 * (ADR-118 §6.4). Pinned: the name is prefilled from the app, the path preview
 * shows where the bytes will land, Save settles the pending promise with that
 * path, Cancel settles it with null, and a cleared name cannot be saved.
 */
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { SaveAsDialog } from "./SaveAsDialog";
import { useSaveAsStore, type SaveAsTarget } from "../../stores/saveAsStore";

// `fetchWorkspaces` must keep one identity across calls, as a real zustand
// action does — a fresh function per selector call re-fires the dialog's
// "reset to the requested workspace" effect on every render and wipes the
// folder the user just picked.
const fetchWorkspaces = async () => undefined;

vi.mock("../../stores/workspaceStore", () => ({
    useWorkspaceStore: (sel: (s: unknown) => unknown) =>
        sel({
            workspaces: [{ id: "ws-1", path: "E:/docs", alias: "docs" }],
            workspacesAgentId: "agent-1",
            fetchWorkspaces,
        }),
}));

vi.mock("../../stores/fileTree/fileTreeStore", () => ({
    fetchTreeNode: async () => ({
        kind: "ready",
        entries: [
            { name: "reports", type: "directory" },
            { name: "a.docx", type: "file" },
        ],
        root: "",
        fetchedAt: 0,
    }),
}));

vi.mock("../../i18n/useTranslation", () => ({
    useTranslation: () => ({ t: (k: string) => k }),
}));

afterEach(() => {
    cleanup();
    useSaveAsStore.setState({ request: null });
});

/**
 * Mount the dialog and open it with a request. The save promise is returned
 * boxed: an async function would adopt (and so await) a bare promise, turning
 * `await open()` into "wait until the user clicks".
 */
async function open(name = "Q3 报表.docx"): Promise<{ settled: Promise<SaveAsTarget | null> }> {
    render(<SaveAsDialog />);
    let settled: Promise<SaveAsTarget | null> = Promise.resolve(null);
    await act(async () => {
        settled = useSaveAsStore.getState().open({ agentId: "agent-1", defaultWorkspaceId: "ws-1", name });
    });
    return { settled };
}

describe("SaveAsDialog", () => {
    it("prefills the app's name and saves into the chosen folder", async () => {
        const { settled } = await open();
        const input = (await screen.findByRole("textbox")) as HTMLInputElement;
        expect(input.value).toBe("Q3 报表.docx");

        await act(async () => {
            fireEvent.click(await screen.findByText("reports"));
        });
        expect(screen.getByText("reports/Q3 报表.docx")).toBeTruthy();

        await act(async () => {
            fireEvent.click(screen.getByText("saveAs.save"));
        });

        await expect(settled).resolves.toEqual({
            agentId: "agent-1",
            workspaceId: "ws-1",
            relPath: "reports/Q3 报表.docx",
        });
    });

    it("settles as cancelled — no target, nothing written", async () => {
        const { settled } = await open();
        await act(async () => {
            fireEvent.click(screen.getByText("common.cancel"));
        });
        await expect(settled).resolves.toBeNull();
    });

    it("cannot save an empty name", async () => {
        await open();
        const input = (await screen.findByRole("textbox")) as HTMLInputElement;
        await act(async () => {
            fireEvent.change(input, { target: { value: "   " } });
        });
        const save = screen.getByText("saveAs.save") as HTMLButtonElement;
        expect(save.disabled).toBe(true);
    });
});