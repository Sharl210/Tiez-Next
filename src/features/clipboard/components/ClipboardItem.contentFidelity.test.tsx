// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import ClipboardItem from "./ClipboardItem";
import { isRichBodyEditable } from "../types";
import HtmlContent, { sanitizeHTML } from "../../../shared/components/HtmlContent";
import type { ClipboardEntry } from "../../../shared/types";

/**
 * 条目正文的**保真**：列表显示的就是完整原文，不是被剪短的版本。
 *
 * # 这张测试守的是什么
 *
 * 用户反复反馈「正文被截断了」。历史上截断发生在四个位置，其中三个会让用户
 * 真的看到残缺内容：
 *
 * | 位置 | 表现 |
 * |---|---|
 * | 后端列表读取时把 `content` 切到 2000 字 | 尾部变成 `... [Truncated for speed]` |
 * | 后端列表读取时把 `html_content` 切到 5000 字 | 尾部变成 `... [HTML Truncated]` |
 * | 前端列表把表格第 4 行以后删掉 | 插入一行 `... content truncated for preview ...` |
 * | 列表显示用 `preview`（500 字摘要）而不是 `content` | 长正文只剩开头一小段 |
 *
 * 前三处已从源码移除（后端有 `body_fidelity_tests` 守着数据层），本文件守**显示层**：
 * 长正文必须原样渲染出来，且不能出现任何截断标记。
 *
 * # 为什么必须真实挂载
 *
 * 显示字段是三元判断（`item.content || item.preview`）加 CSS 换行的组合结果，
 * 只读源码看不出最终落在 DOM 里的到底是哪一份文本。
 */

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: invokeMock,
  convertFileSrc: (p: string) => p,
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
  emit: vi.fn(async () => undefined),
}));

const T = (key: string) => key;

const entry = (over: Partial<ClipboardEntry> = {}): ClipboardEntry => ({
  id: 7,
  content_type: "text",
  content: "",
  html_content: undefined,
  source_app: "test",
  timestamp: 1758600000000,
  preview: "",
  is_pinned: false,
  tags: [],
  use_count: 0,
  ...over,
});

let container: HTMLDivElement;
let root: Root;

const flush = async () => {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
};

const installMatchMedia = () => {
  if (typeof window.matchMedia === "function") return;
  window.matchMedia = ((query: string) => ({
    matches: false,
    media: query,
    onchange: null,
    addEventListener: () => {},
    removeEventListener: () => {},
    addListener: () => {},
    removeListener: () => {},
    dispatchEvent: () => false,
  })) as unknown as typeof window.matchMedia;
};

/** 列表（非编辑态）渲染。 */
const mountList = async (item: ClipboardEntry) => {
  await act(async () => {
    root.render(
      createElement(ClipboardItem, {
        item,
        isSelected: false,
        isSensitiveHidden: false,
        isRevealed: true,
        isEditingTags: false,
        tagInput: "",
        theme: "dark",
        language: "zh",
        t: T,
        onSelect: () => {},
        onCopy: () => {},
        onToggleReveal: () => {},
        onOpen: () => {},
        onTogglePin: () => {},
        onDelete: () => {},
        onToggleTagEditor: () => {},
        onTagInput: () => {},
        onTagAdd: () => {},
        onTagDelete: () => {},
        onEdit: () => {},
        isEditingBody: false,
        bodyEditIsRich: isRichBodyEditable(item.content_type),
        onBodyEditSave: () => {},
        onBodyEditCancel: () => {},
      } as never)
    );
  });
  await flush();
};

const click = async (el: HTMLElement) => {
  await act(async () => {
    el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
  });
  await flush();
};

const previewText = (): string =>
  document.querySelector<HTMLElement>(".content-preview")?.textContent ?? "";

/** 三条截断标记，出现任何一条都说明截断逻辑又回来了。 */
const TRUNCATION_MARKERS = [
  "[Truncated for speed]",
  "[HTML Truncated]",
  "content truncated for preview",
];

const expectNoTruncationMarker = (text: string) => {
  for (const marker of TRUNCATION_MARKERS) {
    expect(text).not.toContain(marker);
  }
};

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  installMatchMedia();
  invokeMock.mockReset();
  invokeMock.mockImplementation(async () => undefined);
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(async () => {
  await act(async () => {
    root.unmount();
  });
  container.remove();
});

describe("长纯文本正文原样显示", () => {
  /** 正文 2000 字是后端曾经的切点，这里刻意跨过去。 */
  const LINES = Array.from({ length: 400 }, (_, i) => `CUSTOMER_LABEL_${i} = MOLY.NR17.R2`);
  const LONG_TEXT = LINES.join("\n");
  /** 收尾那一行的独特内容：只有拿到完整正文才可能出现。 */
  const TAIL = LINES[LINES.length - 1];

  it("正文超过 2000 字时，尾部仍然渲染出来", async () => {
    expect(LONG_TEXT.length).toBeGreaterThan(2000);
    await mountList(entry({ content: LONG_TEXT, preview: LONG_TEXT.slice(0, 500) }));

    const shown = previewText();
    expect(shown).toContain(TAIL);
    expectNoTruncationMarker(shown);
  });

  it("显示的是完整 content，而不是 500 字摘要 preview", async () => {
    // preview 只有开头，且刻意不含尾行；若界面回退到 preview，下面必然失败。
    await mountList(entry({ content: LONG_TEXT, preview: "SHORT_SUMMARY_ONLY" }));

    const shown = previewText();
    expect(shown).toContain(TAIL);
    expect(shown).not.toContain("SHORT_SUMMARY_ONLY");
  });

  it("换行保留（纯文本不被压成一行）", async () => {
    await mountList(entry({ content: "第一行\n第二行\n第三行" }));

    expect(previewText()).toContain("第一行");
    expect(previewText()).toContain("第三行");
  });
});

