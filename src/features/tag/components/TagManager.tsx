import { useState, useEffect, useRef, useMemo, useCallback } from 'react';
import { invoke, convertFileSrc } from '@tauri-apps/api/core';
import { listen, emit } from '@tauri-apps/api/event';
import {
    Edit2, Trash2, X, ChevronRight, LayoutGrid, List,
    Clock, MousePointer2, ChevronLeft, Plus, Search, ExternalLink, CheckSquare, Copy,
    Sparkles, AlertTriangle
} from 'lucide-react';
import { getTagColor } from "../../../shared/lib/utils";
import type { ClipboardEntry } from "../../../shared/types";
import TagGroupContextMenu from "./TagGroupContextMenu";
import "../../../styles/components/tag-group-menu.css";

interface TagManagerProps {
    t: (key: string) => string;
    theme: string;
    /**
     * R2: the remembered split geometry, handed over by the caller that already
     * holds the settings blob (`useSettingsInit` loads it at app boot).
     *
     * It exists so the first painted frame can already be the remembered geometry:
     * waiting for this component to fetch the same value would show the default
     * split for a frame and then jump, which is the flicker this prop removes.
     * Optional and deliberately `unknown` — absent, truncated or hand-edited values
     * all go through `resolveTagManagerLayout`, which falls back to the defaults.
     */
    persistedSize?: unknown;
}

interface TagInfo {
    name: string;
    count: number;
}

/** R4: content types whose `content` is a path or a data URL, not editable text. */
const BINARY_CONTENT_TYPES = ['image', 'file', 'video'];

const isBinaryContentType = (contentType: string | undefined | null) =>
    !!contentType && BINARY_CONTENT_TYPES.includes(contentType);

/** R6: mirror of `MAX_ENTRY_NOTE_CHARS` in `clipboard_repo.rs`. */
const MAX_NOTE_CHARS = 2000;

/**
 * R3: the one built-in tag name that a feature actually produces.
 *
 * `sensitive` is pushed onto an entry by the capture pipeline
 * (`services/clipboard/pipeline.rs`) while privacy protection is on, so an empty
 * leftover row for it should follow that setting.
 *
 * `密码` / `password` are deliberately NOT in this list. They have no producer
 * anywhere in the code base — they only ever came from the schema seed — so tying
 * them to the privacy setting would attach them to a feature that does not create
 * them. They are treated as legacy names: freely deletable, and shown like any other
 * group.
 *
 * The back end still treats all three as sensitive when blurring, which is why the
 * names are recognised in the main page's blur check regardless of this list.
 */
const FEATURE_PRODUCED_SENSITIVE_TAGS = ['sensitive'];

const isFeatureProducedSensitiveTag = (name: string) =>
    FEATURE_PRODUCED_SENSITIVE_TAGS.some((n) => n.toLowerCase() === name.toLowerCase());

/**
 * R3: should a tag group be shown?
 *
 * For the one feature-produced name (`sensitive`), two independent reasons keep it
 * visible:
 *  - the feature that produces it is on, so it is a normal, usable group; or
 *  - it actually holds entries, in which case hiding it would strand real data.
 * Only "feature off AND empty" hides it — a leftover seeded row nothing can put an
 * entry into. This never *deletes* the row; it only stops rendering it.
 *
 * A legacy name (`密码` / `password`) has no producing feature to consult, so it is
 * shown on its own merits: entries mean it is real and stays; an empty row is hidden
 * like any other empty group would be. Deleting it is permanent either way.
 *
 * Exported for tests: an off-by-one in this predicate silently hides user data or
 * silently resurrects a deleted seed, and neither is visible in a type check.
 */
export function shouldShowTag(
    tag: { name: string; count: number },
    sensitiveFeatureEnabled: boolean
): boolean {
    if (!isFeatureProducedSensitiveTag(tag.name)) return true;
    if (tag.count > 0) return true;
    return sensitiveFeatureEnabled;
}

/**
 * R3: read the privacy-protection setting, treating "unreadable" as enabled.
 *
 * The database seeds `app.privacy_protection` to `true`, and the capture pipeline
 * only appends the `sensitive` tag while that stored value is on. A failed settings
 * read must therefore not be interpreted as "feature off", or a transient error would
 * hide the group; only the literal string `false` counts as disabled.
 */
export function isSensitiveFeatureEnabled(
    settings: Record<string, string> | null | undefined
): boolean {
    return settings?.['app.privacy_protection'] !== 'false';
}

/**
 * R2: persisted geometry of the tag sidebar.
 *
 * The wide and stacked layouts are dragged along different axes (horizontal for
 * the column, vertical for the rail above the editor), so each keeps its own
 * numbers and its own collapsed flag. `width` / `height` / `collapsed` keep the
 * beta branch's key names so a settings blob written by beta still loads.
 */
interface TagManagerSidebarSize {
    width: number;
    height: number;
    collapsed: boolean;
    stackedWidth: number;
    stackedHeight: number;
    stackedCollapsed: boolean;
}

const DEFAULT_SIDEBAR_WIDTH = 130;
const DEFAULT_SIDEBAR_HEIGHT = 180;
/** `handleMouseMove` clamps the drag to these bounds; the parser accepts the same. */
const MIN_SIDEBAR_WIDTH = 48;
/** Below this drag position the sidebar folds to the collapsed rail (see `handleMouseMove`). */
export const COLLAPSE_THRESHOLD_PX = 110;
/** Width restored when the sidebar is expanded from the collapsed rail. */
export const EXPANDED_SIDEBAR_WIDTH = 160;
const MAX_SIDEBAR_WIDTH = 320;
const MIN_SIDEBAR_HEIGHT = 120;
const MAX_SIDEBAR_HEIGHT = 4000;

const DEFAULT_TAG_MANAGER_SIZE: TagManagerSidebarSize = {
    width: DEFAULT_SIDEBAR_WIDTH,
    height: DEFAULT_SIDEBAR_HEIGHT,
    collapsed: false,
    stackedWidth: DEFAULT_SIDEBAR_WIDTH,
    stackedHeight: DEFAULT_SIDEBAR_HEIGHT,
    stackedCollapsed: false,
};

/**
 * R2: read a numeric field, rejecting anything that is not a finite number inside
 * the range the drag interaction can produce.
 *
 * Storage can be absent, truncated, hand-edited, written by an older build, or
 * hold a string where a number belongs. Every one of those must degrade to the
 * default instead of reaching the layout as `NaN` or an absurd size, so this is
 * deliberately strict and total: it never throws.
 */
const readBoundedNumber = (raw: unknown, min: number, max: number): number | null =>
    typeof raw === 'number' && Number.isFinite(raw) && raw >= min && raw <= max ? raw : null;

/**
 * R2: parse the persisted sidebar geometry, falling back to defaults per field.
 *
 * Accepts either the raw settings string or an already-parsed object. A single
 * corrupt field only resets that field; the rest of the stored geometry survives.
 * `raw` being `null` / `undefined` / `''` is the normal first-run case and yields
 * the defaults, so this function is safe to call unconditionally on boot.
 */
export function parseTagManagerSidebarSize(raw: unknown): TagManagerSidebarSize {
    let source: unknown = raw;
    if (typeof raw === 'string') {
        if (!raw.trim()) return { ...DEFAULT_TAG_MANAGER_SIZE };
        try {
            source = JSON.parse(raw);
        } catch {
            return { ...DEFAULT_TAG_MANAGER_SIZE };
        }
    }
    if (!source || typeof source !== 'object' || Array.isArray(source)) {
        return { ...DEFAULT_TAG_MANAGER_SIZE };
    }

    const record = source as Record<string, unknown>;
    const width = readBoundedNumber(record.width, MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH);
    const height = readBoundedNumber(record.height, MIN_SIDEBAR_HEIGHT, MAX_SIDEBAR_HEIGHT);
    const stackedWidth = readBoundedNumber(record.stackedWidth, MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH);
    const stackedHeight = readBoundedNumber(record.stackedHeight, MIN_SIDEBAR_HEIGHT, MAX_SIDEBAR_HEIGHT);

    return {
        width: width ?? DEFAULT_SIDEBAR_WIDTH,
        height: height ?? DEFAULT_SIDEBAR_HEIGHT,
        collapsed: record.collapsed === true,
        // A layout the user never dragged inherits the wide-layout geometry, which is
        // what beta stored, so upgrading from beta does not reset the stacked view.
        stackedWidth: stackedWidth ?? width ?? DEFAULT_SIDEBAR_WIDTH,
        stackedHeight: stackedHeight ?? height ?? DEFAULT_SIDEBAR_HEIGHT,
        stackedCollapsed: record.stackedCollapsed === true,
    };
}

/**
 * R2: fold a collapse-toggle click into the stored geometry.
 *
 * Pure so it can be unit-tested: the click path is the one geometry change that does
 * not go through a drag, and a regression there is invisible until the next launch.
 *
 * Rules, matching the drag behaviour:
 *  - toggling writes only to the layout currently in effect (`stacked`), leaving the
 *    other layout's remembered numbers untouched;
 *  - expanding a sidebar that was folded at the collapsed rail width restores a
 *    usable width (160) instead of leaving it at the rail width, because a
 *    yet-unset width would otherwise reopen at 48px;
 *  - `collapsed` is stored as a real boolean so `parseTagManagerSidebarSize`'s
 *    strict `=== true` check round-trips it.
 */
export function applyCollapseToggle(
    stored: TagManagerSidebarSize,
    current: { stacked: boolean; width: number; collapsed: boolean },
    expandedWidthFallback: number = EXPANDED_SIDEBAR_WIDTH
): TagManagerSidebarSize {
    const nextCollapsed = !current.collapsed;
    const nextWidth =
        !nextCollapsed && current.width < COLLAPSE_THRESHOLD_PX
            ? expandedWidthFallback
            : current.width;

    if (current.stacked) {
        return {
            ...stored,
            stackedCollapsed: nextCollapsed,
            stackedWidth: nextWidth,
        };
    }
    return {
        ...stored,
        collapsed: nextCollapsed,
        width: nextWidth,
    };
}

/**
 * R2: the viewport width at which the sidebar is arranged as a rail above the
 * editor instead of a column beside it. It mirrors the media query the `isStacked`
 * effect listens to, and lives here so the first-frame geometry and that effect
 * cannot drift apart.
 */
const STACKED_VIEWPORT_MAX_WIDTH = 340;

/** R2: the media query the stacked-arrangement effect listens to. */
const STACKED_VIEWPORT_QUERY = `(max-width: ${STACKED_VIEWPORT_MAX_WIDTH}px)`;

/** R2: is the stacked arrangement the one in effect at this viewport width? */
export function isStackedViewport(viewportWidth: number): boolean {
    return Number.isFinite(viewportWidth) && viewportWidth <= STACKED_VIEWPORT_MAX_WIDTH;
}

/**
 * R2: resolve the arrangement in effect at first paint.
 *
 * The `isStacked` effect answers the same question, but only after the first
 * commit — too late to pick which remembered geometry the first frame should
 * show. Asking the same media query here keeps the two answers identical instead
 * of guessing from `innerWidth`.
 *
 * Total by construction: a renderer without `window` (static markup in tests) or
 * without `matchMedia` simply takes the non-stacked arrangement, which is the
 * previous first-frame default.
 */
const readStackedAtFirstFrame = (): boolean => {
    if (typeof window === 'undefined') return false;
    try {
        if (typeof window.matchMedia === 'function') {
            return window.matchMedia(STACKED_VIEWPORT_QUERY).matches;
        }
        if (typeof window.innerWidth === 'number') {
            return isStackedViewport(window.innerWidth);
        }
    } catch {
        // Fall through to the non-stacked arrangement below.
    }
    return false;
};

/**
 * R2: fold a stored geometry into the three numbers the layout actually renders.
 *
 * This is the single place that decides "which remembered numbers apply right now",
 * which is what lets the sidebar be painted with them on the very first render
 * instead of after a settings round trip. Pure and total: a missing or corrupt
 * stored value degrades to the defaults, which is also the normal first-run case.
 *
 * Exported for tests: a regression here is exactly the visible bug this exists to
 * prevent — the split showing the default geometry for a frame before snapping to
 * the remembered one.
 */
