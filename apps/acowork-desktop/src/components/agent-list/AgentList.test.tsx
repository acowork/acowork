import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";

vi.mock("../../i18n/useTranslation", () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn().mockResolvedValue({ enrolled: true }) }));
vi.mock("../common/ToastProvider", () => ({ useToast: () => ({ addToast: vi.fn() }), showToast: vi.fn() }));
vi.mock("./AgentLibraryDialog", () => ({ AgentLibraryDialog: () => null }));
vi.mock("./AgentDetailDialog", () => ({ AgentDetailDialog: () => null }));
vi.mock("./CloneDialog", () => ({ CloneDialog: () => null }));
vi.mock("./PermissionDialog", () => ({ PermissionDialog: () => null }));
vi.mock("./PublishWizard", () => ({ PublishWizard: () => null }));
vi.mock("./CreateWizard", () => ({ CreateWizard: () => null }));
vi.mock("../user-list/UserList", () => ({ UserList: () => null }));
vi.mock("../chat/NewGroupDialog", () => ({ NewGroupDialog: () => null }));
vi.mock("../common/OverlayScrollbar", () => ({ OverlayScrollbar: ({ children }: { children: React.ReactNode }) => <div>{children}</div> }));
vi.mock("../common/AgentAvatar", () => ({ AgentAvatar: () => null }));

import { AgentList } from "./AgentList";
import { useAgentStore, type AgentStorage } from "../../stores/agentStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { useProjectGroupStore } from "../../stores/projectGroupStore";
import type { AgentInfo, NodeInfo } from "../../lib/types";

const node = (id: string): NodeInfo => ({ node_id: id, node_name: id, online: true, capabilities: [] });
const agent = (id: string, nodeId: string): AgentInfo => ({
  agent_id: id, instance_id: id, node_id: nodeId, name: id, running: false, installed: true,
});

const storage = (meta: AgentInfo): AgentStorage => ({
  meta, profile: {}, sessions: [], sessionTitle: null,
  pagination: { currentPage: 1, totalPages: 1, totalCount: 0, pageSize: 20 },
  isLoading: false, accessDenied: false,
});

beforeEach(() => {
  useAgentStore.setState({
    loading: false,
    fetchAgents: vi.fn().mockResolvedValue(undefined),
    fetchNodes: vi.fn().mockResolvedValue(undefined),
  });
  useProjectGroupStore.setState({ fetchGroups: vi.fn().mockResolvedValue(undefined) });
});
afterEach(cleanup);

describe("AgentList node groups", () => {
  it.each(["local", "remote", "relay"] as const)("shows and collapses node headers in %s mode", (mode) => {
    useSettingsStore.setState({ gatewayMode: mode });
    useAgentStore.setState({
      nodes: [node("Workstation")],
      agents: { Alice: storage(agent("Alice", "Workstation")) },
    });
    render(<AgentList />);
    expect(screen.getAllByTestId("node-group-header")).toHaveLength(1);
    expect(screen.getByText("Workstation")).toBeTruthy();
    expect(screen.getByText("Alice")).toBeTruthy();
    fireEvent.click(screen.getByLabelText("Toggle node group: Workstation"));
    expect(screen.queryByText("Alice")).toBeNull();
  });

  it("shows each node including an empty node and filters headers when searching", () => {
    useSettingsStore.setState({ gatewayMode: "local" });
    useAgentStore.setState({ nodes: [node("Workstation"), node("Server")], agents: {
      Alice: storage(agent("Alice", "Workstation")),
    } });
    render(<AgentList />);
    const headers = screen.getAllByTestId("node-group-header");
    expect(headers).toHaveLength(2);
    expect(within(headers[1]).getByText("Server")).toBeTruthy();
    expect(screen.getByTestId("node-group-install")).toBeTruthy();
    fireEvent.change(screen.getByLabelText("agentList.searchPlaceholder"), { target: { value: "Alice" } });
    expect(screen.getAllByTestId("node-group-header")).toHaveLength(1);
    expect(screen.queryByText("Server")).toBeNull();
  });
});
