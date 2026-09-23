import { useEffect, useState } from "react";
import { open, ask, message } from "@tauri-apps/plugin-dialog";
import { invoke } from "@tauri-apps/api/core";
import {
    ChevronDown,
    ChevronRight,
    CopyCheck,
    FolderInput,
    FolderOpen,
    Trash2,
} from "lucide-react";

interface DataSettingsGroupProps {
    t: (key: string) => string;
    collapsed: boolean;
    onToggle: () => void;
    dataPath: string;
}

/** 一条历史数据目录信息（对应后端 `list_legacy_data_dirs`）。 */
interface LegacyDir {
    path: string;
    identifier: string;
    bytes: number;
    files: number;
    has_database: boolean;
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
    files: number;
    bytes: number;
    skipReason: string | null;
    error: string | null;
    pathsRewritten: boolean;
    rewriteError: string | null;
    sourceUntouched: boolean;
    restartRequired: boolean;
    supersededDb: string | null;
}

/** 把字节数格式化为人类可读形式。 */
const formatBytes = (bytes: number): string => {
    if (!bytes) return "0 B";
    const units = ["B", "KB", "MB", "GB", "TB"];
    const i = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
    const value = bytes / Math.pow(1024, i);
    return `${value >= 100 || i === 0 ? Math.round(value) : value.toFixed(1)} ${units[i]}`;
};

const DataSettingsGroup = ({ t, collapsed, onToggle, dataPath }: DataSettingsGroupProps) => {
    // 迁移中心：旧标识符遗留的数据目录。加载失败不阻塞设置面板其他部分。
    const [legacyDirs, setLegacyDirs] = useState<LegacyDir[]>([]);
    const [busyPath, setBusyPath] = useState<string | null>(null);
    // 最近一次手动迁移的结果：用户点完按钮必须能核对自己刚才到底做成了什么，
    // 而不是只看一句"成功/失败"。
    const [lastResult, setLastResult] = useState<MigrationReport | null>(null);

    const refreshLegacyDirs = () => {
        invoke<LegacyDir[]>("list_legacy_data_dirs")
            .then(setLegacyDirs)
            .catch((e) => {
                console.error("list_legacy_data_dirs failed:", e);
                setLegacyDirs([]);
            });
    };

    useEffect(() => {
        // 展开时才查询：折叠状态下不做无谓的磁盘统计。
        if (!collapsed) refreshLegacyDirs();
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
                    `${t("legacy_migrate_done")
                        .replace("{files}", String(report.files))
                        .replace("{size}", formatBytes(report.bytes))}\n\n${t(
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
                                    <div style={{ display: 'flex', gap: '6px', flexShrink: 0 }}>
                                        <button
                                            className="btn-icon"
                                            title={t('legacy_migrate')}
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
                                        <button
                                            className="btn-icon"
                                            title={t('legacy_dir_delete')}
                                            disabled={busyPath === dir.path}
                                            onClick={() => handleRemove(dir)}
                                            style={{ width: 'auto', padding: '4px 8px', height: '24px' }}
                                        >
                                            <Trash2 size={12} />
                                        </button>
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
                                    {t('legacy_migrate_result_files')
                                        .replace('{files}', String(lastResult.files))
                                        .replace('{size}', formatBytes(lastResult.bytes))}
                                </div>
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
                                    <div style={{ marginTop: '4px' }}>{lastResult.error}</div>
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
