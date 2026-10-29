//! ADR-104: the Create Agent wizard's observable behaviour, step by step.
//!
//! The mapping rules (`"" ≠ 0`, absent ≠ empty, capability rows) are pinned by
//! `lib/createWizardDraft.test.ts` — repeating them here would test the same
//! function twice. This suite covers what only a rendered dialog can show:
//!
//!   1. all five steps exist and the gate is what stops the user
//!   2. the four new input kinds (labels / `[config]` / `[[mcp_servers]]` /
//!      `[capabilities.*]`) actually reach the draft
//!   3. **preview and create send the same draft** — the single-construction
//!      point that made the old hand-built TOML preview drift
//!   4. warnings do not block, errors do
//!
//! `invoke` is mocked at the Tauri boundary, so every assertion is on the wire
//! payload the Rust command would receive.

import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor, within } from "@testing-library/react";
import i18n from "../../i18n";

const invoke = vi.hoisted(() => vi.fn());
vi.mock("@tauri-apps/api/core", () => ({ invoke }));

const fetchMcpPresets = vi.hoisted(() => vi.fn());
vi.mock("../../lib/mcpPresets", async () => {
  const actual = await vi.importActual<typeof import("../../lib/mcpPresets")>("../../lib/mcpPresets");
  return { ...actual, fetchMcpPresets };
});

import { CreateWizard } from "./CreateWizard";
import type { McpPreset } from "../../lib/mcpPresets";

const PRESET: McpPreset = {
  id: "brave-search",
  name: "Brave Search",
  description: "Privacy-first web search",
  category: "search" as McpPreset["category"],
  transport: "stdio" as McpPreset["transport"],
  command: "npx",
  args: ["-y", "@brave/mcp"],
  required_env: ["BRAVE_API_KEY"],
  install_hint: "Set BRAVE_API_KEY",
  icon: "Search",
  install: {
    method: "npm",
    package: "@brave/mcp",
    requires_npm: true,
    requires_uvx: false,
    requires_docker: false,
    post_install_note: null,
  } as unknown as McpPreset["install"],
};

const BUILTIN_TOOLS = [
  { name: "bash", description: "Run shell commands" },
  { name: "file_read", description: "Read a file" },
];

/** Route `invoke` by command name; anything unmocked fails loudly. */
function wireInvoke(overrides: Record<string, (args: unknown) => unknown> = {}) {
  invoke.mockImplementation(async (cmd: string, args: unknown) => {
    if (overrides[cmd]) return overrides[cmd](args);
    switch (cmd) {
      case "list_builtin_tools":
        return BUILTIN_TOOLS;
      case "parse_skill_zip_preview":
        return { name: "requirements-analysis", description: "Collect requirements", tool_deps: ["file_read"] };
      case "preview_manifest":
        return { toml: 'agent_id = "com.example.agent"', errors: [], warnings: [] };
      case "create_agent":
        return "com.example.agent";
      default:
        throw new Error(`unexpected invoke: ${cmd}`);
    }
  });
}

function lastDraftFor(cmd: string): Record<string, unknown> {
  const call = invoke.mock.calls.filter((c) => c[0] === cmd).at(-1);
  if (!call) throw new Error(`${cmd} was never invoked`);
  return (call[1] as { draft: Record<string, unknown> }).draft;
}

let onCreated: ReturnType<typeof vi.fn>;
let onClose: ReturnType<typeof vi.fn>;

function renderWizard() {
  return render(<CreateWizard open onCreated={onCreated} onClose={onClose} />);
}

const next = () => screen.getByRole("button", { name: /Next|下一步|次へ/ });
const back = () => screen.getByRole("button", { name: /Back|上一步/ });

async function fillBasics(agentId = "com.example.agent", name = "Agent") {
  fireEvent.change(screen.getByLabelText("Agent ID"), { target: { value: agentId } });
  fireEvent.change(screen.getByLabelText("Display Name"), { target: { value: name } });
}

beforeEach(async () => {
  await i18n.changeLanguage("en");
  invoke.mockReset();
  fetchMcpPresets.mockReset();
  fetchMcpPresets.mockResolvedValue([PRESET]);
  onCreated = vi.fn();
  onClose = vi.fn();
  wireInvoke();
});

describe("CreateWizard — step rail", () => {
  it("offers all five steps, including the three that were missing", () => {
    renderWizard();
    for (const label of ["Basic", "Model & Params", "Tools & MCP", "Skills & Capabilities", "Preview"]) {
      expect(screen.getByText(label), label).toBeTruthy();
    }
  });
});

