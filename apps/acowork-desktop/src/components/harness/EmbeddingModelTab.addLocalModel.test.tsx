//! The "add local embedding model" dialog.
//!
//! Pins the three things a user gets wrong here, all of which are silent
//! failures elsewhere:
//!  - a pasted HuggingFace URL must be reduced to `owner/name` (a URL in
//!    `hf_repo` builds an unresolvable download URL);
//!  - a missing / zero dimension must be refused before the request, because
//!    the embed service has no way to discover it after the fact;
//!  - the "Open on HuggingFace" link goes through `openExternal` (the single
//!    blessed path out of the app), with the bare repo id appended.

import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { AddLocalEmbeddingModelDialog } from "./EmbeddingModelTab";

const mockAdd = vi.fn(async () => ({
  model_id: "custom-bge",
  status: "added",
  message: "ok",
}));
const mockOpenExternal = vi.fn(async () => {});

vi.mock("../../lib/gateway-api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../lib/gateway-api")>()),
  addLocalEmbeddingModel: (...args: unknown[]) => mockAdd(...(args as [])),
}));

vi.mock("../../lib/openExternal", () => ({
  openExternal: (url: string) => mockOpenExternal(url),
}));

const noop = () => {};

function renderDialog(existingIds: string[] = []) {
  const onSaved = vi.fn(async () => {});
  render(
    <AddLocalEmbeddingModelDialog
      open
      onClose={noop}
      onSaved={onSaved}
      existingIds={existingIds}
    />,
  );
  const type = (placeholder: string | RegExp, value: string) => {
    fireEvent.change(screen.getByPlaceholderText(placeholder), {
      target: { value },
    });
  };
  return { onSaved, type };
}

describe("AddLocalEmbeddingModelDialog", () => {
  beforeEach(() => {
    mockAdd.mockClear();
    mockOpenExternal.mockClear();
  });

  it("normalizes a pasted HuggingFace URL and sends the configured defaults", async () => {
    const { onSaved, type } = renderDialog();

    type("e.g. bge-small-zh-custom", "custom-bge");
    type("e.g. BGE Small (custom)", "Custom BGE");
    type("onnx-community/bge-small-zh-v1.5-ONNX", "https://huggingface.co/Xenova/bge-base-zh-v1.5/");
    type("e.g. 512", "768");

    fireEvent.click(screen.getByText("Save"));

    await waitFor(() => expect(mockAdd).toHaveBeenCalledTimes(1));
    expect(mockAdd.mock.calls[0][0]).toEqual({
      id: "custom-bge",
      name: "Custom BGE",
      dimension: 768,
      hf_repo: "Xenova/bge-base-zh-v1.5",
      max_tokens: undefined,
      onnx_file: "onnx/model.onnx",
      tokenizer_file: "tokenizer.json",
      pooling_strategy: "cls",
      onnx_output_kind: "hidden_states",
    });
    await waitFor(() => expect(onSaved).toHaveBeenCalled());
  });

  it("refuses a zero dimension instead of writing a model that cannot embed", async () => {
    const { type } = renderDialog();

    type("e.g. bge-small-zh-custom", "custom-bge");
    type("e.g. BGE Small (custom)", "Custom BGE");
    type("onnx-community/bge-small-zh-v1.5-ONNX", "owner/repo");
    type("e.g. 512", "0");

    fireEvent.click(screen.getByText("Save"));

    await screen.findByText("Dimension must be a whole number greater than 0");
    expect(mockAdd).not.toHaveBeenCalled();
  });

  it("refuses an id that is already in the catalog", async () => {
    const { type } = renderDialog(["bge-small-zh-v1.5"]);

    type("e.g. bge-small-zh-custom", "bge-small-zh-v1.5");
    type("e.g. BGE Small (custom)", "Dup");
    type("onnx-community/bge-small-zh-v1.5-ONNX", "owner/repo");
    type("e.g. 512", "512");

    fireEvent.click(screen.getByText("Save"));

    await screen.findByText("Model ID 'bge-small-zh-v1.5' already exists");
    expect(mockAdd).not.toHaveBeenCalled();
  });

  it("opens the repo on HuggingFace through openExternal", () => {
    const { type } = renderDialog();

    type("onnx-community/bge-small-zh-v1.5-ONNX", "owner/repo");
    fireEvent.click(screen.getByText("Open on HuggingFace"));

    expect(mockOpenExternal).toHaveBeenCalledWith("https://huggingface.co/owner/repo");
  });
});