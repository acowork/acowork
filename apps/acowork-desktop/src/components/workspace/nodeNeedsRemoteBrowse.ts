import { isLoopbackGatewayUrl } from "../../lib/gatewayUrl";

/**
 * Whether "Add workspace → Browse" must open {@link RemoteFolderPicker}
 * against this Node (ADR-055 L7-1) instead of the Desktop Tauri dialog.
 *
 * Gateway-local agents (`node_id === "local"`, or a Node whose
 * `http_endpoint` is loopback) stay on the OS folder picker. Agents on a
 * phone / LAN Node — including when Desktop's Gateway itself is in
 * `local` mode — must browse that machine via `/api/fs/browse?target=…`.
 */
export function nodeNeedsRemoteBrowse(
  nodeId: string | undefined,
  nodes: { node_id: string; http_endpoint?: string }[],
): boolean {
  if (!nodeId || nodeId === "local") return false;
  const node = nodes.find((n) => n.node_id === nodeId);
  // Unknown / not yet listed → prefer remote browse (safer than a Desktop path).
  if (!node?.http_endpoint) return true;
  return !isLoopbackGatewayUrl(node.http_endpoint);
}
