import { useCallback, useEffect, useState } from "react";
import type { ComponentType, ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ChevronDown, ChevronRight, List } from "lucide-react";
import BackupListModal, { type AutoBackupListPayload } from "../BackupListModal";
import { formatBytes } from "../../lib/formatBytes";

/**
 * 「自动备份（容灾）」设置区块。
 *
 * # 为什么与「数据管理」并列成独立分组，而不是塞进数据管理里面
 *
 * 用户对数据管理的分类有一条硬边界：**自动容灾备份**与**用户自己手动导出的备份**
 * 是两件不同的事，备份目录都不一样（前者由应用固定在容灾专用目录、参与份数轮换；
 * 后者由用户自己挑路径、不受份数约束）。数据管理分组里已经有一个「备份与恢复」
 * 子区块，那讲的正是**手动导出/导入**。
 *
 * 若把自动备份塞进同一个分组，两条链会共处一屏且都叫"备份"，用户很难分清"我改的
 * 份数上限影响的是哪一批"。独立分组 + 显式写出"此处备份与手动导出的路径不同"，
 * 是把这条边界摆在界面上，而不是只写在文档里。
 *
 * # 样式来源
 *
 * 结构、类名、间距全部沿用既有分组（`settings-group` / `group-header` /
 * `group-content` / `setting-item` / `item-label-group` / `switch` 的 `cb+toggle`
 * 三层写法 / `btn-icon` 内联尺寸），与「剪贴板设置」等分组逐项对齐；没有自创样式。
 * `LabelWithHint` 由 `SettingsPanel` 注入，与其它分组同源。
 */

interface LabelWithHintProps {
    label: string;
    hint?: string | ReactNode;
    hintKey: string;
}

interface AutoBackupSettingsGroupProps {
    t: (key: string) => string;
    collapsed: boolean;
    onToggle: () => void;
    LabelWithHint: ComponentType<LabelWithHintProps>;
    theme: string;
}

/** 与后端 `config.rs` 的合法区间一致；渲染成文案里的 "1–200"。 */
const MAX_KEEP_MIN = 1;
const MAX_KEEP_MAX = 200;
/** 周期区间（后端 `INTERVAL_MINUTES_MIN/MAX`）。用户只指定了默认 30 分钟。 */
const INTERVAL_MIN = 1;
const INTERVAL_MAX = 1440;

