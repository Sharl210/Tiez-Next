import { useState, useEffect, useRef, useMemo, useCallback } from 'react';
import { invoke, convertFileSrc } from '@tauri-apps/api/core';
import { listen, emit } from '@tauri-apps/api/event';
import {
    Edit2, Trash2, X, ChevronRight, LayoutGrid, List,
    Clock, MousePointer2, ChevronLeft, Plus, Search, ExternalLink, CheckSquare, Copy,
    Sparkles, StickyNote
} from 'lucide-react';
import { getTagColor } from "../../../shared/lib/utils";
import type { ClipboardEntry } from "../../../shared/types";
import {
    isBodyEditable,
    isNoteEditable,
    MAX_ENTRY_NOTE_CHARS,
} from "../../clipboard/types";
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

/**
 * 标签管理页卡片的「编辑内容 / 编辑备注」按钮可见性。
 *
 * # 为什么这两个判据必须从 `features/clipboard/types` 引入，而不是本文件自己写一份
 *
 * 本文件原来自带一套 `BINARY_CONTENT_TYPES = ['image','file','video']` 和一个自实现的
 * 二元判据函数。它与主页面用的判据**不等价**：
 *
 *   - `isNoteEditable` 是"可编辑正文类型的**补集**"，所以 `emoji_sync`、
 *     以及后端将来新增的任何类型，**都**算它命中；
 *   - 那套自实现的判据是只含三个类型的白名单，上述类型**都落空**。
 *
 * 于是同一个条目在主页面有备注按钮、在标签管理页没有 —— v0.5.4 已经因为
 * "两处各写一套判据"踩过一次这个分叉，这里不再重复。
 *
 * # 为什么 `canEditNote` 是"两个判据的并集"，而不是 `isNoteEditable` 单独一个
 *
 * 用户的要求是「编辑备注内容**每个条目都要有这个按钮**」，而 `isNoteEditable`
 * 按定义把 `text` / `code` / `url` / `rich_text` 排除在外（实测这四个返回 `false`）——
 * 单独用它，这四类条目就**没有**备注入口，直接违背这句话。
 *
 * 另一条路是复制 `EDITABLE_BODY_TYPES` 的清单自己判，那正是本注释开头说的分叉。
 * 所以这里取两个**同源**判据的并集：`isBodyEditable` 命中 → 它有正文入口，
 * 但它**仍然需要一个独立的备注入口**（两个按钮是分开的）；`isNoteEditable` 命中 →
 * 它只有备注入口。两者合起来 = 每一条都有备注入口，且不新增任何本地清单。
 */
export const resolveCardEditActions = (contentType: string | undefined | null) => {
    const type = contentType ?? '';
    return {
        /** 「编辑内容」按钮：只有正文是文本的类型才显示（白名单，未预见的类型不获得正文写入口）。 */
        canEditBody: isBodyEditable(type),
        /**
         * 「编辑备注」按钮：**每个条目**都显示。
         *
         * 直接用同源判据，不做 `|| isBodyEditable(type)` 的补丁 —— `isNoteEditable`
         * 本身已经是恒真（备注是条目元数据，与内容类型无关）。在这里再或一次，
         * 会让"备注为什么显示"这件事有两个来源，下次任一边改动就会出现第三种组合。
         */
        canEditNote: isNoteEditable(type),
    };
};

/** 卡片编辑弹窗的两种模式：只改正文，或只改备注。 */
/**
 * R13：把纯文本转义成可放进 contentEditable 的 HTML。
 *
 * 用于 `rich_text` 行**没有** `html_content` 的历史数据：直接把纯文本塞进
 * contentEditable 会让文本里的 `<` 被当成标签吃掉，转义后再写才与用户看到的一致。
 */
export const escapeHtmlForEditor = (text: string): string =>
    text
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/\n/g, '<br>');

/**
 * R13：从富文本 HTML 里取出**纯文本正文**。
 *
 * 为什么界面也要做一次：`content` 是派生的纯文本（粘贴与列表预览用的就是它）。
 * 富文本编辑器改的是 HTML，若直接把 `innerHTML` 当正文送去，`content` 里会存下
 * `<p>…</p>`，与界面显示的正文不符。后端以同一口径再派生一次作为权威值，
 * 这里派生是为了让"送去的内容"与"最终落库的内容"一致，避免脏检查误判。
 *
 * 用 `DOMParser` 而不是正则：标签嵌套与实体转义（`&amp;`、`&nbsp;`）正则会算错。
 */
