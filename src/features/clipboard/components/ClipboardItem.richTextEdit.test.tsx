// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import ClipboardItem from "./ClipboardItem";
import { isRichBodyEditable } from "../types";
import type { ClipboardEntry } from "../../../shared/types";

/**
 * R13：**编辑富文本不再坍缩成纯文本**。
 *
 * # 这条测试守的是什么
 *
 * 用户的原话是「编辑富文本不会坍缩成纯文本」。缺陷由**四处**彼此独立的降级组成，
 * 只修一处会得到"保存后看着没降级，切窗口才发现降级了"；本文件只覆盖**界面侧**
 * 那几处，后端写入路径由 `src-tauri/src/infrastructure/repository/tag_repo.rs`
 * 的 `r13_*` 用例守着。
 *
 * 界面侧的四种坏法，各自对应下面一条断言：
 *
 * | 坏法 | 现象 | 本文件的哪条 |
 * |---|---|---|
 * | 弹窗初值只填 `item.content`（纯文本列） | 一打开格式就没了 | 「初值是 HTML」 |
 * | 保存只送纯文本 | 后端按纯文本落库 | 「保存送出 HTML」 |
 * | 编辑器是 `<textarea>` | 读不出 HTML | 「富文本用 contentEditable」 |
 * | `rich_text` 与 `text` 走同一个编辑器 | 格式无处承载 | 「非富文本仍是 textarea」 |
 *
 * # 为什么必须真实挂载
 *
 * 编辑器在 portal 里，而且 contentEditable 的初值是**命令式**写入的（非受控）。
 * 静态渲染或只读源码都看不出"初值写进去了没有"——必须真的挂载、真的读 DOM。
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

/** 富文本条目：正文是纯文本，格式在 `html_content` 里。 */
const RICH_CONTENT = "加粗的字和普通的字";
const RICH_HTML = "<p>加粗的<b>字</b>和普通的字</p>";

const entry = (over: Partial<ClipboardEntry> = {}): ClipboardEntry => ({
  id: 42,
  content_type: "rich_text",
  content: RICH_CONTENT,
  html_content: RICH_HTML,
  source_app: "test",
  timestamp: 1758600000000,
  preview: RICH_CONTENT,
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

const mount = async (item: ClipboardEntry, props: Record<string, unknown> = {}) => {
  const onBodyEditSave = vi.fn();
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
        isEditingBody: true,
        bodyInitialDraft: item.content,
        bodyInitialHtml: item.html_content ?? "",
        bodyEditIsRich: isRichBodyEditable(item.content_type),
        onBodyEditSave,
        onBodyEditCancel: () => {},
        ...props,
      } as never)
    );
  });
  await flush();
  return { onBodyEditSave };
};

const richEditor = () =>
  document.querySelector<HTMLElement>('[data-testid="entry-body-editor-rich"]');
const textarea = () =>
  document.querySelector<HTMLTextAreaElement>(".entry-body-editor-dialog textarea");
const saveButton = () => {
  const buttons = Array.from(
    document.querySelectorAll<HTMLElement>(".entry-body-editor-dialog .confirm-dialog-button")
  );
  const primary = buttons.find((b) => b.classList.contains("primary"));
  if (!primary) throw new Error("弹窗里找不到保存按钮");
  return primary;
};

const click = async (el: HTMLElement) => {
  await act(async () => {
    el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
  });
  await flush();
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
  // portal 挂在 <body> 上，unmount 后才会被清掉
  document.querySelectorAll(".modal-overlay").forEach((n) => n.remove());
});

describe("R13｜富文本编辑器：初值是 HTML 而不是纯文本", () => {
  it("编辑器里能读到原有格式（<b> 还在）", async () => {
    await mount(entry());
    const node = richEditor();
    expect(node).not.toBeNull();
    // 初值必须来自 html_content。若退回"只填 item.content"，这里会得到纯文本，
    // 下面这条断言直接变红 —— 那正是用户看到的"一打开格式就没了"。
    expect(node!.innerHTML).toContain("<b>");
    expect(node!.textContent).toBe(RICH_CONTENT);
  });

  it("非富文本条目走的仍是 textarea（否则代码/链接的换行与缩进会被 HTML 语义改写）", async () => {
    await mount(
      entry({ content_type: "code", content: "fn main() {}", html_content: undefined }),
      { bodyInitialHtml: undefined, bodyEditIsRich: false }
    );
    expect(richEditor()).toBeNull();
    const ta = textarea();
    expect(ta).not.toBeNull();
    expect(ta!.value).toBe("fn main() {}");
  });

  it("isRichBodyEditable 只对 rich_text 为真（判据单点）", () => {
    expect(isRichBodyEditable("rich_text")).toBe(true);
    for (const t of ["text", "code", "url", "image", "file", "video", "emoji_sync", ""]) {
      expect(isRichBodyEditable(t)).toBe(false);
    }
  });
});