const AutoBackupSettingsGroup = ({
    t,
    collapsed,
    onToggle,
    LabelWithHint,
    theme,
}: AutoBackupSettingsGroupProps) => {
    /**
     * 配置与统计都来自后端（`get_auto_backup_config` / `list_auto_backups`），
     * 界面**不各自维护一份副本**：三个控件改完立即回写后端并以后端回传值为准，
     * 于是界面永远不会显示一个后端并不认的值。
     */
    const [config, setConfig] = useState<AutoBackupListPayload["config"] | null>(null);

    // 数字输入框用"草稿 + 提交"两段式（与「剪贴板设置」的 persistent_limit 同一做法）：
    // 用户清空输入框准备重敲时不能立刻把 0 发给后端。
    const [maxKeepDraft, setMaxKeepDraft] = useState("");
    const [intervalDraft, setIntervalDraft] = useState("");
    const [rangeError, setRangeError] = useState<string | null>(null);
    const [busy, setBusy] = useState(false);

    const [listOpen, setListOpen] = useState(false);
    const [summary, setSummary] = useState<{ total: number; pinned: number; bytes: number } | null>(null);

    const applyConfig = useCallback((next: AutoBackupListPayload["config"]) => {
        setConfig(next);
        setMaxKeepDraft(String(next.maxKeep));
        setIntervalDraft(String(next.intervalMinutes));
    }, []);

    const loadConfig = useCallback(() => {
        invoke<AutoBackupListPayload["config"]>("get_auto_backup_config")
            .then((cfg) => {
                applyConfig(cfg);
                setRangeError(null);
            })
            .catch((e) => {
                console.error("get_auto_backup_config failed:", e);
                setConfig(null);
            });
    }, [applyConfig]);

    const loadSummary = useCallback(() => {
        invoke<AutoBackupListPayload>("list_auto_backups")
            .then((payload) => {
                setSummary({
                    total: payload.totalCount,
                    pinned: payload.pinnedCount,
                    bytes: payload.entries.reduce((sum, e) => sum + e.sizeBytes, 0),
                });
            })
            .catch((e) => {
                console.error("list_auto_backups failed:", e);
                setSummary(null);
            });
    }, []);

    useEffect(() => {
        // 与数据管理分组同一取舍：折叠状态下不做无谓的磁盘统计。
        if (!collapsed) {
            loadConfig();
            loadSummary();
        }
    }, [collapsed, loadConfig, loadSummary]);

    /**
     * 提交一项配置改动。
     *
     * 后端对越界值是**报错**而不是夹紧（用户敲了 500 就要被告知合法区间是 1–200，
     * 而不是悄悄改成 200 让他以为生效了）。报错载荷带 `min`/`max`/`value`，因此提示里
     * 的区间数字来自后端，界面不硬编码。
     */
    const saveConfig = useCallback(
        async (patch: Record<string, unknown>) => {
            setBusy(true);
            setRangeError(null);
            try {
                const next = await invoke<AutoBackupListPayload["config"]>("set_auto_backup_config", {
                    patch,
                });
                applyConfig(next);
            } catch (e: unknown) {
                console.error("set_auto_backup_config failed:", e);
                const raw = e instanceof Error ? e.message : String(e);
                let text = raw;
                let fields: Record<string, unknown> | null = null;
                let code: string | null = null;
                try {
                    const parsed = JSON.parse(raw);
                    code = typeof parsed?.code === "string" ? parsed.code : null;
                    fields = parsed ?? null;
                } catch {
                    const brace = raw.indexOf("{");
                    if (brace > 0) {
                        try {
                            const parsed = JSON.parse(raw.slice(brace));
                            code = typeof parsed?.code === "string" ? parsed.code : null;
                            fields = parsed ?? null;
                        } catch {
                            /* 保留原文 */
                        }
                    }
                }
                if (code) {
                    const key = `auto_backup_err_${code.replace(/^auto_backup_/, "")}`;
                    const text2 = t(key);
                    if (text2 !== key) {
                        text = text2
                            .replace("{value}", String(fields?.value ?? "?"))
                            .replace("{min}", String(fields?.min ?? "?"))
                            .replace("{max}", String(fields?.max ?? "?"));
                    }
                }
                setRangeError(text);
                // 回读真实配置，把输入框恢复成后端认可的值。
                loadConfig();
            } finally {
                setBusy(false);
            }
        },
        [applyConfig, loadConfig, t]
    );

    /**
     * 提交最大留存份数。
     *
     * 先做一次本地范围检查是为了**在用户敲完立刻给反馈**（回车/blur 就能看到），
     * 而不是等一次往返；真正的权威判定仍在后端。
     */
    const commitMaxKeep = (rawValue?: string) => {
        const source = (rawValue ?? maxKeepDraft).trim();
        const parsed = parseInt(source, 10);
        if (!Number.isFinite(parsed)) {
            setMaxKeepDraft(config ? String(config.maxKeep) : String(MAX_KEEP_MIN));
            return;
        }
        if (parsed < MAX_KEEP_MIN || parsed > MAX_KEEP_MAX) {
            setRangeError(
                t("auto_backup_err_max_keep_out_of_range")
                    .replace("{value}", String(parsed))
                    .replace("{min}", String(MAX_KEEP_MIN))
                    .replace("{max}", String(MAX_KEEP_MAX))
            );
            setMaxKeepDraft(config ? String(config.maxKeep) : String(MAX_KEEP_MIN));
            return;
        }
        if (config && parsed === config.maxKeep) return;
        void saveConfig({ maxKeep: parsed });
    };

    const commitInterval = (rawValue?: string) => {
        const source = (rawValue ?? intervalDraft).trim();
        const parsed = parseInt(source, 10);
        if (!Number.isFinite(parsed)) {
            setIntervalDraft(config ? String(config.intervalMinutes) : "30");
            return;
        }
        if (parsed < INTERVAL_MIN || parsed > INTERVAL_MAX) {
            setRangeError(
                t("auto_backup_err_interval_out_of_range")
                    .replace("{value}", String(parsed))
                    .replace("{min}", String(INTERVAL_MIN))
                    .replace("{max}", String(INTERVAL_MAX))
            );
            setIntervalDraft(config ? String(config.intervalMinutes) : "30");
            return;
        }
        if (config && parsed === config.intervalMinutes) return;
        void saveConfig({ intervalMinutes: parsed });
    };

    /**
     * 数字输入框的样式，逐字对齐「剪贴板设置」里 `persistent_limit` 的那个输入框
     * （`ClipboardSettingsGroup.tsx`），保证两个分组里的数字输入长得一样。
     *
     * 【为什么必须写 fallback】`--border-color` / `--input-bg` / `--text-color` 这三个
     * 变量在本项目的样式表里**从未被定义过**（实测 `grep` 计数为 0）。`var()` 未定义时
     * 整条声明会被丢弃：`border` 变成 0、背景与文字色不再被指定。所以照抄既有写法时
     * 必须把 fallback 一并带上，否则量出来就是"没有边框的裸输入框"。
     */
    const numberInputStyle = {
        width: "90px",
        padding: "4px 8px",
        borderRadius: "4px",
        border: "1px solid var(--border-color, rgba(128,128,128,0.35))",
        background: "var(--input-bg, var(--bg-input))",
        color: "var(--text-color, var(--text-primary))",
        fontSize: "14px",
    } as const;

    return (
        <div className={`settings-group ${collapsed ? "collapsed" : ""}`}>
            <div className="group-header" onClick={onToggle}>
                <h3 style={{ margin: 0 }}>{t("auto_backup_section")}</h3>
                {collapsed ? <ChevronRight size={16} /> : <ChevronDown size={16} />}
            </div>
            {!collapsed && (
                <div className="group-content">
                    {/* 与手动导出备份的边界：先说清楚再列控件。 */}
                    <div
                        style={{
                            fontSize: "11px",
                            color: "var(--text-secondary)",
                            marginBottom: "8px",
                            lineHeight: 1.5,
                        }}
                    >
                        {t("auto_backup_intro")}
                    </div>

                    {/* 1) 定时备份总开关。 */}
                    <div className="setting-item">
                        <LabelWithHint
                            label={t("auto_backup_enabled")}
                            hint={t("auto_backup_enabled_hint")}
                            hintKey="auto_backup_enabled"
                        />
                        <label className="switch">
                            <input
                                className="cb"
                                type="checkbox"
                                checked={config?.enabled ?? false}
                                disabled={busy || !config}
                                onChange={(e) => void saveConfig({ enabled: e.target.checked })}
                            />
                            <div className="toggle">
                                <div className="left" />
                                <div className="right" />
                            </div>
                        </label>
                    </div>

                    {/* 2) 备份周期。受总开关约束——关掉定时备份后周期没有意义。 */}
                    {config?.enabled && (
                        <div className="setting-item">
                            <LabelWithHint
                                label={t("auto_backup_interval")}
                                hint={t("auto_backup_interval_hint")}
                                hintKey="auto_backup_interval"
                            />
                            <input
                                type="number"
                                min={INTERVAL_MIN}
                                max={INTERVAL_MAX}
                                value={intervalDraft}
                                style={numberInputStyle}
                                onFocus={(e) => e.target.select()}
                                onChange={(e) => {
                                    const next = e.target.value;
                                    if (next === "") {
                                        setIntervalDraft("");
                                        return;
                                    }
                                    if (!/^\d+$/.test(next)) return;
                                    setIntervalDraft(next);
                                }}
                                onBlur={() => commitInterval()}
                                onKeyDown={(e) => {
                                    if (e.key === "Enter") {
                                        commitInterval(e.currentTarget.value);
                                        e.currentTarget.blur();
                                    }
                                }}
                            />
                        </div>
                    )}

                    {/* 3) 最大留存份数。1–200；越界由后端报错并回读真实值。 */}
                    <div className="setting-item">
                        <LabelWithHint
                            label={t("auto_backup_max_keep")}
                            hint={t("auto_backup_max_keep_hint")
                                .replace("{min}", String(MAX_KEEP_MIN))
                                .replace("{max}", String(MAX_KEEP_MAX))}
                            hintKey="auto_backup_max_keep"
                        />
                        <input
                            type="number"
                            min={MAX_KEEP_MIN}
                            max={MAX_KEEP_MAX}
                            value={maxKeepDraft}
                            style={numberInputStyle}
                            data-auto-backup-max-keep=""
                            onFocus={(e) => e.target.select()}
                            onChange={(e) => {
                                const next = e.target.value;
                                if (next === "") {
                                    setMaxKeepDraft("");
                                    return;
                                }
                                if (!/^\d+$/.test(next)) return;
                                setMaxKeepDraft(next);
                            }}
                            onBlur={() => commitMaxKeep()}
                            onKeyDown={(e) => {
                                if (e.key === "Enter") {
                                    commitMaxKeep(e.currentTarget.value);
                                    e.currentTarget.blur();
                                }
                            }}
                        />
                    </div>

                    {/*
                      4) 启动时自动备份一次 —— **独立勾选项**。

                      用户原话："不随定时备份开关约束，只是放在里面作为一个勾选项而已"。
                      因此它渲染在 `config.enabled` 的条件之外（关掉上面的定时开关，
                      这一项依然可见可改），并在标签下明写这层关系，避免用户以为
                      "关掉定时备份就万事大吉"。
                    */}
                    <div className="setting-item">
                        <LabelWithHint
                            label={t("auto_backup_on_startup")}
                            hint={t("auto_backup_on_startup_hint")}
                            hintKey="auto_backup_on_startup"
                        />
                        <label className="switch">
                            <input
                                className="cb"
                                type="checkbox"
                                data-auto-backup-on-startup=""
                                checked={config?.backupOnStartup ?? false}
                                disabled={busy || !config}
                                onChange={(e) => void saveConfig({ backupOnStartup: e.target.checked })}
                            />
                            <div className="toggle">
                                <div className="left" />
                                <div className="right" />
                            </div>
                        </label>
                    </div>

                    {/*
                      5) 备份列表入口。

                      用户原话把入口描述成"子菜单点击后弹出悬浮窗"。这里做成同一分组
                      内的一个入口行，而不是再套一层可折叠子菜单：分组本身已经是一次
                      折叠，再嵌一层会变成"点两次才看到内容"，而列表本身还是浮窗、
                      并不真的嵌入在这里。
                    */}
                    <div className="setting-item no-border">
                        <div className="item-label-group">
                            <span className="item-label">{t("auto_backup_list")}</span>
                            <span className="hint">
                                {summary
                                    ? t("auto_backup_summary")
                                          .replace("{total}", String(summary.total))
                                          .replace("{pinned}", String(summary.pinned))
                                          .replace("{size}", formatBytes(summary.bytes))
                                    : t("auto_backup_summary_unknown")}
                            </span>
                        </div>
                        <button
                            type="button"
                            className="btn-icon"
                            data-auto-backup-open-list=""
                            onClick={() => setListOpen(true)}
                            style={{
                                width: "auto",
                                padding: "4px 12px",
                                fontSize: "10px",
                                height: "26px",
                                display: "flex",
                                alignItems: "center",
                                gap: "6px",
                            }}
                        >
                            <List size={12} />
                            {t("auto_backup_list_open")}
                        </button>
                    </div>

                    {/* 越界/失败提示：就摆在被改的控件下方，不弹窗打断输入。 */}
                    {rangeError && (
                        <div
                            data-auto-backup-range-error=""
                            style={{
                                border: "1px solid rgba(200,80,80,0.5)",
                                borderRadius: "6px",
                                padding: "6px 10px",
                                fontSize: "10px",
                                lineHeight: 1.6,
                                marginTop: "6px",
                            }}
                        >
                            {rangeError}
                        </div>
                    )}

                    <div
                        style={{
                            fontSize: "10px",
                            color: "var(--text-secondary)",
                            opacity: 0.75,
                            lineHeight: 1.5,
                            marginTop: "6px",
                        }}
                    >
                        {t("auto_backup_pin_note")}
                    </div>
                </div>
            )}

            <BackupListModal
                open={listOpen}
                t={t}
                theme={theme}
                onClose={() => setListOpen(false)}
                onLoaded={(payload) => {
                    applyConfig(payload.config);
                    setSummary({
                        total: payload.totalCount,
                        pinned: payload.pinnedCount,
                        bytes: payload.entries.reduce((sum, e) => sum + e.sizeBytes, 0),
                    });
                    // 固定数/份数可能刚被用户在列表里改过，配置也一并同步回来。
                    setRangeError(null);
                }}
            />
        </div>
    );
};

export default AutoBackupSettingsGroup;
