/**
 * 坍缩量测台 —— 把**真实**的 `ClipboardItem` 按内容类型各挂一份，供 `collapse.mjs`
 * 用系统 Chrome 量真实布局高度与按钮矩阵。
 *
 * # 为什么必须量真实高度
 *
 * 「不坍缩」是一个**几何**断言：「组件渲染了」不代表它有高度。
 * `.content-preview` 是 `-webkit-box` 四行截断 + `overflow:hidden`；富文本预览可能
 * 走 `<img>` 快照、`HtmlContent` 注入、或纯文本兜底三条路径；编辑器弹窗是
 * `position:fixed` 的 flex 居中盒。以上任何一处高度归零，DOM 结构、类名、文案
 * **全都完全正确** —— 只有 `getBoundingClientRect()` 能看见。
 * 本仓库已有同类教训：`var()` 取不到值时整条声明被静默丢弃，DOM 断言全绿而界面上
 * 什么都没有（见 `migmeasure.mjs` 的头注释）。
 *
 * # 组件代码一行不改
 *
 * 与验证台其余页面同一约定：只替换宿主 API（`vite.config.mjs` 的 alias），
 * 被测组件源码不动，因此量到的几何就是真机上的几何。编辑能力的判据
 * （`isBodyEditable` / `isNoteEditable`）也从**真实模块**导入，不在这里另抄一份
 * 列表 —— 抄一份就会变成"量测台自己写的规则"，量出来的是量测台的假设。
 *
 * # 与真机的边界（必须如实说明）
 *
 * - `invoke` 是 mock、`listen` 是 no-op ⇒ 只证明**渲染与样式**，不证明命令行为；
 * - 真机上条目位于 react-virtuoso 的滚动容器内，本页是普通纵向流 ⇒
 *   两者对「行高」的影响经评审认为可忽略（Virtuoso 的 item 包装层是
 *   `height:auto` 的普通 div，`height:100%` 百分比在不定高父级上按 `auto` 处理），
 *   但真机未验证，报告中如实标注；
 * - 编辑器弹窗走 `createPortal` 到 `document.body`，与真机路径一致；
 * - 真机主窗口默认 352×380（`tauri.conf.json`），本页视口由量测脚本给出。
 *
 * # 参数
 *
 * - `?case=<id>`  只渲染一个用例（量测脚本逐用例访问，避免互相干扰）
 * - `?compact=1`  紧凑模式
 * - `?open=body|note` 挂载后自动打开该条目的正文编辑器 / 备注编辑器
 */

import React from "react";
import ReactDOM from "react-dom/client";

// ---- 真实样式，与 src/main.tsx 的加载顺序一致 ----
import "../../../src/index.css";
import "../../../src/styles/components/index.css";
import "../../../src/styles/themes/load";

// ---- 真实组件与真实判据（一字未改）----
import ClipboardItem from "../../../src/features/clipboard/components/ClipboardItem";
import { isBodyEditable, isNoteEditable } from "../../../src/features/clipboard/types";
import { translations } from "../../../src/locales";
import type { ClipboardEntry } from "../../../src/shared/types";

const params = new URLSearchParams(location.search);
const lang = (params.get("lang") ?? "zh") as "zh" | "en" | "tw";
const theme = params.get("theme") ?? "mica";
const colorMode = params.get("colorMode") ?? "light";
const compactMode = params.get("compact") === "1";
const openEditor = params.get("open") ?? "";

/** 标签编辑态：`?tagopen=1&tagquery=i` —— 供标签候补浮层的量测使用。 */
const tagOpen = params.get("tagopen") === "1";
const tagQuery = params.get("tagquery") ?? "";

/**
 * 量测用的标签池。
 *
 * 刻意给足 40 条、且多条包含同一个字母：候补列表的**可见高度上限是 4 行**，
 * 若池子太小，"超出可滚动"这条根本量不到（列表本身就填不满 4 行），
 * 于是"限高失效"这类缺陷会被假绿掩盖。
 */
