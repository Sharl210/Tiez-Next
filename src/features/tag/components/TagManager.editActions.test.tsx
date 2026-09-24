// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import TagManager, { resolveCardEditActions, resolveEditSavePlan } from "./TagManager";
import { isBodyEditable, isNoteEditable } from "../../clipboard/types";
import type { ClipboardEntry } from "../../../shared/types";

/**
 * R12：标签管理页卡片的「编辑内容 / 编辑备注」是两个**独立按钮**。
 *
 * # 这张测试要守住什么
 *
 * 1. **按钮矩阵** —— 每种内容类型该有哪几个按钮。边界是 `emoji_sync` 与未预见的
 *    类型：它们**没有**「编辑内容」（正文不是文本），但**必须有**「编辑备注」。
 * 2. **判据同源** —— 标签管理页必须用 `features/clipboard/types` 的
 *    `isBodyEditable` / `isNoteEditable`，不能自己再写一套。v0.5.4 就是因为
 *    这里自带一份 `BINARY_CONTENT_TYPES = ['image','file','video']` 白名单，
 *    与主页面的"补集"判据分叉，导致 `emoji_sync` 在主页面有备注入口、
 *    在标签管理页没有。
 * 3. **保存分开** —— 备注弹窗保存时**不写正文**，正文弹窗保存时**不写备注**。
 *
 * # 为什么必须真实挂载
 *
 * 卡片列表来自 `invoke('get_tag_items')` 的异步结果。静态渲染时列表是空的，
 * 在空列表上断言"没有某个按钮"会**恒真** —— 那种通过的测试什么也证明不了。
 * 所以这里 mock 掉宿主 API，让 `get_tag_items` 真的返回条目，再对真实 DOM 下断言。
 *
 * 每条断言前都先证明"卡片确实渲染出来了"，避免空集合恒真。
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

/** 透传式 `t`：断言里比对的就是 locale 键本身，不引入任何新文案。 */
const T = (key: string) => key;

/** 一张卡片要触发两个写入，所以正文与备注都从非空值开始。 */
const BODY = "BODY_ORIGINAL";
const NOTE = "NOTE_ORIGINAL";
const TAG = "工作";

/**
 * R12：期望的按钮矩阵 —— 与需求表逐行对应。
 *
 * 「编辑备注」**恒为 true**：用户原话是「编辑备注内容**每个条目都要有这个按钮**」，
 * 所以这一列没有例外。注意这**不等于** `isNoteEditable` 单独的值 ——
 * 后者是"可编辑正文类型的补集"，对 `text`/`code`/`url`/`rich_text` 返回 `false`
 * （实测），单用它会让这四类条目**丢掉**备注入口。产品的判据是两个同源函数的
 * **并集**（见 `resolveCardEditActions`），本表断的就是那个并集。
 */
const expectBodyButton = (contentType: string) => isBodyEditable(contentType);
const expectNoteButton = (_contentType: string) => true;

const CARD_TYPES = [
  { id: 1, contentType: "text" },
  { id: 2, contentType: "code" },
  { id: 3, contentType: "url" },
  { id: 4, contentType: "rich_text" },
  { id: 5, contentType: "image" },
  { id: 6, contentType: "file" },
  { id: 7, contentType: "video" },
  { id: 8, contentType: "emoji_sync" },
  { id: 9, contentType: "unknown_type" },
] as const;

const cardById = (id: number): ClipboardEntry => ({
  id,
  content_type: CARD_TYPES.find((c) => c.id === id)!.contentType,
  content: BODY,
  source_app: "test",
  timestamp: 1758600000000 + id,
  preview: BODY,
  is_pinned: false,
  tags: [TAG],
  use_count: 0,
  note: NOTE,
});

/** 组件按 `timestamp` 倒序展示，所以 DOM 顺序是 id 从大到小。 */
const ORDERED_IDS: number[] = CARD_TYPES.map((c) => c.id).slice().sort((a, b) => b - a);

let container: HTMLDivElement;
let root: Root;

const flush = async () => {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
};