describe("富文本 HTML 不再为预览删内容", () => {
  /**
   * 列表里的富文本走的是「按高度生成的快照图」（`richTextSnapshot`，靠 maxHeight 约束
   * 画面高度），那是**呈现**上的边界，不是把字符串剪掉。真正会**删掉节点**的地方是
   * `sanitizeHTML`：它曾经在 preview 模式下把表格第 4 行以后整段移除，再插入一行
   * `... content truncated for preview ...`。那会让正文真的少掉内容（尤其是打开/
   * 紧凑预览这两个走 HTML 渲染的入口），所以这里直接盯住那个函数。
   */
  const ROW_COUNT = 9;
  const tableHtml = `<table>${Array.from(
    { length: ROW_COUNT },
    (_, i) => `<tr><td>R${i}</td><td>V${i}</td></tr>`
  ).join("")}</table>`;

  it("preview 模式下表格 9 行全部保留，且没有截断提示行", () => {
    const { html } = sanitizeHTML(tableHtml, true);
    const host = document.createElement("div");
    host.innerHTML = html;

    const rows = host.querySelectorAll("tr");
    expect(rows.length).toBe(ROW_COUNT);
    expect(host.textContent).toContain(`R${ROW_COUNT - 1}`);
    expect(host.textContent).not.toContain("content truncated for preview");
  });

  it("非 preview 模式同样保留全部行（两种模式不再有差异）", () => {
    const { html } = sanitizeHTML(tableHtml, false);
    const host = document.createElement("div");
    host.innerHTML = html;

    expect(host.querySelectorAll("tr").length).toBe(ROW_COUNT);
  });

  it("HtmlContent 渲染长表格时 9 行都在 DOM 里", async () => {
    await act(async () => {
      root.render(
        createElement(HtmlContent, {
          htmlContent: tableHtml,
          className: "rich-text-preview",
          // preview=true 让内容同步渲染，不依赖 jsdom 里不存在的 IntersectionObserver。
          preview: true,
        })
      );
    });
    await flush();

    const host = document.querySelector<HTMLElement>(".rich-text-preview");
    expect(host).not.toBeNull();
    // 先证明表格渲染出来了，否则行数断言会恒真。
    expect(host!.querySelectorAll("table").length).toBe(1);
    expect(host!.querySelectorAll("tr").length).toBe(ROW_COUNT);
    expect(host!.textContent).not.toContain("content truncated for preview");
  });
});

describe("编辑器种子与回写：长正文不会被改短", () => {
  /**
   * 这条守的是**最危险的那条链**：列表 payload → 编辑器初值 → 保存回写。
   *
   * 历史上列表读取时把正文切到 2000 字，编辑器又用它当初值，于是用户对一条长条目
   * 点一次「编辑 → 保存」（哪怕一个字都没改），库里就永久只剩截断版。数据丢失是
   * 静默且不可逆的，所以这里必须证明"进去多少、出来还是多少"。
   */
  const LONG = Array.from({ length: 400 }, (_, i) => `CUSTOMER_LABEL_${i} = MOLY.NR17.R2`).join("\n");

  it("纯文本：初始草稿是完整正文，保存回写的长度与原文一致", async () => {
    expect(LONG.length).toBeGreaterThan(2000);

    const onBodyEditSave = vi.fn();
    await act(async () => {
      root.render(
        createElement(ClipboardItem, {
          item: entry({ content: LONG, preview: LONG.slice(0, 500) }),
          isSelected: false,
          isSensitiveHidden: false,
          isRevealed: true,
          isEditingTags: false,
          tagInput: "",
          theme: "dark",
          language: "zh",
          t: T,
          onSelect: () => {},
          onCopy: () => {},
          onToggleReveal: () => {},
          onOpen: () => {},
          onTogglePin: () => {},
          onDelete: () => {},
          onToggleTagEditor: () => {},
          onTagInput: () => {},
          onTagAdd: () => {},
          onTagDelete: () => {},
          onEdit: () => {},
          // 编辑态开着，且初值就是列表下发的那份正文。
          isEditingBody: true,
          bodyInitialDraft: LONG,
          bodyEditIsRich: false,
          onBodyEditSave,
          onBodyEditCancel: () => {},
        } as never)
      );
    });
    await flush();

    const area = document.querySelector<HTMLTextAreaElement>(".entry-body-editor-dialog textarea");
    expect(area).not.toBeNull();
    // 先证明种子真的进去了，否则"长度相等"可能只是两个空值相等。
    expect(area!.value.length).toBe(LONG.length);

    // 不做任何修改，直接保存 —— 这正是用户"点开又关掉/保存"的路径。
    const buttons = Array.from(
      document.querySelectorAll<HTMLElement>(".entry-body-editor-dialog .confirm-dialog-button")
    );
    const primary = buttons.find((b) => b.classList.contains("primary"));
    expect(primary).toBeDefined();
    await click(primary!);

    expect(onBodyEditSave).toHaveBeenCalledTimes(1);
    // 组件的这枚回调签名是 `(newContent, htmlContent)`——条目 id 由调用方闭包绑定
    // （见 `useClipboardItemRenderer` 的 `saveBodyEdit(item.id, ...)`），所以这里只看正文。
    const [savedContent] = onBodyEditSave.mock.calls[0] as [string, string?];
    expect(savedContent.length).toBe(LONG.length);
    expect(savedContent).toBe(LONG);
  });
});
