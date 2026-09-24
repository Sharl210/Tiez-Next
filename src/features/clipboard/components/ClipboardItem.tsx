import { useRef, useEffect, useLayoutEffect, useState, useMemo, memo } from "react";
import { createPortal } from "react-dom";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import type { WebviewWindow } from "@tauri-apps/api/webviewWindow";
import { currentMonitor, getCurrentWindow, PhysicalPosition, PhysicalSize } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import {
    Pin,
    PinOff,
    Eye,
    EyeOff,
    ExternalLink,
    Tag,
    X,
    FileText,
    Image as ImageIcon,
    Link as LinkIcon,
    Code,
    File,
    Plus,
    Video,
    Sparkles,
    Loader2,
    FileArchive,
    Music,
    FileCode,
    Cpu,
    Files,
    ImageOff,
    FileQuestion,
    GripVertical,
    Pencil,
    FolderInput
} from "lucide-react";
import { motion, AnimatePresence } from "framer-motion";
import type { ClipboardItemProps } from "../types";
import { TagAssignMenu } from "./TagAssignMenu";
import { getEntryNote, isNoteEditable, MAX_ENTRY_NOTE_CHARS } from "../types";
import {
    formatSensitivePreview,
    getConciseTime,
    getTagColor,
    getTagTextColor
} from "../../../shared/lib/utils";
import HtmlContent, { sanitizeHTML } from "../../../shared/components/HtmlContent";
import { toTauriLocalImageSrc } from "../../../shared/lib/localImageSrc";
import { getRichTextSnapshotDataUrl } from "../../../shared/lib/richTextSnapshot";
import { getFileIcon as getSystemFileIcon, peekFileIcon } from "../../../shared/lib/fileIcon";
import { getSourceAppIcon, peekSourceAppIcon } from "../../../shared/lib/sourceAppIcon";
import { registerCompactPreviewControls } from "../lib/compactPreviewControls";
import { TAG_SUGGEST_VISIBLE_ROWS } from "../constants";
import { selectTagSuggestions } from "../lib/tagSuggestions";

const COMPACT_PREVIEW_LABEL = "compact-preview";

/**
 * R6: how much of an entry note is shown inline before it is cut off. The full text
 * stays reachable through the `title` tooltip, so a long note cannot break the row
 * layout while still being readable.
 */
const NOTE_INLINE_MAX_CHARS = 120;
const truncateNoteForInline = (note: string): string => {
    const trimmed = note.trim();
    const chars = Array.from(trimmed);
    if (chars.length <= NOTE_INLINE_MAX_CHARS) return trimmed;
    return chars.slice(0, NOTE_INLINE_MAX_CHARS).join("") + "…";
};
/**
 * R13: 把一个 `rich_text` 条目**没有** HTML 时的兜底：把纯文本按行转义成 HTML。
 *
 * `rich_text` 行的 `html_content` 理论上非空，但历史数据（以及导入的旧备份）里存在
 * 只有 `content` 的行。此时把纯文本直接塞进 contentEditable 会让文本里的 `<` 被当成
 * 标签吃掉，所以转义后再写。刻意不在这里加 `<p>` 包裹 —— 编辑器只是展示这份内容，
 * 真正的写入以用户编辑后的 `innerHTML` 为准。
 */
/**
 * R13：从富文本 HTML 里取出**纯文本正文**。
 *
 * `content` 列是派生的纯文本（粘贴与列表预览用的就是它）。富文本编辑器改的是 HTML，
 * 保存时若把 `innerHTML` 当正文送去，`content` 里就会存下 `<p>…</p>` —— 用户复制出来
 * 会看到 HTML 源码。后端以同一口径再派生一次作为权威值，这里派生是为了让界面上显示的
 * 正文与最终落库的内容一致。
 */
const htmlToPlainText = (html: string): string => {
    if (!html) return "";
    const doc = new DOMParser().parseFromString(html, "text/html");
    doc.querySelectorAll("br").forEach((br) => br.replaceWith("\n"));
    doc.querySelectorAll("p, div, li, tr, h1, h2, h3, h4, h5, h6, blockquote, pre")
        .forEach((el) => el.append("\n"));
    return (doc.body.textContent ?? "").replace(/\n{3,}/g, "\n\n").trim();
};

const escapeHtmlForEditor = (text: string): string =>
    text
        .replace(/&/g, "&amp;")
        .replace(/</g, "&lt;")
        .replace(/>/g, "&gt;")
        .replace(/\n/g, "<br>");

