const CONFIG = { enabled: true, intervalMinutes: 30, maxKeep: 20, backupOnStartup: true };
const ENTRIES = [
  { archiveName: "Tiez-Next-auto-timed-20260923T120000-01.zip", path: "/home/u/.local/share/com.tiez.next/auto_backups/Tiez-Next-auto-timed-20260923T120000-01.zip", origin: "scheduled", createdAt: "2026-09-23T12:00:00+08:00", createdAtMs: 1758600000000, createdAtLocal: "2026-09-23 12:00:00", sizeBytes: 2048, pinned: false, seq: 1 },
  { archiveName: "Tiez-Next-auto-startup-20260923T090000-01-p.zip", path: "/home/u/.local/share/com.tiez.next/auto_backups/Tiez-Next-auto-startup-20260923T090000-01-p.zip", origin: "startup", createdAt: "2026-09-23T09:00:00+08:00", createdAtMs: 1758589200000, createdAtLocal: "2026-09-23 09:00:00", sizeBytes: 3174400, pinned: true, seq: 1 },
  { archiveName: "Tiez-Next-auto-timed-20260923T083000-01.zip", path: "/home/u/.local/share/com.tiez.next/auto_backups/Tiez-Next-auto-timed-20260923T083000-01.zip", origin: "scheduled", createdAt: "2026-09-23T08:30:00+08:00", createdAtMs: 1758587400000, createdAtLocal: "2026-09-23 08:30:00", sizeBytes: 1048576, pinned: false, seq: 1 },
  { archiveName: "Tiez-Next-auto-manual-20260923T081500-01.zip", path: "/home/u/.local/share/com.tiez.next/auto_backups/Tiez-Next-auto-manual-20260923T081500-01.zip", origin: "manual", createdAt: "2026-09-23T08:15:00+08:00", createdAtMs: 1758586500000, createdAtLocal: "2026-09-23 08:15:00", sizeBytes: 972800, pinned: false, seq: 1 },
];

/**
 * 对端消息：让 `FileTransferChatView` 真的渲染出对端头像，从而能把
 * `--wt-peer-gradient-*` 量到 computed `background-image` 上。
 * 三条的 `sender_id` 有意不同，避免"只量一条"掩盖缺陷。
 */
const CHAT_HISTORY = [
  { id: 1, direction: "in", msg_type: "text", content: "这台电脑在吗？", timestamp: 1758600000000, sender_id: "mobile-1", sender_name: "iPhone" },
  { id: 2, direction: "in", msg_type: "text", content: "在的，收到。", timestamp: 1758600060000, sender_id: "mobile-2", sender_name: "Android" },
  { id: 3, direction: "in", msg_type: "text", content: "第二条消息。", timestamp: 1758600120000, sender_id: "mobile-3", sender_name: "平板" },
  { id: 4, direction: "out", msg_type: "text", content: "已发给你。", timestamp: 1758600180000, sender_id: "pc" },
];

export const invoke = async (cmd: string) => {
  switch (cmd) {
    case "get_auto_backup_config": return CONFIG;
    case "get_chat_history": return CHAT_HISTORY;
    case "get_app_logo": return "";
    case "get_download_url": return "http://192.168.1.20:51820/dl";
    case "list_auto_backups": return { dir: "/home/u/.local/share/com.tiez.next/auto_backups", config: CONFIG, maxPinned: 19, pinnedCount: 1, totalCount: ENTRIES.length, entries: ENTRIES, warnings: [] };
    case "set_auto_backup_config": return CONFIG;
    case "set_auto_backup_pinned": return true;
    case "plugin:app|version": return "0.5.0";
    case "backup_preflight": return { dataDir: "/home/u/.local/share/com.tiez.next", managedBytes: 3174400, managedFiles: 12, backgroundOutside: false, backgroundPath: null };
    case "list_legacy_data_dirs": return [
      { path: "/home/u/.local/share/com.tiez.next", identifier: "com.tiez.next", origin: "previous_tiez_next", bytes: 3174400, files: 12, has_database: true, canDelete: false },
      { path: "/home/u/.local/share/com.tiez", identifier: "com.tiez", origin: "legacy_tiez", bytes: 921600, files: 7, has_database: true, canDelete: true },
    ];
    default: return undefined;
  }
};
export const convertFileSrc = (p: string) => p;
