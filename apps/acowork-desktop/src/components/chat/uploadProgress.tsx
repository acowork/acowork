import { useCallback, useRef, useState } from "react";
import { DocumentChip } from "./DocumentChip";

/** One file whose bytes are still moving. */
export interface UploadingFile {
  /** React key. Two same-named files picked together must not share a chip. */
  key: string;
  filename: string;
  sizeBytes: number;
  /** Bytes the server has accepted so far. */
  sentBytes?: number;
}

/**
 * In-flight uploads for a composer's attachment tray.
 *
 * Every attachment surface uploads through `chatSessionStore.upload`, which
 * reports accepted bytes (`onProgress`) — but a surface that forgets to wire
 * that up shows a pulsing spinner for the whole transfer, which on a 300 MB
 * file is a multi-minute "is it hung?" stare. That is what each surface used
 * to be, in three near-identical copies, so the wiring lives here instead:
 * `begin(file)` reserves the chip and hands back the callback and signal,
 * `done()` drops it. A surface cannot report nothing by accident any more.
 *
 * `cancel(key)` aborts that file's transfer; the chip goes away through the
 * surface's own `done()` in the same `finally` that handles a failure, so a
 * cancelled upload and a broken one leave the tray the same.
 */
export function useUploadingFiles() {
  const [uploadingFiles, setUploadingFiles] = useState<UploadingFile[]>([]);
  const controllers = useRef(new Map<string, AbortController>());

  const begin = useCallback((file: File) => {
    const key = `up-${Date.now()}-${Math.random().toString(36).slice(2)}`;
    const controller = new AbortController();
    controllers.current.set(key, controller);
    setUploadingFiles((prev) => [
      ...prev,
      { key, filename: file.name, sizeBytes: file.size },
    ]);
    return {
      signal: controller.signal,
      onProgress: (sent: number) =>
        setUploadingFiles((prev) =>
          prev.map((u) => (u.key === key ? { ...u, sentBytes: sent } : u)),
        ),
      done: () => {
        controllers.current.delete(key);
        setUploadingFiles((prev) => prev.filter((u) => u.key !== key));
      },
    };
  }, []);

  const cancel = useCallback((key: string) => {
    controllers.current.get(key)?.abort();
  }, []);

  return { uploadingFiles, begin, cancel };
}

/** The tray chips for [`useUploadingFiles`] — `sent / total` per file, and an
 *  X that stops the transfer (`cancel` from the same hook). */
export function UploadingFileChips({
  files,
  onCancel,
}: {
  files: UploadingFile[];
  onCancel: (key: string) => void;
}) {
  return (
    <>
      {files.map((u) => (
        <DocumentChip
          key={u.key}
          filename={u.filename}
          format={u.filename.split(".").pop()?.toLowerCase() ?? ""}
          size={u.sizeBytes}
          sentBytes={u.sentBytes}
          status="uploading"
          onRemove={() => onCancel(u.key)}
        />
      ))}
    </>
  );
}