import { useCallback, useEffect, useState } from "react";
import { open, ask, message, confirm } from "@tauri-apps/plugin-dialog";
import { invoke } from "@tauri-apps/api/core";
import {
    ChevronDown,
    ChevronRight,
    CopyCheck,
    Download,
    FolderInput,
    FolderOpen,
    Trash2,
    Upload,
} from "lucide-react";
import { formatBytes } from "../../lib/formatBytes";
import { backendErrorText as backupErrorText } from "../../lib/backendError";

interface DataSettingsGroupProps {
    t: (key: string) => string;
    collapsed: boolean;
    onToggle: () => void;
    dataPath: string;
}

/** 一条可迁移来源目录信息（对应后端 `list_legacy_data_dirs`）。 */
interface LegacyDir {
    path: string;
    identifier: string;
    /**
     * 这条数据原本属于哪个应用：
     * - `legacy_tiez`：旧版 TieZ（应用改名前的上游版本）
     * - `previous_tiez_next`：历史版本的 Tiez-Next（标识符未变，含之后所有版本）
     *
     * 是机器可读码，界面按语言映射成 `legacy_origin_*` 文案。
     */
    origin: string;
    bytes: number;
    files: number;
    has_database: boolean;
    /** 是否允许"备份后删除"。本应用自己标识符的目录恒为 false。 */
    canDelete: boolean;
}

/**
 * 一次手动迁移的结果（对应后端 `migrate_from_data_dir` 的 `IdentifierMigrationReport`）。
 *
 * `skipReason` / `error` 是**机器可读原因码**，不是给用户看的文案：界面负责把它们
 * 映射成当前语言的人话（见 `skipReasonText`），这样新增原因码时也不会出现"半英文"。
 */
interface MigrationReport {
    status: "migrated" | "skipped" | "failed";
    source: string;
    target: string;
    /** 源侧条目总数（含目录条目）——仅用于详情展示。 */
    files: number;
    /** 源侧全部条目字节数之和——仅用于详情展示。 */
    bytes: number;
    /** 本次真正新交付的文件数（不含目录条目、不含沿用的文件）。 */
    deliveredFiles: number;
    /** 本次真正新交付的字节数。 */
    deliveredBytes: number;
    /** 目标里原本就有、本次未覆盖而沿用的文件数。 */
    keptExisting: number;
    skipReason: string | null;
    error: string | null;
    pathsRewritten: boolean;
    rewriteError: string | null;
    sourceUntouched: boolean;
    restartRequired: boolean;
    supersededDb: string | null;
}

/** 导出前的只读清点（对应后端 `backup_preflight`）。 */
interface BackupPreflight {
    dataDir: string;
    managedBytes: number;
    managedFiles: number;
    /** 自定义背景图在数据目录之外时为 true（导出会一并打包）。 */
    backgroundOutside: boolean;
    backgroundPath: string | null;
}

/** 导出结果（对应后端 `export_backup` 的 `BackupReport`）。 */
interface BackupReport {
    outputPath: string;
    entriesWritten: number;
    bytesWritten: number;
    counts: {
        entries: number;
        tags: number;
        attachments: number;
        emojiFavorites: number;
        settings: number;
    };
    skipped: string[];
    notes: string[];
    sha256: string;
}

/** 包预览（对应后端 `inspect_backup_package` 的 `InspectReport`）。 */
interface InspectReport {
    appId: string;
    appVersion: string;
    formatVersion: number;
    exportedAt: string;
    schemaVersion: number;
    counts: {
        entries: number;
        tags: number;
        attachments: number;
        emojiFavorites: number;
        settings: number;
    };
    archiveEntries: number;
    notes: string[];
    fileBytes: number;
}

/** 导入结果（对应后端 `import_backup` 的 `RestoreReport`）。 */
interface RestoreReport {
    archivePath: string;
    formatVersion: number;
    exportedAt: string;
    exportedAppVersion: string;
    preRestoreBackup: string | null;
    restoredFiles: number;
    restoredBytes: number;
    verifiedEntries: number;
    counts: {
        entries: number;
        tags: number;
        attachments: number;
        emojiFavorites: number;
        settings: number;
    };
    resetsApplied: string[];
    warnings: string[];
    restartRequired: boolean;
}

// 字节数格式化：与自动备份的「备份列表」共用同一份实现（见 lib/formatBytes.ts）。

/**
 * 取当前应用版本号。
 *
 * 用 `core:app:allow-version` 权限（capabilities 已声明）向后端要；拿不到就返回空串，
 * 只是 package 的 manifest 少一个展示字段，不影响备份本身的可用性——因此这里
 * **不**抛错，避免"版本查询失败导致整个导出不可用"。
 */
const appVersionSafe = async (): Promise<string> => {
    try {
        return await invoke<string>("plugin:app|version");
    } catch (e) {
        console.warn("app version unavailable:", e);
        return "";
    }
};

