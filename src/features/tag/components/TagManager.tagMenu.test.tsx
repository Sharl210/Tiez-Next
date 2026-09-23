// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import TagManager from "./TagManager";
import {
  TagGroupContextMenu,
  resolveContextMenuPosition,
} from "./TagGroupContextMenu";

/**
 * B9：标签组的「重命名 / 删除」已从行内图标改为右键菜单。
 *
 * # 为什么必须真实挂载，而不是像 `TagManager.size.test.ts` 那样静态渲染
 *
 * 那 44 个测试断言的是可以静态推导的几何值（分栏宽高来自 props）。本组断言的对象
 * 是"标签行里到底渲染了什么"和"右键之后发生什么"，而标签行来自 `fetchTags()` 的
 * 异步结果：静态渲染时 `tags` 仍是初始空数组，一行都不会出现。在空列表上断言
 * "行内没有重命名按钮"会恒真——那种通过的测试什么也证明不了。所以这里走完整挂载：
 * mock 掉 Tauri 宿主 API，让 `get_all_tags_info` 真的返回标签，再对渲染出的真实
 * DOM 下断言。
 */

const { invokeMock, emitMock } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
  emitMock: vi.fn(async () => undefined),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: invokeMock,
  convertFileSrc: (p: string) => p,
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
  emit: emitMock,
}));

/** 本轮渲染的标签组：名称 -> 条目数。 */
const GROUP_TAGS: Record<string, number> = { 工作: 5, 学习: 2 };

/**
 * 透传式 `t`，只对需要断言的插值模板给出真实形状。
 *
 * 身份函数 `t` 会让确认框里的 `{count}` 永远保持原样（没有可替换的占位符），于是
 * "确认框写清了受影响条目数"这条断言在看身份函数时会假失败。这两个键的形状取自
 * `src/locales.ts` 的实际值，不引入任何新文案。
 */
const T_OVERRIDES: Record<string, string> = {
  confirm_delete_tag_scope: "将解除 {count} 条条目与该分组的关联；条目本身会保留，不会被删除。",
};
const t = (key: string) => T_OVERRIDES[key] ?? key;

let container: HTMLDivElement;
let root: Root;

const flush = async () => {
  // 两轮微任务 + act：fetchTags 的 Promise.all 链要在断言前落地。
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
};

const mount = async () => {
  await act(async () => {
    root.render(createElement(TagManager, { t, theme: "light" }));
  });
  await flush();
};

/** 按可见文本取标签行。 */
const tagRow = (name: string): HTMLElement => {
  const rows = Array.from(container.querySelectorAll<HTMLElement>(".tag-item"));
  const row = rows.find((el) => el.querySelector(".tag-name")?.textContent === name);
  if (!row) throw new Error(`找不到标签行：${name}`);
  return row;
};

const menu = () => document.querySelector<HTMLElement>(".tag-group-menu");
const menuItems = () =>
  Array.from(document.querySelectorAll<HTMLElement>('[role="menuitem"]'));

const rightClick = async (el: HTMLElement, x = 120, y = 240) => {
  await act(async () => {
    el.dispatchEvent(
      new MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: x, clientY: y })
    );
  });
};

/**
 * jsdom 没有实现 `matchMedia`，而该组件用它判断"是否处于窄视口（堆叠布局）"。
 * 真实浏览器里它始终存在，所以这是测试环境缺口，不是产品缺陷——补一个只回
 * `matches: false` 的最小实现，让布局落在宽屏分支上。
 * 形状跟随组件实际读取的字段（`matches` 与 `addEventListener`/`removeEventListener`），
 * 不多造用不上的能力。
 */
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

beforeEach(async () => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  installMatchMedia();
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string) => {
    switch (cmd) {
      case "get_all_tags_info":
        return { ...GROUP_TAGS };
      case "get_tag_colors":
        return {};
      case "get_settings":
        // 隐私保护开着：`sensitive` 组不参与本组断言，但不该因此改变其他组。
        return { privacy_protection: "true" };
      default:
        return undefined;
    }
  });
  emitMock.mockClear();
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

