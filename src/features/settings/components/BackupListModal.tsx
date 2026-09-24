import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { ComponentType } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { invoke } from "@tauri-apps/api/core";
import { Clock, CornerDownRight, FolderOpen, Loader2, Play, Power, RefreshCw, X, Zap } from "lucide-react";
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

  /**
   * `onLoaded` 的「最新引用」盒子。
   *
   * # 为什么不能把 `onLoaded` 直接放进 `refresh` 的依赖数组
   *
   * 父组件（`AutoBackupSettingsGroup`）把它写成**内联箭头函数**，于是父组件每渲染
   * 一次，这个 prop 就是一个新引用。链路是：
   *
   *   父渲染 → 新 onLoaded → refresh 重建 → 依赖 refresh 的两个 effect 重跑
   *          → 再一次 invoke + setPayload + onLoaded → 父组件 setState → 父渲染 …
   *
   * 这不是"动画不生效"，而是**每帧都在重新请求后端**：`list_auto_backups` 会被
   * 无休止地调用，界面上看到的就是刷新按钮在 `Loader2` 与 `RefreshCw` 之间高频切换。
   *
   * # 为什么改写进 ref 不会引入过期闭包
   *
   * 每次渲染都**无条件**执行 `onLoadedRef.current = onLoaded`。ref 是同一个可变盒子，
   * 这次赋值发生在渲染阶段，早于任何 effect 或事件回调被调度；因此之后无论谁调用
   * `onLoadedRef.current(...)`，拿到的都是「最近一次渲染传进来的那个函数」。
   * `refresh` 的依赖里不再有 `onLoaded`，它的身份就与父组件的渲染次数解耦，
   * effect 只会在 `open` 真正变化时重跑 —— 循环被打断，而回调内容始终是最新的。
   *
   * # 为什么不写成 `useRef(onLoaded)`
   *
   * `useRef(onLoaded)` 只在首次渲染取初值、之后不再更新，ref 会永远停在第一版闭包上；
   * 父组件换了语言或上下文后，回调读到的仍是旧值 —— 那才是真正的过期闭包。
   * 必须"每轮都赋值"，这正是 React 文档里的 *Latest ref* 模式。
   *
   * # 为什么不用 React 19.2 的 `useEffectEvent`
   *
   * 两者渲染期语义等价，但 `useEffectEvent` 的契约限定返回的函数**只能在 effect 内调用**；
   * 而本组件的 `refresh` 既被 effect 调用，也被「刷新」按钮的 `onClick` 调用，会从
   * 事件处理器里触发这个回调，越出契约。本仓库其他位置（如
   * `src/shared/hooks/useClipboardEvents.ts`）也用同一套 ref 写法，保持一致。
   */
  const onLoadedRef = useRef(onLoaded);
  onLoadedRef.current = onLoaded;

  const refresh = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const data = await invoke<AutoBackupListPayload>("list_auto_backups");
      setPayload(data);
      onLoadedRef.current?.(data);
    } catch (e: unknown) {
      console.error("list_auto_backups failed:", e);
      setError(explain(e));
      setPayload(null);
    } finally {
      setLoading(false);
    }
    // 依赖里只有 explain（它自己依赖 t）。onLoaded 走 ref，因此父组件的每次渲染
    // 都不再改变 refresh 的身份 —— 这是打断上面那条循环的关键。
  }, [explain]);

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

  /**
   * 来源码 → 图标组件（未知码返回 `undefined`，只显示文字，不会显示错误图标）。
   *
   * 来源标签此前只是一块浅色小方块，与旁边的真按钮在底色上只差 4% 不透明度，
   * 连视觉模型都把它读成了按钮。除样式收紧外再配一个语义图标
   * （定时=时钟、启动=电源、立即=闪电），用户一眼就能读出"这是一段来源说明"。
   */
  const ORIGIN_ICONS: Record<string, ComponentType<{ size?: number }>> = {
    scheduled: Clock,
    startup: Power,
    manual: Zap,
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
            className="modal-content backup-modal"
            role="dialog"
            aria-modal="true"
            aria-label={t("auto_backup_list")}
            data-backup-list-modal=""
            onClick={(e) => e.stopPropagation()}
          >
            <div className="backup-modal-header">
              <h3 className="modal-title">{t("auto_backup_list")}</h3>
              {/* 关闭按钮与工具栏按钮同量级：仓库里没有通用弹窗关闭类，
                  原先靠内联把 `.btn-icon` 的令牌清空，导致它比工具栏按钮更大更重、
                  圆角还随主题在 0–999px 之间跳。这里改由 `.backup-close-btn`
                  统一管（尺寸 26px、圆角走 `--button-radius`）。 */}
              <button className="backup-close-btn" onClick={onClose} aria-label={t("cancel")}>
                <X size={14} />
              </button>
            </div>

            {/* 条数概览 + 备份目录。目录必须明示：用户要求"和手动备份的路径不同"，
                只靠文字说明不够，直接把真实路径摆出来。

                层级：汇总行是这一屏里优先级最高的信息（份数 / 已固定 / 固定上限），
                给最大字号与字重；目录是"去哪里找文件"的补充，低一档。
                此前两者同为 11px / 同色 / 挤在一起，用户读不出主次。 */}
            <div className="backup-meta">
              <div className="backup-counts">{subTitle}</div>
              {payload?.dir && (
                <div className="backup-dir">
                  {t("auto_backup_dir").replace("{path}", payload.dir)}
                </div>
              )}
            </div>

            <div className="backup-toolbar">
              <button
                className="btn-icon backup-toolbar-btn"
                data-backup-refresh=""
                disabled={busy}
                onClick={() => void refresh()}
              >
                {/* 【必须带 animate-spin】此前这里没有动画类，"刷新中"只表现为图标在
                    Loader2 与 RefreshCw 之间来回换。配合当时存在的无限刷新循环，
                    用户看到的正是"按钮一直在闪"。`.animate-spin` 是 `ai.css` 里
                    既有的全局工具类。 */}
                {loading ? <Loader2 size={12} className="animate-spin" /> : <RefreshCw size={12} />}
                {t("auto_backup_refresh")}
              </button>
              <button
                className="btn-icon backup-toolbar-btn"
                disabled={busy}
                onClick={() => void runNow()}
              >
                <Play size={12} />
                {t("auto_backup_run_now")}
              </button>
              <button
                className="btn-icon backup-toolbar-btn"
                onClick={() => invoke<string>("open_auto_backup_folder").catch(console.error)}
              >
                <FolderOpen size={12} />
                {t("auto_backup_open_folder")}
              </button>
            </div>

            {/* 固定数已达上限的提示、以及后端其他的非致命告警。 */}
            {error && (
              <div className="backup-error" data-backup-list-error="">
                {error}
              </div>
            )}
            {payload?.warnings?.map((w) => (
              <div key={w} className="backup-warning">
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
            <div className="backup-list" data-backup-list="">
              {entries.length === 0 ? (
                <div className="backup-empty">
                  {loading ? t("auto_backup_loading") : t("auto_backup_empty")}
                </div>
              ) : (
                entries.map((entry) => {
                  const OriginIcon = ORIGIN_ICONS[entry.origin];
                  return (
                    /* 右键是这条记录唯一的操作入口——与标签组的交互一致。
                       用 `onContextMenu` 而不是常驻按钮：每行都摆三个按钮会让
                       "这份备份多大、什么时候做的"这些真正要看的信息被挤走。 */
                    <div
                      key={entry.archiveName}
                      data-backup-row={entry.archiveName}
                      className={`backup-row${entry.pinned ? " pinned" : ""}`}
                      onContextMenu={(e) => {
                        e.preventDefault();
                        e.stopPropagation();
                        setMenu({ x: e.clientX, y: e.clientY, entry });
                      }}
                      title={entry.path}
                    >
                      <div className="backup-row-main">
                        {/* 时间精确到秒（后端直接给 `YYYY-MM-DD HH:MM:SS`）。 */}
                        <div className="backup-row-time">{entry.createdAtLocal}</div>
                        <div className="backup-row-size">{formatBytes(entry.sizeBytes)}</div>
                      </div>

                      {/* 来源标签：让"为什么有的备份不在预期时间点"一眼可见。
                          它是一段**说明**而不是控件——`.backup-origin-tag` 去掉了
                          底色/描边、加了左侧竖条与语义图标、压到 16px 高与 2px 圆角，
                          与旁边 26px 高、带 `--button-border` 描边与 `--button-radius`
                          圆角的真按钮在多个可量测属性上明显不同。
                          `role="note"` 让辅助技术也把它读成说明而非可操作控件。 */}
                      <span className="backup-origin-tag" data-backup-origin={entry.origin} role="note">
                        {OriginIcon && <OriginIcon size={9} />}
                        {originText(entry.origin)}
                      </span>

                      {/* 固定状态：一眼能看出哪些不会被轮换删掉。
                          与来源标签共用同一套"小号方角标注"形态语言，
                          靠颜色（绿=受保护）区分语义。 */}
                      {entry.pinned && (
                        <span className="backup-pinned-tag" data-backup-pinned="">
                          {t("auto_backup_pinned_badge")}
                        </span>
                      )}
                    </div>
                  );
                })
              )}
            </div>

            {/* 底部交互提示。它是用户唯一能学会"右键可以做什么"的地方，
                原先的 `--text-secondary` + `opacity: .8` 在六套主题的浅色态下
                对比度只有 2.20–3.68（AA 要求 ≥ 4.5），实测确实"几乎难以辨认"。
                现在改由字号 + 分隔线 + 承托底表达层级，文字本身保持可读。 */}
            <div className="backup-hint" data-backup-row-hint="">
              <CornerDownRight size={12} />
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
            <div className="backup-confirm-subject" data-backup-confirm-subject="">
              <div className="backup-confirm-subject-time">{pending.entry.createdAtLocal}</div>
              <div className="backup-confirm-subject-meta">
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
