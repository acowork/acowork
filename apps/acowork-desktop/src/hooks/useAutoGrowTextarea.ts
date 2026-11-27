import { useEffect, type RefObject } from "react";

/**
 * Height (border-box px) for a composer field whose content is `contentPx`
 * tall: grow with the text, but never past `maxLines` lines plus the field's
 * own vertical padding — beyond that the field scrolls instead.
 */
export function clampFieldHeight(
  contentPx: number,
  linePx: number,
  paddingPx: number,
  maxLines: number,
): number {
  return Math.min(contentPx, Math.round(linePx * maxLines) + paddingPx);
}

/**
 * Auto-growing composer textarea: one line when empty, one line taller per
 * wrapped line, capped at `maxLines` — past the cap the field keeps its height
 * and `overflow-y-auto` takes over (the overlay scrollbar the rest of the app
 * paints renders from there).
 *
 * Why JS and not `field-sizing: content`: Chromium supports it, the WKWebView
 * macOS ships does not, so the field would silently lose its growth there.
 *
 * ponytail: ceiling — measured on `value` changes only, so changing the app
 * font size (settings slider) re-measures on the next keystroke, not instantly.
 * Upgrade: observe the field with a ResizeObserver on the font-size knob.
 */
export function useAutoGrowTextarea(
  ref: RefObject<HTMLTextAreaElement | null>,
  value: string,
  maxLines = 6,
): void {
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const cs = getComputedStyle(el);
    const fontSize = parseFloat(cs.fontSize) || 14;
    const linePx = parseFloat(cs.lineHeight) || fontSize * 1.5;
    const paddingPx =
      (parseFloat(cs.paddingTop) || 0) + (parseFloat(cs.paddingBottom) || 0);
    // Collapse first so a shrinking draft can shed the previous height.
    el.style.height = "auto";
    // An unmeasurable field (display:none, mid-layout) reports 0 — leave it at
    // its `rows` height rather than pinning it to nothing; the next value
    // change re-measures.
    const contentPx = el.scrollHeight;
    if (contentPx > 0) {
      el.style.height = `${clampFieldHeight(contentPx, linePx, paddingPx, maxLines)}px`;
    }
  }, [ref, value, maxLines]);
}