// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import DataSettingsGroup from "./groups/DataSettingsGroup";

/**
 * 「迁移中心」的进度显示、`deferred` 结果与重复点击防护（真实挂载 + 模拟事件序列）。
 *
 * # 为什么必须真实挂载并真的推事件
 *
 * 要断言的每一件事都发生在**异步事件落地之后**：
 *   - 进度条显示的是不是后端给的 `stageLabel`（而不是前端自己翻的文案）；
 *   - `total === 0` 时显示的是"不确定进度"还是被算成了 `0%` / `NaN%`；
 *   - 进行中时按钮是不是真的 `disabled`。
 *
 * 静态渲染时这三样全都处于初始态：进度条根本不渲染、按钮也没被禁用，
 * 在那上面下断言会**恒真** —— 那种通过的测试什么都证明不了。因此这里 mock 掉
 * Tauri 宿主 API，把 `listen` 注册的处理器抓在手里，真的按契约时序推事件。
 *
 * # 桩文案的形状必须与 `src/locales.ts` 一致
 *
 * `t` 桩只给本文件要断言的键。占位符写错（例如给没有占位符的词条编一个 `{x}`）
 * 会让"数字被真实替换进提示"这类断言靠着桩自己编的东西变绿。
 * 真实的键存在性与三语占位符一致性由 `migrationLocaleContract.test.ts` 单独把守。
 */

