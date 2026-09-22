import { useEffect, useState } from "react";
import { open, ask, message } from "@tauri-apps/plugin-dialog";
import { invoke } from "@tauri-apps/api/core";
import { ChevronDown, ChevronRight, FolderOpen, Trash2 } from "lucide-react";

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

                    {/* 迁移中心：应用改名后遗留的旧数据目录 */}
                    {legacyDirs.length > 0 && (
                        <div className="setting-item column no-border">
                            <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: '8px' }}>
                                <span className="item-label" style={{ textTransform: 'uppercase', fontSize: '11px', opacity: 0.8 }}>
                                    {t('migration_center')}
                                </span>
                                <span style={{ fontSize: '10px', color: 'var(--text-secondary)' }}>
                                    {t('legacy_dir_total').replace('{size}', formatBytes(totalBytes))}
                                </span>
                            </div>

                            <div style={{ fontSize: '11px', color: 'var(--text-secondary)', marginBottom: '8px', lineHeight: 1.5 }}>
                                {t('legacy_dir_intro')}
                            </div>

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

                            <div style={{ fontSize: '10px', color: 'var(--text-secondary)', opacity: 0.85, lineHeight: 1.5 }}>
                                {t('legacy_dir_backup_note')}
                            </div>
                        </div>
                    )}
                </div>
            )}
        </div>
    );
};

export default DataSettingsGroup;
