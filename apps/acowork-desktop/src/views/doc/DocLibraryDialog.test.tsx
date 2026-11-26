/**
 * Self-check for `DocLibraryDialog` — the document library's one-shot draft.
 *
 * Why this exists: a library is the container a user creates *instead of*
 * a root-level folder, so the whole point of the dialog is that name, icon,
 * description, members and owner reach the store together. The contract
 * locked down here is the wiring, not the visuals:
 *
 * - create sends ONE draft, and never transfers ownership to the creator
 *   (the backend already treats `creator` as the owner — `owner: null`);
 * - the creator is not offered as a member candidate, same rule as
 *   `ProjectCreateDialog` (`create` seeds the caller's ACL already);
 * - edit sends only the fields that actually changed, so re-saving an
 *   untouched library is a no-op instead of a blind overwrite;
 * - an empty name is refused before the store is called.
 *
 * Stub strategy mirrors `ProjectCreateDialog.test.tsx`: `vi.mock` the leaf
 * stores and `pmMembers` (no network, no i18n), assert on the rendered DOM.
 */
import { describe, it, expect, vi, beforeEach } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";

const SELF = "21a727ee-d2ac-4d7a-9bb1-a846cb34cf3c";
const PEER = "9b3d44cc-1f2e-4a5b-8c9d-0e7f6a5b4c3d";

const mocks = vi.hoisted(() => ({
  createLibrary: vi.fn(),
  updateLibrary: vi.fn(),
  showToast: vi.fn(),
}));

vi.mock("../../stores/doc/treeStore", () => ({
  useDocTreeStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({
      createLibrary: mocks.createLibrary,
      updateLibrary: mocks.updateLibrary,
    }),
}));

vi.mock("../../components/common/ToastProvider", () => ({
  showToast: mocks.showToast,
}));

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

// `pmMembers` pulls agent/auth/directory stores; the dialog only needs the
// two lists and an avatar. Rendering `MemberAvatar` as a fragment keeps the
// member rows' text content equal to their display names.
vi.mock("../pm/pmMembers", () => ({
  useSelfAccount: () => ({ user_id: SELF, display_name: "admin" }),
  useInviteCandidates: () => ({
    agents: [],
    humans: [
      { instance_id: SELF, kind: "user", displayName: "admin", unresolved: false },
      { instance_id: PEER, kind: "user", displayName: "Bob", unresolved: false },
    ],
  }),
  MemberAvatar: () => null,
}));

import { DocLibraryDialog } from "./DocLibraryDialog";

beforeEach(() => {
  vi.clearAllMocks();
});

const LIBRARY = {
  dir_id: "dir-lib",
  parent_dir_id: "root",
  name: "研发文档",
  icon: "book-open",
  description: "internal specs",
  created_at: "2026-01-01T00:00:00Z",
  updated_at: "2026-01-01T00:00:00Z",
  creator: SELF,
};

