import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ChevronDown, ChevronRight, Copy, RefreshCw } from "lucide-react";

/** 后端 `get_mcp_status` 的返回形状。 */
interface McpStatus {
    running: boolean;
    port: number;
    enabled: boolean;
    allowWrite: boolean;
    autostart: boolean;
    token: string;
    endpoint: string;
}

interface McpSettingsGroupProps {
    t: (key: string) => string;
    collapsed: boolean;
    onToggle: () => void;
}

/**
 * MCP 服务设置。
 *
 * 设计要点（与后端一致）：
 * - 默认 **关闭**、默认 **只读**：两件事都必须由用户显式打开；
 * - 令牌由后端生成，界面只负责展示与复制，不参与生成；
 * - 界面永远显示"现在到底能不能写"，避免用户以为已经开了。
 */
const McpSettingsGroup = ({ t, collapsed, onToggle }: McpSettingsGroupProps) => {
    const [status, setStatus] = useState<McpStatus | null>(null);
    const [portInput, setPortInput] = useState("");
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState("");
    const [copied, setCopied] = useState(false);

    const refresh = useCallback(async () => {
        try {
            const next = await invoke<McpStatus>("get_mcp_status");
            setStatus(next);
            setPortInput(String(next.port || next.port === 0 ? next.port || "" : ""));
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

    const toggleEnabled = (next: boolean) =>
        run(async () => {
            await invoke<number>("set_mcp_server_enabled", { enabled: next });
        });

    const toggleWrite = (next: boolean) =>
        run(async () => {
            await invoke("set_mcp_allow_write", { allow: next });
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

    const copyToken = async () => {
        if (!status) return;
        try {
            await navigator.clipboard.writeText(status.token);
            setCopied(true);
            window.setTimeout(() => setCopied(false), 1500);
        } catch (e) {
            setError(String(e));
        }
    };

    return (
        <div className={`settings-group ${collapsed ? "collapsed" : ""}`}>
            <button type="button" className="group-header" onClick={onToggle}>
                <h3>{t("mcp_service")}</h3>
                {collapsed ? <ChevronRight size={16} /> : <ChevronDown size={16} />}
            </button>

            {!collapsed && (
                <div className="group-content">
                    <p className="settings-subpage-note">{t("mcp_service_desc")}</p>

                    <div className="setting-row">
                        <div className="setting-label">
                            <span>{t("mcp_enable")}</span>
                            <small>{t("mcp_enable_hint")}</small>
                        </div>
                        <label className="switch">
                            <input
                                type="checkbox"
                                checked={status?.enabled ?? false}
                                disabled={busy}
                                onChange={(e) => void toggleEnabled(e.target.checked)}
                            />
                            <span className="slider" />
                        </label>
                    </div>

                    <div className="setting-row">
                        <div className="setting-label">
                            <span>{t("mcp_allow_write")}</span>
                            <small>{t("mcp_allow_write_hint")}</small>
                        </div>
                        <label className="switch">
                            <input
                                type="checkbox"
                                checked={status?.allowWrite ?? false}
                                disabled={busy || !status?.enabled}
                                onChange={(e) => void toggleWrite(e.target.checked)}
                            />
                            <span className="slider" />
                        </label>
                    </div>

                    <div className="setting-row">
                        <div className="setting-label">
                            <span>{t("mcp_autostart")}</span>
                            <small>{t("mcp_autostart_hint")}</small>
                        </div>
                        <label className="switch">
                            <input
                                type="checkbox"
                                checked={status?.autostart ?? false}
                                disabled={busy}
                                onChange={(e) => void toggleAutostart(e.target.checked)}
                            />
                            <span className="slider" />
                        </label>
                    </div>

                    <div className="setting-row">
                        <div className="setting-label">
                            <span>{t("mcp_port")}</span>
                            <small>{t("mcp_port_hint")}</small>
                        </div>
                        <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
                            <input
                                type="text"
                                inputMode="numeric"
                                value={portInput}
                                disabled={busy}
                                onChange={(e) => setPortInput(e.target.value.replace(/[^0-9]/g, ""))}
                                style={{ width: 90 }}
                            />
                            <button type="button" className="btn-secondary" disabled={busy} onClick={() => void applyPort()}>
                                {t("mcp_apply")}
                            </button>
                        </div>
                    </div>

                    <div className="setting-row" style={{ flexDirection: "column", alignItems: "stretch", gap: 8 }}>
                        <div className="setting-label">
                            <span>{t("mcp_token")}</span>
                            <small>
                                {t("mcp_token_hint")}
                                {status?.running
                                    ? ` ${t("mcp_running_on")} ${status.endpoint}`
                                    : ` ${t("mcp_not_running")}`}
                            </small>
                        </div>
                        <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
                            <input
                                type="text"
                                readOnly
                                value={status?.token ?? ""}
                                onFocus={(e) => e.currentTarget.select()}
                                style={{ flex: 1, fontFamily: "monospace" }}
                            />
                            <button type="button" className="btn-secondary" disabled={busy} onClick={() => void copyToken()}>
                                <Copy size={14} />
                                {copied ? t("mcp_copied") : t("mcp_copy")}
                            </button>
                            <button type="button" className="btn-secondary" disabled={busy} onClick={() => void regenerate()} title={t("mcp_regenerate_hint")}>
                                <RefreshCw size={14} />
                                {t("mcp_regenerate")}
                            </button>
                        </div>
                    </div>

                    <p className="settings-subpage-note">{t("mcp_client_hint")}</p>
                    {status?.running && !status.allowWrite && (
                        <p className="settings-subpage-note">{t("mcp_readonly_notice")}</p>
                    )}
                    {error && <p className="settings-subpage-note" style={{ color: "#e5484d" }}>{error}</p>}
                </div>
            )}
        </div>
    );
};

export default McpSettingsGroup;