describe("B9｜标签组行内不再直接显示重命名/删除按钮", () => {
  it("前置条件：标签行确实渲染出来了（否则下面的断言会恒真）", () => {
    expect(container.querySelectorAll(".tag-item").length).toBeGreaterThanOrEqual(2);
    expect(tagRow("工作").textContent).toContain("工作");
  });

  it("行内没有 hover 动作组", () => {
    expect(container.querySelector(".tag-hover-actions")).toBeNull();
  });

  it("行内没有任何 title=rename / title=delete 的元素", () => {
    const names = ["rename", "delete"];
    const found = Array.from(container.querySelectorAll(".tag-item [title]"))
      .map((el) => el.getAttribute("title"))
      .filter((title) => title !== null && names.includes(title));
    expect(found).toEqual([]);
  });

  it("行内不渲染 Edit2 / Trash2 图标（只保留色点 + 名称 + 条目数）", () => {
    const row = tagRow("工作");
    expect(row.querySelector("svg")).toBeNull();
    expect(row.querySelector(".tag-name")?.textContent).toBe("工作");
    expect(row.querySelector(".tag-badge")?.textContent).toBe("5");
  });
});

describe("B9｜右键标签组弹出菜单", () => {
  it("右键前没有菜单，右键后同一个查询能查到菜单", async () => {
    // 只断言"右键前为 null"是恒真的：一个从不渲染菜单的实现同样满足它。
    // 因此必须在同一个测试里证明"这个查询确实能返回菜单"，两条合起来才有判别力。
    expect(menu()).toBeNull();
    await rightClick(tagRow("工作"));
    expect(menu()).not.toBeNull();
  });

  it("右键后出现菜单，含「重命名」「删除」两项", async () => {
    await rightClick(tagRow("工作"));
    const m = menu();
    expect(m).not.toBeNull();
    // 取按钮内的文字节点，不取整个 textContent：删除项右侧还有一个独立的计数徽标
    // （`<span class="tag-group-menu-count">`），拼进来就成了 "delete5"。
    const labels = menuItems().map(
      (el) => el.querySelector("span:not(.tag-group-menu-count)")?.textContent
    );
    expect(labels).toEqual(["rename", "delete"]);
  });

  it("菜单项是可聚焦的 button 且带 role=menuitem（不是裸 div）", async () => {
    await rightClick(tagRow("工作"));
    const items = menuItems();
    // 空的 NodeList 会让下面的 for 循环一次都不执行，"全部通过"变成恒真。
    // 先断言数量，再逐项检查属性。
    expect(items.length).toBeGreaterThan(0);
    expect(items.length).toBe(2);
    for (const item of items) {
      expect(item.tagName).toBe("BUTTON");
      expect(item.getAttribute("type")).toBe("button");
      expect(item.getAttribute("role")).toBe("menuitem");
    }
  });

  it("菜单 portal 到 body，不是标签列表的后代（因此不被 overflow 裁掉）", async () => {
    await rightClick(tagRow("工作"));
    const m = menu();
    expect(m?.parentElement).toBe(document.body);
    expect(container.contains(m)).toBe(false);
  });

  it("右键不会冒泡成浏览器原生菜单", async () => {
    const row = tagRow("工作");
    const ev = new MouseEvent("contextmenu", { bubbles: true, cancelable: true });
    await act(async () => {
      row.dispatchEvent(ev);
    });
    expect(ev.defaultPrevented).toBe(true);
  });

  it("menu 记录了被右键的组名与其条目数", async () => {
    await rightClick(tagRow("学习"));
    expect(menu()?.getAttribute("data-tag-group-menu")).toBe("学习");
    expect(menu()?.querySelector(".tag-group-menu-count")?.textContent).toBe("2");
  });
});

describe("B9｜菜单关闭", () => {
  it("Escape 关闭菜单", async () => {
    await rightClick(tagRow("工作"));
    expect(menu()).not.toBeNull();
    await act(async () => {
      window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    });
    expect(menu()).toBeNull();
  });

  it("Escape 被拦截在捕获阶段，不冒泡给页面全局快捷键", async () => {
    await rightClick(tagRow("工作"));
    let bubbled = 0;
    const spy = (e: KeyboardEvent) => {
      if (e.key === "Escape") bubbled += 1;
    };
    window.addEventListener("keydown", spy, false);
    await act(async () => {
      window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    });
    window.removeEventListener("keydown", spy, false);
    expect(bubbled).toBe(0);
  });

  it("在菜单外部按下指针关闭菜单", async () => {
    await rightClick(tagRow("工作"));
    expect(menu()).not.toBeNull(); // 前置：先证明菜单真的开了，否则"关闭后为 null"恒真
    await act(async () => {
      document.body.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true }));
    });
    expect(menu()).toBeNull();
  });

  it("在菜单内部按下指针不关闭菜单", async () => {
    await rightClick(tagRow("工作"));
    await act(async () => {
      menuItems()[0].dispatchEvent(new MouseEvent("pointerdown", { bubbles: true }));
    });
    expect(menu()).not.toBeNull();
  });

  it("滚动时关闭菜单（锚点行会随列表滚走）", async () => {
    await rightClick(tagRow("工作"));
    expect(menu()).not.toBeNull(); // 前置：菜单确实开过
    await act(async () => {
      window.dispatchEvent(new Event("scroll"));
    });
    expect(menu()).toBeNull();
  });

  it("窗口尺寸变化时关闭菜单", async () => {
    await rightClick(tagRow("工作"));
    expect(menu()).not.toBeNull(); // 前置：菜单确实开过
    await act(async () => {
      window.dispatchEvent(new Event("resize"));
    });
    expect(menu()).toBeNull();
  });

  it("在别处再次右键时，菜单改为指向新的那一组而不是叠加两个", async () => {
    await rightClick(tagRow("工作"));
    await rightClick(tagRow("学习"));
    expect(document.querySelectorAll(".tag-group-menu").length).toBe(1);
    expect(menu()?.getAttribute("data-tag-group-menu")).toBe("学习");
  });
});

