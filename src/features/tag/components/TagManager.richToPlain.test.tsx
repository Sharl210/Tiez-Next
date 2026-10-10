// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import TagManager from "./TagManager";
import type { ClipboardEntry } from "../../../shared/types";

/**
 * 标签管理页的「富文本 → 纯文本」入口。
 *
 * # 这张测试要守住什么
 *
 * 1. **入口存在且只对富文本出现** —— 用户在标签管理页也要能把富文本降级成纯文本，
 *    但纯文本/图片/文件/视频条目不该多出一个永远无意义的按钮。
 * 2. **与主页面的后端语义完全一致** —— 转换必须走 `update_item_content` 且带
 *    `htmlContent: ""`。仓储层把 `Some("")` 定义为"显式清空 HTML 并降级为 text"
 *    （见 `clipboard_repo::update_entry_content_with_conn`），这是主页面上那个
 *    「转换为纯文本」按钮用的同一条路径。若这里改用别的写法（例如只写正文而不带
 *    HTML），富文本条目会保留旧 HTML，用户看到"点了没反应"。
 * 3. **进入页面默认聚焦标签组搜索框** —— 用户要求进页面就能直接打字搜索标签组，
 *    不必先点一下输入框。
 *
 * # 为什么必须真实挂载
 *
 * 卡片列表来自 `invoke('get_tag_items')` 的异步结果；静态渲染时列表为空，
 * 在空列表上断言"没有按钮"会恒真。
 */

const { invokeMock } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: invokeMock,
  convertFileSrc: (p: string) => p,
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
  emit: vi.fn(async () => undefined),
}));

const T = (key: string) => key;

const TAG = "工作";
/** 富文本条目的纯文本正文与 HTML 是两个不同的东西，正好用来分辨写入了哪一个。 */
// 两个相邻 <p> 之间是**空行**：后端把块标签的开闭都换成换行，前端已与之对齐。
// 这个常量原先写单个换行，那会让"点按钮转换"与"直接纯文本粘贴"的行结构不一致。
const RICH_PLAIN = "第一行\n\n第二行";
const RICH_HTML = "<p>第一行</p><p>第二行</p>";

const entry = (
  id: number,
  content_type: string,
  content: string,
  html_content?: string
): ClipboardEntry => ({
  id,
  content_type,
  content,
  html_content,
  source_app: "test",
  timestamp: 1758600000000 + id,
  preview: content,
  is_pinned: false,
  tags: [TAG],
  use_count: 0,
  note: "",
});

const ENTRIES: ClipboardEntry[] = [
  entry(4, "rich_text", RICH_PLAIN, RICH_HTML),
  entry(1, "text", "纯文本条目"),
  entry(2, "code", "const a = 1;"),
  entry(5, "image", "C:/tmp/a.png"),
];

/** 组件按 `timestamp` 倒序展示，所以 DOM 顺序是 id 从大到小。 */
const ORDERED_IDS = [5, 4, 2, 1];

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

const mount = async () => {
  await act(async () => {
    root.render(createElement(TagManager, { t: T, theme: "light" }));
  });
  await flush();
};

const cardNode = (id: number): HTMLElement => {
  const cards = Array.from(document.querySelectorAll<HTMLElement>(".items-grid .themed-card"));
  const index = ORDERED_IDS.indexOf(id);
  const node = cards[index];
  if (!node) throw new Error(`找不到 id=${id} 的卡片（第 ${index} 张，共 ${cards.length} 张）`);
  return node;
};

const buttonIn = (id: number, testId: string): HTMLElement | null =>
  cardNode(id).querySelector<HTMLElement>(`[data-testid="${testId}"]`);

const click = async (el: HTMLElement) => {
  await act(async () => {
    el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
  });
  await flush();
};

const callsTo = (cmd: string) => invokeMock.mock.calls.filter((c) => c[0] === cmd);

const tagSearchInput = (): HTMLInputElement | null =>
  document.querySelector<HTMLInputElement>(".tag-search-box input");

beforeEach(async () => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  installMatchMedia();
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string) => {
    switch (cmd) {
      case "get_all_tags_info":
        return { [TAG]: ENTRIES.length };
      case "get_tag_colors":
        return {};
      case "get_settings":
        return {};
      case "get_tag_items":
        return ENTRIES;
      default:
        return undefined;
    }
  });
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  await mount();
});

afterEach(async () => {
  await act(async () => {
    root.unmount();
  });
  container.remove();
});

describe("入口可见性：只有富文本条目才有「转为纯文本」", () => {
  it("四张卡片都渲染出来了（否则下面的断言会恒真）", () => {
    expect(document.querySelectorAll(".items-grid .themed-card").length).toBe(ENTRIES.length);
    for (const id of ORDERED_IDS) {
      expect(buttonIn(id, "card-open")).not.toBeNull();
    }
  });

  it("rich_text 卡片有该按钮", () => {
    expect(buttonIn(4, "card-to-plain-text")).not.toBeNull();
  });

  for (const id of [1, 2, 5]) {
    it(`id=${id} 的非富文本卡片没有该按钮`, () => {
      expect(buttonIn(id, "card-to-plain-text")).toBeNull();
    });
  }
});

describe("转换语义与主页面一致", () => {
  it("点卡片按钮：写 update_item_content，且 htmlContent 是空串（显式清空并降级）", async () => {
    await click(buttonIn(4, "card-to-plain-text")!);
    const calls = callsTo("update_item_content");
    expect(calls.length).toBe(1);
    const args = calls[0][1] as {
      id: number;
      newContent: string;
      htmlContent: string | undefined;
    };
    expect(args.id).toBe(4);
    expect(args.htmlContent).toBe("");
    // 正文是该条目的纯文本，绝不能把 HTML 源码写进正文列。
    expect(args.newContent).toBe(RICH_PLAIN);
    expect(args.newContent).not.toContain("<p>");
  });

  it("转换后刷新该标签的列表", async () => {
    const before = callsTo("get_tag_items").length;
    await click(buttonIn(4, "card-to-plain-text")!);
    expect(callsTo("get_tag_items").length).toBeGreaterThan(before);
  });
});

describe("进入标签管理页默认聚焦标签组搜索框", () => {
  it("搜索框存在，且挂载后自动获得键盘焦点", async () => {
    const input = tagSearchInput();
    // 先证明定位器找到了真实输入框，否则下面的 focus 断言会恒假。
    expect(input).not.toBeNull();
    expect(input!.placeholder).toBe("find_or_create");

    // 聚焦走的是 `setTimeout(..., 0)`（等 DOM 提交完成），所以要让真实计时器跑一拍。
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 5));
    });

    expect(document.activeElement).toBe(input);
  });
});