const RICH_IMAGE_FALLBACK_PREFIX = "<!--TIEZ_RICH_IMAGE:";
const RICH_IMAGE_FALLBACK_SUFFIX = "-->";
const TABULAR_RICH_HTML_RE = /<(table|tr|td|th|thead|tbody|tfoot|colgroup|col)\b/i;
const SPREADSHEET_SOURCE_RE = /\b(excel|et|wps|sheet|spreadsheet|calc)\b/i;
const SPREADSHEET_APP_RE = /(?:^|[\\/])(excel|et|wps|wpssheet|soffice)(?:\.exe|\.app)?$/i;
const STANDALONE_COLOR_RE = /^(#(?:[0-9a-f]{3}|[0-9a-f]{4}|[0-9a-f]{6}|[0-9a-f]{8})|(?:rgb|hsl)a?\(\s*[^)]+\s*\))$/i;
const COMPACT_PREVIEW_DEBUG = false;
const IS_MACOS =
    typeof navigator !== "undefined" &&
    (/Mac|iPhone|iPad|iPod/i.test(navigator.userAgent) || /Mac/i.test(navigator.platform));
const COMPACT_PREVIEW_WINDOW_SUPPORTED = true;
const COMPACT_PREVIEW_WARMUP_SUPPORTED = !IS_MACOS;
const compactPreviewLog = (...args: unknown[]) => {
    if (!COMPACT_PREVIEW_DEBUG) return;
    const ts = new Date().toISOString();
    console.log(`[CompactPreview][Main][${ts}]`, ...args);
};
const richPreviewFailureLog = (stage: string, detail?: Record<string, unknown>) => {
    console.warn("[RichTextPreview][MainList]", stage, detail || {});
};
type CompactPreviewAnchor = {
    clientX: number;
    clientY: number;
    screenX: number;
    screenY: number;
};

const extractRichImageFallback = (html?: string): { cleanHtml?: string; imagePayload?: string } => {
    if (!html) return {};
    const start = html.lastIndexOf(RICH_IMAGE_FALLBACK_PREFIX);
    if (start < 0) return { cleanHtml: html };

    const markerStart = start + RICH_IMAGE_FALLBACK_PREFIX.length;
    const endRel = html.slice(markerStart).indexOf(RICH_IMAGE_FALLBACK_SUFFIX);
    if (endRel < 0) return { cleanHtml: html };

    const markerEnd = markerStart + endRel;
    const payload = html.slice(markerStart, markerEnd).trim();
    const cleanHtml = `${html.slice(0, start)}${html.slice(markerEnd + RICH_IMAGE_FALLBACK_SUFFIX.length)}`.trim();
    return {
        cleanHtml: cleanHtml || html,
        imagePayload: payload || undefined
    };
};

const resolveRichImageSrc = (payload?: string): string | null => {
    if (!payload) return null;
    const value = payload.trim();
    if (!value) return null;
    if (value.startsWith("data:image/")) return value;
    if (/^https?:\/\/asset\.localhost\//i.test(value)) return value;
    return toTauriLocalImageSrc(value);
};

const isAnimatedGifSrc = (src?: string | null): boolean => {
    const value = (src || "").trim().toLowerCase();
    if (!value) return false;
    return value.startsWith("data:image/gif") || /\.gif(?:$|[?#])/i.test(value);
};

const richHtmlLooksTabular = (html?: string): boolean => {
    if (!html) return false;
    return TABULAR_RICH_HTML_RE.test(html);
};

const isSpreadsheetLikeSource = (...candidates: Array<string | undefined>): boolean => {
    return candidates.some((candidate) => {
        const value = (candidate || "").trim();
        if (!value) return false;
        return SPREADSHEET_APP_RE.test(value) || SPREADSHEET_SOURCE_RE.test(value);
    });
};

const getStandaloneColorValue = (contentType: string, content: string): string | null => {
    if (contentType !== "text" && contentType !== "code") {
        return null;
    }

    const normalized = content.trim();
    if (!normalized || normalized.includes("\n")) {
        return null;
    }

    return STANDALONE_COLOR_RE.test(normalized) ? normalized : null;
};

let compactPreviewWindow: WebviewWindow | null = null;
let compactPreviewCreating = false;
let compactPreviewReady: Promise<WebviewWindow | null> | null = null;
let compactPreviewMounted = false;
let compactPreviewMountedPromise: Promise<boolean> | null = null;
let compactPreviewResizeListener: Promise<() => void> | null = null;
let compactPreviewPendingShow = false;
let compactPreviewPendingAnchor: CompactPreviewAnchor | null = null;
let compactPreviewPendingTimer: ReturnType<typeof setTimeout> | null = null;
let compactPreviewLifecycleListenersReady: Promise<void> | null = null;

const loadWebviewWindowModule = async () => import("@tauri-apps/api/webviewWindow");

const setIgnoreBlurSafe = (ignore: boolean) => {
    compactPreviewLog("set_ignore_blur", { ignore });
    invoke("set_ignore_blur", { ignore }).catch(() => { });
};

const clearCompactPreviewPendingState = () => {
    compactPreviewLog("clear pending state");
    if (compactPreviewPendingTimer) {
        clearTimeout(compactPreviewPendingTimer);
        compactPreviewPendingTimer = null;
    }
    compactPreviewPendingShow = false;
    compactPreviewPendingAnchor = null;
};

const resolveAnchorPhysical = async (
    anchor: CompactPreviewAnchor,
    scale: number
): Promise<{ x: number; y: number }> => {
    try {
        const appWindow = getCurrentWindow();
        const outer = await appWindow.outerPosition();
        return {
            x: Math.round(outer.x + anchor.clientX * scale),
            y: Math.round(outer.y + anchor.clientY * scale)
        };
    } catch {
        return {
            x: Math.round(anchor.screenX * scale),
            y: Math.round(anchor.screenY * scale)
        };
    }
};

const pickPreviewPosition = (
    anchorX: number,
    anchorY: number,
    widthPx: number,
    heightPx: number,
    monitorPos: { x: number; y: number },
    monitorSize: { width: number; height: number },
    margin: number,
    offset: number,
    avoidRect?: { left: number; top: number; right: number; bottom: number } | null
) => {
    const left = monitorPos.x + margin;
    const top = monitorPos.y + margin;
    const right = monitorPos.x + monitorSize.width - margin;
    const bottom = monitorPos.y + monitorSize.height - margin;

    const clampPoint = (p: { x: number; y: number }) => ({
        x: Math.min(Math.max(p.x, left), right - widthPx),
        y: Math.min(Math.max(p.y, top), bottom - heightPx)
    });

    const intersectsAvoidRect = (p: { x: number; y: number }) => {
        if (!avoidRect) return false;
        const previewRect = {
            left: p.x,
            top: p.y,
            right: p.x + widthPx,
            bottom: p.y + heightPx
        };
        return !(
            previewRect.right <= avoidRect.left ||
            previewRect.left >= avoidRect.right ||
            previewRect.bottom <= avoidRect.top ||
            previewRect.top >= avoidRect.bottom
        );
    };

    const candidates = [
        { x: anchorX + offset, y: anchorY + offset }, // right-bottom
        { x: anchorX + offset, y: anchorY - heightPx - offset }, // right-top
        { x: anchorX - widthPx - offset, y: anchorY + offset }, // left-bottom
        { x: anchorX - widthPx - offset, y: anchorY - heightPx - offset } // left-top
    ];

    const fits = (p: { x: number; y: number }) =>
        p.x >= left && p.y >= top && p.x + widthPx <= right && p.y + heightPx <= bottom;

    for (const c of candidates) {
        if (fits(c) && !intersectsAvoidRect(c)) return c;
    }

    if (avoidRect) {
        const outsideCandidates = [
            { x: avoidRect.right + offset, y: anchorY - Math.round(heightPx * 0.25) }, // right of main
            { x: avoidRect.left - widthPx - offset, y: anchorY - Math.round(heightPx * 0.25) }, // left of main
            { x: anchorX - Math.round(widthPx * 0.2), y: avoidRect.top - heightPx - offset }, // above main
            { x: anchorX - Math.round(widthPx * 0.2), y: avoidRect.bottom + offset } // below main
        ].map(clampPoint);

        for (const c of outsideCandidates) {
            if (!intersectsAvoidRect(c)) return c;
        }
    }

    for (const c of candidates) {
        const clamped = clampPoint(c);
        if (!intersectsAvoidRect(clamped)) return clamped;
    }

    // Final fallback: clamp the default candidate into monitor bounds.
    return clampPoint(candidates[0]);
};

const placeAndShowPendingCompactPreview = async (
    widthLogical: number,
    heightLogical: number,
    options?: { keepPending?: boolean }
) => {
    if (!compactPreviewPendingShow || !compactPreviewWindow || !compactPreviewPendingAnchor) {
        compactPreviewLog("skip place/show: pending state not ready", {
            pendingShow: compactPreviewPendingShow,
            hasWindow: !!compactPreviewWindow,
            hasAnchor: !!compactPreviewPendingAnchor
        });
        return;
    }

    const appWindow = getCurrentWindow();
    const scale = await appWindow.scaleFactor();
    const monitor = await currentMonitor();
    const monitorPos = monitor?.position || { x: 0, y: 0 };
    const monitorSize = monitor?.size || { width: 1920, height: 1080 };
    const margin = Math.round(10 * scale);
    const offset = Math.round(12 * scale);

    const widthPx = Math.round(widthLogical * scale);
    const heightPx = Math.round(heightLogical * scale);
    const anchorPx = await resolveAnchorPhysical(compactPreviewPendingAnchor, scale);
    const mainOuter = await appWindow.outerPosition().catch(() => null);
    const mainSize = await appWindow.outerSize().catch(() => null);
    const avoidRect =
        mainOuter && mainSize
            ? {
                left: mainOuter.x,
                top: mainOuter.y,
                right: mainOuter.x + mainSize.width,
                bottom: mainOuter.y + mainSize.height
            }
            : null;

    const target = pickPreviewPosition(
        anchorPx.x,
        anchorPx.y,
        widthPx,
        heightPx,
        monitorPos,
        monitorSize,
        margin,
        offset,
        avoidRect
    );
    compactPreviewLog("place/show target resolved", {
        widthLogical,
        heightLogical,
        widthPx,
        heightPx,
        anchorPx,
        target,
        avoidRect,
        scale
    });

    setIgnoreBlurSafe(true);
    try {
        await compactPreviewWindow.setPosition(new PhysicalPosition(target.x, target.y));
        await compactPreviewWindow.show();
        // Force top-most z-order refresh so preview is not occluded by the main top-most window.
        // macOS skips this toggle because frequent style-mask sync can cause UI stalls.
        if (!IS_MACOS) {
            try {
                await compactPreviewWindow.setAlwaysOnTop(false);
                await compactPreviewWindow.setAlwaysOnTop(true);
                compactPreviewLog("refresh always-on-top stacking done");
            } catch (stackErr) {
                compactPreviewLog("refresh always-on-top stacking failed", stackErr);
            }
        }
        const visible = await compactPreviewWindow.isVisible().catch(() => null);
        compactPreviewLog("preview window shown", { visible, target });
    } catch (err) {
        setIgnoreBlurSafe(false);
        compactPreviewLog("preview show failed", err);
        throw err;
    }
    if (options?.keepPending) {
        compactPreviewLog("keep pending state after place/show", { widthLogical, heightLogical });
    } else {
        clearCompactPreviewPendingState();
    }
};

const hideCompactPreviewGlobal = async () => {
    const previewWindow = compactPreviewWindow;
    compactPreviewLog("hide preview requested", { hasWindow: !!previewWindow });
    clearCompactPreviewPendingState();
    setIgnoreBlurSafe(false);

    if (!previewWindow) return;

    try {
        await previewWindow.hide();
        const visible = await previewWindow.isVisible().catch(() => null);
        compactPreviewLog("preview window hidden", { visible });
    } catch (err) {
        console.error("Failed to hide compact preview window:", err);
        compactPreviewLog("hide preview failed, reset window reference", err);
        compactPreviewWindow = null;
        compactPreviewMounted = false;
        compactPreviewMountedPromise = null;
    }
};

const forceHideCompactPreviewWindow = () => {
    void hideCompactPreviewGlobal();
};

const seekVideoPreviewFrame = (video: HTMLVideoElement | null) => {
    if (!video) return;
    const duration = video.duration;
    if (!Number.isFinite(duration) || duration <= 0) return;
    const maxSeek = Math.max(duration - 0.05, 0);
    if (maxSeek <= 0) return;
    const preferred = Math.min(duration * 0.1, 2);
    const target = Math.min(Math.max(preferred, 0.1), maxSeek);
    if (target <= 0) return;
    try {
        video.currentTime = target;
    } catch {
        // Ignore seek errors; fallback will just show the first frame.
    }
};

const waitForCompactPreviewMounted = async (): Promise<boolean> => {
    if (compactPreviewMounted) {
        compactPreviewLog("mounted already true, skip wait");
        return true;
    }
    if (!compactPreviewMountedPromise) {
        compactPreviewLog("waiting compact preview mounted event...");
        compactPreviewMountedPromise = new Promise(async (resolve) => {
            const timeout = setTimeout(() => {
                compactPreviewLog("wait compact-preview-mounted timeout");
                resolve(false);
            }, 1200);
            try {
                const unlisten = await listen("compact-preview-mounted", () => {
                    compactPreviewMounted = true;
                    clearTimeout(timeout);
                    unlisten();
                    compactPreviewLog("received compact-preview-mounted");
                    resolve(true);
                });
            } catch (err) {
                clearTimeout(timeout);
                console.error("Failed to listen for compact preview ready:", err);
                compactPreviewLog("listen compact-preview-mounted failed", err);
                resolve(false);
            }
        });
    }
    return compactPreviewMountedPromise;
};

const ensureCompactPreviewResizeListener = async (): Promise<void> => {
    if (compactPreviewResizeListener) {
        await compactPreviewResizeListener;
        return;
    }
    compactPreviewLog("register compact-preview-resize listener");
    compactPreviewResizeListener = listen<{ width: number; height: number }>(
        "compact-preview-resize",
        async (event) => {
            const { width, height } = event.payload || {};
            if (!width || !height) {
                compactPreviewLog("ignore compact-preview-resize with invalid payload", event.payload);
                return;
            }
            compactPreviewLog("received compact-preview-resize", { width, height });

            try {
                await placeAndShowPendingCompactPreview(width, height);
            } catch (err) {
                console.error("Failed to resize compact preview window:", err);
                compactPreviewLog("resize handling failed", err);
            }
        }
    );
    await compactPreviewResizeListener;
};

const ensureCompactPreviewLifecycleListeners = async (): Promise<void> => {
    if (compactPreviewLifecycleListenersReady) {
        await compactPreviewLifecycleListenersReady;
        return;
    }

    compactPreviewLifecycleListenersReady = (async () => {
        const lifecycleEvents = ["tauri://hide", "tauri://close-requested", "tauri://destroyed"];
        await Promise.all(
            lifecycleEvents.map(async (eventName) => {
                try {
                    compactPreviewLog("bind lifecycle listener", eventName);
                    await listen(eventName, () => {
                        compactPreviewLog("lifecycle event -> hide preview", eventName);
                        void hideCompactPreviewGlobal();
                    });
                } catch (err) {
                    console.error(`Failed to bind compact preview lifecycle listener: ${eventName}`, err);
                    compactPreviewLog("bind lifecycle listener failed", { eventName, err });
                }
            })
        );
    })();

    await compactPreviewLifecycleListenersReady;
};

const tryReuseExistingCompactPreviewWindow = async (): Promise<WebviewWindow | null> => {
    try {
        const { WebviewWindow } = await loadWebviewWindowModule();
        const existing = await WebviewWindow.getByLabel(COMPACT_PREVIEW_LABEL);
        if (!existing) {
            compactPreviewLog("no existing compact preview window by label");
            return null;
        }

        const visible = await existing.isVisible().catch(() => null);
        compactPreviewLog("reuse compact preview window by label", { visible });
        compactPreviewWindow = existing;
        compactPreviewMounted = true;
        compactPreviewMountedPromise = Promise.resolve(true);
        try {
            await existing.setIgnoreCursorEvents(true);
        } catch { }
        try {
            await existing.setAlwaysOnTop(true);
        } catch { }
        return existing;
    } catch (err) {
        compactPreviewLog("reuse compact preview window by label failed", err);
        return null;
    }
};

const ensureCompactPreviewWindow = async (): Promise<WebviewWindow | null> => {
    if (!COMPACT_PREVIEW_WINDOW_SUPPORTED) return null;
    if (compactPreviewWindow) {
        compactPreviewMounted = true;
        compactPreviewMountedPromise = Promise.resolve(true);
        compactPreviewLog("reuse existing compact preview window");
        return compactPreviewWindow;
    }
    if (compactPreviewReady) return compactPreviewReady;
    if (compactPreviewCreating) return null;
    const reusedBeforeCreate = await tryReuseExistingCompactPreviewWindow();
    if (reusedBeforeCreate) {
        return reusedBeforeCreate;
    }
    compactPreviewLog("create compact preview window start");
    compactPreviewCreating = true;
    compactPreviewReady = (async () => {
        try {
            const { WebviewWindow } = await loadWebviewWindowModule();
            const previewWindow = new WebviewWindow(COMPACT_PREVIEW_LABEL, {
                url: "index.html?window=compact-preview",
                decorations: false,
                transparent: true,
                resizable: false,
                skipTaskbar: true,
                alwaysOnTop: true,
                visible: false,
                focus: false,
                focusable: false,
                shadow: false
            });

            compactPreviewMounted = false;
            compactPreviewMountedPromise = null;
            compactPreviewLog("compact preview window instance created, waiting tauri://created");

            const created = await new Promise<boolean>((resolve) => {
                const timeout = setTimeout(() => resolve(false), 1500);
                previewWindow.once("tauri://created", () => {
                    clearTimeout(timeout);
                    compactPreviewLog("compact preview tauri://created");
                    resolve(true);
                });
                previewWindow.once("tauri://error", (event) => {
                    clearTimeout(timeout);
                    compactPreviewLog("compact preview tauri://error", event.payload);
                    resolve(false);
                });
            });

            if (!created) {
                compactPreviewLog("compact preview create timeout/failure, try reuse by label");
                const reusedAfterFailedCreate = await tryReuseExistingCompactPreviewWindow();
                if (reusedAfterFailedCreate) {
                    return reusedAfterFailedCreate;
                }
                return null;
            }

            try {
                await previewWindow.setSize(new PhysicalSize(1, 1));
            } catch (err) {
                console.error("Failed to initialize compact preview size:", err);
            }

            try {
                await previewWindow.setIgnoreCursorEvents(true);
            } catch (err) {
                console.error("Failed to enable ignore cursor events:", err);
            }

            compactPreviewWindow = previewWindow;
            compactPreviewLog("compact preview window ready");
            return previewWindow;
        } catch (err) {
            console.error("Failed to create compact preview window:", err);
            compactPreviewLog("create compact preview window failed", err);
            return null;
        } finally {
            compactPreviewCreating = false;
            compactPreviewReady = null;
        }
    })();
    return compactPreviewReady;
};

/**
 * Pre-warm the compact preview window so it's ready before the user hovers.
 * On macOS we deliberately skip warmup to reduce startup-time UI stalls.
 */
const warmupCompactPreviewWindow = () => {
    if (!COMPACT_PREVIEW_WINDOW_SUPPORTED || !COMPACT_PREVIEW_WARMUP_SUPPORTED) return;
    // Only warm up if not already created/creating
    if (compactPreviewWindow || compactPreviewCreating || compactPreviewReady) return;
    compactPreviewLog("warmup: pre-creating compact preview window");
    // Fire and forget - creates the window in the background
    ensureCompactPreviewWindow().catch((err) => {
        compactPreviewLog("warmup: failed", err);
    });
};

const isCompactPreviewWindowSupported = () => COMPACT_PREVIEW_WINDOW_SUPPORTED;
const isCompactPreviewWarmupSupported = () => COMPACT_PREVIEW_WARMUP_SUPPORTED;

registerCompactPreviewControls({
    forceHide: forceHideCompactPreviewWindow,
    warmup: warmupCompactPreviewWindow,
    supported: isCompactPreviewWindowSupported,
    warmupSupported: isCompactPreviewWarmupSupported,
});

const getIcon = (type: string) => {
    switch (type) {
        case "text": return <FileText size={14} />;
        case "image": return <ImageIcon size={14} />;
        case "url": return <LinkIcon size={14} />;
        case "code": return <Code size={14} />;
        case "file": return <File size={14} />;
        case "video": return <Video size={14} />;
        default: return <FileText size={14} />;
    }
};

const renderSourceAppIcon = (iconSrc: string | null, contentType: string, sourceApp: string) => {
    if (!iconSrc) {
        return getIcon(contentType);
    }

    return (
        <img
            src={iconSrc}
            alt={`${sourceApp} icon`}
            className="source-app-icon"
            loading="lazy"
        />
    );
};

const getFallbackFileIcon = (filePath: string) => {
    const ext = filePath.split('.').pop()?.toLowerCase();
    switch (ext) {
        case 'zip':
        case 'rar':
        case '7z':
        case 'tar':
        case 'gz':
            return <FileArchive size={20} />;
        case 'mp3':
        case 'wav':
        case 'flac':
        case 'm4a':
            return <Music size={20} />;
        case 'exe':
        case 'msi':
        case 'bat':
        case 'sh':
            return <Cpu size={20} />;
        case 'pdf':
        case 'doc':
        case 'docx':
        case 'ppt':
        case 'pptx':
        case 'xls':
        case 'xlsx':
            return <FileText size={20} />;
        case 'js':
        case 'ts':
        case 'tsx':
        case 'jsx':
        case 'py':
        case 'rs':
        case 'c':
        case 'cpp':
        case 'go':
        case 'java':
        case 'html':
        case 'css':
        case 'json':
            return <FileCode size={20} />;
        default:
            return <File size={20} />;
    }
};

const ClipboardItem = ({
    item,
    isSelected,
    isSensitiveHidden,
    isRevealed,
    isEditingTags,
    tagInput,
    tagSuggestions = [],
    allTagNames,
    theme,
    language,
    t,
    isAIProcessing,
    onSelect,
    onCopy,
    onToggleReveal,
    onOpen,
    onTogglePin,
    onDelete,
    onToggleTagEditor,
    onTagInput,
    onTagAdd,
    onTagPick,
    onTagEditCancel,
    onTagDelete,
    onAIAction,
    onInputSubmit,
    onEdit,
    isEditingBody = false,
    bodyInitialDraft,
    bodyInitialHtml,
    bodyEditIsRich = false,
    bodyEditSaving = false,
    bodyEditError,
    onBodyEditSave,
    onBodyEditCancel,
    onEditNote,
    isEditingNote = false,
    noteInitialDraft,
    noteEditSaving = false,
    noteEditError,
    onNoteEditSave,
    onNoteEditCancel,
    aiEnabled,
    aiOptionsOpen,
    onAIOptionsToggle,
    tagColors,
    richTextSnapshotPreview = false,
    showSourceAppIcon = true,
    sensitiveMaskPrefixVisible = 3,
    sensitiveMaskSuffixVisible = 3,
    sensitiveMaskEmailDomain = false,
    quickPasteHint,
    dragControls,
    id,
    compactMode,
    className,
    disableLayout
}: ClipboardItemProps & { compactMode?: boolean, className?: string }) => {
    /**
     * v0.5 需求⑨：「移动到标签 / 复制到标签」的入口开关。
     *
     * 状态与整块交互都放在独立的 `TagAssignMenu` 组件里，这里只留一个布尔量；
     * 这样并行修改同一文件的其他改动不会与本功能互相踩到。
     */
    const [isTagAssignOpen, setIsTagAssignOpen] = useState(false);
    const itemRef = useRef<HTMLDivElement | null>(null);
    const tagInputRef = useRef<HTMLInputElement>(null);
    const [localTagInput, setLocalTagInput] = useState(tagInput);
    const [localAiOptionsOpen, setLocalAiOptionsOpen] = useState(!!aiOptionsOpen);
    /**
     * R10: draft of the body editor. Seeded from `bodyInitialDraft` each time the
     * dialog opens, so cancelling and reopening never resurrects a discarded draft.
     */
    const [bodyDraft, setBodyDraft] = useState<string>(() => bodyInitialDraft ?? "");
    const bodyEditorOpen = isEditingBody && !!onBodyEditSave;
    /**
     * R11: draft of the note editor, seeded from `noteInitialDraft` when the dialog
     * opens — same lifecycle as `bodyDraft`, so a discarded draft never comes back.
     */
    const [noteDraft, setNoteDraft] = useState<string>(() => noteInitialDraft ?? "");
    const noteEditorOpen = isEditingNote && !!onNoteEditSave;
    const noteText = getEntryNote(item);
    const noteIsEmpty = noteText.trim().length === 0;
    const bodyEditorTextareaRef = useRef<HTMLTextAreaElement | null>(null);
    /**
     * R13: 富文本编辑器的 DOM 节点。
     *
     * contentEditable **必须**是非受控的：让 React 在每次 render 时重写它的内容会把
     * 光标推到开头、打断中文输入法。所以这里只在弹窗打开时写一次初值，之后只读取
     * `innerHTML`。`bodyDraft` 仍跟随输入更新，以便 Ctrl+Enter 与保存按钮拿到最新值。
     */
    const bodyEditorRichRef = useRef<HTMLDivElement | null>(null);
    const [snapshotFailed, setSnapshotFailed] = useState(false);
    const [richImageFallbackFailed, setRichImageFallbackFailed] = useState(false);
    const [sourceAppIcon, setSourceAppIcon] = useState<string | null>(() => peekSourceAppIcon(item.source_app_path) ?? null);
    const filePaths = useMemo(
        () => item.content_type === "file" ? item.content.split('\n').filter((p) => p.trim()) : [],
        [item.content, item.content_type]
    );
    const singleFilePath = filePaths.length === 1 ? filePaths[0] : null;
    const [fileIcon, setFileIcon] = useState<string | null>(() => peekFileIcon(singleFilePath) ?? null);
    const isComposing = useRef(false);
    const richSnapshotImgRef = useRef<HTMLImageElement | null>(null);
    const richSnapshotFallbackTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
    const hoverTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
    const hoverAnchorRef = useRef<CompactPreviewAnchor | null>(null);
    const hoverRequestIdRef = useRef(0);
    const richTextFallback = item.content_type === "rich_text" && item.html_content
        ? (() => {
            const { cleanHtml, imagePayload } = extractRichImageFallback(item.html_content);
            return {
                cleanHtml: cleanHtml || item.html_content,
                imagePayload,
                imageSrc: resolveRichImageSrc(imagePayload)
            };
        })()
        : null;
    const pickableTagSuggestions = useMemo(
        () =>
            selectTagSuggestions({
                editing: isEditingTags,
                query: localTagInput,
                allTags: tagSuggestions,
                existingTags: item.tags || [],
            }),
        [isEditingTags, item.tags, localTagInput, tagSuggestions]
    );

    const [tagSuggestIndex, setTagSuggestIndex] = useState(-1);
    const tagSuggestListRef = useRef<HTMLDivElement | null>(null);

    useEffect(() => {
        if (!isEditingTags) setTagSuggestIndex(-1);
    }, [isEditingTags]);

    /*
     * 候选变化时维护高亮下标。
     *
     * 【为什么候选出现时要**自动高亮第一项**】
     *
     * 用户要的是"类似于 tab 那种"补全。而 Tab 补全的前提是**有一个默认选中项** ——
     * 若初始下标是 -1（无高亮），用户打完 `i` 按 Tab 什么都不会发生，得先按一次
     * 方向键才能选中，这就不像补全了。
     *
     * 原实现 `if (prev < 0) return -1;` 把"从未高亮"与"候选清空后保持无高亮"
     * 混为一谈，于是下标**永远停在 -1**，方向键成了唯一进入方式。
     * 现在改为：列表从空变非空时**自动指向 0**。
     *
     * 不做"每次输入都重置为 0" —— 那会让用户按方向键选中第 3 项后，
     * 再多打一个字就被拽回第 1 项。
     */
    useEffect(() => {
        setTagSuggestIndex((prev) => {
            const n = pickableTagSuggestions.length;
            if (n === 0) return -1;
            if (prev < 0) return 0;
            return Math.min(prev, n - 1);
        });
    }, [pickableTagSuggestions]);

    useLayoutEffect(() => {
        if (tagSuggestIndex < 0 || !tagSuggestListRef.current) return;
        const row = tagSuggestListRef.current.children[tagSuggestIndex] as HTMLElement | undefined;
        row?.scrollIntoView({ block: "nearest" });
    }, [tagSuggestIndex, pickableTagSuggestions]);

    useEffect(() => {
        if (!isEditingTags || !onTagEditCancel) return;

        const onDocMouseDown = (e: MouseEvent) => {
            if (e.button !== 0) return;
            const root = itemRef.current;
            if (!root) return;
            const t = e.target as HTMLElement;

            if (root.contains(t)) {
                if (t.closest(".tag-edit-anchor")) return;
                if (t.closest(".item-tags-container .tag-chip")) return;
                if (
                    t.closest("button") ||
                    t.closest("input") ||
                    t.closest("textarea") ||
                    t.closest('[role="button"]') ||
                    t.closest(".drag-handle")
                ) {
                    return;
                }
            }

            onTagEditCancel();
            e.preventDefault();
            e.stopPropagation();
        };

        document.addEventListener("mousedown", onDocMouseDown, true);
        return () => document.removeEventListener("mousedown", onDocMouseDown, true);
    }, [isEditingTags, onTagEditCancel]);

    const sensitivePreview = useMemo(
        () => formatSensitivePreview(item.content, item.content_type, {
            prefixVisible: sensitiveMaskPrefixVisible,
            suffixVisible: sensitiveMaskSuffixVisible,
            maskEmailDomain: sensitiveMaskEmailDomain,
        }),
        [
            item.content,
            item.content_type,
            sensitiveMaskPrefixVisible,
            sensitiveMaskSuffixVisible,
            sensitiveMaskEmailDomain
        ]
    );
    const richTextCleanHtml = richTextFallback?.cleanHtml || item.html_content || "";
    const richTextSnapshotDisplayMaxHeight = compactMode ? 40 : 64;
    const richTextSnapshotRenderMaxHeight = compactMode ? 100 : 200;
    const spreadsheetLikeRichSource = item.content_type === "rich_text"
        && !!item.html_content
        && isSpreadsheetLikeSource(item.source_app, item.source_app_path);
    const richTextHasAnimatedImageFallback = isAnimatedGifSrc(
        richTextFallback?.imagePayload || richTextFallback?.imageSrc || null
    );
    const preferHtmlRichPreview = item.content_type === "rich_text"
        && !!item.html_content
        && !richTextHasAnimatedImageFallback
        && !richHtmlLooksTabular(richTextCleanHtml)
        && !spreadsheetLikeRichSource;
    const preferGeneratedRichPreview = item.content_type === "rich_text"
        && !!item.html_content
        && !preferHtmlRichPreview
        && (
            !!richTextSnapshotPreview
            || richHtmlLooksTabular(richTextCleanHtml)
            || spreadsheetLikeRichSource
        );
    const richTextSnapshotSrc = useMemo(() => {
        if (!preferGeneratedRichPreview) return null;
        if (item.content_type !== "rich_text" || !item.html_content) return null;
        if (!richTextCleanHtml) return null;
        return getRichTextSnapshotDataUrl(richTextCleanHtml, {
            width: compactMode ? 360 : 560,
            // Keep source snapshot height bounded so list-item preview does not over-shrink text.
            maxHeight: richTextSnapshotRenderMaxHeight
        });
    }, [
        preferGeneratedRichPreview,
        item.content_type,
        item.html_content,
        richTextCleanHtml,
        compactMode,
        richTextSnapshotRenderMaxHeight
    ]);
    const effectiveRichTextSnapshotSrc = !snapshotFailed ? richTextSnapshotSrc : null;
    const effectiveRichImageFallbackSrc = !richImageFallbackFailed
        ? (richTextFallback?.imageSrc || null)
        : null;
    const preferImageFallbackForTabular = (
        richHtmlLooksTabular(richTextCleanHtml) || spreadsheetLikeRichSource
    ) && !!effectiveRichImageFallbackSrc;
    const richTextPreviewSrc = richTextHasAnimatedImageFallback
        ? (effectiveRichImageFallbackSrc || effectiveRichTextSnapshotSrc)
        : preferImageFallbackForTabular
            ? (effectiveRichImageFallbackSrc || effectiveRichTextSnapshotSrc)
            : (effectiveRichTextSnapshotSrc || null);
    const useSnapshotPreviewImage = !!richTextPreviewSrc && richTextPreviewSrc === effectiveRichTextSnapshotSrc;
    const useRichImageFallback = !!richTextPreviewSrc && richTextPreviewSrc === effectiveRichImageFallbackSrc;
    const visibleTagCount = item.tags?.length || 0;
    const hasTagsSection = visibleTagCount > 0 || isEditingTags;
    const overlayTagsInPreview = !compactMode && !isEditingTags && visibleTagCount > 0;
    const standaloneColorValue = useMemo(
        () => getStandaloneColorValue(item.content_type, item.content),
        [item.content, item.content_type]
    );

    useEffect(() => {
        let cancelled = false;
        const sourceAppPath = item.source_app_path?.trim();
        const cachedIcon = peekSourceAppIcon(sourceAppPath);

        if (!showSourceAppIcon) {
            setSourceAppIcon(null);
            return () => {
                cancelled = true;
            };
        }

        if (cachedIcon !== undefined) {
            setSourceAppIcon(cachedIcon ?? null);
            return () => {
                cancelled = true;
            };
        }

        setSourceAppIcon(null);
        if (!sourceAppPath) {
            return () => {
                cancelled = true;
            };
        }

        getSourceAppIcon(sourceAppPath).then((icon) => {
            if (!cancelled) {
                setSourceAppIcon(icon);
            }
        });

        return () => {
            cancelled = true;
        };
    }, [item.source_app_path, showSourceAppIcon]);

    useEffect(() => {
        let cancelled = false;
        const cachedIcon = peekFileIcon(singleFilePath);

        if (item.content_type !== "file" || item.file_preview_exists === false || !singleFilePath) {
            setFileIcon(null);
            return () => {
                cancelled = true;
            };
        }

        if (cachedIcon !== undefined) {
            setFileIcon(cachedIcon ?? null);
            return () => {
                cancelled = true;
            };
        }

        setFileIcon(null);
        getSystemFileIcon(singleFilePath).then((icon) => {
            if (!cancelled) {
                setFileIcon(icon);
            }
        });

        return () => {
            cancelled = true;
        };
    }, [item.content_type, item.file_preview_exists, singleFilePath]);

    const compactPreviewEnabled =
        compactMode &&
        COMPACT_PREVIEW_WINDOW_SUPPORTED &&
        item.content_type !== "file";

    const isHoverPreviewRequestCurrent = (requestId: number) => {
        const node = itemRef.current;
        return (
            hoverRequestIdRef.current === requestId &&
            !!hoverAnchorRef.current &&
            !!node &&
            node.isConnected &&
            node.matches(":hover")
        );
    };

    const cancelHoverPreview = () => {
        hoverRequestIdRef.current += 1;
        if (hoverTimerRef.current) {
            clearTimeout(hoverTimerRef.current);
            hoverTimerRef.current = null;
        }
        hoverAnchorRef.current = null;
    };

    const hideCompactPreview = async () => {
        cancelHoverPreview();
        await hideCompactPreviewGlobal();
    };

    const showCompactPreview = async (anchor: CompactPreviewAnchor, requestId: number) => {
        if (!compactPreviewEnabled) return;
        if (!isHoverPreviewRequestCurrent(requestId)) {
            compactPreviewLog("show preview aborted: stale hover request before start", {
                itemId: item.id,
                requestId
            });
            return;
        }
        compactPreviewLog("show preview requested", {
            itemId: item.id,
            contentType: item.content_type,
            anchor
        });
        let previewWindow = await ensureCompactPreviewWindow();
        if (!isHoverPreviewRequestCurrent(requestId)) {
            compactPreviewLog("show preview aborted: stale hover request after ensure window", {
                itemId: item.id,
                requestId
            });
            return;
        }
        if (!previewWindow) {
            compactPreviewLog("show preview aborted: window unavailable");
            return;
        }
        await ensureCompactPreviewLifecycleListeners();
        if (!isHoverPreviewRequestCurrent(requestId)) {
            compactPreviewLog("show preview aborted: stale hover request after lifecycle listeners", {
                itemId: item.id,
                requestId
            });
            return;
        }
        await ensureCompactPreviewResizeListener();
        if (!isHoverPreviewRequestCurrent(requestId)) {
            compactPreviewLog("show preview aborted: stale hover request after resize listener", {
                itemId: item.id,
                requestId
            });
            return;
        }
        compactPreviewLog("preview listeners ready");
        const mounted = await waitForCompactPreviewMounted();
        if (!isHoverPreviewRequestCurrent(requestId)) {
            compactPreviewLog("show preview aborted: stale hover request after mounted wait", {
                itemId: item.id,
                requestId
            });
            return;
        }
        compactPreviewLog("mounted state before emit", { mounted });
        if (!mounted) {
            compactPreviewLog("mounted wait returned false; continue with fallback timer");
        }

        try {
            const rootStyle = getComputedStyle(document.documentElement);
            const clipboardItemFontSizeRaw = parseInt(
                rootStyle.getPropertyValue("--clipboard-item-font-size")
            );
            const clipboardTagFontSizeRaw = parseInt(
                rootStyle.getPropertyValue("--clipboard-tag-font-size")
            );
            const clipboardItemFontSize = Number.isFinite(clipboardItemFontSizeRaw)
                ? clipboardItemFontSizeRaw
                : undefined;
            const clipboardTagFontSize = Number.isFinite(clipboardTagFontSizeRaw)
                ? clipboardTagFontSizeRaw
                : undefined;
            const colorMode = document.documentElement.classList.contains("dark-mode") ? "dark" : "light";

            if (!isHoverPreviewRequestCurrent(requestId)) {
                compactPreviewLog("show preview aborted: stale hover request before emit", {
                    itemId: item.id,
                    requestId
                });
                return;
            }
            compactPreviewPendingShow = true;
            compactPreviewPendingAnchor = anchor;
            compactPreviewLog("emit compact-preview-update", {
                itemId: item.id,
                contentType: item.content_type,
                hasHtml: !!item.html_content
            });
            await previewWindow.emit("compact-preview-update", {
                contentType: item.content_type,
                content: item.content,
                preview: item.preview,
                htmlContent: item.html_content,
                sourceApp: item.source_app,
                timestamp: item.timestamp,
                language,
                theme,
                colorMode,
                richTextSnapshotPreview,
                clipboardItemFontSize,
                clipboardTagFontSize
            });
            compactPreviewLog("emit compact-preview-update done");
            if (compactPreviewPendingTimer) {
                clearTimeout(compactPreviewPendingTimer);
            }
            compactPreviewPendingTimer = setTimeout(async () => {
                if (!compactPreviewPendingShow || !compactPreviewWindow || !compactPreviewPendingAnchor) {
                    compactPreviewLog("fallback timer canceled: pending state changed");
                    return;
                }
                try {
                    compactPreviewLog("fallback timer place/show with default size");
                    await placeAndShowPendingCompactPreview(320, 220, { keepPending: true });
                } catch (fallbackErr) {
                    console.error("Failed to show compact preview window (fallback):", fallbackErr);
                    compactPreviewLog("fallback place/show failed", fallbackErr);
                }
            }, 200);
        } catch (err) {
            const message = err instanceof Error ? err.message : String(err);
            if (message.includes("window not found")) {
                compactPreviewLog("window not found, recreate flow");
                compactPreviewWindow = null;
                compactPreviewMounted = false;
                compactPreviewMountedPromise = null;
                previewWindow = await ensureCompactPreviewWindow();
                if (!isHoverPreviewRequestCurrent(requestId)) {
                    compactPreviewLog("show preview aborted: stale hover request after recreate", {
                        itemId: item.id,
                        requestId
                    });
                    return;
                }
                if (!previewWindow) return;
                try {
                    compactPreviewPendingShow = true;
                    compactPreviewPendingAnchor = anchor;
                    compactPreviewLog("emit compact-preview-update after recreate");
                    await previewWindow.emit("compact-preview-update", {
                        contentType: item.content_type,
                        content: item.content,
                        preview: item.preview,
                        htmlContent: item.html_content,
                        sourceApp: item.source_app,
                        timestamp: item.timestamp,
                        language,
                        theme,
                        richTextSnapshotPreview,
                        colorMode: document.documentElement.classList.contains("dark-mode") ? "dark" : "light"
                    });
                    compactPreviewLog("emit compact-preview-update after recreate done");
                    if (compactPreviewPendingTimer) {
                        clearTimeout(compactPreviewPendingTimer);
                    }
                    compactPreviewPendingTimer = setTimeout(async () => {
                        if (!compactPreviewPendingShow || !compactPreviewWindow || !compactPreviewPendingAnchor) {
                            compactPreviewLog("recreate fallback canceled: pending state changed");
                            return;
                        }
                        try {
                            compactPreviewLog("recreate fallback place/show with default size");
                            await placeAndShowPendingCompactPreview(320, 220, { keepPending: true });
                        } catch (fallbackErr) {
                            console.error("Failed to show compact preview window (fallback):", fallbackErr);
                            compactPreviewLog("recreate fallback failed", fallbackErr);
                        }
                    }, 200);
                } catch (retryErr) {
                    console.error("Failed to show compact preview window:", retryErr);
                    compactPreviewLog("recreate flow failed", retryErr);
                }
                return;
            }
            console.error("Failed to show compact preview window:", err);
            compactPreviewLog("show preview failed", err);
        }
    };

    // Sync local state when prop changes (e.g. when editor opens)
    useEffect(() => {
        setLocalTagInput(tagInput);
    }, [tagInput]);

    useEffect(() => {
        setLocalAiOptionsOpen(!!aiOptionsOpen);
    }, [aiOptionsOpen]);

    useEffect(() => {
        setSnapshotFailed(false);
        setRichImageFallbackFailed(false);
    }, [item.id, item.html_content, richTextSnapshotPreview, compactMode]);

    useEffect(() => {
        if (richSnapshotFallbackTimerRef.current) {
            clearTimeout(richSnapshotFallbackTimerRef.current);
            richSnapshotFallbackTimerRef.current = null;
        }
        if (!useSnapshotPreviewImage) return;

        // Safety net: some WebView failures do not reliably fire <img onError>.
        richSnapshotFallbackTimerRef.current = setTimeout(() => {
            const img = richSnapshotImgRef.current;
            if (!img || !img.complete || img.naturalWidth <= 0 || img.naturalHeight <= 0) {
                richPreviewFailureLog("snapshot image timeout -> fallback to html", {
                    itemId: item.id,
                    hasImageElement: !!img,
                    complete: img?.complete ?? false,
                    naturalWidth: img?.naturalWidth ?? 0,
                    naturalHeight: img?.naturalHeight ?? 0
                });
                setSnapshotFailed(true);
            }
        }, 700);

        return () => {
            if (richSnapshotFallbackTimerRef.current) {
                clearTimeout(richSnapshotFallbackTimerRef.current);
                richSnapshotFallbackTimerRef.current = null;
            }
        };
    }, [useSnapshotPreviewImage, effectiveRichTextSnapshotSrc, item.id]);

    const showAIOptions = localAiOptionsOpen;
    const inlineAiVariants = {
        open: { opacity: 1, height: "auto", marginTop: 8, marginBottom: 8 },
        collapsed: { opacity: 0, height: 0, marginTop: 0, marginBottom: 0 }
    };
    useEffect(() => {
        if (isEditingTags && tagInputRef.current) {
            tagInputRef.current.focus();
        }
    }, [isEditingTags]);

    /**
     * R10: seed the draft when the body editor opens. `item.content` is the source of
     * truth; the note is deliberately not part of this draft (the note editor belongs
     * to the tag-management side and writes through a different command).
     */
    useEffect(() => {
        if (!bodyEditorOpen) return;
        setBodyDraft(bodyInitialDraft ?? item.content ?? "");
        // The dialog is portalled to <body>, so it would otherwise sit under the
        // blurred/backdropped list. Close the hover preview for the same reason.
        void hideCompactPreview();
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [bodyEditorOpen, bodyInitialDraft]);

    /**
     * R13: 富文本编辑器是**非受控**的，所以初值必须在挂载后手动写进去。
     *
     * 用 `sanitizeHTML`（显示侧那道弱净化）洗一遍再写：这里的目的不是安全 ——
     * 安全由后端写入路径的白名单净化器负责 —— 而是**复用渲染同一条管线**，
     * 让编辑器里看到的内容与条目在列表里显示的内容一致（Office 噪声清理、
     * 内嵌图片路径转 `asset:` 等都在那一步完成）。
     *
     * 依赖里带上 `bodyEditorOpen`：关掉再打开同一个条目时要重新写入初值，否则用户
     * 丢弃的草稿会在重开时复活。
     */
    useEffect(() => {
        if (!bodyEditorOpen || !bodyEditIsRich) return;
        const node = bodyEditorRichRef.current;
        if (!node) return;
        const raw = bodyInitialHtml ?? "";
        const { html } = sanitizeHTML(raw);
        node.innerHTML = html || escapeHtmlForEditor(raw);
        setBodyDraft(node.innerHTML);
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [bodyEditorOpen, bodyEditIsRich, bodyInitialHtml]);

    /**
     * R11: seed the note draft when the note editor opens. `item.note` is the source of
     * truth; the hook passes the same value in `noteInitialDraft` so the draft and the
     * row cannot disagree at open time.
     */
    useEffect(() => {
        if (!noteEditorOpen) return;
        setNoteDraft(noteInitialDraft ?? getEntryNote(item));
        // Same reason as the body editor: the dialog is portalled to <body> and would
        // otherwise sit under the hover preview.
        void hideCompactPreview();
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [noteEditorOpen, noteInitialDraft]);

    useEffect(() => {
        if (!compactPreviewEnabled) {
            void hideCompactPreview();
        }
    }, [compactPreviewEnabled]);

    useEffect(() => {
        return () => {
            cancelHoverPreview();
            void hideCompactPreviewGlobal();
        };
    }, []);

    const renderFilePreview = () => {
        if (item.file_preview_exists === false) {
            return (
                <div className="file-thumbnail-card error-bg" title={t('file_deleted') || "File Deleted"}>
                    <div className="file-icon-wrapper error-icon">
                        <FileQuestion size={24} />
                    </div>
                    <div className="file-info-wrapper">
                        <div className="file-name error-text">{t('file_deleted') || "Deleted"}</div>
                        <div className="file-hint error-text">{item.content}</div>
                    </div>
                </div>
            );
        }

        if (filePaths.length > 1) {
            return (
                <div className="file-thumbnail-card" title={item.content}>
                    <div className="file-icon-wrapper">
                        <Files size={24} />
                    </div>
                    <div className="file-info-wrapper">
                        <div className="file-name">{filePaths.length} {t('items')}</div>
                        <div className="file-hint">{filePaths[0].split(/[\\/]/).pop()} ...</div>
                    </div>
                </div>
            );
        }

        const filePath = filePaths[0];
        if (!filePath) {
            return (
                <div className="file-thumbnail-card" title={item.content}>
                    <div className="file-icon-wrapper">
                        <File size={24} />
                    </div>
                    <div className="file-info-wrapper">
                        <div className="file-name">{t('file') || "File"}</div>
                        <div className="file-hint">{item.content}</div>
                    </div>
                </div>
            );
        }

        const fileName = filePath.split(/[\\/]/).pop();
        const dirPath = filePath.split(/[\\/]/).slice(0, -1).join('\\');

        return (
            <div className="file-thumbnail-card" title={item.content}>
                <div className={`file-icon-wrapper${fileIcon ? " file-icon-wrapper-system" : ""}`}>
                    {fileIcon ? (
                        <img
                            src={fileIcon}
                            alt={`${fileName || "file"} icon`}
                            className="file-icon-image"
                            loading="lazy"
                        />
                    ) : (
                        getFallbackFileIcon(filePath)
                    )}
                </div>
                <div className="file-info-wrapper">
                    <div className="file-name">{fileName}</div>
                    <div className="file-hint">{dirPath}</div>
                </div>
            </div>
        );
    };

    const renderTagsContainer = (overlay = false) => (
        <div
            className={`item-tags-container${overlay ? ' overlay' : ''}${isEditingTags ? ' tag-edit-active' : ''}`}
            style={{
                marginTop: overlay ? '0' : '2px',
                display: 'flex',
                flexWrap: 'wrap',
                justifyContent: 'flex-end',
                gap: '4px',
                paddingTop: '0'
            }}
        >
            {item.tags?.map((tag) => {
                const tagBackground = tagColors?.[tag] || getTagColor(tag, theme);
                const tagTextColor = getTagTextColor(tagBackground);
                return (
                    <span
                        key={tag}
                        className="tag-chip"
                        style={{
                            background: tagBackground,
                            color: tagTextColor,
                            display: 'flex',
                            alignItems: 'center',
                            gap: '4px'
                        }}
                    >
                        {tag}
                        {isEditingTags && (
                            <button
                                onClick={(e) => {
                                    e.stopPropagation();
                                    onTagDelete(tag);
                                }}
                                title={t('remove_tag')}
                                style={{ background: 'none', border: 'none', padding: 0, color: 'inherit', opacity: 0.72, cursor: 'pointer', display: 'flex' }}
                            >
                                <X size={8} />
                            </button>
                        )}
                    </span>
                );
            })}

            {isEditingTags && (
                <div className="tag-edit-anchor">
                    <div className="tag-edit-input-row">
                        <input
                            ref={tagInputRef}
                            type="text"
                            value={localTagInput}
                            onCompositionStart={() => {
                                isComposing.current = true;
                            }}
                            onCompositionEnd={(e) => {
                                isComposing.current = false;
                                const val = (e.target as HTMLInputElement).value;
                                setLocalTagInput(val);
                                onTagInput(val);
                            }}
                            onMouseDown={() => {
                                invoke('activate_window_focus').catch(console.error);
                            }}
                            onFocus={() => {
                                invoke('activate_window_focus').catch(console.error);
                            }}
                            onChange={(e) => {
                                const val = e.target.value;
                                setLocalTagInput(val);
                                if (!isComposing.current) {
                                    onTagInput(val);
                                }
                            }}
                            onKeyDown={(e) => {
                                if (e.key === 'Escape') {
                                    e.preventDefault();
                                    e.stopPropagation();
                                    onTagEditCancel?.();
                                    return;
                                }
                                const suggestionCount = pickableTagSuggestions.length;
                                if (e.key === 'ArrowDown' && suggestionCount > 0 && onTagPick) {
                                    e.preventDefault();
                                    e.stopPropagation();
                                    setTagSuggestIndex((prev) =>
                                        prev < 0 ? 0 : Math.min(prev + 1, suggestionCount - 1)
                                    );
                                    return;
                                }
                                if (e.key === 'ArrowUp' && suggestionCount > 0 && onTagPick) {
                                    e.preventDefault();
                                    e.stopPropagation();
                                    setTagSuggestIndex((prev) => (prev <= 0 ? -1 : prev - 1));
                                    return;
                                }
                                /*
                               * Tab：**选中当前高亮的候补**（用户明确提到"tab 键那种"）。
                               *
                               * 与 Enter 的区别是有意保留的：
                               * - Tab 只在**有候补且已高亮**时接管，否则放行让焦点正常移动
                               *   （否则用户没法用 Tab 离开这个输入框）
                               * - Enter 在无候选时是"提交当前输入"（新增标签），有候选时选中候选
                               *
                               * 默认高亮第 0 项（见下方 `tagSuggestIndex` 的初始化逻辑）——
                               * 否则用户打完字按 Tab 什么都不会发生，"tab 补全"就无从谈起。
                               */
                              if (e.key === 'Tab' && !e.shiftKey && !isComposing.current) {
                                  if (
                                      suggestionCount > 0 &&
                                      onTagPick &&
                                      tagSuggestIndex >= 0 &&
                                      tagSuggestIndex < suggestionCount
                                  ) {
                                      e.preventDefault();
                                      e.stopPropagation();
                                      const picked = pickableTagSuggestions[tagSuggestIndex];
                                      onTagPick(picked);
                                      setLocalTagInput('');
                                      setTagSuggestIndex(-1);
                                  }
                                  // 无候补时不 preventDefault：让 Tab 正常移出焦点。
                                  return;
                              }
                              if (e.key === 'Enter' && !isComposing.current) {
                                    e.preventDefault();
                                    e.stopPropagation();
                                    if (
                                        onTagPick &&
                                        tagSuggestIndex >= 0 &&
                                        tagSuggestIndex < suggestionCount
                                    ) {
                                        const picked = pickableTagSuggestions[tagSuggestIndex];
                                        onTagPick(picked);
                                        setLocalTagInput('');
                                        setTagSuggestIndex(-1);
                                    } else {
                                        onTagAdd();
                                    }
                                }
                            }}
                            className="tag-input"
                            aria-autocomplete="list"
                            aria-controls={
                                pickableTagSuggestions.length > 0 && onTagPick
                                    ? `tag-suggest-list-${item.id}`
                                    : undefined
                            }
                            aria-activedescendant={
                                tagSuggestIndex >= 0 && pickableTagSuggestions.length > 0 && onTagPick
                                    ? `tag-suggest-opt-${item.id}-${tagSuggestIndex}`
                                    : undefined
                            }
                            placeholder={t('enter_tag_name')}
                            style={{
                                background: 'var(--bg-input)',
                                border: 'none',
                                borderRadius: '0',
                                padding: '2px 6px',
                                fontSize: '10px',
                                color: 'var(--text-primary)',
                                outline: 'none'
                            }}
                            onClick={(e) => e.stopPropagation()}
                        />
                        <button
                            type="button"
                            onClick={(e) => {
                                e.stopPropagation();
                                onTagAdd();
                            }}
                            className="btn-icon"
                            title={t('add_tag')}
                            style={{ padding: '2px', height: '16px', width: '16px' }}
                        >
                            <Plus size={10} />
                        </button>
                    </div>
                    {pickableTagSuggestions.length > 0 && onTagPick && (
                        <div
                            ref={tagSuggestListRef}
                            id={`tag-suggest-list-${item.id}`}
                            className="tag-edit-suggestions-popover"
                              /*
                               * 把"最多几行"从 JS 侧传给 CSS，让行数只有一个来源。
                               *
                               * CSS 的 `max-height: calc(rows × 行高)` 需要知道行数；
                               * 若 CSS 再自己写一遍 4，改一处忘一处就会让"JS 限制的条数"
                               * 与"CSS 限制的高度"分叉 —— 那正是本次要修的那类缺陷。
                               */
                              style={{ ["--tag-suggest-rows" as string]: String(TAG_SUGGEST_VISIBLE_ROWS) }}
                            role="listbox"
                            aria-label={t('find_tags')}
                            onMouseDown={(e) => e.stopPropagation()}
                        >
                            {pickableTagSuggestions.map((sTag, sIdx) => {
                                const bg = tagColors?.[sTag] || getTagColor(sTag, theme);
                                const fg = getTagTextColor(bg);
                                return (
                                    <button
                                        key={sTag}
                                        type="button"
                                        role="option"
                                        id={`tag-suggest-opt-${item.id}-${sIdx}`}
                                        aria-selected={tagSuggestIndex === sIdx}
                                        className={`tag-suggest-item${tagSuggestIndex === sIdx ? ' tag-suggest-item--active' : ''}`}
                                        onMouseEnter={() => setTagSuggestIndex(sIdx)}
                                        onClick={(e) => {
                                            e.stopPropagation();
                                            onTagPick(sTag);
                                            setLocalTagInput('');
                                            setTagSuggestIndex(-1);
                                        }}
                                    >
                                        <span
                                            className="tag-suggest-pill"
                                            style={{
                                                background: bg,
                                                color: fg
                                            }}
                                        >
                                            {sTag}
                                        </span>
                                    </button>
                                );
                            })}
                        </div>
                    )}
                </div>
            )}
        </div>
    );

    /**
     * R6: show the entry note on the clipboard main page when it is not empty.
     *
     * The inline text is cut at `NOTE_INLINE_MAX_CHARS` and the untruncated note is
     * carried in `title`, so a 2000-character note stays readable without ever
     * stretching the row.
     *
     * Compact mode keeps this in normal flow rather than as an overlay. An absolutely
     * positioned chip was tried first (mirroring the tag strip) and measured to sit on
     * top of the second line of content, hiding it — worse than the height it saves.
     * In flow it costs ~13px, which stays below the ~40px a two-line compact row already
     * occupies: compact rows were never fixed-height, so density holds and nothing is
     * covered.
     *
     * The remaining collision is the compact tag strip, which is anchored to the
     * bottom-right of the row. The note therefore reserves that side for itself in
     * compact mode (`paddingRight`, `textAlign`) so the two never overlap.
     */
    const renderNote = () => {
        const isCompactNote = !!compactMode;
        // The compact tag strip is absolutely positioned over the row's bottom-right
        // corner, so the note must not run under it. `maxWidth` on the text element is
        // what actually bounds it — a flex container's `paddingRight` is not honoured
        // as a reserve once the child is allowed to grow.
        const compactNoteReservesTags = isCompactNote && visibleTagCount > 0;
        return (
            <div
                className="entry-note-row"
                title={noteText}
                style={{
                    display: 'flex',
                    alignItems: isCompactNote ? 'center' : 'flex-start',
                    gap: isCompactNote ? '3px' : '4px',
                    marginTop: isCompactNote ? '0' : '2px',
                    minWidth: 0,
                    fontSize: '10px',
                    lineHeight: isCompactNote ? 1.3 : 1.4,
                    color: 'var(--text-secondary)',
                    opacity: 0.9
                }}
            >
                <Sparkles
                    size={10}
                    className="entry-note-sparkle"
                    style={{ flexShrink: 0, marginTop: isCompactNote ? 0 : '2px' }}
                />
                <span
                    className="entry-note-text"
                    style={{
                        minWidth: 0,
                        maxWidth: compactNoteReservesTags ? '48%' : '100%',
                        overflow: 'hidden',
                        textOverflow: 'ellipsis',
                        whiteSpace: 'nowrap'
                    }}
                >
                    {truncateNoteForInline(noteText)}
                </span>
            </div>
        );
    };

    /**
     * R10: close the body editor on Escape, from anywhere.
     *
     * This has to live on `window` rather than on the dialog element. The dialog is
     * portalled and its content can lose focus (clicking the heading, or any
     * non-focusable area, leaves `activeElement` on <body>), and a handler bound to the
     * overlay then never sees the key at all. The capture phase is deliberate: it runs
     * before the app's global navigation hook, which would otherwise read Escape as
     * "hide the window" and leave the dialog orphaned on a hidden window.
     */
    useEffect(() => {
        if (!bodyEditorOpen) return;
        const onKey = (e: KeyboardEvent) => {
            if (e.key === 'Escape') {
                e.stopPropagation();
                e.preventDefault();
                onBodyEditCancel?.();
            }
        };
        window.addEventListener('keydown', onKey, true);
        return () => window.removeEventListener('keydown', onKey, true);
    }, [bodyEditorOpen, onBodyEditCancel]);

    /**
     * R10: body editor.
     *
     * Portalled to <body> so it is not trapped by the virtual list's transform/overflow
     * or by the item's hover styles. Only rendered when the renderer hook supplied
     * `onBodyEditSave`, which it does only for text-like content types.
     */
    const renderBodyEditor = () => {
        if (!bodyEditorOpen || !onBodyEditSave) return null;

        return createPortal(
            <div
                className={`modal-overlay theme-${theme}`}
                onClick={() => onBodyEditCancel?.()}
                onMouseDown={(e) => e.stopPropagation()}
                onContextMenu={(e) => e.stopPropagation()}
            >
                <div
                    className="confirm-dialog entry-body-editor-dialog"
                    onClick={(e) => e.stopPropagation()}
                    style={{ maxWidth: '520px', width: '100%' }}
                >
                    {/* TODO(i18n): 文案暂硬编码，待 locales.ts 统一收纳 */}
                    <h3 style={{ margin: '0 0 12px 0', fontSize: '15px', fontWeight: 600 }}>
                        {t('edit_item') || '编辑条目内容'}
                    </h3>
                    {bodyEditIsRich ? (
                        /*
                         * R13：富文本条目用 contentEditable 编辑，保存时读回 `innerHTML`。
                         *
                         * 这一块是"编辑富文本不会坍缩成纯文本"在界面上的落点：
                         *  - 初值是 `bodyInitialHtml`（HTML，经 sanitizeHTML 洗过），
                         *    而不是纯文本列 `item.content`；
                         *  - 保存时送 `innerHTML`，与正文一起提交给后端，
                         *    后端保持 `content_type = rich_text` 并写入 `html_content`。
                         *
                         * 为什么用 `dangerouslySetInnerHTML` 之外的原生写法：React 的
                         * 受控组件模型和 contentEditable 天生冲突（每次按键都由 React
                         * 重写 DOM 会把光标推到开头）。这里走"挂载时写一次初值、之后
                         * 只读取"的非受控方式，光标与输入法正常。
                         */
                        <div
                            ref={bodyEditorRichRef}
                            className="entry-body-editor-textarea entry-body-editor-rich"
                            contentEditable
                            suppressContentEditableWarning
                            autoFocus
                            role="textbox"
                            aria-multiline="true"
                            data-testid="entry-body-editor-rich"
                            onMouseDown={() => invoke('activate_window_focus').catch(console.error)}
                            onFocus={() => invoke('activate_window_focus').catch(console.error)}
                            onInput={(e) => {
                                // 草稿存**纯文本**（= 最终会写进 `content` 的东西），
                                // HTML 在保存的那一刻从 DOM 读。这样两处不变量都成立：
                                // 用户看到的正文 == 落库的 content；格式 == innerHTML。
                                setBodyDraft(htmlToPlainText((e.target as HTMLElement).innerHTML));
                            }}
                            onKeyDown={(e) => {
                                e.stopPropagation();
                                if (e.key === 'Escape') {
                                    e.preventDefault();
                                    onBodyEditCancel?.();
                                    return;
                                }
                                // Ctrl/Cmd+Enter 保存；纯 Enter 在富文本里是"换行/分段"，
                                // 与用户的直觉一致（textarea 分支同理）。
                                if (e.key === 'Enter' && (e.ctrlKey || e.metaKey) && !bodyEditSaving) {
                                    e.preventDefault();
                                    onBodyEditSave(bodyDraft, bodyEditorRichRef.current?.innerHTML ?? bodyDraft);
                                }
                            }}
                            style={{
                                width: '100%',
                                minHeight: '132px',
                                maxHeight: '46vh',
                                overflowY: 'auto',
                                marginBottom: '12px',
                                padding: '12px',
                                border: 'var(--input-border)',
                                borderRadius: 'var(--input-radius)',
                                background: 'var(--bg-input)',
                                boxShadow: 'var(--input-shadow)',
                                color: 'var(--text-primary)',
                                fontFamily: 'inherit',
                                fontSize: '13px',
                                lineHeight: 1.55,
                                outline: 'none',
                                boxSizing: 'border-box',
                                wordBreak: 'break-word'
                            }}
                        />
                    ) : (
                        <textarea
                            ref={bodyEditorTextareaRef}
                            className="entry-body-editor-textarea"
                            autoFocus
                            value={bodyDraft}
                            onMouseDown={() => invoke('activate_window_focus').catch(console.error)}
                            onFocus={() => invoke('activate_window_focus').catch(console.error)}
                            onChange={(e) => setBodyDraft(e.target.value)}
                            onKeyDown={(e) => {
                                e.stopPropagation();
                                if (e.key === 'Escape') {
                                    e.preventDefault();
                                    onBodyEditCancel?.();
                                    return;
                                }
                                // Ctrl/Cmd+Enter saves, matching the muscle memory of the
                                // tag manager's editor while plain Enter stays a newline.
                                if (e.key === 'Enter' && (e.ctrlKey || e.metaKey) && !bodyEditSaving) {
                                    e.preventDefault();
                                    onBodyEditSave(bodyDraft);
                                }
                            }}
                            style={{
                                width: '100%',
                                minHeight: '132px',
                                marginBottom: '12px',
                                padding: '12px',
                                border: 'var(--input-border)',
                                borderRadius: 'var(--input-radius)',
                                background: 'var(--bg-input)',
                                boxShadow: 'var(--input-shadow)',
                                color: 'var(--text-primary)',
                                fontFamily: 'inherit',
                                fontSize: '13px',
                                lineHeight: 1.55,
                                outline: 'none',
                                resize: 'vertical',
                                boxSizing: 'border-box'
                            }}
                        />
                    )}
                    {bodyEditError && (
                        <div
                            className="entry-body-editor-error"
                            style={{ marginBottom: '10px', fontSize: '12px', color: 'var(--accent-color)' }}
                        >
                            {bodyEditError}
                        </div>
                    )}
                    <div className="confirm-dialog-buttons">
                        <button
                            className="confirm-dialog-button"
                            disabled={bodyEditSaving}
                            onClick={() => onBodyEditCancel?.()}
                        >
                            {t('cancel')}
                        </button>
                        <button
                            className="confirm-dialog-button primary"
                            disabled={bodyEditSaving}
                            onClick={() => {
                                // R13：富文本条目保存时读回编辑器的 `innerHTML`，
                                // 与正文一起提交 —— 只送纯文本就等于把格式丢掉。
                                if (bodyEditIsRich) {
                                    onBodyEditSave(
                                        bodyDraft,
                                        bodyEditorRichRef.current?.innerHTML ?? bodyDraft
                                    );
                                    return;
                                }
                                onBodyEditSave(bodyDraft);
                            }}
                        >
                            {t('save')}
                        </button>
                    </div>
                </div>
            </div>,
            document.body
        );
    };

    /**
     * R11: close the note editor on Escape, from anywhere.
     *
     * Deliberately the same mechanism as the body editor above, for the same reasons:
     * the dialog is portalled and focus can land on <body>, so a handler on the overlay
     * would miss the key; and the capture phase is required because the app's global
     * navigation hook reads Escape as "hide the window". The two editors are mutually
     * exclusive per row, and each binds only while its own dialog is open.
     */
    useEffect(() => {
        if (!noteEditorOpen) return;
        const onKey = (e: KeyboardEvent) => {
            if (e.key === 'Escape') {
                e.stopPropagation();
                e.preventDefault();
                onNoteEditCancel?.();
            }
        };
        window.addEventListener('keydown', onKey, true);
        return () => window.removeEventListener('keydown', onKey, true);
    }, [noteEditorOpen, onNoteEditCancel]);

    /**
     * R11: note-only editor for content types whose body is not editable text.
     *
     * It reuses the body editor's portal, overlay and Escape/backdrop machinery but
     * renders **no body field at all** — showing a textarea the back end would refuse to
     * write would invite the user to lose work. Only rendered when the renderer hook
     * supplied `onNoteEditSave`, which it does only for `image` / `file` / `video`.
     */
    const renderNoteEditor = () => {
        if (!noteEditorOpen || !onNoteEditSave) return null;
        const noteCharCount = Array.from(noteDraft).length;

        return createPortal(
            <div
                className={`modal-overlay theme-${theme}`}
                onClick={() => onNoteEditCancel?.()}
                onMouseDown={(e) => e.stopPropagation()}
                onContextMenu={(e) => e.stopPropagation()}
            >
                <div
                    className="confirm-dialog entry-note-editor-dialog"
                    onClick={(e) => e.stopPropagation()}
                    style={{ maxWidth: '520px', width: '100%' }}
                >
                    {/* TODO(i18n): 文案暂硬编码，待 locales.ts 统一收纳 */}
                    <h3 style={{ margin: '0 0 12px 0', fontSize: '15px', fontWeight: 600 }}>
                        编辑备注
                    </h3>
                    <textarea
                        className="entry-note-editor-textarea"
                        autoFocus
                        value={noteDraft}
                        maxLength={MAX_ENTRY_NOTE_CHARS}
                        placeholder="为这条记录添加备注（可留空）"
                        onMouseDown={() => invoke('activate_window_focus').catch(console.error)}
                        onFocus={() => invoke('activate_window_focus').catch(console.error)}
                        onChange={(e) => setNoteDraft(e.target.value)}
                        onKeyDown={(e) => {
                            e.stopPropagation();
                            if (e.key === 'Escape') {
                                e.preventDefault();
                                onNoteEditCancel?.();
                                return;
                            }
                            // Ctrl/Cmd+Enter saves, matching the body editor and the tag
                            // manager; plain Enter stays a newline.
                            if (e.key === 'Enter' && (e.ctrlKey || e.metaKey) && !noteEditSaving) {
                                e.preventDefault();
                                onNoteEditSave(noteDraft);
                            }
                        }}
                        style={{
                            width: '100%',
                            minHeight: '96px',
                            marginBottom: '8px',
                            padding: '12px',
                            border: 'var(--input-border)',
                            borderRadius: 'var(--input-radius)',
                            background: 'var(--bg-input)',
                            boxShadow: 'var(--input-shadow)',
                            color: 'var(--text-primary)',
                            fontFamily: 'inherit',
                            fontSize: '13px',
                            lineHeight: 1.55,
                            outline: 'none',
                            resize: 'vertical',
                            boxSizing: 'border-box'
                        }}
                    />
                    <div
                        className="entry-note-editor-meta"
                        style={{
                            display: 'flex',
                            alignItems: 'center',
                            justifyContent: 'space-between',
                            gap: '8px',
                            marginBottom: '10px',
                            fontSize: '11px',
                            color: 'var(--text-secondary)'
                        }}
                    >
                        <span>清空输入框即可删除备注</span>
                        <span>{noteCharCount} / {MAX_ENTRY_NOTE_CHARS}</span>
                    </div>
                    {noteEditError && (
                        <div
                            className="entry-note-editor-error"
                            style={{ marginBottom: '10px', fontSize: '12px', color: 'var(--accent-color)' }}
                        >
                            {noteEditError}
                        </div>
                    )}
                    <div className="confirm-dialog-buttons">
                        <button
                            className="confirm-dialog-button"
                            disabled={noteEditSaving}
                            onClick={() => onNoteEditCancel?.()}
                        >
                            {t('cancel')}
                        </button>
                        <button
                            className="confirm-dialog-button primary"
                            disabled={noteEditSaving}
                            onClick={() => onNoteEditSave(noteDraft)}
                        >
                            {t('save')}
                        </button>
                    </div>
                </div>
            </div>,
            document.body
        );
    };

    return (
        <motion.div
            ref={itemRef}
            id={id}
            data-test-clipboard-item
            layout={!disableLayout}
            initial={false}
            animate={{ marginBottom: 0 }}
            exit={{ opacity: 0, scale: 0.95 }}
            transition={{ duration: 0.1 }}
            className={`history-item ${isSelected ? "selected" : ""} ${compactMode ? "compact" : ""} ${item.is_pinned ? "pinned" : ""} ${className || ''}`}
            onMouseDown={(e) => {
                const target = e.target as HTMLElement;
                if (e.button !== 0) return;

                if (isEditingTags) {
                    if (target.closest(".tag-edit-anchor")) return;
                    if (target.closest(".item-tags-container .tag-chip")) return;
                    if (target.closest('button, input, textarea, [role="button"], .drag-handle')) {
                        return;
                    }
                    if (target.closest('a')) return;
                    e.preventDefault();
                    e.stopPropagation();
                    onTagEditCancel?.();
                    return;
                }

                if (target.closest('button, input, textarea, [role="button"], .drag-handle')) {
                    return;
                }
                if (target.closest('a')) {
                    return;
                }
                // e.preventDefault() stops macOS from transferring key-window focus to Tiez-Next
                // when the user clicks on a clipboard item, including pinned mode.
                // Without this, the first click activates Tiez-Next and the original input
                // target loses focus before we dispatch the paste keystroke.
                e.preventDefault();
                void hideCompactPreview();
                onCopy(false); // Plain text by default
                onSelect();
            }}
            onClick={(e) => {
                if (isEditingTags) return;
                const target = e.target as HTMLElement;
                if (target.closest('button') || target.closest('input') || target.closest('textarea')) {
                    return;
                }
                // Prevent link navigation - we want to copy, not open links
                if (target.closest('a')) {
                    e.preventDefault();
                }
                // Copy is handled on mousedown so the source app keeps focus.
            }}
            onContextMenu={(e) => {
                const target = e.target as HTMLElement;
                if (isEditingTags) {
                    if (target.closest(".tag-edit-anchor")) return;
                    e.preventDefault();
                    e.stopPropagation();
                    onTagEditCancel?.();
                    return;
                }
                if (target.closest('button') || target.closest('input') || target.closest('textarea')) {
                    return;
                }
                void hideCompactPreview();
                e.preventDefault();
                // Prevent link navigation on right-click too
                if (target.closest('a')) {
                    e.stopPropagation();
                }
                onCopy(true); // Formatted text for right-click

                onSelect();
            }}
            onMouseEnter={(e) => {
                if (!compactPreviewEnabled) return;
                // Don't show preview if AI options are open to avoid interference
                if (showAIOptions) return;
                compactPreviewLog("mouseenter schedule preview", { itemId: item.id });
                const requestId = hoverRequestIdRef.current + 1;
                hoverRequestIdRef.current = requestId;
                hoverAnchorRef.current = {
                    clientX: e.clientX,
                    clientY: e.clientY,
                    screenX: e.screenX,
                    screenY: e.screenY
                };
                const target = e.currentTarget;

                // Clear any pending hide timer
                if (hoverTimerRef.current) clearTimeout(hoverTimerRef.current);

                // Set a delay to show
                hoverTimerRef.current = setTimeout(() => {
                    hoverTimerRef.current = null;
                    // Double-check AI options are still closed before showing
                    if (showAIOptions) return;
                    if (!target.isConnected) return;
                    if (!isHoverPreviewRequestCurrent(requestId)) return;
                    const anchor = hoverAnchorRef.current;
                    if (!anchor) return;
                    compactPreviewLog("mouseenter timer fired, show preview", { itemId: item.id });
                    void showCompactPreview(anchor, requestId);
                }, 1000); // 1s delay
            }}
            onMouseMove={(e) => {
                if (!compactPreviewEnabled) return;
                hoverAnchorRef.current = {
                    clientX: e.clientX,
                    clientY: e.clientY,
                    screenX: e.screenX,
                    screenY: e.screenY
                };
            }}
            onMouseLeave={() => {
                compactPreviewLog("mouseleave hide preview", { itemId: item.id });
                void hideCompactPreview();
            }}
        >
            <div className="item-meta">
                <div className="item-meta-left">
                    {dragControls && (
                        <div
                            className="drag-handle"
                            onPointerDown={(e) => dragControls.start(e)}
                            onClick={(e) => e.stopPropagation()}
                            title={t('tooltip_drag_handle')}
                            style={{
                                cursor: 'grab',
                                opacity: 0.5,
                                display: 'flex',
                                alignItems: 'center',
                                touchAction: 'none'
                            }}
                        >
                            <GripVertical size={14} />
                        </div>
                    )}
                    <div className="app-info">
                        {item.is_pinned && !dragControls && <Pin size={10} style={{ color: 'var(--accent-color)', marginRight: '-2px' }} />}
                        {showSourceAppIcon
                            ? renderSourceAppIcon(sourceAppIcon, item.content_type, item.source_app)
                            : getIcon(item.content_type)}
                        <span>{item.source_app}</span>
                    </div>
                </div>

                <div className="item-meta-right">
                    <div className="item-actions">
                        {(item.tags?.includes('sensitive') || item.tags?.includes('密码') || item.tags?.includes('password')) && (
                            <button
                                className={`btn-icon ${isRevealed ? "active" : ""}`}
                                onClick={onToggleReveal}
                                title={isRevealed ? t('hide') : t('reveal')}
                            >
                                {isRevealed ? <EyeOff size={12} /> : <Eye size={12} />}
                            </button>
                        )}
                        {onEdit && (
                            <button
                                className={`btn-icon ${bodyEditorOpen ? "active" : ""}`}
                                onClick={(e) => {
                                    e.stopPropagation();
                                    onEdit(e);
                                }}
                                title={t('edit_item') || '编辑条目内容'}
                            >
                                <Pencil size={12} />
                            </button>
                        )}
                        {isNoteEditable(item.content_type) && onEditNote && (
                            <button
                                className={`btn-icon note-edit-btn entry-note-mark ${noteEditorOpen ? "active" : ""}`}
                                onClick={(e) => {
                                    e.stopPropagation();
                                    onEditNote(e);
                                }}
                                title={t('edit_item_note_label')}
                            >
                                {/*
                                  * 图标与备注行前面那个 ✨ **是同一个**（`Sparkles`）。
                                  *
                                  * 用户的要求是"编辑备注的图标要和备注前面那个 ✨ 图标一样"。
                                  * 此前这里是 `StickyNote`（便利贴），与备注在界面上的标识符
                                  * 毫无关联 —— 用户只能靠 tooltip 才知道这个按钮是干什么的。
                                  * 两处统一之后，"✨ = 备注"成了这个界面的固定符号。
                                  */}
                                {/*
                                  * 【尺寸与备注行的 ✨ 严格一致（都是 10）】
                                  *
                                  * 此前这里是 12、备注行是 10 —— 用户的原话是
                                  * "备注的图标应该是和这个 ✨ 要**百分百一致**不管是颜色还是形状"。
                                  * 两个同款图标只差 2px 时，并排看过去就是"不一样"，而这种
                                  * 不一致没有任何设计理由支撑（它只是两处各写各的默认值造成的）。
                                  *
                                  * 颜色由 `.note-edit-btn` 给（与 `.entry-note-sparkle`
                                  * 用同一个令牌），见 clipboard-item.css。
                                  */}
                                <Sparkles size={10} />
                            </button>
                        )}
                        <button
                            className="btn-icon"
                            onClick={onOpen}
                            title={t('open')}
                        >
                            <ExternalLink size={12} />
                        </button>
                        <button
                            className={`btn-icon ${item.is_pinned ? "active" : ""}`}
                            onClick={onTogglePin}
                            title={item.is_pinned ? t('unpin') : t('pin')}
                        >
                            {item.is_pinned ? <PinOff size={12} /> : <Pin size={12} />}
                        </button>
                        <button
                            className={`btn-icon ${item.tags && item.tags.length > 0 ? "active" : ""}`}
                            onClick={onToggleTagEditor}
                            title={t('tags')}
                        >
                            <Tag size={12} />
                        </button>
                        {/* v0.5 需求⑨：移动到标签 / 复制到标签。入口与"标签编辑"分开，
                            因为它改的是标签的归属，而不是标签的增删。 */}
                        <button
                            className={`btn-icon ${isTagAssignOpen ? "active" : ""}`}
                            onClick={(e) => {
                                e.stopPropagation();
                                void hideCompactPreview();
                                setIsTagAssignOpen(true);
                            }}
                            title={t('tag_transfer_action') || '移动到标签 / 复制到标签'}
                        >
                            <FolderInput size={12} />
                        </button>
                        {(item.content_type === 'text' || item.content_type === 'rich_text') && aiEnabled && (
                            <button
                                className={`btn-icon ai-btn ${isAIProcessing || showAIOptions ? 'active' : ''}`}
                                onClick={(e) => {
                                    e.stopPropagation();
                                    if (!isAIProcessing) {
                                        // Close preview window when opening AI options
                                        if (!showAIOptions) {
                                            hideCompactPreview();
                                        }
                                        setLocalAiOptionsOpen(prev => !prev);
                                        onAIOptionsToggle?.();
                                    }
                                }}
                                title={t('ai_assistant')}
                            >
                                {isAIProcessing ? <Loader2 size={12} className="animate-spin" /> : <Sparkles size={12} />}
                            </button>
                        )}
                        <button className="btn-icon" onClick={onDelete} title={t('delete')}>
                            <X size={12} />
                        </button>
                    </div>
                    <div className="item-meta-right-info">
                        {quickPasteHint && item.is_pinned && (
                            <span
                                className="quick-paste-hint"
                                title={`${t('quick_paste_modifier')}: ${quickPasteHint.combo}`}
                            >
                                {quickPasteHint.combo}
                            </span>
                        )}
                        <span>{getConciseTime(item.timestamp, language)}</span>
                    </div>
                </div>
            </div>

            {
                compactMode && item.is_pinned && (
                    <div className="compact-pinned-indicator" title={t('pinned')}>
                        <Pin size={10} fill="currentColor" />
                    </div>
                )
            }
            <div className={`content-preview-shell${overlayTagsInPreview ? ' has-overlay-tags' : ''}`}>
                <div className={`content-preview ${item.content_type === 'rich_text' ? 'rich-text' : ''} ${item.content_type === 'file' ? 'file-preview' : ''} ${isSensitiveHidden ? 'sensitive-blur' : ''}`}>
                {item.content_type === "image" ? (
                    <div style={{ position: 'relative' }}>
                        {item.is_external && item.file_preview_exists === false ? (
                            // 图片已失效时的占位块。底色用 `--bg-input`：与条目里其它
                            // "需要从卡片底上再抬起一档"的内嵌表面（`.video-file-card`、
                            // 标签芯片）同一令牌，因此占位块在六套主题下都跟随主题。
                            // 原先写的是从未定义的 `--bg-secondary`，整条 background 被
                            // 丢弃，占位块一直是透明的——而这个块平时看不见（只在图片
                            // 文件丢失时出现），所以没人察觉。
                            <div className="image-preview error-placeholder" style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', justifyContent: 'center', background: 'var(--bg-input)', color: 'var(--text-secondary)', height: '100px', fontSize: '12px' }}>
                                <ImageOff size={24} style={{ marginBottom: '8px', opacity: 0.5 }} />
                                <span>{t('image_deleted') || 'Image Deleted'}</span>
                            </div>
                        ) : (
                            <img
                                src={
                                    item.content.startsWith("data:")
                                        ? item.content
                                        : (
                                            toTauriLocalImageSrc(item.content) ||
                                            (item.is_external ? convertFileSrc(item.content) : item.content)
                                        )
                                }
                                alt={t('image_preview')}
                                className="image-preview"
                                loading="lazy"
                                style={isSensitiveHidden ? { filter: 'blur(8px)' } : {}}
                                onError={(e) => {
                                    // Fallback for load errors even if backend said it exists (e.g. deleted after fetch)
                                    e.currentTarget.style.display = 'none';
                                    e.currentTarget.parentElement?.classList.add('image-load-error');
                                }}
                            />
                        )}
                        {isSensitiveHidden && (
                            <div style={{ position: 'absolute', top: '50%', left: '50%', transform: 'translate(-50%, -50%)', fontWeight: 'bold', opacity: 0.5, fontSize: '10px' }}>
                                SENSITIVE
                            </div>
                        )}
                    </div>
                ) : item.content_type === "video" ? (
                    <div className="video-thumbnail-card">
                        <div className="video-thumbnail-wrapper">
                            <video
                                src={item.content.startsWith("data:")
                                    ? item.content
                                    : (toTauriLocalImageSrc(item.content) || item.content)}
                                preload="metadata"
                                muted
                                playsInline
                                className="video-thumbnail-element"
                                onLoadedMetadata={(e) => seekVideoPreviewFrame(e.currentTarget)}
                            />
                            <div className="video-play-overlay">
                                <Video size={16} />
                            </div>
                        </div>
                        <div className="video-info-wrapper">
                            <div className="video-name">{item.content.split(/[\\/]/).pop()}</div>
                        </div>
                    </div>
                ) : item.content_type === "file" ? (
                    renderFilePreview()
                ) : isAIProcessing ? (
                    <div className="ai-skeleton-wrapper">
                        <div className="ai-skeleton-line" style={{ width: '90%' }}></div>
                        <div className="ai-skeleton-line" style={{ width: '75%' }}></div>
                        <div className="ai-skeleton-line" style={{ width: '85%' }}></div>
                    </div>
                ) : item.isInputting ? (
                    <div className="ai-input-wrapper">
                        <input
                            autoFocus
                            className="search-input"
                            onMouseDown={() => invoke('activate_window_focus').catch(console.error)}
                            onFocus={() => invoke('activate_window_focus').catch(console.error)}
                            style={{ width: '100%', fontSize: '12px', padding: '8px', border: '1px solid var(--accent-color)' }}
                            placeholder={item.content}
                            onKeyDown={(e) => {
                                if (e.key === 'Enter') {
                                    e.preventDefault();
                                    const val = e.currentTarget.value.trim();
                                    if (onInputSubmit) {
                                        onInputSubmit(val);
                                    }
                                }
                            }}
                            onClick={(e) => e.stopPropagation()}
                        />
                        <div style={{ fontSize: '10px', opacity: 0.6, marginTop: '4px' }}>
                            {language === 'zh' ? '输入补充信息后按回车提交' : 'Press Enter to submit supplementary info'}
                        </div>
                    </div>
                ) : item.content_type === "rich_text" && item.html_content && !isSensitiveHidden ? (
                    richTextPreviewSrc ? (
                        <img
                            ref={richSnapshotImgRef}
                            src={richTextPreviewSrc}
                            alt="rich text preview"
                            onLoad={() => {
                                if (useSnapshotPreviewImage && richSnapshotFallbackTimerRef.current) {
                                    clearTimeout(richSnapshotFallbackTimerRef.current);
                                    richSnapshotFallbackTimerRef.current = null;
                                }
                            }}
                            onError={() => {
                                if (useRichImageFallback) {
                                    richPreviewFailureLog("fallback image load error -> switch to snapshot", {
                                        itemId: item.id,
                                        srcLength: (richTextPreviewSrc || "").length,
                                        srcSample: (richTextPreviewSrc || "").slice(0, 140)
                                    });
                                    setRichImageFallbackFailed(true);
                                    return;
                                }
                                if (richSnapshotFallbackTimerRef.current) {
                                    clearTimeout(richSnapshotFallbackTimerRef.current);
                                    richSnapshotFallbackTimerRef.current = null;
                                }
                                if (effectiveRichTextSnapshotSrc) {
                                    richPreviewFailureLog("snapshot image load error -> fallback to html", {
                                        itemId: item.id,
                                        srcLength: (richTextPreviewSrc || "").length,
                                        srcSample: (richTextPreviewSrc || "").slice(0, 140)
                                    });
                                    setSnapshotFailed(true);
                                }
                            }}
                            style={{
                                width: 'auto',
                                maxWidth: '100%',
                                maxHeight: `${richTextSnapshotDisplayMaxHeight}px`,
                                display: 'block',
                                marginRight: 'auto',
                                pointerEvents: 'none',
                                borderRadius: '4px',
                                maskImage: 'linear-gradient(to bottom, black 78%, transparent 100%)',
                                WebkitMaskImage: 'linear-gradient(to bottom, black 78%, transparent 100%)'
                            }}
                        />
                    ) : (
                        <HtmlContent
                            className="rich-text-preview"
                            htmlContent={richTextCleanHtml || item.html_content}
                            fallbackText={item.preview}
                            preview={true}
                            style={{
                                maxHeight: `${richTextSnapshotDisplayMaxHeight}px`,
                                overflow: 'hidden',
                                fontSize: 'var(--clipboard-item-font-size)',
                                lineHeight: '1.4',
                                position: 'relative',
                                pointerEvents: 'none', // Prevent interacting with links in the list
                                maskImage: 'linear-gradient(to bottom, black 70%, transparent 100%)',
                                WebkitMaskImage: 'linear-gradient(to bottom, black 70%, transparent 100%)'
                            }}
                        />
                    )
                ) : standaloneColorValue && !isSensitiveHidden ? (
                    <div className="color-code-preview">
                        <span
                            className="color-code-swatch"
                            style={{ background: standaloneColorValue }}
                            aria-hidden="true"
                        />
                        <span className="color-code-value">{standaloneColorValue}</span>
                    </div>
                ) : (
                    isSensitiveHidden
                        ? (
                            <div style={{ minHeight: '24px', opacity: 0.6, fontStyle: 'italic', display: 'flex', alignItems: 'center', gap: '8px', fontFamily: 'var(--font-mono)' }}>
                                <span style={{ letterSpacing: '1px' }}>
                                    {sensitivePreview}
                                </span>
                                <span style={{ fontSize: '10px', opacity: 0.7 }}>
                                    ({item.content.length} {t('chars') || 'chars'})
                                </span>
                            </div>
                        )
                        : item.preview
                )}
                {overlayTagsInPreview && renderTagsContainer(true)}
                </div>
            </div>

            {/* AI Options - Compact Mode: Dropdown Panel, Normal Mode: Inline */}
            <AnimatePresence>
                {showAIOptions && (
                    <motion.div
                        className={compactMode ? "ai-options-dropdown" : ""}
                        initial={compactMode ? { opacity: 0, y: -10 } : "collapsed"}
                        animate={compactMode ? { opacity: 1, y: 0 } : "open"}
                        exit={compactMode ? { opacity: 0, y: -10 } : "collapsed"}
                        variants={compactMode ? undefined : inlineAiVariants}
                        transition={compactMode ? { duration: 0.16 } : { duration: 0.18 }}
                        style={compactMode ? {
                            position: 'absolute',
                            top: '100%',
                            right: '4px',
                            zIndex: 100000,
                            marginTop: '4px',
                            background: 'var(--bg-element)',
                            border: '2px solid var(--border-dark)',
                            borderRadius: '4px',
                            boxShadow: '4px 4px 0 0 var(--shadow-color)',
                            padding: '6px',
                            minWidth: '140px',
                            maxHeight: '200px',
                            overflowY: 'auto'
                        } : { overflow: 'hidden' }}
                    >
                        <div style={compactMode ? {
                            display: 'flex',
                            flexDirection: 'column',
                            gap: '4px'
                        } : {
                            padding: '8px 10px',
                            background: 'rgba(72, 123, 219, 0.05)',
                            border: '1.5px dashed var(--accent-color)',
                            borderRadius: '4px',
                            display: 'flex',
                            flexWrap: 'wrap',
                            gap: '6px',
                            alignItems: 'center'
                        }}>
                            {['task', 'mouthpiece', 'translate'].map(actionType => (
                                <button
                                    key={actionType}
                                    onClick={(e) => {
                                        e.stopPropagation();
                                        onAIAction?.(actionType);
                                        onAIOptionsToggle?.();
                                    }}
                                    className="btn-icon"
                                    style={compactMode ? {
                                        width: '100%',
                                        fontSize: '11px',
                                        height: '32px',
                                        boxShadow: '2px 2px 0 0 var(--shadow-color)',
                                        textTransform: 'none',
                                        justifyContent: 'flex-start',
                                        paddingLeft: '10px'
                                    } : {
                                        flex: 1,
                                        minWidth: '90px',
                                        fontSize: '11px',
                                        height: '32px',
                                        padding: '0 12px',
                                        boxShadow: '2px 2px 0 0 var(--shadow-color)',
                                        textTransform: 'none',
                                        whiteSpace: 'nowrap'
                                    }}
                                >
                                    {t(`ai_${actionType}`)}
                                </button>
                            ))}
                        </div>
                    </motion.div>
                )}
            </AnimatePresence>

            {!overlayTagsInPreview && hasTagsSection && renderTagsContainer()}
            {/* R6: note is shown for every content type, above the tag chips. */}
            {!noteIsEmpty && renderNote()}
            {renderBodyEditor()}
            {/* R11: binary rows get the note-only editor instead of the body one. */}
            {renderNoteEditor()}
            {/* v0.5 需求⑨：移动 / 复制到标签。组件自己 portal 到 <body>，因此不受
                虚拟列表的 transform 与溢出裁剪影响（与上面两个编辑器同款做法）。 */}
            {isTagAssignOpen && (
                <TagAssignMenu
                    entryId={item.id}
                    tags={item.tags || []}
                    allTags={allTagNames ?? tagSuggestions}
                    tagColors={tagColors}
                    theme={theme}
                    t={t}
                    onClose={() => setIsTagAssignOpen(false)}
                />
            )}
        </motion.div >
    );
};

export default memo(ClipboardItem, (prevProps, nextProps) => {
    return prevProps.isSelected === nextProps.isSelected &&
        prevProps.item.id === nextProps.item.id &&
        prevProps.item.content_type === nextProps.item.content_type &&
        prevProps.item.timestamp === nextProps.item.timestamp &&
        prevProps.item.content === nextProps.item.content &&
        prevProps.item.preview === nextProps.item.preview &&
        prevProps.item.html_content === nextProps.item.html_content &&
        prevProps.item.source_app === nextProps.item.source_app &&
        prevProps.item.source_app_path === nextProps.item.source_app_path &&
        prevProps.item.is_pinned === nextProps.item.is_pinned &&
        prevProps.item.is_external === nextProps.item.is_external &&
        prevProps.item.file_preview_exists === nextProps.item.file_preview_exists &&
        prevProps.item.tags === nextProps.item.tags &&
        // R6: the note is part of what this row renders.
        getEntryNote(prevProps.item) === getEntryNote(nextProps.item) &&
        // R10: without these the body editor would never appear — the memo would
        // keep reporting "unchanged" while the dialog state flips on the parent.
        prevProps.isEditingBody === nextProps.isEditingBody &&
        prevProps.bodyInitialDraft === nextProps.bodyInitialDraft &&
        // R13: without these the memo would keep reporting "unchanged" while the rich
        // editor's seed value flips on the parent, so a reopened dialog would show the
        // previously discarded draft.
        prevProps.bodyInitialHtml === nextProps.bodyInitialHtml &&
        prevProps.bodyEditIsRich === nextProps.bodyEditIsRich &&
        prevProps.bodyEditSaving === nextProps.bodyEditSaving &&
        prevProps.bodyEditError === nextProps.bodyEditError &&
        !!prevProps.onEdit === !!nextProps.onEdit &&
        !!prevProps.onBodyEditSave === !!nextProps.onBodyEditSave &&
        // R11: the same trap as R10. `onEditNote` / `noteInitialDraft` / `noteEditError` /
        // `noteEditSaving` / `isEditingNote` all flip on the parent while `item` stays
        // referentially identical, so omitting any of them freezes a stale row: the note
        // dialog would never appear, never show a save error, or keep a discarded draft.
        prevProps.isEditingNote === nextProps.isEditingNote &&
        prevProps.noteInitialDraft === nextProps.noteInitialDraft &&
        prevProps.noteEditSaving === nextProps.noteEditSaving &&
        prevProps.noteEditError === nextProps.noteEditError &&
        !!prevProps.onEditNote === !!nextProps.onEditNote &&
        !!prevProps.onNoteEditSave === !!nextProps.onNoteEditSave &&
        prevProps.isRevealed === nextProps.isRevealed &&
        prevProps.isEditingTags === nextProps.isEditingTags &&
        prevProps.isAIProcessing === nextProps.isAIProcessing &&
        prevProps.aiOptionsOpen === nextProps.aiOptionsOpen &&
        prevProps.aiEnabled === nextProps.aiEnabled &&
        prevProps.richTextSnapshotPreview === nextProps.richTextSnapshotPreview &&
        prevProps.showSourceAppIcon === nextProps.showSourceAppIcon &&
        prevProps.quickPasteHint?.slot === nextProps.quickPasteHint?.slot &&
        prevProps.quickPasteHint?.combo === nextProps.quickPasteHint?.combo &&
        prevProps.compactMode === nextProps.compactMode &&
        prevProps.theme === nextProps.theme &&
        prevProps.language === nextProps.language &&
        prevProps.tagInput === nextProps.tagInput &&
        (prevProps.tagSuggestions ?? []).join('\u0000') === (nextProps.tagSuggestions ?? []).join('\u0000');
});
