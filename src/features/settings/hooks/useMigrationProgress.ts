import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import {
    COMMAND_MIGRATION_PROGRESS_SNAPSHOT,
    EVENT_MIGRATION_DONE,
    EVENT_MIGRATION_PROGRESS,
    buildProgressView,
    isTerminalStage,
    normalizeProgressPayload,
    type MigrationProgressView,
} from "../lib/migrationProgress";

/**
 * 订阅迁移进度事件，并把"迁移是否正在进行"变成一份界面状态。
 *
 * # 三条必须照抄既有写法的地方（都不是风格问题，是缺陷防线）
 *
 * 1. **先 `listen` 再 `invoke`**（`SettingsPanel.tsx:429-431`）。
 *    反过来的话，在"命令已在跑、事件已开始发"的那一小段里才建立监听，前面的进度
 *    永久丢失 —— 用户看到进度条停在半路再也不动。先监听后拉快照，两条路径合起来
 *    才不会漏帧。
 *
 * 2. **卸载时逐个 unlisten**（`useClipboardEvents.ts:39-44`）。
 *    `listen()` 返回 Promise；不 `.then(f => f())` 就等于没清理，设置面板反复开合
 *    会让监听器不断累积。
 *
 * 3. **快照命令不存在不报错**。快照命令是**契约缺口**（冻结契约只规定了两个事件，
 *    没写初值怎么拉，详见 `migrationProgress.ts` 的说明）。后端尚未实现时，
 *    `invoke` 会 reject —— 那不能让设置面板崩，进度只是"暂时没有初值"。
 *
 * # 为什么禁用态由事件驱动，而不只靠 `await invoke(...)` 的 pending
 *
 * 迁移是**两阶段**的：运行时段做完（`deferred`）之后用户完全可能再点一次。
 * 事件是唯一能同时覆盖"命令还在跑"与"事件已经先说结束"的可信来源；
 * `markCommandSettled` 只作为事件通道缺失终态帧时的兜底解除条件。
 */
export interface UseMigrationProgressResult {
    /** 最近一次进度（`null` = 还没收到任何事件/快照）。 */
    progress: MigrationProgressView | null;
    /** 是否正在进行中。用于禁用迁移按钮，防止重复点击叠加迁移。 */
    running: boolean;
    /**
     * 迁移命令返回时由调用方显式通知：命令已结束，无论事件有没有到，都该解除禁用。
     *
     * 需求把"invoke 返回"也列为解除条件，因为事件通道在某些失败路径上可能没有
     * 对应的终态帧。
     */
    markCommandSettled: () => void;
}

export const useMigrationProgress = (enabled = true): UseMigrationProgressResult => {
    const [progress, setProgress] = useState<MigrationProgressView | null>(null);
    const [running, setRunning] = useState(false);

    // 卸载后不再 setState：`listen()` 的 Promise 可能在卸载之后才 settle，
    // 那时 setState 已经无处可去（React 18+ 不再警告，但状态仍会被错误地推进）。
    const aliveRef = useRef(true);

    const apply = useCallback((raw: unknown) => {
        const payload = normalizeProgressPayload(raw);
        // 归一化失败（payload 不是对象、后端还没发过任何一帧）时保持现状。
        if (!payload || !aliveRef.current) return;
        setProgress(buildProgressView(payload));
        // 终态（done / failed / deferred）即解除禁用；其余一律视为进行中。
        setRunning(!isTerminalStage(payload.stage));
    }, []);

    useEffect(() => {
        if (!enabled) return;
        aliveRef.current = true;

        const unlistenProgress = listen<unknown>(EVENT_MIGRATION_PROGRESS, (event) => {
            apply(event.payload);
        });
        const unlistenDone = listen<unknown>(EVENT_MIGRATION_DONE, (event) => {
            apply(event.payload);
        });

        // 先监听、后拉初值：避免"事件先于监听"丢失快照。
        invoke<unknown>(COMMAND_MIGRATION_PROGRESS_SNAPSHOT)
            .then((snapshot) => {
                // 没有进行中的迁移时后端可能回 null/undefined，此时保持现状，
                // 不要把刚收到的事件进度清掉（那会让进度条在收尾时闪回空白）。
                if (snapshot) apply(snapshot);
            })
            .catch(() => {
                // 契约缺口 / 后端未实现：静默降级，不阻断设置面板的其他部分。
            });

        return () => {
            aliveRef.current = false;
            unlistenProgress.then((f) => f());
            unlistenDone.then((f) => f());
        };
    }, [enabled, apply]);

    const markCommandSettled = useCallback(() => setRunning(false), []);

    return { progress, running, markCommandSettled };
};
