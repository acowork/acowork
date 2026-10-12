/**
 * Self-check for the avatar shape (agent + user).
 *
 * The avatars are WeChat-style: a square with rounded corners, not a
 * circle. Each component spells the radius in two separate elements (the
 * built-in icon path and the custom-file path), so both have to carry it —
 * and the pickers spell a third copy on their own thumbnails. Pinning the
 * class strings keeps a `rounded-full` from creeping back into one branch
 * while the other stays square: they render the same avatar for the same
 * subject, so they must never diverge.
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

function expectSquare(cls: string) {
  expect(cls).toContain("rounded-lg");
  expect(cls).not.toContain("rounded-full");
}

describe("AgentAvatar shape", () => {
  it("renders the built-in icon square with rounded corners", () => {
    const { container } = render(<AgentAvatar agentId="a1" builtinAvatarId="icon-01" />);
    expectSquare(classOf(container.querySelector("img")));
  });

  it("renders a custom avatar square with rounded corners", async () => {
    globalThis.fetch = vi.fn(async () => ({
      ok: true,
      status: 200,
      blob: async () => new Blob(["bytes"]),
    })) as unknown as typeof globalThis.fetch;

    const { container } = render(<AgentAvatar agentId="a1" avatarUrl="assets/avatar-01.png" />);

    await waitFor(() => {
      expect(container.querySelector("img")?.getAttribute("src")).toBe("blob:avatar");
    });
    expectSquare(classOf(container.querySelector("img")));
  });
});

describe("UserAvatar shape", () => {
  it("renders the built-in icon square with rounded corners", () => {
    const { container } = render(<UserAvatar builtinAvatarId="icon-01" />);
    expectSquare(classOf(container.querySelector("img")));
  });

  it("renders a custom avatar square with rounded corners", async () => {
    globalThis.fetch = vi.fn(async () => ({
      ok: true,
      status: 200,
      blob: async () => new Blob(["bytes"]),
    })) as unknown as typeof globalThis.fetch;

    const { container } = render(<UserAvatar avatarUrl="assets/avatar-01.png" />);

    await waitFor(() => {
      expect(container.querySelector("img")?.getAttribute("src")).toBe("blob:avatar");
    });
    expectSquare(classOf(container.querySelector("img")));
  });
});

describe("avatar pickers and frames", () => {
  // Each of these renders its own copy of the image rather than going
  // through the shared component — the circle has to be chased here too,
  // or the picker shows circles while the list shows squares. (The pencil
  // badge and the range sliders in these same files stay `rounded-full`
  // on purpose, hence matching the <img> element rather than the file.)
  const sites = [
    { rel: "components/settings/ProfileTab.tsx", cls: "h-12 w-12 rounded-lg object-cover" },
    { rel: "components/right-panel/AgentSetupTab.tsx", cls: "h-12 w-12 rounded-lg object-cover" },
    { rel: "views/pm/pmMembers.tsx", cls: "rounded-lg bg-zinc-200" },
  ];

  it.each(sites)("$rel keeps its thumbnail square", ({ rel, cls }) => {
    expect(read(rel)).toContain(cls);
  });

  it.each([
    "components/settings/ProfileTab.tsx",
    "components/right-panel/AgentSetupTab.tsx",
    "views/pm/pmMembers.tsx",
  ])("%s renders no circular avatar image", (rel) => {
    expect(read(rel)).not.toMatch(/<img[^>]*rounded-full/);
  });

  it.each([
    "components/settings/ProfileTab.tsx",
    "components/right-panel/AgentSetupTab.tsx",
  ])("%s frames the avatar button with the same radius", (rel) => {
    const src = read(rel);
    // The ring button wrapping the 64px avatar — a rounded-full frame over
    // a square avatar clips the corners away.
    expect(src).toContain("relative block rounded-lg ring-1");
    expect(src).not.toContain("relative block rounded-full ring-1");
  });
});