const HARNESS_TAG_POOL = [
  "ims", "img", "invoice", "ims配置", "ims下发", "image",
  ...Array.from({ length: 34 }, (_, i) => `tag-${i}`),
];
const onlyCase = params.get("case") ?? "";

/** 与 `App.tsx` 的 `t` 同一约定：查不到就返回键名本身。 */
const t = (key: string): string => {
  const dict = translations[lang] as unknown as Record<string, string>;
  return dict[key] ?? (translations.zh as unknown as Record<string, string>)[key] ?? key;
};

document.documentElement.classList.add(`theme-${theme}`, `${colorMode}-mode`);
document.body.classList.add(`theme-${theme}`, `${colorMode}-mode`);

const baseEntry = (over: Partial<ClipboardEntry>): ClipboardEntry => ({
  id: 1,
  content_type: "text",
  content: "示例内容",
  source_app: "harness",
  timestamp: Date.now(),
  preview: "示例内容",
  is_pinned: false,
  tags: [],
  ...over,
});

/** 一段**正常**的富文本：走 `HtmlContent`（非表格、非快照）分支。 */
const RICH_HTML = "<p>第一段正文，用来把预览撑出真实高度。</p><p>第二段正文。</p>";

/** 一段**表格**富文本：代码会选 `getRichTextSnapshotDataUrl` 的 `<img>` 快照分支。 */
const RICH_TABLE_HTML =
  "<table><tr><th>列一</th><th>列二</th></tr>" +
  "<tr><td>a</td><td>b</td></tr><tr><td>c</td><td>d</td></tr>" +
  "<tr><td>e</td><td>f</td></tr><tr><td>g</td><td>h</td></tr></table>";

/**
 * 一段**图片坏掉**的富文本。
 *
 * 这条是坍缩的高危路径：`<img src="data:...">` 解不出来时，`<img>` 既没有固有尺寸
 * 也没有显式高度，在 `max-height:64px` 的约束下会量到 **0 高**；代码靠 700ms 的
 * 兜底计时器切回 HTML 渲染，中间的窗口期整行会塌回「只剩元信息 + 标签」。
 */
const RICH_BROKEN_IMG_HTML =
  '<p>下面本来应该有一张图。</p><img src="data:image/png;base64,AAAA" alt="broken" />';

/** 一段**只剩 Office 噪声**的富文本：清洗后没有可渲染节点，走纯文本兜底。 */
const RICH_NOISE_HTML =
  "<style>/* style definitions */ table.mso{mso-style-name:'表格';mso-padding-alt:0cm;}</style>";

/**
 * ⚠️ 坍缩高危夹具 1：**唯一内容是一张加载不出来的图**的富文本。
 *
 * `HtmlContent` 在 `preview={true}` 时**不下发 `minHeight`**
 * （源码里是 `minHeight: isVisible ? undefined : (preview ? previewPlaceholderMinHeight : '100px')`，
 * 而 preview 模式下 `isVisible` 初值就是 true ⇒ 三元永远走 `undefined`）。
 * 列表行正是用 `preview={true}` 渲染的，因此这一段没有任何高度下限：
 * 图加载失败 → `<img>` 量到 0 高 → 整个预览 0 高 → 整行塌回「元信息 + 按钮」。
 *
 * `fallbackText` 给了非空值，正是为了排除"兜底文案救回来"的偶然性。
 */
const RICH_IMG_ONLY_HTML = '<img src="data:image/png;base64,AAAA" alt="broken" />';

/**
 * ⚠️ 坍缩高危夹具 2：**渲染成零高内容**的富文本（空 `div` / 空 `span`）。
 *
 * Office 与部分编辑器的 HTML 里常见这种"有元素、没文字"的片段。
 * `sanitizeHTML` 的 `hasRenderableElement` 会因此判定"可渲染"，于是走
 * `innerHTML` 注入 —— 注入的却是一个 0 高的盒子。
 */
