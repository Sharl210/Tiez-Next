import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ask } from "@tauri-apps/plugin-dialog";

interface WinVShortcutSettingProps {
    t: (key: string) => string;
    /** 注册表里 Win+V 接管是否**已实际生效**（由后端 `is_registry_win_v_optimized` 回读）。 */
    enabled: boolean;
    setEnabled: (val: boolean) => void;
    /** 主快捷键当前值：接管 Win+V 时要把它切到 `Win+V`，关闭时归还用户原来的热键。 */
    hotkey: string;
    updateHotkey: (key: string) => void;
    saveAppSetting: (key: string, val: string) => void;
    appSettings: Record<string, string>;
    theme: string;
    colorMode: string;
}

/** 用户原来的热键在开启接管前的暂存键（关闭时据此归还）。 */
const PRE_WIN_V_HOTKEY_KEY = "app.pre_win_v_hotkey";

/**
 * 「是否使用 Win+V 快捷键」——接管系统 Win+V 实现极速呼出。
 *
 * # 这个功能为什么曾经彻底不可用（两层原因，都要修）
 *
 * 1. **入口被误删**：上游「macos -> windows 对齐」把设置开关与它唯一的写入点一起删了，
 *    而后端 6 个命令全部完好。结果 `app.use_win_v_shortcut` **永远没人写**，
 *    `setup.rs` 里那个"启动时按设置启用优化"的分支**永不可达**——不是"设置不生效"，
 *    是根本没有代码路径。
 * 2. **键名分叉**：前端另一处代码读的是 `app.registry_win_v_enabled`，与后端读的
 *    `app.use_win_v_shortcut` 不是同一个键。即使恢复开关，**界面显示的开关状态**与
 *    **后端实际触发的优化**也会永远对不上。现在统一到 `app.use_win_v_shortcut`
 *    （旧键由一个一次性迁移搬过来，老用户的开关不会被静默关掉）。
 *
 * # 第三个坑：改了设置但没重启资源管理器
 *
 * `DisabledHotkeys` 只在 **explorer 启动时读一次**。旧实现重启失败时用
 * `let _ = ...` 吞掉错误，界面却写着"会重启资源管理器"——用户以为生效了，实际系统
 * 仍然占用 Win+V。这里按既有"改了要重启才生效"的视觉语言（`mcp_restart_badge` 同款）
 * 把这件事标出来，并把重启失败如实告知（设置本身已保存，只是要等下次登录）。
 */
