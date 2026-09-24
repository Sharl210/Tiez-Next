// @vitest-environment node
import { describe, it, expect } from "vitest";
import {
    buildProgressView,
    isTerminalStage,
    normalizeProgressPayload,
    EVENT_MIGRATION_DONE,
    EVENT_MIGRATION_PROGRESS,
    TERMINAL_MIGRATION_STAGES,
} from "./migrationProgress";

/**
 * 迁移进度换算的纯逻辑测试。
 *
 * # 为什么这些案例必须是纯函数测试
 *
 * 这一组的核心缺陷是「`total === 0` 时算出 `NaN`」。`NaN` 在浏览器里**既不抛异常、
 * 也不进控制台**，只是画出一根宽度为 `NaN` 的条（等价于 0 宽）。所以：
 *
 * - 靠"渲染后截图看看有没有进度条"发现不了它（截图里就是一根空轨道，看起来像还没开始）；
 * - 靠 `expect(percent).toBeGreaterThan(0)` 之类的断言也发现不了，
 *   因为 `NaN > 0` 是 `false`，会让测试以"进度没涨"这种错误理由失败，
 *   掩盖真正的根因是 `total` 缺失而不是拷贝没动。
 *
 * 因此这里对**返回值本身**下断言：不可计量时必须是 `null`，且**不能是任何数字**。
 */

describe("事件常量与契约一致", () => {
    it("事件名逐字等于冻结契约（kebab-case，前后端拼接的唯一依据）", () => {
        expect(EVENT_MIGRATION_PROGRESS).toBe("migration-progress");
        expect(EVENT_MIGRATION_DONE).toBe("migration-done");
    });

    it("终态集合含 deferred/done/failed —— deferred 是终态但不是失败", () => {
        expect([...TERMINAL_MIGRATION_STAGES].sort()).toEqual(["deferred", "done", "failed"]);
        expect(isTerminalStage("deferred")).toBe(true);
        expect(isTerminalStage("done")).toBe(true);
        expect(isTerminalStage("failed")).toBe(true);
        // 过程中阶段绝不能被当成终态，否则按钮会在拷贝到一半时提前恢复可点。
        expect(isTerminalStage("precheck")).toBe(false);
        expect(isTerminalStage("copying")).toBe(false);
        expect(isTerminalStage("verifying")).toBe(false);
    });
});

describe("normalizeProgressPayload 容错", () => {
    it("逐字对齐后端 MigrateProgressPayload 的真实 JSON（camelCase）", () => {
        // 后端 `system_cmd.rs` 的 `MigrateProgressPayload` 带
        // `#[serde(rename_all = "camelCase")]`，因此线上到达前端的键就是下面这些。
        // 本案例用**后端真实产出的形状**（含真实量级字节数）过一次，避免"我按我理解的
        // 字段名写、后端按另一套发"这种两边都自测通过却在真机上不显示的情况。
        const p = normalizeProgressPayload({
            stage: "copying",
            stageLabel: "正在复制数据",
            done: 128,
            total: 512,
            bytes: 13002342,
            bytesTotal: 50436504,
            message: null,
        });
        expect(p).not.toBeNull();
        expect(p?.stageLabel).toBe("正在复制数据");
        expect(p?.bytesTotal).toBe(50436504);
        const v = buildProgressView(p!);
        expect(v.percent).toBe(25);
        expect(v.bytesText).toBe("12.4 MB / 48.1 MB");
    });

    it("接受契约里的 camelCase 拼写", () => {
        const p = normalizeProgressPayload({
            stage: "copying",
            stageLabel: "正在复制数据",
            done: 3,
            total: 9,
            bytes: 1024,
            bytesTotal: 4096,
            message: null,
        });
        expect(p).not.toBeNull();
        expect(p?.stageLabel).toBe("正在复制数据");
        expect(p?.bytesTotal).toBe(4096);
    });

    it("也接受 Rust 默认的 snake_case —— 漏 rename_all 不会让阶段名变成 undefined", () => {
        // Rust 结构体字段默认是 snake_case。若后端漏了
        // `#[serde(rename_all = "camelCase")]`，**编译不会报错**，只是
        // `stageLabel` 变成 undefined，于是用户看到一根没有说明文字的进度条。
        // 两种拼写都认，能让前端在拼写尚未对齐时仍然可用。
        const p = normalizeProgressPayload({ stage: "copying", stage_label: "正在复制数据" });
        expect(p?.stageLabel).toBe("正在复制数据");
    });

    it("payload 不是对象时返回 null（不产出半截进度）", () => {
        expect(normalizeProgressPayload(null)).toBeNull();
        expect(normalizeProgressPayload(undefined)).toBeNull();
        expect(normalizeProgressPayload("copying")).toBeNull();
        expect(normalizeProgressPayload(42)).toBeNull();
    });

    it("数字字段缺失或非有限值时按 0 处理，不把 NaN 透传下去", () => {
        const p = normalizeProgressPayload({ stage: "copying", stageLabel: "x", total: NaN });
        expect(p?.total).toBe(0);
        expect(Number.isFinite(p?.total)).toBe(true);
    });

    it("空 message 归一成 null（避免渲染出一个空的说明行）", () => {
        expect(normalizeProgressPayload({ stage: "done", message: "" })?.message).toBeNull();
        expect(normalizeProgressPayload({ stage: "done", message: "磁盘写入失败" })?.message).toBe(
            "磁盘写入失败"
        );
    });
});