/** jsdom 缺口：组件用 `matchMedia` 判断窄视口，真实浏览器里它始终存在。 */
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

const dialog = () => document.querySelector<HTMLElement>(".tag-manager-dialog");
const dialogTitle = () => dialog()?.querySelector("h3")?.textContent ?? null;
const textareas = (): HTMLTextAreaElement[] =>
  Array.from(document.querySelectorAll<HTMLTextAreaElement>(".tag-manager-dialog textarea"));

/** 走 React 的受控输入通道（直接改 `.value` 不会触发 onChange）。 */
const typeInto = async (el: HTMLTextAreaElement, value: string) => {
  await act(async () => {
    const setter = Object.getOwnPropertyDescriptor(
      HTMLTextAreaElement.prototype,
      "value"
    )!.set!;
    setter.call(el, value);
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await flush();
};

const closeDialog = async () => {
  const overlay = document.querySelector<HTMLElement>(".modal-overlay");
  if (overlay) await click(overlay);
};

const saveButton = (): HTMLElement => {
  const buttons = Array.from(
    document.querySelectorAll<HTMLElement>(".tag-manager-dialog .confirm-dialog-button")
  );
  const primary = buttons.find((b) => b.classList.contains("primary"));
  if (!primary) throw new Error("弹窗里找不到保存按钮");
  return primary;
};

const callsTo = (cmd: string) => invokeMock.mock.calls.filter((c) => c[0] === cmd);

beforeEach(async () => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  installMatchMedia();
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string) => {
    switch (cmd) {
      case "get_all_tags_info":
        return { [TAG]: CARD_TYPES.length };
      case "get_tag_colors":
        return {};
      case "get_settings":
        return {};
      case "get_tag_items":
        return CARD_TYPES.map((c) => cardById(c.id));
      case "update_item_content":
      case "update_entry_note":
        return undefined;
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

describe("R12｜卡片确实渲染出来了（前置条件）", () => {
  it("九张卡片都在，否则下面所有按钮断言都会恒真", () => {
    expect(document.querySelectorAll(".items-grid .themed-card").length).toBe(CARD_TYPES.length);
    // 定位器本身也要有效：每张卡都必须能被 `cardNode` 找到，且都带「打开」按钮。
    for (const { id } of CARD_TYPES) {
      expect(buttonIn(id, "card-open")).not.toBeNull();
    }
  });
});

describe("R12｜按钮可见性矩阵与主页面判据同源", () => {
  for (const { id, contentType } of CARD_TYPES) {
    const wantBody = expectBodyButton(contentType);
    const wantNote = expectNoteButton(contentType);

    it(`${contentType}：「编辑内容」${wantBody ? "有" : "无"}、「编辑备注」${wantNote ? "有" : "无"}`, () => {
      // 先证明定位器对这张卡有效（每张卡都带「打开」按钮），
      // 否则两个 `toBeNull` / `not.toBeNull` 可能因为卡都没渲染而误判。
      expect(buttonIn(id, "card-open")).not.toBeNull();
      expect(buttonIn(id, "card-edit-body") !== null).toBe(wantBody);
      expect(buttonIn(id, "card-edit-note") !== null).toBe(wantNote);
    });
  }

  it("边界：emoji_sync 与未知类型都没有「编辑内容」，但都有「编辑备注」", () => {
    const boundary = CARD_TYPES.filter(
      (c) => c.contentType === "emoji_sync" || c.contentType === "unknown_type"
    );
    // 先证明这两张卡真的在列表里 —— 空循环会让下面的 for 一次都不执行。
    expect(boundary.length).toBe(2);
    for (const { id, contentType } of boundary) {
      expect(buttonIn(id, "card-edit-body")).toBeNull();
      expect(buttonIn(id, "card-edit-note")).not.toBeNull();
      // 与源码里的判据核对：这两类都不在正文白名单里，也都不是"正文可编辑类型"。
      expect(isBodyEditable(contentType)).toBe(false);
    }
  });

  it("rich_text 两个按钮都要有（正文可编辑，备注也总能编辑）", () => {
    expect(buttonIn(4, "card-edit-body")).not.toBeNull();
    expect(buttonIn(4, "card-edit-note")).not.toBeNull();
  });

  it("每个条目都有「编辑备注」——没有例外（含四类正文可编辑的类型）", () => {
    const missing = CARD_TYPES.map((c) => c.id).filter((id) => !buttonIn(id, "card-edit-note"));
    expect(missing).toEqual([]);
    // 这条断言也必须能真的识别缺失：把「打开」按钮当成一个不存在的 id 去查，
    // 结果应当是"全部缺失"。否则 `missing === []` 可能只是因为查询永远返回 null。
    const allMissing = CARD_TYPES.map((c) => c.id).filter((id) => !buttonIn(id, "card-does-not-exist"));
    expect(allMissing.length).toBe(CARD_TYPES.length);
  });
});

describe("R12｜TagManager 用的是共享判据，不是自己的一套", () => {
  const SOURCE = fs.readFileSync(
    path.join(path.dirname(fileURLToPath(import.meta.url)), "TagManager.tsx"),
    "utf8"
  );

  it("从 features/clipboard/types 引入了 isBodyEditable / isNoteEditable", () => {
    expect(SOURCE).toMatch(/from\s+["'][^"']*clipboard\/types["']/);
    expect(SOURCE).toMatch(/\bisBodyEditable\b/);
    expect(SOURCE).toMatch(/\bisNoteEditable\b/);
  });

  it("不再自带 BINARY_CONTENT_TYPES / isBinaryContentType（分叉的源头）", () => {
    // 注释里提到旧名字是可以的（说明为什么废弃）；这里禁的是**定义与调用**。
    expect(SOURCE).not.toMatch(/const\s+BINARY_CONTENT_TYPES\s*=/);
    expect(SOURCE).not.toMatch(/const\s+isBinaryContentType\s*=/);
    expect(SOURCE).not.toMatch(/isBinaryContentType\s*\(/);
  });

  it("卡片可见性走 resolveCardEditActions，而它就是那两个共享判据的组合", () => {
    for (const contentType of [
      "text", "code", "url", "rich_text",
      "image", "file", "video", "emoji_sync", "unknown_type", "what_is_this",
    ]) {
      expect(resolveCardEditActions(contentType)).toEqual({
        canEditBody: isBodyEditable(contentType),
        // 并集：正文可编辑的类型**也要**有独立的备注入口（用户原话"每个条目都要有"）。
        canEditNote: isNoteEditable(contentType) || isBodyEditable(contentType),
      });
    }
    // 缺字段的旧数据不该崩，也不该因此丢掉备注入口。
    expect(resolveCardEditActions(undefined)).toEqual({ canEditBody: false, canEditNote: true });
    expect(resolveCardEditActions(null)).toEqual({ canEditBody: false, canEditNote: true });
  });

  it("备注入口覆盖每一条，含正文可编辑的四类", () => {
    // 这四类是历史上被漏掉的一批：v0.5.4 的 `!isBodyEditable(t)` 判据把它们全部排除
    // 在备注入口之外，而用户要求的是"每个条目都要有这个按钮"。
    const bodyEditable = ["text", "code", "url", "rich_text"];
    for (const t of bodyEditable) {
      expect(resolveCardEditActions(t).canEditBody).toBe(true);
      expect(resolveCardEditActions(t).canEditNote).toBe(true);
    }
  });

  it("备注判据恒真：不依赖内容类型（所以不会再有「漏掉某一批」）", () => {
    // 这条钉的是"根因"而不是"现象"：两轮修复都错在拿正文的可编辑性去派生备注的。
    // 现在 isNoteEditable 对所有输入都返回 true，任何内容类型都不会被漏掉 ——
    // 包括后端将来新增的、前端此刻还不认识的类型。
    for (const t of ["text", "code", "url", "rich_text", "image", "file", "video", "emoji_sync", "future_kind", "", "任意新类型"]) {
      expect(isNoteEditable(t)).toBe(true);
      expect(resolveCardEditActions(t).canEditNote).toBe(true);
    }
  });
});

describe("R12｜两种弹窗：只渲染自己那一个字段", () => {
  it("「编辑内容」进去只有正文框，没有备注框", async () => {
    await click(buttonIn(1, "card-edit-body")!);
    expect(dialog()).not.toBeNull();
    expect(dialog()!.getAttribute("data-testid")).toBe("item-editor-body");
    const fields = textareas();
    expect(fields.length).toBe(1);
    expect(fields[0].value).toBe(BODY);
  });

  it("「编辑备注」进去只有备注框，没有正文框", async () => {
    await click(buttonIn(1, "card-edit-note")!);
    expect(dialog()).not.toBeNull();
    expect(dialog()!.getAttribute("data-testid")).toBe("item-editor-note");
    const fields = textareas();
    expect(fields.length).toBe(1);
    // 备注框里必须是备注的初值，不是正文 —— 否则用户在备注框里会看到正文。
    expect(fields[0].value).toBe(NOTE);
  });

  it("两个入口的弹窗标题不同", async () => {
    await click(buttonIn(1, "card-edit-body")!);
    const bodyTitle = dialogTitle();
    await closeDialog();
    expect(dialog()).toBeNull();
    await click(buttonIn(1, "card-edit-note")!);
    const noteTitle = dialogTitle();
    expect(bodyTitle).toBe("edit_item_body_title");
    expect(noteTitle).toBe("edit_item_note_title");
    expect(bodyTitle).not.toBe(noteTitle);
  });
});

describe("R12｜保存分开：备注弹窗不写正文，正文弹窗不写备注", () => {
  it("备注弹窗保存只发 update_entry_note", async () => {
    await click(buttonIn(1, "card-edit-note")!);
    const field = textareas()[0];
    await typeInto(field, "备注改了");
    expect(field.value).toBe("备注改了"); // 否则下面会因为"输入没生效"而恒真
    await click(saveButton());

    expect(callsTo("update_entry_note").length).toBe(1);
    expect(callsTo("update_entry_note")[0][1]).toMatchObject({ id: 1, note: "备注改了" });
    expect(callsTo("update_item_content").length).toBe(0);
  });

  it("正文弹窗保存只发 update_item_content", async () => {
    await click(buttonIn(1, "card-edit-body")!);
    const field = textareas()[0];
    await typeInto(field, "正文改了");
    expect(field.value).toBe("正文改了");
    await click(saveButton());

    expect(callsTo("update_item_content").length).toBe(1);
    expect(callsTo("update_item_content")[0][1]).toMatchObject({ id: 1, newContent: "正文改了" });
    expect(callsTo("update_entry_note").length).toBe(0);
  });

  it("图片这类没有正文入口的类型，备注弹窗保存也绝不写正文", async () => {
    const imageId = 5;
    await click(buttonIn(imageId, "card-edit-note")!);
    await typeInto(textareas()[0], "图片备注");
    await click(saveButton());
    expect(callsTo("update_entry_note").length).toBe(1);
    expect(callsTo("update_item_content").length).toBe(0);
  });

  it("保存计划是纯函数，且按模式门控：note 模式永不写正文", () => {
    const base = { content: BODY, note: NOTE, originalContent: BODY, originalNote: NOTE };
    // 备注模式：即便正文真的与原文不一致（预填、格式化、未来新增的字段同步），也不写。
    expect(
      resolveEditSavePlan({ ...base, mode: "note", content: "正文被动了", note: "新备注" })
    ).toEqual({ writeBody: false, writeNote: true });
    // 正文模式：反过来。
    expect(
      resolveEditSavePlan({ ...base, mode: "body", content: "新正文", note: "备注被动了" })
    ).toEqual({ writeBody: true, writeNote: false });
    // 都没动 → 一个写入都不发。
    expect(resolveEditSavePlan({ ...base, mode: "note" })).toEqual({
      writeBody: false,
      writeNote: false,
    });
    expect(resolveEditSavePlan({ ...base, mode: "body" })).toEqual({
      writeBody: false,
      writeNote: false,
    });
  });
});
