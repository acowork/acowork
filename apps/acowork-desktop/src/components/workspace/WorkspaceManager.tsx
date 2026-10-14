import { useState, useEffect, useCallback } from "react";
import { X, Plus, Trash2, FolderOpen, AlertCircle } from "lucide-react";
import { useSettingsStore } from "../../stores/settingsStore";
import { useToast } from "../common/ToastProvider";
import { ConfirmDialog } from "../common/ConfirmDialog";
import { open } from "@tauri-apps/plugin-dialog";
import { StyledInput } from "../common/StyledInput";
import { Dropdown } from "../common/Dropdown";
import { RemoteFolderPicker } from "./RemoteFolderPicker";
import { useTranslation } from "../../i18n/useTranslation";

interface WorkspaceDir {
  id: string;
  path: string;
  alias?: string;
  access: "read-only" | "read-write";
  added_at: string;
}

interface WorkspaceManagerProps {
  agentId: string;
  onClose: () => void;
}

export function WorkspaceManager({ agentId, onClose }: WorkspaceManagerProps) {
  const { t } = useTranslation();
  const [workspaces, setWorkspaces] = useState<WorkspaceDir[]>([]);
  const [loading, setLoading] = useState(true);
  const [showAddDialog, setShowAddDialog] = useState(false);
  const [confirmChange, setConfirmChange] = useState<{ dir: WorkspaceDir; newAccess: "read-only" | "read-write" } | null>(null);
  const [deleteConfirm, setDeleteConfirm] = useState<{ open: boolean; id: string; name: string }>({ open: false, id: "", name: "" });
  const { gatewayUrl } = useSettingsStore();
  const { addToast } = useToast();

  const loadWorkspaces = useCallback(async () => {
    try {
      const response = await fetch(`${gatewayUrl}/api/agents/${agentId}/workspaces`);
      if (response.ok) {
        const data = await response.json();
        setWorkspaces(data.workspaces || []);
      } else {
        addToast({ type: "error", message: "Failed to load workspaces" });
      }
    } catch (error) {
      addToast({ type: "error", message: `Failed to load workspaces: ${String(error)}` });
    } finally {
      setLoading(false);
    }
  }, [gatewayUrl, agentId, addToast]);

  useEffect(() => {
    loadWorkspaces();
  }, [loadWorkspaces]);

  const handleAdd = async (path: string, alias: string, access: "read-only" | "read-write") => {
    try {
      const response = await fetch(`${gatewayUrl}/api/agents/${agentId}/workspaces`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ path, alias: alias || undefined, access }),
      });

      if (response.ok) {
        addToast({ type: "success", message: "Workspace added" });
        await loadWorkspaces();
        setShowAddDialog(false);
      } else {
        const err = await response.json().catch(() => null);
        addToast({ type: "error", message: err?.error || "Failed to add workspace" });
      }
    } catch (error) {
      addToast({ type: "error", message: `Failed to add workspace: ${String(error)}` });
    }
  };

  const handleDelete = async (id: string) => {
    try {
      const response = await fetch(`${gatewayUrl}/api/agents/${agentId}/workspaces/${id}`, {
        method: "DELETE",
      });

      if (response.ok) {
        addToast({ type: "success", message: "Workspace removed" });
        await loadWorkspaces();
      } else {
        addToast({ type: "error", message: "Failed to remove workspace" });
      }
    } catch (error) {
      addToast({ type: "error", message: `Failed to remove workspace: ${String(error)}` });
    }
  };

  const handleAccessChange = async (dir: WorkspaceDir, newAccess: "read-only" | "read-write") => {
    // Require confirmation when changing from read-only to read-write
    if (dir.access === "read-only" && newAccess === "read-write") {
      setConfirmChange({ dir, newAccess });
      return;
    }

    await updateAccess(dir.id, newAccess);
  };

  const updateAccess = async (id: string, access: "read-only" | "read-write") => {
    try {
      const response = await fetch(`${gatewayUrl}/api/agents/${agentId}/workspaces/${id}`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ access }),
      });

      if (response.ok) {
        addToast({ type: "success", message: "Access level updated" });
        await loadWorkspaces();
      } else {
        addToast({ type: "error", message: "Failed to update access level" });
      }
    } catch (error) {
      addToast({ type: "error", message: `Failed to update access: ${String(error)}` });
    }
  };

  const readWriteCount = workspaces.filter(w => w.access === "read-write").length;

  return (
      <div className="fixed inset-0 z-50 flex items-center justify-center bg-modal-overlay">
      <div className="w-full max-w-2xl rounded-md border border-border-outer bg-modal-surface shadow-xl">
        {/* Header */}
        <div className="flex items-center justify-between border-b border-border-divider px-6 py-4">
          <h2 className="text-lg font-semibold text-text ">
            Workspaces
          </h2>
          <button
            onClick={onClose}
            className="rounded-md p-1 text-text-tertiary hover:bg-zinc-100 hover:text-zinc-600 dark:hover:bg-zinc-800 dark:hover:text-zinc-300"
          >
            <X className="h-5 w-5" />
          </button>
        </div>

        {/* Content */}
        <div className="max-h-[600px] overflow-y-auto">
          {readWriteCount > 0 && (
            <div className="mx-6 mt-4 flex items-center gap-2 rounded-md bg-orange-50 px-3 py-2 text-xs text-orange-700 dark:bg-orange-900/20 dark:text-orange-400">
              <AlertCircle className="h-4 w-4" />
              <span>{readWriteCount} read-write director{readWriteCount > 1 ? "ies" : "y"} — Agent can modify files</span>
            </div>
          )}

          {loading ? (
            <div className="flex items-center justify-center py-12 text-sm text-text-tertiary">
              Loading...
            </div>
          ) : workspaces.length === 0 ? (
            <div className="flex flex-col items-center justify-center py-12 text-center">
              <FolderOpen className="mb-3 h-12 w-12 text-text-secondary " />
              <p className="text-sm text-text-tertiary ">
                No additional workspaces configured
              </p>
              <p className="mt-1 text-xs text-text-tertiary ">
                Add directories for this agent to access
              </p>
            </div>
          ) : (
            <div className="divide-y divide-border-divider">
              {workspaces.map((dir) => (
                <div key={dir.id} className="flex items-center justify-between px-6 py-4">
                  <div className="flex-1">
                    <div className="flex items-center gap-2">
                      <FolderOpen className="h-4 w-4 text-text-tertiary" />
                      <span className="font-medium text-text ">
                        {dir.alias || dir.path.split(/[\/\\]/).filter(Boolean).pop() || dir.path}
                      </span>
                    </div>
                    <p className="mt-1 text-xs text-text-tertiary ">{dir.path}</p>
                  </div>

                  <div className="flex items-center gap-3">
                    {/* Access Level Dropdown */}
                    <Dropdown
                      value={dir.access}
                      onChange={(v) => handleAccessChange(dir, v as "read-only" | "read-write")}
                      className={`rounded-md border px-2 py-1.5 text-xs font-medium ${dir.access === "read-write"
                        ? "border-orange-300 bg-orange-50 text-orange-700 dark:border-orange-700 dark:bg-orange-900/30 dark:text-orange-400"
                        : "border-zinc-300 bg-zinc-50 text-text-secondary dark:border-zinc-600 dark:bg-zinc-800 "
                        }`}
                      options={[
                        { value: "read-only", label: "🔒 Read-only" },
                        { value: "read-write", label: "✏ Read-write" },
                      ]}
                    />

                    {/* Delete Button */}
                    <button
                      onClick={() => setDeleteConfirm({ open: true, id: dir.id, name: dir.alias || dir.path.split(/[\/\\]/).filter(Boolean).pop() || dir.path })}
                      className="rounded-md p-1.5 text-text-tertiary hover:bg-red-50 hover:text-red-600 dark:hover:bg-red-900/20 dark:hover:text-red-400"

                    >
                      <Trash2 className="h-4 w-4" />
                    </button>
                  </div>
                </div>
              ))}
            </div>
          )}
        </div>

        {/* Footer */}
        <div className="flex items-center justify-between border-t border-border-divider px-6 py-4">
          <button
            onClick={() => setShowAddDialog(true)}
            className="flex items-center gap-2 rounded btn-solid px-4 py-2 text-sm font-medium"
          >
            <Plus className="h-4 w-4" />
            Add Workspace
          </button>
          <button
            onClick={onClose}
            className="rounded-md px-4 py-2 text-sm font-medium text-text-secondary hover:bg-zinc-100  dark:hover:bg-zinc-800"
          >
            Done
          </button>
        </div>
      </div>

      {/* Add Workspace Dialog */}
      {showAddDialog && (
        <AddWorkspaceDialog
          onClose={() => setShowAddDialog(false)}
          onAdd={handleAdd}
          recentPaths={workspaces.map(w => w.path)}
        />
      )}

      {/* Confirm Permission Change */}
      {confirmChange && (
        <ConfirmPermissionChange
          dir={confirmChange.dir}
          newAccess={confirmChange.newAccess}
          onConfirm={() => {
            updateAccess(confirmChange.dir.id, confirmChange.newAccess);
            setConfirmChange(null);
          }}
          onCancel={() => setConfirmChange(null)}
        />
      )}

      {/* Delete Confirm Dialog */}
      <ConfirmDialog
        open={deleteConfirm.open}
        title={t("workspace.removeWorkspaceTitle")}
        message={`Remove \"${deleteConfirm.name}\" from the workspace list? The directory itself will not be deleted.`}
        confirmLabel={t("workspace.buttonRemove")}
        destructive
        onConfirm={async () => {
          setDeleteConfirm(prev => ({ ...prev, open: false }));
          await handleDelete(deleteConfirm.id);
        }}
        onCancel={() => setDeleteConfirm(prev => ({ ...prev, open: false }))}
      />
    </div>
  );
}

