/**
 * vscroll 量测台 —— **复刻真机的滚动容器链**，用来量「浮层里的滚轮会不会带动外层」。
 *
 * # 为什么必须再开一个台（collapse.html 量不到这件事）
 *
 * `collapse.html` 的外层是手写的 `<div className="history-list" style={{overflowY:'auto'}}>`，
 * 而真机的外层滚动发生在 **react-virtuoso 自己生成的 `div[data-virtuoso-scroller]`** 上，
 * 两者嵌套的祖先链不同（真机中间还夹着 `.virtual-list-wrapper` / `.history-list-container`
 * / `.main-content`）。在 collapse 台里量「外层有没有被带动」等于在量一个**别的元素**。
 *
 * 本台用**真实**的 `VirtualClipboardList`（即真实的 `Virtuoso`）与**真实**的
 * `ClipboardItem`，外层按 `App.tsx` / `AppMainContent.tsx` 的真实类名与内联样式搭出来，
 * 因此滚动容器就是真机那一个。
 *
 * # 与真机的边界（必须如实说明）
 *
 * - `invoke`/`listen` 是 mock、条目数据是造的 ⇒ 只证明**布局与事件链**，不证明数据行为；
 * - 真机是 Windows 上的 WebView2（Chromium 内核），这里是 Linux 上的 Chrome 150。
 *   滚轮/滚动链属 Chromium 通用行为，但仍属**跨宿主推断**；
 * - **真机（用户的 Windows 机器）上的实际表现没有观测过**，本台只能给机制证据。
 *
 * # 参数
 *
 * - `?n=<条数>`        外层条目数量（决定外层是否可滚）
 * - `?pool=<条数>`     候补标签池大小（`2` = 用户说的"只有 2–3 条"；`40` = 溢出场景）
 * - `?fi=<下标>`       第几个条目进入标签编辑态
 * - `?compact=1`       紧凑模式
 * - `?fix=1`           载入候选修复（wheel 监听器）后再量
 */

import React from "react";
import ReactDOM from "react-dom/client";

// ---- 真实样式，与 src/main.tsx 的加载顺序一致 ----
import "../../../src/index.css";
import "../../../src/styles/components/index.css";
import "../../../src/styles/themes/load";

// ---- 真实组件（一字未改）----
import { VirtualClipboardList } from "../../../src/features/clipboard/components/VirtualClipboardList";
import ClipboardItem from "../../../src/features/clipboard/components/ClipboardItem";
import { translations } from "../../../src/locales";
import type { ClipboardEntry } from "../../../src/shared/types";

const params = new URLSearchParams(location.search);
const lang = (params.get("lang") ?? "zh") as "zh" | "en" | "tw";
const theme = params.get("theme") ?? "retro";
const colorMode = params.get("colorMode") ?? "light";
const compactMode = params.get("compact") === "1";
const count = parseInt(params.get("n") ?? "14", 10);
const poolSize = parseInt(params.get("pool") ?? "40", 10);
const editIndex = parseInt(params.get("fi") ?? "1", 10);

document.documentElement.classList.add(`theme-${theme}`, `${colorMode}-mode`);
document.body.classList.add(`theme-${theme}`, `${colorMode}-mode`);

/** 与 `App.tsx` 的 `t` 同一约定：查不到就返回键名本身。 */
const t = (key: string): string => {
  const dict = translations[lang] as unknown as Record<string, string>;
  return dict[key] ?? (translations.zh as unknown as Record<string, string>)[key] ?? key;
};

const noop = () => {};

/**
 * 候补标签池。
 *
 * 2 条的那一档就是用户描述的"只有 2–3 条"：此时浮层 `scrollHeight == clientHeight`，
 * 元素**没有可滚动内容**，自己也就不吃滚轮。
 */
const POOL: string[] = [
  "ims", "img", "invoice", "ims配置", "ims下发", "image",
  ...Array.from({ length: 20 }, (_, i) => `item-${i}`),
  ...Array.from({ length: 34 }, (_, i) => `tag-${i}`),
].slice(0, poolSize);

const ENTRIES: ClipboardEntry[] = Array.from({ length: count }, (_, i) => ({
  id: 1000 + i,
  content_type: "text",
  content: `第 ${i} 条内容，长度足够撑出一行。`,
  source_app: "harness",
  timestamp: Date.now(),
  preview: `第 ${i} 条内容，长度足够撑出一行。`,
  is_pinned: false,
  tags: i === editIndex ? ["ims"] : [],
}));

const renderItem = (item: ClipboardEntry) => {
  const editing = item.id === 1000 + editIndex;
  return (
    <ClipboardItem
      id={`clipboard-item-${item.id}`}
      item={item}
      isSelected={false}
      windowPinned={false}
      isSensitiveHidden={false}
      isRevealed={true}
      isEditingTags={editing}
      tagInput="i"
      tagSuggestions={editing ? POOL : []}
      onTagPick={editing ? noop : undefined}
      onTagEditCancel={noop}
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
    />
  );
};

ReactDOM.createRoot(document.getElementById("root")!).render(
  // 与 App.tsx:1128 的 `.app-container`、App.tsx:1162 的 `<main className="main-content">`
  // 逐字对应（含 `overflowY: 'hidden'` 这条内联样式）。
  <div className="app-container">
    <main className="main-content" style={{ overflowY: "hidden" }}>
      {/* AppMainContent.tsx:291 的真实容器类名（它自己没有 overflow，见 layout.css:181） */}
      <div className="history-list-container">
        <VirtualClipboardList
          items={ENTRIES}
          renderItem={renderItem}
          hasMore={false}
          isLoading={false}
          selectedIndex={-1}
          isKeyboardMode={false}
          compactMode={compactMode}
        />
      </div>
    </main>
  </div>
);

(window as unknown as { __READY__: boolean }).__READY__ = true;
