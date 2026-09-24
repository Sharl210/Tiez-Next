// @vitest-environment jsdom
/**
 * 守住「设置页分组默认收起」这条真机回归。
 *
 * # 背景
 *
 * `useSettingsPanelReset` 的重置字典曾硬编码 9 个键，而 `useAppState` 的初值有 12 个。
 * 新增的 `mcp` / `auto_backup` 只加了初值，没加重置字典。由于重置走的是
 * `setCollapsedGroups({...})`（**整体替换**），替换后这两个键变成 `undefined`，
 * 被 `SettingsPanel` 的 `collapsed ? "collapsed" : ""` 当成 falsy → **判定为展开**。
 *
 * 用户看到的是「MCP 与自动容灾备份打开设置页就是展开的，其他都是收起的」。
 *
 * # 为什么用真实挂载而不是断言常量
 *
 * 「断言重置字典等于某个字面量」只能证明"此刻这两份清单一样"，改起来同样方便，
 * 守不住回归。本测试**复刻真实链路**：一个持有 state 的宿主组件 → 打开设置页 →
 * hook 触发重置 → 对**渲染出来的 class** 下断言。class 是用户最终看到的东西。
 *
 * 本仓库既有测试（`AutoBackupPanel.test.tsx`）用 `react-dom/client` 手工挂载，
 * 未引入 `@testing-library/react`；这里沿用同一套做法，避免新增依赖。
 */
import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { act, createElement, useState, useEffect, useRef } from "react";
import { createRoot, type Root } from "react-dom/client";
import { useSettingsPanelReset } from "./useSettingsPanelReset";
import {
  SETTINGS_GROUP_KEYS,
  createAllCollapsedGroups
} from "../config/settingsGroups";

/**
 * 模块级空函数 —— **必须**是稳定引用。
 *
 * 第一版把 `setSettingsSubpage: () => {}` 内联在组件里，每次渲染都产生新身份；
 * `useSettingsPanelReset` 的 effect 依赖它 → effect 每帧重跑 → `setCollapsedGroups`
 * 每次都收到新对象 → 重渲染 → **死循环 → vitest OOM**。
 *
 * 这与真机上「备份列表无限刷新循环」是同一类缺陷（不稳定回调进依赖数组）。
 * 生产代码里 `setSettingsSubpage` 来自 `useState`，身份天然稳定；测试里必须手动保证。
 */
const noop = () => {};

/**
 * 复刻 `SettingsPanel` 的判定：`collapsed ? "collapsed" : ""`。
 *
 * 单独抽出来，是为了让「`undefined` 也是 falsy」这件事在测试里显式可见 ——
 * 那正是 bug 藏身之处（`Record<string, boolean>` 的索引在类型上允许缺键）。
 */
const collapseClassOf = (groups: Record<string, boolean>, key: string): string =>
  groups[key] ? "collapsed" : "";

/**
 * 宿主组件：持有 `collapsedGroups` state，把判定结果渲染成 DOM。
 *
 * **不要**在这里用 `useEffect` 向上报告 state —— 那会让"报告"本身触发重渲染，
 * 与树上的 key 变化形成闭环（本文件第一版就是这么写的，直接把 vitest 跑成 OOM）。
 * 断言改为直接读渲染出来的 DOM，不需要把 state 传出去。
 */
function Harness() {
  const [collapsedGroups, setCollapsedGroups] = useState<Record<string, boolean>>(
    createAllCollapsedGroups
  );
  const [showSettings, setShowSettings] = useState(false);
  const setShow = useRef<((v: boolean) => void) | null>(null);
  setShow.current = setShowSettings;

  // 暴露给测试，用来"打开设置页"
  useEffect(() => {
    (window as unknown as { __openSettings?: () => void }).__openSettings = () =>
      setShow.current?.(true);
  }, []);

  useSettingsPanelReset({
    showSettings,
    setCollapsedGroups,
    setSettingsSubpage: noop
  });

  return createElement(
    "div",
    null,
    ...SETTINGS_GROUP_KEYS.map((key) =>
      createElement("div", {
        key,
        "data-group": key,
        className: collapseClassOf(collapsedGroups, key)
      })
    )
  );
}

describe("设置页分组默认状态", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
  });

  it("打开设置页后，每个分组渲染出来的 class 都应是 collapsed", () => {
    act(() => {
      root.render(createElement(Harness));
    });

    // 模拟用户打开设置页 → 触发 hook 里的重置
    act(() => {
      (window as unknown as { __openSettings: () => void }).__openSettings();
    });

    // 【核心断言】看**渲染结果**，不是看重置字典里有没有这个键。
    // 缺键时 groups[key] 为 undefined → class 是空串 → 用户看到展开。
    const expanded = SETTINGS_GROUP_KEYS.filter((key) => {
      const el = container.querySelector(`[data-group="${key}"]`);
      return el?.className !== "collapsed";
    });

    expect(
      expanded,
      `这些分组在打开设置页后会展开（应全部收起）：${expanded.join(", ")}`
    ).toEqual([]);
  });

  it("重置字典必须覆盖全部键，且值都是 true", () => {
    const reset = createAllCollapsedGroups();
    const missing = SETTINGS_GROUP_KEYS.filter((key) => !(key in reset));
    expect(missing, `重置字典缺少这些键：${missing.join(", ")}`).toEqual([]);

    const notCollapsed = SETTINGS_GROUP_KEYS.filter((key) => reset[key] !== true);
    expect(notCollapsed, `这些键的值不是 true：${notCollapsed.join(", ")}`).toEqual([]);
  });

  it("每次调用返回新对象（React 依赖引用变化触发重渲染）", () => {
    const a = createAllCollapsedGroups();
    const b = createAllCollapsedGroups();
    expect(a).not.toBe(b);
    expect(a).toEqual(b);
  });
});
