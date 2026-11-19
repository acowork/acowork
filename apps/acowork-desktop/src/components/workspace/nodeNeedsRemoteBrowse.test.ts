import { describe, expect, it } from "vitest";
import { nodeNeedsRemoteBrowse } from "./nodeNeedsRemoteBrowse";

describe("nodeNeedsRemoteBrowse", () => {
  const nodes = [
    { node_id: "desk", http_endpoint: "http://127.0.0.1:19900" },
    { node_id: "phone", http_endpoint: "http://192.168.18.40:19900" },
  ];

  it("keeps the Desktop Tauri dialog for Gateway-local / loopback Nodes", () => {
    expect(nodeNeedsRemoteBrowse(undefined, nodes)).toBe(false);
    expect(nodeNeedsRemoteBrowse("local", nodes)).toBe(false);
    expect(nodeNeedsRemoteBrowse("desk", nodes)).toBe(false);
  });

  it("opens the remote picker for a LAN Node (phone) even if Gateway is local", () => {
    expect(nodeNeedsRemoteBrowse("phone", nodes)).toBe(true);
  });

  it("prefers remote browse when the Node is not in the list yet", () => {
    expect(nodeNeedsRemoteBrowse("unknown-uuid", nodes)).toBe(true);
  });
});