// ─── Add Workspace Dialog ──────────────────────────────────────────────────

function AddWorkspaceDialog({ onClose, onAdd, recentPaths: _recentPaths }: { onClose: () => void; onAdd: (path: string, alias: string, access: "read-only" | "read-write") => void; recentPaths: string[] }) {
  const { t } = useTranslation();
  const { gatewayMode } = useSettingsStore();
  const [path, setPath] = useState("");
  const [alias, setAlias] = useState("");
  const [access, setAccess] = useState<"read-only" | "read-write">("read-only");
  const [showRemotePicker, setShowRemotePicker] = useState(false);

  const handleBrowse = async () => {
    // Relay mode: server-side fs browse is blocked by the Gateway's
    // remote ACL (design doc 24 §7.2) and the local dialog is
    // meaningless for a remote workspace host — button is disabled.
    if (gatewayMode === "relay") {
      return;
    }
    if (gatewayMode === "remote") {
      setShowRemotePicker(true);
      return;
    }
    try {
      const selected = await open({ directory: true });
      if (selected) {
        setPath(selected);
        if (!alias) {
          setAlias(selected.split(/[\/\\]/).filter(Boolean).pop() || "");
        }
      }
    } catch (error) {
      // User cancelled dialog or error — no toast needed
    }
  };

  const handleSubmit = () => {
    if (!path) return;
    onAdd(path, alias, access);
  };

  return (
      <div className="fixed inset-0 z-50 flex items-center justify-center bg-modal-overlay">
      <div className="w-full max-w-lg rounded-md bg-modal-surface shadow-xl">
        <div className="flex items-center justify-between border-b border-border-divider px-6 py-4">
          <h3 className="text-base font-semibold text-text ">Add Workspace</h3>
          <button onClick={onClose} className="rounded-md p-1 text-text-tertiary hover:bg-zinc-100 dark:hover:bg-zinc-800">
            <X className="h-5 w-5" />
          </button>
        </div>

        <div className="space-y-4 px-6 py-6">
          <div>
            <label className="mb-1.5 block text-sm font-medium text-text-secondary ">
              Path
            </label>
            <div className="flex gap-2">
              <StyledInput
                type="text"
                value={path}
                onChange={(e) => setPath(e.target.value)}
                placeholder="e.g. F:/work/project"
                className="flex-1 py-2 text-sm"
              />
              <button
                onClick={handleBrowse}
                disabled={gatewayMode === "relay"}
                className="rounded-md border border-zinc-300 px-3 py-2 text-sm hover:bg-zinc-50 disabled:opacity-40 dark:border-zinc-600 dark:hover:bg-zinc-700"
              >
                {gatewayMode === "remote" ? t("workspace.remoteBrowseBtn") : t("workspace.buttonBrowse")}
              </button>
            </div>
          </div>

          <div>
            <label className="mb-1.5 block text-sm font-medium text-text-secondary ">
              Alias (optional)
            </label>
            <StyledInput
              type="text"
              value={alias}
              onChange={(e) => setAlias(e.target.value)}
              placeholder="e.g. my-project"
              className="py-2 text-sm"
            />
          </div>

          <div>
            <label className="mb-1.5 block text-sm font-medium text-text-secondary ">
              Access Level
            </label>
            <div className="space-y-2">
              <label
                className={`flex cursor-pointer items-start gap-3 rounded-md border p-3 ${access === "read-only"
                  ? "border-[var(--color-accent)]/50"
                  : "border-zinc-300 dark:border-zinc-600"
                  }`}
                style={access === "read-only" ? { backgroundColor: "color-mix(in srgb, var(--color-accent) 10%, transparent)" } : undefined}
              >
                <input
                  type="radio"
                  name="access"
                  value="read-only"
                  checked={access === "read-only"}
                  onChange={(e) => setAccess(e.target.value as "read-only")}
                  className="mt-0.5"
                />
                <div>
                  <div className="text-sm font-medium text-text ">
                    🔒 Read-only (default)
                  </div>
                  <div className="text-xs text-text-tertiary ">
                    Agent can only read files
                  </div>
                </div>
              </label>

              <label
                className={`flex cursor-pointer items-start gap-3 rounded-md border p-3 ${access === "read-write"
                  ? "border-orange-500 bg-orange-50 dark:border-orange-600 dark:bg-orange-900/20"
                  : "border-zinc-300 dark:border-zinc-600"
                  }`}
              >
                <input
                  type="radio"
                  name="access"
                  value="read-write"
                  checked={access === "read-write"}
                  onChange={(e) => setAccess(e.target.value as "read-write")}
                  className="mt-0.5"
                />
                <div>
                  <div className="text-sm font-medium text-text ">
                    ✏️ Read-write
                  </div>
                  <div className="flex items-center gap-1 text-xs text-orange-600 dark:text-orange-400">
                    <AlertCircle className="h-3 w-3" />
                    Agent can modify and delete files
                  </div>
                </div>
              </label>
            </div>
          </div>
        </div>

        <div className="flex items-center justify-end gap-2 border-t border-border-divider px-6 py-4">
          <button
            onClick={onClose}
            className="rounded-md px-4 py-2 text-sm font-medium text-text-secondary hover:bg-zinc-100  dark:hover:bg-zinc-800"
          >
            Cancel
          </button>
          <button
            onClick={handleSubmit}
            disabled={!path}
            className="rounded-md px-4 py-2 text-sm font-medium text-white disabled:opacity-50"
            style={{ backgroundColor: "var(--color-accent-strong)" }}
            onMouseEnter={(e) => { e.currentTarget.style.filter = "brightness(0.85)"; }}
            onMouseLeave={(e) => { e.currentTarget.style.filter = ""; }}
          >
            Add
          </button>
        </div>
      </div>

      {/* Remote folder picker (only shown in remote mode) */}
      {showRemotePicker && (
        <RemoteFolderPicker
          onSelect={(selectedPath: string) => {
            setShowRemotePicker(false);
            setPath(selectedPath);
            if (!alias) {
              setAlias(selectedPath.split("/").filter(Boolean).pop() || "");
            }
          }}
          onCancel={() => setShowRemotePicker(false)}
        />
      )}
    </div>
  );
}

