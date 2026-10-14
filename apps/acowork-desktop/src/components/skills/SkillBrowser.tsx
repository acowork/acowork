import { useState, useEffect, useMemo, useRef, useCallback } from "react";
import { useSkillStore } from "../../stores/skillStore";
import { useAgentStore } from "../../stores/agentStore";
import { SkillDetail } from "./SkillDetail";
import { RefreshCw, Wrench, FolderPlus, X, Loader2 } from "lucide-react";
import { useToast } from "../common/ToastProvider";
import { cn } from "../../lib/utils";
import { StyledInput } from "../common/StyledInput";
import { useTranslation } from "../../i18n/useTranslation";
import { ErrorBox } from "../common/ErrorBox";

export function SkillBrowser() {
  const { t } = useTranslation();
  const { selectedAgentId } = useAgentStore();
  const {
    skills,
    total,
    selectedSkillName,
    selectedSkillDetail,
    loading,
    error,
    fetchSkills,
    selectSkill,
    importSkill,
    clearSkills,
  } = useSkillStore();

  const { addToast } = useToast();
  const [searchQuery, setSearchQuery] = useState("");
  const [importing, setImporting] = useState(false);

  // Import dialog state
  const [importDialogOpen, setImportDialogOpen] = useState(false);
  const [selectedFile, setSelectedFile] = useState<File | null>(null);
  const [importError, setImportError] = useState<string | null>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);

  // Load skills when agent changes
  useEffect(() => {
    if (!selectedAgentId) return;
    clearSkills();
    void fetchSkills(selectedAgentId);
  }, [selectedAgentId, clearSkills, fetchSkills]);

  const handleRefresh = () => {
    if (!selectedAgentId) return;
    void fetchSkills(selectedAgentId);
  };

  const handleImportClick = () => {
    setImportDialogOpen(true);
    setSelectedFile(null);
    setImportError(null);
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

    const result = await importSkill(selectedAgentId, selectedFile);

    setImporting(false);
    if (result.success) {
      addToast({ type: "success", message: result.message || `Skill "${result.skillName}" imported successfully` });
      setImportDialogOpen(false);
      setSelectedFile(null);
    } else {
      setImportError(result.message || "Import failed");
    }
  };

  const handleCloseDialog = () => {
    setImportDialogOpen(false);
    setSelectedFile(null);
    setImportError(null);
  };

  const handleSelectSkill = (skillName: string) => {
    if (!selectedAgentId) return;
    void selectSkill(selectedAgentId, skillName);
  };

  // Frontend filter
  const filteredSkills = useMemo(() => {
    if (!searchQuery.trim()) return skills;
    const q = searchQuery.toLowerCase();
    return skills.filter(
      (s) =>
        s.name.toLowerCase().includes(q) ||
        s.description.toLowerCase().includes(q) ||
        s.triggers.some((t) => t.toLowerCase().includes(q)),
    );
  }, [skills, searchQuery]);

  // ── Empty state: no agent selected ──
  if (!selectedAgentId) {
    return (
      <div className="flex flex-1 items-center justify-center bg-page-bg">
        <div className="text-center">
          <Wrench className="mx-auto h-12 w-12 text-text-secondary " />
          <p className="mt-3 text-sm text-text-tertiary ">
            Select an agent to browse skills
          </p>
        </div>
      </div>
    );
  }

  return (
    <div className="flex flex-1 flex-col bg-page-bg">
      {/* Header */}
      <div className="flex items-center justify-between border-b border-border-divider px-6 py-4">
        <h1 className="text-xl font-semibold">{t("skillsPanel.title")}</h1>
        <div className="flex items-center gap-2">
          <button
            onClick={handleImportClick}
            disabled={importing || loading}
            className="inline-flex items-center gap-1.5 rounded-md border border-zinc-200 px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-50 disabled:opacity-50 dark:border-zinc-700  dark:hover:bg-zinc-800"
          >
            <FolderPlus className={cn("h-3.5 w-3.5", importing && "animate-pulse")} />
            Import
          </button>
          <button
            onClick={handleRefresh}
            disabled={loading}
            className="inline-flex items-center gap-1.5 rounded-md border border-zinc-200 px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-50 disabled:opacity-50 dark:border-zinc-700  dark:hover:bg-zinc-800"
          >
            <RefreshCw className={cn("h-3.5 w-3.5", loading && "animate-spin")} />
            Refresh
          </button>
        </div>
      </div>

      {/* Error banner */}
      {error && (
        <div className="border-b border-red-200 dark:border-red-900">
          <ErrorBox message={error} className="!rounded-none !border-0" />
        </div>
      )}

      {/* Main content: drawer-style master-detail toggle */}
      <div className="flex flex-1 overflow-hidden">
        {selectedSkillName === null ? (
          /* List view */
          <div className="flex flex-1 flex-col overflow-hidden">
            {/* Search */}
            <div className="border-b border-border-divider p-3">
              <StyledInput
                type="text"
                value={searchQuery}
                onChange={(e) => setSearchQuery(e.target.value)}
                placeholder={t("skillsPanel.searchPlaceholder")}
                className=""
              />
            </div>

            {/* Skill list */}
            <div className="flex-1 overflow-y-auto">
              {loading && skills.length === 0 && (
                <div className="flex h-full items-center justify-center">
                  <RefreshCw className="h-5 w-5 animate-spin text-text-tertiary " />
                </div>
              )}

              {!loading && filteredSkills.length === 0 && (
                <div className="flex h-full items-center justify-center text-sm text-text-tertiary ">
                  {searchQuery ? "No matching skills" : "No skills available"}
                </div>
              )}

              <div className="divide-y divide-border-divider">
                {filteredSkills.map((skill) => {
                  const isSelected = skill.name === selectedSkillName;
                  return (
                    <button
                      key={skill.name}
                      onClick={() => handleSelectSkill(skill.name)}
                      className={cn(
                        "flex w-full flex-col gap-1.5 px-4 py-3 text-left transition-colors",
                        isSelected
                          ? "bg-zinc-100 dark:bg-zinc-800"
                          : "hover:bg-zinc-50 dark:hover:bg-zinc-800/50",
                      )}
                    >
                      <span className="text-sm font-medium text-text ">
                        {skill.name}
                      </span>
                      <p className="text-xs text-text-tertiary ">
                        {skill.description}
                      </p>
                      {skill.triggers.length > 0 && (
                        <div className="flex flex-wrap gap-1">
                          {skill.triggers.slice(0, 3).map((t) => (
                            <span
                              key={t}
                              className="rounded px-1.5 py-0.5 text-10 font-medium border" style={{ backgroundColor: "color-mix(in srgb, var(--color-accent) 10%, transparent)", color: "var(--color-accent)", borderColor: "var(--color-accent)" }}
                            >
                              {t}
                            </span>
                          ))}
                          {skill.triggers.length > 3 && (
                            <span className="rounded bg-zinc-100 px-1.5 py-0.5 text-10 text-text-tertiary dark:bg-zinc-800 ">
                              +{skill.triggers.length - 3}
                            </span>
                          )}
                        </div>
                      )}
                    </button>
                  );
                })}
              </div>
            </div>

            {/* List footer */}
            <div className="flex items-center justify-between border-t border-border-divider px-4 py-2 text-xs text-text-tertiary">
              <span>
                {total > 0 ? (
                  <>
                    {filteredSkills.length} of {total}
                  </>
                ) : (
                  "No skills"
                )}
              </span>
            </div>
          </div>
        ) : (
          /* Detail view */
          <div className="flex flex-1 flex-col overflow-hidden">
            <SkillDetail
              detail={selectedSkillDetail}
              loading={loading && selectedSkillName !== null && selectedSkillDetail === null}
              onBack={() => useSkillStore.getState().deselectSkill()}
            />
          </div>
        )}
      </div>

      {/* Import Dialog */}
      {importDialogOpen && (
        <div className="fixed inset-0 z-[200] flex items-center justify-center bg-modal-overlay">
          <div className="w-96 rounded-md border border-border-outer bg-modal-surface p-6 shadow-xl">
            {/* Header */}
            <div className="mb-4 flex items-center justify-between">
              <h3 className="text-sm font-semibold text-text ">
                Import Skill
              </h3>
              <button
                onClick={handleCloseDialog}
                className="rounded p-1 text-text-tertiary hover:bg-zinc-100 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300"
              >
                <X className="h-4 w-4" />
              </button>
            </div>

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

            {/* Error message */}
            {importError && (
              <ErrorBox message={importError} onClose={() => setImportError(null)} />
            )}

            {/* Actions */}
            <div className="flex justify-end gap-2">
              <button
                onClick={handleCloseDialog}
                className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary transition-colors hover:bg-zinc-100  dark:hover:bg-zinc-700"
              >
                Cancel
              </button>
              <button
                onClick={handleImport}
                disabled={!selectedFile || importing}
                className={cn(
                  "inline-flex items-center gap-1.5 rounded-md px-3 py-1.5 text-xs font-medium transition-colors",
                  !selectedFile || importing
                    ? "cursor-not-allowed bg-zinc-200 text-text-tertiary dark:bg-zinc-700 "
                    : "text-white",
                )}
                style={selectedFile && !importing ? { backgroundColor: "var(--color-accent-strong)" } : undefined}
                onMouseEnter={(e) => { if (selectedFile && !importing) e.currentTarget.style.filter = "brightness(0.85)"; }}
                onMouseLeave={(e) => { e.currentTarget.style.filter = ""; }}
              >
                {importing && <Loader2 className="h-3 w-3 animate-spin" />}
                {importing ? t("skillsPanel.importing") : t("skillsPanel.buttonImport")}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