const RICH_EMPTY_HTML = '<div><span></span></div>';

/**
 * ⚠️ 坍缩高危夹具 3：**图片条目指向一个不存在的文件**。
 *
 * `ClipboardItem` 的 `<img onError>` 里做的是
 * `e.currentTarget.style.display = 'none'` 并给父元素加 `image-load-error`。
 * 那个类名**全仓库没有任何 CSS 规则**（只有这一行 JS 在加），
 * 所以图片一失败，预览里就什么都不剩 —— 除非有高度下限兜住。
 */
const MISSING_IMAGE_PATH = "C:\\Users\\me\\Pictures\\definitely-not-here.png";

type CaseSpec = {
  id: string;
  note: string;
  entry: ClipboardEntry;
};

/** 覆盖后端 schema 允许的**全部** 8 种类型，外加一个未知类型（矩阵默认值）。 */
const CASES: CaseSpec[] = [
  { id: "text", note: "纯文本", entry: baseEntry({ id: 101, content_type: "text", content: "一条普通的文本内容，长度足够撑出一行。", preview: "一条普通的文本内容，长度足够撑出一行。" }) },
  { id: "code", note: "代码", entry: baseEntry({ id: 102, content_type: "code", content: "fn main() {\n    println!(\"hi\");\n}", preview: "fn main() {" }) },
  { id: "url", note: "链接", entry: baseEntry({ id: 103, content_type: "url", content: "https://example.com/a/b", preview: "https://example.com/a/b" }) },
  { id: "rich_html", note: "富文本（HTML 分支）", entry: baseEntry({ id: 104, content_type: "rich_text", content: "第一段正文\n第二段正文", preview: "第一段正文", html_content: RICH_HTML }) },
  { id: "rich_table", note: "富文本（快照 <img> 分支）", entry: baseEntry({ id: 105, content_type: "rich_text", content: "表格", preview: "表格", html_content: RICH_TABLE_HTML }) },
  { id: "rich_broken_img", note: "富文本（内嵌图片坏掉）", entry: baseEntry({ id: 106, content_type: "rich_text", content: "下面本来应该有一张图。", preview: "下面本来应该有一张图。", html_content: RICH_BROKEN_IMG_HTML }) },
  { id: "rich_noise", note: "富文本（仅 Office 噪声）", entry: baseEntry({ id: 107, content_type: "rich_text", content: "兜底纯文本", preview: "兜底纯文本", html_content: RICH_NOISE_HTML }) },
  { id: "rich_img_only", note: "富文本（唯一内容是一张坏图）← 坍缩高危", entry: baseEntry({ id: 113, content_type: "rich_text", content: "兜底纯文本", preview: "兜底纯文本", html_content: RICH_IMG_ONLY_HTML }) },
  { id: "rich_empty", note: "富文本（渲染成零高内容）← 坍缩高危", entry: baseEntry({ id: 114, content_type: "rich_text", content: "兜底纯文本", preview: "兜底纯文本", html_content: RICH_EMPTY_HTML }) },
  { id: "image", note: "图片", entry: baseEntry({ id: 108, content_type: "image", content: "data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7", preview: "图片" }) },
  { id: "image_missing", note: "图片（文件不存在）← 坍缩高危", entry: baseEntry({ id: 115, content_type: "image", content: MISSING_IMAGE_PATH, preview: "definitely-not-here.png", is_external: true, file_preview_exists: true }) },
  { id: "file", note: "文件", entry: baseEntry({ id: 109, content_type: "file", content: "C:\\Users\\me\\Documents\\report.pdf", preview: "report.pdf" }) },
  { id: "video", note: "视频", entry: baseEntry({ id: 110, content_type: "video", content: "C:\\Users\\me\\Videos\\clip.mp4", preview: "clip.mp4" }) },
  { id: "emoji_sync", note: "表情同步载荷（非文本、非 image/file/video）", entry: baseEntry({ id: 111, content_type: "emoji_sync", content: "[{\"path\":\"/x.png\"}]", preview: "⭐ Emoji Sync" }) },
  { id: "unknown_type", note: "未知类型（矩阵默认值）", entry: baseEntry({ id: 112, content_type: "future_kind", content: "未知类型的内容", preview: "未知类型的内容" }) },
];

