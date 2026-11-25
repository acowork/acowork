/**
 * Self-check for the avatar shape (agent + user).
 *
 * A single agent / user avatar is a **circle**. The group composite
 * (`GroupAvatarStack`) is deliberately **not**: a group has no face, so it
 * stays a rounded square the way a group icon does in every messenger — even
 * though it shares the 40px footprint with the circles beside it in the
 * sidebar. That split is what these assertions pin, because both shapes are
 * spelled as class strings in several places (each avatar component spells the
 * radius once per render branch, and the pickers spell their own copy on the
 * thumbnail), and a lone `rounded-lg` surviving in one branch means the same
 * subject renders as a square in one surface and a circle in another.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { cleanup, render, waitFor } from "@testing-library/react";
import { AgentAvatar } from "./AgentAvatar";
import { UserAvatar } from "./UserAvatar";

const __dirname = dirname(fileURLToPath(import.meta.url));
const read = (rel: string) => readFileSync(resolve(__dirname, "..", "..", rel), "utf8");

beforeEach(() => {
  URL.createObjectURL = vi.fn(() => "blob:avatar");
  URL.revokeObjectURL = vi.fn();
});

afterEach(cleanup);

const classOf = (el: Element | null) => el?.getAttribute("class") ?? "";

function expectCircle(cls: string) {
  expect(cls).toContain("rounded-full");
  expect(cls).not.toContain("rounded-lg");
}

describe("AgentAvatar shape", () => {
  it("renders the built-in icon as a circle", () => {
    const { container } = render(<AgentAvatar agentId="a1" builtinAvatarId="icon-01" />);
    expectCircle(classOf(container.querySelector("img")));
  });

  it("renders a custom avatar as a circle", async () => {
    globalThis.fetch = vi.fn(async () => ({
      ok: true,
      status: 200,
      blob: async () => new Blob(["bytes"]),
    })) as unknown as typeof globalThis.fetch;

    const { container } = render(<AgentAvatar agentId="a1" avatarUrl="assets/avatar-01.png" />);

    await waitFor(() => {
      expect(container.querySelector("img")?.getAttribute("src")).toBe("blob:avatar");
    });
    expectCircle(classOf(container.querySelector("img")));
  });
});

describe("UserAvatar shape", () => {
  it("renders the built-in icon as a circle", () => {
    const { container } = render(<UserAvatar builtinAvatarId="icon-01" />);
    expectCircle(classOf(container.querySelector("img")));
  });

  it("renders a custom avatar as a circle", async () => {
    globalThis.fetch = vi.fn(async () => ({
      ok: true,
      status: 200,
      blob: async () => new Blob(["bytes"]),
    })) as unknown as typeof globalThis.fetch;

    const { container } = render(<UserAvatar avatarUrl="assets/avatar-01.png" />);

    await waitFor(() => {
      expect(container.querySelector("img")?.getAttribute("src")).toBe("blob:avatar");
    });
    expectCircle(classOf(container.querySelector("img")));
  });
});

describe("avatar pickers and frames", () => {
  // Each of these renders its own copy of the avatar rather than going
  // through the shared component — the circle has to be chased here too, or
  // the picker shows squares while the list shows circles.
  const sites = [
    { rel: "components/settings/ProfileTab.tsx", cls: "h-12 w-12 rounded-full object-cover" },
    { rel: "components/right-panel/AgentSetupTab.tsx", cls: "h-12 w-12 rounded-full object-cover" },
    { rel: "views/pm/pmMembers.tsx", cls: "rounded-full bg-surface-hover" },
    { rel: "components/agent-list/AgentLibraryDialog.tsx", cls: "h-9 w-9 shrink-0 rounded-full object-cover" },
  ];

  it.each(sites)("$rel keeps its avatar circular", ({ rel, cls }) => {
    expect(read(rel)).toContain(cls);
  });

  it.each([
    "components/settings/ProfileTab.tsx",
    "components/right-panel/AgentSetupTab.tsx",
  ])("%s frames the avatar button with the same radius", (rel) => {
    const src = read(rel);
    // The ring button wrapping the 64px avatar, and the custom-avatar tiles.
    // A square frame over a round avatar clips the circle back to a square.
    expect(src).toContain("relative block rounded-full ring-1");
    expect(src).not.toContain("relative block rounded-lg ring-1");
    expect(src).not.toContain("aspect-square overflow-hidden rounded-md");
  });
});

describe("group composite stays square", () => {
  it("keeps the 40px container square and its member tiles square", () => {
    const src = read("components/chat/GroupAvatarStack.tsx");
    expect(src).toContain('overflow-hidden rounded-lg');
    expect(src).not.toContain("rounded-full");
  });
});