const WinVShortcutSetting = ({
    t,
    enabled,
    setEnabled,
    hotkey,
    updateHotkey,
    saveAppSetting,
    appSettings,
    theme,
    colorMode,
}: WinVShortcutSettingProps) => {
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState<string | null>(null);
    /** 本机是否 Windows：非 Windows 上 Win+V 接管没有意义，整项不渲染。 */
    const [isWindows, setIsWindows] = useState(false);
    /** "注册表改了、explorer 还没重启"——由本开关自己记录本次会话是否发生过改动。 */
    const [needsRestart, setNeedsRestart] = useState(false);

    useEffect(() => {
        let alive = true;
        invoke<{ platform?: string }>("get_platform_info")
            .then((info) => {
                if (alive) setIsWindows((info?.platform ?? "").toLowerCase() === "windows");
            })
            .catch(() => {
                // 拿不到平台信息时按"不是 Windows"处理：宁可不显示这个开关，
                // 也不要在一个没有注册表的位置上给用户一个假的开关。
                if (alive) setIsWindows(false);
            });
        return () => {
            alive = false;
        };
    }, []);

    /** 用注册表回读刷新真值（界面显示的必须是系统此刻的真实状态）。 */
    const refresh = useCallback(async () => {
        try {
            const actual = await invoke<boolean>("is_registry_win_v_optimized");
            setEnabled(actual);
        } catch (err) {
            console.error("读取 Win+V 接管状态失败", err);
        }
    }, [setEnabled]);

    useEffect(() => {
        if (!isWindows) return;
        void refresh();
    }, [isWindows, refresh]);

    const toggle = async (next: boolean) => {
        setBusy(true);
        setError(null);
        try {
            // 1) 先落设置：这是后端启动优化分支的唯一依据，写失败必须让用户看见，
            //    否则"下次启动又变回去了"这种事没人能解释。
            await invoke("save_setting", { key: "app.use_win_v_shortcut", value: String(next) });

            // 2) 真正改注册表。
            const changed = await invoke<boolean>("trigger_registry_win_v_optimization", {
                enable: next,
            });

            // 3) 接管 / 归还主快捷键。
            let targetHotkey = "Alt+C";
            if (next) {
                if (hotkey && hotkey !== "Win+V") {
                    saveAppSetting("pre_win_v_hotkey", hotkey);
                }
                targetHotkey = "Win+V";
            } else {
                const saved = appSettings[PRE_WIN_V_HOTKEY_KEY];
                if (saved && saved !== "Win+V") targetHotkey = saved;
            }

            if (changed) {
                // 注册表发生了真实改动 → 必须重启 explorer 才会生效。
                setNeedsRestart(true);
                const confirmed = await ask(t("restart_explorer_confirm"), {
                    title: t("restart_explorer_title"),
                    kind: "warning",
                });
                if (confirmed) {
                    try {
                        await invoke("restart_explorer");
                        setNeedsRestart(false);
                    } catch (err) {
                        // 重启失败**不等于设置失败**：注册表已经写好了。
                        // 如实告知，并说清"下次登录会自动生效"（否则用户会以为白改了）。
                        setError(
                            `${t("win_v_restart_failed")}${String(err)} ${t("win_v_restart_failed_hint")}`
                        );
                    }
                    // 让 Win+V 有一个"已经重启完"的窗口再重新注册快捷键。
                    if (next) {
                        setTimeout(() => void updateHotkey(targetHotkey), 1500);
                    } else {
                        await updateHotkey(targetHotkey);
                    }
                    // explorer 重启会顶掉主题，稍后补一次。
                    setTimeout(() => {
                        void invoke("set_theme", {
                            theme,
                            color_mode: colorMode,
                            show_app_border: appSettings["app.show_app_border"] !== "false",
                        }).catch((e) => console.error("恢复主题失败", e));
                    }, 2500);
                } else {
                    // 用户选择先不重启：设置已生效于注册表，但系统仍未释放 Win+V。
                    // 保持 needsRestart 标记，让界面持续提示"需重启资源管理器"。
                    await updateHotkey(targetHotkey);
                }
            } else {
                // 注册表没有变化（本来就是这个状态）→ 不需要重启。
                await updateHotkey(targetHotkey);
            }

            await refresh();
        } catch (err) {
            await refresh();
            setError(`${t("win_v_write_failed")}${String(err)}`);
        } finally {
            setBusy(false);
        }
    };

    if (!isWindows) return null;

    return (
        <div className="setting-item">
            <div className="item-label-group">
                <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
                    <span className="item-label">{t("use_win_v_shortcut")}</span>
                    {enabled && needsRestart && (
                        <span
                            style={{
                                fontSize: "10px",
                                padding: "1px 5px",
                                borderRadius: "3px",
                                border: "1px solid rgba(200,80,80,0.5)",
                                color: "var(--danger-color, #c05050)",
                            }}
                        >
                            {t("win_v_restart_badge")}
                        </span>
                    )}
                </div>
                <span className="hint">{t("use_win_v_shortcut_hint")}</span>
                {error && (
                    <span className="hint" style={{ color: "var(--danger-color, #c05050)", wordBreak: "break-all" }}>
                        {error}
                    </span>
                )}
            </div>
            <label className="switch">
                <input
                    className="cb"
                    type="checkbox"
                    checked={enabled}
                    disabled={busy}
                    onChange={(e) => void toggle(e.target.checked)}
                />
                <div className="toggle"><div className="left" /><div className="right" /></div>
            </label>
        </div>
    );
};

export default WinVShortcutSetting;
