/**
 * Image lightbox for chat attachments.
 *
 * Why a bridge and not a prop: the click that opens this lives in
 * `lib/openChatAttachment.ts`, a plain module — it must stay callable from a
 * row component, a store action, or a future surface without every caller
 * having to thread state down. Same pattern as `showToast` (ADR-038): fire a
 * `CustomEvent`, and one mounted host renders the overlay.
 *
 * Why one host and not per-chip state: chat rows live inside a virtualized
 * list, so an overlay rendered inside a row would be clipped by the scroll
 * container and unmounted the moment the row leaves the viewport. The host
 * sits above the app shell.
 *
 * Scope: display + "save to disk" only. It never fetches anything itself —
 * the caller resolves the blob URL (the chip already knows how) and hands it
 * over, so this component has no auth or Gateway knowledge. Because the bytes
 * are the caller's, so is their lifetime: `onClose` is the release hook, and
 * the host calls it exactly once per request, whether the overlay was
 * dismissed, replaced by another image, or unmounted.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import type { KeyboardEvent as ReactKeyboardEvent } from "react";
import { Download, X } from "lucide-react";

export interface ImageLightboxRequest {
  /** Object URL or data URL for the full-size image. */
  src: string;
  /** Shown in the header and used as the alt text. */
  filename: string;
  /** Optional save action; the button is hidden when absent. */
  onDownload?: () => void;
  /**
   * Release hook for whatever `src` holds — a caller-owned object URL is the
   * case this exists for. Called once, when this request stops being shown;
   * omitted when the URL is shared (a cached thumbnail, a data URL).
   */
  onClose?: () => void;
}

const LIGHTBOX_EVENT = "acowork:image-lightbox";

/** Open the lightbox from anywhere (no React context required). */
export function openImageLightbox(request: ImageLightboxRequest): void {
  window.dispatchEvent(new CustomEvent(LIGHTBOX_EVENT, { detail: request }));
}

/** Everything inside the overlay the keyboard can reach. */
const FOCUSABLE = 'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])';

export function ImageLightboxHost() {
  const [request, setRequest] = useState<ImageLightboxRequest | null>(null);
  /** The request currently on screen. Held outside state so the release hook
   *  fires exactly once per request even if React renders twice. */
  const current = useRef<ImageLightboxRequest | null>(null);
  const dialogRef = useRef<HTMLDivElement | null>(null);
  /** Where focus was when the overlay opened — a modal that does not give it
   *  back strands the keyboard at the top of the document. */
  const restoreFocus = useRef<HTMLElement | null>(null);

  useEffect(() => {
    const handler = (e: Event) => {
      const detail = (e as CustomEvent<ImageLightboxRequest>).detail;
      if (!detail?.src) return;
      const previous = current.current;
      if (!previous) restoreFocus.current = document.activeElement as HTMLElement | null;
      current.current = detail;
      setRequest(detail);
      // A second image replaces the first: the first one's bytes are no
      // longer on screen, so this is where its object URL goes back.
      previous?.onClose?.();
    };
    window.addEventListener(LIGHTBOX_EVENT, handler);
    return () => window.removeEventListener(LIGHTBOX_EVENT, handler);
  }, []);

  const close = useCallback(() => {
    const live = current.current;
    current.current = null;
    setRequest(null);
    live?.onClose?.();
    const back = restoreFocus.current;
    restoreFocus.current = null;
    back?.focus?.();
  }, []);

  // Move focus into the overlay for as long as it is up.
  useEffect(() => {
    if (!request) return;
    dialogRef.current?.focus();
  }, [request]);

  useEffect(() => {
    if (!request) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") close();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [request, close]);

  if (!request) return null;

  /** Keep Tab inside the overlay: `aria-modal` alone does not. */
  const trapTab = (e: ReactKeyboardEvent<HTMLDivElement>) => {
    if (e.key !== "Tab") return;
    const root = dialogRef.current;
    if (!root) return;
    const focusable = Array.from(root.querySelectorAll<HTMLElement>(FOCUSABLE));
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (!first || !last) return;
    const active = document.activeElement;
    if (e.shiftKey && (active === first || active === root)) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && active === last) {
      e.preventDefault();
      first.focus();
    }
  };

  return (
    <div
      ref={dialogRef}
      tabIndex={-1}
      className="fixed inset-0 z-[200] flex flex-col bg-black/80 backdrop-blur-sm outline-none"
      role="dialog"
      aria-modal="true"
      aria-label={request.filename}
      onClick={close}
      onKeyDown={trapTab}
    >
      <div
        className="flex shrink-0 items-center gap-3 px-4 py-3 text-white"
        onClick={(e) => e.stopPropagation()}
      >
        <span className="min-w-0 flex-1 truncate text-sm" title={request.filename}>
          {request.filename}
        </span>
        {request.onDownload && (
          <button
            type="button"
            className="inline-flex items-center gap-1.5 rounded border border-white/30 px-2.5 py-1 text-xs text-white transition-colors hover:bg-white/10"
            onClick={request.onDownload}
          >
            <Download className="h-3.5 w-3.5" />
            <span>保存</span>
          </button>
        )}
        <button
          type="button"
          aria-label="关闭"
          className="inline-flex h-7 w-7 items-center justify-center rounded text-white transition-colors hover:bg-white/10"
          onClick={close}
        >
          <X className="h-4 w-4" />
        </button>
      </div>
      <div className="flex min-h-0 flex-1 items-center justify-center p-4">
        <img
          src={request.src}
          alt={request.filename}
          className="max-h-full max-w-full object-contain"
          onClick={(e) => e.stopPropagation()}
        />
      </div>
    </div>
  );
}