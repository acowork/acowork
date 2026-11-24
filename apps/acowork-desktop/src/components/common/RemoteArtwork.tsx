/**
 * RemoteArtwork — an `<img>` for artwork fetched over the network.
 *
 * A Chromium `<img>` whose load failed is terminal: `complete` is `true`,
 * `naturalWidth` is 0, and nothing re-requests it while `src` stays the same —
 * not even a re-render. So when the machine wakes and the first paint beats
 * the network (the gallery icons are `https://gallery.acowork.ai/...` URLs;
 * the bundled nav artwork never had this problem), the rail keeps painting the
 * browser's broken-image glyph until the app restarts.
 *
 * Two guards, both per image:
 *   - a failed load renders `fallback` (usually the letter tile) instead of
 *     the broken glyph;
 *   - while failed, the next window focus / visibility return / `online`
 *     event clears the flag.
 *     The `<img>` mounts fresh, which does make the browser request the URL
 *     again — a new element carries no terminal failure state.
 */

import { useEffect, useState, type ReactNode } from "react";

export interface RemoteArtworkProps {
  src: string;
  className?: string;
  /** Rendered when the load failed. `null` = render nothing. */
  fallback: ReactNode;
}

export function RemoteArtwork({ src, className, fallback }: RemoteArtworkProps) {
  // Tracked per URL, not as a bare flag: a new `src` (the catalog republished
  // an icon) must start clean instead of inheriting the old failure.
  const [failedSrc, setFailedSrc] = useState<string | null>(null);
  const failed = failedSrc === src;

  useEffect(() => {
    if (!failed) return;
    const retry = () => setFailedSrc(null);
    const onVisible = () => {
      if (document.visibilityState === "visible") retry();
    };
    window.addEventListener("focus", retry);
    window.addEventListener("online", retry);
    document.addEventListener("visibilitychange", onVisible);
    return () => {
      window.removeEventListener("focus", retry);
      window.removeEventListener("online", retry);
      document.removeEventListener("visibilitychange", onVisible);
    };
  }, [failed]);

  if (failed) return <>{fallback}</>;

  return (
    <img
      src={src}
      alt=""
      draggable={false}
      className={className}
      onError={() => setFailedSrc(src)}
    />
  );
}