import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
    ChevronDown,
    ChevronRight,
    Copy,
    Globe,
    Lock,
    RefreshCw,
    ServerOff,
    ShieldAlert,
    Activity,
    X as XIcon,
} from "lucide-react";

/** 后端 `get_mcp_status` 的返回形状（camelCase）。 */
interface McpStatus {
    running: boolean;
    port: number;
    enabled: boolean;
    allowWrite: boolean;
    autostart: boolean;
    /** 是否强制校验令牌。`false` = 免鉴权（出厂默认）。 */
    requireToken: boolean;
    /** 是否允许局域网访问。`false` = 仅本机（出厂默认）。 */
    allowLan: boolean;
    token: string;
    /** 本机入口；无论是否开放局域网都可用。仅在运行中非空。 */
    endpoint: string;
    /** 局域网入口；仅"运行中 + 已开放局域网"时非空。 */
    lanEndpoint: string;
    defaultPort: number;
    defaultEnabled: boolean;
    defaultAllowWrite: boolean;
    defaultRequireToken: boolean;
    defaultAllowLan: boolean;
}

interface McpSettingsGroupProps {
    t: (key: string) => string;
    collapsed: boolean;
    onToggle: () => void;
}

/** 后端 `inspect_mcp_port_occupancy` 返回的进程信息。 */
interface PortProcess {
    pid: number;
    processName: string;
    executablePath: string | null;
    localAddress: string;
    state: string;
    canTerminate: boolean;
    isCurrentProcess: boolean;
}

/** 后端 `stop_mcp_port_process` / `stop_mcp_port_process_as_admin` 的返回。 */
interface StopProcessResult {
    stopped: boolean;
    launchedElevated: boolean;
    requiresAdmin: boolean;
    message: string;
}

/**
 * 与相邻分组共用的内联样式片段。
 *
 * 取值刻意与 `DataSettingsGroup` / `CloudSyncSettingsGroup` 对齐：说明文字 10–11px、
 * 区块标题 11px + uppercase、分隔线用 `--border-color`、控件用 `--search-input` 与
 * `btn-icon`，危险色用 `var(--danger-color, #c05050)`。此处只做复用，不引入新视觉规则。
 */
const STYLES = {
    /** 区块小标题：与 DataSettingsGroup 的 `data_path` / `backup_section` 同款。 */
    sectionTitle: {
        textTransform: "uppercase",
        fontSize: "11px",
        opacity: 0.8,
    },
    /** 二级说明文字：与 DataSettingsGroup 的按钮下方提示同款。 */
    subNote: {
        fontSize: "10px",
        color: "var(--text-secondary)",
        opacity: 0.85,
        lineHeight: 1.5,
    },
    /** 徽标（默认 / 需重启 等）。 */
    badge: {
        fontSize: "10px",
        lineHeight: 1,
        padding: "3px 6px",
        borderRadius: "4px",
        border: "1px solid var(--border-color, rgba(128,128,128,0.25))",
        color: "var(--text-secondary)",
        whiteSpace: "nowrap" as const,
        flexShrink: 0,
    },
    /** 提示框：沿用 DataSettingsGroup 结果框的圆角/内边距/行高。 */
    callout: {
        borderRadius: "6px",
        padding: "8px 10px",
        fontSize: "10px",
        lineHeight: 1.6,
        wordBreak: "break-all" as const,
    },
    /** 小尺寸文字按钮：与 DataSettingsGroup 的操作按钮尺寸一致。 */
    actionButton: {
        width: "auto",
        padding: "4px 12px",
        fontSize: "10px",
        height: "24px",
        display: "flex",
        alignItems: "center",
        gap: "6px",
    },
    iconButton: {
        width: "auto",
        padding: "4px 8px",
        height: "24px",
    },
} as const;

const DANGER_COLOR = "var(--danger-color, #c05050)";

/**
 * MCP 服务设置。
 *
 * 设计要点（与后端契约一致）：
 * - 出厂姿态是"开箱即用但只监听本机"：服务开、可写、免鉴权、固定端口 23123，且
 *   **仅绑回环**。免鉴权只有在"外部机器根本连不上"时才成立，因此开放局域网是这一组
 *   设置里风险最高的一项，界面必须把它讲清楚，而不是等用户自己去推断。
 * - 不同开关生效方式不同：令牌校验与写权限即时生效；监听地址在 bind 时定下，改局域网
 *   开关必须重启服务——界面上用徽标区分，不能让用户以为点完就生效了。
 * - 令牌由后端生成，界面只负责展示与复制；免鉴权模式下它依然存在，用户随时可以打开
 *   校验而不必重新生成，所以这里不隐藏令牌，只说明"当前未校验"。
 */
