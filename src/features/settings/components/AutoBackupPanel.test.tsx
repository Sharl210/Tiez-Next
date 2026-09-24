// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { act, createElement, useState } from "react";
import { createRoot, type Root } from "react-dom/client";
import AutoBackupSettingsGroup from "./groups/AutoBackupSettingsGroup";
import BackupListModal, { type AutoBackupListPayload } from "./BackupListModal";

/**
 * 自动容灾备份的前端界面（设置区块 + 「备份列表」悬浮窗）。
 *
 * # 为什么必须真实挂载
 *
 * 本组要断言的对象是"右键之后菜单里有什么""点了删除之后是不是先弹确认框""固定数
 * 达上限时提示里到底写了哪两个数字"。这些全部发生在**异步数据落地之后**：列表来自
 * `list_auto_backups`。静态渲染时 `entries` 仍是空数组，一行都不会出现，在空列表上
 * 断言"菜单里有恢复项"会恒真——那种通过的测试什么也证明不了。因此这里 mock 掉 Tauri
 * 宿主 API，让命令真的返回数据，再对渲染出的真实 DOM 下断言。
 *
 * # 文案
 *
 * `t` 用**三语词条的真实形状**（取自待插入 `src/locales.ts` 的文案），因为要断言的
 * 正是"提示里的数字被真实替换"。身份函数会让 `{maxKeep}` 永远保持原样，从而假失败。
 */

const { invokeMock, listenMock } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
  listenMock: vi.fn(async () => () => {}),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: invokeMock,
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: listenMock,
}));

const CONFIG = { enabled: true, intervalMinutes: 30, maxKeep: 20, backupOnStartup: true };

/** 两份有区分度的备份：一份定时且未固定，一份启动且已固定。 */
const ENTRIES = [
  {
    archiveName: "Tiez-Next-auto-timed-20260923T120000-01.zip",
    path: "/home/u/.local/share/Tiez-Next/auto_backups/Tiez-Next-auto-timed-20260923T120000-01.zip",
    origin: "scheduled",
    createdAt: "2026-09-23T12:00:00+08:00",
    createdAtMs: 1,
    createdAtLocal: "2026-09-23 12:00:00",
    sizeBytes: 2048,
    pinned: false,
    seq: 1,
  },
  {
    archiveName: "Tiez-Next-auto-startup-20260923T090000-01-p.zip",
    path: "/home/u/.local/share/Tiez-Next/auto_backups/Tiez-Next-auto-startup-20260923T090000-01-p.zip",
    origin: "startup",
    createdAt: "2026-09-23T09:00:00+08:00",
    createdAtMs: 2,
    createdAtLocal: "2026-09-23 09:00:00",
    sizeBytes: 3174400,
    pinned: true,
    seq: 1,
  },
];

const listPayload = (over: Partial<AutoBackupListPayload> = {}): AutoBackupListPayload => ({
  dir: "/home/u/.local/share/Tiez-Next/auto_backups",
  config: CONFIG,
  maxPinned: CONFIG.maxKeep - 1,
  pinnedCount: ENTRIES.filter((e) => e.pinned).length,
  totalCount: ENTRIES.length,
  entries: ENTRIES,
  warnings: [],
  ...over,
});

