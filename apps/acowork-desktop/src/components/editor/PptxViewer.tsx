/**
 * PptxViewer — workspace .pptx preview, deliberately DEGRADED to a
 * per-slide card list.
 *
 * True slide fidelity (animations, masters, SmartArt, free layouts)
 * would need a rendering engine; what the preview plan commits to is
 * reading the deck's own text and pictures out of the OOXML package
 * with JSZip and showing one card per slide: page number, every run of
 * slide text (`a:t` nodes), and the images referenced by that slide's
 * relationship file. That is enough to read a deck's content in-app;
 * anything finer goes through "open externally" in the header, which
 * hands the real file to PowerPoint / Keynote.
 *
 * Legacy binary .ppt never reaches this component (the panel routes it
 * to the unsupported card). Image object URLs are revoked when the deck
 * re-parses or the tab unmounts — the slide images can outweigh the
 * pptx itself.
 */
import { useEffect, useRef, useState } from "react";
import { AlertCircle } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { useWorkspaceFileBlob } from "../../hooks/useWorkspaceFileBlob";
import { DocumentViewerFrame, type ViewerFileProps } from "./DocumentViewerFrame";

interface SlideCard {
    page: number;
    texts: string[];
    /** `[name, objectUrl]` pairs already minted for this slide's media. */
    images: Array<[string, string]>;
}

/** `ppt/slides/slide7.xml` → 7. */
function slideNumber(name: string): number | null {
    const m = /^ppt\/slides\/slide(\d+)\.xml$/.exec(name);
    return m ? Number(m[1]) : null;
}

