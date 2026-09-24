import type { MigrationProgressView } from "../lib/migrationProgress";
// 样式随之按需加载（与 `TagManager.tsx` 加载 `tag-group-menu.css` 同一做法）：
// 目标样式只在「迁移中心」里出现，没必要进全局样式表。`DataSettingsGroup` 渲染本
// 组件，因此在设置面板打开时这份样式必然已被加载。
import "../../../styles/components/migration-progress.css";

/**
 * 迁移进度条（纯展示，无副作用）。
 *
 * # 为什么进度条要单独成组件
 *
 * "进度"这件事有两个必须能被单独测的判定，混在 `DataSettingsGroup` 那一千行里
 * 就没法钉住：
 *
 * 1. **`total === 0` 时不能显示百分比**。`done / 0` 会算出 `NaN`；`NaN%` 在浏览器里
 *    既不抛异常也不进控制台，只是**画出一根宽度为 NaN 的条**（等价于 0 宽）。
 *    真机上用户看到的就是"点了迁移，什么反应都没有" —— 正是本轮要修的那个反馈。
 *    因此不可计量时必须走 `.indeterminate`（一条往复动画），而不是 0%。
 * 2. **`stageLabel` 必须原样显示**。后端已产出人话（「正在复制数据」）。
 *    前端若再做一次 stage→文案映射，两边翻译就会各自演化：后端改了文案，
 *    用户看到的还是旧说法。这里只渲染，不翻译。
 *
 * # 呈现方式参考了既有进度条
 *
 * 轨道/填充的视觉语言对齐 `UpdateDialog.tsx` 的 `.update-progress-track/bar`
 * （3px 高、圆角、`--accent-color` 填充）。**但没有沿用它的数据来源**：
 * `useAutoUpdate.ts` 的进度是 `Math.min(prev + 5, 99)` 的**模拟增量**，
 * 与真实拷贝字节无关。迁移的每个数字都来自后端事件，是真实量。
 */
interface MigrationProgressPanelProps {
    /** 当前进度；`null` 时整块不渲染。 */
    progress: MigrationProgressView | null;
    t: (key: string) => string;
}

const MigrationProgressPanel = ({ progress, t }: MigrationProgressPanelProps) => {
    if (!progress) return null;

    /*
     * 什么时候显示、什么时候收起。
     *
     * `deferred` **必须继续显示**，这不是可选项：
     * 契约 §2 的表格里 `deferred` / `done` / `failed` 三个阶段都标着"不可计量"，
     * 而 `precheck` / `copying` / `verifying` 全部可计量（有 total）。
     * 也就是说 `total === 0` 这个分支在真机上**只会出现在终态**。
     * 若把终态一律收起，"不确定进度"就成了永远走不到的代码 ——
     * 而它恰恰是用户反馈"看不到进度"时要看到的那句话。
     * 而且 `deferred` 表达的是一个**持续存在的待办状态**（等用户重启），
     * 让它留在界面上才是诚实的。
     *
     * `done` / `failed` 则收起：结果卡片紧接着给出结论与后续动作，
     * 两处同时讲同一件事会让用户分不清哪个才算数。
     */
    if (progress.terminal && progress.stage !== "deferred") return null;

    return (
        <div className="migration-progress" data-stage={progress.stage}>
            <div className="migration-progress-head">
                {/* stageLabel 原样显示：后端已经给了人话，界面不翻译。 */}
                <span className="migration-progress-label">{progress.stageLabel}</span>
                {/* 不可计量时显示"不确定进度"文案，绝不显示 done/0 或 0%。 */}
                <span className="migration-progress-value">
                    {progress.indeterminate
                        ? t("migration_progress_indeterminate")
                        : `${progress.percent}%`}
                </span>
            </div>

            <div
                className="migration-progress-track"
                role="progressbar"
                aria-label={progress.stageLabel}
                // 不可计量时不设 aria-valuenow —— 传 0 会让读屏软件念出"0%",
                // 正是我们要避免的那个误导。
                aria-valuemin={progress.indeterminate ? undefined : 0}
                aria-valuemax={progress.indeterminate ? undefined : 100}
                aria-valuenow={progress.indeterminate ? undefined : (progress.percent ?? undefined)}
            >
                <div
                    className={
                        progress.indeterminate
                            ? "migration-progress-bar indeterminate"
                            : "migration-progress-bar"
                    }
                    style={progress.indeterminate ? undefined : { width: `${progress.percent}%` }}
                />
            </div>

            {(progress.itemsText || progress.bytesText) && (
                <div className="migration-progress-meta">
                    {progress.itemsText && (
                        <span>
                            {t("migration_progress_items").replace("{progress}", progress.itemsText)}
                        </span>
                    )}
                    {progress.bytesText && (
                        <span>
                            {t("migration_progress_bytes").replace("{progress}", progress.bytesText)}
                        </span>
                    )}
                </div>
            )}

            {progress.message && <div className="migration-progress-message">{progress.message}</div>}
        </div>
    );
};

export default MigrationProgressPanel;