/** 三语词条的真实形状（只取本组要断言的键）。 */
const T: Record<string, string> = {
  auto_backup_section: "自动备份（容灾）",
  auto_backup_enabled: "定时备份",
  auto_backup_interval: "备份周期（分钟）",
  auto_backup_max_keep: "最大留存份数",
  auto_backup_max_keep_hint: "可调 {min}–{max} 份，超出后自动删除最老的一份",
  auto_backup_on_startup: "启动时自动备份一次",
  auto_backup_list: "备份列表",
  auto_backup_list_open: "查看列表",
  auto_backup_summary: "{total} 份备份 · 已固定 {pinned} · 共 {size}",
  auto_backup_summary_unknown: "尚未统计",
  auto_backup_origin_scheduled: "定时",
  auto_backup_origin_startup: "启动",
  auto_backup_pinned_badge: "已固定",
  auto_backup_pin: "固定",
  auto_backup_unpin: "取消固定",
  auto_backup_restore: "恢复此备份",
  auto_backup_pinned_badge_short: "固定",
  // 【必须与 src/locales.ts 的真实形状一致】这两条**不含任何占位符**，身份信息
  // 由弹窗单独渲染成一行。早期版本的桩在这里写了 {time}/{size}/{origin}，于是
  // "确认框写清了删的是哪一份"这条断言靠着桩自己编的占位符通过了——而真实词条里
  // 根本没有占位符，用户在真机上看到的是一个不指明对象的确认框。桩写得不真实，
  // 测试就会失去判别力。
  auto_backup_delete_title: "删除这份备份？",
  auto_backup_delete_confirm: "删除后无法通过本列表找回。确认删除这份备份？",
  auto_backup_restore_title: "用这份备份恢复？",
  auto_backup_restore_confirm:
    "恢复会用这份备份替换当前的全部数据，并先为当前数据建一份旁路备份。恢复后需要重启应用。确认恢复？",
  backup_import_restart: "请重启应用以加载导入的数据（当前界面显示的仍是导入前的内容）。",
  backup_import_restart_now: "立即重启",
  auto_backup_modal_counts: "共 {total} 份 · 已固定 {pinned}（固定上限 {maxPinned}）",
  auto_backup_err_pinned_limit_reached:
    "当前配置最大留存备份数量为 {maxKeep}，而当前您已固定 {currentPinned} 个，不能全部设置为固定，否则后续新增备份没有轮位可用于存储。",
  auto_backup_err_max_keep_out_of_range:
    "最大留存备份份数必须在 {min}–{max} 之间（当前为 {value}）。",
  auto_backup_err_interval_out_of_range: "定时备份周期必须在 {min}–{max} 分钟之间（当前为 {value}）。",
  delete: "删除",
  cancel: "取消",
  save: "保存",
};
const t = (key: string) => T[key] ?? key;

let container: HTMLDivElement;
let root: Root;

const flush = async () => {
  // 多轮微任务 + act：`invoke` 的 Promise 链要在断言前落地。
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  });
};

beforeEach(() => {
  invokeMock.mockReset();
  listenMock.mockClear();
  invokeMock.mockImplementation(async (cmd: string) => {
    if (cmd === "get_auto_backup_config") return CONFIG;
    if (cmd === "list_auto_backups") return listPayload();
    if (cmd === "set_auto_backup_config") return CONFIG;
    if (cmd === "set_auto_backup_pinned") return true;
    if (cmd === "delete_auto_backup") return undefined;
    if (cmd === "restore_auto_backup") return {};
    return undefined;
  });
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  document.querySelectorAll(".tag-group-menu").forEach((n) => n.remove());
});

const LabelWithHint = ({ label }: { label: string }) => createElement("span", { className: "item-label" }, label);

const mountGroup = async (collapsed = false) => {
  await act(async () => {
    root.render(
      createElement(AutoBackupSettingsGroup, {
        t,
        collapsed,
        onToggle: () => {},
        LabelWithHint,
        theme: "light",
      })
    );
  });
  await flush();
};

/** 直接挂载悬浮窗（列表入口在设置区块里，两者分开挂能让断言更聚焦）。 */
const mountModal = async () => {
  await act(async () => {
    root.render(
      createElement(BackupListModal, { open: true, t, theme: "light", onClose: () => {} })
    );
  });
  await flush();
};

/**
 * 挂载悬浮窗，并**故意每轮渲染都传一个新的 `onLoaded`**。
 *
 * 这正是父组件 `AutoBackupSettingsGroup` 的真实写法（内联箭头函数），也是当年那个
 * 无限刷新循环的起点。用一个稳定的函数挂载会让本组测试失去判别力——它测不到循环。
 */
const mountModalWithUnstableOnLoaded = async () => {
  await act(async () => {
    root.render(
      createElement(BackupListModal, {
        open: true,
        t,
        theme: "light",
        onClose: () => {},
        // 【必须内联】每次 render() 都新建一个函数引用，模拟父组件重渲染。
        onLoaded: () => {
          reRenders += 1;
        },
      })
    );
  });
  await flush();
};

/** 供上面那个不稳定回调累加"父组件第几次收到通知"。 */
let reRenders = 0;

