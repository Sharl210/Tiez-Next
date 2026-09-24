import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

/** 后端 `get_paste_method_status` 的回读形状。 */
interface PasteMethodStatus {
    /** 用户配置的粘贴方案（原样回传，后端不会替换它）。 */
    method: string;
    /** 当前进程是否已提权。 */
    isAdmin: boolean;
    /** 该配置在当前权限下是否按用户预期生效。 */
    effective: boolean;
    /** 该方案是否**需要**提权才完整生效（目前只有 `game_mode`）。 */
    requiresAdmin: boolean;
}

interface PasteMethodSettingProps {
    t: (key: string) => string;
    method: string;
    setMethod: (val: string) => void;
}

/**
 * 「粘贴方案」——把"静默改用户设置"换成"保留选择 + 告知 + 一键提权"。
 *
 * # 原来的行为为什么必须改
 *
 * 「游戏模式」需要管理员权限。旧实现在**每次启动**时：发现未提权就把 `app.paste_method`
 * 直接改成 `shift_insert`，只留一行日志。用户侧看到的是"我明明选过游戏模式，怎么又变
 * 回去了"，而且**没有任何提示**——这是全仓唯一一处"应用自己推翻用户设置且不告知"。
 *
 * # 现在的行为
 *
 * - **不动用户的设置**：`app.paste_method` 原样保留，哪怕当前没提权；
 * - **如实告知**：把"未提权，所以本选项暂不生效"写在设置行里，并说明"你的选择已被保留"；
 * - **给一条出路**：接上早就写好却零引用的 `restart_as_admin`（一键以管理员身份重启）；
 * - **未生效时不假装生效**：选中项旁边直接标注，而不是让用户以为已经在用游戏模式了。
 *
 * 【为什么不保留"自动回退"】回退会让"用户以为开着"和"实际生效"继续分叉，只是把分叉
 * 藏得更深。告知 + 一键提权才是把分叉摆到明面上。
 */
const PasteMethodSetting = ({ t, method, setMethod }: PasteMethodSettingProps) => {
    const [status, setStatus] = useState<PasteMethodStatus | null>(null);
    const [restarting, setRestarting] = useState(false);
    const [restartError, setRestartError] = useState<string | null>(null);

    /**
     * 重新询问后端"当前配置是否生效"。
     *
     * 【为什么要重新问而不是本地推导】提权状态只有后端能查（`check_is_admin` 走
     * TokenElevation）。本地缓存一个 `isAdmin` 会在用户手动以管理员身份重启后撒谎。
     */
    const refresh = useCallback(async () => {
        try {
            const next = await invoke<PasteMethodStatus>("get_paste_method_status");
            setStatus(next);
        } catch (err) {
            // 问不到就按"未知"处理：不显示"未生效"（可能只是命令失败），
            // 也绝不显示"已生效"（那是没有依据的保证）。
            console.error("读取粘贴方案状态失败", err);
            setStatus(null);
        }
    }, []);

    useEffect(() => {
        void refresh();
    }, [refresh]);

    const needsAdmin = status?.requiresAdmin === true && status.isAdmin === false;

    const change = async (value: string) => {
        // 先落设置（后端启动优化与粘贴路径都读这个键），再刷新状态用于告知。
        setMethod(value);
        try {
            await invoke("save_setting", { key: "app.paste_method", value });
        } catch (err) {
            console.error("保存粘贴方案失败", err);
        }
        await refresh();
    };

    const restartAsAdmin = async () => {
        setRestarting(true);
        setRestartError(null);
        try {
            await invoke("restart_as_admin");
        } catch (err) {
            // UAC 被取消、或被安全策略挡住时如实告知（后端已把这种情况与成功区分）。
            setRestartError(String(err));
            setRestarting(false);
        }
    };

    return (
        <div className="setting-item">
            <div className="item-label-group">
                <span className="item-label">{t("paste_method")}</span>
                <span className="hint">{t("paste_method_hint")}</span>
                {status && (
                    <span className="hint">{t(`paste_method_${status.method}_hint`)}</span>
                )}
                {needsAdmin && (
                    <span className="hint" style={{ color: "var(--danger-color, #c05050)" }}>
                        {t("game_mode_needs_admin")}
                    </span>
                )}
                {restartError && (
                    <span className="hint" style={{ color: "var(--danger-color, #c05050)" }}>
                        {restartError}
                    </span>
                )}
            </div>
            <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
                <select
                    className="search-input"
                    style={{
                        borderRadius: "0",
                        padding: "6px",
                        width: "130px",
                        background: "var(--bg-input)",
                        border: "2px solid var(--border-dark)",
                        color: "var(--text-primary)",
                        fontSize: "12px",
                    }}
                    value={method}
                    onChange={(e) => void change(e.target.value)}
                >
                    <option value="shift_insert">{t("paste_method_shift_insert")}</option>
                    <option value="ctrl_v">{t("paste_method_ctrl_v")}</option>
                    <option value="game_mode">{t("paste_method_game_mode")}</option>
                </select>
                {needsAdmin && (
                    <button
                        type="button"
                        className="btn-icon"
                        disabled={restarting}
                        onClick={() => void restartAsAdmin()}
                        title={t("restart_as_admin_hint_settings")}
                        style={{ width: "auto", padding: "4px 12px", fontSize: "10px", height: "24px" }}
                    >
                        {t("restart_as_admin")}
                    </button>
                )}
            </div>
        </div>
    );
};

export default PasteMethodSetting;