export function resolveTagManagerLayout(
    storedRaw: unknown,
    stacked: boolean
): { width: number; height: number; collapsed: boolean } {
    const stored = parseTagManagerSidebarSize(storedRaw);
    return stacked
        ? {
              width: stored.stackedWidth,
              height: stored.stackedHeight,
              collapsed: stored.stackedCollapsed,
          }
        : { width: stored.width, height: stored.height, collapsed: stored.collapsed };
}

/**
 * R2: the geometry this session last wrote to settings.
 *
 * `persistedSize` is read from the settings blob the app loaded at *boot*, so a
 * value written while the app is running would still be absent from that prop when
 * the manager is reopened in the same session — the first frame would then show the
 * previous split and snap once the fetch landed, i.e. the very flicker being fixed.
 * This component is the only writer of that key (it is excluded from cloud sync),
 * so remembering our own last write closes that gap with no settings round trip.
 * A fresh app start simply has nothing cached and uses the prop.
 */
let sessionWrittenSize: unknown;

/**
 * R2: pick the geometry for the first frame.
 *
 * Same-session write beats the value captured at boot; anything else defers to the
 * boot value, and `resolveTagManagerLayout` degrades absent/corrupt input to the
 * defaults. Pure so the precedence itself is testable.
 */
export function resolveInitialTagManagerLayout(
    persistedSize: unknown,
    sessionWritten: unknown,
    stacked: boolean
): { width: number; height: number; collapsed: boolean } {
    return resolveTagManagerLayout(sessionWritten ?? persistedSize, stacked);
}

