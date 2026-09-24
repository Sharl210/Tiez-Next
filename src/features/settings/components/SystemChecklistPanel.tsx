import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

/**
 * 需要**用户手动操作**的系统级设置清单。
 *
 * # 这三项为什么应用自己做不了
 *
 * 应用能改的东西分三层：自己的设置（能做）、当前用户可写的系统项如 `HKCU\...\Run`
 * 开机自启动（能做，且**不需要管理员权限**，但要写后回读确认）、以及**系统强制介入项** ——
 * 防火墙放行、UAC 提权、任务管理器里的启动项开关。第三类只有三项，且都**没有**让应用
 * 静默代做的接口：防火墙要管理员令牌、UAC 是系统弹窗、任务管理器的 `StartupApproved`
 * 会覆盖应用写好的 `Run` 键意图。
 *
 * # 为什么是清单而不是一次性弹窗
 *
 * 用户的要求是「**逐条**让用户帮忙，**点一条、设置一条**」——像安卓应用申请权限那样：
 * 每条独立、有当前状态、可重试。所以这里不做"一次性告知"，而是可反复查看的清单。
 *
 * # 为什么**不**用"已读标记"（与同类组件的关键区别）
 *
 * 同类的"凭据暴露告知"用的是"这条已展示并处理过"的一次性标记。**本清单不能照抄**：
 * 凭据告知是一次性事件，而系统级设置的状态是**客观可变的** —— 用户今天放行了防火墙，
 * 明天可能又去任务管理器把启动项禁掉。用标记掩盖的后果是
 * **"上次点了知道了，这次真坏了却不提示"**。
 *
 * 所以：
 * - 每次展开都**重新探测**（不缓存）
 * - `ack`（"不再提醒"）**不改变**探测到的真实状态，只是让这一条不再催促
 * - 探测失败是**第三种状态**（"无法确认"），**不得**显示成"待处理" ——
 *   否则用户会去处理一件本来就做不到的事
 */

export interface SystemChecklistItem {
    /** 稳定标识：`firewall` / `uac` / `startup_approved` */
    id: string;
    /** 探测到的真实状态。**由探测得出，不由标记得出。** */
    satisfied: boolean;
    /**
     * 探测本身是否可信。
     *
     * 为 `false` 时表示"无法确认"——必须与 `satisfied: false`（已确认未满足）分开呈现。
     */
    probeOk: boolean;
    /** 探测细节原文（读到的规则/值）。这是"真的查过了"的唯一证据。 */
    detail: string | null;
    /** 用户是否已要求不再提醒这一条。**不影响上面三个字段。** */
    ack: boolean;
}

interface SystemChecklistPanelProps {
    t: (key: string) => string;
}

/** 每一项的 id → 文案键前缀。文案缺失时回退到 id，不会渲染成空白。 */
const LABEL_PREFIX = "system_check_";

const SystemChecklistPanel = ({ t }: SystemChecklistPanelProps) => {
    const [items, setItems] = useState<SystemChecklistItem[] | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [busyId, setBusyId] = useState<string | null>(null);
    const [probing, setProbing] = useState(false);

    /** 重新探测。**每次都真的问后端**，不使用任何缓存值。 */
    const probe = useCallback(async () => {
        setProbing(true);
        setError(null);
        try {
            const next = await invoke<SystemChecklistItem[]>("get_system_checklist");
            setItems(next);
        } catch (e) {
            // 整体探测失败与"某一项无法确认"是两件事：这里是命令本身失败，
            // 不能悄悄显示成空清单（那看起来就是"都处理好了"）。
            setError(String(e));
            setItems(null);
        } finally {
            setProbing(false);
        }
    }, []);

    useEffect(() => {
        void probe();
    }, [probe]);

    /** 用户明确要求这一条不再提醒。**只写标记，不改探测结果。** */
    const ack = useCallback(
        async (id: string) => {
            setBusyId(id);
            try {
                await invoke("ack_system_checklist_item", { id });
                // 写后重新探测：标记写入是否生效由后端回读确认，不乐观更新界面。
                await probe();
            } catch (e) {
                setError(String(e));
            } finally {
                setBusyId(null);
            }
        },
        [probe]
    );

    /**
     * 把一条渲染成一行。
     *
     * 三种状态各自呈现，互不混同：
     * - `probeOk === false` → **无法确认**（中性色，不催办）
     * - `satisfied === true` → 已满足（收起来，不占用户注意力）
     * - 其余 → 待处理（**给出具体怎么做**）
     */
    const renderItem = (item: SystemChecklistItem) => {
        const key = `${LABEL_PREFIX}${item.id}`;
        const title = t(`${key}_title`) === `${key}_title` ? item.id : t(`${key}_title`);
        const howto = t(`${key}_howto`);
        const hasHowto = howto !== `${key}_howto`;

        // 已满足 → 不显示（清单只留"还需要做事"的项）。
        // 注意：`satisfied` 与 `probeOk` 是两个维度，探测失败时 `satisfied` 必为 false
        // （见后端 `build_checklist`），所以这里不会把"无法确认"误收起来。
        if (item.satisfied) return null;

        const unavailable = !item.probeOk;

        return (
            <div
                key={item.id}
                className={`system-check-row${unavailable ? " is-unknown" : " is-pending"}`}
            >
                <div className="system-check-main">
                    <span className="system-check-title">{title}</span>
                    <span className="system-check-state">
                        {unavailable ? t("system_check_state_unknown") : t("system_check_state_pending")}
                    </span>
                </div>

                {hasHowto && <p className="system-check-howto">{howto}</p>}

                {/* 探测细节：把读到的原文给用户看，这是"真的查过了"的唯一证据 */}
                {item.detail && <code className="system-check-detail">{item.detail}</code>}

                <div className="system-check-actions">
                    <button className="btn-sm" onClick={() => void probe()} disabled={probing}>
                        {t("system_check_recheck")}
                    </button>
                    {!unavailable && !item.ack && (
                        <button
                            className="btn-sm"
                            onClick={() => void ack(item.id)}
                            disabled={busyId === item.id}
                        >
                            {t("system_check_ack")}
                        </button>
                    )}
                </div>
            </div>
        );
    };

    // 与 `renderItem` 的早退条件**用同一条判据**。两处若各写一份，迟早分叉成
    // "过滤掉了但没渲染"或"渲染了但没过滤"（前者让整块清单凭空消失）。
    const visible = (items ?? []).filter((i) => !i.satisfied);

    return (
        <div className="system-checklist">
            <p className="settings-subpage-note">{t("system_check_intro")}</p>

            {error && <div className="system-check-error">{t("system_check_probe_failed")}</div>}

            {items === null && !error && (
                <div className="system-check-loading">{t("system_check_loading")}</div>
            )}

            {items !== null && visible.length === 0 && (
                <div className="system-check-allok">{t("system_check_all_ok")}</div>
            )}

            {items !== null && visible.map(renderItem)}
        </div>
    );
};

export default SystemChecklistPanel;