// ─── Confirm Permission Change ─────────────────────────────────────────────

function ConfirmPermissionChange({
  dir,
  onConfirm,
  onCancel,
}: {
  dir: WorkspaceDir;
  newAccess: "read-only" | "read-write";
  onConfirm: () => void;
  onCancel: () => void;
}) {
  return (
      <div className="fixed inset-0 z-50 flex items-center justify-center bg-modal-overlay">
      <div className="w-full max-w-md rounded-md bg-modal-surface shadow-xl">
        <div className="flex items-start gap-3 px-6 py-5">
          <div className="flex h-10 w-10 flex-shrink-0 items-center justify-center rounded-full bg-orange-100 dark:bg-orange-900/30">
            <AlertCircle className="h-5 w-5 text-orange-600 dark:text-orange-400" />
          </div>
          <div className="flex-1">
            <h3 className="text-base font-semibold text-text ">
              Change to Read-write?
            </h3>
            <p className="mt-2 text-sm text-text-secondary ">
              Agent will be able to <strong>modify and delete</strong> files in:
            </p>
            <p className="mt-1 text-xs font-mono text-text-tertiary ">
              {dir.path}
            </p>
          </div>
        </div>

        <div className="flex items-center justify-end gap-2 border-t border-border-divider px-6 py-4">
          <button
            onClick={onCancel}
            className="rounded-md px-4 py-2 text-sm font-medium text-text-secondary hover:bg-zinc-100  dark:hover:bg-zinc-800"
          >
            Cancel
          </button>
          <button
            onClick={onConfirm}
            className="rounded-md bg-orange-600 px-4 py-2 text-sm font-medium text-white hover:bg-orange-700"
          >
            Confirm
          </button>
        </div>
      </div>
    </div>
  );
}