describe("R13｜保存：把编辑后的 HTML 一起送出去", () => {
  const typeRich = async (html: string) => {
    const node = richEditor()!;
    await act(async () => {
      node.innerHTML = html;
      node.dispatchEvent(new Event("input", { bubbles: true }));
    });
    await flush();
  };

  it("保存回调带上 innerHTML（只送纯文本就等于把格式丢掉）", async () => {
    const { onBodyEditSave } = await mount(entry());
    await typeRich("<p>改过的<b>加粗</b></p>");
    await click(saveButton());

    expect(onBodyEditSave).toHaveBeenCalledTimes(1);
    const [content, html] = onBodyEditSave.mock.calls[0];
    // 第二个参数必须有内容 —— 若界面只送纯文本，这里会是 undefined 而变红。
    expect(typeof html).toBe("string");
    expect(html).toContain("<b>");
    expect(html).toContain("加粗");
    // 第一个参数必须是**纯文本**："改过的加粗"，而不是带标签的源码。
    expect(content).toBe("改过的加粗");
    expect(content).not.toContain("<");
  });

  it("只改格式、正文一字未变时，第二个参数仍然带着新的 HTML", async () => {
    const { onBodyEditSave } = await mount(entry());
    // 同一段文字，只把它整段包进 <i>：正文文本相同，格式不同。
    await typeRich(`<p><i>${RICH_CONTENT}</i></p>`);
    await click(saveButton());

    const [content, html] = onBodyEditSave.mock.calls[0];
    // 正文是**派生的纯文本**（粘贴与列表预览用的就是它），不是 innerHTML ——
    // 若把 innerHTML 当正文送去，用户复制出来会看到 `<p><i>…` 源码。
    expect(content).toBe(RICH_CONTENT);
    expect(html).toContain("<i>");
  });

  it("Ctrl+Enter 走的也是同一条带 HTML 的保存路径", async () => {
    const { onBodyEditSave } = await mount(entry());
    const node = richEditor()!;
    await act(async () => {
      node.innerHTML = "<p>快捷键保存<b>x</b></p>";
      node.dispatchEvent(new Event("input", { bubbles: true }));
      node.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", ctrlKey: true, bubbles: true }));
    });
    await flush();

    expect(onBodyEditSave).toHaveBeenCalledTimes(1);
    expect(onBodyEditSave.mock.calls[0][1]).toContain("<b>");
  });

  it("非富文本条目的保存只送一个参数（不制造无意义的 HTML）", async () => {
    const { onBodyEditSave } = await mount(
      entry({ content_type: "text", content: "普通文本", html_content: undefined }),
      { bodyInitialHtml: undefined, bodyEditIsRich: false }
    );
    await click(saveButton());

    expect(onBodyEditSave).toHaveBeenCalledTimes(1);
    const [content, html] = onBodyEditSave.mock.calls[0];
    expect(content).toBe("普通文本");
    expect(html).toBeUndefined();
  });
});

describe("R13｜界面不再声称「保存会降级」（那句话现在是错的）", () => {
  const dir = path.dirname(fileURLToPath(import.meta.url));
  const read = (rel: string) => fs.readFileSync(path.join(dir, rel), "utf8");

  it("ClipboardItem 不再引用降级警告", () => {
    const src = read("ClipboardItem.tsx");
    expect(src).not.toContain("entry-body-editor-warning");
    expect(src).not.toContain("格式会转为纯文本");
    expect(src).not.toContain("bodyEditDowngradesFormat");
  });

  it("TagManager 不再引用降级警告，且它的富文本入口也读 html_content", () => {
    const src = read("../../tag/components/TagManager.tsx");
    expect(src).not.toContain("edit_item_rich_text_warning");
    expect(src).not.toContain("AlertTriangle");
    // 第二个入口必须同样带上 HTML，否则从标签管理页编辑仍会丢格式。
    expect(src).toContain("htmlContent");
    expect(src).toContain("tag-manager-rich-editor");
  });

  it("三语词条里已经没有那条警告（否则是「说降级、其实不降级」的误导）", () => {
    const locales = read("../../../locales.ts");
    expect(locales).not.toContain("edit_item_rich_text_warning");
  });

  it("types.ts 不再导出降级判据，改为导出富文本判据", () => {
    const types = read("../types.ts");
    expect(types).not.toMatch(/export const bodyEditDowngradesFormat/);
    expect(types).toMatch(/export const isRichBodyEditable/);
  });
});