describe("DocLibraryDialog — create", () => {
  it("sends name, icon, description, members and owner in one draft", async () => {
    mocks.createLibrary.mockResolvedValue({ dir_id: "dir-new" });
    const onClose = vi.fn();
    render(<DocLibraryDialog mode="create" onClose={onClose} />);

    fireEvent.change(screen.getByLabelText("doc.library.name"), {
      target: { value: "  研发文档  " },
    });
    fireEvent.change(screen.getByLabelText("doc.library.description"), {
      target: { value: "internal specs" },
    });
    // 图标走共享的 LibraryIconPicker：预览即触发器，点开在网格里选一枚
    fireEvent.click(screen.getByRole("button", { name: "doc.library.iconPickerTitle" }));
    fireEvent.click(screen.getByRole("button", { name: "notebook" }));
    fireEvent.click(screen.getByRole("button", { name: /Bob/ }));
    fireEvent.change(screen.getByLabelText("doc.library.owner"), {
      target: { value: PEER },
    });

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "common.create" }));
    });

    expect(mocks.createLibrary).toHaveBeenCalledWith({
      name: "研发文档",
      icon: "notebook",
      description: "internal specs",
      members: [PEER],
      owner: PEER,
    });
    expect(onClose).toHaveBeenCalled();
    expect(mocks.showToast).toHaveBeenCalledWith(
      expect.objectContaining({ type: "success" }),
    );
  });

  it("does not transfer ownership when the owner stays the creator", async () => {
    mocks.createLibrary.mockResolvedValue({ dir_id: "dir-new" });
    render(<DocLibraryDialog mode="create" onClose={vi.fn()} />);
    fireEvent.change(screen.getByLabelText("doc.library.name"), {
      target: { value: "研发文档" },
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "common.create" }));
    });
    expect(mocks.createLibrary).toHaveBeenCalledWith(
      expect.objectContaining({ owner: null, members: [] }),
    );
  });

  it("does not offer the avatar set as library icons", () => {
    // `BUILTIN_ICONS` (`icon-NN`) is the agent/user portrait set — faces and
    // illustrations. Offering them here made a library look like a bot
    // profile; the grid is document glyphs only, plus the default.
    render(<DocLibraryDialog mode="create" onClose={vi.fn()} />);
    fireEvent.click(screen.getByRole("button", { name: "doc.library.iconPickerTitle" }));
    expect(screen.queryAllByRole("button", { name: /^icon-\d+$/ })).toHaveLength(0);
    expect(screen.getByRole("button", { name: "doc.library.defaultIcon" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "book-open" })).toBeTruthy();
  });

  it("does not offer the creator as a member candidate", () => {
    render(<DocLibraryDialog mode="create" onClose={vi.fn()} />);
    const rows = screen.getAllByRole("button", { pressed: false });
    expect(rows.map((r) => r.textContent?.trim())).toEqual(["Bob"]);
  });

  it("refuses an empty name without calling the store", async () => {
    render(<DocLibraryDialog mode="create" onClose={vi.fn()} />);
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "common.create" }));
    });
    expect(mocks.showToast).toHaveBeenCalledWith(
      expect.objectContaining({ message: "doc.library.nameRequired" }),
    );
    expect(mocks.createLibrary).not.toHaveBeenCalled();
  });
});

describe("DocLibraryDialog — edit", () => {
  it("sends only the fields that changed", async () => {
    mocks.updateLibrary.mockResolvedValue(true);
    const onClose = vi.fn();
    render(
      <DocLibraryDialog mode="edit" library={LIBRARY} onClose={onClose} />,
    );

    fireEvent.change(screen.getByLabelText("doc.library.name"), {
      target: { value: "研发文档 v2" },
    });

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "doc.save" }));
    });

    // Absent keys are "leave alone" (the PATCH body's three-intent encoding);
    // a blind full overwrite would resurrect a field someone else just cleared.
    expect(mocks.updateLibrary).toHaveBeenCalledWith("dir-lib", { name: "研发文档 v2" });
    expect(onClose).toHaveBeenCalled();
  });

  it("clears an icon explicitly with null", async () => {
    mocks.updateLibrary.mockResolvedValue(true);
    render(<DocLibraryDialog mode="edit" library={LIBRARY} onClose={vi.fn()} />);

    fireEvent.click(screen.getByRole("button", { name: "doc.library.iconPickerTitle" }));
    fireEvent.click(screen.getByRole("button", { name: "doc.library.defaultIcon" }));

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "doc.save" }));
    });

    expect(mocks.updateLibrary).toHaveBeenCalledWith("dir-lib", { icon: null });
  });

  it("keeps the dialog open when the store rejects", async () => {
    mocks.updateLibrary.mockResolvedValue(false);
    const onClose = vi.fn();
    render(<DocLibraryDialog mode="edit" library={LIBRARY} onClose={onClose} />);
    fireEvent.change(screen.getByLabelText("doc.library.name"), {
      target: { value: "x" },
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "doc.save" }));
    });
    expect(onClose).not.toHaveBeenCalled();
    expect(mocks.showToast).toHaveBeenCalledWith(
      expect.objectContaining({ message: "doc.library.saveFailed" }),
    );
  });
});