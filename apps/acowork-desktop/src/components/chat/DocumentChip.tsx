import { FileText, FileSpreadsheet, Table, X, Loader, Check, AlertCircle } from "lucide-react";
import { cn } from "../../lib/utils";
import { useTranslation } from "../../i18n/useTranslation";
import { AttachmentCapsule, formatAttachmentSize } from "./AttachmentCapsule";

/** Status of a document upload/persistence */
export type DocumentChipStatus = "uploading" | "success" | "error";

/** Props for the DocumentChip component */
export interface DocumentChipProps {
  /** Display filename */
  filename: string;
  /** Document format: pdf, docx, pptx, xlsx */
  format: string;
  /** File size in bytes (optional) */
  size?: number;
  /** Upload/persistence status */
  status?: DocumentChipStatus;
  /** Error message to display (when status is "error") */
  errorMessage?: string;
  /** Callback to remove the chip (when provided, a remove button is shown) */
  onRemove?: () => void;
  /** Additional CSS classes */
  className?: string;
}

/** Map format to display icon */
function formatIcon(format: string) {
  switch (format) {
    case "pdf":
      return <FileText className="h-4 w-4 shrink-0 text-red-500" />;
    case "docx":
    case "pptx":
      return <FileSpreadsheet className="h-4 w-4 shrink-0 text-[var(--color-accent)]" />;
    case "xlsx":
      return <Table className="h-4 w-4 shrink-0 text-green-600" />;
    default:
      return <FileText className="h-4 w-4 shrink-0 text-text-tertiary" />;
  }
}

/** Format bytes to human-readable string — the shared capsule spelling, so a
 *  document chip and a Chat attachment chip state the same weight identically. */
const formatSize = formatAttachmentSize;

export function DocumentChip({
  filename,
  format,
  size,
  status,
  errorMessage,
  onRemove,
  className,
}: DocumentChipProps) {
  const { t } = useTranslation();
  // The frame belongs to `AttachmentCapsule` — this only states what a status
  // does to it, so a document chip and a workspace-ref chip cannot drift in
  // padding, radius, background or font the way the two surfaces once did.
  const statusFrame =
    status === "uploading"
      ? "border-border-divider bg-zinc-50 animate-pulse dark:bg-zinc-800"
      : status === "error"
        ? "border-red-400 bg-red-50 dark:border-red-500 dark:bg-red-900/20"
        : undefined;

  return (
    <AttachmentCapsule
      title={filename}
      pending={status === "uploading"}
      className={cn(statusFrame, className)}
    >
      {/* Format icon */}
      {formatIcon(format)}

      {/* Filename + size */}
      <span className="max-w-[200px] truncate font-medium">{filename}</span>
      {size != null && (
        <span className="text-text-tertiary ">
          {formatSize(size)}
        </span>
      )}

      {/* Status indicator */}
      {status === "uploading" && (
        <Loader className="h-3 w-3 shrink-0 animate-spin text-text-tertiary" />
      )}
      {status === "success" && (
        <Check className="h-3 w-3 shrink-0 text-green-500" />
      )}
      {status === "error" && (
        <>
          <AlertCircle className="h-3 w-3 shrink-0 text-red-500" />
          <span className="text-red-500 dark:text-red-400">
            {errorMessage || t("documentChip.uploadFailed")}
          </span>
        </>
      )}

      {/* Remove button (only when onRemove is provided) */}
      {onRemove && (
        <button
          type="button"
          className="ml-0.5 rounded p-0.5 text-text-tertiary hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300"
          onClick={(e) => {
            e.stopPropagation();
            onRemove();
          }}
          aria-label={t("documentChip.removeFile", { filename })}
        >
          <X className="h-3 w-3" />
        </button>
      )}
    </AttachmentCapsule>
  );
}
