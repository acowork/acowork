import { useState, useEffect, useRef, useCallback } from "react";
import { Sparkles, FolderPlus, Check, Loader2 } from "lucide-react";
import { cn } from "../../lib/utils";
import { menuItemHover, menuItemSelected } from "../../lib/ui-styles";
import { ToolbarDropdownTrigger } from "../common/ToolbarDropdown";
import { useSkillStore } from "../../stores/skillStore";
import { useAgentStore } from "../../stores/agentStore";
import { useTranslation } from "../../i18n/useTranslation";
import { ErrorBox } from "../common/ErrorBox";

/**
 * `readOnly` (ADR-076): the active session is shared with us, so selecting a
 * skill cannot take effect — the active skill is passed on the next message,
 * and we cannot send one. The trigger is disabled (not hidden) so the active
 * skill of the owner's session stays visible, matching model / workspace.
 *
 * Note the dropdown is also the entry point for *importing* a skill, which is
 * an agent-level (not session-level) write the backend does not gate. That
 * path is unreachable while viewing someone else's session and still works
 * from any session we own — no capability is actually lost.
 */
export function SkillsPanel({ textHidden, readOnly }: { textHidden?: boolean; readOnly?: boolean } = {}) {
  const { t } = useTranslation();
  const { selectedAgentId } = useAgentStore();
  const {
    skills,
    loading,
    fetchSkills,
    activeSkill,
    setActiveSkill,
    clearActiveSkill,
    importSkill,
  } = useSkillStore();
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  // Import dialog state
  const [importDialogOpen, setImportDialogOpen] = useState(false);
  const [selectedFile, setSelectedFile] = useState<File | null>(null);
  const [importing, setImporting] = useState(false);
  const [importError, setImportError] = useState<string | null>(null);
  const [importSuccess, setImportSuccess] = useState<string | null>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);

  // Load skills when agent changes or dropdown opens
  useEffect(() => {
    if (!selectedAgentId) return;
    void fetchSkills(selectedAgentId);
  }, [selectedAgentId, fetchSkills]);

  // Close on outside click
  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [open]);

  const handleImportClick = () => {
    setOpen(false);
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
      setImportError("Please drop a .zip file");
    }
  }, []);

  const handleDragOver = useCallback((e: React.DragEvent) => {
    e.preventDefault();
    e.stopPropagation();
  }, []);

  const handleImport = async () => {
    if (!selectedAgentId || !selectedFile) return;

    setImporting(true);
    setImportError(null);
    setImportSuccess(null);

    const result = await importSkill(selectedAgentId, selectedFile);

    setImporting(false);
    if (result.success) {
      setImportSuccess(result.message || `Skill "${result.skillName}" imported successfully`);
      setSelectedFile(null);
      // Auto-close after 2 seconds
      setTimeout(() => {
        setImportDialogOpen(false);
        setImportSuccess(null);
      }, 2000);
    } else {
      setImportError(result.message || "Import failed");
    }
  };

  const handleCloseDialog = () => {
    setImportDialogOpen(false);
    setSelectedFile(null);
    setImportError(null);
    setImportSuccess(null);
  };

  const skillCount = skills.length;
  const skillsLabel = skillCount > 0 ? t("skillsPanel.skillsLabel", { count: skillCount }) : t("skillsPanel.title");

  return (
    <>
      <ToolbarDropdownTrigger
        icon={<Sparkles size={14} />}
        label={skillsLabel}
        collapseClass="tb-sk-text"
        tipClass="tb-sk-tip"
        tooltip={readOnly ? t("chatPanel.readOnlySession") : t("skillsPanel.selectSkill")}
        open={open}
        onToggle={() => !readOnly && setOpen(!open)}
        wrapperRef={ref}
        textHidden={textHidden}
        btnId="sk"
        disabled={readOnly}
      >
        {/* Dropdown menu */}
        {open && (
          <div className="absolute bottom-full left-0 mb-1 w-60 rounded-md border border-border-outer bg-modal-surface shadow-lg" style={{ zIndex: 100 }}>
            {/* Menu title */}
            <div className="px-3 pt-2.5 pb-1">
              <h2 className="text-sm font-normal text-text-secondary ">
                {t("skillsPanel.title")}
              </h2>
            </div>
            <div className="border-t border-border-divider" />

            {/* Skills list */}
            <div className="max-h-[420px] overflow-y-auto py-1">
              {loading && skills.length === 0 ? (
                <div className="py-4 text-center text-xs text-text-tertiary">{t("skillsPanel.loading")}</div>
              ) : skills.length === 0 ? (
                <div className="py-4 text-center text-xs text-text-tertiary">No skills loaded</div>
              ) : (
                <div className="space-y-0.5">
                  {skills.map((skill) => {
                    const isActive = activeSkill?.name === skill.name;
                    return (
                      <button
                        key={skill.name}
                        type="button"
                        onClick={() => {
                          if (isActive) {
                            clearActiveSkill();
                          } else {
                            setActiveSkill(skill);
                          }
                          setOpen(false);
                        }}
                        className={cn(
                          "flex w-full items-center gap-2 px-3 py-1.5 text-left text-text-secondary transition-colors",
                          isActive ? menuItemSelected : menuItemHover,
                        )}
                      >
                        <Sparkles className="h-3.5 w-3.5 shrink-0 text-text-tertiary" />
                        <div className="min-w-0 flex-1">
                          <div className="truncate text-xs font-medium">
                            {skill.name}
                          </div>
                          {skill.description && (
                            <div className="truncate text-10 text-text-tertiary ">
                              {skill.description}
                            </div>
                          )}
                        </div>
                        {isActive && (
                          <Check className="h-3.5 w-3.5 shrink-0" style={{ color: "var(--color-accent)" }} />
                        )}
                        {!isActive && skill.triggers.length > 0 && (
                          <span className="shrink-0 rounded bg-zinc-100 px-1.5 py-0.5 text-10 text-text-tertiary dark:bg-zinc-700 ">
                            {skill.triggers.length}
                          </span>
                        )}
                      </button>
                    );
                  })}
                </div>
              )}
            </div>

            {/* Divider */}
            <div className="border-t border-border-divider" />

            {/* Import Skills button */}
            <button
              onClick={handleImportClick}
              className="mx-3 mt-2 mb-2.5 flex w-[calc(100%-1.5rem)] items-center justify-center gap-1.5 rounded-md bg-zinc-100 px-3 py-[var(--ui-btn-py)] text-xs font-medium text-text-secondary transition-colors hover:bg-zinc-200 hover:text-zinc-900 dark:bg-white/10  dark:hover:bg-white/15 dark:hover:text-zinc-100"
            >
              <FolderPlus className="h-3.5 w-3.5" />
              {t("skillsPanel.buttonImportSkills")}
            </button>
          </div>
        )}
      </ToolbarDropdownTrigger>

      {/* Import Dialog */}
      {importDialogOpen && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-modal-overlay" onClick={handleCloseDialog}>
          <div
            className="flex w-96 flex-col rounded-md border border-border-outer bg-modal-surface shadow-xl"
            onClick={(e) => e.stopPropagation()}
          >
            {/* Header */}
            <div className="flex items-center gap-2 border-b border-border-divider px-5 py-3 min-h-[var(--ui-dialog-zone-h)]">
              <FolderPlus className="h-5 w-5 text-text-tertiary " />
              <h2 className="text-sm font-semibold text-text ">
                {t("skillsPanel.buttonImportSkills")}
              </h2>
            </div>

            {/* Body */}
            <div className="space-y-4 px-5 py-4">
            {/* Description */}
            <p className="mb-4 text-xs text-text-tertiary ">
              Select a skill ZIP package to import. The ZIP must contain a{" "}
              <code className="rounded bg-zinc-100 px-1 py-0.5 text-text-secondary dark:bg-zinc-700 ">
                SKILL.md
              </code>{" "}
              file with YAML frontmatter.
            </p>

            {/* Drop zone */}
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
              style={selectedFile ? { backgroundColor: "color-mix(in srgb, var(--color-accent) 10%, transparent)" } : undefined}
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
                  <div className="mb-1 font-medium" style={{ color: "var(--color-accent)" }}>
                    {selectedFile.name}
                  </div>
                  <div className="text-text-tertiary ">
                    {(selectedFile.size / 1024).toFixed(1)} KB
                  </div>
                </div>
              ) : (
                <div className="text-xs text-text-tertiary ">
                  <FolderPlus className="mx-auto mb-2 h-6 w-6" />
                  <div>Click to select or drop a .zip file</div>
                </div>
              )}
            </div>

            {/* Error / Success messages */}
            {importError && (
              <ErrorBox message={importError} onClose={() => setImportError(null)} />
            )}
            {importSuccess && (
              <div className="mb-3 flex items-center gap-2 rounded-md bg-green-50 p-2 text-xs text-green-700 dark:bg-green-900/20 dark:text-green-300">
                <Check className="h-3.5 w-3.5 shrink-0" />
                {importSuccess}
              </div>
            )}

            </div>

            {/* Footer */}
            <div className="flex items-center justify-end gap-2 border-t border-border-divider px-5 min-h-[var(--ui-dialog-zone-h)]">
              <button
                onClick={handleCloseDialog}
                disabled={importing}
                className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-100 disabled:opacity-50  dark:hover:bg-zinc-700"
              >
                {t("common.cancel")}
              </button>
              <button
                onClick={handleImport}
                disabled={!selectedFile || importing}
                className="flex items-center gap-2 rounded btn-accent px-3 py-1.5 text-xs font-medium disabled:cursor-not-allowed disabled:opacity-50"
              >
                {importing && <Loader2 className="h-3 w-3 animate-spin" />}
                {importing ? t("skillsPanel.importing") : t("skillsPanel.buttonImport")}
              </button>
            </div>
          </div>
        </div>
      )}
    </>
  );
}