/**
 * **完整闭环**的最小复现：一个持有 state 的父组件，把 `onLoaded` 写成内联箭头函数，
 * 并在回调里 `setState`。
 *
 * # 为什么必须有它（而不是只传一个不稳定的函数）
 *
 * 只把 `onLoaded` 传成新的内联函数，并**不足以**复现用户看到的循环 —— 因为闭环的
 * 最后一环是「回调修改父组件状态 → 父组件重渲染 → 生成新的内联函数」。少了这一环，
 * `refresh` 重建后没有任何东西会让它再次重建，循环自然不成立，测试也就抓不到它
 * （实测：只传不稳定函数的版本在**修复前**的旧代码上也会通过 —— 那种测试是假的）。
 *
 * 这里 `setSummary` 每次写入一个新对象字面量，与
 * `AutoBackupSettingsGroup.tsx` 里那段 `setSummary({ total, pinned, bytes })`
 * 逐字同构：新对象引用 → React 必然重渲染 → 必然产出新的内联 `onLoaded`。
 */
const ParentHarness = () => {
  const [, setSummary] = useState<{ total: number; pinned: number; bytes: number } | null>(null);
  return createElement(BackupListModal, {
    open: true,
    t,
    theme: "light",
    onClose: () => {},
    onLoaded: (p: AutoBackupListPayload) => {
      summaryWrites += 1;
      setSummary({
        total: p.totalCount,
        pinned: p.pinnedCount,
        bytes: p.entries.reduce((sum, e) => sum + e.sizeBytes, 0),
      });
    },
  });
};

/** 父组件被回调写入了多少次（每次写入都会引发一轮父渲染）。 */
let summaryWrites = 0;

const rowByName = (name: string): HTMLElement => {
  const el = container.querySelector<HTMLElement>(`[data-backup-row="${name}"]`);
  if (!el) throw new Error(`找不到备份行：${name}`);
  return el;
};

const menu = () => document.querySelector<HTMLElement>(".tag-group-menu");
const menuItem = (kind: string) =>
  document.querySelector<HTMLButtonElement>(`[data-backup-menu-item="${kind}"]`);
const confirmBox = (kind: string) => document.querySelector<HTMLElement>(`[data-backup-confirm="${kind}"]`);

const rightClick = async (el: HTMLElement) => {
  await act(async () => {
    el.dispatchEvent(
      new MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: 120, clientY: 80 })
    );
  });
};

const click = async (el: Element | null) => {
  if (!el) throw new Error("要点击的元素不存在");
  await act(async () => {
    (el as HTMLElement).click();
  });
  await flush();
};

// ---------------------------------------------------------------------------
// 设置区块
// ---------------------------------------------------------------------------

