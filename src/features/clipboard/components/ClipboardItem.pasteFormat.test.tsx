// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import ClipboardItem from "./ClipboardItem";
import { isRichBodyEditable } from "../types";
import type { ClipboardEntry } from "../../../shared/types";

/**
 * 粘贴格式的**默认值**：默认富文本，纯文本是那条"例外通道"。
 *
 * # 契约
 *
 * | 手势 | 格式 |
 * |---|---|
 * | 左键单击条目 | 富文本 |
 * | 右键单击条目 | 纯文本 |
 *
 * # 为什么反向很容易被改回去
 *
 * 这个默认值由界面上的两个布尔字面量决定，写反了不会有任何编译错误、也没有别的
 * 测试会红 —— 只会让用户觉得"复制过去格式没了"。所以这里把两个手势都钉死：
 * 断言的是**实际传出去的 `withFormat`**，不是源码里写了什么。
 *
 * # 为什么"默认富文本"能满足 Ctrl+V / Ctrl+Shift+V
 *
 * 带格式写剪贴板会同时放上 CF_HTML 和纯文本两份。于是目标应用自己的 Ctrl+V 取到
 * 带格式那份、Ctrl+Shift+V 取到纯文本那份 —— 这两条不是本应用去模拟的，而是
 * 目标应用的原生行为。所以"默认富文本"正是"Ctrl+V 富文本、Ctrl+Shift+V 纯文本"
 * 的前提，而不是它的对立面。
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

const RICH_HTML = "<p>加粗的<b>字</b></p>";

const entry = (over: Partial<ClipboardEntry> = {}): ClipboardEntry => ({
  id: 11,
  content_type: "rich_text",
  content: "加粗的字",
  html_content: RICH_HTML,
  source_app: "test",
  timestamp: 1758600000000,
  preview: "加粗的字",
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

/** 挂载一个条目，返回记录 `onCopy` 实参的 spy。 */
const mountItem = async (item: ClipboardEntry) => {
  const onCopy = vi.fn();
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
        onCopy,
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
  return onCopy;
};

/** 列表条目的根节点（复制手势挂在它身上）。 */
const rootRow = (): HTMLElement => {
  const el = container.querySelector<HTMLElement>('[data-test-clipboard-item="true"]');
  if (!el) throw new Error("未找到条目根节点");
  return el;
};

const dispatch = async (el: HTMLElement, type: string) => {
  await act(async () => {
    el.dispatchEvent(new MouseEvent(type, { bubbles: true, cancelable: true, button: 0 }));
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
});

describe("单击 = 带格式", () => {
  it("富文本条目左键按下，传出的 withFormat 是 true", async () => {
    const onCopy = await mountItem(entry());

    await dispatch(rootRow(), "mousedown");

    expect(onCopy).toHaveBeenCalledTimes(1);
    expect(onCopy).toHaveBeenCalledWith(true);
  });

  it("纯文本条目左键按下同样传 true（该参数只表示允许带格式，无 HTML 时自然退化为纯文本）", async () => {
    const onCopy = await mountItem(
      entry({ content_type: "text", html_content: undefined, content: "普通文字" })
    );

    await dispatch(rootRow(), "mousedown");

    expect(onCopy).toHaveBeenCalledWith(true);
  });
});

describe("右键 = 纯文本", () => {
  it("富文本条目右键，传出的 withFormat 是 false", async () => {
    const onCopy = await mountItem(entry());

    await dispatch(rootRow(), "contextmenu");

    expect(onCopy).toHaveBeenCalledTimes(1);
    expect(onCopy).toHaveBeenCalledWith(false);
  });

  it("一次点击只触发一次复制（不会既按左键又按右键语义各来一次）", async () => {
    const onCopy = await mountItem(entry());

    await dispatch(rootRow(), "contextmenu");

    expect(onCopy).toHaveBeenCalledTimes(1);
  });
});
