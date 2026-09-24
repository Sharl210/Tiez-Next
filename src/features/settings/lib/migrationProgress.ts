import { formatBytes } from "./formatBytes";

/**
 * 迁移进度事件 ↔ 界面 的**纯逻辑**（无 React、无 Tauri）。
 *
 * # 为什么单独抽出来
 *
 * 进度显示最容易被"看起来对"的实现蒙过去：`done / 0` 会算出 `Infinity` 或 `NaN`，
 * 而 `NaN%` 在浏览器里既不是异常也不会让任何测试变红 —— 它只是**静默地渲染成
 * 一根画不出来的进度条**。真机上用户看到的就是"点了迁移，什么都没有"。
 * 所以进度换算必须是一个可以直接对数字下断言、不依赖 DOM 的纯函数。
 *
 * # 契约归属
 *
 * 事件名、字段名、`stage` 取值、`stageLabel` 的语义全部来自
 * `plans/v0.5.3-migration-and-settings/plan.md` 的【冻结契约】。
 * **界面不改写 stageLabel**：后端已经产出人话，前端再翻一遍必然出现两边不一致
 * （后端改了文案、前端还在说旧话）。这里只做"怎么摆"，不做"叫什么"。
 */

/** 过程中事件。 */
export const EVENT_MIGRATION_PROGRESS = "migration-progress";
/** 结束事件（成功 / 失败 / deferred 都会发一次）。 */
export const EVENT_MIGRATION_DONE = "migration-done";

/**
 * `listen` 建立之后拉取一次当前快照所用的命令名。
 *
 * ⚠️【契约缺口】冻结契约只规定了两个**事件**，没有规定"初值怎么拉"。
 * 需求明确要求照抄 `SettingsPanel.tsx:429-431` 的既有模式（先 `listen` 再 `invoke`），
 * 否则"事件先于监听"的那一段会永久丢失，用户看到的是进度条卡在第一帧。
 * 这里按本仓库既有命名习惯取名（对齐 `get_cloud_sync_status` / `get_mqtt_status`），
 * **已在报告中列为需要后端确认的契约项**，不是前端擅自扩权。
 * 命令不存在时前端不报错、不伪造进度（见 hook 的 catch）。
 */
export const COMMAND_MIGRATION_PROGRESS_SNAPSHOT = "get_migration_progress";

/** 契约里列出的阶段取值。后端若新增取值，界面无需改动即可显示（见下方容错）。 */
export type MigrationStage =
    | "precheck"
    | "copying"
    | "verifying"
    | "deferred"
    | "done"
    | "failed";

/**
 * 终态阶段：到达后本轮迁移结束，界面解除"进行中"的禁用态。
 *
 * `deferred` 是终态**但不是失败** —— 它是新版两阶段迁移的常态路径
 * （运行时段只复制到暂存，重启后接管）。
 */
export const TERMINAL_MIGRATION_STAGES: readonly string[] = ["deferred", "done", "failed"];

/** 事件 payload（契约 §2）。字段名一律用后端的 camelCase 拼写。 */
export interface MigrationProgressPayload {
    stage: string;
    stageLabel: string;
    done: number;
    total: number;
    bytes: number;
    bytesTotal: number;
    message: string | null;
}

/** 界面真正要用的、已经算好的形状。 */
export interface MigrationProgressView {
    stage: string;
    /** 直接来自后端的 stageLabel，未做任何映射。 */
    stageLabel: string;
    /** `null` = 不可计量（`total === 0`），界面显示"不确定进度"，**不得显示 0%**。 */
    percent: number | null;
    indeterminate: boolean;
    /** 例如 `12.4 MB / 48.1 MB`；`bytesTotal === 0` 时为 `null`。 */
    bytesText: string | null;
    /** 例如 `128 / 512 项`；`total === 0` 时为 `null`。 */
    itemsText: string | null;
    message: string | null;
    terminal: boolean;
}

const toFiniteNumber = (value: unknown): number => {
    const n = typeof value === "number" ? value : Number(value);
    return Number.isFinite(n) ? n : 0;
};

const toText = (value: unknown): string => (typeof value === "string" ? value : "");

/**
 * 把事件 payload 收敛成契约形状。
 *
 * # 为什么要容错两种拼写
 *
 * 后端与本前端是**并行**实施的。Rust 结构体字段是 `snake_case`，要变成契约里的
 * `camelCase` 必须在结构体上加 `#[serde(rename_all = "camelCase")]`。**漏了它不会
 * 编译报错**，只会让 `stageLabel` 变成 `undefined` —— 于是用户看到一根没有说明文字
 * 的进度条，"迁移显示"这个核心诉求等于没做。
 *
 * 因此这里同时接受 `stageLabel` 与 `stage_label`。这**不改变契约字段名**，
 * 只是让前端在字段拼写还没对齐时仍然可用；拼写本身已在报告中列为需确认项。
 */
export const normalizeProgressPayload = (raw: unknown): MigrationProgressPayload | null => {
    if (!raw || typeof raw !== "object") return null;
    const r = raw as Record<string, unknown>;
    const pick = (camel: string, snake: string): unknown =>
        r[camel] !== undefined ? r[camel] : r[snake];

    const message = pick("message", "message");

    return {
        stage: toText(pick("stage", "stage")),
        stageLabel: toText(pick("stageLabel", "stage_label")),
        done: toFiniteNumber(pick("done", "done")),
        total: toFiniteNumber(pick("total", "total")),
        bytes: toFiniteNumber(pick("bytes", "bytes")),
        bytesTotal: toFiniteNumber(pick("bytesTotal", "bytes_total")),
        message: typeof message === "string" && message.length > 0 ? message : null,
    };
};

/** 是否终态（收到它就不再"进行中"）。 */
export const isTerminalStage = (stage: string): boolean =>
    TERMINAL_MIGRATION_STAGES.includes(stage);

/**
 * 由 payload 算出界面要显示的一切。
 *
 * 关键约束（对应真机反馈"看不到进度"）：
 * - `total === 0` → `percent = null`、`indeterminate = true`；
 *   **绝不返回 `0`**，因为 `0%` 会让用户以为迁移卡死，而实际上后端只是没给总量。
 * - 任何非有限数（`NaN` / `Infinity`，来自 `0/0` 或字段缺失）一律按不可计量处理。
 */
export const buildProgressView = (
    payload: MigrationProgressPayload
): MigrationProgressView => {
    const measurable = payload.total > 0 && Number.isFinite(payload.total);
    const rawPercent = measurable ? (payload.done / payload.total) * 100 : NaN;
    const percent = Number.isFinite(rawPercent)
        ? Math.max(0, Math.min(100, Math.round(rawPercent)))
        : null;

    const hasItems = measurable && Number.isFinite(payload.done);
    const hasBytes = payload.bytesTotal > 0 && Number.isFinite(payload.bytesTotal);

    return {
        stage: payload.stage,
        stageLabel: payload.stageLabel,
        percent,
        indeterminate: percent === null,
        bytesText: hasBytes
            ? `${formatBytes(payload.bytes)} / ${formatBytes(payload.bytesTotal)}`
            : null,
        itemsText: hasItems ? `${payload.done} / ${payload.total}` : null,
        message: payload.message,
        terminal: isTerminalStage(payload.stage),
    };
};