const McpSettingsGroup = ({ t, collapsed, onToggle }: McpSettingsGroupProps) => {
    const [status, setStatus] = useState<McpStatus | null>(null);
    const [configuredPort, setConfiguredPort] = useState<number | null>(null);
    const [portInput, setPortInput] = useState("");
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState("");
    const [copied, setCopied] = useState<"endpoint" | "lanEndpoint" | "token" | null>(null);
    /** 局域网开关触发重启后返回的端口，用于给用户一个明确的"已生效"回执。 */
    const [restartedPort, setRestartedPort] = useState<number | null>(null);

    // --- 端口占用处理器（仅 Windows）---
    const [portModalOpen, setPortModalOpen] = useState(false);
    const [portProcesses, setPortProcesses] = useState<PortProcess[]>([]);
    const [portModalPort, setPortModalPort] = useState("");
    const [portModalBusy, setPortModalBusy] = useState(false);
    const [portModalError, setPortModalError] = useState("");
    const [portModalInfo, setPortModalInfo] = useState("");
    const [adminProcess, setAdminProcess] = useState<PortProcess | null>(null);
    const portOperationInFlightRef = useRef(false);

    const isWindows = /Windows/i.test(navigator.userAgent);

    const inspectPort = useCallback(async (port: number, clearFeedback = true, nested = false): Promise<PortProcess[] | null> => {
        if (!nested && portOperationInFlightRef.current) return null;
        if (!nested) portOperationInFlightRef.current = true;
        setPortModalBusy(true);
        setPortModalError("");
        if (clearFeedback) setPortModalInfo("");
        try {
            const result = await invoke<PortProcess[]>("inspect_mcp_port_occupancy", { port });
            setPortProcesses(result);
            if (clearFeedback && result.length === 0) {
                setPortModalInfo(t("mcp_port_free").replace("{port}", String(port)));
            }
            return result;
        } catch (e) {
            setPortModalError(String(e));
            return null;
        } finally {
            setPortModalBusy(false);
            if (!nested) portOperationInFlightRef.current = false;
        }
    }, [t]);

    const stopProcess = useCallback(async (pid: number) => {
        if (portOperationInFlightRef.current) return;
        portOperationInFlightRef.current = true;
        const process = portProcesses.find((item) => item.pid === pid);
        const processLabel = process?.processName || `PID ${pid}`;
        const port = Number.parseInt(portModalPort, 10);
        setPortModalBusy(true);
        setPortModalError("");
        setPortModalInfo(
            t("mcp_port_stopping")
                .replace("{process}", processLabel)
                .replace("{pid}", String(pid)),
        );
        try {
            const result = await invoke<StopProcessResult>("stop_mcp_port_process", { pid });
            if (!result.stopped && result.requiresAdmin) {
                setAdminProcess(process ?? {
                    pid,
                    processName: processLabel,
                    executablePath: null,
                    localAddress: "",
                    state: "",
                    canTerminate: false,
                    isCurrentProcess: false,
                });
                setPortModalInfo(
                    t("mcp_port_admin_needed")
                        .replace("{process}", processLabel)
                        .replace("{pid}", String(pid)),
                );
                return;
            }
            if (!result.stopped) {
                setPortModalError(result.message);
                return;
            }
            const latest = await inspectPort(port, false, true);
            if (latest && !latest.some((item) => item.pid === pid)) {
                setPortModalInfo(
                    t("mcp_port_stopped")
                        .replace("{process}", processLabel)
                        .replace("{pid}", String(pid))
                        .replace("{port}", String(port)),
                );
            } else if (latest) {
                setPortModalError(
                    t("mcp_port_still_occupied")
                        .replace("{process}", processLabel)
                        .replace("{port}", String(port)),
                );
            }
        } catch (e) {
            setPortModalError(String(e));
        } finally {
            portOperationInFlightRef.current = false;
            setPortModalBusy(false);
        }
    }, [portProcesses, portModalPort, inspectPort, t]);

    const confirmAdminStop = useCallback(async () => {
        const process = adminProcess;
        if (!process || portOperationInFlightRef.current) return;
        portOperationInFlightRef.current = true;
        const port = Number.parseInt(portModalPort, 10);
        setAdminProcess(null);
        setPortModalBusy(true);
        setPortModalError("");
        let waitingForVerification = false;
        try {
            const elevated = await invoke<StopProcessResult>("stop_mcp_port_process_as_admin", { pid: process.pid });
            if (!elevated.launchedElevated) {
                setPortModalError(elevated.message);
                return;
            }
            waitingForVerification = true;
            setPortModalInfo(
                t("mcp_port_admin_request")
                    .replace("{process}", process.processName)
                    .replace("{pid}", String(process.pid)),
            );
            window.setTimeout(() => {
                void (async () => {
                    try {
                        const latest = await inspectPort(port, false, true);
                        if (latest && !latest.some((item) => item.pid === process.pid)) {
                            setPortModalInfo(
                                t("mcp_port_stopped")
                                    .replace("{process}", process.processName)
                                    .replace("{pid}", String(process.pid))
                                    .replace("{port}", String(port)),
                            );
                        } else if (latest) {
                            setPortModalError(
                                t("mcp_port_still_occupied")
                                    .replace("{process}", process.processName)
                                    .replace("{port}", String(port)),
                            );
                        }
                    } finally {
                        portOperationInFlightRef.current = false;
                        setPortModalBusy(false);
                    }
                })();
            }, 900);
        } catch (e) {
            setPortModalError(String(e));
        } finally {
            if (!waitingForVerification) {
                portOperationInFlightRef.current = false;
                setPortModalBusy(false);
            }
        }
    }, [adminProcess, portModalPort, inspectPort, t]);

    const refreshPortModal = useCallback(() => {
        const p = Number.parseInt(portModalPort, 10);
        if (!Number.isNaN(p) && p > 0) void inspectPort(p);
    }, [portModalPort, inspectPort]);

    const refresh = useCallback(async () => {
        try {
            const next = await invoke<McpStatus>("get_mcp_status");
            setStatus(next);
            // 服务停止时后端返回 port=0，此时"当前端口"要回到已保存的配置值，
            // 否则重启后用户就看不到自己设过的端口了。
            if (next.running) {
                setConfiguredPort(next.port);
                setPortInput(String(next.port));
            } else {
                let saved: number | null = null;
                try {
                    const settings = await invoke<Record<string, string>>("get_settings");
                    const raw = Number.parseInt(settings?.["mcp.port"] ?? "", 10);
                    if (!Number.isNaN(raw)) saved = raw;
                } catch {
                    // 读不到就退回默认值展示，不影响主流程
                }
                const shown = saved ?? next.defaultPort;
                setConfiguredPort(shown);
                setPortInput(String(shown));
            }
            setError("");
        } catch (e) {
            setError(String(e));
        }
    }, []);

    useEffect(() => {
        void refresh();
    }, [refresh]);

    const run = async (fn: () => Promise<void>) => {
        setBusy(true);
        setError("");
        try {
            await fn();
            await refresh();
        } catch (e) {
            setError(String(e));
        } finally {
            setBusy(false);
        }
    };

    const copy = async (kind: "endpoint" | "lanEndpoint" | "token", value: string) => {
        if (!value) return;
        try {
            await navigator.clipboard.writeText(value);
            setCopied(kind);
            window.setTimeout(() => setCopied(null), 1500);
        } catch (e) {
            setError(String(e));
        }
    };

    const toggleEnabled = (next: boolean) =>
        run(async () => {
            await invoke<number>("set_mcp_server_enabled", { enabled: next });
        });

    /** 即时生效：只改运行时开关，不重启服务。 */
    const toggleRequireToken = (next: boolean) =>
        run(async () => {
            await invoke("set_mcp_require_token", { require: next });
        });

    /** 即时生效：立刻收回/放开写权限。 */
    const toggleWrite = (next: boolean) =>
        run(async () => {
            await invoke("set_mcp_allow_write", { allow: next });
        });

    /** 需要重启服务：监听地址在 bind 时定下，运行中改不了。 */
    const toggleAllowLan = (next: boolean) =>
        run(async () => {
            const port = await invoke<number>("set_mcp_allow_lan", { allow: next });
            setRestartedPort(port > 0 ? port : null);
        });

    const toggleAutostart = (next: boolean) =>
        run(async () => {
            await invoke("set_mcp_autostart", { autostart: next });
        });

    const applyPort = () =>
        run(async () => {
            const parsed = Number.parseInt(portInput, 10);
            if (Number.isNaN(parsed) || parsed < 1024 || parsed > 65535) {
                throw new Error(t("mcp_port_invalid"));
            }
            await invoke<number>("set_mcp_port", { port: parsed });
        });

    const regenerate = () =>
        run(async () => {
            await invoke<string>("regenerate_mcp_token");
        });

    const resetPortInput = () => {
        if (!status) return;
        setPortInput(String(configuredPort ?? status.defaultPort));
    };

    const enabled = status?.enabled ?? false;
    const running = status?.running ?? false;
    const allowLan = status?.allowLan ?? false;
    const requireToken = status?.requireToken ?? false;
    const currentPort = configuredPort ?? status?.port ?? 0;
    const defaultPort = status?.defaultPort ?? 23123;
    const portIsDefault = currentPort === defaultPort;

    const openPortModal = useCallback(() => {
        const p = configuredPort ?? defaultPort;
        setPortModalPort(String(p));
        setPortModalOpen(true);
        setPortProcesses([]);
        setPortModalError("");
        setPortModalInfo("");
        void inspectPort(p);
    }, [configuredPort, defaultPort, inspectPort]);
    /** 高风险组合：已开放局域网、却不要令牌。两者正交，因此只提示、不联动。 */
    const riskyCombo = allowLan && !requireToken;

    return (
        <div className={`settings-group ${collapsed ? "collapsed" : ""}`}>
            <div className="group-header" onClick={onToggle}>
                <h3 style={{ margin: 0 }}>{t("mcp_service")}</h3>
                {collapsed ? <ChevronRight size={16} /> : <ChevronDown size={16} />}
            </div>

            {!collapsed && (
                <div className="group-content">
                    <p className="settings-subpage-note">{t("mcp_service_desc")}</p>

                    {/* 当前暴露面：用户最该一眼看懂的两件事——能从哪里连进来、连进来要不要令牌 */}
                    <div className="setting-item column no-border">
                        <div
                            style={{
                                display: "flex",
                                justifyContent: "space-between",
                                alignItems: "center",
                                marginBottom: "8px",
                            }}
                        >
                            <span className="item-label" style={STYLES.sectionTitle}>
                                {t("mcp_exposure_title")}
                            </span>
                            <div style={{ display: "flex", gap: "6px", alignItems: "center", flexShrink: 0 }}>
                                <span
                                    style={{
                                        ...STYLES.badge,
                                        color: running ? "var(--text-primary)" : "var(--text-secondary)",
                                        borderColor: running
                                            ? "rgba(64,160,96,0.5)"
                                            : "var(--border-color, rgba(128,128,128,0.25))",
                                        display: "flex",
                                        alignItems: "center",
                                        gap: "4px",
                                    }}
                                >
                                    <ServerOff size={10} style={{ display: running ? "none" : "block" }} />
                                    {running ? t("mcp_status_running") : t("mcp_status_stopped")}
                                </span>
                                <span
                                    style={{
                                        ...STYLES.badge,
                                        display: "flex",
                                        alignItems: "center",
                                        gap: "4px",
                                        color: allowLan ? DANGER_COLOR : "var(--text-secondary)",
                                        borderColor: allowLan
                                            ? "rgba(200,80,80,0.5)"
                                            : "var(--border-color, rgba(128,128,128,0.25))",
                                    }}
                                >
                                    <Globe size={10} />
                                    {allowLan ? t("mcp_exposure_lan") : t("mcp_exposure_local")}
                                </span>
                                <span
                                    style={{
                                        ...STYLES.badge,
                                        display: "flex",
                                        alignItems: "center",
                                        gap: "4px",
                                        color: requireToken ? "var(--text-primary)" : DANGER_COLOR,
                                        borderColor: requireToken
                                            ? "var(--border-color, rgba(128,128,128,0.25))"
                                            : "rgba(200,80,80,0.5)",
                                    }}
                                >
                                    <Lock size={10} />
                                    {requireToken ? t("mcp_exposure_auth_on") : t("mcp_exposure_auth_off")}
                                </span>
                            </div>
                        </div>

                        {/* 本机端点：始终可用，因此永远显示；复制按钮沿用相邻分组的 btn-icon */}
                        <div style={{ display: "flex", gap: "6px", alignItems: "center", marginBottom: "6px" }}>
                            <span style={{ ...STYLES.subNote, flexShrink: 0 }}>{t("mcp_endpoint_local")}</span>
                            <div
                                className="data-panel"
                                style={{ fontSize: "11px", flex: 1, minWidth: 0 }}
                                title={status?.endpoint || "-"}
                            >
                                {running && status?.endpoint ? status.endpoint : "-"}
                            </div>
                            <button
                                type="button"
                                className="btn-icon"
                                disabled={!running || !status?.endpoint}
                                onClick={() => void copy("endpoint", status?.endpoint ?? "")}
                                title={t("mcp_copy")}
                                style={STYLES.iconButton}
                            >
                                <Copy size={12} />
                            </button>
                        </div>

                        {/* 局域网端点：只在真的对外开放且服务在跑时才有值，没有就不编一个出来 */}
                        {running && allowLan && status?.lanEndpoint && (
                            <div style={{ display: "flex", gap: "6px", alignItems: "center", marginBottom: "6px" }}>
                                <span style={{ ...STYLES.subNote, flexShrink: 0 }}>{t("mcp_endpoint_lan")}</span>
                                <div
                                    className="data-panel"
                                    style={{ fontSize: "11px", flex: 1, minWidth: 0, color: DANGER_COLOR }}
                                    title={status.lanEndpoint}
                                >
                                    {status.lanEndpoint}
                                </div>
                                <button
                                    type="button"
                                    className="btn-icon"
                                    onClick={() => void copy("lanEndpoint", status.lanEndpoint)}
                                    title={t("mcp_copy")}
                                    style={STYLES.iconButton}
                                >
                                    <Copy size={12} />
                                </button>
                            </div>
                        )}

                        {restartedPort !== null && (
                            <div style={{ ...STYLES.subNote, marginBottom: "6px" }}>
                                {t("mcp_lan_restarted").replace("{port}", String(restartedPort))}
                            </div>
                        )}
                    </div>

                    {/* 安全提示：开放局域网是这一组设置里唯一会扩大攻击面的一项，必须显著 */}
                    {allowLan && (
                        <div
                            style={{
                                ...STYLES.callout,
                                border: `1px solid ${riskyCombo ? "rgba(200,80,80,0.5)" : "rgba(217,119,6,0.5)"}`,
                                marginTop: "8px",
                                display: "flex",
                                gap: "8px",
                                alignItems: "flex-start",
                            }}
                        >
                            <ShieldAlert size={14} style={{ color: DANGER_COLOR, flexShrink: 0, marginTop: "1px" }} />
                            <div style={{ minWidth: 0 }}>
                                <div style={{ fontWeight: 600, marginBottom: "4px", color: DANGER_COLOR }}>
                                    {t("mcp_lan_warning_title")}
                                </div>
                                <div>{t("mcp_lan_warning")}</div>
                                {riskyCombo && (
                                    <div style={{ marginTop: "4px" }}>{t("mcp_lan_suggest_token")}</div>
                                )}
                            </div>
                        </div>
                    )}

                    {/* 免鉴权只在"仅本机"下成立——这是出厂姿态能安全的原因，必须写出来 */}
                    {!requireToken && !allowLan && (
                        <div style={{ ...STYLES.callout, border: "1px solid var(--border-color, rgba(128,128,128,0.25))", marginTop: "8px" }}>
                            {t("mcp_noauth_warning")}
                        </div>
                    )}

                    <div className="setting-item">
                        <div className="item-label-group">
                            <span className="item-label">{t("mcp_enable")}</span>
                            <span className="hint">{t("mcp_enable_hint")}</span>
                        </div>
                        <label className="switch">
                            <input
                                className="cb"
                                type="checkbox"
                                checked={enabled}
                                disabled={busy}
                                onChange={(e) => void toggleEnabled(e.target.checked)}
                            />
                            <div className="toggle"><div className="left" /><div className="right" /></div>
                        </label>
                    </div>

                    <div className="setting-item">
                        <div className="item-label-group">
                            <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
                                <span className="item-label">{t("mcp_require_token")}</span>
                                <span style={STYLES.badge}>{t("mcp_instant_badge")}</span>
                            </div>
                            <span className="hint">{t("mcp_require_token_hint")}</span>
                        </div>
                        <label className="switch">
                            <input
                                className="cb"
                                type="checkbox"
                                checked={requireToken}
                                disabled={busy || !enabled}
                                onChange={(e) => void toggleRequireToken(e.target.checked)}
                            />
                            <div className="toggle"><div className="left" /><div className="right" /></div>
                        </label>
                    </div>

                    <div className="setting-item">
                        <div className="item-label-group">
                            <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
                                <span className="item-label">{t("mcp_allow_lan")}</span>
                                <span style={{ ...STYLES.badge, color: DANGER_COLOR, borderColor: "rgba(200,80,80,0.5)" }}>
                                    {t("mcp_restart_badge")}
                                </span>
                            </div>
                            <span className="hint">{t("mcp_allow_lan_hint")}</span>
                        </div>
                        <label className="switch">
                            <input
                                className="cb"
                                type="checkbox"
                                checked={allowLan}
                                disabled={busy || !enabled}
                                onChange={(e) => void toggleAllowLan(e.target.checked)}
                            />
                            <div className="toggle"><div className="left" /><div className="right" /></div>
                        </label>
                    </div>

                    <div className="setting-item">
                        <div className="item-label-group">
                            <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
                                <span className="item-label">{t("mcp_allow_write")}</span>
                                <span style={STYLES.badge}>{t("mcp_instant_badge")}</span>
                            </div>
                            <span className="hint">{t("mcp_allow_write_hint")}</span>
                        </div>
                        <label className="switch">
                            <input
                                className="cb"
                                type="checkbox"
                                checked={status?.allowWrite ?? false}
                                disabled={busy || !enabled}
                                onChange={(e) => void toggleWrite(e.target.checked)}
                            />
                            <div className="toggle"><div className="left" /><div className="right" /></div>
                        </label>
                    </div>

                    <div className="setting-item">
                        <div className="item-label-group">
                            <span className="item-label">{t("mcp_autostart")}</span>
                            <span className="hint">{t("mcp_autostart_hint")}</span>
                        </div>
                        <label className="switch">
                            <input
                                className="cb"
                                type="checkbox"
                                checked={status?.autostart ?? false}
                                disabled={busy}
                                onChange={(e) => void toggleAutostart(e.target.checked)}
                            />
                            <div className="toggle"><div className="left" /><div className="right" /></div>
                        </label>
                    </div>

                    <div className="setting-item column no-border">
                        <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", gap: "8px" }}>
                            <div style={{ display: "flex", alignItems: "center", gap: "6px", minWidth: 0 }}>
                                <span className="item-label">{t("mcp_port")}</span>
                                {portIsDefault && <span style={STYLES.badge}>{t("mcp_default_badge")}</span>}
                            </div>
                            <div style={{ display: "flex", gap: "6px", alignItems: "center", flexShrink: 0 }}>
                                <input
                                    className="search-input window-no-drag"
                                    inputMode="numeric"
                                    value={portInput}
                                    disabled={busy}
                                    onMouseDown={(e) => e.stopPropagation()}
                                    onFocus={(e) => e.currentTarget.select()}
                                    onChange={(e) => setPortInput(e.target.value.replace(/[^0-9]/g, ""))}
                                    style={{ borderRadius: "4px", padding: "4px 8px", width: "84px", textAlign: "right" }}
                                />
                                <button
                                    type="button"
                                    className="btn-icon"
                                    disabled={busy}
                                    onClick={() => void applyPort()}
                                    style={STYLES.actionButton}
                                >
                                    {t("mcp_apply")}
                                </button>
                                <button
                                    type="button"
                                    className="btn-icon"
                                    disabled={busy || portInput === String(currentPort)}
                                    onClick={resetPortInput}
                                    title={t("mcp_port_reset_hint")}
                                    style={STYLES.iconButton}
                                >
                                    <RefreshCw size={12} />
                                </button>
                            </div>
                        </div>
                        <div style={STYLES.subNote}>
                            {t("mcp_port_default_note")
                                .replace("{default}", String(defaultPort))
                                .replace("{current}", String(currentPort))}
                        </div>
                        <div style={STYLES.subNote}>{t("mcp_port_hint")}</div>
                        {isWindows && (
                            <div style={{ marginTop: "6px" }}>
                                <button
                                    type="button"
                                    className="btn-icon"
                                    onClick={() => openPortModal()}
                                    style={STYLES.actionButton}
                                    title={t("mcp_port_resolve_hint") || "检测并停止端口占用进程"}
                                >
                                    <Activity size={12} />
                                    {t("mcp_port_resolve") || "端口占用处理"}
                                </button>
                            </div>
                        )}
                    </div>

                    <div className="setting-item column no-border">
                        <div style={{ display: "flex", alignItems: "center", gap: "6px", marginBottom: "6px" }}>
                            <span className="item-label">{t("mcp_token")}</span>
                            <span
                                style={{
                                    ...STYLES.badge,
                                    color: requireToken ? "var(--text-primary)" : DANGER_COLOR,
                                    borderColor: requireToken
                                        ? "var(--border-color, rgba(128,128,128,0.25))"
                                        : "rgba(200,80,80,0.5)",
                                }}
                            >
                                {requireToken ? t("mcp_exposure_auth_on") : t("mcp_exposure_auth_off")}
                            </span>
                        </div>
                        <div style={{ display: "flex", gap: "6px", alignItems: "center" }}>
                            <input
                                className="search-input"
                                type="text"
                                readOnly
                                value={status?.token ?? ""}
                                onFocus={(e) => e.currentTarget.select()}
                                style={{
                                    borderRadius: "4px",
                                    padding: "4px 8px",
                                    flex: 1,
                                    minWidth: 0,
                                    fontFamily: "monospace",
                                    fontSize: "11px",
                                }}
                            />
                            <button
                                type="button"
                                className="btn-icon"
                                onClick={() => void copy("token", status?.token ?? "")}
                                style={STYLES.actionButton}
                            >
                                <Copy size={12} />
                                {copied === "token" ? t("mcp_copied") : t("mcp_copy")}
                            </button>
                            <span
                                aria-hidden="true"
                                style={{ width: "1px", height: "16px", background: "var(--border-color, rgba(128,128,128,0.3))" }}
                            />
                            <button
                                type="button"
                                className="btn-icon"
                                disabled={busy}
                                onClick={() => void regenerate()}
                                title={t("mcp_regenerate_hint")}
                                style={STYLES.actionButton}
                            >
                                <RefreshCw size={12} />
                                {t("mcp_regenerate")}
                            </button>
                        </div>
                        <div style={STYLES.subNote}>
                            {requireToken ? t("mcp_token_hint") : t("mcp_token_idle_hint")}
                        </div>
                    </div>

                    <p className="settings-subpage-note">{t("mcp_client_hint")}</p>
                    {running && !status?.allowWrite && (
                        <p className="settings-subpage-note">{t("mcp_readonly_notice")}</p>
                    )}
                    {error && <p className="settings-subpage-note" style={{ color: DANGER_COLOR }}>{error}</p>}
                </div>
            )}

            {/* 端口占用处理悬浮子页面（仅 Windows）*/}
            {portModalOpen && (
                <div className="modal-overlay" onClick={() => setPortModalOpen(false)} style={{ zIndex: 3400 }}>
                    <div className="modal-content" onClick={(e) => e.stopPropagation()} style={{ maxWidth: "460px" }}>
                        <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: "12px" }}>
                            <h3 className="modal-title">{t("mcp_port_resolve") || "端口占用处理"}</h3>
                            <button
                                type="button"
                                className="btn-icon"
                                onClick={() => setPortModalOpen(false)}
                                title={t("cancel") || "关闭"}
                                style={STYLES.iconButton}
                            >
                                <XIcon size={14} />
                            </button>
                        </div>

                        <div style={{ display: "flex", gap: "6px", alignItems: "center", marginBottom: "10px" }}>
                            <input
                                className="search-input window-no-drag"
                                inputMode="numeric"
                                value={portModalPort}
                                disabled={portModalBusy}
                                onMouseDown={(e) => e.stopPropagation()}
                                onFocus={(e) => e.currentTarget.select()}
                                onChange={(e) => setPortModalPort(e.target.value.replace(/[^0-9]/g, ""))}
                                style={{ borderRadius: "4px", padding: "4px 8px", width: "84px", textAlign: "right" }}
                            />
                            <button
                                type="button"
                                className="btn-icon"
                                disabled={portModalBusy}
                                onClick={() => refreshPortModal()}
                                style={STYLES.actionButton}
                            >
                                <RefreshCw size={12} />
                                {t("mcp_port_check") || "检测"}
                            </button>
                        </div>

                        {portModalBusy && <div style={STYLES.subNote}>正在检测…</div>}
                        {portModalInfo && <div style={{ ...STYLES.subNote, marginBottom: "8px" }}>{portModalInfo}</div>}
                        {portModalError && <div style={{ ...STYLES.subNote, color: DANGER_COLOR, marginBottom: "8px" }}>{portModalError}</div>}

                        {portProcesses.length > 0 && (
                            <div style={{ maxHeight: "240px", overflowY: "auto", borderTop: "1px solid var(--border-color, rgba(128,128,128,0.2))", marginTop: "8px" }}>
                                {portProcesses.map((proc) => (
                                    <div
                                        key={`${proc.pid}-${proc.localAddress}`}
                                        style={{
                                            display: "flex",
                                            alignItems: "center",
                                            justifyContent: "space-between",
                                            padding: "8px 0",
                                            borderBottom: "1px solid var(--border-color, rgba(128,128,128,0.12))",
                                            gap: "8px",
                                        }}
                                    >
                                        <div style={{ minWidth: 0, flex: 1 }}>
                                            <div style={{ fontSize: "12px", fontWeight: 500, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
                                                {proc.processName}
                                                <span style={{ opacity: 0.6 }}>（PID {proc.pid}）</span>
                                                {proc.isCurrentProcess && (
                                                    <span
                                                        style={{
                                                            display: "inline-block",
                                                            marginLeft: "6px",
                                                            padding: "1px 5px",
                                                            borderRadius: "4px",
                                                            color: "var(--accent-color)",
                                                            border: "1px solid var(--accent-color)",
                                                            fontSize: "10px",
                                                            fontWeight: 600,
                                                        }}
                                                    >
                                                        本 APP 自身
                                                    </span>
                                                )}
                                            </div>
                                            {proc.executablePath && (
                                                <div style={STYLES.subNote}>
                                                    {proc.executablePath}
                                                </div>
                                            )}
                                            <div style={STYLES.subNote}>
                                                {proc.localAddress} · {proc.state}
                                                {!proc.canTerminate && !proc.isCurrentProcess && " · 进程已退出或无法访问"}
                                            </div>
                                        </div>
                                        <button
                                            type="button"
                                            className="btn-icon"
                                            disabled={portModalBusy || proc.isCurrentProcess || !proc.canTerminate}
                                            onClick={() => void stopProcess(proc.pid)}
                                            style={{
                                                ...STYLES.actionButton,
                                                color: proc.isCurrentProcess ? "var(--text-secondary)" : DANGER_COLOR,
                                                flexShrink: 0,
                                            }}
                                            title={proc.isCurrentProcess ? (t("mcp_port_self_skip") || "不能停止当前应用") : (t("mcp_port_stop") || "停止进程")}
                                        >
                                            {t("mcp_port_stop") || "停止"}
                                        </button>
                                    </div>
                                ))}
                            </div>
                        )}

                        <div style={{ ...STYLES.subNote, marginTop: "10px" }}>
                            {t("mcp_port_resolve_desc") || "输入端口号检测占用进程，点击停止可结束对应进程。权限不足时会请求管理员权限。"}
                        </div>
                    </div>
                </div>
            )}

            {adminProcess && (
                <div className="modal-overlay" onClick={() => setAdminProcess(null)} style={{ zIndex: 3500 }}>
                    <div className="confirm-dialog" onClick={(e) => e.stopPropagation()}>
                        <h3 className="modal-title">需要管理员权限</h3>
                        <p style={{ fontSize: "12px", lineHeight: 1.6 }}>
                            {t("mcp_port_admin_needed")
                                .replace("{process}", adminProcess.processName)
                                .replace("{pid}", String(adminProcess.pid))}
                        </p>
                        <div style={{ display: "flex", justifyContent: "flex-end", gap: "8px", marginTop: "14px" }}>
                            <button type="button" className="btn-icon" onClick={() => setAdminProcess(null)}>
                                取消
                            </button>
                            <button type="button" className="btn-icon" onClick={() => void confirmAdminStop()} style={{ color: DANGER_COLOR }}>
                                请求管理员权限
                            </button>
                        </div>
                    </div>
                </div>
            )}
        </div>
    );
};

export default McpSettingsGroup;