describe("自动备份设置区块", () => {
  it("四个控件 + 备份列表入口都渲染出来", async () => {
    await mountGroup();
    const text = container.textContent ?? "";
    expect(text).toContain("自动备份（容灾）");
    expect(text).toContain("定时备份");
    expect(text).toContain("备份周期（分钟）");
    expect(text).toContain("最大留存份数");
    expect(text).toContain("启动时自动备份一次");
    expect(text).toContain("备份列表");
    expect(container.querySelector("[data-auto-backup-open-list]")).not.toBeNull();
  });

  it("渲染的是后端返回的配置：开关为开、周期 30、上限 20", async () => {
    await mountGroup();
    const maxKeep = container.querySelector<HTMLInputElement>("[data-auto-backup-max-keep]");
    expect(maxKeep?.value).toBe("20");
    // 定时开关（第一个 switch）与启动备份（带 data 属性那个）都应为勾选态。
    const startup = container.querySelector<HTMLInputElement>("[data-auto-backup-on-startup]");
    expect(startup?.checked).toBe(true);
  });

  /**
   * 用户原话："启动备份不随定时备份开关约束，只是放在里面作为一个勾选项而已"。
   *
   * 这条的**最易漏之处**在界面层：如果把这一项也写进 `config.enabled && (...)`，
   * 关掉定时备份后它就整块消失，用户根本没机会改它。因此断言的是"定时开关关闭时，
   * 这一项仍然存在且仍可点"。
   */
  it("定时备份关闭时，启动备份勾选项仍在（不受总开关约束）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "get_auto_backup_config") return { ...CONFIG, enabled: false };
      if (cmd === "list_auto_backups") return listPayload({ config: { ...CONFIG, enabled: false } });
      return CONFIG;
    });
    await mountGroup();

    const startup = container.querySelector<HTMLInputElement>("[data-auto-backup-on-startup]");
    expect(startup).not.toBeNull();
    expect(startup?.disabled).toBe(false);

    // 反向对照的锚点：**周期**项确实随总开关隐藏了——证明"条件渲染"这条机制
    // 在本组件里是真的生效的，上面那条不是因为条件渲染整体失效才通过。
    expect(container.textContent ?? "").not.toContain("备份周期（分钟）");
  });

  it("点启动备份开关时，只提交 backupOnStartup 这一个字段（不顺手改 enabled）", async () => {
    await mountGroup();
    const startup = container.querySelector<HTMLInputElement>("[data-auto-backup-on-startup]")!;
    await click(startup);

    const call = invokeMock.mock.calls.find((c) => c[0] === "set_auto_backup_config");
    expect(call).toBeTruthy();
    const patch = (call![1] as { patch: Record<string, unknown> }).patch;
    expect(Object.keys(patch)).toEqual(["backupOnStartup"]);
    expect(patch.backupOnStartup).toBe(false);
  });

  it("点定时开关时，只提交 enabled 这一个字段", async () => {
    await mountGroup();
    const boxes = Array.from(
      container.querySelectorAll<HTMLInputElement>('input[type="checkbox"]')
    );
    // 第一个 checkbox 是定时备份总开关（渲染顺序即代码顺序）。
    await click(boxes[0]);

    const call = invokeMock.mock.calls.find((c) => c[0] === "set_auto_backup_config");
    const patch = (call![1] as { patch: Record<string, unknown> }).patch;
    expect(Object.keys(patch)).toEqual(["enabled"]);
  });

  it("份数越界（501）不发给后端，就地提示合法区间且输入框回到真实值", async () => {
    await mountGroup();
    const maxKeep = container.querySelector<HTMLInputElement>("[data-auto-backup-max-keep]")!;

    await act(async () => {
      // 走 React 的受控输入通道（直接改 .value 不会触发 onChange）。
      const setter = Object.getOwnPropertyDescriptor(
        HTMLInputElement.prototype,
        "value"
      )!.set!;
      setter.call(maxKeep, "501");
      maxKeep.dispatchEvent(new Event("input", { bubbles: true }));
    });
    // 先证明"输入框里确实变成了 501"——否则下面那条"什么都没发给后端"会因为
    // 输入根本没生效而恒真（典型的空集合恒真断言）。
    expect(maxKeep.value).toBe("501");

    await act(async () => {
      // React 把 `onBlur` 挂到原生的 `focusout` 上（它才冒泡），直接派发 `blur`
      // 不会触发 React 的处理器。
      maxKeep.dispatchEvent(new FocusEvent("focusout", { bubbles: true }));
    });
    await flush();

    const err = container.querySelector("[data-auto-backup-range-error]");
    expect(err).not.toBeNull();
    const errText = err?.textContent ?? "";
    expect(errText).toContain("1–200");
    expect(errText).toContain("501");
    // 越界值一个字都不许发给后端。
    expect(invokeMock.mock.calls.some((c) => c[0] === "set_auto_backup_config")).toBe(false);
    expect(maxKeep.value).toBe("20");
  });

  it("折叠时不查询后端（不做无谓的磁盘统计）", async () => {
    await mountGroup(true);
    expect(invokeMock.mock.calls.some((c) => c[0] === "list_auto_backups")).toBe(false);
    expect(invokeMock.mock.calls.some((c) => c[0] === "get_auto_backup_config")).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// 备份列表悬浮窗
// ---------------------------------------------------------------------------

describe("备份列表悬浮窗", () => {
  it("列出全部自动备份：秒级时间、人类可读大小、来源、固定状态", async () => {
    await mountModal();

    const timed = rowByName(ENTRIES[0].archiveName);
    const startup = rowByName(ENTRIES[1].archiveName);

    // 时间精确到秒（后端直接给的 `YYYY-MM-DD HH:MM:SS`，界面不许截断成分钟）。
    expect(timed.textContent).toContain("2026-09-23 12:00:00");
    expect(startup.textContent).toContain("2026-09-23 09:00:00");

    // 大小复用既有的 `formatBytes`：数值 < 100 时保留一位小数，因此是
    // "2.0 KB" / "3.0 MB"（`formatBytes` 的行为，不是本组件自己定的格式）。
    expect(timed.textContent).toContain("2.0 KB");
    expect(startup.textContent).toContain("3.0 MB");

    // 来源必须逐行标出，用户才能理解"为什么有的备份不在预期时间点"。
    expect(timed.textContent).toContain("定时");
    expect(startup.textContent).toContain("启动");

    // 固定状态：只有已固定的那一行带徽标。
    expect(startup.querySelector("[data-backup-pinned]")).not.toBeNull();
    expect(timed.querySelector("[data-backup-pinned]")).toBeNull();
  });

  it("列表为空时不显示任何备份行，只显示空态", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "list_auto_backups")
        return listPayload({ entries: [], totalCount: 0, pinnedCount: 0 });
      return CONFIG;
    });
    await mountModal();
    expect(container.querySelectorAll("[data-backup-row]").length).toBe(0);
    expect(container.textContent).toContain("auto_backup_empty");
  });

  it("概览里写出真实份数与固定上限", async () => {
    await mountModal();
    expect(container.textContent).toContain("共 2 份 · 已固定 1（固定上限 19）");
  });
});