// 后端错误码解析与三语文案映射已提到 `lib/backendError.ts`：自动备份的
// 「固定数已达上限」提示要按原因码取出 `maxKeep` / `currentPinned` 两个真实数字，
// 与这里的备份导出/导入共用同一份解析，避免两份兜底逻辑各自演化。

const DataSettingsGroup = ({ t, collapsed, onToggle, dataPath }: DataSettingsGroupProps) => {
    // 迁移中心：旧标识符遗留的数据目录。加载失败不阻塞设置面板其他部分。
    const [legacyDirs, setLegacyDirs] = useState<LegacyDir[]>([]);
    const [busyPath, setBusyPath] = useState<string | null>(null);
    // 最近一次手动迁移的结果：用户点完按钮必须能核对自己刚才到底做成了什么，
    // 而不是只看一句"成功/失败"。
    const [lastResult, setLastResult] = useState<MigrationReport | null>(null);

    // 备份与恢复
    const [preflight, setPreflight] = useState<BackupPreflight | null>(null);
    const [lastBackup, setLastBackup] = useState<BackupReport | null>(null);
    const [lastRestore, setLastRestore] = useState<RestoreReport | null>(null);
    const [backupBusy, setBackupBusy] = useState<"export" | "import" | null>(null);

    const refreshLegacyDirs = () => {
        invoke<LegacyDir[]>("list_legacy_data_dirs")
            .then(setLegacyDirs)
            .catch((e) => {
                console.error("list_legacy_data_dirs failed:", e);
                setLegacyDirs([]);
            });
    };

    const refreshPreflight = useCallback(() => {
        invoke<BackupPreflight>("backup_preflight")
            .then(setPreflight)
            .catch((e) => {
                console.error("backup_preflight failed:", e);
                setPreflight(null);
            });
    }, []);

    useEffect(() => {
        // 展开时才查询：折叠状态下不做无谓的磁盘统计。
        if (!collapsed) {
            refreshLegacyDirs();
            refreshPreflight();
        }
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [collapsed]);

    /**
     * 把后端返回的原因码翻成当前语言。
     *
     * `t()` 查不到词条时会原样返回键名，据此判断"没翻译成功"，改为显示原因码本身，
     * 而不是把 `legacy_migrate_notice_xxx` 这种内部键名甩给用户。
     */
    const skipReasonText = (code: string | null): string => {
        if (!code) return "";
        const key = `legacy_migrate_notice_${code}`;
        const text = t(key);
        return text === key ? code : text;
    };

    /**
     * 把后端给的来源码翻成当前语言，让用户知道每条是**谁的数据**。
     *
     * 两类来源必须区分清楚，因为它们的处置完全不同：
     * - 旧版 TieZ 的数据：可以迁移，也可以在确认新版无误后清理；
     * - 历史版本 Tiez-Next 的数据：可以迁移，但**不能清理**（那不是被取代的旧应用）。
     *
     * 与 `skipReasonText` 同一约定：查不到词条时退回显示原始码，不把内部键名甩给用户。
     */
    const originText = (code: string): string => {
        const key = `legacy_origin_${code}`;
        const text = t(key);
        return text === key ? code : text;
    };

    /**
     * 从用户指定的目录迁移数据。
     *
     * 三条与用户约定一致的语义：
     * - 这是**唯一**的数据搬移入口，应用启动时不会自动迁移任何东西；
     * - 源目录全程只读，迁移成功也不会被删除，因此同一个源目录可以反复验证；
     * - 新版里已经存过数据时会跳过并说明原因，绝不覆盖当前数据。
     */
    const handleMigrate = async (sourcePath: string, label: string) => {
        const confirmed = await ask(
            t("legacy_migrate_confirm")
                .replace("{path}", sourcePath)
                .replace("{label}", label),
            {
                title: t("legacy_migrate_title"),
                kind: "warning",
                okLabel: t("legacy_migrate_ok"),
                cancelLabel: t("cancel"),
            }
        );
        if (!confirmed) return;

        setBusyPath(sourcePath);
        setLastResult(null);
        try {
            const report = await invoke<MigrationReport>("migrate_from_data_dir", {
                path: sourcePath,
            });
            setLastResult(report);

            if (report.status === "migrated") {
                await message(
                    `${report.deliveredFiles === 0 && report.keptExisting > 0
                        ? t("legacy_migrate_result_already_present")
                        : t("legacy_migrate_done")
                              .replace("{files}", String(report.deliveredFiles))
                              .replace("{size}", formatBytes(report.deliveredBytes))}\n\n${t(
                        "legacy_migrate_source_safe"
                    )}${report.restartRequired ? `\n\n${t("legacy_migrate_restart")}` : ""}`,
                    { title: t("notice"), kind: "info" }
                );
            } else if (report.status === "failed") {
                await message(
                    `${t("legacy_migrate_failed").replace("{e}", report.error || "")}\n\n${t(
                        "legacy_migrate_source_safe"
                    )}`,
                    { title: t("error"), kind: "error" }
                );
            } else {
                await message(
                    `${t("legacy_migrate_skipped")}\n\n${skipReasonText(report.skipReason)}`,
                    { title: t("notice"), kind: "info" }
                );
            }
            refreshLegacyDirs();
        } catch (e: unknown) {
            const errorMsg = e instanceof Error ? e.message : String(e);
            await message(t("legacy_migrate_failed").replace("{e}", errorMsg), {
                title: t("error"),
                kind: "error",
            });
        } finally {
            setBusyPath(null);
        }
    };

    /** 「选择其它目录…」：迁移中心的核心入口——自动发现不一定覆盖用户的旧路径。 */
    const handleChooseDir = async () => {
        const selected = await open({
            directory: true,
            multiple: false,
            title: t("legacy_migrate_choose_title"),
        });
        if (!selected) return;
        const sourcePath = selected as string;
        // 系统目录选择器本身就是一次明确动作，选完再弹一次只会让人以为要选两遍；
        // 真正的二次确认在 handleMigrate 里，会带上将要读取的完整路径。
        await handleMigrate(sourcePath, sourcePath);
    };

    /**
     * 清理一条旧目录：先向用户交代将发生什么（含路径、占用与备份说明），
     * 二次确认后才调用后端。后端会先备份再删除，因此即使误操作也可恢复。
     */
    const handleRemove = async (dir: LegacyDir) => {
        const confirmed = await ask(
            t("legacy_dir_delete_confirm")
                .replace("{path}", dir.path)
                .replace("{size}", formatBytes(dir.bytes)),
            {
                title: t("legacy_dir_delete_title"),
                kind: "warning",
                okLabel: t("legacy_dir_delete_ok"),
                cancelLabel: t("cancel"),
            }
        );
        if (!confirmed) return;

        setBusyPath(dir.path);
        try {
            const backupPath = await invoke<string>("remove_legacy_data_dir", { path: dir.path });
            await message(
                backupPath
                    ? t("legacy_dir_delete_done_with_backup").replace("{backup}", backupPath)
                    : t("legacy_dir_delete_done"),
                { title: t("notice"), kind: "info" }
            );
            refreshLegacyDirs();
        } catch (e: unknown) {
            const errorMsg = e instanceof Error ? e.message : String(e);
            await message(t("legacy_dir_delete_failed").replace("{e}", errorMsg), {
                title: t("error"),
                kind: "error",
            });
        } finally {
            setBusyPath(null);
        }
    };

    // ------------------------------------------------------------------
    // 备份导出 / 导入恢复
    // ------------------------------------------------------------------

    /**
     * 导出备份。
     *
     * 路径不弹系统保存对话框（那需要额外的 fs 权限），而是由后端挑一个稳妥位置并
     * 在界面上**明示**完整路径，再提供"打开所在文件夹"。用户始终确切知道文件落在哪。
     */
    const handleExport = async () => {
        setBackupBusy("export");
        setLastBackup(null);
        try {
            const version = await appVersionSafe();
            // 由后端挑一个稳妥的默认位置并回传完整路径——界面随后明示给用户，
            // 因此不弹系统保存对话框（那会额外要求 fs/dialog 保存权限）。
            const target = await invoke<string>("suggest_backup_path", {
                appVersion: version,
            });
            const report = await invoke<BackupReport>("export_backup", {
                outputPath: target,
                appVersion: version,
            });
            setLastBackup(report);
            await message(
                t("backup_export_done")
                    .replace("{path}", report.outputPath)
                    .replace("{files}", String(report.entriesWritten))
                    .replace("{size}", formatBytes(report.bytesWritten)) +
                    (report.notes.length ? `\n\n${report.notes.join("\n")}` : ""),
                { title: t("notice"), kind: "info" }
            );
        } catch (e: unknown) {
            await message(t("backup_export_failed").replace("{e}", backupErrorText(t, e)), {
                title: t("error"),
                kind: "error",
            });
        } finally {
            setBackupBusy(null);
            refreshPreflight();
        }
    };

    /**
     * 导入备份（**破坏性操作**，四层防护）。
     *
     * 1. 后端只接受"具体的一个 zip 文件路径"，不是任意目录（白名单式输入）；
     * 2. 先只读预览包：拒绝原版 TieZ / 缺 manifest / 比本版新的包，并展示包内数量；
     * 3. 二次确认弹窗明确写出"当前数据将被替换"以及包内到底有多少数据；
     * 4. 后端先给当前数据做完整旁路备份，再把备份路径回传，界面明确告知用户
     *    "可回退到哪里"。
     */
    const handleImport = async () => {
        // 【为什么 busy 必须在这里就置位】`open()` 与 `confirm()` 都是 await 的系统对话框，
        // 期间用户完全可以再点一次"导入"——若 `disabled` 直到那时才生效，就会开出两个
        // 文件选择器、跑起两次并发导入。后端已加进程级互斥兜底，但界面这一层也要在
        // **第一个 await 之前**就上锁，否则用户看到的是两个并行流程。
        if (backupBusy !== null) return;
        setBackupBusy("import");

        let selected: string | string[] | null = null;
        try {
            selected = await open({
                multiple: false,
                directory: false,
                filters: [{ name: "zip", extensions: ["zip"] }],
                title: t("backup_import"),
            });
        } finally {
            if (!selected) {
                setBackupBusy(null);
                return;
            }
        }

        // ---- 只读预览：把"将发生什么"提前摆给用户看 ----
        let info: InspectReport;
        try {
            info = await invoke<InspectReport>("inspect_backup_package", {
                path: selected as string,
            });
        } catch (e: unknown) {
            await message(t("backup_inspect_failed").replace("{e}", backupErrorText(t, e)), {
                title: t("error"),
                kind: "error",
            });
            setBackupBusy(null);
            return;
        }

        // ---- 二次确认：破坏性操作必须让用户明确点头 ----
        const ok = await confirm(
            t("backup_import_confirm")
                .replace("{entries}", String(info.counts.entries))
                .replace("{tags}", String(info.counts.tags))
                .replace("{attachments}", String(info.counts.attachments))
                // 表情收藏是"磁盘目录 + 设置项 JSON"双份存储，用户点确认前必须能核对
                // 它在不在包里——只听"附件 9 个"不足以判断。
                .replace("{emoji}", String(info.counts.emojiFavorites))
                .replace("{settings}", String(info.counts.settings))
                .replace("{exported_at}", info.exportedAt || "—")
                .replace("{version}", info.appVersion || "—"),
            {
                title: t("backup_import_confirm_title"),
                kind: "warning",
                okLabel: t("backup_import_confirm_ok"),
                cancelLabel: t("cancel"),
            }
        );
        if (!ok) {
            setBackupBusy(null);
            return;
        }

        setLastRestore(null);
        try {
            const report = await invoke<RestoreReport>("import_backup", {
                archivePath: selected as string,
            });
            setLastRestore(report);
            const summary = report.preRestoreBackup
                ? t("backup_import_done")
                      .replace("{files}", String(report.restoredFiles))
                      .replace("{size}", formatBytes(report.restoredBytes))
                      .replace("{backup}", report.preRestoreBackup)
                : t("backup_import_done_no_backup")
                      .replace("{files}", String(report.restoredFiles))
                      .replace("{size}", formatBytes(report.restoredBytes));
            const warnings = report.warnings.length ? `\n\n${report.warnings.join("\n")}` : "";
            await message(`${summary}${warnings}\n\n${t("backup_import_restart")}`, {
                title: t("notice"),
                kind: "info",
            });
        } catch (e: unknown) {
            await message(t("backup_import_failed").replace("{e}", backupErrorText(t, e)), {
                title: t("error"),
                kind: "error",
            });
        } finally {
            setBackupBusy(null);
            refreshPreflight();
        }
    };

    const totalBytes = legacyDirs.reduce((sum, d) => sum + d.bytes, 0);

    return (
        <div className={`settings-group ${collapsed ? 'collapsed' : ''}`}>
            <div className="group-header" onClick={onToggle}>
                <h3 style={{ margin: 0 }}>{t('data_management')}</h3>
                {collapsed ? <ChevronRight size={16} /> : <ChevronDown size={16} />}
            </div>
            {!collapsed && (
                <div className="group-content">
                    <div className="setting-item column no-border">
                        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: '8px' }}>
                            <span className="item-label" style={{ textTransform: 'uppercase', fontSize: '11px', opacity: 0.8 }}>{t('data_path')}</span>
                            <div style={{ display: 'flex', gap: '8px' }}>
                                <button
                                    className="btn-icon"
                                    onClick={() => {
                                        open({
                                            directory: true,
                                            multiple: false,
                                            title: t('change_data_path')
                                        }).then(async (selected) => {
                                            if (selected) {
                                                const newPath = selected as string;
                                                const confirm = await ask(
                                                    t('data_move_confirm').replace('{path}', newPath),
                                                    { title: t('change_data_path'), kind: 'warning', okLabel: t('confirm'), cancelLabel: t('cancel') }
                                                );

                                                if (confirm) {
                                                    try {
                                                        // Logic Update:
                                                        // We DO NOT copy the file here because the DB is locked/in-use.
                                                        // Instead, we just set the path and restart.
                                                        // The backend 'main.rs' startup logic will handle the migration (copying)
                                                        // if it detects a custom path with no DB using the default DB as source.

                                                        await invoke("set_data_path", { newPath });

                                                        await message(
                                                            t('data_move_success'),
                                                            { title: t('notice'), kind: 'info' }
                                                        );

                                                        await invoke("relaunch");
                                                    } catch (e: unknown) {
                                                        console.error(e);
                                                        const errorMsg = e instanceof Error ? e.message : String(e);
                                                        await message(
                                                            t('data_move_failed').replace('{e}', errorMsg),
                                                            { title: t('error'), kind: 'error' }
                                                        );
                                                    }
                                                }
                                            }
                                        });
                                    }}
                                    style={{ width: 'auto', padding: '4px 12px', fontSize: '10px', textTransform: 'uppercase', height: '24px' }}
                                >
                                    {t('change_app')}
                                </button>
                                <button
                                    className="btn-icon"
                                    onClick={() => invoke("open_data_folder").catch(console.error)}
                                    title={t('open_folder') || "Open"}
                                    style={{ width: 'auto', padding: '4px 12px', fontSize: '10px', textTransform: 'uppercase', height: '24px' }}
                                >
                                    {t('open_folder')}
                                </button>
                            </div>
                        </div>
                        <div className="data-panel" style={{ fontSize: '11px', color: 'var(--text-secondary)', wordBreak: 'break-all' }}>
                            {dataPath}
                        </div>
                    </div>

                    {/*
                      备份与恢复：导出 zip / 导入即完全恢复。
                      导入是破坏性操作，因此按钮旁始终写明"会先自动备份当前数据"，
                      且导入走「只读预览 → 二次确认 → 落地」三步。
                    */}
                    <div className="setting-item column no-border">
                        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: '8px' }}>
                            <span className="item-label" style={{ textTransform: 'uppercase', fontSize: '11px', opacity: 0.8 }}>
                                {t('backup_section')}
                            </span>
                            {preflight && (
                                <span style={{ fontSize: '10px', color: 'var(--text-secondary)' }}>
                                    {t('backup_preflight')
                                        .replace('{files}', String(preflight.managedFiles))
                                        .replace('{size}', formatBytes(preflight.managedBytes))}
                                </span>
                            )}
                        </div>

                        <div style={{ fontSize: '11px', color: 'var(--text-secondary)', marginBottom: '8px', lineHeight: 1.5 }}>
                            {t('backup_intro')}
                        </div>

                        <div style={{ display: 'flex', gap: '8px', marginBottom: '10px', flexWrap: 'wrap' }}>
                            <button
                                className="btn-icon"
                                disabled={backupBusy !== null}
                                onClick={handleExport}
                                style={{ width: 'auto', padding: '4px 12px', fontSize: '10px', height: '26px', display: 'flex', alignItems: 'center', gap: '6px' }}
                            >
                                <Download size={12} />
                                {backupBusy === 'export' ? t('backup_export_running') : t('backup_export')}
                            </button>
                            <button
                                className="btn-icon"
                                disabled={backupBusy !== null}
                                onClick={handleImport}
                                style={{ width: 'auto', padding: '4px 12px', fontSize: '10px', height: '26px', display: 'flex', alignItems: 'center', gap: '6px' }}
                            >
                                <Upload size={12} />
                                {backupBusy === 'import' ? t('backup_import_running') : t('backup_import')}
                            </button>
                        </div>

                        <div style={{ fontSize: '10px', color: 'var(--text-secondary)', opacity: 0.85, lineHeight: 1.5, marginBottom: '6px' }}>
                            {t('backup_export_hint')}
                        </div>
                        <div style={{ fontSize: '10px', color: 'var(--text-secondary)', opacity: 0.85, lineHeight: 1.5, marginBottom: '6px' }}>
                            {t('backup_import_hint')}
                        </div>

                        {/* 自定义背景图在数据目录之外：提前告知会被一并打包，避免用户以为丢了 */}
                        {preflight?.backgroundOutside && preflight.backgroundPath && (
                            <div style={{ fontSize: '10px', color: 'var(--text-secondary)', opacity: 0.9, lineHeight: 1.5, marginBottom: '6px' }}>
                                {t('backup_background_outside').replace('{path}', preflight.backgroundPath)}
                            </div>
                        )}

                        <div style={{ fontSize: '10px', color: 'var(--text-secondary)', opacity: 0.75, lineHeight: 1.5 }}>
                            {t('backup_datapath_note')}
                        </div>

                        {/* 导出结果：路径、条目数与校验和——用户能核对自己拿到了什么 */}
                        {lastBackup && (
                            <div
                                style={{
                                    border: '1px solid rgba(64,160,96,0.5)',
                                    borderRadius: '6px',
                                    padding: '8px 10px',
                                    marginTop: '8px',
                                    fontSize: '10px',
                                    lineHeight: 1.6,
                                    wordBreak: 'break-all',
                                }}
                            >
                                <div style={{ fontWeight: 600, marginBottom: '4px' }}>{t('backup_export')}</div>
                                <div>
                                    {t('backup_export_path').replace('{path}', lastBackup.outputPath)}
                                </div>
                                <div>
                                    {t('backup_preflight')
                                        .replace('{files}', String(lastBackup.entriesWritten))
                                        .replace('{size}', formatBytes(lastBackup.bytesWritten))}
                                </div>
                                <button
                                    className="btn-icon"
                                    onClick={() => invoke('reveal_path', { path: lastBackup.outputPath }).catch(console.error)}
                                    style={{ width: 'auto', padding: '4px 12px', fontSize: '10px', height: '24px', marginTop: '6px' }}
                                >
                                    {t('backup_reveal')}
                                </button>
                            </div>
                        )}

                        {/* 导入结果：明确写出"导入前的数据备份在哪"，这是可回退性的凭据 */}
                        {lastRestore && (
                            <div
                                style={{
                                    border: '1px solid rgba(64,160,96,0.5)',
                                    borderRadius: '6px',
                                    padding: '8px 10px',
                                    marginTop: '8px',
                                    fontSize: '10px',
                                    lineHeight: 1.6,
                                    wordBreak: 'break-all',
                                }}
                            >
                                <div style={{ fontWeight: 600, marginBottom: '4px' }}>
                                    {t('backup_import')}
                                </div>
                                <div>
                                    {t('backup_preflight')
                                        .replace('{files}', String(lastRestore.restoredFiles))
                                        .replace('{size}', formatBytes(lastRestore.restoredBytes))}
                                </div>
                                {lastRestore.preRestoreBackup ? (
                                    <div>
                                        {t('backup_import_done')
                                            .replace('{files}', String(lastRestore.restoredFiles))
                                            .replace('{size}', formatBytes(lastRestore.restoredBytes))
                                            .replace('{backup}', lastRestore.preRestoreBackup)}
                                    </div>
                                ) : (
                                    <div>{t('backup_import_done_no_backup')
                                        .replace('{files}', String(lastRestore.restoredFiles))
                                        .replace('{size}', formatBytes(lastRestore.restoredBytes))}</div>
                                )}
                                {/* 需求明确要求"导入后必须重置"（WAL/SHM、迁移、云同步游标），
                                    这些是用户可核查的事实，因此必须回执，而不是只写在后端日志里。 */}
                                {lastRestore.resetsApplied?.length > 0 && (
                                    <div style={{ marginTop: '4px' }}>
                                        <div>{t('backup_import_resets')}</div>
                                        <ul style={{ margin: '2px 0 0 16px', padding: 0 }}>
                                            {lastRestore.resetsApplied.map((r) => (
                                                <li key={r}>{r}</li>
                                            ))}
                                        </ul>
                                    </div>
                                )}
                                {typeof lastRestore.verifiedEntries === 'number' && (
                                    <div>
                                        {t('backup_import_verified').replace(
                                            '{verified}',
                                            String(lastRestore.verifiedEntries)
                                        )}
                                    </div>
                                )}
                                {lastRestore.warnings.map((w) => (
                                    <div key={w} style={{ opacity: 0.85 }}>
                                        {w}
                                    </div>
                                ))}
                                {lastRestore.restartRequired && (
                                    <>
                                        <div style={{ marginTop: '4px', fontWeight: 600 }}>
                                            {t('backup_import_restart')}
                                        </div>
                                        <button
                                            className="btn-icon"
                                            onClick={() => invoke('relaunch').catch(console.error)}
                                            style={{ width: 'auto', padding: '4px 12px', fontSize: '10px', height: '24px', marginTop: '6px' }}
                                        >
                                            {t('backup_import_restart_now')}
                                        </button>
                                    </>
                                )}
                            </div>
                        )}
                    </div>

                    {/*
                      迁移中心：应用改名后遗留的旧数据目录。
                      始终渲染（不再以"是否发现了旧目录"为条件）——「选择其它目录…」
                      正是用户手动指定旧路径的入口，该入口必须随时可用。
                    */}
                    <div className="setting-item column no-border">
                        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: '8px' }}>
                            <span className="item-label" style={{ textTransform: 'uppercase', fontSize: '11px', opacity: 0.8 }}>
                                {t('migration_center')}
                            </span>
                            {legacyDirs.length > 0 && (
                                <span style={{ fontSize: '10px', color: 'var(--text-secondary)' }}>
                                    {t('legacy_dir_total').replace('{size}', formatBytes(totalBytes))}
                                </span>
                            )}
                        </div>

                        <div style={{ fontSize: '11px', color: 'var(--text-secondary)', marginBottom: '8px', lineHeight: 1.5 }}>
                            {t('legacy_dir_intro')}
                        </div>

                        <div style={{ display: 'flex', gap: '8px', marginBottom: '10px', flexWrap: 'wrap' }}>
                            <button
                                className="btn-icon"
                                disabled={busyPath !== null}
                                onClick={handleChooseDir}
                                style={{ width: 'auto', padding: '4px 12px', fontSize: '10px', height: '26px', display: 'flex', alignItems: 'center', gap: '6px' }}
                            >
                                <FolderInput size={12} />
                                {t('legacy_migrate_choose')}
                            </button>
                            <button
                                className="btn-icon"
                                onClick={() => invoke("open_data_folder").catch(console.error)}
                                style={{ width: 'auto', padding: '4px 12px', fontSize: '10px', height: '26px', display: 'flex', alignItems: 'center', gap: '6px' }}
                            >
                                <FolderOpen size={12} />
                                {t('legacy_migrate_open_target')}
                            </button>
                        </div>

                        {legacyDirs.length === 0 && (
                            <div style={{ fontSize: '10px', color: 'var(--text-secondary)', opacity: 0.85, lineHeight: 1.5, marginBottom: '8px' }}>
                                {t('legacy_dir_none')}
                            </div>
                        )}

                        {legacyDirs.map((dir) => (
                            <div
                                key={dir.path}
                                style={{
                                    border: '1px solid var(--border-color, rgba(128,128,128,0.25))',
                                    borderRadius: '6px',
                                    padding: '8px 10px',
                                    marginBottom: '8px',
                                }}
                            >
                                <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', gap: '8px' }}>
                                    <div style={{ minWidth: 0, flex: 1 }}>
                                        <div style={{ fontSize: '11px', fontWeight: 600, marginBottom: '2px' }}>
                                            {dir.identifier}
                                            {/* 来源标签：如实告诉用户这是"谁的数据"。
                                                两类来源的处置不同——旧版 TieZ 的可以清理，
                                                历史版本 Tiez-Next 的不能，所以必须分得清。 */}
                                            <span
                                                style={{
                                                    marginLeft: '6px',
                                                    fontSize: '9px',
                                                    fontWeight: 500,
                                                    padding: '1px 5px',
                                                    borderRadius: '3px',
                                                    background: 'var(--bg-main, var(--bg-element))',
                                                    color: 'var(--text-secondary)',
                                                    verticalAlign: 'middle',
                                                }}
                                            >
                                                {originText(dir.origin)}
                                            </span>
                                        </div>
                                        <div style={{ fontSize: '10px', color: 'var(--text-secondary)', wordBreak: 'break-all' }}>
                                            {dir.path}
                                        </div>
                                        <div style={{ fontSize: '10px', color: 'var(--text-secondary)', marginTop: '2px' }}>
                                            {t('legacy_dir_usage')
                                                .replace('{size}', formatBytes(dir.bytes))
                                                .replace('{files}', String(dir.files))}
                                            {dir.has_database ? ` · ${t('legacy_dir_has_db')}` : ''}
                                        </div>
                                    </div>
                                    <div style={{ display: 'flex', gap: '6px', flexShrink: 0, alignItems: 'center' }}>
                                        {/* 主操作：迁移（只读源，不改动原版） */}
                                        <button
                                            className="btn-icon"
                                            title={t('legacy_migrate_hint')}
                                            disabled={busyPath !== null}
                                            onClick={() => handleMigrate(dir.path, dir.identifier)}
                                            style={{ width: 'auto', padding: '4px 10px', fontSize: '10px', height: '24px', display: 'flex', alignItems: 'center', gap: '6px' }}
                                        >
                                            <CopyCheck size={12} />
                                            {t('legacy_migrate')}
                                        </button>
                                        <button
                                            className="btn-icon"
                                            title={t('open_folder')}
                                            onClick={() => invoke("open_folder", { path: dir.path }).catch(console.error)}
                                            style={{ width: 'auto', padding: '4px 8px', height: '24px' }}
                                        >
                                            <FolderOpen size={12} />
                                        </button>
                                        {/* 危险操作：删除原版数据。
                                            刻意与「迁移」拉开距离并弱化配色 —— 两者语义相反
                                            （一个只读源、一个销毁源），并排放置容易被当成同一件事。
                                            本应用自己标识符的目录（`canDelete === false`）整组不渲染：
                                            那是用户留着的旧版 Tiez-Next 数据，不是被取代的旧应用，
                                            清理按钮不该销毁它。后端同样会拒绝，这里是第一道防线。 */}
                                        {dir.canDelete && (
                                            <>
                                                <span
                                                    aria-hidden="true"
                                                    style={{ width: '1px', height: '16px', background: 'var(--border-color, rgba(128,128,128,0.3))' }}
                                                />
                                                <button
                                                    className="btn-icon"
                                                    title={t('legacy_dir_delete_hint')}
                                                    disabled={busyPath === dir.path}
                                                    onClick={() => handleRemove(dir)}
                                                    style={{
                                                        width: 'auto',
                                                        padding: '4px 8px',
                                                        height: '24px',
                                                        opacity: 0.7,
                                                        color: 'var(--danger-color, #c05050)',
                                                    }}
                                                >
                                                    <Trash2 size={12} />
                                                </button>
                                            </>
                                        )}
                                    </div>
                                </div>
                            </div>
                        ))}

                        {/* 迁移结果：用户点完按钮必须能核对具体发生了什么 */}
                        {lastResult && (
                            <div
                                style={{
                                    border: `1px solid ${
                                        lastResult.status === "migrated"
                                            ? "rgba(64,160,96,0.5)"
                                            : lastResult.status === "failed"
                                            ? "rgba(200,80,80,0.5)"
                                            : "var(--border-color, rgba(128,128,128,0.25))"
                                    }`,
                                    borderRadius: '6px',
                                    padding: '8px 10px',
                                    marginBottom: '8px',
                                    fontSize: '10px',
                                    lineHeight: 1.6,
                                    wordBreak: 'break-all',
                                }}
                            >
                                <div style={{ fontWeight: 600, marginBottom: '4px' }}>
                                    {lastResult.status === "migrated"
                                        ? t('legacy_migrate_result_migrated')
                                        : lastResult.status === "failed"
                                        ? t('legacy_migrate_result_failed')
                                        : t('legacy_migrate_result_skipped')}
                                </div>
                                <div>
                                    {lastResult.status === "migrated" &&
                                    lastResult.deliveredFiles === 0 &&
                                    lastResult.keptExisting > 0
                                        ? t('legacy_migrate_result_already_present')
                                        : t('legacy_migrate_result_files')
                                              .replace(
                                                  '{files}',
                                                  String(lastResult.deliveredFiles)
                                              )
                                              .replace(
                                                  '{size}',
                                                  formatBytes(lastResult.deliveredBytes)
                                              )}
                                </div>
                                {lastResult.status === "migrated" && lastResult.keptExisting > 0 && (
                                    <div>
                                        {t('legacy_migrate_result_kept').replace(
                                            '{files}',
                                            String(lastResult.keptExisting)
                                        )}
                                    </div>
                                )}
                                <div>
                                    {t('legacy_migrate_result_source').replace('{path}', lastResult.source)}
                                </div>
                                <div>
                                    {t('legacy_migrate_result_target').replace('{path}', lastResult.target)}
                                </div>
                                {lastResult.status === "migrated" && (
                                    <>
                                        <div style={{ marginTop: '4px' }}>{t('legacy_migrate_source_safe')}</div>
                                        <div>
                                            {lastResult.pathsRewritten
                                                ? t('legacy_migrate_paths_rewritten')
                                                : t('legacy_migrate_paths_not_rewritten')}
                                        </div>
                                        {lastResult.supersededDb && (
                                            <div>
                                                {t('legacy_migrate_superseded').replace(
                                                    '{path}',
                                                    lastResult.supersededDb
                                                )}
                                            </div>
                                        )}
                                        {lastResult.rewriteError && (
                                            <div>
                                                {t('legacy_migrate_rewrite_warning').replace(
                                                    '{e}',
                                                    lastResult.rewriteError
                                                )}
                                            </div>
                                        )}
                                        {lastResult.restartRequired && (
                                            <div style={{ marginTop: '4px', fontWeight: 600 }}>
                                                {t('legacy_migrate_restart')}
                                            </div>
                                        )}
                                    </>
                                )}
                                {lastResult.status === "skipped" && (
                                    <div style={{ marginTop: '4px' }}>
                                        {skipReasonText(lastResult.skipReason)}
                                    </div>
                                )}
                                {lastResult.status === "failed" && lastResult.error && (
                                    <>
                                        <div style={{ marginTop: '4px' }}>{lastResult.error}</div>
                                        {/* Windows 上文件被应用占用时改名/覆盖会失败；失败是安全的
                                            （源与目标都保留），但用户需要知道下一步该做什么。 */}
                                        <div style={{ marginTop: '4px', opacity: 0.85 }}>
                                            {t('legacy_migrate_failed_hint')}
                                        </div>
                                    </>
                                )}
                                {lastResult.restartRequired && lastResult.status === "migrated" && (
                                    <button
                                        className="btn-icon"
                                        onClick={() => invoke("relaunch").catch(console.error)}
                                        style={{ width: 'auto', padding: '4px 12px', fontSize: '10px', height: '24px', marginTop: '8px' }}
                                    >
                                        {t('legacy_migrate_restart_now')}
                                    </button>
                                )}
                            </div>
                        )}

                        <div style={{ fontSize: '10px', color: 'var(--text-secondary)', opacity: 0.85, lineHeight: 1.5 }}>
                            {t('legacy_dir_backup_note')}
                        </div>
                    </div>
                </div>
            )}
        </div>
    );
};

export default DataSettingsGroup;