const { invokeMock, listeners, unlistenSpy } = vi.hoisted(() => ({
    invokeMock: vi.fn(),
    listeners: [] as Array<{ event: string; handler: (e: { payload: unknown }) => void }>,
    unlistenSpy: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));

vi.mock("@tauri-apps/api/event", () => ({
    listen: async (event: string, handler: (e: { payload: unknown }) => void) => {
        listeners.push({ event, handler });
        return () => {
            unlistenSpy(event);
            const i = listeners.findIndex((l) => l.event === event && l.handler === handler);
            if (i >= 0) listeners.splice(i, 1);
        };
    },
}));

vi.mock("@tauri-apps/plugin-dialog", () => ({
    open: vi.fn(async () => null),
    ask: vi.fn(async () => true),
    message: vi.fn(async () => undefined),
    confirm: vi.fn(async () => true),
}));

/** 三语词条的真实形状（只取本组要断言的键）。 */
const T: Record<string, string> = {
    data_management: "数据管理",
    data_path: "数据目录",
    change_app: "更改数据存储位置",
    change_data_path: "更改数据存储位置",
    open_folder: "打开目录",
    data_move_confirm: "确认把数据目录改为 {path}？",
    data_move_success: "已更改",
    data_move_failed: "更改失败：{e}",
    backup_section: "备份与恢复",
    backup_intro: "导出或导入",
    backup_export: "导出备份",
    backup_export_running: "导出中…",
    backup_import: "导入备份",
    backup_import_running: "导入中…",
    backup_export_hint: "导出提示",
    backup_import_hint: "导入提示",
    backup_datapath_note: "数据目录说明",
    backup_preflight: "共 {files} 个文件 / {size}",
    backup_reveal: "打开所在目录",
    migration_center: "迁移中心",
    legacy_dir_intro: "旧数据目录介绍",
    legacy_dir_total: "共 {size}",
    legacy_dir_none: "没有发现旧目录",
    legacy_dir_usage: "{size} / {files} 个文件",
    legacy_dir_has_db: "含数据库",
    legacy_dir_backup_note: "备份说明",
    legacy_dir_delete_hint: "删除这条旧数据",
    legacy_dir_delete_confirm: "删除 {path}（{size}）？",
    legacy_dir_delete_title: "删除旧数据目录",
    legacy_dir_delete_ok: "备份并删除",
    legacy_dir_delete_done: "已删除",
    legacy_dir_delete_done_with_backup: "已删除，备份在 {backup}",
    legacy_dir_delete_failed: "删除失败：{e}",
    legacy_origin_legacy_tiez: "旧版 TieZ",
    legacy_origin_previous_tiez_next: "历史版本 Tiez-Next",
    legacy_migrate: "从此目录迁移",
    legacy_migrate_hint: "只读源目录",
    legacy_migrate_choose: "选择其它目录...",
    legacy_migrate_choose_title: "选择旧版数据目录",
    legacy_migrate_open_target: "打开新版数据目录",
    legacy_migrate_title: "从旧数据目录迁移",
    legacy_migrate_confirm: "即将从 {label}（{path}）复制数据。",
    legacy_migrate_ok: "开始迁移",
    legacy_migrate_done: "迁移完成：已复制 {files} 项、共 {size}。",
    legacy_migrate_source_safe: "源目录未被改动。",
    legacy_migrate_restart: "迁移后需要重启应用。",
    legacy_migrate_restart_now: "立即重启应用",
    legacy_migrate_skipped: "本次未迁移任何数据。",
    legacy_migrate_failed: "迁移未完成：{e}",
    legacy_migrate_failed_hint: "若文件被占用，请重启应用后重试。",
    legacy_migrate_result_migrated: "上次迁移结果：成功",
    legacy_migrate_result_skipped: "上次迁移结果：已跳过",
    legacy_migrate_result_failed: "上次迁移结果：未完成",
    legacy_migrate_result_files: "本次复制 {files} 个文件 / {size}",
    legacy_migrate_result_kept: "另有 {files} 个文件保留未覆盖。",
    legacy_migrate_result_already_present: "源里的数据已全部存在。",
    legacy_migrate_result_source: "源目录：{path}",
    legacy_migrate_result_target: "新版数据目录：{path}",
    legacy_migrate_paths_rewritten: "路径已更新。",
    legacy_migrate_paths_not_rewritten: "路径未改写。",
    legacy_migrate_rewrite_warning: "路径改写失败：{e}",
    legacy_migrate_superseded: "空库已留档：{path}",
    legacy_migrate_notice_target_already_has_data: "目标已有数据。",
    legacy_migrate_notice_no_legacy_dir: "没有找到旧目录。",
    legacy_migrate_notice_same_path: "就是当前目录。",
    legacy_migrate_notice_not_a_directory: "不是目录。",
    legacy_migrate_notice_source_missing: "路径不存在。",
    legacy_migrate_notice_empty_source: "目录是空的。",
    legacy_migrate_notice_not_a_data_directory: "找不到 clipboard.db。",
    legacy_migrate_notice_source_is_ancestor_of_target: "是上级目录。",
    legacy_migrate_notice_source_inside_target: "在目标内部。",
    // 本轮新增（三语草稿见报告；形状必须与 src/locales.ts 一致）
    migration_deferred_title: "源数据已复制就绪，重启后自动完成接管",
    migration_deferred_hint: "源数据已复制就绪。重启应用后自动完成接管，无需其他操作。",
    migration_progress_indeterminate: "不确定进度",
    migration_progress_items: "已处理 {progress} 项",
    migration_progress_bytes: "已复制 {progress}",
    notice: "提示",
    error: "错误",
    cancel: "取消",
    confirm: "确认",
};
const t = (key: string) => T[key] ?? key;

const LEGACY_DIRS = [
    {
        path: "/home/u/.local/share/com.tiez",
        identifier: "com.tiez",
        origin: "legacy_tiez",
        bytes: 921600,
        files: 7,
        has_database: true,
        canDelete: true,
    },
];

let container: HTMLDivElement;
let root: Root;

const flush = async () => {
    await act(async () => {
        await Promise.resolve();
        await Promise.resolve();
        await Promise.resolve();
    });
};

/** 按事件名找到 `listen` 注册的处理器，真的把一帧推给组件。 */
const emit = async (event: string, payload: unknown) => {
    const targets = listeners.filter((l) => l.event === event);
    await act(async () => {
        targets.forEach((l) => l.handler({ payload }));
    });
};

const progress = (over: Partial<Record<string, unknown>> = {}) => ({
    stage: "copying",
    stageLabel: "正在复制数据",
    done: 0,
    total: 0,
    bytes: 0,
    bytesTotal: 0,
    message: null,
    ...over,
});

const mount = async (collapsed = false) => {
    await act(async () => {
        root.render(
            createElement(DataSettingsGroup, {
                t,
                collapsed,
                onToggle: () => {},
                dataPath: "/home/u/.local/share/com.tiez.next",
            })
        );
    });
    await flush();
};

/** 找到「从此目录迁移」按钮 —— 进行中禁用状态要断言在它身上。 */
const migrateButton = (): HTMLButtonElement => {
    const btn = Array.from(container.querySelectorAll("button")).find((b) =>
        (b.textContent ?? "").includes("从此目录迁移")
    );
    if (!btn) throw new Error("找不到「从此目录迁移」按钮");
    return btn as HTMLButtonElement;
};

beforeEach(() => {
    invokeMock.mockReset();
    listeners.length = 0;
    unlistenSpy.mockClear();
    invokeMock.mockImplementation(async (cmd: string) => {
        if (cmd === "list_legacy_data_dirs") return LEGACY_DIRS;
        if (cmd === "backup_preflight")
            return { dataDir: "/d", managedBytes: 1, managedFiles: 1, backgroundOutside: false, backgroundPath: null };
        if (cmd === "plugin:app|version") return "0.5.3";
        // 快照命令：契约缺口，后端可能尚未实现 —— 默认按"未实现"返回 undefined。
        return undefined;
    });
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
});

afterEach(() => {
    act(() => root.unmount());
    container.remove();
});

describe("迁移进度显示（用户核心诉求）", () => {
    it("按事件序列显示后端给的 stageLabel 与百分比，不自己翻译 stage", async () => {
        await mount();
        expect(container.querySelector(".migration-progress")).toBeNull();

        await emit("migration-progress", progress({ stage: "precheck", stageLabel: "正在检查源目录", done: 0, total: 512 }));
        // 后端说的是「正在检查源目录」，界面必须原样显示 —— 若前端按 stage 自己映射
        // （precheck → 某个自家文案），这条就会红。
        expect(container.querySelector(".migration-progress-label")?.textContent).toBe("正在检查源目录");
        expect(container.querySelector(".migration-progress-value")?.textContent).toBe("0%");

        await emit(
            "migration-progress",
            progress({ done: 128, total: 512, bytes: 12 * 1024 * 1024, bytesTotal: 48 * 1024 * 1024 })
        );
        expect(container.querySelector(".migration-progress-label")?.textContent).toBe("正在复制数据");
        expect(container.querySelector(".migration-progress-value")?.textContent).toBe("25%");
        expect(container.querySelector(".migration-progress-bar")?.getAttribute("style")).toContain("25%");

        // 字节与条目都要人话显示：只报一个数用户判断不了还剩多少。
        const meta = container.querySelector(".migration-progress-meta")?.textContent ?? "";
        expect(meta).toContain("12.0 MB / 48.0 MB");
        expect(meta).toContain("128 / 512");

        await emit("migration-progress", progress({ stage: "verifying", stageLabel: "正在校验完整性", done: 3, total: 4 }));
        expect(container.querySelector(".migration-progress-label")?.textContent).toBe("正在校验完整性");
        expect(container.querySelector(".migration-progress-value")?.textContent).toBe("75%");
    });

    it("进度条带 role=progressbar 且有可访问的数值（读屏也要能读出进度）", async () => {
        await mount();
        await emit("migration-progress", progress({ done: 1, total: 4 }));
        const bar = container.querySelector('[role="progressbar"]');
        expect(bar).not.toBeNull();
        expect(bar?.getAttribute("aria-valuenow")).toBe("25");
        expect(bar?.getAttribute("aria-valuemax")).toBe("100");
    });

    it("migration-done 事件也参与显示（它不是只在结束时清状态）", async () => {
        await mount();
        await emit("migration-done", progress({ stage: "done", stageLabel: "迁移完成", total: 0, done: 0 }));
        // 终态不再占用"进行中"的位置：结果卡片紧接着给出结论，两处同时显示会混淆。
        expect(container.querySelector(".migration-progress")).toBeNull();
        // 但这次迁移已经结束，按钮必须恢复可点。
        expect(migrateButton().disabled).toBe(false);
    });
});

describe("total === 0：不确定进度，不是 0% 也不是 NaN", () => {
    it("显示「不确定进度」文案，且页面上不出现 0% / NaN%", async () => {
        await mount();
        // `deferred` 阶段按契约是「不可计量」的那一档 —— 这正是用户会真实看到
        // "等重启接管"的那一刻，界面必须在此显示"不确定进度"，而不是 0%。
        await emit(
            "migration-progress",
            progress({ stage: "deferred", stageLabel: "数据已就绪，等待重启接管", done: 0, total: 0 })
        );

        const value = container.querySelector(".migration-progress-value")?.textContent;
        expect(value).toBe("不确定进度");
        expect(value).not.toBe("0%");
        expect(value).not.toContain("NaN");

        // 整块区域都不应出现 0% 或 NaN —— 只看那一个 span 会漏掉"另一个地方也在算"。
        const block = container.querySelector(".migration-progress")?.textContent ?? "";
        expect(block).not.toContain("0%");
        expect(block).not.toContain("NaN");
        expect(block).not.toContain("0 / 0");
    });

    it("deferred 阶段仍显示后端的 stageLabel（等重启时用户要知道在等什么）", async () => {
        await mount();
        await emit(
            "migration-progress",
            progress({ stage: "deferred", stageLabel: "数据已就绪，等待重启接管", total: 0 })
        );
        expect(container.querySelector(".migration-progress-label")?.textContent).toBe(
            "数据已就绪，等待重启接管"
        );
    });

    it("不设 aria-valuenow（传 0 会让读屏念出 0%，正是要避免的那个误导）", async () => {
        await mount();
        await emit("migration-progress", progress({ stage: "deferred", stageLabel: "等待重启接管", total: 0, done: 0 }));
        const bar = container.querySelector('[role="progressbar"]');
        expect(bar?.getAttribute("aria-valuenow")).toBeNull();
    });

    it("不可计量时进度条走 indeterminate 类，而不是一条 width:0% 的静止条", async () => {
        await mount();
        await emit("migration-progress", progress({ stage: "deferred", stageLabel: "等待重启接管", total: 0, done: 0 }));
        const bar = container.querySelector(".migration-progress-bar");
        expect(bar?.className).toContain("indeterminate");
        // 关键：不能给一个 0% 的宽度。静止的 0% 与"卡死"无法区分。
        expect(bar?.getAttribute("style")).toBeNull();
    });

    it("failed 是终态且不可计量：收起进度块，由结果卡片给结论", async () => {
        await mount();
        await emit("migration-progress", progress({ stage: "failed", stageLabel: "迁移失败", message: "磁盘空间不足" }));
        expect(container.querySelector(".migration-progress")).toBeNull();
    });
});

describe("进行中禁用重复触发", () => {
    it("收到非终态 progress 后迁移按钮 disabled", async () => {
        await mount();
        expect(migrateButton().disabled).toBe(false);

        await emit("migration-progress", progress({ stage: "copying", done: 1, total: 10 }));
        expect(migrateButton().disabled).toBe(true);

        // 「选择其它目录…」也走同一个迁移命令，必须一起锁上，
        // 否则用户能从另一条路径发起第二次迁移。
        const chooseBtn = Array.from(container.querySelectorAll("button")).find((b) =>
            (b.textContent ?? "").includes("选择其它目录")
        ) as HTMLButtonElement;
        expect(chooseBtn.disabled).toBe(true);
    });

    it("收到终态（done / failed / deferred）后解除禁用", async () => {
        await mount();
        for (const [stage, label] of [
            ["done", "迁移完成"],
            ["failed", "迁移失败"],
            ["deferred", "数据已就绪，等待重启接管"],
        ] as const) {
            await emit("migration-progress", progress({ stage, stageLabel: label, done: 1, total: 1 }));
            expect(migrateButton().disabled).toBe(false);
            await emit("migration-progress", progress({ stage: "copying", done: 1, total: 10 }));
            expect(migrateButton().disabled).toBe(true);
        }
    });

    it("migration-done 事件同样解除禁用（事件通道是唯一可信来源）", async () => {
        await mount();
        await emit("migration-progress", progress({ stage: "copying", done: 1, total: 10 }));
        expect(migrateButton().disabled).toBe(true);
        await emit("migration-done", progress({ stage: "done", stageLabel: "迁移完成", done: 10, total: 10 }));
        expect(migrateButton().disabled).toBe(false);
    });

    it("终态帧用 deferred 命名阶段时也解除 —— deferred 是终态不是失败", async () => {
        await mount();
        await emit("migration-progress", progress({ stage: "copying", done: 1, total: 10 }));
        await emit("migration-done", progress({ stage: "deferred", stageLabel: "数据已就绪，等待重启接管" }));
        expect(migrateButton().disabled).toBe(false);
    });
});

describe("事件订阅的建立与清理", () => {
    it("同时订阅两个事件", async () => {
        await mount();
        const names = listeners.map((l) => l.event).sort();
        expect(names).toEqual(["migration-done", "migration-progress"]);
    });

    it("listen 之后才 invoke 拉初值（先监听后拉快照，避免丢帧）", async () => {
        await mount();
        const calls = invokeMock.mock.calls.map((c) => c[0]);
        const snapshotIdx = calls.indexOf("get_migration_progress");
        expect(snapshotIdx).toBeGreaterThanOrEqual(0);
        // 挂载完成时监听已经建立 —— 若实现是先 invoke 再 listen，
        // 中间那段事件就永远收不到（进度条会卡在半路）。
        expect(listeners.length).toBe(2);
    });

    it("快照返回一帧时界面显示它（补上事件先于监听时丢掉的那一次）", async () => {
        invokeMock.mockImplementation(async (cmd: string) => {
            if (cmd === "list_legacy_data_dirs") return LEGACY_DIRS;
            if (cmd === "backup_preflight")
                return { dataDir: "/d", managedBytes: 1, managedFiles: 1, backgroundOutside: false, backgroundPath: null };
            if (cmd === "get_migration_progress")
                return progress({ done: 5, total: 20, stageLabel: "正在复制数据" });
            return undefined;
        });
        await mount();
        expect(container.querySelector(".migration-progress-value")?.textContent).toBe("25%");
    });

    it("快照命令不存在（契约缺口/后端未实现）时不崩溃、不伪造进度", async () => {
        invokeMock.mockImplementation(async (cmd: string) => {
            if (cmd === "list_legacy_data_dirs") return LEGACY_DIRS;
            if (cmd === "backup_preflight")
                return { dataDir: "/d", managedBytes: 1, managedFiles: 1, backgroundOutside: false, backgroundPath: null };
            if (cmd === "get_migration_progress") throw new Error("Command get_migration_progress not found");
            return undefined;
        });
        await mount();
        expect(container.querySelector(".migration-progress")).toBeNull();
        // 设置面板的其他部分照常渲染 —— 一个未实现的进度命令不该打崩整页。
        expect(container.textContent).toContain("迁移中心");
        expect(migrateButton().disabled).toBe(false);
    });

    it("卸载后逐个 unlisten —— 两个事件都被清理", async () => {
        await mount();
        expect(unlistenSpy).not.toHaveBeenCalled();

        act(() => root.unmount());
        await flush();

        expect(unlistenSpy).toHaveBeenCalledTimes(2);
        const cleaned = unlistenSpy.mock.calls.map((c) => c[0]).sort();
        expect(cleaned).toEqual(["migration-done", "migration-progress"]);
        // 监听器表必须真的空了：`listen()` 返回 Promise，忘了 `.then(f => f())`
        // 就等于没清理，设置面板反复开合会让监听器不断累积。
        expect(listeners.length).toBe(0);
        root = createRoot(container);
    });

    it("卸载后再推事件不会 setState（alive 守卫）", async () => {
        await mount();
        const stale = [...listeners];
        act(() => root.unmount());
        await flush();
        // 直接调用已摘除的处理器：不应抛错，也不应产生任何 warning 级别的副作用。
        await act(async () => {
            stale.forEach((l) => l.handler({ payload: progress({ done: 1, total: 2 }) }));
        });
        root = createRoot(container);
    });
});

/**
 * `deferred` 结果：**成功但需重启**，不是失败。
 *
 * # 这一组要钉住的到底是什么
 *
 * 真机上迁移在 Windows 必然无法在做运行时时完成（应用自己占着目标数据库）。
 * 后端改成两阶段后，`deferred` 就成为**常态路径**。它一旦被做成红色警告，
 * 每一次正常迁移都会长得像出了问题，用户会去"处理"一个本来不需要处理的提示，
 * 甚至以为数据没迁成功而重做。
 *
 * 因此这里断言的不是"有这段文案"，而是**配色归属**：
 * 结果卡片的 class 必须是成功的那一支，且**不得**出现错误态 class 或危险色。
 * 颜色本身还有第二道防线（`migrationResultStyle.test.ts` 读样式文件断言真实边框色）——
 * 类名对不代表颜色对，只断言类名的话，一条 `border-color` 写成危险色也照样全绿。
 */
describe("deferred 结果按成功呈现", () => {
    const deferredReport = {
        status: "deferred",
        source: "/old/data",
        target: "/new/data",
        files: 12,
        bytes: 1024,
        deliveredFiles: 12,
        deliveredBytes: 9200000,
        keptExisting: 0,
        skipReason: null,
        error: null,
        pathsRewritten: false,
        rewriteError: null,
        sourceUntouched: true,
        restartRequired: false,
        supersededDb: null,
        pendingUntilRestart: true,
    };

    const clickMigrate = async () => {
        await act(async () => {
            migrateButton().dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        await flush();
    };

    const withMigrationReturning = async (report: unknown) => {
        invokeMock.mockImplementation(async (cmd: string) => {
            if (cmd === "list_legacy_data_dirs") return LEGACY_DIRS;
            if (cmd === "backup_preflight")
                return { dataDir: "/d", managedBytes: 1, managedFiles: 1, backgroundOutside: false, backgroundPath: null };
            if (cmd === "migrate_from_data_dir") return report;
            return undefined;
        });
        await mount();
        await clickMigrate();
    };

    it("显示「重启后自动完成接管」文案", async () => {
        await withMigrationReturning(deferredReport);
        const card = container.querySelector(".migration-result");
        expect(card).not.toBeNull();
        expect(card?.textContent).toContain("重启后自动完成接管");
        expect(card?.textContent).toContain("无需其他操作");
    });

    it("用成功样式，不用错误样式（不出现 --danger-color / 错误态 class）", async () => {
        await withMigrationReturning(deferredReport);
        const card = container.querySelector(".migration-result") as HTMLElement;

        expect(card.className).toContain("is-deferred");
        // 与 done 同属成功支：两阶段迁移下 deferred 是常态，做成黄色警告
        // 会让每次正常迁移都长得像出了问题。
        expect(card.className).not.toContain("is-failed");
        expect(card.className).not.toContain("danger");
        expect(card.className).not.toContain("error");
        expect(card.className).not.toContain("warning");
        // 内联样式里也不能塞危险色（样式表之外的第二种写法同样会染色）
        expect(card.getAttribute("style") ?? "").not.toContain("--danger-color");
        // 整棵子树都不该带上危险色 —— 样式可能被写在某个子元素上。
        const html = card.outerHTML;
        expect(html).not.toContain("--danger-color");
    });

    it("deferred 分支不套用未完成、失败这两类标题", async () => {
        await withMigrationReturning(deferredReport);
        const card = container.querySelector(".migration-result") as HTMLElement;
        expect(card.textContent).not.toContain("未完成");
        expect(card.textContent).not.toContain("迁移未完成");
    });

    it("提供重启按钮，且复用的是既有 relaunch 命令（不新增后端命令）", async () => {
        await withMigrationReturning(deferredReport);
        const btn = Array.from(container.querySelectorAll("button")).find((b) =>
            (b.textContent ?? "").includes("立即重启应用")
        ) as HTMLButtonElement;
        expect(btn).toBeTruthy();

        invokeMock.mockClear();
        await act(async () => {
            btn.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        await flush();
        expect(invokeMock.mock.calls.map((c) => c[0])).toContain("relaunch");
    });

    it("不显示「路径未改写」这类中间态说明（接管还没发生，写出来像漏了一步）", async () => {
        await withMigrationReturning(deferredReport);
        const card = container.querySelector(".migration-result") as HTMLElement;
        expect(card.textContent).not.toContain("路径未改写");
    });

    it("只有 pendingUntilRestart 而没有 deferred 状态时，同样按待接管呈现", async () => {
        // 契约要求 status 与 pendingUntilRestart 两个字段都给；任一字段先落地时，
        // 界面都不能把一次**成功**的迁移显示成「未完成」。
        await withMigrationReturning({ ...deferredReport, status: "migrated" });
        const card = container.querySelector(".migration-result") as HTMLElement;
        expect(card.className).toContain("is-deferred");
        expect(card.textContent).toContain("重启后自动完成接管");
    });

    it("普通 done 结果仍走既有成功文案，不被 deferred 分支吞掉", async () => {
        await withMigrationReturning({
            ...deferredReport,
            status: "done",
            pendingUntilRestart: false,
            pathsRewritten: true,
        });
        const card = container.querySelector(".migration-result") as HTMLElement;
        expect(card.className).toContain("is-done");
        expect(card.textContent).toContain("上次迁移结果：成功");
        expect(card.textContent).toContain("路径已更新");
    });

    it("真正的 failed 仍然是错误样式（不能因为放宽 deferred 就把失败也洗白）", async () => {
        await withMigrationReturning({
            ...deferredReport,
            status: "failed",
            pendingUntilRestart: false,
            error: "文件被占用",
        });
        const card = container.querySelector(".migration-result") as HTMLElement;
        expect(card.className).toContain("is-failed");
        expect(card.textContent).toContain("文件被占用");
    });

    it("迁移命令返回后解除禁用（invoke 返回也是解除条件之一）", async () => {
        await withMigrationReturning(deferredReport);
        expect(migrateButton().disabled).toBe(false);
    });

    it("命令返回 deferred 后若再来一帧非终态进度，按钮重新禁用", async () => {
        await withMigrationReturning(deferredReport);
        expect(migrateButton().disabled).toBe(false);
        await emit("migration-progress", progress({ stage: "copying", done: 1, total: 10 }));
        expect(migrateButton().disabled).toBe(true);
    });
});
