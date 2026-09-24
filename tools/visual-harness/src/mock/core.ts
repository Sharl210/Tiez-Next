/** 让验证台能渲染出真实进度条的固定快照（仅用于量样式，不代表真实时序）。 */
const MIGRATION_PROGRESS = {
  stage: "copying",
  stageLabel: "正在复制数据",
  done: 128,
  total: 512,
  bytes: 13002342,
  bytesTotal: 50436504,
  message: null,
};

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
    // MCP 状态：`McpSettingsGroup` 会读 `status.running` / `status.endpoint` 等字段，
    // 缺这条会渲染出 `TypeError: ... reading 'running'` 盖在量测区域上，
    // 干扰截图取色（本文件曾缺它）。
    // 系统级设置清单：返回"一项满足、一项待处理、一项无法确认"三种状态，
    // 好让验证台能同时量到三种呈现（否则只会渲染出其中一种，漏掉另两种的样式问题）。
    /**
     * 全库标签（含**尚无条目的**）。
     *
     * 这里刻意放两个"没有任何条目在用"的标签（`ims-未使用` / `img-未使用`）。
     * 主页面的候补池曾经**只**从 history 的 `item.tags` 收集，于是这类标签
     * 在标签管理页看得到、在主页面打标签时搜不到 —— 用户的原话是
     * "怎么输入都只有这个"。带上它们，量测才有判别力。
     */
    case "get_all_tags_info": return {
      ims: 3, img: 1, invoice: 0, "ims-未使用": 0, "img-未使用": 0,
    };
    case "get_system_checklist": return [
      { id: "firewall", satisfied: false, probeOk: true, detail: "Action: Block", ack: false },
      { id: "uac", satisfied: false, probeOk: false, detail: "UAC 探测仅支持 Windows", ack: false },
      { id: "startup_approved", satisfied: true, probeOk: true, detail: "首字节 = 0x02", ack: false },
    ];
    case "ack_system_checklist_item": return null;
    case "get_mcp_status": return {
      running: true,
      enabled: true,
      port: 23123,
      endpoint: "http://127.0.0.1:23123/mcp",
      lanEndpoint: "http://192.168.1.20:23123/mcp",
      requireToken: false,
      allowWrite: true,
      allowLan: false,
      autostart: false,
      token: "mock-token-0000",
      uptimeSec: 3600,
    };
    case "set_mcp_server_enabled": return true;
    case "set_mcp_port": return true;
    case "set_mcp_require_token": return true;
    case "set_mcp_allow_write": return true;
    case "set_mcp_allow_lan": return true;
    case "set_mcp_autostart": return true;
    case "regenerate_mcp_token": return "mock-token-1111";
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
    // 迁移进度快照：视觉验证台只用来量渲染与样式。
    // ?mig=determinate 给一帧 `128/512` 的可计量进度；?mig=indeterminate 给 `total === 0`。
    // ?result=deferred|done|failed 时让迁移命令返回对应报告，用来量结果卡片的**真实边框色**
    // （类名对不代表颜色对，本仓库踩过 var() 静默失效的坑）。
    case "migrate_from_data_dir": {
      const kind = new URLSearchParams(location.search).get("result") ?? "deferred";
      const base = {
        source: "/home/u/.local/share/com.tiez",
        target: "/home/u/.local/share/com.tiez.next",
        files: 12, bytes: 9216000, deliveredFiles: 12, deliveredBytes: 9216000, keptExisting: 0,
        skipReason: null, error: null, pathsRewritten: false, rewriteError: null,
        sourceUntouched: true, restartRequired: false, supersededDb: null,
      };
      if (kind === "deferred") return { ...base, status: "deferred", pendingUntilRestart: true };
      if (kind === "failed") return { ...base, status: "failed", error: "错误：目标数据库被占用（错误码 32）", pendingUntilRestart: false };
      return { ...base, status: "done", pendingUntilRestart: false, pathsRewritten: true };
    }
    // 进度快照**只在显式要求时**才给：真实后端也只在进行中的迁移上回一帧。
    // 一直回非终态会让"进行中"恒为真、迁移按钮永久禁用 —— 那是设计使然，
    // 但会让结果卡片的量测点不到按钮。?mig=determinate 量可计量那一支；
    // ?mig=indeterminate 量 `total === 0`（即"不确定进度"）那一支。
    case "get_migration_progress": {
      const mig = new URLSearchParams(location.search).get("mig");
      if (mig === "indeterminate")
        return { ...MIGRATION_PROGRESS, stage: "deferred", stageLabel: "数据已就绪，等待重启接管", done: 0, total: 0, bytes: 0, bytesTotal: 0 };
      if (mig === "determinate") return MIGRATION_PROGRESS;
      return null;
    }
    default: return undefined;
  }
};
export const convertFileSrc = (p: string) => p;