describe("B9｜菜单动作复用既有链路", () => {
  it("「重命名」进入行内编辑态，输入框预填当前组名", async () => {
    await rightClick(tagRow("工作"));
    await act(async () => {
      menuItems()[0].click();
    });
    const input = container.querySelector<HTMLInputElement>("input.inline-tag-edit");
    expect(input).not.toBeNull();
    expect(input?.value).toBe("工作");
    // 菜单自身已关闭，不留在编辑态上面。
    expect(menu()).toBeNull();
  });

  it("「删除」只打开二次确认框，绝不当场调用删除命令", async () => {
    const row = tagRow("工作");
    await rightClick(row);
    await act(async () => {
      menuItems()[1].click();
    });
    // 确认框出现，并且写清了受影响条目数。
    const dialog = container.querySelector(".tag-manager-dialog");
    expect(dialog).not.toBeNull();
    expect(dialog?.textContent).toContain("工作");
    expect(dialog?.querySelector(".tag-delete-scope")?.textContent).toContain("5");
    // 关键：此刻后端删除命令一次都没被调用。
    expect(invokeMock).not.toHaveBeenCalledWith("delete_tag_from_all", expect.anything());
    // 确认框出现后组还在。
    expect(container.querySelectorAll(".tag-item").length).toBe(2);
  });

  it("确认框里确认后才真正删除该组", async () => {
    await rightClick(tagRow("工作"));
    await act(async () => {
      menuItems()[1].click();
    });
    const confirmBtn = Array.from(
      container.querySelectorAll<HTMLButtonElement>(".confirm-dialog-buttons button")
    ).find((b) => b.textContent?.trim() === "delete");
    expect(confirmBtn).toBeDefined();
    await act(async () => {
      confirmBtn!.click();
    });
    await flush();
    expect(invokeMock).toHaveBeenCalledWith("delete_tag_from_all", { tagName: "工作" });
  });

  it("确认框里取消则不调用删除命令", async () => {
    await rightClick(tagRow("工作"));
    await act(async () => {
      menuItems()[1].click();
    });
    const cancelBtn = Array.from(
      container.querySelectorAll<HTMLButtonElement>(".confirm-dialog-buttons button")
    ).find((b) => b.textContent?.trim() === "cancel");
    expect(cancelBtn).toBeDefined();
    await act(async () => {
      cancelBtn!.click();
    });
    expect(invokeMock).not.toHaveBeenCalledWith("delete_tag_from_all", expect.anything());
  });
});

describe("B9｜菜单定位", () => {
  it("常规落点：菜单出现在落点处", () => {
    expect(
      resolveContextMenuPosition({
        anchorX: 120, anchorY: 240, menuWidth: 152, menuHeight: 80,
        viewportWidth: 1024, viewportHeight: 768,
      })
    ).toEqual({ left: 120, top: 240 });
  });

  it("右/下越界时回退，保证整个菜单都在视口内", () => {
    const pos = resolveContextMenuPosition({
      anchorX: 1000, anchorY: 760, menuWidth: 152, menuHeight: 80,
      viewportWidth: 1024, viewportHeight: 768,
    });
    expect(pos.left + 152).toBeLessThanOrEqual(1024);
    expect(pos.top + 80).toBeLessThanOrEqual(768);
    expect(pos.left).toBe(1024 - 152 - 8);
    expect(pos.top).toBe(768 - 80 - 8);
  });

  it("视口比菜单还小时至少留出边距，不产生负坐标", () => {
    const pos = resolveContextMenuPosition({
      anchorX: 5, anchorY: 5, menuWidth: 400, menuHeight: 300,
      viewportWidth: 200, viewportHeight: 150,
    });
    expect(pos.left).toBeGreaterThanOrEqual(8);
    expect(pos.top).toBeGreaterThanOrEqual(8);
  });

  it("负落点被夹回边距内", () => {
    const pos = resolveContextMenuPosition({
      anchorX: -20, anchorY: -20, menuWidth: 152, menuHeight: 80,
      viewportWidth: 1024, viewportHeight: 768,
    });
    expect(pos).toEqual({ left: 8, top: 8 });
  });
});