export default function TagManager({ t, theme, persistedSize }: TagManagerProps) {
    const TAG_MANAGER_VIEW_MODE_KEY = "tiez_tag_manager_view_mode";
    const TAG_MANAGER_SIZE_KEY = "app.tag_manager_size";
    const [tags, setTags] = useState<TagInfo[]>([]);
    const [tagSearch, setTagSearch] = useState('');
    const [selectedTag, setSelectedTag] = useState<string | null>(null);
    const [tagItems, setTagItems] = useState<ClipboardEntry[]>([]);
    const [tagColors, setTagColors] = useState<Record<string, string>>({});
    const [editingTag, setEditingTag] = useState<string | null>(null);
    const [newTagName, setNewTagName] = useState('');
    const [loading, setLoading] = useState(false);
    const [viewMode, setViewMode] = useState<'list' | 'grid'>(() => {
        try {
            const saved = window.localStorage.getItem(TAG_MANAGER_VIEW_MODE_KEY);
            return saved === 'list' ? 'list' : 'grid';
        } catch {
            return 'grid';
        }
    });
    const [isDeleting, setIsDeleting] = useState(false);
    /**
     * B9: the group's rename/delete actions live in a right-click menu.
     *
     * They used to be two inline icons that appeared on hover, sharing the row with the
     * tag name. That is exactly where the user aims when they mean "select this group",
     * so the icons stole clicks meant for selection. Moving them behind `contextmenu`
     * leaves the row with only color dot + name + count, and the pointer has nothing
     * else to land on.
     *
     * `null` = menu closed. The coordinates are viewport coordinates of the right-click.
     */
    const [tagMenu, setTagMenu] = useState<{ x: number; y: number; tagName: string; affected: number } | null>(null);
    const [deleteConfirmation, setDeleteConfirmation] = useState<{ show: boolean, tagName: string | null, affected: number }>({ show: false, tagName: null, affected: 0 });
    const [itemDeleteConfirmation, setItemDeleteConfirmation] = useState<{ show: boolean, id: number | null }>({ show: false, id: null });
    /**
     * R2: the arrangement in effect is decided before the first paint, not by the
     * listener effect below, because the first frame has to pick which remembered
     * geometry to show and cannot wait a commit for that answer.
     */
    const [isStacked, setIsStacked] = useState(readStackedAtFirstFrame);
    const [sortBy, setSortBy] = useState<'time' | 'count'>('time');
    const [isCreatingItem, setIsCreatingItem] = useState(false);
    /**
     * R4/R6: the edit dialog now serves every content type. `originalContent` /
     * `originalNote` are kept so saving only issues the commands for what actually
     * changed — firing `update_item_content` on an untouched body would otherwise
     * be a no-op that still emits a refresh, and `update_entry_note` on an untouched
     * note would rewrite the row for nothing.
     */
    const [editingItem, setEditingItem] = useState<{
        id: number;
        content: string;
        note: string;
        contentType: string;
        originalContent: string;
        originalNote: string;
    } | null>(null);
    const [newItemContent, setNewItemContent] = useState('');
    /**
     * R2: the first frame carries the remembered split geometry.
     *
     * `persistedSize` comes from the settings blob the app already loaded at boot,
     * so this component has the value synchronously and can seed the layout with it.
     * The previous arrangement started from the defaults and swapped in the
     * remembered numbers from an effect after `get_settings` resolved, which is
     * exactly the "opens at one ratio, then snaps to another" flicker.
     *
     * Computed once, in a lazy `useRef` initialiser, so it neither re-runs on later
     * renders nor depends on an async round trip.
     */
    const initialLayout = useRef(
        resolveInitialTagManagerLayout(persistedSize, sessionWrittenSize, isStacked)
    ).current;
    /**
     * R2: once the user has dragged or toggled the split, the live geometry is the
     * authority and a late-arriving remembered value must not overwrite it.
     */
    const hasUserAdjustedGeometryRef = useRef(false);
    const [sidebarWidth, setSidebarWidth] = useState(initialLayout.width);
    const [sidebarHeight, setSidebarHeight] = useState(initialLayout.height);
    const [isResizing, setIsResizing] = useState(false);
    const [isCollapsed, setIsCollapsed] = useState(initialLayout.collapsed);
    const [isManageMode, setIsManageMode] = useState(false);
    const [selectedItemIds, setSelectedItemIds] = useState<Set<number>>(new Set());
    const containerRef = useRef<HTMLDivElement>(null);

    const selectedTagRef = useRef<string | null>(null);
    useEffect(() => { selectedTagRef.current = selectedTag; }, [selectedTag]);

    // ---------------------------------------------------------------------
    // R2: sidebar geometry persistence
    // ---------------------------------------------------------------------

    /**
     * R2: the geometry as it should be written to settings.
     *
     * A collapsed sidebar has a fixed width, so persisting the collapsed *width*
     * would destroy the width the user had before collapsing — reopening would then
     * have to guess. The expanded width is therefore what gets stored, together with
     * the collapsed flag; that is also exactly what the beta branch stores.
     *
     * Seeded from the caller's remembered value (both layouts, not just the one on
     * screen) so that what gets written back on the first drag already carries the
     * other layout's remembered numbers.
     */
    const sidebarSizeRef = useRef<TagManagerSidebarSize>(
        parseTagManagerSidebarSize(sessionWrittenSize ?? persistedSize)
    );

    /**
     * R2: geometry state must be readable by the drag handlers without adding
     * `sidebarWidth` / `isCollapsed` to their dependency arrays, because those
     * change on every mousemove and would tear down and re-add the listeners
     * mid-drag. A ref mirror keeps the drag effect keyed only on `isResizing`.
     */
    const geometryRef = useRef({
        width: initialLayout.width,
        height: initialLayout.height,
        collapsed: initialLayout.collapsed,
        stacked: isStacked,
    });

    useEffect(() => {
        geometryRef.current = {
            width: sidebarWidth,
            height: sidebarHeight,
            collapsed: isCollapsed,
            stacked: isStacked,
        };
        const current = sidebarSizeRef.current;
        if (isStacked) {
            current.stackedWidth = sidebarWidth;
            current.stackedHeight = sidebarHeight;
            current.stackedCollapsed = isCollapsed;
        } else {
            current.width = sidebarWidth;
            current.height = sidebarHeight;
            current.collapsed = isCollapsed;
        }
    }, [sidebarWidth, sidebarHeight, isCollapsed, isStacked]);

    /**
     * R2: write the current geometry through `save_setting` (the beta approach).
     *
     * Settings rather than `localStorage` so the values live in the same store as the
     * rest of the configuration (and therefore ride along with a future backup/export)
     * instead of being stranded inside a WebView origin that an identifier rename can
     * invalidate. Note that this key is deliberately NOT cloud-synced — see the
     * exclusion list in `services/cloud_sync.rs` — so "travels with cloud sync" would
     * be wrong here. A failure is logged and swallowed: losing a remembered split
     * position must never break the tag manager.
     */
    const persistSidebarSize = useCallback(() => {
        const geometry = sidebarSizeRef.current;
        // R2: remember our own write for the rest of the session, so reopening the
        // manager seeds its first frame from this value rather than from the older
        // one the app captured at boot.
        sessionWrittenSize = geometry;
        invoke('save_setting', {
            key: TAG_MANAGER_SIZE_KEY,
            value: JSON.stringify(geometry),
        }).catch(console.error);
    }, []);

    /**
     * R2: apply a collapse-toggle click to the stored geometry and return the record
     * to persist.
     *
     * Extracted as a pure function for two reasons: the click happens outside the
     * drag, so the width/height mirror ref has not been refreshed when the click runs
     * and the new values have to be written explicitly; and exportable pure logic can
     * carry a regression test, unlike a handler closed over component state.
     *
     * Only the layout currently in effect is touched, so toggling the sidebar while
     * wide never disturbs the remembered stacked geometry (and vice versa).
     */
    const toggleCollapse = (current: TagManagerSidebarSize) =>
        applyCollapseToggle(current, geometryRef.current);

    /**
     * R2: adopt a geometry that only becomes known after the first frame.
     *
     * The normal path needs nothing here: the caller hands over the remembered
     * value before the first render, so the layout already shows it and this effect
     * only re-applies an identical value (React bails out of that). It earns its
     * keep in two cases:
     *
     *  - the settings blob was still in flight when the manager opened, so the prop
     *    arrives late — the layout then adopts it instead of staying on the defaults;
     *  - no prop is available at all (`persistedSize === undefined`, e.g. a caller
     *    that does not thread it through), which keeps the previous self-fetch as a
     *    fallback rather than losing the feature.
     *
     * The parser is total: unset, truncated, hand-edited or wrong-typed storage all
     * degrade to the defaults instead of producing `NaN` sizes or throwing. A
     * geometry the user has already adjusted is never overwritten by either path.
     */
    useEffect(() => {
        if (hasUserAdjustedGeometryRef.current) return;

        const adopt = (raw: unknown) => {
            const restored = parseTagManagerSidebarSize(raw);
            sidebarSizeRef.current = restored;
            // The layout in effect at mount decides which of the two remembered
            // geometries applies. `isStacked` is resolved before the first paint, so
            // it is already correct here; read the *stacked* fields on a narrow
            // window rather than always taking the wide ones, otherwise a user who
            // only ever opened the manager stacked would have their remembered
            // height replaced by the wide-layout default.
            const applied = resolveTagManagerLayout(restored, geometryRef.current.stacked);
            setSidebarWidth(applied.width);
            setSidebarHeight(applied.height);
            setIsCollapsed(applied.collapsed);
        };

        if (persistedSize !== undefined) {
            // Same precedence as the first frame, so this settles on the value the
            // layout is already showing instead of reverting a same-session write.
            adopt(sessionWrittenSize ?? persistedSize);
            return;
        }

        let cancelled = false;
        invoke<Record<string, string>>('get_settings')
            .then((settings) => {
                // A prop that arrived while this fetch was in flight is the fresher
                // source, and the user may have dragged in the meantime.
                if (cancelled || hasUserAdjustedGeometryRef.current) return;
                adopt(settings?.[TAG_MANAGER_SIZE_KEY]);
            })
            .catch(console.error);
        return () => { cancelled = true; };
    }, [persistedSize]);

    /**
     * R2: geometry is remembered per layout, so switching between the wide and
     * stacked arrangements restores the numbers that belong to the arrangement now
     * in effect rather than reusing the other one's.
     */
    const appliedStackedRef = useRef<boolean | null>(null);
    useEffect(() => {
        if (appliedStackedRef.current === isStacked) return;
        const isFirstApplication = appliedStackedRef.current === null;
        appliedStackedRef.current = isStacked;
        // The initial application must not overwrite what the restore effect just
        // loaded, otherwise a saved stacked geometry would be replaced by the wide
        // one on the very first render.
        if (isFirstApplication) return;
        const stored = sidebarSizeRef.current;
        if (isStacked) {
            setSidebarWidth(stored.stackedWidth);
            setSidebarHeight(stored.stackedHeight);
            setIsCollapsed(stored.stackedCollapsed);
        } else {
            setSidebarWidth(stored.width);
            setSidebarHeight(stored.height);
            setIsCollapsed(stored.collapsed);
        }
    }, [isStacked]);

    useEffect(() => {
        try {
            window.localStorage.setItem(TAG_MANAGER_VIEW_MODE_KEY, viewMode);
        } catch {
            // Ignore storage write failures and keep UI functional.
        }
    }, [viewMode]);

    useEffect(() => {
        let unlisteners: (() => void)[] = [];
        const setupListeners = async () => {
            const handleUpdate = () => {
                // Don't refresh if we're in the middle of a delete operation
                if (isDeleting) return;
                fetchTags();
                if (selectedTagRef.current) loadTagItems(selectedTagRef.current);
            };
            unlisteners.push(await listen('clipboard-changed', handleUpdate));
            unlisteners.push(await listen('clipboard-updated', handleUpdate));
            unlisteners.push(await listen('clipboard-removed', handleUpdate));
        };
        setupListeners();
        return () => unlisteners.forEach(f => f());
    }, [isDeleting]);

    useEffect(() => { fetchTags(); }, []);

    useEffect(() => {
        const mediaQuery = window.matchMedia(STACKED_VIEWPORT_QUERY);
        const updateLayoutMode = () => {
            setIsStacked(mediaQuery.matches);
        };

        updateLayoutMode();
        mediaQuery.addEventListener("change", updateLayoutMode);

        return () => mediaQuery.removeEventListener("change", updateLayoutMode);
    }, []);

    useEffect(() => {
        if (!isResizing) return;

        const handleMouseMove = (event: MouseEvent) => {
            const bounds = containerRef.current?.getBoundingClientRect();
            if (!bounds) return;
            // R2: from here on the drag position wins over any remembered value.
            hasUserAdjustedGeometryRef.current = true;
            if (isStacked) {
                const maxHeight = Math.max(140, bounds.height - 180);
                const nextHeight = Math.min(Math.max(event.clientY - bounds.top, 120), maxHeight);
                setSidebarHeight(nextHeight);
                return;
            }

            const dragPos = event.clientX - bounds.left;
            
            // Auto collapse threshold (shared with the toggle button).
            if (dragPos < COLLAPSE_THRESHOLD_PX) {
                if (!isCollapsed) setIsCollapsed(true);
                setSidebarWidth(48);
            } else {
                if (isCollapsed) setIsCollapsed(false);
                const nextWidth = Math.min(dragPos, 320);
                setSidebarWidth(nextWidth);
            }
        };

        const handleMouseUp = () => {
            setIsResizing(false);
            document.body.style.cursor = "";
            document.body.style.userSelect = "";
            // R2: the drag position is only meaningful once the user lets go, so the
            // write happens here rather than on every mousemove.
            persistSidebarSize();
        };

        document.body.style.cursor = isStacked ? "row-resize" : "col-resize";
        document.body.style.userSelect = "none";
        window.addEventListener("mousemove", handleMouseMove);
        window.addEventListener("mouseup", handleMouseUp);

        return () => {
            window.removeEventListener("mousemove", handleMouseMove);
            window.removeEventListener("mouseup", handleMouseUp);
            document.body.style.cursor = "";
            document.body.style.userSelect = "";
        };
    }, [isResizing, isStacked, persistSidebarSize]);

    const fetchTags = async () => {
        try {
            const [tagMap, colors, settings] = await Promise.all([
                invoke<Record<string, number>>('get_all_tags_info'),
                invoke<Record<string, string>>('get_tag_colors'),
                // R3: read on every refresh rather than once, so toggling the feature in
                // settings is reflected without remounting the tag manager.
                invoke<Record<string, string>>('get_settings').catch(() => ({} as Record<string, string>)),
            ]);

            // R3: the privacy-protection switch is the feature that *produces* the
            // `sensitive` tag — the capture pipeline only appends it while that setting
            // is on (`services/clipboard/pipeline.rs`), and the main-page blur reads the
            // same tag names. Missing / unreadable setting counts as enabled, matching
            // the database default of `true`, so a read failure hides nothing.
            const sensitiveFeatureEnabled = isSensitiveFeatureEnabled(settings);

            const tagArray = Object.entries(tagMap)
                .map(([name, count]) => ({ name, count }))
                // R3: hide a built-in sensitive tag only when both conditions hold — the
                // feature that would produce it is off, and it holds nothing. A group with
                // entries is always shown (and is fully deletable); a leftover seeded row
                // follows the feature instead of lingering as an un-actionable group.
                .filter((tag) => shouldShowTag(tag, sensitiveFeatureEnabled));
            tagArray.sort((a, b) => b.count - a.count);
            setTags(tagArray);
            setTagColors(colors || {});

            const activeTag = selectedTagRef.current;
            if (tagArray.length === 0) {
                setSelectedTag(null);
                setTagItems([]);
                return;
            }
            if (!activeTag || !tagArray.some(tag => tag.name === activeTag)) {
                loadTagItems(tagArray[0].name);
            }
        } catch (err) { console.error(err); }
    };

    const loadTagItems = async (tagName: string) => {
        setLoading(true);
        setSelectedTag(tagName);
        try {
            const items = await invoke<ClipboardEntry[]>('get_tag_items', { tag: tagName });
            setTagItems(items || []);
        } catch (err) { console.error(err); setTagItems([]); }
        finally { setLoading(false); }
    };

    const createTag = async (rawName: string) => {
        const trimmed = rawName.trim();
        if (!trimmed) return;

        try {
            await invoke('create_new_tag', { tagName: trimmed });
            setNewTagName('');
            setTagSearch('');
            await fetchTags();
            await loadTagItems(trimmed);
        } catch (err) { console.error(err); }
    };

    /**
     * R3: rename any group, including the built-in sensitive ones.
     *
     * These two names used to be refused here (and the buttons hidden at the call
     * site), which made them the only groups in the product that could be renamed
     * into existence but never renamed or removed. They are ordinary `saved_tags`
     * rows; the back end has no built-in-tag concept, so the refusal existed purely
     * in this component.
     *
     * Renaming `sensitive` is a real behaviour change and is called out in the
     * confirmation-free rename path only in the sense that the main page's blur check
     * keys off the tag *name*: after a rename nothing is blurred until the entry is
     * tagged again. That is the user's explicit choice to make here.
     */
    const handleRenameTag = async (oldName: string) => {
        const trimmed = newTagName.trim();
        if (!trimmed || trimmed === oldName) { setEditingTag(null); return; }

        try {
            await invoke('rename_tag_globally', { oldName, newName: trimmed });
            if (selectedTag === oldName) setSelectedTag(trimmed);
            await fetchTags();
            await loadTagItems(trimmed);
            setEditingTag(null);
            setNewTagName('');
        } catch (err) { console.error(err); }
    };

    /**
     * R3: delete any group.
     *
     * The back end (`delete_tag_from_all` → `tag_repo.delete_globally`) removes the
     * `saved_tags` row and the `entry_tags` links and leaves every entry in place, so
     * this is a group operation, not a data operation. The confirmation dialog states
     * how many entries will be unlinked before it runs.
     */
    const handleDeleteTag = async (tagName: string) => {
        setIsDeleting(true);
        // Remember whether the group we are removing is the one on screen, so a stale
        // selection cannot leave the item pane pointing at a tag that no longer exists.
        const wasSelected = selectedTagRef.current === tagName;
        try {
            await invoke('delete_tag_from_all', { tagName });
            if (wasSelected) {
                setSelectedTag(null);
                setTagItems([]);
            }
            await emit('clipboard-changed'); // Notify App.tsx to refresh
            await fetchTags();
        } catch (err) { console.error(err); }
        finally {
            setIsDeleting(false);
        }
    };

    const handleAddManualItem = async () => {
        if (!newItemContent.trim() || !selectedTag) return;
        try {
            await invoke('add_manual_item', {
                content: newItemContent,
                contentType: 'text',
                tags: [selectedTag]
            });
            setNewItemContent('');
            setIsCreatingItem(false);
            await loadTagItems(selectedTag);
        } catch (err) { console.error(err); }
    };

    /**
     * R4/R6: save the edit dialog.
     *
     * Two independent writes:
     *   - the *note* is sent for every content type, because a note is entry
     *     metadata and never touches `content`;
     *   - the *body* is sent only for text-like types. For `image` / `file` /
     *     `video` the body is a path or a data URL, so the textarea is not rendered
     *     for them and this branch is unreachable from the UI; the back end rejects
     *     it as well (`update_entry_content` → `is_binary_content_type`), so a stale
     *     caller cannot corrupt a row either.
     *
     * Each write is guarded by a dirty check so opening the dialog and pressing save
     * does not emit a pointless `clipboard-changed` round trip.
     */
    const handleSaveItem = async () => {
        if (!editingItem) return;
        const { id, contentType, content, note, originalContent, originalNote } = editingItem;
        const isBinary = isBinaryContentType(contentType);
        const bodyChanged = !isBinary && content !== originalContent;
        const noteChanged = note !== originalNote;

        if (bodyChanged && !content.trim()) return;

        try {
            if (bodyChanged) {
                await invoke('update_item_content', { id, newContent: content });
            }
            if (noteChanged) {
                await invoke('update_entry_note', { id, note });
            }
            setEditingItem(null);
            if (selectedTag) await loadTagItems(selectedTag);
        } catch (err) { console.error(err); }
    };

    /**
     * R6: open the quick note editor for one card.
     *
     * The dialog is shared with the body editor (R4), so this seeds it with the
     * entry's current body and note and lets the user change either. Keeping one
     * dialog means the two features cannot drift apart in behaviour.
     */
    const openItemEditor = (item: ClipboardEntry) => {
        const note = item.note || '';
        setEditingItem({
            id: item.id,
            content: item.content,
            note,
            contentType: item.content_type,
            originalContent: item.content,
            originalNote: note,
        });
    };

    const copyToClipboard = async (id: number, content: string, type: string) => {
        try {
            // R7: a paste from the tag manager counts as a paste and must land as the
            // newest entry on the clipboard home page, so pin the move-to-top intent
            // explicitly instead of relying on the ambient app setting.
            await invoke('copy_to_clipboard', { content, contentType: type, paste: true, id, deleteAfterUse: false, moveToTop: true });
        } catch (err) { console.error(err); }
    };

    const filteredTags = useMemo(() => {
        return tags.filter(t => t.name.toLowerCase().includes(tagSearch.toLowerCase()));
    }, [tags, tagSearch]);

    const normalizedTagSearch = tagSearch.trim().toLowerCase();
    const canCreateTag = normalizedTagSearch.length > 0
        && !tags.some(tag => tag.name.toLowerCase() === normalizedTagSearch);

    const sortedItems = [...tagItems].sort((a, b) => {
        if (sortBy === 'count') return (b.use_count || 0) - (a.use_count || 0);
        return b.timestamp - a.timestamp;
    });

    const formatItemDate = (timestamp: number) => {
        const date = new Date(timestamp);
        const year = date.getFullYear();
        const month = String(date.getMonth() + 1).padStart(2, '0');
        const day = String(date.getDate()).padStart(2, '0');
        return `${year}-${month}-${day}`;
    };

    return (
        <div
            ref={containerRef}
            className={`themed-tag-manager theme-${theme} ${isCollapsed ? 'sidebar-collapsed' : ''} ${isStacked ? 'stacked-layout' : ''}`}
            style={{ 
                ["--tm-sidebar-width" as any]: isCollapsed ? '48px' : `${sidebarWidth}px`,
                ["--tm-sidebar-height" as any]: `${sidebarHeight}px`
            } as any}
            onMouseDown={() => invoke('activate_window_focus').catch(console.error)}
        >
            {/* Sidebar with CRUD support */}
            {/* Sidebar with Unified Search & Create */}
            <div className="tag-sidebar">
                <div className="sidebar-header">
                    {!isCollapsed && <span className="header-label">{t('tags')}</span>}
                    <button
                        className="collapse-toggle"
                        title={isCollapsed ? (t('open') || '展开') : (t('collapse') || '收起')}
                        onClick={() => {
                            // R2: collapsing is a geometry change, so it is remembered
                            // even though no drag happened. `applyCollapseToggle` writes
                            // the new values into the ref explicitly, because the mirror
                            // effect that normally maintains it has not run yet at click
                            // time and persisting would store the pre-click value.
                            const updated = toggleCollapse(sidebarSizeRef.current);
                            sidebarSizeRef.current = updated;
                            // R2: the click is a deliberate geometry change, so a
                            // remembered value arriving later must not undo it.
                            hasUserAdjustedGeometryRef.current = true;
                            // Read the values back from the layout that was actually
                            // updated instead of assuming which one it was.
                            const stackedNow = geometryRef.current.stacked;
                            setIsCollapsed(
                                stackedNow ? updated.stackedCollapsed : updated.collapsed
                            );
                            const nextWidth = stackedNow ? updated.stackedWidth : updated.width;
                            if (nextWidth !== sidebarWidth) setSidebarWidth(nextWidth);
                            persistSidebarSize();
                        }}
                    >
                        {isCollapsed ? <ChevronRight size={14} /> : <ChevronLeft size={14} />}
                    </button>
                </div>

                {!isCollapsed && (
                    <div className="tag-search-box">
                        <Search size={16} className="search-icon-placeholder" />
                        <input
                            placeholder={t('find_or_create')}
                            value={tagSearch}
                            onMouseDown={() => invoke('activate_window_focus').catch(console.error)}
                            onFocus={() => invoke('activate_window_focus').catch(console.error)}
                            onChange={e => setTagSearch(e.target.value)}
                            onKeyDown={async (e) => {
                                if (e.key === 'Enter' && tagSearch.trim()) {
                                    // If exact match exists, select it. If not, create new.
                                    const exactMatch = tags.find(t => t.name.toLowerCase() === normalizedTagSearch);
                                    if (exactMatch) {
                                        loadTagItems(exactMatch.name);
                                    } else {
                                        await createTag(tagSearch);
                                    }
                                }
                            }}
                        />
                        {tagSearch ? (
                            <div className="action-icons">
                                { /* If no exact match, show Plus to indicate creation */}
                                {canCreateTag ? (
                                    <span
                                        title={t('create_new_tag_tooltip')}
                                        className="action-icon create"
                                        onClick={() => createTag(tagSearch)}
                                    >
                                        <Plus size={12} />
                                    </span>
                                ) : null}
                                <X size={12} className="action-icon clear" onClick={() => setTagSearch('')} />
                            </div>
                        ) : null}
                    </div>
                )}

                <div className="tag-scroll custom-scrollbar">
                    {filteredTags.map(tag => (
                        <div
                            key={tag.name}
                            className={`tag-item ${selectedTag === tag.name ? 'active' : ''}`}
                            onClick={() => loadTagItems(tag.name)}
                            /**
                             * B9: right-click is the only way in to rename/delete now, so it
                             * must not fall through to the WebView's own context menu either.
                             * While the row is being renamed inline the menu is suppressed —
                             * the inline input is the active surface and a stray right-click
                             * should not open a second entry point for the same group.
                             */
                            onContextMenu={(e) => {
                                e.preventDefault();
                                e.stopPropagation();
                                if (editingTag === tag.name) return;
                                setTagMenu({ x: e.clientX, y: e.clientY, tagName: tag.name, affected: tag.count });
                            }}
                            title={tag.name}
                        >
                            <div className="tag-color-wrapper" onClick={(e) => e.stopPropagation()}>
                                <div
                                    className="tag-color-dot"
                                    style={{ background: tagColors[tag.name] || getTagColor(tag.name, theme) }}
                                    onClick={() => document.getElementById(`color-picker-${tag.name}`)?.click()}
                                />
                                <input
                                    type="color"
                                    id={`color-picker-${tag.name}`}
                                    style={{ display: 'none' }}
                                    value={tagColors[tag.name] || '#888888'} // Approximation if not set, or maybe convert HSL to Hex?
                                    onChange={async (e) => {
                                        const newColor = e.target.value;
                                        setTagColors(prev => ({ ...prev, [tag.name]: newColor }));
                                        await invoke('set_tag_color', { name: tag.name, color: newColor });
                                        await emit('tag-colors-updated');
                                    }}
                                />
                            </div>
                            {editingTag === tag.name ? (
                                <input
                                    className="inline-tag-edit"
                                    value={newTagName}
                                    onMouseDown={() => invoke('activate_window_focus').catch(console.error)}
                                    onFocus={() => invoke('activate_window_focus').catch(console.error)}
                                    onChange={(e) => setNewTagName(e.target.value)}
                                    autoFocus
                                    onKeyDown={async (e) => {
                                        if (e.key === 'Enter') {
                                            await handleRenameTag(tag.name);
                                        } else if (e.key === 'Escape') {
                                            setEditingTag(null);
                                        }
                                    }}
                                    onBlur={() => setEditingTag(null)}
                                    onClick={(e) => e.stopPropagation()}
                                />
                            ) : (
                                <>
                                    {/*
                                     * B9: the rename/delete icons used to sit right here and
                                     * popped in on hover. They shared the row with the tag name,
                                     * which is where the pointer lands when the intent is
                                     * "select this group" — so they were moved into the
                                     * right-click menu (`tagMenu` above). The row now carries
                                     * only the color dot, the name and the count, and a left
                                     * click anywhere on it selects the group.
                                     */}
                                    <span className="tag-name">{tag.name}</span>
                                    <span className="tag-badge">{tag.count}</span>
                                </>
                            )}
                        </div>
                    ))}
                    {filteredTags.length === 0 && !tagSearch.trim() && (
                        <div className="sidebar-status">{t('no_tags')}</div>
                    )}
                    {/* Visual cue for creating new tag when filtering shows no results */}
                    {!isCollapsed && canCreateTag && filteredTags.length === 0 && (
                        <div className="tag-item create-hint" onClick={() => createTag(tagSearch)}>
                            <div className="tag-color-dot" style={{ border: '1px dashed currentColor', background: 'transparent' }} />
                            <span className="tag-name" style={{ opacity: 0.7 }}>{t('create_tag_hint').replace('{tag}', tagSearch.trim())}</span>
                            <Plus size={10} />
                        </div>
                    )}
                </div>
            </div>

            {!isCollapsed && (
                <div 
                    className={`tag-divider ${isResizing ? 'active' : ''} ${isStacked ? 'stacked' : ''}`}
                    onMouseDown={(e) => {
                        e.preventDefault();
                        setIsResizing(true);
                    }}
                >
                    <div className="tag-divider-handle" />
                </div>
            )}

            {/* Right Main Area */}
            <div className="tag-content">
                <div className="content-toolbar">
                    <div className="toolbar-left">
                        <div className="selected-tag-indicator">
                            <span className="breadcrumb-marker">#</span>
                            <span className="breadcrumb-text">{selectedTag || t('tags')}</span>
                        </div>
                        <div className="toolbar-divider" />
                        <div className="sort-group">
                            <button
                                className={`sort-btn ${sortBy === 'time' ? 'active' : ''}`}
                                title={t('sort_time') || '按时间'}
                                onClick={() => setSortBy('time')}
                            >
                                <Clock size={12} />
                                <span>{t('sort_time') || '时间'}</span>
                            </button>
                            <button
                                className={`sort-btn ${sortBy === 'count' ? 'active' : ''}`}
                                title={t('sort_usage') || '按频率'}
                                onClick={() => setSortBy('count')}
                            >
                                <MousePointer2 size={12} />
                                <span>{t('sort_usage') || '频率'}</span>
                            </button>
                        </div>
                    </div>
                    <div className="toolbar-right">
                        {selectedTag && (
                            <div className="toolbar-actions">
                                {isManageMode ? (
                                    <>
                                        <button
                                            className="sort-btn"
                                            onClick={() => {
                                                setIsManageMode(false);
                                                setSelectedItemIds(new Set());
                                            }}
                                        >
                                            {t('cancel') || '取消'}
                                        </button>
                                        <button
                                            className="sort-btn danger"
                                            disabled={selectedItemIds.size === 0}
                                            onClick={() => setItemDeleteConfirmation({ show: true, id: -1 })}
                                        >
                                            <Trash2 size={14} />
                                            <span>{t('delete_selected') || '删除选中'}</span>
                                        </button>
                                        <button
                                            className="sort-btn active"
                                            disabled={selectedItemIds.size === 0}
                                            onClick={async () => {
                                                const selectedItems = tagItems.filter(item => selectedItemIds.has(item.id));
                                                if (selectedItems.length > 0) {
                                                    const combinedContent = selectedItems.map(item => item.content).join('\n');
                                                    await invoke('copy_to_clipboard', {
                                                        content: combinedContent,
                                                        contentType: 'text',
                                                        paste: true,
                                                        id: -1,
                                                        deleteAfterUse: false
                                                    });
                                                    setIsManageMode(false);
                                                    setSelectedItemIds(new Set());
                                                }
                                            }}
                                        >
                                            <Copy size={14} />
                                            <span>{t('copy_selected') || '复制选中'}</span>
                                        </button>
                                    </>
                                ) : (
                                    <>
                                        <button
                                            className={`sort-btn manage-btn ${isManageMode ? 'active' : ''}`}
                                            onClick={() => setIsManageMode(true)}
                                            title={t('manage_items') || '管理条目'}
                                        >
                                            <CheckSquare size={14} />
                                            <span>{t('manage') || '管理'}</span>
                                        </button>
                                    </>
                                )}
                            </div>
                        )}
                    <div className="view-toggle">
                        <button
                            type="button"
                            className={`toggle-btn btn-icon ${viewMode === 'list' ? 'active' : ''}`}
                            title="列表视图"
                            onClick={() => setViewMode('list')}
                        ><List size={14} /></button>
                        <button
                            type="button"
                            className={`toggle-btn btn-icon ${viewMode === 'grid' ? 'active' : ''}`}
                            title="卡片视图"
                            onClick={() => setViewMode('grid')}
                        ><LayoutGrid size={14} /></button>
                    </div>
                    </div>
                </div>

                <div className="items-area custom-scrollbar">
                    {loading ? <div className="status-msg">{t('processing')}</div> : sortedItems.length === 0 ? (
                        <div className="status-msg">{selectedTag ? t('no_items') : t('select_tag_to_begin')}</div>
                    ) : (
                        <div className={`items-${viewMode} ${isManageMode ? 'manage-mode' : ''}`}>
                            {sortedItems.map(item => (
                                <div
                                    key={item.id}
                                    className={`themed-card ${selectedItemIds.has(item.id) ? 'selected' : ''}`}
                                    onClick={() => {
                                        if (isManageMode) {
                                            setSelectedItemIds(prev => {
                                                const next = new Set(prev);
                                                if (next.has(item.id)) next.delete(item.id);
                                                else next.add(item.id);
                                                return next;
                                            });
                                        } else {
                                            copyToClipboard(item.id, item.content, item.content_type);
                                        }
                                    }}
                                >
                                    <div className="card-top-row">
                                        <div className="card-actions-left">
                                            {isManageMode ? (
                                                <div className={`selection-indicator ${selectedItemIds.has(item.id) ? 'checked' : ''}`}>
                                                    <div className="inner-check" />
                                                </div>
                                            ) : (
                                                <>
                                                    {/* R4: the edit entry point is offered for every
                                                        content type. For text-like bodies the dialog edits
                                                        the text; for image/file/video it edits the note and
                                                        the body field is not rendered, because those rows
                                                        store a path or a data URL in `content`. */}
                                                    <button className="card-action-btn" title={t('edit_item')} onClick={(e) => {
                                                        e.stopPropagation();
                                                        openItemEditor(item);
                                                    }}>
                                                        <Edit2 size={10} />
                                                    </button>
                                                    <button
                                                        className="card-action-btn"
                                                        onClick={(e) => {
                                                            e.stopPropagation();
                                                            invoke('open_content', {
                                                                id: item.id,
                                                                content: item.content,
                                                                contentType: item.content_type
                                                            });
                                                        }}
                                                        title={t('open')}
                                                    >
                                                        <ExternalLink size={10} />
                                                    </button>
                                                </>
                                            )}
                                        </div>
                                        {!isManageMode && (
                                            <button className="del-btn" title="删除" onClick={(e) => {
                                                e.stopPropagation();
                                                setItemDeleteConfirmation({ show: true, id: item.id });
                                            }}>
                                                <X size={10} />
                                            </button>
                                        )}
                                    </div>

                                    {item.content_type === 'image' ? (
                                        <div className="card-media">
                                            <img
                                                src={item.content.startsWith('data:') ? item.content : convertFileSrc(item.content)}
                                                alt=""
                                                className="image-preview"
                                                loading="lazy"
                                            />
                                        </div>
                                    ) : (
                                        <div className="card-body-text">{item.preview || item.content}</div>
                                    )}

                                    {/* R6: the note is shown on the card, for every content type.
                                        The full text lives in the `title` attribute so a note that
                                        is visually clamped to two lines is still readable on hover,
                                        which is what keeps a 2000-character note from breaking the
                                        grid layout. */}
                                    {item.note ? (
                                        <div className="card-note" title={item.note}>
                                            <Sparkles size={9} />
                                            <span className="card-note-text">{item.note}</span>
                                        </div>
                                    ) : null}

                                    <div className="card-divider" />
                                    <div className="card-footer">
                                        <span className="meta-time">{formatItemDate(item.timestamp)}</span>
                                        <div className="meta-usage"><MousePointer2 size={8} /> {item.use_count || 0}</div>
                                    </div>
                                </div>
                            ))}
                        </div>
                    )}
                </div>
                {selectedTag && !isManageMode && (
                    <button
                        className="fab-add-btn"
                        onClick={(e) => {
                            e.stopPropagation();
                            setIsCreatingItem(true);
                        }}
                        title={t('add_item')}
                    >
                        <Plus size={24} />
                    </button>
                )}
            </div>

            {/* Modals for Create (Rename is handled inline now) */}
            {/* Kept minimal if needed for future extensions, but currently inline handles rename */}

            {/* B9: 标签组的右键菜单。两个动作都复用既有链路——
                「重命名」进入行内编辑态（`editingTag`），「删除」打开下面的二次确认框，
                在这里直接删掉一个组是绝对不允许的。 */}
            {tagMenu && (
                <TagGroupContextMenu
                    x={tagMenu.x}
                    y={tagMenu.y}
                    tagName={tagMenu.tagName}
                    affectedCount={tagMenu.affected}
                    t={t}
                    onRename={() => {
                        setEditingTag(tagMenu.tagName);
                        setNewTagName(tagMenu.tagName);
                    }}
                    onDelete={() => setDeleteConfirmation({
                        show: true,
                        tagName: tagMenu.tagName,
                        affected: tagMenu.affected,
                    })}
                    onClose={() => setTagMenu(null)}
                />
            )}

            {/* Tag Delete Confirmation Modal */}
            {deleteConfirmation.show && (
                <div className="modal-overlay" onClick={() => setDeleteConfirmation({ show: false, tagName: null, affected: 0 })}>
                    <div className={`confirm-dialog tag-manager-dialog theme-${theme}`} onClick={(e) => e.stopPropagation()}>
                        <h3>{t('confirm_delete')}</h3>
                        <p>
                            {t('confirm_delete_tag')}
                            <br />
                            <span className="tag-highlight" style={{ marginTop: '8px', display: 'inline-block' }}>
                                {deleteConfirmation.tagName}
                            </span>
                        </p>
                        {/* R3: deleting a group must never look like deleting data. The
                            impact is stated before the action runs, in the same dialog the
                            user is already reading, rather than in a toast afterwards. */}
                        <p className="tag-delete-scope">
                            {t('confirm_delete_tag_scope').replace(
                                '{count}',
                                String(deleteConfirmation.affected)
                            )}
                        </p>
                        <div className="confirm-dialog-buttons">
                            <button className="confirm-dialog-button" onClick={() => setDeleteConfirmation({ show: false, tagName: null, affected: 0 })}>
                                {t('cancel')}
                            </button>
                            <button className="confirm-dialog-button primary" onClick={() => {
                                if (deleteConfirmation.tagName) {
                                    handleDeleteTag(deleteConfirmation.tagName);
                                }
                                setDeleteConfirmation({ show: false, tagName: null, affected: 0 });
                            }}>
                                {t('delete')}
                            </button>
                        </div>
                    </div>
                </div>
            )}

            {/* Item Delete Confirmation Modal */}
            {itemDeleteConfirmation.show && (
                <div className="modal-overlay" onClick={() => setItemDeleteConfirmation({ show: false, id: null })}>
                    <div className={`confirm-dialog tag-manager-dialog theme-${theme}`} onClick={e => e.stopPropagation()}>
                        <h3>{t('confirm_delete')}</h3>
                        <p>{t('confirm_delete_desc') || "确定要删除这条记录吗？"}</p>
                        <div className="confirm-dialog-buttons">
                            <button className="confirm-dialog-button" onClick={() => setItemDeleteConfirmation({ show: false, id: null })}>
                                {t('cancel')}
                            </button>
                            <button className="confirm-dialog-button primary" onClick={async () => {
                                if (itemDeleteConfirmation.id === -1) {
                                    // Bulk delete
                                    try {
                                        for (const id of Array.from(selectedItemIds)) {
                                            await invoke('delete_clipboard_entry', { id });
                                        }
                                        setIsManageMode(false);
                                        setSelectedItemIds(new Set());
                                        if (selectedTag) await loadTagItems(selectedTag);
                                        emit('clipboard-changed');
                                    } catch (err) { console.error(err); }
                                } else if (itemDeleteConfirmation.id) {
                                    await invoke('delete_clipboard_entry', { id: itemDeleteConfirmation.id });
                                    loadTagItems(selectedTag!);
                                    emit('clipboard-changed');
                                }
                                setItemDeleteConfirmation({ show: false, id: null });
                            }}>
                                {t('delete')}
                            </button>
                        </div>
                    </div>
                </div>
            )}

            {/* Create Item Modal */}
            {isCreatingItem && (
                <div className="modal-overlay" onClick={() => setIsCreatingItem(false)}>
                    <div className={`confirm-dialog tag-manager-dialog theme-${theme}`} onClick={e => e.stopPropagation()}>
                        <h3>{t('add_item')}</h3>
                        <div className="modal-input-field">
                            <textarea
                                className="tag-manager-textarea"
                                value={newItemContent}
                                onChange={e => setNewItemContent(e.target.value)}
                                placeholder={t('input_content_placeholder')}
                                autoFocus
                            />
                        </div>
                        <div className="confirm-dialog-buttons">
                            <button className="confirm-dialog-button" onClick={() => setIsCreatingItem(false)}>
                                {t('cancel')}
                            </button>
                            <button className="confirm-dialog-button primary" onClick={handleAddManualItem}>
                                {t('confirm')}
                            </button>
                        </div>
                    </div>
                </div>
            )}

            {/* Edit Item Modal */}
            {editingItem && (
                <div className="modal-overlay" onClick={() => setEditingItem(null)}>
                    <div className={`confirm-dialog tag-manager-dialog theme-${theme}`} onClick={e => e.stopPropagation()}>
                        <h3>{t('edit_item')}</h3>

                        {/* R4: the body field is rendered only for text-like content.
                            For image/file/video the row stores a path or a data URL, so
                            editing it as text would break the reference; the dialog then
                            offers the note alone instead of a disabled field. */}
                        {isBinaryContentType(editingItem.contentType) ? (
                            <p className="edit-item-body-notice">
                                {t('edit_item_binary_notice')}
                            </p>
                        ) : (
                            <div className="modal-input-field">
                                <label className="edit-item-label">{t('edit_item_content_label')}</label>
                                <textarea
                                    className="tag-manager-textarea"
                                    value={editingItem.content}
                                    onChange={e => setEditingItem({ ...editingItem, content: e.target.value })}
                                    autoFocus
                                />
                                {/* R4: the back end rewrites a rich-text row as plain text and
                                    drops `html_content` whenever its body is edited. Stating the
                                    consequence before saving is the honest option, since it cannot
                                    be undone from the UI. */}
                                {editingItem.contentType === 'rich_text' && (
                                    <p className="edit-item-warning">
                                        <AlertTriangle size={11} />
                                        <span>{t('edit_item_rich_text_warning')}</span>
                                    </p>
                                )}
                            </div>
                        )}

                        {/* R6: the note is editable for every content type and is written
                            through its own command, so it never touches the body. */}
                        <div className="modal-input-field">
                            <label className="edit-item-label">{t('edit_item_note_label')}</label>
                            <textarea
                                className="tag-manager-textarea note-textarea"
                                value={editingItem.note}
                                placeholder={t('edit_item_note_placeholder')}
                                maxLength={MAX_NOTE_CHARS}
                                onChange={e => setEditingItem({ ...editingItem, note: e.target.value })}
                                onKeyDown={e => e.stopPropagation()}
                            />
                            <div className="edit-item-note-meta">
                                <span>{t('edit_item_note_clear_hint')}</span>
                                <span>{editingItem.note.length} / {MAX_NOTE_CHARS}</span>
                            </div>
                        </div>

                        <div className="confirm-dialog-buttons">
                            <button className="confirm-dialog-button" onClick={() => setEditingItem(null)}>
                                {t('cancel')}
                            </button>
                            <button className="confirm-dialog-button primary" onClick={handleSaveItem}>
                                {t('save')}
                            </button>
                        </div>
                    </div>
                </div>
            )}
            <style>{`
                .themed-tag-manager {
                    display: grid;
                    grid-template-columns: var(--tag-sidebar-width, 130px) auto 1fr;
                    height: 100%;
                    background: var(--bg-content);
                    font-family: var(--font-main, ui-monospace, monospace);
                    color: var(--text-primary);
                    gap: 0;
                    padding: 0;
                }

                /* Sidebar */
                .tag-sidebar {
                    width: var(--tag-sidebar-width, 130px);
                    flex-shrink: 0;
                    display: flex;
                    flex-direction: column;
                    background: var(--bg-panel);
                    border-radius: 0;
                    box-shadow: none;
                    overflow: hidden;
                    border: var(--panel-border);
                }
                .sidebar-collapsed .tag-sidebar { width: 48px; }
                
                .sidebar-header {
                    padding: 16px 20px;
                    border-bottom: 1px solid var(--panel-divider-color);
                    display: flex;
                    justify-content: space-between;
                    align-items: center;
                    min-height: auto;
                    background: transparent;
                    color: var(--text-secondary);
                    font-size: 13px;
                    font-weight: 600;
                    text-transform: uppercase;
                    letter-spacing: 0.5px;
                }
                .header-actions { display: flex; align-items: center; gap: 8px; }
                .action-btn { background: transparent; border: none; color: inherit; cursor: pointer; padding: 2px; opacity: 0.7; transition: opacity 0.2s; }
                .action-btn:hover { opacity: 1; }
                .collapse-toggle { 
                    background: var(--bg-main); 
                    border: none; 
                    color: inherit; 
                    cursor: pointer; 
                    display: flex; 
                    align-items: center;
                    justify-content: center;
                    width: 28px;
                    height: 28px;
                    border-radius: var(--radius-sm);
                    transition: all 0.2s;
                }
                .collapse-toggle:hover { background: var(--border-light); color: var(--text-primary); }

                /* Tag Search Box */
                .tag-search-box {
                    padding: 12px 16px;
                    display: flex; align-items: center; gap: 6px;
                    background: transparent;
                    border-bottom: 1px solid var(--panel-divider-color);
                    margin: 0;
                    min-height: auto;
                    position: relative;
                }
                .tag-search-box .search-icon-placeholder { opacity: 0.3; color: var(--text-primary); flex-shrink: 0; }
                .tag-search-box input {
                    width: 100%;
                    background: var(--bg-main); 
                    border: 1px solid var(--border); 
                    outline: none;
                    font-size: 13px; 
                    font-weight: 500; 
                    color: var(--text-primary);
                    padding: 10px 12px 10px 36px;
                    flex: 1;
                    min-width: 0; 
                    border-radius: var(--radius-sm);
                    transition: all 0.2s;
                }
                .tag-search-box input:focus {
                    border-color: var(--accent-color);
                    background: var(--bg-panel);
                    box-shadow: 0 0 0 3px var(--accent-light);
                }
                .tag-search-box input::placeholder { color: var(--text-muted); opacity: 0.7; font-style: normal; font-size: 13px; }
                
                .action-icons { display: flex; align-items: center; gap: 4px; }
                .action-icon { cursor: pointer; opacity: 0.5; color: var(--text-primary); transition: all 0.15s; }
                .action-icon:hover { opacity: 1; transform: scale(1.1); }
                .action-icon.create { color: var(--accent-color); opacity: 0.8; }
                .action-icon.create:hover { opacity: 1; }

                .tag-scroll { flex: 1; overflow-y: auto; padding: 8px; overflow-x: hidden; }
                /* Tag Item Layout: [Color] [Name (Flex)] [Actions (Hover)] [Badge] */
                .tag-item {
                    display: flex; 
                    align-items: center; 
                    gap: 10px;
                    padding: 10px 12px; 
                    cursor: pointer;
                    margin-bottom: 2px; 
                    border: 1px solid transparent;
                    border-radius: var(--radius-sm);
                    transition: all 0.15s;
                    position: relative;
                    overflow: hidden;
                    width: 100%;
                }
                .tag-item:hover { background: var(--bg-main); }
                .tag-item.active { 
                    background: var(--accent-light); 
                    border-color: transparent;
                    box-shadow: none;
                }
                .tag-item.create-hint { border: 1px dashed var(--border); opacity: 0.8; }
                .tag-item.create-hint:hover { background: var(--bg-main); border-style: solid; }

                .sidebar-collapsed .tag-item { justify-content: center; padding: 10px 0; gap: 0; }
                .sidebar-collapsed .tag-name,
                .sidebar-collapsed .tag-badge { display: none; }
                .sidebar-collapsed .tag-color-wrapper { width: 100%; justify-content: center; }
                .tag-color-wrapper { display: flex; align-items: center; justify-content: center; }
                .tag-color-dot { 
                    width: 10px; 
                    height: 10px; 
                    border-radius: 50%; 
                    flex-shrink: 0; 
                    cursor: pointer; 
                    border: none;
                    transition: transform 0.2s; 
                }
                .tag-color-dot:hover { transform: scale(1.2); }
                .tag-name { 
                    flex: 1; 
                    font-size: 13px; 
                    font-weight: 500; 
                    white-space: nowrap; 
                    overflow: hidden; 
                    text-overflow: ellipsis; 
                    min-width: 0; 
                    margin-right: 4px;
                }
                
                /* Inline Edit Input */
                .inline-tag-edit {
                    flex: 1; 
                    border: 1px solid var(--border); 
                    background: var(--bg-main); 
                    color: var(--text-primary); 
                    font-size: 13px; 
                    font-weight: 500;
                    padding: 6px 10px; 
                    border-radius: var(--radius-sm);
                    min-width: 0; 
                    outline: none;
                    box-shadow: 0 0 0 3px var(--accent-light);
                }

                /* B9: the hover action group is gone — rename/delete now live in the
                   right-click menu, so the row no longer swaps its count badge for two
                   icons as the pointer passes over it. */

                .tag-badge { 
                    font-size: 11px; 
                    font-weight: 600; 
                    color: var(--text-secondary); 
                    background: var(--bg-main); 
                    padding: 2px 8px; 
                    border-radius: 10px;
                    min-width: auto;
                    text-align: center;
                }
                .tag-item.active .tag-badge {
                    background: var(--accent-color);
                    color: white;
                }
                
                /* Content Area */
                .tag-content { flex: 1; display: flex; flex-direction: column; overflow: hidden; }
                .content-toolbar {
                    height: 48px; border-bottom: 1px solid var(--panel-divider-color);
                    background: var(--bg-panel);
                    display: flex; align-items: center; justify-content: space-between; padding: 0 16px;
                }
                .toolbar-left { display: flex; align-items: center; gap: 12px; }
                .selected-tag-indicator { display: flex; align-items: center; gap: 6px; font-weight: 600; font-size: 14px; color: var(--text-primary); }
                .breadcrumb-marker { color: var(--accent-color); }

                .sort-group { display: flex; gap: 6px; padding-left: 12px; border-left: 1px solid var(--panel-divider-color); }
                .sort-btn { background: transparent; border: none; color: var(--text-secondary); cursor: pointer; display: flex; align-items: center; gap: 4px; padding: 4px 8px; border-radius: var(--radius-sm); transition: all 0.15s; }
                .sort-btn:hover { background: var(--bg-main); color: var(--text-primary); }
                .sort-btn.active { background: var(--accent-light); color: var(--accent-color); }

                .view-toggle {
                    display: flex;
                    align-items: center;
                    gap: 4px;
                    padding: 2px;
                    border: 1px solid var(--panel-divider-color);
                    border-radius: var(--radius-sm);
                    background: var(--bg-main);
                }
                .toggle-btn {
                    padding: 4px;
                    border-radius: var(--radius-sm);
                    transition: all 0.15s;
                }
                .toggle-btn:hover { background: var(--bg-input); }
                .toggle-btn.active { background: var(--accent-color); color: white; }

                .items-area { 
                    flex: 1; 
                    overflow-y: auto; 
                    padding: 16px 16px 80px 16px; 
                    background: var(--bg-content); 
                    position: relative;
                }

                .items-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(160px, 1fr)); gap: 12px; }
                .items-list { display: flex; flex-direction: column; gap: 8px; }

                .themed-card {
                    background: var(--bg-element);
                    border: 1px solid var(--border);
                    padding: 12px; cursor: pointer;
                    position: relative;
                    border-radius: var(--radius-md);
                    transition: all 0.15s ease;
                }
                .themed-card:hover { transform: translateY(-1px); box-shadow: 0 4px 12px var(--shadow); border-color: var(--accent-color); }

                .del-btn { background: transparent; border: none; color: var(--text-muted); cursor: pointer; opacity: 0.4; transition: opacity 0.15s; }
                .del-btn:hover { opacity: 1; color: #ff4d4f; }

                .card-media { min-height: 60px; border-radius: var(--radius-sm); margin: 8px 0; overflow: hidden; background: var(--bg-main); display: flex; justify-content: center; align-items: center; }
                .card-media img { max-width: 100%; max-height: 140px; object-fit: contain; border-radius: var(--radius-sm); }
                
                .card-body-text { font-size: 13px; line-height: 1.4; display: -webkit-box; -webkit-line-clamp: 4; -webkit-box-orient: vertical; overflow: hidden; word-break: break-word; color: var(--text-primary); }
                .card-footer { display: flex; justify-content: space-between; margin-top: 8px; font-size: 11px; color: var(--text-secondary); opacity: 0.8; }
                .meta-usage { display: flex; align-items: center; gap: 4px; }

                /* R6: per-entry note on a card and its editors.
                   Kept inside this component's own <style> block: the note is a
                   TagManager feature and the shared stylesheet is outside this change.
                   The note is free text up to 2000 chars, so the layout must be
                   indifferent to its length — clamped to two lines here, with the full
                   text in the element title attribute. */
                .card-note { display: flex; align-items: flex-start; gap: 4px; margin-top: 6px; padding: 4px 6px; border-radius: var(--radius-sm); background: var(--bg-main); color: var(--text-secondary); font-size: 10px; line-height: 1.35; }
                .card-note svg { flex-shrink: 0; margin-top: 2px; }
                .card-note-text { flex: 1; min-width: 0; display: -webkit-box; -webkit-line-clamp: 2; -webkit-box-orient: vertical; overflow: hidden; word-break: break-word; white-space: pre-wrap; }
                /* The stacked list renders cards as a grid whose grid-template-areas
                   live in the shared stylesheet, which is outside this change. The note
                   therefore claims a full-width auto row (1 / -1) instead of a named
                   area, so the template stays authoritative and no implicit track is
                   invented next to its columns. */
                .stacked-layout .items-list .card-note { grid-column: 1 / -1; margin-top: 4px; }
                .stacked-layout .items-grid .card-note { font-size: 9px; }
                .note-textarea { min-height: 56px; max-height: 140px; }
                .edit-item-label { display: block; margin-bottom: 4px; color: var(--text-secondary); font-size: 11px; font-weight: 600; }
                .edit-item-body-notice { margin: 0 0 12px; padding: 8px; border-radius: var(--radius-sm); background: var(--bg-element); color: var(--text-secondary); font-size: 11px; line-height: 1.45; }
                .edit-item-warning { display: flex; align-items: flex-start; gap: 4px; margin: 6px 0 0; color: #d08c30; font-size: 10px; line-height: 1.4; }
                .edit-item-warning svg { flex-shrink: 0; margin-top: 2px; }
                .edit-item-note-meta { display: flex; align-items: center; justify-content: space-between; gap: 8px; margin-top: 4px; color: var(--text-secondary); font-size: 10px; }
                .tag-delete-scope { margin: 8px 0 0; color: var(--text-secondary); font-size: 11px; line-height: 1.45; }
                
                .add-item-btn {
                    margin-left: 12px;
                }

                .card-top-row { display: flex; justify-content: space-between; align-items: center; margin-bottom: 8px; }
                .card-actions-left { display: flex; gap: 6px; }
                .card-action-btn {
                    background: transparent;
                    border: none;
                    color: var(--text-secondary);
                    cursor: pointer;
                    display: flex;
                    align-items: center;
                    padding: 4px;
                    border-radius: var(--radius-sm);
                    opacity: 0.6;
                    transition: all 0.15s;
                }
                .card-action-btn:hover { opacity: 1; color: var(--accent-color); background: var(--bg-main); }

                /* Overlay */
                .modal-overlay {
                    position: fixed; top: 0; left: 0; right: 0; bottom: 0;
                    background: rgba(0, 0, 0, 0.4);
                    backdrop-filter: blur(4px);
                    display: flex; align-items: center; justify-content: center;
                    z-index: 2000;
                }


                /* Confirm Dialog - Modern Style */
                .modal-overlay .confirm-dialog {
                    background: var(--bg-panel) !important;
                    padding: 24px;
                    border: 1px solid var(--border) !important;
                    box-shadow: 0 20px 40px rgba(0,0,0,0.15) !important;
                    border-radius: var(--radius-lg) !important;
                    width: 400px;
                    max-width: 90%;
                    animation: modal-pop 0.2s cubic-bezier(0.34, 1.56, 0.64, 1);
                }

                @keyframes modal-pop {
                    0% { transform: scale(0.95); opacity: 0; }
                    100% { transform: scale(1); opacity: 1; }
                }

                .modal-overlay .confirm-dialog h3 {
                    margin: 0 0 16px 0;
                    font-size: 17px;
                    font-weight: 600;
                    background: transparent !important;
                    color: var(--text-primary) !important;
                    padding: 0;
                    display: block;
                    text-transform: none;
                }

                .modal-overlay .confirm-dialog p {
                    margin: 12px 0 24px 0;
                    font-size: 14px;
                    font-weight: 400;
                    line-height: 1.5;
                    color: var(--text-secondary);
                }

                .modal-overlay .confirm-dialog-buttons {
                    display: flex;
                    justify-content: flex-end;
                    gap: 8px;
                }

                .modal-overlay .confirm-dialog-button {
                    padding: 8px 16px;
                    font-size: 13px;
                    font-weight: 500;
                    cursor: pointer;
                    background: var(--bg-main) !important;
                    border: 1px solid var(--border) !important;
                    color: var(--text-primary) !important;
                    box-shadow: none !important;
                    transition: all 0.15s;
                    border-radius: var(--radius-sm);
                }
                .modal-overlay .confirm-dialog-button:hover {
                    background: var(--border-light) !important;
                }
                .modal-overlay .confirm-dialog-button:active {
                    transform: scale(0.98);
                    box-shadow: none !important;
                }

                .modal-overlay .confirm-dialog-button.primary {
                    background: var(--accent-color) !important;
                    color: #fff !important;
                    border: none !important;
                }
                .modal-overlay .confirm-dialog-button.primary:hover {
                    background: var(--accent-color-dark) !important;
                }

                /* Modern Theme Polishes for Confirm Dialog */
                .theme-mica .confirm-dialog,
                .theme-acrylic .confirm-dialog {
                    background: rgba(255, 255, 255, 0.8) !important;
                    backdrop-filter: blur(20px);
                    padding: 24px !important;
                    border-radius: 16px !important;
                    box-shadow: 0 10px 40px rgba(0,0,0,0.2) !important;
                    border: 1px solid rgba(255,255,255,0.4) !important;
                    animation: modal-pop-modern 0.3s cubic-bezier(0.34, 1.56, 0.64, 1) !important;
                }
                
                @keyframes modal-pop-modern {
                    0% { transform: scale(0.95); opacity: 0; }
                    100% { transform: scale(1); opacity: 1; }
                }

                .theme-mica .confirm-dialog h3,
                .theme-acrylic .confirm-dialog h3 {
                    background: transparent !important;
                    color: var(--text-primary) !important;
                    font-size: 18px !important;
                    font-weight: 700 !important;
                    text-transform: none !important;
                    padding: 0 !important;
                }

                .theme-mica .confirm-dialog-button,
                .theme-acrylic .confirm-dialog-button {
                    border-radius: 10px !important;
                    border: none !important;
                    box-shadow: none !important;
                    font-weight: 600 !important;
                    background: rgba(0,0,0,0.05) !important;
                }
                .theme-mica .confirm-dialog-button:active,
                .theme-acrylic .confirm-dialog-button:active {
                    transform: scale(0.95);
                }

                .theme-mica .confirm-dialog-button.primary,
                .theme-acrylic .confirm-dialog-button.primary {
                    background: var(--accent-color) !important;
                }

                /* Dark Mode Adaptation */
                .dark-mode .modal-overlay .confirm-dialog {
                    background: #1f1f1f !important;
                    border-color: #000 !important;
                }
                .dark-mode .modal-overlay .confirm-dialog h3 {
                    color: #fff !important;
                }
                .dark-mode .modal-overlay .confirm-dialog p {
                    color: #d1d1d1 !important;
                }
                .dark-mode .theme-mica .confirm-dialog,
                .dark-mode .theme-acrylic .confirm-dialog {
                    background: rgba(30,30,30,0.8) !important;
                    border-color: rgba(255,255,255,0.1) !important;
                }

                .modal-input-field input {
                    width: 100%; 
                    background: var(--bg-main);
                    border: 1px solid var(--border);
                    padding: 12px; 
                    color: var(--text-primary);
                    font-family: inherit; 
                    font-size: 14px; 
                    font-weight: 400;
                    outline: none; 
                    margin-bottom: 20px;
                    border-radius: var(--radius-sm);
                    transition: all 0.2s;
                }
                .modal-input-field input:focus {
                    border-color: var(--accent-color);
                    box-shadow: 0 0 0 3px var(--accent-light);
                }
                .modal-buttons { display: flex; gap: 8px; justify-content: flex-end; }
                .modal-buttons button {
                    padding: 8px 16px; 
                    cursor: pointer;
                    font-size: 13px; 
                    font-weight: 500;
                    border: 1px solid var(--border);
                    background: var(--bg-main);
                    color: var(--text-primary);
                    box-shadow: none;
                    transition: all 0.15s;
                    border-radius: var(--radius-sm);
                }
                .modal-buttons button:active { transform: scale(0.98); }
                .btn-save { background: var(--accent-color); color: white; border: none; }
                .btn-save:hover { background: var(--accent-color-dark); }
                
                /* Modern Theme Polishes */
                .theme-mica.themed-tag-manager,
                .theme-acrylic.themed-tag-manager {
                    gap: 14px;
                    padding: 14px;
                    background: transparent !important;
                    overflow: hidden;
                }

                .theme-mica .tag-sidebar,
                .theme-acrylic .tag-sidebar {
                    width: clamp(196px, 24%, 248px);
                    border: var(--panel-border);
                    border-radius: 24px;
                    background: var(--bg-panel);
                    box-shadow: var(--panel-shadow);
                    overflow: hidden;
                }

                .theme-mica.sidebar-collapsed .tag-sidebar,
                .theme-acrylic.sidebar-collapsed .tag-sidebar {
                    width: 64px;
                }

                .theme-mica .sidebar-header,
                .theme-acrylic .sidebar-header {
                    min-height: 88px;
                    padding: 24px 24px 18px;
                    background: transparent;
                    border-bottom: 1px solid var(--panel-divider-color);
                }

                .theme-mica .header-label,
                .theme-acrylic .header-label {
                    font-size: 18px;
                    font-weight: 700;
                    letter-spacing: 0;
                }

                .theme-mica .collapse-toggle,
                .theme-acrylic .collapse-toggle {
                    width: 40px;
                    height: 40px;
                    border: 1px solid rgba(var(--accent-color-rgb), 0.12);
                    border-radius: 14px;
                    background: var(--bg-input);
                    color: var(--text-secondary);
                    box-shadow: none;
                }

                .theme-mica .collapse-toggle:hover,
                .theme-acrylic .collapse-toggle:hover {
                    background: rgba(var(--accent-color-rgb), 0.08);
                    color: var(--text-primary);
                }

                .theme-mica .tag-search-box,
                .theme-acrylic .tag-search-box {
                    margin: 18px 16px;
                    min-height: 56px;
                    padding: 0 16px;
                    gap: 12px;
                    border: var(--input-border);
                    border-radius: 16px;
                    background: var(--bg-input);
                    box-shadow: var(--input-shadow);
                }

                .theme-mica .tag-search-box .search-icon-placeholder,
                .theme-acrylic .tag-search-box .search-icon-placeholder {
                    opacity: 0.78;
                    color: var(--text-secondary);
                }

                .theme-mica .tag-search-box input,
                .theme-acrylic .tag-search-box input {
                    padding: 0;
                    font-size: 15px;
                    font-weight: 600;
                }

                .theme-mica .tag-search-box input::placeholder,
                .theme-acrylic .tag-search-box input::placeholder {
                    font-size: 15px;
                    font-style: normal;
                    opacity: 0.72;
                }

                .theme-mica .action-icons,
                .theme-acrylic .action-icons {
                    gap: 8px;
                }

                .theme-mica .action-icon,
                .theme-acrylic .action-icon {
                    display: flex;
                    align-items: center;
                    justify-content: center;
                    width: 28px;
                    height: 28px;
                    border-radius: 999px;
                    background: rgba(var(--accent-color-rgb), 0.08);
                    color: var(--text-secondary);
                    opacity: 1;
                }

                .theme-mica .action-icon:hover,
                .theme-acrylic .action-icon:hover {
                    background: rgba(var(--accent-color-rgb), 0.14);
                    color: var(--accent-color);
                    transform: none;
                }

                .theme-mica .tag-scroll,
                .theme-acrylic .tag-scroll {
                    padding: 4px 12px 16px;
                }

                .theme-mica .tag-item,
                .theme-acrylic .tag-item {
                    min-height: 60px;
                    padding: 14px 16px;
                    margin-bottom: 6px;
                    border: 1px solid transparent;
                    border-radius: 16px;
                    background: transparent;
                }

                .theme-mica .tag-item:hover,
                .theme-acrylic .tag-item:hover {
                    background: rgba(var(--accent-color-rgb), 0.06);
                    border-color: rgba(var(--accent-color-rgb), 0.12);
                }

                .theme-mica .tag-item.active,
                .theme-acrylic .tag-item.active {
                    background: rgba(var(--accent-color-rgb), 0.12);
                    border-color: rgba(var(--accent-color-rgb), 0.16);
                    box-shadow: none;
                }

                .theme-mica .tag-color-dot,
                .theme-acrylic .tag-color-dot {
                    width: 14px;
                    height: 14px;
                    border: none;
                    box-shadow: inset 0 0 0 1px rgba(255, 255, 255, 0.28);
                }

                .theme-mica .tag-name,
                .theme-acrylic .tag-name {
                    font-size: 15px;
                    font-weight: 700;
                }

                /* B9: the mica/acrylic overrides for the removed hover action group went
                   with the markup. The context menu is styled from root-level tokens in
                   tag-group-menu.css, because a portal to body is not a descendant of these
                   theme classes and would never match selectors written here. */

                .theme-mica .tag-badge,
                .theme-acrylic .tag-badge {
                    margin-left: auto;
                    min-width: 34px;
                    height: 30px;
                    padding: 0 10px;
                    border-radius: 999px;
                    background: rgba(127, 140, 160, 0.1);
                    color: var(--text-secondary);
                    font-size: 14px;
                    font-weight: 700;
                    display: inline-flex;
                    align-items: center;
                    justify-content: center;
                    opacity: 1;
                }

                .theme-mica .tag-item.active .tag-badge,
                .theme-acrylic .tag-item.active .tag-badge {
                    background: var(--accent-color);
                    color: #ffffff;
                }

                .theme-mica .tag-content,
                .theme-acrylic .tag-content {
                    min-width: 0;
                    border: var(--panel-border);
                    border-radius: 28px;
                    background: var(--bg-panel);
                    box-shadow: var(--panel-shadow);
                }

                .theme-mica .content-toolbar,
                .theme-acrylic .content-toolbar {
                    min-height: 88px;
                    padding: 20px 28px;
                    border-bottom: 1px solid var(--panel-divider-color);
                    background: transparent;
                }

                .theme-mica .toolbar-left,
                .theme-mica .toolbar-right,
                .theme-acrylic .toolbar-left,
                .theme-acrylic .toolbar-right {
                    display: flex;
                    align-items: center;
                    gap: 14px;
                }

                .theme-mica .toolbar-right,
                .theme-acrylic .toolbar-right {
                    margin-left: auto;
                }

                .theme-mica .toolbar-divider,
                .theme-acrylic .toolbar-divider {
                    width: 1px;
                    height: 28px;
                    background: var(--panel-divider-color);
                }

                .theme-mica .selected-tag-indicator,
                .theme-acrylic .selected-tag-indicator {
                    padding: 8px 16px;
                    border-radius: var(--radius-pill);
                    background: rgba(var(--accent-color-rgb), 0.12);
                    border: 1px solid rgba(var(--accent-color-rgb), 0.16);
                    color: var(--accent-color);
                    font-size: 14px;
                    font-weight: 600;
                    gap: 10px;
                    opacity: 1;
                }

                .theme-mica .breadcrumb-text,
                .theme-acrylic .breadcrumb-text {
                    color: var(--text-primary);
                }

                .theme-mica .sort-group,
                .theme-acrylic .sort-group {
                    gap: 10px;
                    padding-left: 0;
                    border-left: none;
                }

                .theme-mica .sort-btn,
                .theme-acrylic .sort-btn {
                    min-height: 40px;
                    padding: 0 16px;
                    border: 1px solid rgba(var(--accent-color-rgb), 0.14);
                    border-radius: 14px;
                    background: transparent;
                    color: var(--text-secondary);
                    box-shadow: none;
                    display: inline-flex;
                    align-items: center;
                    justify-content: center;
                    gap: 8px;
                }

                .theme-mica .sort-btn span,
                .theme-acrylic .sort-btn span {
                    font-size: 13px;
                    font-weight: 500;
                }

                .theme-mica .sort-btn:hover,
                .theme-acrylic .sort-btn:hover {
                    background: rgba(var(--accent-color-rgb), 0.08);
                    color: var(--text-primary);
                }

                .theme-mica .sort-btn.active,
                .theme-acrylic .sort-btn.active {
                    background: var(--accent-color);
                    border-color: var(--accent-color);
                    color: #ffffff;
                    box-shadow: 0 12px 24px rgba(var(--accent-color-rgb), 0.24);
                }

                .theme-mica .add-item-btn,
                .theme-acrylic .add-item-btn {
                    width: auto !important;
                    min-height: 40px;
                    padding: 0 18px;
                    border: none;
                    border-radius: 14px;
                    background: var(--accent-color);
                    color: #ffffff;
                    box-shadow: 0 12px 24px rgba(var(--accent-color-rgb), 0.26);
                    gap: 8px;
                    font-size: 14px;
                    font-weight: 600;
                }

                .theme-mica .add-item-btn span,
                .theme-acrylic .add-item-btn span {
                    display: inline-block;
                }

                .theme-mica .add-item-btn:hover,
                .theme-acrylic .add-item-btn:hover {
                    background: var(--accent-hover);
                    color: #ffffff;
                }

                .theme-mica .view-toggle,
                .theme-acrylic .view-toggle {
                    padding: 4px;
                    gap: 4px;
                    border: 1px solid rgba(var(--accent-color-rgb), 0.12);
                    border-radius: 18px;
                    background: var(--bg-input);
                }

                .theme-mica .toggle-btn,
                .theme-acrylic .toggle-btn {
                    width: 44px;
                    height: 44px;
                    padding: 0;
                    border: none;
                    border-radius: 14px;
                    background: transparent;
                    color: var(--text-secondary);
                    box-shadow: none;
                }

                .theme-mica .toggle-btn:hover,
                .theme-acrylic .toggle-btn:hover {
                    background: rgba(var(--accent-color-rgb), 0.08);
                    color: var(--text-primary);
                }

                .theme-mica .toggle-btn.active,
                .theme-acrylic .toggle-btn.active {
                    background: var(--accent-color);
                    color: #ffffff;
                    box-shadow: 0 10px 20px rgba(var(--accent-color-rgb), 0.2);
                }

                .theme-mica .items-area,
                .theme-acrylic .items-area {
                    padding: 28px;
                    background: transparent;
                }

                .theme-mica .status-msg,
                .theme-acrylic .status-msg {
                    padding: 36px 12px;
                    text-align: center;
                    color: var(--text-secondary);
                    font-size: 14px;
                }

                .theme-mica .items-grid,
                .theme-acrylic .items-grid {
                    grid-template-columns: repeat(auto-fill, minmax(240px, 1fr));
                    gap: 22px;
                }

                .theme-mica .items-list,
                .theme-acrylic .items-list {
                    display: grid;
                    grid-template-columns: 1fr;
                    gap: 18px;
                }

                .theme-mica .themed-card,
                .theme-acrylic .themed-card {
                    position: relative;
                    min-height: 244px;
                    padding: 24px 22px 18px;
                    border: 1px solid rgba(var(--accent-color-rgb), 0.08);
                    border-radius: 22px;
                    background: var(--bg-input);
                    box-shadow: 0 12px 28px rgba(15, 23, 42, 0.06);
                    display: flex;
                    flex-direction: column;
                    transition: transform 0.18s ease, box-shadow 0.18s ease, border-color 0.18s ease;
                }

                .theme-mica .themed-card:hover,
                .theme-acrylic .themed-card:hover {
                    transform: translateY(-2px);
                    border-color: rgba(var(--accent-color-rgb), 0.14);
                    box-shadow: 0 18px 34px rgba(15, 23, 42, 0.1);
                    background: var(--bg-input);
                }

                .theme-mica .items-list .themed-card,
                .theme-acrylic .items-list .themed-card {
                    min-height: 180px;
                }

                .theme-mica .card-top-row,
                .theme-acrylic .card-top-row {
                    position: absolute;
                    top: 14px;
                    right: 14px;
                    display: flex;
                    align-items: center;
                    gap: 6px;
                    opacity: 0;
                    transition: opacity 0.18s ease;
                    z-index: 1;
                }

                .theme-mica .themed-card:hover .card-top-row,
                .theme-acrylic .themed-card:hover .card-top-row {
                    opacity: 1;
                }

                .theme-mica .card-actions-left,
                .theme-acrylic .card-actions-left {
                    gap: 6px;
                }

                .theme-mica .card-action-btn,
                .theme-mica .del-btn,
                .theme-acrylic .card-action-btn,
                .theme-acrylic .del-btn {
                    width: 28px;
                    height: 28px;
                    padding: 0;
                    border: 1px solid rgba(var(--accent-color-rgb), 0.08);
                    border-radius: 999px;
                    background: rgba(255, 255, 255, 0.88);
                    color: var(--text-secondary);
                    box-shadow: none;
                    opacity: 1;
                }

                .theme-mica .card-action-btn:hover,
                .theme-mica .del-btn:hover,
                .theme-acrylic .card-action-btn:hover,
                .theme-acrylic .del-btn:hover {
                    background: rgba(var(--accent-color-rgb), 0.12);
                    color: var(--accent-color);
                }

                .theme-mica .card-body-text,
                .theme-acrylic .card-body-text {
                    flex: 1;
                    padding-top: 12px;
                    font-size: 15px;
                    line-height: 1.7;
                    font-weight: 500;
                    color: var(--text-primary);
                    -webkit-line-clamp: 5;
                    min-height: 122px;
                }

                .theme-mica .items-list .card-body-text,
                .theme-acrylic .items-list .card-body-text {
                    -webkit-line-clamp: 3;
                    min-height: 84px;
                }

                .theme-mica .card-media,
                .theme-acrylic .card-media {
                    flex: 1;
                    min-height: 190px;
                    margin-top: 14px;
                    border: none;
                    border-radius: 18px;
                    background: rgba(127, 140, 160, 0.12);
                    align-items: center;
                }

                .theme-mica .card-media img,
                .theme-acrylic .card-media img {
                    max-width: 100%;
                    max-height: 190px;
                    object-fit: contain;
                    border-radius: 14px;
                }

                .theme-mica .card-divider,
                .theme-acrylic .card-divider {
                    height: 1px;
                    margin: 18px 0 14px;
                    background: var(--panel-divider-color);
                }

                .theme-mica .card-footer,
                .theme-acrylic .card-footer {
                    margin-top: auto;
                    font-size: 13px;
                    font-weight: 600;
                    color: var(--text-secondary);
                    opacity: 1;
                }

                .theme-mica .meta-usage,
                .theme-acrylic .meta-usage {
                    gap: 4px;
                }

                .theme-mica .inline-tag-edit,
                .theme-acrylic .inline-tag-edit,
                .theme-mica .modal-input-field input,
                .theme-acrylic .modal-input-field input {
                    border: var(--input-border);
                    border-radius: var(--input-radius);
                    box-shadow: var(--input-shadow);
                    padding: 8px 10px;
                    outline: none;
                }

                .theme-mica .modal-input-field textarea,
                .theme-acrylic .modal-input-field textarea {
                    background: var(--bg-input) !important;
                    border: var(--input-border) !important;
                    border-radius: 16px !important;
                    box-shadow: var(--input-shadow) !important;
                    color: var(--text-primary) !important;
                }

                .theme-mica .modal-buttons button,
                .theme-acrylic .modal-buttons button {
                    border: var(--button-border);
                    border-radius: var(--button-radius);
                    box-shadow: var(--button-shadow);
                }

                .dark-mode .theme-mica .tag-search-box,
                .dark-mode .theme-acrylic .tag-search-box,
                .dark-mode .theme-mica .tag-sidebar,
                .dark-mode .theme-acrylic .tag-sidebar,
                .dark-mode .theme-mica .tag-content,
                .dark-mode .theme-acrylic .tag-content {
                    border-color: rgba(255, 255, 255, 0.08);
                }

                .dark-mode .theme-mica .card-action-btn,
                .dark-mode .theme-mica .del-btn,
                .dark-mode .theme-acrylic .card-action-btn,
                .dark-mode .theme-acrylic .del-btn {
                    background: rgba(22, 28, 39, 0.92);
                    border-color: rgba(255, 255, 255, 0.08);
                }

                .dark-mode .theme-mica .tag-badge,
                .dark-mode .theme-acrylic .tag-badge {
                    background: rgba(255, 255, 255, 0.08);
                }
                
                .custom-scrollbar::-webkit-scrollbar { width: var(--scrollbar-size-thin); }
                .custom-scrollbar::-webkit-scrollbar-thumb { background: var(--scrollbar-thumb-color); border-radius: var(--scrollbar-radius); }

                @media (max-width: 320px) {
                    .themed-tag-manager {
                        flex-direction: column;
                        padding: 8px;
                        gap: 12px;
                        overflow-y: auto;
                    }
                    .tag-sidebar {
                        width: 100% !important;
                        height: 240px;
                        flex-shrink: 0;
                    }
                    .tag-content {
                        min-height: 300px;
                    }
                }

                .tag-divider {
                    width: 4px;
                    height: 100%;
                    cursor: col-resize;
                    margin: 0;
                    background: transparent;
                    transition: background 0.2s;
                    position: relative;
                    z-index: 10;
                    display: flex;
                    align-items: center;
                    justify-content: center;
                }
                .tag-divider:hover, .tag-divider.active {
                    background: rgba(var(--accent-color-rgb), 0.1);
                }
                .tag-divider-handle {
                    width: 1px;
                    height: 32px;
                    background: var(--accent-color);
                    opacity: 0.22;
                    border-radius: 2px;
                    transition: opacity 0.2s, height 0.2s;
                }
                .tag-divider:hover .tag-divider-handle, .tag-divider.active .tag-divider-handle {
                    opacity: 0.6;
                    height: 48px;
                }

                /* Multi-selection Management Styles */
                .toolbar-actions {
                    display: flex;
                    align-items: center;
                    gap: 8px;
                }

                .manage-btn {
                    margin-left: 12px !important;
                }
                @media (max-width: 500px) {
                    .manage-btn span {
                        display: none;
                    }
                }


                .sort-btn.danger {
                    color: #ff4d4f !important;
                }
                .sort-btn.danger:hover:not(:disabled) {
                    background: rgba(255, 77, 79, 0.1) !important;
                }
                .sort-btn:disabled {
                    opacity: 0.4;
                    cursor: not-allowed;
                }
                
                .theme-mica .sort-btn.danger:hover:not(:disabled),
                .theme-acrylic .sort-btn.danger:hover:not(:disabled) {
                    background: rgba(255, 77, 79, 0.15);
                }

                .selection-indicator {
                    width: 20px;
                    height: 20px;
                    border: 2px solid var(--border);
                    border-radius: 6px;
                    display: flex;
                    align-items: center;
                    justify-content: center;
                    transition: all 0.2s;
                    background: var(--bg-main);
                }
                .selection-indicator.checked {
                    background: var(--accent-color);
                    border-color: var(--accent-color);
                }
                .inner-check {
                    width: 7px;
                    height: 4px;
                    border-left: 2px solid white;
                    border-bottom: 2px solid white;
                    transform: rotate(-45deg);
                    opacity: 0;
                    transition: opacity 0.2s;
                    margin-top: -2px;
                }
                .selection-indicator.checked .inner-check {
                    opacity: 1;
                }

                .manage-mode .themed-card {
                    border-color: var(--border);
                }
                .manage-mode .themed-card:hover {
                    border-color: var(--accent-light);
                    transform: none;
                    box-shadow: none;
                }
                .manage-mode .themed-card.selected {
                    background: rgba(var(--accent-color-rgb), 0.05);
                    border-color: var(--accent-color);
                    box-shadow: 0 0 0 1px var(--accent-color);
                }

                /* Ensure card top row shows up in manage mode to hold selection indicator */
                .manage-mode .themed-card .card-top-row {
                    opacity: 1 !important;
                }
                .manage-mode .card-actions-left {
                    display: flex !important;
                }
                .manage-mode .card-top-row {
                    right: auto !important;
                    left: 14px !important;
                }

                /* Premium adjustments for modern themes */
                .theme-mica .manage-btn, .theme-acrylic .manage-btn {
                    min-height: 40px;
                    border-radius: 14px;
                    padding: 0 16px;
                    background: var(--bg-input) !important;
                    margin-left: 0;
                }

                .theme-mica .action-btn-primary, .theme-acrylic .action-btn-primary,
                .theme-mica .action-btn-danger, .theme-acrylic .action-btn-danger,
                .theme-mica .action-btn-secondary, .theme-acrylic .action-btn-secondary {
                    min-height: 40px;
                    border-radius: 14px;
                }

                .theme-mica .selection-indicator, .theme-acrylic .selection-indicator {
                    border-radius: 8px;
                    border-color: rgba(var(--accent-color-rgb), 0.2);
                }
                
                .theme-mica .manage-mode .themed-card.selected,
                .theme-acrylic .manage-mode .themed-card.selected {
                    background: rgba(var(--accent-color-rgb), 0.08);
                    box-shadow: 0 12px 28px rgba(var(--accent-color-rgb), 0.15);
                }

                .fab-add-btn {
                    position: absolute;
                    bottom: 20px;
                    right: 20px;
                    width: 44px;
                    height: 44px;
                    border-radius: 50%;
                    background: var(--accent-color);
                    color: white;
                    border: none;
                    display: flex;
                    align-items: center;
                    justify-content: center;
                    cursor: pointer;
                    box-shadow: 0 4px 12px rgba(var(--accent-color-rgb), 0.3);
                    transition: all 0.2s cubic-bezier(0.34, 1.56, 0.64, 1);
                    z-index: 100;
                    opacity: 0.85;
                }
                .fab-add-btn:hover {
                    background: var(--accent-hover);
                    transform: scale(1.08) translateY(-2px);
                    box-shadow: 0 12px 24px rgba(var(--accent-color-rgb), 0.4);
                    opacity: 1;
                }
                .fab-add-btn:active {
                    transform: scale(0.95);
                }
                
                .theme-mica .fab-add-btn, .theme-acrylic .fab-add-btn {
                    width: 48px;
                    height: 48px;
                    background: var(--accent-color);
                    box-shadow: 0 10px 24px rgba(var(--accent-color-rgb), 0.4);
                    border: 1px solid rgba(255, 255, 255, 0.25);
                }
                
                /* Ensure tag-content is the anchor for FAB */
                .tag-content { 
                    position: relative; 
                }

            `}</style>
        </div >
    );
}
