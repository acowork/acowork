import { useCallback, useEffect, useRef, useState } from "react";
import { Sparkles, FolderPlus, Check, Loader2 } from "lucide-react";
import { cn } from "../../lib/utils";
import { menuItemHover, menuItemSelected } from "../../lib/ui-styles";
import { useSkillStore } from "../../stores/skillStore";
import { useAgentStore } from "../../stores/agentStore";
import { useTranslation } from "../../i18n/useTranslation";
import { ErrorBox } from "../common/ErrorBox";

/**
 * The skill list — one level of the composer `+` menu.
 *
 * Extracted from the old standalone skills toolbar dropdown, which the `+`
 * menu replaced (composer compaction). Nothing else changed about how a skill
 * is chosen: the store still holds the *session-scoped* `activeSkill`, which
 * the composer renders as a `/name` chip and passes as `skill_name` on the
 * next message. Picking the active skill again clears it, mirroring the old
 * toggle behaviour.
 *
 * `onDone` fires after a pick so the caller can close the menu; the import
 * dialog keeps itself open on top of it (the menu unmounts underneath, which
 * is why the dialog is rendered here rather than by the menu root).
 */
export function SkillMenuList({
  onDone,
  readOnly,
}: {
  /** Called after a skill is picked or cleared. */
  onDone?: () => void;
  /** ADR-076: a session shared with us cannot send, so a skill cannot take
   *  effect. Rows stay visible (the owner's active skill must still be
   *  readable) but are inert. */
  readOnly?: boolean;
}) {
  const { t } = useTranslation();
  const { selectedAgentId } = useAgentStore();
  const { skills, loading, fetchSkills, activeSkill, setActiveSkill, clearActiveSkill, importSkill } =
    useSkillStore();

  const [importDialogOpen, setImportDialogOpen] = useState(false);
  const [selectedFile, setSelectedFile] = useState<File | null>(null);
  const [importing, setImporting] = useState(false);
  const [importError, setImportError] = useState<string | null>(null);
  const [importSuccess, setImportSuccess] = useState<string | null>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);

  // Load skills for the current agent. The store caches per agent, so opening
  // the menu repeatedly does not re-hit the Runtime.
  useEffect(() => {
    if (!selectedAgentId) return;
    void fetchSkills(selectedAgentId);
  }, [selectedAgentId, fetchSkills]);

  const handleImportClick = () => {
    setImportDialogOpen(true);
    setSelectedFile(null);
    setImportError(null);
    setImportSuccess(null);
  };

  const handleFileSelect = (e: React.ChangeEvent<HTMLInputElement>) => {
    const file = e.target.files?.[0];
    if (file) {
      setSelectedFile(file);
      setImportError(null);
    }
  };

  const handleDrop = useCallback((e: React.DragEvent) => {
    e.preventDefault();
    e.stopPropagation();
    const file = e.dataTransfer.files?.[0];
    if (file && file.name.endsWith(".zip")) {
      setSelectedFile(file);
      setImportError(null);
    } else {
      setImportError(t("skillsPanel.importZipOnly"));
    }
  }, [t]);

  const handleDragOver = useCallback((e: React.DragEvent) => {
    e.preventDefault();
    e.stopPropagation();
  }, []);

  const handleCloseDialog = () => {
    if (importing) return;
    setImportDialogOpen(false);
    setSelectedFile(null);
    setImportError(null);
    setImportSuccess(null);
  };

  const handleImport = async () => {
    if (!selectedAgentId || !selectedFile) return;
    setImporting(true);
    setImportError(null);
    setImportSuccess(null);
    const result = await importSkill(selectedAgentId, selectedFile);
    setImporting(false);
    if (result.success) {
      setImportSuccess(
        result.message ||
          t("skillsPanel.importSuccess", { name: result.skillName ?? "" }),
      );
      setSelectedFile(null);
      setTimeout(() => setImportDialogOpen(false), 2000);
    } else {
      setImportError(result.message || t("skillsPanel.importFailed"));
    }
  };

  return (
    <>
      {loading && skills.length === 0 ? (
        <div className="px-3 py-3 text-center text-xs text-text-tertiary">
          {t("skillsPanel.loading")}
        </div>
      ) : skills.length === 0 ? (
        <div className="px-3 py-3 text-center text-xs text-text-tertiary">
          {t("skillsPanel.empty")}
        </div>
      ) : (
        <div className="space-y-0.5">
          {skills.map((skill) => {
            const isActive = activeSkill?.name === skill.name;
            return (
              <button
                key={skill.name}
                type="button"
                disabled={readOnly}
                onClick={() => {
                  if (isActive) clearActiveSkill();
                  else setActiveSkill(skill);
                  onDone?.();
                }}
                className={cn(
                  "flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-text-secondary transition-colors",
                  isActive ? menuItemSelected : menuItemHover,
                  readOnly && "cursor-not-allowed opacity-50",
                )}
              >
                <Sparkles className="h-3.5 w-3.5 shrink-0 text-text-tertiary" />
                <div className="min-w-0 flex-1">
                  <div className="truncate text-xs font-medium">{skill.name}</div>
                  {skill.description && (
                    <div className="truncate text-10 text-text-tertiary">
                      {skill.description}
                    </div>
                  )}
                </div>
                {isActive && (
                  <Check
                    className="h-3.5 w-3.5 shrink-0"
                    style={{ color: "var(--color-accent)" }}
                  />
                )}
                {!isActive && skill.triggers.length > 0 && (
                  <span className="shrink-0 rounded bg-zinc-100 px-1.5 py-0.5 text-10 text-text-tertiary dark:bg-zinc-700">
                    {skill.triggers.length}
                  </span>
                )}
              </button>
            );
          })}
        </div>
      )}

      {/* Import — an agent-level write, so it stays available even on a
          read-only session (it is reachable from any session the user owns).
          No capability is lost by keeping it disabled here rather than
          hiding it: a viewer can open their own session to import. */}
      <button
        type="button"
        onClick={handleImportClick}
        disabled={readOnly || !selectedAgentId}
        className={cn(
          "mt-1 flex w-full items-center justify-center gap-1.5 rounded-md bg-zinc-100 px-2 py-1.5 text-xs font-medium text-text-secondary transition-colors",
          readOnly
            ? "cursor-not-allowed opacity-50"
            : "hover:bg-zinc-200 hover:text-zinc-900 dark:bg-white/10 dark:hover:bg-white/15 dark:hover:text-zinc-100",
        )}
      >
        <FolderPlus className="h-3.5 w-3.5" />
        {t("skillsPanel.buttonImportSkills")}
      </button>

      {importDialogOpen && (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-modal-overlay"
          onClick={handleCloseDialog}
        >
          <div
            className="flex w-96 flex-col rounded-md border border-border-outer bg-modal-surface shadow-xl"
            onClick={(e) => e.stopPropagation()}
          >
            <div className="flex items-center gap-2 border-b border-border-divider px-5 py-3 min-h-[var(--ui-dialog-zone-h)]">
              <FolderPlus className="h-5 w-5 text-text-tertiary" />
              <h2 className="text-sm font-semibold text-text">
                {t("skillsPanel.buttonImportSkills")}
              </h2>
            </div>

            <div className="space-y-4 px-5 py-4">
              <p className="mb-4 text-xs text-text-tertiary">
                {t("skillsPanel.importHint")}
              </p>

              <div
                onDrop={handleDrop}
                onDragOver={handleDragOver}
                onClick={() => fileInputRef.current?.click()}
                className={cn(
                  "mb-3 cursor-pointer rounded-md border-2 border-dashed p-6 text-center transition-colors",
                  selectedFile
                    ? "border-[var(--color-accent)]/40"
                    : "border-zinc-300 hover:border-zinc-400 dark:border-zinc-600 dark:hover:border-zinc-500",
                )}
                style={
                  selectedFile
                    ? {
                        backgroundColor:
                          "color-mix(in srgb, var(--color-accent) 10%, transparent)",
                      }
                    : undefined
                }
              >
                <input
                  ref={fileInputRef}
                  type="file"
                  accept=".zip"
                  onChange={handleFileSelect}
                  className="hidden"
                />
                {selectedFile ? (
                  <div className="text-xs">
                    <div
                      className="mb-1 font-medium"
                      style={{ color: "var(--color-accent)" }}
                    >
                      {selectedFile.name}
                    </div>
                    <div className="text-text-tertiary">
                      {(selectedFile.size / 1024).toFixed(1)} KB
                    </div>
                  </div>
                ) : (
                  <div className="text-xs text-text-tertiary">
                    <FolderPlus className="mx-auto mb-2 h-6 w-6" />
                    <div>{t("skillsPanel.importDropZone")}</div>
                  </div>
                )}
              </div>

              {importError && (
                <ErrorBox
                  message={importError}
                  onClose={() => setImportError(null)}
                />
              )}
              {importSuccess && (
                <div className="mb-3 flex items-center gap-2 rounded-md bg-green-50 p-2 text-xs text-green-700 dark:bg-green-900/20 dark:text-green-300">
                  <Check className="h-3.5 w-3.5 shrink-0" />
                  {importSuccess}
                </div>
              )}
            </div>

            <div className="flex items-center justify-end gap-2 border-t border-border-divider px-5 min-h-[var(--ui-dialog-zone-h)]">
              <button
                onClick={handleCloseDialog}
                disabled={importing}
                className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-100 disabled:opacity-50 dark:hover:bg-zinc-700"
              >
                {t("common.cancel")}
              </button>
              <button
                onClick={handleImport}
                disabled={!selectedFile || importing}
                className="flex items-center gap-2 rounded btn-accent px-3 py-1.5 text-xs font-medium disabled:cursor-not-allowed disabled:opacity-50"
              >
                {importing && <Loader2 className="h-3 w-3 animate-spin" />}
                {importing
                  ? t("skillsPanel.importing")
                  : t("skillsPanel.buttonImport")}
              </button>
            </div>
          </div>
        </div>
      )}
    </>
  );
}