describe("备份列表右键菜单", () => {
  it("未固定的行给出「固定」，已固定的行给出「取消固定」", async () => {
    await mountModal();

    await rightClick(rowByName(ENTRIES[0].archiveName));
    expect(menu()).not.toBeNull();
    expect(menuItem("pin")?.textContent).toContain("固定");
    expect(menuItem("unpin")).toBeNull();
    // 删除与恢复也必须都在。
    expect(menuItem("delete")).not.toBeNull();
    expect(menuItem("restore")).not.toBeNull();

    // 关掉再右键另一行。
    await act(async () => {
      document.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
    });
    expect(menu()).toBeNull();

    await rightClick(rowByName(ENTRIES[1].archiveName));
    expect(menuItem("unpin")?.textContent).toContain("取消固定");
    expect(menuItem("pin")).toBeNull();
  });

  it("点「固定」把 archiveName 与该行 `pinned: true` 发给后端", async () => {
    await mountModal();
    await rightClick(rowByName(ENTRIES[0].archiveName));
    await click(menuItem("pin"));

    const call = invokeMock.mock.calls.find((c) => c[0] === "set_auto_backup_pinned");
    expect(call).toBeTruthy();
    // 参数名必须是 camelCase 的 `archiveName`：Tauri 会把 JS 的 camelCase 映射到
    // Rust 的 `archive_name`，写成 snake_case 会直接报缺参数。
    expect(call![1]).toEqual({ archiveName: ENTRIES[0].archiveName, pinned: true });
  });

  it("点「取消固定」立即执行，不弹二次确认（它不破坏数据）", async () => {
    await mountModal();
    await rightClick(rowByName(ENTRIES[1].archiveName));
    await click(menuItem("unpin"));

    expect(confirmBox("delete")).toBeNull();
    expect(confirmBox("restore")).toBeNull();
    const call = invokeMock.mock.calls.find((c) => c[0] === "set_auto_backup_pinned");
    expect(call![1]).toEqual({ archiveName: ENTRIES[1].archiveName, pinned: false });
  });

  it("「删除」必须先弹二次确认，确认前一个删除命令都不发", async () => {
    await mountModal();
    await rightClick(rowByName(ENTRIES[0].archiveName));
    await click(menuItem("delete"));

    const box = confirmBox("delete");
    expect(box).not.toBeNull();
    // 确认框里要写清删的是哪一份（时间/大小/来源），用户才能核对。
    const subject = box!.querySelector("[data-backup-confirm-subject]");
    expect(subject).not.toBeNull();
    expect(subject!.textContent).toContain("2026-09-23 12:00:00");
    expect(subject!.textContent).toContain("2.0 KB");
    expect(subject!.textContent).toContain("定时");
    // 确认文案本身仍在（说明后果：删了找不回）。
    expect(box!.textContent).toContain("无法通过本列表找回");
    expect(invokeMock.mock.calls.some((c) => c[0] === "delete_auto_backup")).toBe(false);
  });

  it("删除确认框点「取消」后什么也不发生，且不残留确认框", async () => {
    await mountModal();
    await rightClick(rowByName(ENTRIES[0].archiveName));
    await click(menuItem("delete"));

    const cancelBtn = Array.from(confirmBox("delete")!.querySelectorAll("button")).find(
      (b) => b.textContent === "取消"
    );
    await click(cancelBtn!);

    expect(confirmBox("delete")).toBeNull();
    expect(invokeMock.mock.calls.some((c) => c[0] === "delete_auto_backup")).toBe(false);
  });

  it("删除确认后才真正调用 delete_auto_backup", async () => {
    await mountModal();
    await rightClick(rowByName(ENTRIES[0].archiveName));
    await click(menuItem("delete"));
    await click(confirmBox("delete")!.querySelector("[data-backup-confirm-ok]"));

    const call = invokeMock.mock.calls.find((c) => c[0] === "delete_auto_backup");
    expect(call).toBeTruthy();
    expect(call![1]).toEqual({ archiveName: ENTRIES[0].archiveName });
  });

  it("「恢复」必须先弹二次确认，且写明会替换当前数据、会先做旁路备份", async () => {
    await mountModal();
    await rightClick(rowByName(ENTRIES[1].archiveName));
    await click(menuItem("restore"));

    const box = confirmBox("restore");
    expect(box).not.toBeNull();
    const subject = box!.querySelector("[data-backup-confirm-subject]");
    expect(subject).not.toBeNull();
    expect(subject!.textContent).toContain("2026-09-23 09:00:00");
    expect(subject!.textContent).toContain("3.0 MB");
    expect(subject!.textContent).toContain("启动");
    // 可回退性（会先建旁路备份）与后果（会替换当前数据、需要重启）都必须在这一屏里说清。
    expect(box!.textContent).toContain("旁路备份");
    expect(box!.textContent).toContain("替换当前的全部数据");
    expect(invokeMock.mock.calls.some((c) => c[0] === "restore_auto_backup")).toBe(false);
  });

  it("恢复确认后才真正调用 restore_auto_backup 并带上 archiveName", async () => {
    await mountModal();
    await rightClick(rowByName(ENTRIES[1].archiveName));
    await click(menuItem("restore"));
    await click(confirmBox("restore")!.querySelector("[data-backup-confirm-ok]"));

    const call = invokeMock.mock.calls.find((c) => c[0] === "restore_auto_backup");
    expect(call).toBeTruthy();
    expect(call![1]).toEqual({ archiveName: ENTRIES[1].archiveName });
  });
});