/** Resolve one slide's media targets through its .rels file. */
async function slideMedia(
    zip: import("jszip"),
    slideNum: number,
    makeUrl: (blob: Blob) => string,
): Promise<Array<[string, string]>> {
    const rels = zip.file(`ppt/slides/_rels/slide${slideNum}.xml.rels`);
    if (!rels) return [];
    const xml = await rels.async("text");
    const doc = new DOMParser().parseFromString(xml, "application/xml");
    const out: Array<[string, string]> = [];
    for (const rel of Array.from(doc.getElementsByTagName("Relationship"))) {
        const target = rel.getAttribute("Target") ?? "";
        if (!/\/media\//.test(target)) continue;
        // Targets are relative to ppt/slides/ — `../media/image1.png`.
        const normalized = target.replace(/^\.\.\//, "ppt/").replace(/^\//, "ppt/");
        const file = zip.file(normalized) ?? zip.file(`ppt/${normalized.replace(/^ppt\//, "")}`);
        if (!file) continue;
        const blob = await file.async("blob");
        out.push([normalized.split("/").pop() ?? "image", makeUrl(blob)]);
    }
    return out;
}

export default function PptxViewer(props: ViewerFileProps) {
    const { t } = useTranslation();
    const state = useWorkspaceFileBlob(
        { agentId: props.agentId, workspaceId: props.workspaceId, relPath: props.relPath },
        { mode: "blob", reloadSeq: props.previewSeq },
    );
    const [slides, setSlides] = useState<SlideCard[]>([]);
    const [parseError, setParseError] = useState<string | null>(null);
    const [jumpValue, setJumpValue] = useState("");
    const containerRef = useRef<HTMLDivElement | null>(null);
    // Every object URL this component minted, for teardown — the frame
    // never revokes what it does not own, so the list lives here.
    const createdUrlsRef = useRef<string[]>([]);

    useEffect(() => {
        if (!state.blob) return;
        let cancelled = false;
        setParseError(null);
        const urls: string[] = [];
        const makeUrl = (blob: Blob) => {
            const url = URL.createObjectURL(blob);
            urls.push(url);
            return url;
        };
        (async () => {
            const JSZip = (await import("jszip")).default;
            const zip = await JSZip.loadAsync(state.blob!);
            const slideFiles = Object.keys(zip.files)
                .map((name) => ({ name, num: slideNumber(name) }))
                .filter((x): x is { name: string; num: number } => x.num !== null)
                .sort((a, b) => a.num - b.num);
            const parsed: SlideCard[] = [];
            for (const { name, num } of slideFiles) {
                if (cancelled) return;
                const xml = await zip.file(name)!.async("text");
                const doc = new DOMParser().parseFromString(xml, "application/xml");
                const texts = Array.from(doc.getElementsByTagName("a:t"))
                    .map((node) => node.textContent ?? "")
                    .filter((s) => s.trim().length > 0);
                const images = await slideMedia(zip, num, makeUrl);
                parsed.push({ page: num, texts, images });
            }
            if (cancelled) return;
            // Replace: revoke the PREVIOUS deck's URLs before handing the
            // new list to React (the ref tracks what is currently mounted).
            for (const old of createdUrlsRef.current) URL.revokeObjectURL(old);
            createdUrlsRef.current = urls;
            setSlides(parsed);
        })().catch((e) => {
            if (cancelled) return;
            for (const url of urls) URL.revokeObjectURL(url);
            setParseError(String(e));
        });
        return () => {
            cancelled = true;
        };
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [state.blob, props.previewSeq]);

    // Unmount teardown for the last mounted deck.
    useEffect(
        () => () => {
            for (const url of createdUrlsRef.current) URL.revokeObjectURL(url);
            createdUrlsRef.current = [];
        },
        [],
    );

    const handleJump = () => {
        const page = parseInt(jumpValue, 10);
        if (!Number.isFinite(page) || page < 1) return;
        const root = containerRef.current;
        root?.querySelector(`[data-slide-page="${page}"]`)?.scrollIntoView({
            behavior: "smooth",
            block: "start",
        });
    };

    const body = parseError ? (
        <div className="flex h-full flex-col items-center justify-center gap-2 p-6 text-center text-xs text-text-tertiary">
            <AlertCircle className="h-5 w-5" />
            <span>{t("fileEditor.pptxViewer.parseFailed")}</span>
            <span className="max-w-[80%] truncate font-mono text-10">{parseError}</span>
        </div>
    ) : (
        <div className="flex h-full flex-col">
            <div className="flex shrink-0 items-center gap-2 border-b border-border-divider px-3 py-1.5 text-xs text-text-tertiary">
                <span>{t("fileEditor.pptxViewer.degradedHint")}</span>
                <div className="ml-auto flex items-center gap-1">
                    <input
                        type="text"
                        inputMode="numeric"
                        value={jumpValue}
                        onChange={(e) => setJumpValue(e.target.value)}
                        onKeyDown={(e) => e.key === "Enter" && handleJump()}
                        placeholder={t("fileEditor.pptxViewer.jumpPlaceholder")}
                        className="h-6 w-16 rounded border border-border-default bg-transparent px-1.5 text-xs text-text placeholder:text-text-tertiary"
                        aria-label={t("fileEditor.pptxViewer.jumpPlaceholder")}
                    />
                    <button
                        type="button"
                        onClick={handleJump}
                        className="rounded border border-border-default px-1.5 py-0.5 text-xs hover:bg-hover"
                    >
                        {t("fileEditor.pptxViewer.jump")}
                    </button>
                </div>
            </div>
            <div ref={containerRef} className="min-h-0 flex-1 space-y-3 overflow-auto p-3">
                {slides.map((slide) => (
                    <div
                        key={slide.page}
                        data-slide-page={slide.page}
                        className="rounded-lg border border-border-divider bg-modal-surface p-3 shadow-sm"
                    >
                        <div className="mb-2 text-xs font-medium text-text-tertiary">
                            {t("fileEditor.pptxViewer.page", { page: slide.page })}
                        </div>
                        <div className="select-text space-y-1 text-xs leading-relaxed text-text">
                            {slide.texts.map((text, i) => (
                                <p key={i} className="m-0 whitespace-pre-wrap">
                                    {text}
                                </p>
                            ))}
                        </div>
                        {slide.images.length > 0 && (
                            <div className="mt-2 flex flex-wrap gap-2">
                                {slide.images.map(([name, url]) => (
                                    <img
                                        key={url}
                                        src={url}
                                        alt={name}
                                        className="max-h-48 max-w-full rounded border border-border-divider object-contain"
                                    />
                                ))}
                            </div>
                        )}
                    </div>
                ))}
            </div>
        </div>
    );

    return (
        <DocumentViewerFrame file={props} state={state}>
            {body}
        </DocumentViewerFrame>
    );
}