describe("buildProgressView —— 可计量阶段", () => {
    it("按 done/total 算出百分比并保留 stageLabel 原文", () => {
        const v = buildProgressView({
            stage: "copying",
            stageLabel: "正在复制数据",
            done: 128,
            total: 512,
            bytes: 12 * 1024 * 1024,
            bytesTotal: 48 * 1024 * 1024,
            message: null,
        });
        expect(v.stageLabel).toBe("正在复制数据");
        expect(v.percent).toBe(25);
        expect(v.indeterminate).toBe(false);
        // 字节按人话格式化，两个数都要在：只给"已复制"用户无法判断还剩多少。
        expect(v.bytesText).toBe("12.0 MB / 48.0 MB");
        expect(v.itemsText).toBe("128 / 512");
    });

    it("百分比封顶 100，不出现 101%（后端重复发最后一帧也不会画出界）", () => {
        const v = buildProgressView({
            stage: "copying",
            stageLabel: "正在复制数据",
            done: 513,
            total: 512,
            bytes: 0,
            bytesTotal: 0,
            message: null,
        });
        expect(v.percent).toBe(100);
    });
});

describe("buildProgressView —— total === 0 必须走「不确定进度」", () => {
    const base = {
        stage: "deferred",
        stageLabel: "数据已就绪，等待重启接管",
        done: 0,
        total: 0,
        bytes: 0,
        bytesTotal: 0,
        message: null,
    };

    it("percent 必须是 null，而不是 0 或 NaN", () => {
        const v = buildProgressView(base);
        expect(v.percent).toBeNull();
        expect(v.indeterminate).toBe(true);
        // 这两条是真正的判别点：写成 `done/total*100` 会得到 NaN，
        // 写成 `total ? ... : 0` 会得到 0。两种实现都会让下面任一条变红。
        expect(v.percent).not.toBe(0);
        expect(Number.isNaN(v.percent as unknown as number)).toBe(false);
    });

    it("不产出条目计数（`0 / 0` 是最容易漏出去的那种假数字）", () => {
        expect(buildProgressView(base).itemsText).toBeNull();
    });

    it("bytesTotal === 0 时不产出字节文案", () => {
        expect(buildProgressView(base).bytesText).toBeNull();
    });

    it("total 为负或非有限值同样按不可计量处理", () => {
        expect(buildProgressView({ ...base, total: -5 }).percent).toBeNull();
        expect(buildProgressView({ ...base, total: Infinity, done: 1 }).percent).toBeNull();
    });

    it("done=0 而 total>0 时是真实的 0%，不能与「不可计量」混为一谈", () => {
        // 反向边界：如果实现把所有 0 都当"不可计量"，这条会红。
        // 真 0%（刚开始拷贝）就该显示 0%，否则用户看不到"已开始"。
        const v = buildProgressView({ ...base, stage: "copying", total: 100, done: 0 });
        expect(v.percent).toBe(0);
        expect(v.indeterminate).toBe(false);
    });
});