export const htmlToPlainText = (html: string): string => {
    if (!html) return '';
    const doc = new DOMParser().parseFromString(html, 'text/html');
    // 块级元素之间补换行，否则两段文字会粘成一行。
    doc.querySelectorAll('br').forEach(br => br.replaceWith('\n'));
    doc.querySelectorAll('p, div, li, tr, h1, h2, h3, h4, h5, h6, blockquote, pre')
        .forEach(el => el.append('\n'));
    return (doc.body.textContent ?? '').replace(/\n{3,}/g, '\n\n').trim();
};

export type CardEditMode = 'body' | 'note';

/**
 * 编辑弹窗的保存计划 —— 抽成纯函数，好让"备注弹窗绝不会写正文"这条不变量
 * 可以被直接断言，而不是只能靠"界面上没渲染那个输入框"间接推断。
 *
 * 两个写入**各自受自己的模式门控**：
 *   - `body` 模式只可能写正文，`note` 模式只可能写备注；
 *   - 再叠加脏检查，避免"打开就保存"也发一次没有意义的写命令。
 *
 * 只靠脏检查是不够的：`note` 模式里 `content` 与 `originalContent` 恒等，
 * 于是把模式门控删掉也"看起来没错"，直到某天弹窗开始预填正文草稿为止。
 * 所以门控必须显式存在，并由单测直接钉住。
 */
export const resolveEditSavePlan = (edit: {
    mode: CardEditMode;
    content: string;
    note: string;
    originalContent: string;
    originalNote: string;
    /**
     * R13：富文本条目的 HTML。可选 —— 非富文本条目根本不带，此时下面的比较退化成
     * "两边的 undefined 相等"，判据与加富文本之前逐字一致。
     */
    html?: string;
    originalHtml?: string;
}): { writeBody: boolean; writeNote: boolean } => ({
    // R13：只改格式（加粗、换色）时 `content` 一字未变，若只看正文就会把改动当
    // "无变化"丢弃 —— 用户点了保存，格式却没落盘。所以正文的差异**或** HTML 的差异
    // 任一成立都算写正文。
    writeBody:
        edit.mode === 'body' &&
        (edit.content !== edit.originalContent || edit.html !== edit.originalHtml),
    writeNote: edit.mode === 'note' && edit.note !== edit.originalNote,
});

/** R6: mirror of `MAX_ENTRY_NOTE_CHARS`（与主页面共用同一常量，不再各自写 2000）。 */
const MAX_NOTE_CHARS = MAX_ENTRY_NOTE_CHARS;

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
/**
 * 标签分组的排序方式。
 *
 * `default` 是原行为（按条目数从多到少）——用户要求"保留默认方式选项"，所以它必须
 * 是一个显式可选项，而不是"没有选择"。
 */