describe("B9｜菜单组件单独契约", () => {
  let host: HTMLDivElement;
  let menuRoot: Root;

  beforeEach(() => {
    host = document.createElement("div");
    document.body.appendChild(host);
    menuRoot = createRoot(host);
  });

  afterEach(async () => {
    // 外层 beforeEach 失败时本层 root 可能根本没建起来；清理要能容忍这种情况，
    // 否则真正的失败原因会被一个清理期的 TypeError 盖掉。
    if (menuRoot) {
      await act(async () => {
        menuRoot.unmount();
      });
    }
    host.remove();
  });

  it("渲染出的第一项自动获得焦点（键盘用户不必先 Tab）", async () => {
    const onRename = vi.fn();
    await act(async () => {
      menuRoot.render(
        createElement(TagGroupContextMenu, {
          x: 10, y: 10, tagName: "工作", affectedCount: 3,
          t: (k: string) => k, onRename, onDelete: vi.fn(), onClose: vi.fn(),
        })
      );
    });
    expect(document.activeElement?.getAttribute("role")).toBe("menuitem");
    expect(document.activeElement?.textContent).toBe("rename");
  });

  it("点击「重命名」触发 onClose 后 onRename（先关菜单再进编辑态）", async () => {
    const order: string[] = [];
    await act(async () => {
      menuRoot.render(
        createElement(TagGroupContextMenu, {
          x: 10, y: 10, tagName: "工作", affectedCount: 3,
          t: (k: string) => k,
          onRename: () => order.push("rename"),
          onDelete: () => order.push("delete"),
          onClose: () => order.push("close"),
        })
      );
    });
    await act(async () => {
      menuItems()[0].click();
    });
    expect(order).toEqual(["close", "rename"]);
  });

  it("菜单内的右键不会把菜单自己关掉", async () => {
    const onClose = vi.fn();
    await act(async () => {
      menuRoot.render(
        createElement(TagGroupContextMenu, {
          x: 10, y: 10, tagName: "工作", affectedCount: 0,
          t: (k: string) => k, onRename: vi.fn(), onDelete: vi.fn(), onClose,
        })
      );
    });
    await act(async () => {
      document.querySelector(".tag-group-menu")!.dispatchEvent(
        new MouseEvent("contextmenu", { bubbles: true, cancelable: true })
      );
    });
    expect(onClose).not.toHaveBeenCalled();
  });

  it("条目数为 0 时不显示计数徽标", async () => {
    await act(async () => {
      menuRoot.render(
        createElement(TagGroupContextMenu, {
          x: 10, y: 10, tagName: "空组", affectedCount: 0,
          t: (k: string) => k, onRename: vi.fn(), onDelete: vi.fn(), onClose: vi.fn(),
        })
      );
    });
    expect(document.querySelector(".tag-group-menu-count")).toBeNull();
  });

  it("上下方向键在两项之间移动焦点并循环", async () => {
    await act(async () => {
      menuRoot.render(
        createElement(TagGroupContextMenu, {
          x: 10, y: 10, tagName: "工作", affectedCount: 3,
          t: (k: string) => k, onRename: vi.fn(), onDelete: vi.fn(), onClose: vi.fn(),
        })
      );
    });
    // 焦点项取文字节点，去掉删除项尾部那个计数徽标。
    const focused = () =>
      document.activeElement?.querySelector("span:not(.tag-group-menu-count)")?.textContent ?? null;
    expect(focused()).toBe("rename");

    const press = async (key: string) => {
      await act(async () => {
        document.activeElement?.dispatchEvent(
          new KeyboardEvent("keydown", { key, bubbles: true })
        );
      });
    };

    await press("ArrowDown");
    expect(focused()).toBe("delete");
    // 末项再往下回到首项（循环，不停在边界）。
    await press("ArrowDown");
    expect(focused()).toBe("rename");
    await press("ArrowUp");
    expect(focused()).toBe("delete");
  });
});
