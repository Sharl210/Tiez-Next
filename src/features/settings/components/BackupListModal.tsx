import { useCallback, useEffect, useMemo, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { invoke } from "@tauri-apps/api/core";
import { FolderOpen, Loader2, Play, RefreshCw, X } from "lucide-react";
import BackupContextMenu, { type BackupMenuKind } from "./BackupContextMenu";
import { autoBackupErrorText } from "../lib/autoBackupError";
import { formatBytes } from "../lib/formatBytes";

/**
 * 「备份列表」悬浮窗 —— 展示全部**自动容灾备份**（定时 + 软件启动），并支持
 * 固定 / 删除 / 恢复。
 *
 * # 为什么和「导出备份」分开
 *
 * 用户把两件事分得很清楚：容灾自动保险（定时 + 启动，同一目录、受份数与轮换约束）
 * 和用户自己手动导出的备份（自选路径、不受约束）。本窗只展示前者，后端也是两套
 * 目录、两套命令。
 *
 * # 为什么来源要显示出来
 *
 * 用户原话特意解释了"为什么有的备份不在预期时间点"：定时备份之外的**启动备份**不受
 * 定时开关约束。列表里逐行标出来源，用户才能理解时间分布，而不是以为定时器坏了。
 *
 * # 二次确认的落点
 *
 * 删除与恢复都在本窗内用 `confirm-dialog` 浮层做二次确认（与标签管理页的删除确认
 * 同一套类名与结构）。恢复的确认文案里必须写明"导入前会自动给当前数据再做一份旁路
 * 备份"，这是可回退性的凭据，也是导入链既有的保证。
 */

/** 一条自动备份（对应后端 `store::BackupEntry`，camelCase 序列化）。 */
export interface AutoBackupEntry {
  /** 文件名，界面用它作为唯一标识调用固定/删除/恢复命令。 */
  archiveName: string;
  path: string;
  /** `scheduled` / `startup` / `manual`。 */
  origin: string;
  /** RFC3339（秒级）。 */
  createdAt: string;
  createdAtMs: number;
  /** 人类可读本地时间 `YYYY-MM-DD HH:MM:SS`，界面直接显示（精确到秒）。 */
  createdAtLocal: string;
  sizeBytes: number;
  pinned: boolean;
  seq: number;
}

/** 配置（对应后端 `AutoBackupConfig`）。 */
export interface AutoBackupConfig {
  enabled: boolean;
  intervalMinutes: number;
  maxKeep: number;
  backupOnStartup: boolean;
}

/** 列表命令的返回（对应后端 `mod.rs::view`）。 */
export interface AutoBackupListPayload {
  /** 备份目录（容灾专用，与手动导出的路径不同）。 */
  dir: string;
  config: AutoBackupConfig;
  /** 允许固定的最大条数 = `maxKeep - 1`。 */
  maxPinned: number;
  pinnedCount: number;
  totalCount: number;
  entries: AutoBackupEntry[];
  warnings: string[];
}

interface BackupListModalProps {
  open: boolean;
  t: (key: string) => string;
  theme: string;
  onClose: () => void;
  /** 列表加载完成后回传计数，供设置区块显示"N 份备份 · 已固定 M"。 */
  onLoaded?: (payload: AutoBackupListPayload) => void;
}

/** 待二次确认的动作：一次只可能有一个（联合类型让"两个确认框同时出现"无法表达）。 */
type PendingAction =
  | { kind: "delete"; entry: AutoBackupEntry }
  | { kind: "restore"; entry: AutoBackupEntry };

export const BackupListModal = ({ open, t, theme, onClose, onLoaded }: BackupListModalProps) => {
  const [payload, setPayload] = useState<AutoBackupListPayload | null>(null);
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [menu, setMenu] = useState<{ x: number; y: number; entry: AutoBackupEntry } | null>(null);
  const [pending, setPending] = useState<PendingAction | null>(null);
  /** 恢复成功后若后端要求重启，这里置位并显示重启提示（含"立即重启"）。 */
  const [restartRequired, setRestartRequired] = useState(false);

  const explain = useCallback((e: unknown) => autoBackupErrorText(t, e), [t]);

  const refresh = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const data = await invoke<AutoBackupListPayload>("list_auto_backups");
      setPayload(data);
      onLoaded?.(data);
    } catch (e: unknown) {
      console.error("list_auto_backups failed:", e);
      setError(explain(e));
      setPayload(null);
    } finally {
      setLoading(false);
    }
  }, [explain, onLoaded]);

  // 打开时加载一次。关闭后不清空 payload：再次打开会先显示上次的内容再刷新，
  // 比白屏一闪更稳。
  useEffect(() => {
    if (open) void refresh();
  }, [open, refresh]);

  // 后端在"备份生成 / 目录变化（删除、固定、轮换、恢复）"后广播事件，窗口开着时
  // 自动跟着刷新，用户不必手动点刷新。
  useEffect(() => {
    if (!open) return;
    let dispose: (() => void) | undefined;
    let cancelled = false;
    import("@tauri-apps/api/event")
      .then(({ listen }) =>
        Promise.all([
          listen("auto-backup-created", () => void refresh()),
          listen("auto-backup-changed", () => void refresh()),
        ])
      )
      .then((unlisteners) => {
        const off = () => unlisteners.forEach((u) => u());
        if (cancelled) off();
        else dispose = off;
      })
      .catch((e) => console.error("auto backup event listen failed:", e));
    return () => {
      cancelled = true;
      dispose?.();
    };
  }, [open, refresh]);

  const entries = payload?.entries ?? [];
  const pinnedCount = payload?.pinnedCount ?? entries.filter((e) => e.pinned).length;
  const maxPinned = payload?.maxPinned ?? 0;

  const subTitle = useMemo(() => {
    if (!payload) return "";
    return t("auto_backup_modal_counts")
      .replace("{total}", String(payload.totalCount))
      .replace("{pinned}", String(pinnedCount))
      .replace("{maxPinned}", String(maxPinned));
  }, [payload, pinnedCount, maxPinned, t]);

  /** 来源码 → 当前语言文案（查不到词条时退回原始码，不把内部键名甩给用户）。 */
  const originText = (code: string): string => {
    const key = `auto_backup_origin_${code}`;
    const text = t(key);
    return text === key ? code : text;
  };

  /** 固定 / 取消固定。上限判定在后端，界面的职责是把原因码与真实数字讲清楚。 */
  const applyPin = async (entry: AutoBackupEntry, pinned: boolean) => {
    if (busy) return;
    setBusy(true);
    try {
      await invoke("set_auto_backup_pinned", { archiveName: entry.archiveName, pinned });
      await refresh();
    } catch (e: unknown) {
      console.error("set_auto_backup_pinned failed:", e);
      setError(explain(e));
    } finally {
      setBusy(false);
    }
  };

  const doDelete = async (entry: AutoBackupEntry) => {
    setBusy(true);
    try {
      await invoke("delete_auto_backup", { archiveName: entry.archiveName });
      await refresh();
    } catch (e: unknown) {
      console.error("delete_auto_backup failed:", e);
      setError(explain(e));
    } finally {
      setBusy(false);
    }
  };

  const doRestore = async (entry: AutoBackupEntry) => {
    setBusy(true);
    setError(null);
    try {
      // 后端复用既有导入链，返回 `{ archiveName, restoreReport }`，其中
      // `restoreReport.restartRequired` 表示"必须重启应用才能看到恢复后的数据"。
      // 这不是可选提示：不重启用户会以为恢复没生效。因此这里必须读出来并提示，
      // 复用既有导入链已经在用的重启文案（`backup_import_restart*`），不新造词条。
      const result = await invoke<{ restoreReport?: { restartRequired?: boolean } }>(
        "restore_auto_backup",
        { archiveName: entry.archiveName }
      );
      if (result?.restoreReport?.restartRequired) setRestartRequired(true);
      await refresh();
    } catch (e: unknown) {
      console.error("restore_auto_backup failed:", e);
      setError(explain(e));
    } finally {
      setBusy(false);
    }
  };

  /** 「立即备份一次」：与定时备份同一目录、同一批留存名额。 */
  const runNow = async () => {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      // 版本号只进备份包的 manifest 作为展示字段。取不到就给空串——不能因为
      // "查版本失败"让用户点不动「立即备份」。
      //
      // 【为什么要判类型而不是只 catch】`invoke` 在某些 mock/降级路径下会**正常 resolve
      // 出 undefined**（不抛错）。若把这个值直接放进参数对象，Tauri 序列化时该字段会
      // 整个消失，后端 `app_version: String` 直接报"缺参数"——于是"版本查不到"又一次
      // 变成了"备份点不动"，正是这里要避免的那件事。所以只接受真正的字符串。
      let version = "";
      try {
        const probed = await invoke<unknown>("plugin:app|version");
        if (typeof probed === "string") version = probed;
      } catch (e) {
        console.warn("app version unavailable:", e);
      }
      await invoke("run_auto_backup_now", { appVersion: version });
      await refresh();
    } catch (e: unknown) {
      console.error("run_auto_backup_now failed:", e);
      setError(explain(e));
    } finally {
      setBusy(false);
    }
  };

  const onMenuSelect = (kind: BackupMenuKind) => {
    const entry = menu?.entry;
    if (!entry) return;
    if (kind === "pin") void applyPin(entry, true);
    // 取消固定不设二次确认：它不破坏任何数据，只是解除保护。
    else if (kind === "unpin") void applyPin(entry, false);
    else if (kind === "delete") setPending({ kind: "delete", entry });
    else setPending({ kind: "restore", entry });
  };

  return (
    <AnimatePresence>
      {open && (
        <div className="modal-overlay" onClick={onClose}>
          <motion.div
            initial={{ scale: 0.9, opacity: 0 }}
            animate={{ scale: 1, opacity: 1 }}
            exit={{ scale: 0.9, opacity: 0 }}
            className="modal-content"
            role="dialog"
            aria-modal="true"
            aria-label={t("auto_backup_list")}
            data-backup-list-modal=""
            style={{
              width: "94%",
              maxWidth: "720px",
              gap: "12px",
              display: "flex",
              flexDirection: "column",
              maxHeight: "88vh",
            }}
            onClick={(e) => e.stopPropagation()}
          >
            <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center" }}>
              <h3 className="modal-title">{t("auto_backup_list")}</h3>
              <button
                className="btn-icon"
                onClick={onClose}
                aria-label={t("cancel")}
                style={{ border: "none", background: "transparent", boxShadow: "none" }}
              >
                <X size={18} />
              </button>
            </div>

            {/* 条数概览 + 备份目录。目录必须明示：用户要求"和手动备份的路径不同"，
                只靠文字说明不够，直接把真实路径摆出来。 */}
            <div style={{ fontSize: "11px", color: "var(--text-secondary)", lineHeight: 1.6 }}>
              <div>{subTitle}</div>
              {payload?.dir && (
                <div style={{ wordBreak: "break-all", marginTop: "2px" }}>
                  {t("auto_backup_dir").replace("{path}", payload.dir)}
                </div>
              )}
            </div>

            <div style={{ display: "flex", gap: "8px", flexWrap: "wrap" }}>
              <button
                className="btn-icon"
                disabled={busy}
                onClick={() => void refresh()}
                style={{ width: "auto", padding: "4px 12px", fontSize: "10px", height: "26px", display: "flex", alignItems: "center", gap: "6px" }}
              >
                {loading ? <Loader2 size={12} /> : <RefreshCw size={12} />}
                {t("auto_backup_refresh")}
              </button>
              <button
                className="btn-icon"
                disabled={busy}
                onClick={() => void runNow()}
                style={{ width: "auto", padding: "4px 12px", fontSize: "10px", height: "26px", display: "flex", alignItems: "center", gap: "6px" }}
              >
                <Play size={12} />
                {t("auto_backup_run_now")}
              </button>
              <button
                className="btn-icon"
                onClick={() => invoke<string>("open_auto_backup_folder").catch(console.error)}
                style={{ width: "auto", padding: "4px 12px", fontSize: "10px", height: "26px", display: "flex", alignItems: "center", gap: "6px" }}
              >
                <FolderOpen size={12} />
                {t("auto_backup_open_folder")}
              </button>
            </div>

            {/* 固定数已达上限的提示、以及后端其他的非致命告警。 */}
            {error && (
              <div
                data-backup-list-error=""
                style={{
                  border: "1px solid rgba(200,80,80,0.5)",
                  borderRadius: "6px",
                  padding: "8px 10px",
                  fontSize: "11px",
                  lineHeight: 1.6,
                }}
              >
                {error}
              </div>
            )}
            {payload?.warnings?.map((w) => (
              <div
                key={w}
                style={{ fontSize: "10px", color: "var(--text-secondary)", opacity: 0.85, lineHeight: 1.5 }}
              >
                {w}
              </div>
            ))}

            {/*
              列表容器。

              刻意**不加 `custom-scrollbar`**：那个类名只有 `TagManager.tsx` 自己注入的
              `<style>` 里才有定义（一层更细的滚动条），全局样式表里并不存在。写在这里
              等于引用一个不存在的类名——正是"类名看着眼熟但不存在、元素按裸元素渲染"
              那类静默失败。全局 `scrollbar.css` 已给所有元素配了滚动条外观。
            */}
            <div
              data-backup-list=""
              style={{
                overflowY: "auto",
                maxHeight: "46vh",
                minHeight: "60px",
                marginInline: "-2px",
                paddingInline: "2px",
              }}
            >
              {entries.length === 0 ? (
                <div style={{ padding: "16px 0", color: "var(--text-secondary)", fontSize: "11px" }}>
                  {loading ? t("auto_backup_loading") : t("auto_backup_empty")}
                </div>
              ) : (
                entries.map((entry) => (
                  /* 右键是这条记录唯一的操作入口——与标签组的交互一致。
                     用 `onContextMenu` 而不是常驻按钮：每行都摆三个按钮会让
                     "这份备份多大、什么时候做的"这些真正要看的信息被挤走。 */
                  <div
                    key={entry.archiveName}
                    data-backup-row={entry.archiveName}
                    onContextMenu={(e) => {
                      e.preventDefault();
                      e.stopPropagation();
                      setMenu({ x: e.clientX, y: e.clientY, entry });
                    }}
                    title={entry.path}
                    style={{
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "space-between",
                      gap: "8px",
                      padding: "8px 10px",
                      marginBottom: "6px",
                      border: `1px solid ${
                        entry.pinned
                          ? "rgba(64,160,96,0.55)"
                          : "var(--border-color, rgba(128,128,128,0.25))"
                      }`,
                      borderRadius: "6px",
                      background: entry.pinned ? "rgba(64,160,96,0.07)" : "transparent",
                      cursor: "context-menu",
                    }}
                  >
                    <div style={{ minWidth: 0, flex: 1 }}>
                      {/* 时间精确到秒（后端直接给 `YYYY-MM-DD HH:MM:SS`）。 */}
                      <div
                        style={{
                          fontSize: "12px",
                          fontWeight: 600,
                          fontVariantNumeric: "tabular-nums",
                          marginBottom: "2px",
                        }}
                      >
                        {entry.createdAtLocal}
                      </div>
                      <div style={{ fontSize: "10px", color: "var(--text-secondary)" }}>
                        {formatBytes(entry.sizeBytes)}
                      </div>
                    </div>

                    {/* 来源标签：让"为什么有的备份不在预期时间点"一眼可见。 */}
                    <span
                      style={{
                        flexShrink: 0,
                        fontSize: "9px",
                        fontWeight: 500,
                        padding: "1px 5px",
                        borderRadius: "3px",
                        background: "var(--bg-main, var(--bg-element))",
                        color: "var(--text-secondary)",
                        whiteSpace: "nowrap",
                      }}
                    >
                      {originText(entry.origin)}
                    </span>

                    {/* 固定状态：一眼能看出哪些不会被轮换删掉。 */}
                    {entry.pinned && (
                      <span
                        data-backup-pinned=""
                        style={{
                          flexShrink: 0,
                          fontSize: "9px",
                          fontWeight: 600,
                          padding: "1px 5px",
                          borderRadius: "3px",
                          border: "1px solid rgba(64,160,96,0.6)",
                          color: "rgb(64,160,96)",
                          whiteSpace: "nowrap",
                        }}
                      >
                        {t("auto_backup_pinned_badge")}
                      </span>
                    )}
                  </div>
                ))
              )}
            </div>

            <div style={{ fontSize: "10px", color: "var(--text-secondary)", opacity: 0.8, lineHeight: 1.5 }}>
              {t("auto_backup_row_hint")}
            </div>
          </motion.div>
        </div>
      )}

      {/* 右键菜单：portal 到 body，由它自己处理外部点击/Escape/滚动关闭。 */}
      {menu && (
        <BackupContextMenu
          x={menu.x}
          y={menu.y}
          archiveName={menu.entry.archiveName}
          pinned={menu.entry.pinned}
          t={t}
          onSelect={onMenuSelect}
          onClose={() => setMenu(null)}
        />
      )}

      {/*
        恢复成功后的重启提示。

        后端导入链的 `restoreReport.restartRequired` 为真时必须让用户知道并给出动作，
        否则用户会以为恢复没生效。文案**复用既有导入链已经在用的键**
        （`backup_import_restart` / `backup_import_restart_now`）：本功能不许去改
        `src/locales.ts`（多代理并发），复用既有的既省事又与本应用的说法一致。
      */}
      {restartRequired && (
        <div className="modal-overlay" data-backup-restart-prompt="" style={{ zIndex: 3200 }}>
          <div className={`confirm-dialog theme-${theme}`} onClick={(e) => e.stopPropagation()}>
            <div className="confirm-dialog-message">{t("backup_import_restart")}</div>
            <div className="confirm-dialog-buttons">
              <button className="confirm-dialog-button" onClick={() => setRestartRequired(false)}>
                {t("cancel")}
              </button>
              <button
                className="confirm-dialog-button primary"
                data-backup-restart-now=""
                onClick={() => {
                  setRestartRequired(false);
                  invoke("relaunch").catch(console.error);
                }}
              >
                {t("backup_import_restart_now")}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* 二次确认：删除（破坏性）与恢复（替换当前数据）。 */}
      {pending && (
        <div
          className="modal-overlay"
          data-backup-confirm={pending.kind}
          style={{ zIndex: 3200 }}
          onClick={() => setPending(null)}
        >
          <div className={`confirm-dialog theme-${theme}`} onClick={(e) => e.stopPropagation()}>
            <div className="confirm-dialog-title">
              {pending.kind === "delete" ? t("auto_backup_delete_title") : t("auto_backup_restore_title")}
            </div>

            {/*
              被操作对象的**身份行**。

              为什么单独一行、而不是把占位符填进确认文案里：词条
              `auto_backup_delete_confirm` / `auto_backup_restore_confirm` 里**没有任何
              占位符**（三语皆然），所以把它们 `.replace("{time}"...)` 是无效的——
              用户看到的确认框将只写"确认删除这份备份？"，而不告诉他是**哪一份**。
              而"删错了没法从列表里找回"，身份信息正是他唯一能核对的东西。

              本功能不允许改 `src/locales.ts`（多代理并发），因此不新增词条，改为把
              时间/大小/来源作为结构化数据渲染成独立一行。这样三语都不用改词条，
              且信息以等宽数字对齐，比塞进句子里更好读。
            */}
            <div
              data-backup-confirm-subject=""
              style={{
                fontSize: "12px",
                lineHeight: 1.7,
                margin: "0 0 12px",
                color: "var(--text-primary)",
              }}
            >
              <div style={{ fontVariantNumeric: "tabular-nums", fontWeight: 600 }}>
                {pending.entry.createdAtLocal}
              </div>
              <div style={{ color: "var(--text-secondary)" }}>
                {`${formatBytes(pending.entry.sizeBytes)} · ${originText(pending.entry.origin)}`}
              </div>
            </div>

            <div className="confirm-dialog-message">
              {pending.kind === "delete"
                ? t("auto_backup_delete_confirm")
                : t("auto_backup_restore_confirm")}
            </div>
            <div className="confirm-dialog-buttons">
              <button className="confirm-dialog-button" onClick={() => setPending(null)}>
                {t("cancel")}
              </button>
              <button
                className="confirm-dialog-button primary"
                data-backup-confirm-ok=""
                onClick={() => {
                  const action = pending;
                  setPending(null);
                  if (action.kind === "delete") void doDelete(action.entry);
                  else void doRestore(action.entry);
                }}
              >
                {pending.kind === "delete" ? t("delete") : t("auto_backup_restore")}
              </button>
            </div>
          </div>
        </div>
      )}
    </AnimatePresence>
  );
};

export default BackupListModal;