const noop = () => {};

/**
 * 条目卡的图标按钮在紧凑模式下靠 hover 显示
 * （`compact-mode.css` 的 `.history-item.compact:hover .item-actions`）。
 * 量测台没有鼠标，因此必须先把「悬浮」这个前置状态真实地做出来，否则紧凑模式
 * 会量到一条**按钮不可见的**行 —— 那会把「按钮矩阵」的量测变成恒真的空断言。
 *
 * 这段样式只作用于本量测台的外壳类 `.collapse-harness`，不进入产品样式，
 * 也不会被生产构建打包（本文件只在验证台的入口里被引用）。
 */
const HARNESS_HOVER_STYLE = `
  .collapse-harness .item-actions {
    opacity: 1 !important;
    visibility: visible !important;
    transform: none !important;
  }
`;

const styleEl = document.createElement("style");
styleEl.textContent = HARNESS_HOVER_STYLE;
document.head.appendChild(styleEl);

const Row = ({ spec }: { spec: CaseSpec }) => {
  const ct = spec.entry.content_type;
  // 与 `useClipboardItemRenderer` 完全同一套判据（从真实模块导入，不另抄一份列表）。
  const bodyEditable = isBodyEditable(ct);
  const noteEditable = isNoteEditable(ct);
  const openBody = openEditor === "body" && bodyEditable;
  const openNote = openEditor === "note" && noteEditable;

  return (
    <div data-case={spec.id} data-note={spec.note}>
      <ClipboardItem
        id={`clipboard-item-${spec.entry.id}`}
        item={spec.entry}
        isSelected={false}
        windowPinned={false}
        isSensitiveHidden={false}
        isRevealed={true}
        isEditingTags={tagOpen}
        tagInput={tagQuery}
        tagSuggestions={tagOpen ? HARNESS_TAG_POOL : []}
        onTagPick={tagOpen ? noop : undefined}
        theme={theme}
        language={lang}
        t={t}
        compactMode={compactMode}
        onSelect={noop}
        onCopy={noop}
        onToggleReveal={noop}
        onOpen={noop}
        onTogglePin={noop}
        onDelete={noop}
        onToggleTagEditor={noop}
        onTagInput={noop}
        onTagAdd={noop}
        onTagDelete={noop}
        onEdit={bodyEditable ? noop : undefined}
        isEditingBody={openBody}
        bodyInitialDraft={openBody ? spec.entry.content : undefined}
        onBodyEditSave={bodyEditable ? noop : undefined}
        onBodyEditCancel={noop}
        onEditNote={noteEditable ? noop : undefined}
        isEditingNote={openNote}
        noteInitialDraft={openNote ? spec.entry.note ?? "" : undefined}
        onNoteEditSave={noteEditable ? noop : undefined}
        onNoteEditCancel={noop}
      />
    </div>
  );
};

const cases = onlyCase ? CASES.filter((c) => c.id === onlyCase) : CASES;

ReactDOM.createRoot(document.getElementById("root")!).render(
  <div
    className="app-container collapse-harness"
    style={{ width: "min(352px, 100%)", height: "100vh", overflow: "hidden" }}
  >
    <div
      className="history-list"
      style={{ height: "100%", overflowY: "auto", padding: "4px 0" }}
    >
      {cases.map((spec) => (
        <Row key={spec.id} spec={spec} />
      ))}
    </div>
  </div>
);

(window as unknown as { __READY__: boolean }).__READY__ = true;
