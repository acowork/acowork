import { useEffect, useMemo, useRef, useLayoutEffect, useState } from "react";
import Panzoom, { PanzoomObject } from "@panzoom/panzoom";
import { ZoomIn, ZoomOut, Maximize2 } from "lucide-react";
import { log } from "../../lib/logger";
import { hashStr, isPlausibleMermaid, renderMermaid } from "../../lib/mermaid";
import { tokensFor } from "../../lib/mermaidTheme";
import { useSettingsStore } from "../../stores/settingsStore";

const SCALE_MIN = 0.2;
const SCALE_MAX = 8;
const SCALE_STEP = 1.2;

/** Fit-to-container never scales the diagram UP beyond its rendered size.
 *  Why: vertical flowcharts have a narrow viewBox (e.g. 100×800) so
 *  `containerW / renderedW` evaluates to 6× — the diagram would be
 *  blown up to 6× its intended size, including text. Capping at 1 means
 *  wide charts shrink to fit; narrow charts display at 1:1 (still
 *  inside the container, aligned to the left/top). */
const FIT_MAX = 1;

interface MermaidBlockProps {
  chart: string;
}

export function MermaidBlock({ chart }: MermaidBlockProps) {
  const [svgContent, setSvgContent] = useState<string | null>(null);
  const [renderFailed, setRenderFailed] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);
  /** Content area wrapper — has overflow:hidden + bg-chat-body.
   *  Its height is set to match the SVG's aspect ratio scaled to fit width.
   *  Panzoom target lives inside this, so panned content is clipped here. */
  const contentAreaRef = useRef<HTMLDivElement>(null);
  /** The div that wraps the SVG. Panzoom is bound to this. */
  const panzoomTargetRef = useRef<HTMLDivElement>(null);
  const lastLoggedFailRef = useRef<number | null>(null);

  // ---- Theme-driven palette ----
  // Mermaid bakes colors into the SVG at render time, so a diagram keeps
  // whatever palette it was drawn with. Resolving the palette here (rather
  // than at module load) is what makes a live theme switch repaint the
  // chart instead of leaving light-mode edges on a dark background.
  const theme = useSettingsStore((s) => s.theme);
  const osTheme = useSettingsStore((s) => s.osTheme);
  const tokens = useMemo(() => tokensFor(theme, osTheme), [theme, osTheme]);

  // ---- Pan & zoom ----
  const panzoomRef = useRef<PanzoomObject | null>(null);
  const svgNaturalSizeRef = useRef<{ width: number; height: number } | null>(null);
  const containerWidthRef = useRef(0);
  const [transformVersion, setTransformVersion] = useState(0);
  const [currentScale, setCurrentScale] = useState(1);
  /** The height of the content area (px), set to naturalH × fit scale
   *  (see applyFit). NOT an aspect-ratio projection — that's only
   *  correct when the diagram actually fills the container width. */
  const [contentHeight, setContentHeight] = useState<number | null>(null);

  const clampScale = (s: number) =>
    Math.max(SCALE_MIN, Math.min(SCALE_MAX, s));

  /** Compute fit scale + matching contentHeight from current refs, and
   *  apply both to panzoom + React state. This is the SINGLE source of
   *  truth for "fit the diagram to the container" — used by the
   *  initial layout effect, the ResizeObserver, and the reset button,
   *  so all three paths produce identical visuals (no empty space
   *  below the diagram, no surprise scale change on reset).
   *
   *  With origin: '0 0', a scaled element stays at its top-left corner.
   *  We compute panX to center the element horizontally inside the
   *  container: panX = (containerW - naturalW * fit) / 2.
   *  For wide charts (naturalW > container) this is ≈ 0 (no shift);
   *  for narrow charts (vertical flowcharts) it pushes the element
   *  to the visual center of the content area. */
  const applyFit = () => {
    const pz = panzoomRef.current;
    const size = svgNaturalSizeRef.current;
    const w = containerWidthRef.current;
    if (!pz || !size || !size.width || w <= 0) return;
    // Never scale UP (FIT_MAX=1). Wide diagrams shrink to fit;
    // narrow (vertical) diagrams stay at 1:1.
    const fit = Math.max(SCALE_MIN, Math.min(FIT_MAX, w / size.width));
    // [FIX] contentHeight must be the *actually rendered* height
    // (= naturalH * scale), NOT the aspect-ratio-projected height
    // (= naturalH * w / naturalW). The two are equal only when the
    // diagram actually fills the container width; for vertical
    // charts (fit=1) the projection over-estimates by 5–8×, leaving
    // a large empty area below the diagram.
    setContentHeight(size.height * fit);
    // Center horizontally: panX shifts the element right so its
    // scaled visual center aligns with the container center.
    const panX = (w - size.width * fit) / 2;
    pz.zoom(fit, { animate: false, force: true });
    pz.pan(panX, 0, { animate: false, force: true });
    setCurrentScale(pz.getScale());
  };

  const zoomIn = () => {
    const pz = panzoomRef.current;
    if (!pz) return;
    pz.zoom(clampScale(pz.getScale() * SCALE_STEP), { animate: false });
  };
  const zoomOut = () => {
    const pz = panzoomRef.current;
    if (!pz) return;
    pz.zoom(clampScale(pz.getScale() / SCALE_STEP), { animate: false });
  };
  const resetZoom = () => {
    applyFit();
  };

  // Debounced mermaid render
  useEffect(() => {
    // Declared OUTSIDE the timer callback so cleanup can actually flip it.
    // Theme is a dependency (see below), so a theme switch re-runs this
    // effect; without this, an in-flight render from the previous theme
    // would resolve after the new one and overwrite it with stale colors.
    let cancelled = false;
    const timer = setTimeout(() => {
      if (!isPlausibleMermaid(chart)) return;
      const failHash = hashStr(chart);
      (async () => {
        try {
          const svg = await renderMermaid(chart, tokens);
          if (!cancelled) {
            setSvgContent(svg);
            setRenderFailed(false);
          }
        } catch (err) {
          if (lastLoggedFailRef.current !== failHash) {
            log.error("[MermaidBlock] render failed:", err);
            log.error("[MermaidBlock] chart content:", chart.slice(0, 500));
            lastLoggedFailRef.current = failHash;
          }
          if (!cancelled) {
            setSvgContent(null);
            setRenderFailed(true);
          }
        }
      })();
    }, 200);
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
    // `tokens` is memoized on (theme, osTheme), so this re-runs on theme
    // switch and nothing else — mermaid bakes colors into the SVG at render
    // time, so a re-render is the only way to repaint a live diagram.
  }, [chart, tokens]);

  // Measure SVG, set content area height, create panzoom, fit to container
  useLayoutEffect(() => {
    if (!containerRef.current || !contentAreaRef.current || !panzoomTargetRef.current || !svgContent) {
      setContentHeight(null);
      return;
    }

    const target = panzoomTargetRef.current;
    const contentArea = contentAreaRef.current;
    const wrap = containerRef.current;

    // Fix the SVG: make it a block element with explicit dimensions
    // so it sits properly at (0,0) inside the panzoom target div.
    const svg = target.querySelector("svg") as SVGSVGElement | null;
    if (!svg) return;

    // [FIX] Read the SVG's *rendered* pixel size (what mermaid itself
    // produces when useMaxWidth: true), not the viewBox. viewBox width
    // is the logical minimum column width — for a vertical flowchart
    // (e.g. 100×800) using viewBox.width as the "natural width" makes
    // the fit-scale formula amplify the diagram 5–8×. The rendered
    // size is what the user actually sees and what mermaid's own
    // fit-to-container logic uses internally.
    const rect = svg.getBoundingClientRect();
    let naturalW = rect.width;
    let naturalH = rect.height;
    if (!Number.isFinite(naturalW) || !Number.isFinite(naturalH) || naturalW <= 0 || naturalH <= 0) {
      // Fallback: parse width/height attrs (some mermaid versions)
      naturalW = parseFloat(svg.getAttribute("width") || "");
      naturalH = parseFloat(svg.getAttribute("height") || "");
      if (!Number.isFinite(naturalW) || !Number.isFinite(naturalH)) return;
    }

    // Block layout eliminates inline vertical-align gaps
    svg.style.display = "block";

    // Make the panzoom target div match the SVG's rendered size so
    // positioning math inside panzoom is correct.
    target.style.width = `${naturalW}px`;

    svgNaturalSizeRef.current = { width: naturalW, height: naturalH };
    const containerW = wrap.clientWidth;
    containerWidthRef.current = containerW;

    // Compute fit and centering offset BEFORE creating Panzoom, then
    // pass them as startScale / startX so the constructor's internal
    // rAF writes the correct CSS transform from the very first frame.
    // This avoids the rAF race in setTransformWithEvent (L155) that
    // occurs when zoom()/pan() are called separately — the rAFs from
    // those calls may land in different frames, causing a flash of
    // left-aligned content before the centering panX takes effect.
    const fit = Math.max(SCALE_MIN, Math.min(FIT_MAX, containerW / naturalW));
    const panX = (containerW - naturalW * fit) / 2;
    setContentHeight(naturalH * fit);
    setCurrentScale(fit);

    // Panzoom 4.x sets transform-origin to 50% 50% for non-SVG
    // elements (DIVs) by default. This causes scaled-down elements
    // (fit < 1) to visually shift toward their center — the top-left
    // corner moves right and down, which looks like "bottom-right
    // alignment". We use origin: '0 0' so the top-left corner stays
    // fixed; centering is handled manually via startX (panX).
    const pz = Panzoom(target, {
      animate: false,
      cursor: "grab",
      maxScale: SCALE_MAX,
      minScale: SCALE_MIN,
      startScale: fit,
      startX: panX,
      startY: 0,
      origin: "0 0",
    });
    panzoomRef.current = pz;

    // Sync React state on pan/zoom changes
    const onPanzoomChange = (e: Event) => {
      const s = (e as CustomEvent<{ scale: number }>).detail.scale;
      setCurrentScale((prev) => (Math.abs(prev - s) < 1e-3 ? prev : s));
      setTransformVersion((v) => v + 1);
    };
    target.addEventListener("panzoomchange", onPanzoomChange);

    const onPanzoomStart = () => { target.style.cursor = "grabbing"; };
    const onPanzoomEnd = () => { target.style.cursor = "grab"; };
    target.addEventListener("panzoomstart", onPanzoomStart);
    target.addEventListener("panzoomend", onPanzoomEnd);

    // Crisp text under non-integer scales
    svg.querySelectorAll("text").forEach((t) => {
      t.setAttribute("text-rendering", "geometricPrecision");
    });

    // Custom wheel zoom — only when Ctrl/Cmd is held (like browser zoom),
    // so normal wheel scrolls the page instead of zooming the diagram.
    const WHEEL_DENOMINATOR = 100;
    const onWheel = (e: WheelEvent) => {
      if (!e.ctrlKey && !e.metaKey) return;
      e.preventDefault();
      const oldScale = pz.getScale();
      const factor = Math.pow(SCALE_STEP, -e.deltaY / WHEEL_DENOMINATOR);
      const newScale = clampScale(oldScale * factor);
      if (newScale === oldScale) return;
      pz.zoomToPoint(newScale, e, { animate: false });
    };
    contentArea.addEventListener("wheel", onWheel, { passive: false });

    // Resize observer: update container width and re-fit. Same path as
    // initial layout and reset button — keeps diagram + container
    // height consistent across all re-fit triggers.
    const ro = new ResizeObserver((entries) => {
      for (const entry of entries) {
        containerWidthRef.current = entry.contentRect.width;
        applyFit();
      }
    });
    ro.observe(wrap);

    return () => {
      contentArea.removeEventListener("wheel", onWheel);
      target.removeEventListener("panzoomchange", onPanzoomChange);
      target.removeEventListener("panzoomstart", onPanzoomStart);
      target.removeEventListener("panzoomend", onPanzoomEnd);
      ro.disconnect();
      pz.destroy();
      panzoomRef.current = null;
    };
  }, [svgContent]);

  return (
    <div
      ref={containerRef}
      className="my-2 w-full rounded-md border border-chat-border"
    >
      {/* Title bar — matches CodeBlock pattern */}
      <div className="flex items-center justify-between border-b border-chat-border bg-chat-title px-3 py-1.5">
        <div className="flex items-center gap-1.5">
          <span className="text-xs font-medium text-text-tertiary ">
            mermaid
          </span>
        </div>
        <div className="flex items-center gap-1.5">
          <button
            type="button"
            onClick={zoomOut}
            disabled={currentScale <= SCALE_MIN + 1e-6}
            aria-label="缩小"
            title="缩小"
            data-version={transformVersion}
            className="flex items-center justify-center rounded p-0.5 text-text-tertiary hover:text-zinc-700 hover:bg-zinc-200  dark:hover:text-zinc-200 dark:hover:bg-zinc-700 disabled:opacity-40 disabled:cursor-not-allowed"
          >
            <ZoomOut className="h-3.5 w-3.5" />
          </button>
          <button
            type="button"
            onClick={resetZoom}
            aria-label="重置缩放（撑满显示区域）"
            title="重置缩放（撑满显示区域）"
            data-version={transformVersion}
            className="flex items-center justify-center rounded p-0.5 text-text-tertiary hover:text-zinc-700 hover:bg-zinc-200  dark:hover:text-zinc-200 dark:hover:bg-zinc-700"
          >
            <Maximize2 className="h-3.5 w-3.5" />
          </button>
          <button
            type="button"
            onClick={zoomIn}
            disabled={currentScale >= SCALE_MAX - 1e-6}
            aria-label="放大"
            title="放大"
            data-version={transformVersion}
            className="flex items-center justify-center rounded p-0.5 text-text-tertiary hover:text-zinc-700 hover:bg-zinc-200  dark:hover:text-zinc-200 dark:hover:bg-zinc-700 disabled:opacity-40 disabled:cursor-not-allowed"
          >
            <ZoomIn className="h-3.5 w-3.5" />
          </button>
        </div>
      </div>

      {/* Content area — clips panned SVG content, never overlaps title bar */}
      {svgContent ? (
        <div
          ref={contentAreaRef}
          className="relative overflow-hidden bg-chat-body"
          style={{ height: contentHeight ?? undefined }}
        >
          <div
            ref={panzoomTargetRef}
            dangerouslySetInnerHTML={{ __html: svgContent }}
          />
        </div>
      ) : renderFailed ? (
        <pre className="m-0 whitespace-pre-wrap bg-chat-body p-3 font-mono text-xs leading-relaxed text-text-tertiary ">
          {chart}
        </pre>
      ) : (
        <div className="flex min-h-[140px] items-center justify-center bg-chat-body">
          <div className="flex items-center gap-2 text-text-secondary select-none ">
            <svg
              className="h-4 w-4 animate-spin"
              xmlns="http://www.w3.org/2000/svg"
              fill="none"
              viewBox="0 0 24 24"
            >
              <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
              <path
                className="opacity-75"
                fill="currentColor"
                d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z"
              />
            </svg>
            <span className="text-xs">Rendering diagram...</span>
          </div>
        </div>
      )}
    </div>
  );
}