export type TagGroupSort =
    | 'name' | 'name_desc'
    | 'recent' | 'recent_asc'
    | 'count' | 'count_asc'
    | 'size' | 'size_asc';

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
    /** 标签分组的排序方式：纯界面偏好，与 view_mode 同类，存在 localStorage。 */
    const TAG_GROUP_SORT_KEY = "tiez_tag_group_sort";
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

    /**
     * 标签**分组**的排序方式（与上面 `sortBy` 无关——那个排的是组内条目）。
     *
     * `default` 即当前行为：按条目数从多到少，也就是后端返回后原有的排序。
     * 惰性初始化，保证首帧就是用户选过的顺序，不会先按默认渲染再跳变。
     */
    /**
     * 每个标签的统计量（最近使用时间、总字节数），用于排序。
     * 由 `get_tag_stats` 提供；拿不到时排序自动退化到只按名称/条目数。
     */
    const [tagStats, setTagStats] = useState<Record<string, { last_used_at: number; total_bytes: number }>>({});

    const [tagSort, setTagSort] = useState<TagGroupSort>(() => {
        try {
            const saved = window.localStorage.getItem(TAG_GROUP_SORT_KEY);
            const allowed: TagGroupSort[] = [
                'name', 'name_desc', 'recent', 'recent_asc',
                'count', 'count_asc', 'size', 'size_asc',
            ];
            if (allowed.includes(saved as TagGroupSort)) {
                return saved as TagGroupSort;
            }
        } catch {
            // 读不到就用默认；这只是界面偏好，不值得打扰用户。
        }
        // 默认按名称 A-Z：顺序可预期，找起来比"条目多的在前"更快。
        return 'name';
    });

    const changeTagSort = useCallback((next: TagGroupSort) => {
        setTagSort(next);
        try {
            window.localStorage.setItem(TAG_GROUP_SORT_KEY, next);
        } catch {
            // 存不下不影响本次会话内的排序生效。
        }
    }, []);
    const [isCreatingItem, setIsCreatingItem] = useState(false);
    /**
     * R4/R6/R12: the edit dialog serves every content type, in one of **two modes**.
     *
     * `mode` is what keeps the two features from drifting into each other: a body
     * editor renders only the body field and only ever writes the body; a note editor
     * renders only the note field and only ever writes the note. `originalContent` /
     * `originalNote` are still kept so each mode can skip a write it did not change
     * (an untouched `update_item_content` would be a no-op that still emits a refresh).
     */
    const [editingItem, setEditingItem] = useState<{
        id: number;
        mode: CardEditMode;
        content: string;
        note: string;
        contentType: string;
        originalContent: string;
        originalNote: string;
        /**
         * R13：富文本条目的 HTML。标签管理页此前根本不读 `html_content`，
         * 于是这里的"编辑正文"也一定会把格式丢掉（与主页面是两个独立入口，
         * 只修一个，另一个仍会降级）。
         */
        html?: string;
        originalHtml?: string;
    } | null>(null);
    const [newItemContent, setNewItemContent] = useState('');
    /**
     * R13：富文本编辑器的 DOM 节点（非受控 —— 见 `openItemEditor` 附近的说明）。
     */
    const richBodyEditorRef = useRef<HTMLDivElement | null>(null);

    /**
     * R13：把 `editingItem.html` 的初值写进 contentEditable。
     *
     * 只在弹窗（重新）打开时写一次：`editingItem.id` 与 `editingItem.mode` 变化即代表
     * 换了一条记录或换了模式，此时必须重写初值；否则用户丢弃的草稿会在重开时复活。
     * 之后不再写 —— 受控写入会把光标推到开头并打断中文输入法。
     */
    useEffect(() => {
        if (!editingItem || editingItem.mode !== 'body') return;
        if (editingItem.contentType !== 'rich_text') return;
        const node = richBodyEditorRef.current;
        if (!node) return;
        if (node.innerHTML !== (editingItem.html ?? '')) {
            node.innerHTML = editingItem.html ?? '';
        }
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [editingItem?.id, editingItem?.mode, editingItem?.contentType]);
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

            // 统计量单独取一次：失败不影响标签列表本身，排序退化为名称/条目数。
            invoke<Array<{ name: string; last_used_at: number; total_bytes: number }>>("get_tag_stats")
                .then((rows) => {
                    const map: Record<string, { last_used_at: number; total_bytes: number }> = {};
                    (rows || []).forEach((r) => {
                        map[r.name] = { last_used_at: r.last_used_at, total_bytes: r.total_bytes };
                    });
                    setTagStats(map);
                })
                .catch(() => {
                    // 保持既有 map；排序函数对缺失项有兜底。
                });
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
     * R4/R6/R12: save the edit dialog — **only the field its mode owns**.
     *
     * # 为什么必须按模式分开写，而不是"哪个变了就写哪个"
     *
     * 上一版是一个弹窗同时渲染正文与备注，保存时对两者各做一次脏检查。本轮把它们
     * 拆成两个按钮／两种模式之后，"备注弹窗顺手保存正文"就成了必须堵死的回归：
     * 用户点「编辑备注」时并不打算碰正文，一旦保存路径仍按脏检查走，任何让正文草稿
     * 与原文不一致的情形（预填、格式化、未来新增的字段同步）都会**静默重写正文**。
     *
     * 所以写入由 `resolveEditSavePlan` 按模式门控：`body` 模式最多写正文，
     * `note` 模式最多写备注。脏检查只在其上叠加，用来省掉没有意义的写命令。
     */
    const handleSaveItem = async () => {
        if (!editingItem) return;
        const plan = resolveEditSavePlan(editingItem);
        if (!plan.writeBody && !plan.writeNote) {
            setEditingItem(null);
            return;
        }
        // 正文不允许被清空（备注可以，清空即删除备注）。
        if (plan.writeBody && !editingItem.content.trim()) return;

        try {
            if (plan.writeBody) {
                // R13：富文本条目把编辑后的 HTML 一起提交，后端保持 `rich_text` 类型并
                // 写入 `html_content` —— 这正是"编辑富文本不会坍缩成纯文本"。
                //
                // `newContent` 仍要送：后端以它 + HTML 一起**派生**权威的纯文本正文；
                // 只送 HTML 会让"HTML 为空但正文非空"这类边界无处表达。
                const isRich = editingItem.contentType === 'rich_text';
                await invoke('update_item_content', {
                    id: editingItem.id,
                    newContent: isRich ? htmlToPlainText(editingItem.html ?? '') : editingItem.content,
                    htmlContent: isRich ? editingItem.html : undefined,
                });
            }
            if (plan.writeNote) {
                await invoke('update_entry_note', { id: editingItem.id, note: editingItem.note });
            }
            setEditingItem(null);
            if (selectedTag) await loadTagItems(selectedTag);
        } catch (err) { console.error(err); }
    };

    /**
     * R4/R6/R12: open the edit dialog for one card, in exactly one mode.
     *
     * Both fields are seeded with the entry's real values so the dialog never shows a
     * stale or blank draft; the *mode* then decides which one is rendered and which
     * one the save path is allowed to write.
     */
    const openItemEditor = (item: ClipboardEntry, mode: CardEditMode) => {
        const note = item.note || '';
        // R13：富文本条目的 HTML 初值。取库里的 `html_content`；空则用纯文本转义后的
        // 兜底，保证编辑器里不会显示空白（历史数据里存在只有 content 的 rich_text 行）。
        const html = item.content_type === 'rich_text'
            ? (item.html_content ?? escapeHtmlForEditor(item.content))
            : undefined;
        setEditingItem({
            id: item.id,
            mode,
            content: item.content,
            note,
            contentType: item.content_type,
            originalContent: item.content,
            originalNote: note,
            html,
            originalHtml: html,
        });
    };

    const copyToClipboard = async (id: number, content: string, type: string) => {
        try {
            // 不传 moveToTop → 后端回落用户设置（app.move_to_top_after_paste）。
            // 此前这里硬编码 `moveToTop: true`，绕过了用户设置（该设置在用户的库里
            // 是 false），导致"在标签管理页点一下条目就跳到第一条"——用户以为是
            // 编辑导致的，实际是这里的粘贴置顶。编辑本身不改排序键（timestamp）。
            await invoke('copy_to_clipboard', { content, contentType: type, paste: true, id, deleteAfterUse: false });
        } catch (err) { console.error(err); }
    };

    const filteredTags = useMemo(() => {
        const matched = tags.filter(t => t.name.toLowerCase().includes(tagSearch.toLowerCase()));
        // `default` 保持后端给的顺序（按条目数降序），不改动；其余选项在这里重排。
        // 稳定排序：`name` 用 localeCompare 保证中文按拼音、英文按字母。
        const byName = (a: TagInfo, b: TagInfo) => a.name.localeCompare(b.name);
        const stat = (n: string) => tagStats[n] ?? { last_used_at: 0, total_bytes: 0 };
        switch (tagSort) {
            case 'name_desc':
                return [...matched].sort((a, b) => byName(b, a));
            case 'count':
                return [...matched].sort((a, b) => b.count - a.count || byName(a, b));
            case 'count_asc':
                return [...matched].sort((a, b) => a.count - b.count || byName(a, b));
            case 'recent':
                // 从未使用过的标签（时间戳 0）排在最后，而不是混在中间。
                return [...matched].sort(
                    (a, b) => stat(b.name).last_used_at - stat(a.name).last_used_at || byName(a, b)
                );
            case 'recent_asc':
                // 最早使用的在前；同样把"从未使用"（0）放最后，不让它冒充最早。
                return [...matched].sort((a, b) => {
                    const av = stat(a.name).last_used_at;
                    const bv = stat(b.name).last_used_at;
                    if (av === 0 && bv === 0) return byName(a, b);
                    if (av === 0) return 1;
                    if (bv === 0) return -1;
                    return av - bv || byName(a, b);
                });
            case 'size':
                return [...matched].sort(
                    (a, b) => stat(b.name).total_bytes - stat(a.name).total_bytes || byName(a, b)
                );
            case 'size_asc':
                return [...matched].sort(
                    (a, b) => stat(a.name).total_bytes - stat(b.name).total_bytes || byName(a, b)
                );
            case 'name':
            default:
                return [...matched].sort(byName);
        }
    }, [tags, tagSearch, tagSort, tagStats]);

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
                    {/* 排序选择：位于标题右侧、收起按钮左侧。收起状态下标签文字消失，
                        这个按钮也一并隐藏——窄到只剩一条竖栏时没有它的位置。 */}
                    {!isCollapsed && (
                        <select
                            className="tag-sort-select"
                            value={tagSort}
                            title={t('tag_sort') || '排序方式'}
                            aria-label={t('tag_sort') || '排序方式'}
                            onChange={(e) => changeTagSort(e.target.value as TagGroupSort)}
                        >
                            <option value="name">{t('tag_sort_name') || 'A-Z'}</option>
                            <option value="name_desc">{t('tag_sort_name_desc') || 'Z-A'}</option>
                            <option value="recent">{t('tag_sort_recent') || '最近使用'}</option>
                            <option value="recent_asc">{t('tag_sort_recent_asc') || '最早使用'}</option>
                            <option value="count">{t('tag_sort_count') || '按条目数 多→少'}</option>
                            <option value="count_asc">{t('tag_sort_count_asc') || '按条目数 少→多'}</option>
                            <option value="size">{t('tag_sort_size') || '按体积 大→小'}</option>
                            <option value="size_asc">{t('tag_sort_size_asc') || '按体积 小→大'}</option>
                        </select>
                    )}
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
                                { /* 与左侧 `create` 同构：`title` 挂在 `<span>` 上而不是图标上。
                                    lucide 图标不收 `title` prop；且 SVG 上的 title 元素
                                    不保证产生原生悬浮提示，挂在容器上才可靠。 */}
                                <span
                                    className="action-icon clear"
                                    title={t('tooltip_clear_search')}
                                    onClick={() => setTagSearch('')}
                                >
                                    <X size={12} />
                                </span>
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
                            title={t('list_view')}
                            onClick={() => setViewMode('list')}
                        ><List size={14} /></button>
                        <button
                            type="button"
                            className={`toggle-btn btn-icon ${viewMode === 'grid' ? 'active' : ''}`}
                            title={t('grid_view')}
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
                            {sortedItems.map(item => {
                                // R12: 每张卡的按钮可见性只算一次，且判据与主页面同源。
                                const editActions = resolveCardEditActions(item.content_type);
                                return (
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
                                                    {/* R12: 两个**独立**的编辑入口，可见性由共享判据决定
                                                        （`resolveCardEditActions` → `isBodyEditable` /
                                                        `isNoteEditable`）：
                                                          - 「编辑内容」只有正文是文本的类型才有
                                                            （`text`/`code`/`url`/`rich_text`）；
                                                          - 「编辑备注」**每个条目**都有 ——
                                                            `emoji_sync` 与未预见的类型同样包含在内。
                                                        两个按钮各自只开自己那种模式的弹窗，也各自只写
                                                        自己那一个字段（见 `handleSaveItem`）。 */}
                                                    {editActions.canEditBody && (
                                                        <button
                                                            className="card-action-btn"
                                                            data-testid="card-edit-body"
                                                            title={t('edit_item')}
                                                            onClick={(e) => {
                                                                e.stopPropagation();
                                                                openItemEditor(item, 'body');
                                                            }}
                                                        >
                                                            <Edit2 size={10} />
                                                        </button>
                                                    )}
                                                    {editActions.canEditNote && (
                                                        <button
                                                            className="card-action-btn"
                                                            data-testid="card-edit-note"
                                                            title={t('edit_item_note_label')}
                                                            onClick={(e) => {
                                                                e.stopPropagation();
                                                                openItemEditor(item, 'note');
                                                            }}
                                                        >
                                                            <StickyNote size={10} />
                                                        </button>
                                                    )}
                                                    <button
                                                        className="card-action-btn"
                                                        data-testid="card-open"
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
                                            <button className="del-btn" title={t('delete')} onClick={(e) => {
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
                                );
                            })}
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

            {/* Edit Item Modal —— R12：两种模式，**各自只渲染自己那一个字段**。
                「编辑内容」进来只看得到正文，「编辑备注」进来只看得到备注框；
                标题、正文与保存路径都由 `editingItem.mode` 决定，两个入口不会
                再共用同一个把两样东西混在一起的弹窗。 */}
            {editingItem && (
                <div className="modal-overlay" onClick={() => setEditingItem(null)}>
                    <div
                        className={`confirm-dialog tag-manager-dialog theme-${theme}`}
                        data-testid={`item-editor-${editingItem.mode}`}
                        onClick={e => e.stopPropagation()}
                    >
                        <h3>{t(editingItem.mode === 'note' ? 'edit_item_note_title' : 'edit_item_body_title')}</h3>

                        {editingItem.mode === 'body' ? (
                            <div className="modal-input-field">
                                <label className="edit-item-label">{t('edit_item_content_label')}</label>
                                {editingItem.contentType === 'rich_text' ? (
                                    /*
                                     * R13：富文本条目用 contentEditable 编辑，保存时读回 `innerHTML`。
                                     *
                                     * 这里是**第二个入口**（主页面弹窗是第一个）。两处都要改：
                                     * 只改主页面的话，从标签管理页编辑同一个富文本条目仍会把格式丢掉，
                                     * 而且用户会以为"功能没修好"。
                                     *
                                     * 非受控写法（只在打开时写一次初值）的理由与主页面一致：
                                     * 受控的 contentEditable 会把光标推到开头、打断中文输入法。
                                     */
                                    <div
                                        ref={richBodyEditorRef}
                                        className="tag-manager-textarea tag-manager-rich-textarea"
                                        contentEditable
                                        suppressContentEditableWarning
                                        autoFocus
                                        role="textbox"
                                        aria-multiline="true"
                                        data-testid="tag-manager-rich-editor"
                                        onInput={e => {
                                            const html = (e.target as HTMLElement).innerHTML;
                                            // 正文同步派生：界面上的"内容"与粘贴出去的文字
                                            // 必须一致，否则用户改完格式会发现粘出来是另一回事。
                                            setEditingItem({ ...editingItem, html, content: htmlToPlainText(html) });
                                        }}
                                    />
                                ) : (
                                    <textarea
                                        className="tag-manager-textarea"
                                        value={editingItem.content}
                                        onChange={e => setEditingItem({ ...editingItem, content: e.target.value })}
                                        autoFocus
                                    />
                                )}
                            </div>
                        ) : (
                            <div className="modal-input-field">
                                <label className="edit-item-label">{t('edit_item_note_label')}</label>
                                <textarea
                                    className="tag-manager-textarea note-textarea"
                                    value={editingItem.note}
                                    placeholder={t('edit_item_note_placeholder')}
                                    maxLength={MAX_NOTE_CHARS}
                                    onChange={e => setEditingItem({ ...editingItem, note: e.target.value })}
                                    onKeyDown={e => e.stopPropagation()}
                                    autoFocus
                                />
                                <div className="edit-item-note-meta">
                                    <span>{t('edit_item_note_clear_hint')}</span>
                                    <span>{editingItem.note.length} / {MAX_NOTE_CHARS}</span>
                                </div>
                                {/* 正文不是文本的类型（图片/文件/视频等）没有「编辑内容」按钮，
                                    这里说明一句，免得用户以为正文入口丢了。 */}
                                {!isBodyEditable(editingItem.contentType) && (
                                    <p className="edit-item-body-notice">
                                        {t('edit_item_binary_notice')}
                                    </p>
                                )}
                            </div>
                        )}

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
                    grid-template-columns: var(--tm-sidebar-width, 130px) auto 1fr;
                    height: 100%;
                    background: var(--bg-content);
                    font-family: var(--font-main, ui-monospace, monospace);
                    color: var(--text-primary);
                    gap: 0;
                    padding: 0;
                }

                /* Sidebar */
                .tag-sidebar {
                    width: var(--tm-sidebar-width, 130px);
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
                    background: var(--bg-input); 
                    border: none; 
                    color: inherit; 
                    cursor: pointer; 
                    display: flex; 
                    align-items: center;
                    justify-content: center;
                    width: 28px;
                    height: 28px;
                    border-radius: var(--data-panel-radius);
                    transition: all 0.2s;
                }
                .collapse-toggle:hover { background: var(--border-light); color: var(--text-primary); }
                /* 排序选择器：与 .collapse-toggle 同高，配色沿用侧栏既有令牌。
                   刻意去掉原生外观，否则在深色主题下会是一块突兀的系统控件。 */
                .tag-sort-select {
                    background: var(--bg-input);
                    color: inherit;
                    border: none;
                    border-radius: var(--data-panel-radius);
                    height: 28px;
                    padding: 0 4px;
                    font-size: 11px;
                    font-weight: 600;
                    cursor: pointer;
                    opacity: 0.75;
                    transition: opacity 0.2s, background 0.2s;
                    max-width: 108px;
                }
                .tag-sort-select:hover { opacity: 1; background: var(--border-light); }
                .tag-sort-select:focus-visible { outline: 1px solid var(--accent-color); }
                .tag-sort-select option { background: var(--bg-input); color: var(--text-primary); }

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
                    background: var(--bg-input); 
                    border: 1px solid var(--line-soft); 
                    outline: none;
                    font-size: 13px; 
                    font-weight: 500; 
                    color: var(--text-primary);
                    padding: 10px 12px 10px 36px;
                    flex: 1;
                    min-width: 0; 
                    border-radius: var(--data-panel-radius);
                    transition: all 0.2s;
                }
                .tag-search-box input:focus {
                    border-color: var(--accent-color);
                    background: var(--bg-panel);
                    box-shadow: var(--input-focus-shadow);
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
                    border-radius: var(--data-panel-radius);
                    transition: all 0.15s;
                    position: relative;
                    overflow: hidden;
                    width: 100%;
                }
                .tag-item:hover { background: var(--bg-input); }
                .tag-item.active { 
                    background: var(--card-selected-background); 
                    border-color: transparent;
                    box-shadow: none;
                }
                .tag-item.create-hint { border: 1px dashed var(--line-soft); opacity: 0.8; }
                .tag-item.create-hint:hover { background: var(--bg-input); border-style: solid; }

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
                    border: 1px solid var(--line-soft); 
                    background: var(--bg-input); 
                    color: var(--text-primary); 
                    font-size: 13px; 
                    font-weight: 500;
                    padding: 6px 10px; 
                    border-radius: var(--data-panel-radius);
                    min-width: 0; 
                    outline: none;
                    box-shadow: var(--input-focus-shadow);
                }

                /* B9: the hover action group is gone — rename/delete now live in the
                   right-click menu, so the row no longer swaps its count badge for two
                   icons as the pointer passes over it. */

                .tag-badge { 
                    font-size: 11px; 
                    font-weight: 600; 
                    color: var(--text-secondary); 
                    background: var(--bg-input); 
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
                .sort-btn { background: transparent; border: none; color: var(--text-secondary); cursor: pointer; display: flex; align-items: center; gap: 4px; padding: 4px 8px; border-radius: var(--data-panel-radius); transition: all 0.15s; }
                .sort-btn:hover { background: var(--bg-input); color: var(--text-primary); }
                .sort-btn.active { background: var(--card-selected-background); color: var(--accent-color); }

                .view-toggle {
                    display: flex;
                    align-items: center;
                    gap: 4px;
                    padding: 2px;
                    border: 1px solid var(--panel-divider-color);
                    border-radius: var(--data-panel-radius);
                    background: var(--bg-input);
                }
                .toggle-btn {
                    padding: 4px;
                    border-radius: var(--data-panel-radius);
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
                    border: 1px solid var(--line-soft);
                    padding: 12px; cursor: pointer;
                    position: relative;
                    border-radius: var(--card-radius);
                    transition: all 0.15s ease;
                }
                .themed-card:hover { transform: translateY(-1px); box-shadow: var(--shadow-sm); border-color: var(--accent-color); }

                .del-btn { background: transparent; border: none; color: var(--text-muted); cursor: pointer; opacity: 0.4; transition: opacity 0.15s; }
                .del-btn:hover { opacity: 1; color: #ff4d4f; }

                .card-media { min-height: 60px; border-radius: var(--data-panel-radius); margin: 8px 0; overflow: hidden; background: var(--bg-input); display: flex; justify-content: center; align-items: center; }
                .card-media img { max-width: 100%; max-height: 140px; object-fit: contain; border-radius: var(--data-panel-radius); }
                
                .card-body-text { font-size: 13px; line-height: 1.4; display: -webkit-box; -webkit-line-clamp: 4; -webkit-box-orient: vertical; overflow: hidden; word-break: break-word; color: var(--text-primary); }
                .card-footer { display: flex; justify-content: space-between; margin-top: 8px; font-size: 11px; color: var(--text-secondary); opacity: 0.8; }
                .meta-usage { display: flex; align-items: center; gap: 4px; }

                /* R6: per-entry note on a card and its editors.
                   Kept inside this component's own <style> block: the note is a
                   TagManager feature and the shared stylesheet is outside this change.
                   The note is free text up to 2000 chars, so the layout must be
                   indifferent to its length — clamped to two lines here, with the full
                   text in the element title attribute. */
                .card-note { display: flex; align-items: flex-start; gap: 4px; margin-top: 6px; padding: 4px 6px; border-radius: var(--data-panel-radius); background: var(--bg-input); color: var(--text-secondary); font-size: 10px; line-height: 1.35; }
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
                .edit-item-body-notice { margin: 0 0 12px; padding: 8px; border-radius: var(--data-panel-radius); background: var(--bg-element); color: var(--text-secondary); font-size: 11px; line-height: 1.45; }
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
                    border-radius: var(--data-panel-radius);
                    opacity: 0.6;
                    transition: all 0.15s;
                }
                .card-action-btn:hover { opacity: 1; color: var(--accent-color); background: var(--bg-input); }

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
                    border: 1px solid var(--line-soft) !important;
                    box-shadow: 0 20px 40px rgba(0,0,0,0.15) !important;
                    border-radius: var(--modal-radius) !important;
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
                    background: var(--bg-input) !important;
                    border: 1px solid var(--line-soft) !important;
                    color: var(--text-primary) !important;
                    box-shadow: none !important;
                    transition: all 0.15s;
                    border-radius: var(--data-panel-radius);
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
                    background: var(--accent-hover) !important;
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
                    background: var(--bg-input);
                    border: 1px solid var(--line-soft);
                    padding: 12px; 
                    color: var(--text-primary);
                    font-family: inherit; 
                    font-size: 14px; 
                    font-weight: 400;
                    outline: none; 
                    margin-bottom: 20px;
                    border-radius: var(--data-panel-radius);
                    transition: all 0.2s;
                }
                .modal-input-field input:focus {
                    border-color: var(--accent-color);
                    box-shadow: var(--input-focus-shadow);
                }
                .modal-buttons { display: flex; gap: 8px; justify-content: flex-end; }
                .modal-buttons button {
                    padding: 8px 16px; 
                    cursor: pointer;
                    font-size: 13px; 
                    font-weight: 500;
                    border: 1px solid var(--line-soft);
                    background: var(--bg-input);
                    color: var(--text-primary);
                    box-shadow: none;
                    transition: all 0.15s;
                    border-radius: var(--data-panel-radius);
                }
                .modal-buttons button:active { transform: scale(0.98); }
                .btn-save { background: var(--accent-color); color: white; border: none; }
                .btn-save:hover { background: var(--accent-hover); }
                
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
                    border: 2px solid var(--line-soft);
                    border-radius: 6px;
                    display: flex;
                    align-items: center;
                    justify-content: center;
                    transition: all 0.2s;
                    background: var(--bg-input);
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
                    border-color: var(--line-soft);
                }
                .manage-mode .themed-card:hover {
                    border-color: var(--card-selected-border-color);
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