describe("CreateWizard — step 1: identity and labels", () => {
  it("cannot advance without a reverse-domain id and a name", async () => {
    renderWizard();
    expect((next() as HTMLButtonElement).disabled).toBe(true);

    await fillBasics("not-a-domain", "Agent");
    expect((next() as HTMLButtonElement).disabled).toBe(true);

    await fillBasics("com.example.agent", "Agent");
    expect((next() as HTMLButtonElement).disabled).toBe(false);
  });

  it("refuses an invalid label and keeps it out of the draft", async () => {
    renderWizard();
    await fillBasics();
    const input = screen.getByLabelText("Labels");
    fireEvent.change(input, { target: { value: "Not Valid" } });
    fireEvent.keyDown(input, { key: "Enter" });

    expect(screen.getByText(/not a valid label/)).toBeTruthy();
    // No chip was created.
    expect(screen.queryByRole("button", { name: "Remove label Not Valid" })).toBeNull();

    fireEvent.change(input, { target: { value: "project-management" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(screen.getByRole("button", { name: "Remove label project-management" })).toBeTruthy();
  });

  it("carries labels into the draft", async () => {
    renderWizard();
    await fillBasics();
    const input = screen.getByLabelText("Labels");
    for (const label of ["project-management", "agile"]) {
      fireEvent.change(input, { target: { value: label } });
      fireEvent.keyDown(input, { key: "Enter" });
    }
    fireEvent.click(next());
    // Step 2 is config — walk straight to preview.
    while (!screen.queryByText("Review your agent")) fireEvent.click(next());

    await waitFor(() => expect(lastDraftFor("preview_manifest").labels).toEqual([
      "project-management",
      "agile",
    ]));
  });
});

describe("CreateWizard — step 2: [config] defaults", () => {
  it("sends no config table when every field is left empty", async () => {
    renderWizard();
    await fillBasics();
    fireEvent.click(next());
    expect(screen.getByText(/Defaults this package suggests/)).toBeTruthy();
    fireEvent.click(next());
    while (!screen.queryByText("Review your agent")) fireEvent.click(next());

    await waitFor(() => expect("config" in lastDraftFor("preview_manifest")).toBe(false));
  });

  it("sends only the fields the author filled", async () => {
    renderWizard();
    await fillBasics();
    fireEvent.click(next());
    fireEvent.change(screen.getByLabelText("Temperature"), { target: { value: "0.5" } });
    fireEvent.change(screen.getByLabelText("Context window"), { target: { value: "128000" } });
    fireEvent.click(next());
    while (!screen.queryByText("Review your agent")) fireEvent.click(next());

    await waitFor(() =>
      expect(lastDraftFor("preview_manifest").config).toEqual({
        temperature: 0.5,
        context_window: 128000,
      }),
    );
  });

  it("blocks the step on an out-of-range value", async () => {
    renderWizard();
    await fillBasics();
    fireEvent.click(next());
    fireEvent.change(screen.getByLabelText("Temperature"), { target: { value: "5" } });
    expect((next() as HTMLButtonElement).disabled).toBe(true);
    // Step-level issue text is visible next to the field, not in a dialog.
    expect(screen.getAllByText(/outside its allowed range/).length).toBeGreaterThan(0);
  });
});

describe("CreateWizard — step 3: tools and MCP", () => {
  it("declares a builtin tool and both MCP forms", async () => {
    renderWizard();
    await fillBasics();
    fireEvent.click(next());
    fireEvent.click(next());

    fireEvent.click(screen.getByLabelText("bash"));
    fireEvent.click(screen.getByLabelText("pm"));
    await waitFor(() => expect(screen.getByLabelText("Brave Search")).toBeTruthy());
    fireEvent.click(screen.getByLabelText("Brave Search"));

    fireEvent.click(next());
    while (!screen.queryByText("Review your agent")) fireEvent.click(next());

    await waitFor(() => {
      const draft = lastDraftFor("preview_manifest");
      expect(draft.tools).toEqual(["bash"]);
      expect(draft.mcp_servers).toEqual([
        { hosted: "pm", required_secrets: [], tools: [], required: false },
        // `required_secrets` is the preset's fact, copied from the registry.
        { preset: "brave-search", required_secrets: ["BRAVE_API_KEY"], tools: [], required: false },
      ]);
    });
  });

  it("records the author's why and the required flag on the pick", async () => {
    renderWizard();
    await fillBasics();
    fireEvent.click(next());
    fireEvent.click(next());
    await waitFor(() => expect(screen.getByLabelText("Brave Search")).toBeTruthy());
    fireEvent.click(screen.getByLabelText("Brave Search"));

    const why = screen.getByLabelText("Why this agent needs it brave-search");
    fireEvent.change(why, { target: { value: "fresh docs" } });
    fireEvent.click(screen.getByRole("switch", { name: "Required" }));

    fireEvent.click(next());
    while (!screen.queryByText("Review your agent")) fireEvent.click(next());

    await waitFor(() =>
      expect(lastDraftFor("preview_manifest").mcp_servers).toEqual([
        {
          preset: "brave-search",
          required_secrets: ["BRAVE_API_KEY"],
          tools: [],
          required: true,
          why: "fresh docs",
        },
      ]),
    );
  });

  it("keeps authoring usable when the preset registry is unreachable", async () => {
    fetchMcpPresets.mockRejectedValue(new Error("gateway down"));
    renderWizard();
    await fillBasics();
    fireEvent.click(next());
    fireEvent.click(next());
    // The failure is reported in place of the preset list, without a dialog.
    await waitFor(() => expect(screen.getByText("gateway down")).toBeTruthy());
    // Platform-hosted servers need no catalog, so the step still works.
    expect(screen.getByLabelText("pm")).toBeTruthy();
  });
});

describe("CreateWizard — step 4: skills and capabilities", () => {
  it("does not turn an uploaded skill into a capability (the old 1:1 bug)", async () => {
    renderWizard();
    await fillBasics();
    fireEvent.click(next());
    fireEvent.click(next());
    fireEvent.click(next());

    const file = new File([new Uint8Array([1, 2, 3])], "req.zip", { type: "application/zip" });
    fireEvent.change(document.querySelector('input[type="file"]') as HTMLInputElement, {
      target: { files: [file] },
    });
    await waitFor(() => expect(screen.getByText("requirements-analysis")).toBeTruthy());

    fireEvent.click(next());
    while (!screen.queryByText("Review your agent")) fireEvent.click(next());

    await waitFor(() => {
      const draft = lastDraftFor("preview_manifest");
      expect(draft.capabilities).toEqual([]);
      // The ZIP bytes travel with the draft, positionally matched to the skill.
      expect(draft.skill_files).toEqual([[1, 2, 3]]);
    });
  });

  it("links a capability to the skills it is implemented by", async () => {
    renderWizard();
    await fillBasics();
    fireEvent.click(next());
    fireEvent.click(next());
    fireEvent.click(next());

    const file = new File([new Uint8Array([1])], "req.zip", { type: "application/zip" });
    fireEvent.change(document.querySelector('input[type="file"]') as HTMLInputElement, {
      target: { files: [file] },
    });
    await waitFor(() => expect(screen.getByText("requirements-analysis")).toBeTruthy());

    fireEvent.click(screen.getByRole("button", { name: "Add action" }));
    fireEvent.change(screen.getByLabelText("Action name"), {
      target: { value: "requirements_analysis" },
    });
    fireEvent.change(screen.getByLabelText("Description"), {
      target: { value: "Analyze requirements" },
    });
    fireEvent.click(screen.getByRole("button", { name: "requirements-analysis" }));

    fireEvent.click(next());
    while (!screen.queryByText("Review your agent")) fireEvent.click(next());

    await waitFor(() =>
      expect(lastDraftFor("preview_manifest").capabilities).toEqual([
        {
          action: "requirements_analysis",
          description: "Analyze requirements",
          skills: ["requirements-analysis"],
        },
      ]),
    );
  });

  it("blocks on a malformed action name and on bad JSON", async () => {
    renderWizard();
    await fillBasics();
    fireEvent.click(next());
    fireEvent.click(next());
    fireEvent.click(next());

    fireEvent.click(screen.getByRole("button", { name: "Add action" }));
    fireEvent.change(screen.getByLabelText("Action name"), { target: { value: "Bad-Action" } });
    expect((next() as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByText(/must match \[a-z0-9_\]/)).toBeTruthy();

    fireEvent.change(screen.getByLabelText("Action name"), { target: { value: "ok_action" } });
    expect((next() as HTMLButtonElement).disabled).toBe(false);

    fireEvent.click(screen.getByRole("button", { name: "Input / output schema (JSON)" }));
    fireEvent.change(screen.getByLabelText("Input schema"), { target: { value: "{oops" } });
    expect((next() as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getAllByText(/not valid JSON/).length).toBeGreaterThan(0);
  });

  it("warns about a skill dependency nothing declares, without blocking", async () => {
    renderWizard();
    await fillBasics();
    fireEvent.click(next());
    fireEvent.click(next());
    fireEvent.click(next());

    const file = new File([new Uint8Array([1])], "req.zip", { type: "application/zip" });
    fireEvent.change(document.querySelector('input[type="file"]') as HTMLInputElement, {
      target: { files: [file] },
    });

    // The row names the dependency and says it is undeclared...
    await waitFor(() => expect(screen.getByText(/Requires: file_read/)).toBeTruthy());
    expect(screen.getByText(/not declared in tools\/MCP/)).toBeTruthy();
    // ...and the step still lets the user through.
    expect((next() as HTMLButtonElement).disabled).toBe(false);
  });
});

describe("CreateWizard — step 5: preview and create", () => {
  /** `configure` runs on step 1, i.e. while the identity fields exist. */
  async function goToPreview(configure?: () => Promise<void> | void) {
    renderWizard();
    await fillBasics();
    if (configure) await configure();
    fireEvent.click(next());
    fireEvent.click(next());
    fireEvent.click(next());
    fireEvent.click(next());
    await waitFor(() => expect(screen.getByText("Review your agent")).toBeTruthy());
    await waitFor(() => expect(invoke.mock.calls.some((c) => c[0] === "preview_manifest")).toBe(true));
  }

  it("renders the TOML returned by the backend, not a client-built string", async () => {
    wireInvoke({
      preview_manifest: () => ({
        toml: '[config]\ntemperature = 0.5',
        errors: [],
        warnings: [],
      }),
    });
    await goToPreview();
    expect(screen.getByText(/temperature = 0.5/)).toBeTruthy();
  });

  it("shows warnings as non-blocking hints", async () => {
    wireInvoke({
      preview_manifest: () => ({
        toml: 'agent_id = "com.example.agent"',
        errors: [],
        warnings: ["skill `s` depends on `x`, which is not declared"],
      }),
    });
    await goToPreview();
    expect(screen.getByText(/1 hint\(s\)/)).toBeTruthy();
    expect((screen.getByRole("button", { name: "Create Agent" }) as HTMLButtonElement).disabled).toBe(
      false,
    );
  });

  it("disables create when the backend refuses the draft", async () => {
    wireInvoke({
      preview_manifest: () => {
        throw new Error("duplicate capability `x`");
      },
    });
    await goToPreview();
    expect(screen.getByText(/duplicate capability/)).toBeTruthy();
    expect((screen.getByRole("button", { name: "Create Agent" }) as HTMLButtonElement).disabled).toBe(
      true,
    );
  });

  it("sends create_agent exactly the draft it previewed", async () => {
    await goToPreview(async () => {
      fireEvent.change(screen.getByLabelText("Labels"), { target: { value: "agile" } });
      fireEvent.keyDown(screen.getByLabelText("Labels"), { key: "Enter" });
    });

    const previewDraft = lastDraftFor("preview_manifest");
    fireEvent.click(screen.getByRole("button", { name: "Create Agent" }));
    await waitFor(() => expect(onCreated).toHaveBeenCalledWith("com.example.agent"));

    // Same object, field for field: preview and create cannot drift.
    expect(lastDraftFor("create_agent")).toEqual(previewDraft);
    expect(lastDraftFor("create_agent").labels).toEqual(["agile"]);
  });

  it("surfaces a create failure inside the dialog instead of closing it", async () => {
    await goToPreview();
    wireInvoke({
      create_agent: () => {
        throw new Error("gateway rejected the package");
      },
      preview_manifest: () => ({ toml: "x", errors: [], warnings: [] }),
    });
    fireEvent.click(screen.getByRole("button", { name: "Create Agent" }));
    await waitFor(() => expect(screen.getByText(/gateway rejected the package/)).toBeTruthy());
    expect(onCreated).not.toHaveBeenCalled();
  });
});

describe("CreateWizard — reset", () => {
  it("clears the draft when reopened", async () => {
    const { rerender } = renderWizard();
    await fillBasics();
    fireEvent.change(screen.getByLabelText("Labels"), { target: { value: "agile" } });
    fireEvent.keyDown(screen.getByLabelText("Labels"), { key: "Enter" });
    expect(screen.getByRole("button", { name: "Remove label agile" })).toBeTruthy();

    rerender(<CreateWizard open={false} onCreated={onCreated} onClose={onClose} />);
    rerender(<CreateWizard open onCreated={onCreated} onClose={onClose} />);

    expect((screen.getByLabelText("Agent ID") as HTMLInputElement).value).toBe("");
    expect(screen.queryByRole("button", { name: "Remove label agile" })).toBeNull();
    // Back on step 1, not wherever the closed dialog was.
    expect(within(document.body).getByText("Basic")).toBeTruthy();
  });
});