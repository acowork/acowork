/**
 * Inbox attachment preview contract.
 *
 * Two things must not regress:
 *   1. `canPreviewAttachment` — which attachments get the preview
 *      affordance. Saying "yes" to a binary or to a 200 MB file means
 *      loading it into a Monaco model on the main thread, so the gate is
 *      the whole safety story (the download path stays size-independent,
 *      this one cannot be).
 *   2. `openAttachmentPreview` — the tab it produces must stay read-only
 *      and must not look like a workspace file: every save / refresh / LSP
 *      gate in FileEditorPanel keys off `kind !== "file"`, so a wrong
 *      `kind` silently hands the tab save-it-to-Gateway behaviour with no
 *      workspace behind it.
 */
import { describe, it, expect, beforeEach } from "vitest";

import { useFileEditorStore } from "./fileEditorStore";
import { canPreviewAttachment, PREVIEW_MAX_BYTES } from "../lib/user-chat-api";
import { languageForPath, isTextReadablePath } from "../lib/monacoLanguage";
import type { ChatAttachment } from "../lib/types";

function attachment(over: Partial<ChatAttachment> = {}): ChatAttachment {
  return {
    id: "att-1",
    filename: "report.ts",
    mime: "text/plain",
    size: 4096,
    ...over,
  };
}

beforeEach(() => {
  useFileEditorStore.setState({ openFiles: [], activeFileId: null });
});

describe("canPreviewAttachment", () => {
  it("previews text and source files", () => {
    for (const filename of ["a.ts", "a.py", "a.json", "a.md", "a.yaml", "a.sql", "a.txt", "a.log", "a.csv"]) {
      expect(canPreviewAttachment(attachment({ filename }))).toBe(true);
    }
  });

  it("refuses binaries and extensionless names", () => {
    for (const filename of ["a.pdf", "a.png", "a.zip", "a.docx", "a.bin", "LICENSE"]) {
      expect(canPreviewAttachment(attachment({ filename }))).toBe(false);
    }
  });

  it("refuses anything past the size cap, text or not", () => {
    expect(canPreviewAttachment(attachment({ size: PREVIEW_MAX_BYTES }))).toBe(true);
    expect(canPreviewAttachment(attachment({ size: PREVIEW_MAX_BYTES + 1 }))).toBe(false);
  });

  it("is case-insensitive and ignores the path prefix", () => {
    expect(canPreviewAttachment(attachment({ filename: "REPORT.JSON" }))).toBe(true);
    expect(canPreviewAttachment(attachment({ filename: "2024/q1/report.json" }))).toBe(true);
  });

  it("maps unknown text extensions to plaintext rather than refusing them", () => {
    expect(isTextReadablePath("notes.txt")).toBe(true);
    expect(languageForPath("notes.txt")).toBe("plaintext");
    expect(languageForPath("a.bin")).toBe("plaintext");
  });
});

describe("openAttachmentPreview", () => {
  it("opens a read-only attachment tab carrying the fetched body", () => {
    useFileEditorStore.getState().openAttachmentPreview({
      chatId: "chat-1",
      attachmentId: "att-1",
      fileName: "report.ts",
      content: "export const x = 1;",
      language: "typescript",
    });

    const { openFiles, activeFileId } = useFileEditorStore.getState();
    expect(openFiles).toHaveLength(1);
    const tab = openFiles[0];
    expect(tab.kind).toBe("attachment");
    expect(tab.dirty).toBe(false);
    expect(tab.content).toBe("export const x = 1;");
    expect(tab.language).toBe("typescript");
    expect(tab.fileName).toBe("report.ts");
    expect(activeFileId).toBe(tab.id);
    // The tab is addressed by chat + attachment only (D7): the group surface
    // has no owning `userId` to key on, and a chat id is already unique.
    expect(tab.id).toBe("attachment:chat-1:att-1");
    // No workspace behind it — the empty ids are what make the save/refresh
    // /LSP gates in FileEditorPanel skip this tab instead of firing at a
    // non-existent agent workspace.
    expect(tab.agentId).toBe("");
    expect(tab.workspaceId).toBe("");
  });

  it("activates the existing tab instead of stacking duplicates", () => {
    const open = () =>
      useFileEditorStore.getState().openAttachmentPreview({
        chatId: "chat-1",
        attachmentId: "att-1",
        fileName: "report.ts",
        content: "first",
        language: "typescript",
      });

    open();
    const firstId = useFileEditorStore.getState().openFiles[0].id;
    useFileEditorStore.setState({ activeFileId: null });
    open();

    expect(useFileEditorStore.getState().openFiles).toHaveLength(1);
    expect(useFileEditorStore.getState().activeFileId).toBe(firstId);
    // The stale body is kept: re-clicking is "show me that tab again", not
    // "refetch", and the file cannot have changed server-side.
    expect(useFileEditorStore.getState().openFiles[0].content).toBe("first");
  });

  it("keeps same-named attachments from different chats in separate tabs", () => {
    const open = (chatId: string, attachmentId: string) =>
      useFileEditorStore.getState().openAttachmentPreview({
        chatId,
        attachmentId,
        fileName: "notes.txt",
        content: chatId,
        language: "plaintext",
      });

    open("chat-1", "att-1");
    open("chat-2", "att-1");
    expect(useFileEditorStore.getState().openFiles).toHaveLength(2);
  });
});