// ---------------------------------------------------------------------------
// 固定上限：用户要求"不静默失败，弹真实数字的提示"
// ---------------------------------------------------------------------------

describe("固定数上限", () => {
  /** 后端在达到上限时返回的结构化载荷（`AutoBackupError::PinnedLimitReached`）。 */
  const limitError = (maxKeep: number, currentPinned: number) =>
    new Error(
      JSON.stringify({
        code: "auto_backup_pinned_limit_reached",
        maxKeep,
        maxPinned: maxKeep - 1,
        currentPinned,
      })
    );

  it("被后端拒绝时，提示里出现的是配置里的真实数字（50 / 49），不是写死的常量", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "get_auto_backup_config") return { ...CONFIG, maxKeep: 50 };
      if (cmd === "list_auto_backups")
        return listPayload({ config: { ...CONFIG, maxKeep: 50 }, maxPinned: 49 });
      if (cmd === "set_auto_backup_pinned") throw limitError(50, 49);
      return CONFIG;
    });
    await mountModal();

    await rightClick(rowByName(ENTRIES[0].archiveName));
    await click(menuItem("pin"));

    const text = container.querySelector("[data-backup-list-error]")?.textContent ?? "";
    expect(text).toContain("最大留存备份数量为 50");
    expect(text).toContain("已固定 49 个");
    // 用户原话里的那句解释必须在，且不能被吞掉。
    expect(text).toContain("不能全部设置为固定");
  });

  it("换一组数字（200 / 199）提示跟着变——证明数字来自载荷而不是任何固定值", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "get_auto_backup_config") return { ...CONFIG, maxKeep: 200 };
      if (cmd === "list_auto_backups")
        return listPayload({ config: { ...CONFIG, maxKeep: 200 }, maxPinned: 199 });
      if (cmd === "set_auto_backup_pinned") throw limitError(200, 199);
      return CONFIG;
    });
    await mountModal();

    await rightClick(rowByName(ENTRIES[0].archiveName));
    await click(menuItem("pin"));

    const text = container.querySelector("[data-backup-list-error]")?.textContent ?? "";
    expect(text).toContain("最大留存备份数量为 200");
    expect(text).toContain("已固定 199 个");
    expect(text).not.toContain("50");
  });

  it("错误带着中文类别前缀时仍能认出原因码（否则三语映射会整体失效）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "get_auto_backup_config") return CONFIG;
      if (cmd === "list_auto_backups") return listPayload();
      if (cmd === "set_auto_backup_pinned")
        throw new Error(
          `验证错误: ${JSON.stringify({
            code: "auto_backup_pinned_limit_reached",
            maxKeep: 30,
            maxPinned: 29,
            currentPinned: 29,
          })}`
        );
      return CONFIG;
    });
    await mountModal();

    await rightClick(rowByName(ENTRIES[0].archiveName));
    await click(menuItem("pin"));

    const text = container.querySelector("[data-backup-list-error]")?.textContent ?? "";
    expect(text).toContain("最大留存备份数量为 30");
    expect(text).not.toContain("验证错误");
  });

  it("未知原因码退化为后端明细原文，不把内部键名甩给用户", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "get_auto_backup_config") return CONFIG;
      if (cmd === "list_auto_backups") return listPayload();
      if (cmd === "set_auto_backup_pinned")
        throw new Error(JSON.stringify({ code: "auto_backup_brand_new_code", detail: "磁盘满了" }));
      return CONFIG;
    });
    await mountModal();

    await rightClick(rowByName(ENTRIES[0].archiveName));
    await click(menuItem("pin"));

    const text = container.querySelector("[data-backup-list-error]")?.textContent ?? "";
    expect(text).toBe("磁盘满了");
    expect(text).not.toContain("auto_backup_err_");
  });
});

