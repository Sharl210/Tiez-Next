import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
    UNKNOWN_AUTOSTART_STATE,
    registeredPathOf,
    type AutostartState,
} from "../../../shared/lib/autostart";

interface AutostartSettingProps {
    t: (key: string) => string;
    /** 已知的初值（来自应用启动时的读取），仅用于首帧，不作为权威。 */
    initialEnabled: boolean;
    /** 把权威状态同步回应用级 state（其它地方也在读它）。 */
    onStateChange: (enabled: boolean) => void;
}

/**
 * 「开机自启动」开关：**只有后端回读确认过才亮**。
 *
 * # 这里修的是"静默失败"，不是"权限不足"
 *
 * 这个开关写的是 `HKCU\...\Run`，当前用户可写，**不需要管理员权限**。它以前看起来
 * "点了没反应/永远亮着"的真正原因是三步都缺：
 *
 * 1. **乐观置位**：先把开关点亮，再发命令；
 * 2. **错误只进 console**：`.catch(console.error)`，用户看不见任何失败；
 * 3. **写完不回读**：后端 `set_value` 没报错就返回成功——而"写 API 成功"与
 *    "系统真的会在开机时拉起这个路径"是两件事（值可能没落地，也可能指向改名前的旧路径）。
 *
 * 所以本组件的行为是：
 * - **不乐观置位**：切换时进入 pending（开关 disabled），等后端回读结果再决定亮不亮；
 * - **失败可见**：失败时用 `autostart_failed` 文案把原因显示在设置行里（这条文案早就
 *   写好了三语，此前**零引用**）；
 * - **出示证据**：成功后把**注册表里读回来的命令原文**显示出来。这是"真的生效了"的
 *   唯一可信证据；只显示一个亮着的开关，等于让用户继续凭信仰判断。
 */
const AutostartSetting = ({ t, initialEnabled, onStateChange }: AutostartSettingProps) => {
    const [state, setState] = useState<AutostartState>({
        ...UNKNOWN_AUTOSTART_STATE,
        enabled: initialEnabled,
    });
    const [pending, setPending] = useState(false);
    const [error, setError] = useState<string | null>(null);

    /** 读回当前真实状态（含证据），并同步给应用级 state。 */
    const refresh = useCallback(async () => {
        try {
            const next = await invoke<AutostartState>("is_autostart_enabled");
            setState(next);
            onStateChange(next.enabled);
        } catch (err) {
            // 读不到就如实标为"未能确认"，不沿用旧值假装一切正常。
            setState(UNKNOWN_AUTOSTART_STATE);
            setError(String(err));
        }
    }, [onStateChange]);

    useEffect(() => {
        void refresh();
    }, [refresh]);

    const toggle = async (enabled: boolean) => {
        setPending(true);
        setError(null);
        try {
            // 后端在本命令内部完成「写入 → 立即回读注册表 → 比对值内容」，
            // 只有回读通过才返回 Ok；因此这里的返回值本身就是证据。
            const next = await invoke<AutostartState>("toggle_autostart", { enabled });
            setState(next);
            onStateChange(next.enabled);
        } catch (err) {
            // 失败时**保持原状态**：不回滚成"用户点之前的猜测值"，而是重新回读一次
            // 注册表——那才是系统此刻的真实状态（可能是"根本没写进去"，也可能是
            // "写进去了但指向旧路径"，两者对用户的意义不同）。
            await refresh();
            setError(String(err));
        } finally {
            setPending(false);
        }
    };

    const readbackPath = state.registeredCommand ? registeredPathOf(state.registeredCommand) : null;
    const hasStale = state.staleNames.length > 0;

    return (
        <div className="setting-item">
            <div className="item-label-group">
                <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
                    <span className="item-label">{t("autostart")}</span>
                    {state.enabled && (
                        <span
                            style={{
                                fontSize: "10px",
                                padding: "1px 5px",
                                borderRadius: "3px",
                                border: "1px solid var(--border-color, rgba(128,128,128,0.35))",
                                color: "var(--text-secondary)",
                            }}
                        >
                            {t("autostart_verified")}
                        </span>
                    )}
                </div>
                {/*
                  回读证据：显示注册表里读回来的目标路径，而不是"设置成功"四个字。
                  用户据此能判断系统开机时到底会启动哪个文件。
                */}
                {state.enabled && readbackPath && (
                    <span className="hint" style={{ wordBreak: "break-all" }}>
                        {t("autostart_registered_at")}
                        <code>{readbackPath}</code>
                    </span>
                )}
                {!state.enabled && state.readable && hasStale && (
                    <span className="hint" style={{ color: "var(--danger-color, #c05050)" }}>
                        {t("autostart_stale_names")}
                        <code>{state.staleNames.join(", ")}</code>
                        <br />
                        {t("autostart_stale_hint")}
                    </span>
                )}
                {error && (
                    <span className="hint" style={{ color: "var(--danger-color, #c05050)", wordBreak: "break-all" }}>
                        {t("autostart_failed")}
                        {error}
                    </span>
                )}
                {!state.readable && !error && (
                    <span className="hint" style={{ color: "var(--danger-color, #c05050)" }}>
                        {t("autostart_readback_failed")}
                    </span>
                )}
            </div>
            <label className="switch">
                <input
                    className="cb"
                    type="checkbox"
                    checked={state.enabled}
                    disabled={pending}
                    onChange={(e) => void toggle(e.target.checked)}
                />
                <div className="toggle"><div className="left" /><div className="right" /></div>
            </label>
        </div>
    );
};

export default AutostartSetting;
