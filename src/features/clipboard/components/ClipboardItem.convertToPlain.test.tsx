// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import ClipboardItem from "./ClipboardItem";
import { isRichBodyEditable } from "../types";
import type { ClipboardEntry } from "../../../shared/types";

/**
 * 「转换为纯文本」按钮送出的必须是**链接地址**，不是界面上的标签文字。
 *
 * # 为什么单独为这个按钮写测试
 *
 * 这个按钮曾经读编辑器的 `innerText` —— 那是**屏幕上显示的文字**，链接只会给出
 * 标签（"T750 变更单"），地址（`href`）根本不在里面。于是"转成纯文本"之后用户
 * 拿到的还是标题，正是反馈里的问题。改成从 `innerHTML` 派生之后，链接才会变成地址。
 *
 * 这个按钮有三处入口（主页面弹窗、标签管理卡片、标签管理弹窗），曾出现"只修了
 * 其中一处"的情况，所以这里直接盯住**真实挂载后点按钮**的结果。
 *
 * # 断言的是什么
 *
 * `onBodyEditSave(newContent, htmlContent)` 的第一个实参 —— 它就是最终写进库里
 * `content` 列、也是"纯文本粘贴"会用的那份文本。
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

/** 用户报的那类条目：屏幕上显示「T750 变更单」，地址在 href 里。 */
const URL = "http://192.168.23.98:8888/c/T750/+/176116";
const LABEL = "T750 变更单";
const PLAIN = "T750 变更单";
const HTML = `<p><a href="${URL}">${LABEL}</a></p>`;

const entry = (over: Partial<ClipboardEntry> = {}): ClipboardEntry => ({
  id: 21,
  content_type: "rich_text",
  content: PLAIN,
  html_content: HTML,
  source_app: "test",
  timestamp: 1758600000000,
  preview: PLAIN,
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

/** 打开富文本正文编辑器（按钮就在这个弹窗里）。 */
const mountEditor = async (item: ClipboardEntry) => {
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
      } as never)
    );
  });
  await flush();
  return { onBodyEditSave };
};

const click = async (el: HTMLElement) => {
  await act(async () => {
    el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
  });
  await flush();
};

const convertButton = (): HTMLElement => {
  const el = document.querySelector<HTMLElement>(".entry-body-editor-dialog .rich-to-plain-button");
  if (!el) throw new Error("未找到「转为纯文本」按钮");
  return el;
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

describe("弹窗里的「转换为纯文本」", () => {
  it("编辑器里是一条链接时，送出的是网址本身而不是标签文字", async () => {
    const { onBodyEditSave } = await mountEditor(entry());

    await click(convertButton());

    expect(onBodyEditSave).toHaveBeenCalledTimes(1);
    const [newContent, htmlContent] = onBodyEditSave.mock.calls[0] as [string, string];
    expect(newContent).toBe(URL);
    expect(newContent).not.toBe(LABEL);
    expect(htmlContent).toBe(""); // 清空 HTML = 后端据此降级为纯文本
  });

  it("链接周围还有文字时，网址与文字都在", async () => {
    const html = `<p>详见 <a href="${URL}">${LABEL}</a> 里的说明</p>`;
    const { onBodyEditSave } = await mountEditor(
      entry({ html_content: html, content: "详见 T750 变更单 里的说明" })
    );

    await click(convertButton());

    const [newContent] = onBodyEditSave.mock.calls[0] as [string, string];
    expect(newContent).toContain(URL);
    expect(newContent).toContain("详见");
    expect(newContent).toContain("里的说明");
  });

  it("没有链接的富文本条目照常转换（不受影响）", async () => {
    const html = "<p>普通<b>富文本</b>内容</p>";
    const { onBodyEditSave } = await mountEditor(
      entry({ html_content: html, content: "普通富文本内容" })
    );

    await click(convertButton());

    const [newContent] = onBodyEditSave.mock.calls[0] as [string, string];
    expect(newContent).toBe("普通富文本内容");
  });
});