// ---------------------------------------------------------------------------
// 与后端的参数契约
// ---------------------------------------------------------------------------

describe("Tauri 命令参数契约", () => {
  it("「立即备份一次」用 camelCase 的 appVersion 调 run_auto_backup_now", async () => {
    await mountModal();
    const buttons = Array.from(container.querySelectorAll("button"));
    const runBtn = buttons.find((b) => b.textContent?.includes("auto_backup_run_now"));
    await click(runBtn!);

    const call = invokeMock.mock.calls.find((c) => c[0] === "run_auto_backup_now");
    expect(call).toBeTruthy();
    expect(call![1]).toHaveProperty("appVersion");
    // 版本查询失败也不能让按钮点不动：这里必须是字符串（允许空串）。
    expect(typeof (call![1] as { appVersion: unknown }).appVersion).toBe("string");
  });
});

// ---------------------------------------------------------------------------
// 无限刷新循环（用户原话："里面的刷新按钮一直在闪烁"）
// ---------------------------------------------------------------------------

/**
 * # 为什么必须单独锁住这一条
 *
 * 用户反馈的"刷新按钮一直在闪烁"不是动画问题，而是**每帧都在重新请求后端**。
 * 闭环是：
 *
 *   ① 父组件把 `onLoaded` 写成内联箭头函数 → 每次父渲染都是新引用
 *   ② `refresh = useCallback(…, [explain, onLoaded])` → onLoaded 变则 refresh 重建
 *   ③ `useEffect(() => { if (open) void refresh() }, [open, refresh])` → 重跑
 *   ④ refresh 里调 `onLoaded?.(data)` → 父组件 setState → 回到 ①
 *
 * 它有三个"看起来已经修好了"的假象，所以只靠截图或 DOM 断言抓不到：
 *   - `open` 没变，effect 的"打开时加载一次"意图看着仍然成立
 *   - `loading` 在 true/false 之间来回，图标切换像极了动画在跑
 *   - 单次渲染下 `invoke` 确实只被调一次（要"再渲染一次"才暴露）
 *
 * 因此这里的判据是**调用次数**：在"父组件每轮都重建 onLoaded"的条件下挂载并
 * 多轮 flush，`list_auto_backups` 必须恰好被调用 1 次，且此后不再增长。
 * 在旧代码（依赖数组里含 `onLoaded`）上这条会失败——那时每次 setPayload 都会
 * 触发下一轮，次数持续增长（实测 3 轮 flush 后达到 8 次以上，见报告）。
 */
describe("无限刷新循环", () => {
  /** 统计 `list_auto_backups` 被调用的次数。 */
  const listCalls = () => invokeMock.mock.calls.filter((c) => c[0] === "list_auto_backups").length;

  it("父组件在 onLoaded 里 setState 时，list_auto_backups 仍只调用 1 次（不进入刷新循环）", async () => {
    // 这条是缺陷 1 的主判据，用的是**完整闭环**的 harness：
    // 父组件持有 state，onLoaded 是内联函数且会 setState。
    summaryWrites = 0;
    await act(async () => {
      root.render(createElement(ParentHarness));
    });

    // 首轮：挂载 → open=true → refresh → invoke → onLoaded → 父 setState → 父渲染。
    // 到这里为止 1 次 invoke 是**正确**行为（本来就应该加载一次）。
    expect(listCalls()).toBe(1);

    // 关键：不再手动重渲染。若仍有循环，它会靠"父 setState → 新 onLoaded →
    // refresh 重建 → effect 重跑"自行继续。多轮 flush 给它充分机会。
    await flush();
    await flush();
    await flush();

    // 修好后：次数恒为 1，且父组件只被写入了 1 次。
    // 修复前（refresh 依赖里含 onLoaded）：每轮 setState 都会掀起下一轮，
    // 次数随 flush 次数增长（实测 3 轮后达到 6–8 次）。
    expect(listCalls()).toBe(1);
    expect(summaryWrites).toBe(1);
  });

  it("父组件每轮都新建 onLoaded 时，list_auto_backups 仍只调用 1 次（不进入刷新循环）", async () => {
    reRenders = 0;
    await mountModalWithUnstableOnLoaded();

    // 这一条是上面那条的"弱化版"：只给不稳定的 onLoaded，父组件不做 setState。
    // 它单独不足以抓住循环（缺闭环最后一环），但能守住"refresh 的身份不随
    // onLoaded 变化"这一半，且能在 `refresh` 被别处高频调用时报警。
    expect(listCalls()).toBe(1);
    await flush();
    await flush();
    expect(listCalls()).toBe(1);
  });

  it("onLoaded 拿到的始终是最新那个回调（换掉回调后能读到新值，不是过期闭包）", async () => {
    // 这条与上一条互补：上一条防"循环"，这一条防"为了断循环而写死 ref 初值"
    // （`useRef(onLoaded)` 那种写法会让回调永远停在第一版上）。
    const seen: number[] = [];
    const render = (mark: number) =>
      root.render(
        createElement(BackupListModal, {
          open: true,
          t,
          theme: "light",
          onClose: () => {},
          onLoaded: (p: AutoBackupListPayload) => {
            seen.push(mark);
            // 顺带证明载荷是真的传下来了，回调不是被空调用。
            expect(p.totalCount).toBe(ENTRIES.length);
          },
        })
      );

    await act(async () => {
      render(1);
    });
    await flush();
    expect(seen).toEqual([1]);

    // 换一个新回调（新引用）并**触发一次真正的刷新**（点刷新按钮）。
    // 若 refresh 闭包里捕获的是旧回调，这里推入的会是 1 而不是 2。
    await act(async () => {
      render(2);
    });
    await flush();

    const refreshBtn = container.querySelector<HTMLButtonElement>("[data-backup-refresh]");
    expect(refreshBtn).not.toBeNull();
    await click(refreshBtn);

    expect(seen).toContain(2);
    // 且不该因为一次点击又跑出循环（总共 1 次挂载 + 1 次点击 = 2 次）。
    expect(listCalls()).toBe(2);
  });

  it("刷新中显示**带动画的** Loader2（补 animate-spin 的那一处）", async () => {
    // 用户看到的"闪"还有一个次要成因：`<Loader2 size={12} />` 缺 `animate-spin`
    // （该动画定义在 `ai.css`），于是"刷新中"只表现为两个图标来回换，而不是一个
    // 转动的 loader。这里用**挂起的 Promise** 把 loading 态真正冻住再断言，
    // 否则 mock 立刻 resolve，loading 一闪而过，断言会恒真。
    //
    // `hold` 在挂载完成后才打开，因此挂载那次（第 1 次调用）立即返回，
    // 组件处于静止态；打开后下一次调用会卡在 `gate` 上，把 loading 真正冻住。
    let hold = false;
    let release: (() => void) | null = null;
    const gate = new Promise<void>((r) => {
      release = () => r();
    });
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "list_auto_backups") {
        if (hold) await gate;
        return listPayload();
      }
      if (cmd === "get_auto_backup_config") return CONFIG;
      return undefined;
    });

    await mountModal();
    const refreshBtn = container.querySelector<HTMLButtonElement>("[data-backup-refresh]")!;
    // 静止态（挂载那次已 resolve）：RefreshCw，不带动画类。
    // 这条对照很重要——它证明"能查到 animate-spin"不是因为图标一直挂着。
    expect(refreshBtn.querySelector("svg.animate-spin")).toBeNull();

    // 点击刷新：必须同步进入 loading 态，并渲染**带 animate-spin** 的 Loader2。
    hold = true;
    await act(async () => {
      refreshBtn.click();
    });
    expect(refreshBtn.querySelector("svg.animate-spin")).not.toBeNull();

    await act(async () => {
      release?.();
    });
    await flush();
    // 收尾：放行后回到静止态。
    expect(refreshBtn.querySelector("svg.animate-spin")).toBeNull();
  });
});
