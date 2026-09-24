use crate::app::commands::file_cmd::{image_ext_from_mime, save_emoji_favorite_bytes_to_dir};
use crate::database::{is_sensitive_key, DbState};
use crate::domain::models::ClipboardEntry;
use crate::error::{AppError, AppResult};
use crate::infrastructure::repository::clipboard_repo::ClipboardRepository;
use crate::infrastructure::repository::settings_repo::SettingsRepository;
use base64::Engine;
use regex::Regex;
use reqwest::{Client, Method, RequestBuilder, Response, StatusCode};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tokio::time::sleep;
use urlencoding::decode;

const DEFAULT_INTERVAL_SECS: u64 = 120;
const MIN_INTERVAL_SECS: u64 = 5;
const MAX_INTERVAL_SECS: u64 = 3600;
const DEFAULT_SNAPSHOT_INTERVAL_MIN: i64 = 720;
const MIN_SNAPSHOT_INTERVAL_MIN: i64 = 5;
const MAX_SNAPSHOT_INTERVAL_MIN: i64 = 1440;
const SYNC_FETCH_PAGE_SIZE: i32 = 1000;
const DEFAULT_WEBDAV_BASE_PATH: &str = "tiez-sync";
const MAX_REMOTE_SNAPSHOTS: usize = 24;
const MAX_INLINE_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const RICH_IMAGE_FALLBACK_PREFIX: &str = "<!--TIEZ_RICH_IMAGE:";
const RICH_IMAGE_FALLBACK_SUFFIX: &str = "-->";
const WEBDAV_OP_BATCH_SIZE: usize = 400;
const EMOJI_FAVORITES_SETTING_KEY: &str = "app.emoji_favorites";
const CLOUD_SYNC_WEBDAV_LOCAL_SEQ_KEY: &str = "cloud_sync_webdav_local_seq";
const CLOUD_SYNC_WEBDAV_OP_CURSOR_MAP_KEY: &str = "cloud_sync_webdav_op_cursor_map";
const CLOUD_SYNC_WEBDAV_BLOB_CACHE_KEY: &str = "cloud_sync_webdav_blob_cache";
const CLOUD_SYNC_WEBDAV_LAST_SNAPSHOT_PUSH_AT_KEY: &str = "cloud_sync_webdav_last_snapshot_push_at";
const CLOUD_SYNC_WEBDAV_LAST_SNAPSHOT_PULL_AT_KEY: &str = "cloud_sync_webdav_last_snapshot_pull_at";
const CLOUD_SYNC_WEBDAV_LAST_HEAD_REBUILD_AT_KEY: &str = "cloud_sync_webdav_last_head_rebuild_at";
const BLOB_KIND_IMAGE: &str = "image";
const BLOB_KIND_CONTENT: &str = "content";
const BLOB_KIND_HTML: &str = "html";
const BLOB_THRESHOLD_CONTENT: usize = 12 * 1024;
const BLOB_THRESHOLD_HTML: usize = 24 * 1024;
const WEBDAV_REQUEST_TIMEOUT_SECS: u64 = 45;
const WEBDAV_MAX_RETRIES: usize = 3;
const WEBDAV_JSON_READ_RETRIES: usize = 3;
const WEBDAV_RETRY_BASE_DELAY_MS: u64 = 600;
const WEBDAV_HEAD_REBUILD_INTERVAL_SECS: i64 = 5 * 60;
const WEBDAV_HEAD_FILENAME: &str = "head.json";
const WEBDAV_BLOB_CACHE_MAX_ENTRIES: usize = 5000;

static CLOUD_SYNC_TASK_ACTIVE: AtomicBool = AtomicBool::new(false);
static CLOUD_SYNC_REQUESTED: AtomicBool = AtomicBool::new(false);
static CLOUD_SYNC_CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);
static CLOUD_SYNC_LAST_SYNC_AT: AtomicI64 = AtomicI64::new(0);
static LAST_PUSHED_EMOJI_HASH: AtomicI64 = AtomicI64::new(0);
static CLOUD_SYNC_BACKOFF_UNTIL: AtomicI64 = AtomicI64::new(0);


// 用于记录在本次运行中，哪些 WebDAV 目录已经确认存在，避免重复发网络请求
static WEBDAV_KNOWN_DIRS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloudSyncProvider {
    #[allow(dead_code)]
    Http,
    WebDav,
}

impl CloudSyncProvider {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::WebDav => "webdav",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudSyncStatus {
    pub state: String, // disabled | idle | syncing | error
    pub running: bool,
    pub last_sync_at: Option<i64>,
    pub last_error: Option<String>,
    pub uploaded_items: usize,
    pub received_items: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CloudSyncContentPrefs {
    #[serde(default = "default_cloud_sync_pref_true")]
    text: bool,
    #[serde(default = "default_cloud_sync_pref_true")]
    image: bool,
    #[serde(rename = "file_path", default = "default_cloud_sync_pref_true")]
    file_path: bool,
    #[serde(default = "default_cloud_sync_pref_true")]
    emoji: bool,
}

const fn default_cloud_sync_pref_true() -> bool {
    true
}

impl Default for CloudSyncContentPrefs {
    fn default() -> Self {
        Self {
            text: true,
            image: true,
            file_path: true,
            emoji: true,
        }
    }
}

impl CloudSyncContentPrefs {
    fn includes_content_type(&self, content_type: &str) -> bool {
        if !is_cloud_clipboard_content_type(content_type) {
            return false;
        }
        match content_type {
            "image" => self.image,
            "file" | "video" => self.file_path,
            "emoji_sync" => self.emoji,
            "text" | "code" | "url" | "rich_text" => self.text,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
struct CloudSyncConfig {
    enabled: bool,
    auto_sync: bool,
    provider: CloudSyncProvider,
    base_url: String,
    api_key: String,
    device_id: String,
    interval_secs: u64,
    snapshot_interval_secs: i64,
    cursor: i64,
    webdav_url: String,
    webdav_username: String,
    webdav_password: String,
    webdav_base_path: String,
    content_prefs: CloudSyncContentPrefs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CloudSyncItem {
    pub content_type: String,
    pub content: String,
    #[serde(default)]
    pub content_hash: i64,
    #[serde(default)]
    pub deleted_at: i64,
    #[serde(default)]
    pub html_content: Option<String>,
    #[serde(default)]
    pub content_blob_hash: Option<String>,
    #[serde(default)]
    pub html_blob_hash: Option<String>,
    pub source_app: String,
    pub timestamp: i64,
    pub preview: String,
    #[serde(default)]
    pub is_pinned: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub use_count: i32,
    #[serde(default)]
    pub pinned_order: i64,
    /// User remark. Defaulted so that sync payloads from older builds
    /// (which do not carry this field) still deserialize.
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Serialize)]
struct CloudSyncRequest {
    device_id: String,
    cursor: i64,
    entries: Vec<CloudSyncItem>,
}

#[derive(Debug, Deserialize)]
struct CloudSyncResponse {
    #[serde(default)]
    cursor: Option<i64>,
    #[serde(default)]
    entries: Vec<CloudSyncItem>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WebDavDeviceSnapshot {
    device_id: String,
    updated_at: i64,
    #[serde(default)]
    latest_op_seq: i64,
    entries: Vec<CloudSyncItem>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WebDavSettingsSnapshot {
    device_id: String,
    updated_at: i64,
    settings: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct WebDavPaths {
    devices_path: String,
    settings_path: String,
    ops_path: String,
    head_path: String,
    blobs_path: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct WebDavOpsBatch {
    device_id: String,
    seq: i64,
    updated_at: i64,
    entries: Vec<CloudSyncItem>,
}

#[derive(Debug, Clone)]
struct WebDavOpRef {
    device_id: String,
    seq: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct WebDavDeviceHead {
    #[serde(default)]
    latest_op_seq: i64,
    #[serde(default)]
    snapshot_updated_at: i64,
    #[serde(default)]
    snapshot_op_seq: i64,
    #[serde(default)]
    settings_updated_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct WebDavSyncHead {
    #[serde(default)]
    updated_at: i64,
    #[serde(default)]
    devices: BTreeMap<String, WebDavDeviceHead>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn status_store() -> &'static Mutex<CloudSyncStatus> {
    static STORE: OnceLock<Mutex<CloudSyncStatus>> = OnceLock::new();
    STORE.get_or_init(|| {
        Mutex::new(CloudSyncStatus {
            state: "disabled".to_string(),
            running: false,
            last_sync_at: None,
            last_error: None,
            uploaded_items: 0,
            received_items: 0,
        })
    })
}

fn sync_run_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn disabled_status() -> CloudSyncStatus {
    CloudSyncStatus {
        state: "disabled".to_string(),
        running: false,
        last_sync_at: None,
        last_error: None,
        uploaded_items: 0,
        received_items: 0,
    }
}

fn cloud_sync_cancel_requested() -> bool {
    CLOUD_SYNC_CANCEL_REQUESTED.load(Ordering::Relaxed)
}

fn emit_status(app: Option<&AppHandle>, mut next: CloudSyncStatus) {
    if next.last_sync_at.is_none() {
        let ts = CLOUD_SYNC_LAST_SYNC_AT.load(Ordering::Relaxed);
        if ts > 0 {
            next.last_sync_at = Some(ts);
        }
    }
    if let Ok(mut guard) = status_store().lock() {
        *guard = next.clone();
    }
    if let Some(handle) = app {
        let _ = handle.emit("cloud-sync-status", next);
    }
}

fn parse_interval_secs(raw: Option<String>) -> u64 {
    let parsed = raw
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_INTERVAL_SECS);
    parsed.clamp(MIN_INTERVAL_SECS, MAX_INTERVAL_SECS)
}

fn parse_snapshot_interval_secs(raw: Option<String>) -> i64 {
    let parsed_min = raw
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(DEFAULT_SNAPSHOT_INTERVAL_MIN)
        .clamp(MIN_SNAPSHOT_INTERVAL_MIN, MAX_SNAPSHOT_INTERVAL_MIN);
    parsed_min.saturating_mul(60)
}

fn normalize_webdav_base_path(raw: &str) -> String {
    let trimmed = raw.trim().trim_matches('/');
    if trimmed.is_empty() {
        DEFAULT_WEBDAV_BASE_PATH.to_string()
    } else {
        trimmed.to_string()
    }
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

fn get_blob_path(base_blobs: &str, kind: &str, hash: &str) -> String {
    let prefix = if hash.len() >= 2 { &hash[0..2] } else { "xx" };
    format!("{}/{}/{}_{}.blob", base_blobs, prefix, kind, hash)
}

fn blob_cache_storage_key(cfg: &CloudSyncConfig, relative_path: &str) -> String {
    format!(
        "{}|{}|{}",
        cfg.webdav_url.trim_end_matches('/'),
        normalize_webdav_base_path(&cfg.webdav_base_path),
        relative_path
    )
}

fn get_config(app: &AppHandle) -> Option<CloudSyncConfig> {
    let db_state = app.try_state::<DbState>()?;
    let enabled = db_state
        .settings_repo
        .get("cloud_sync_enabled")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    let auto_sync = db_state
        .settings_repo
        .get("cloud_sync_auto")
        .ok()
        .flatten()
        .map(|v| v != "false")
        .unwrap_or(true);

    // HTTP provider is intentionally disabled for now.
    // TODO: Restore provider switching after a real HTTP sync service is available.
    let provider = CloudSyncProvider::WebDav;

    let base_url = db_state
        .settings_repo
        .get("cloud_sync_server")
        .ok()
        .flatten()
        .unwrap_or_default()
        .trim()
        .to_string();

    let api_key = db_state
        .settings_repo
        .get("cloud_sync_api_key")
        .ok()
        .flatten()
        .unwrap_or_default();

    let cursor = db_state
        .settings_repo
        .get("cloud_sync_cursor")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);

    let interval_secs = parse_interval_secs(
        db_state
            .settings_repo
            .get("cloud_sync_interval_sec")
            .ok()
            .flatten(),
    );
    let snapshot_interval_secs = parse_snapshot_interval_secs(
        db_state
            .settings_repo
            .get("cloud_sync_snapshot_interval_min")
            .ok()
            .flatten(),
    );

    let stored_device_id = db_state.settings_repo.get("app.anon_id").ok().flatten();
    let device_id = stored_device_id
        .as_deref()
        .and_then(crate::app::system::normalize_anon_id)
        .unwrap_or_else(
            || crate::app::system::build_anon_id(&crate::app::system::get_machine_id()),
        );
    let should_persist_device_id = stored_device_id
        .as_deref()
        .map(|value| value.trim() != device_id)
        .unwrap_or(true);
    let did_migrate_device_id = stored_device_id
        .as_deref()
        .map(|value| value.trim() != device_id)
        .unwrap_or(false);

    if should_persist_device_id {
        let _ = db_state.settings_repo.set("app.anon_id", &device_id);
    }
    if did_migrate_device_id {
        if let Ok(conn) = db_state.conn.lock() {
            let _ = conn.execute("DELETE FROM cloud_sync_local_index", []);
        }
    }

    let webdav_url = db_state
        .settings_repo
        .get("cloud_sync_webdav_url")
        .ok()
        .flatten()
        .unwrap_or_default()
        .trim()
        .to_string();
    let webdav_username = db_state
        .settings_repo
        .get("cloud_sync_webdav_username")
        .ok()
        .flatten()
        .unwrap_or_default();
    let webdav_password = db_state
        .settings_repo
        .get("cloud_sync_webdav_password")
        .ok()
        .flatten()
        .unwrap_or_default();
    let webdav_base_path = normalize_webdav_base_path(
        &db_state
            .settings_repo
            .get("cloud_sync_webdav_base_path")
            .ok()
            .flatten()
            .unwrap_or_else(|| DEFAULT_WEBDAV_BASE_PATH.to_string()),
    );

    let content_prefs = db_state
        .settings_repo
        .get("cloud_sync_content_prefs")
        .ok()
        .flatten()
        .map(|raw| serde_json::from_str::<CloudSyncContentPrefs>(&raw).unwrap_or_default())
        .unwrap_or_default();

    Some(CloudSyncConfig {
        enabled,
        auto_sync,
        provider,
        base_url: base_url.clone(),
        api_key: api_key.clone(),
        device_id,
        interval_secs,
        snapshot_interval_secs,
        cursor,
        webdav_url: if webdav_url.is_empty() {
            base_url.clone()
        } else {
            webdav_url
        },
        webdav_username,
        webdav_password: if webdav_password.trim().is_empty() {
            api_key
        } else {
            webdav_password
        },
        webdav_base_path,
        content_prefs,
    })
}

fn is_cloud_clipboard_content_type(content_type: &str) -> bool {
    matches!(
        content_type,
        "text" | "code" | "url" | "rich_text" | "image" | "file" | "video" | "emoji_sync"
    )
}

// =============================================================================
// 【存量风险：一次已经发生过的凭据外流，以及"要不要告诉用户"的判据】
//
// 事实链（全部由本仓代码可证，不依赖任何猜测）：
//
// 1. 本函数的排除表一度只列了 `SENSITIVE_KEYS` 5 项里的 2 项
//    （`cloud_sync_api_key` / `cloud_sync_webdav_password`），另外 3 项——
//    `mqtt_password`、`mqtt_username`、`ai_profiles`——是**放行**的。
// 2. `collect_syncable_settings`（本文件）在**每次快照推送**时把放行的设置整体
//    放进 `WebDavSettingsSnapshot.settings` 并上传到用户自己的 WebDAV 空间；
//    同时 `apply_synced_settings` 用同一个判据，因此那 3 项也能被远端快照写回本机。
// 3. 快照推送的周期是 `cloud_sync_snapshot_interval_min`（默认 720 分钟），
//    而 `should_push_webdav_snapshot` 在"从未推送过"（记录为 0）时**立即**返回 true。
//
// 因此：**只要云同步曾经成功推送过一次快照，那 3 项就已经在用户的云端存储里了**，
// 无论它们当时是什么内容。修复只能阻止将来的上传，不能撤回已上传的内容——这就是
// "升级告知"要处理的东西。
//
// 下面这组函数**只读本机可观察的事实**，不做任何网络请求，也不猜测云端有什么。
// =============================================================================

/// 诊断结果：这台机器是否**曾经可能**发生过那次外流。
///
/// 为什么要返回一个结构体而不是 `bool`：告知文案与测试都需要区分"判定的依据是什么"。
/// 一个裸 bool 会让"因为完全没配过云同步所以不提示"和"因为早就提示过所以不提示"
/// 在日志与测试里无法区分，而这正是最容易写错的两处。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialExposureNotice {
    /// 是否需要向用户提示。**只有 `evidence != NotConfigured` 且尚未提示过时为 true。**
    pub should_notify: bool,
    /// 判定依据（见 `ExposureEvidence`）。
    pub evidence: ExposureEvidence,
    /// 本次是否已被标记为"提示过"（标记由调用方的 `mark` 参数控制）。
    pub acknowledged: bool,
    /// 那 3 个键里，**当前**在本机仍然存在的有哪些。
    ///
    /// 【它不是判据，只是文案材料】一个被外流的键用户后来删掉了，仍然算外流；
    /// 反之，本机现在有值也不代表当初就被上传过。因此它绝不参与 `should_notify` 的
    /// 判定——这条边界由 `stored_credential_keys_never_drive_the_decision` 钉住。
    pub stored_credential_keys: Vec<String>,
}

/// 判定依据。按"证据强度"从强到弱排列，`NotConfigured` 是**唯一**不提示的一档。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExposureEvidence {
    /// 本机留有明确的"确实推送过快照"痕迹（见 `CREDENTIAL_EXPOSURE_EVIDENCE_KEYS`）。
    /// **最强的一档**：上传动作自己写下的记录，与"当前配置成什么样"无关。
    ConfirmedSyncHistory,
    /// 配置过（开关开过，或留了服务器地址），但本机看不到任何"确实推送过"的记录。
    /// **仍然提示**：用户可能在关闭、清空或恢复备份之前已经同步过一次。
    ConfiguredOnly,
    /// 既没有配置痕迹、也没有上传痕迹：本地开关为假、服务器地址为空。
    /// 说明 `get_config` 在过去任何时刻都不可能产出 `enabled` 的配置，
    /// 因此**不存在**"曾经推送过快照"的路径。这是唯一安全的不提示档。
    NotConfigured,
}

/// 判定为"确实推送过快照"的本机痕迹键。
///
/// 【为什么是这四个而不是那几个"更像"的计数键】
///
/// 只有**由真实上传动作**写入的键才能证明上传发生过。逐个核对本仓的写入点：
///
/// * `cloud_sync_webdav_last_snapshot_push_at` —— 写在
///   `upload_webdav_settings_snapshot(...)` **返回 Ok 之后**的那几行里。它就是我们要的
///   最强的证据：设置快照上传成功过。
/// * `cloud_sync_settings_applied_at` —— 写在 `pull_remote_settings_snapshot*` 应用远端
///   设置快照之后。它证明"**这台**机器收到过别台机器的设置快照"，而外流的方向正是
///   源机器上传、别的机器收到；因此它同样是外流的证据。
/// * `cloud_sync_webdav_local_seq` / `cloud_sync_cursor` —— 分别由上传 op 批次与
///   同步主循环末尾写入。它们是**弱证据**（本身只表示"跑过一次同步"），但都在
///   "曾经配过且跑过"这一侧，加上不会把 `NotConfigured` 误判成"配过"。
///
/// 【为什么不能只看 `cloud_sync_settings_applied_at` 这一个】
///
/// 它只覆盖"有第二台机器拉到了快照"这一种情形。单机用户（或另一台机器还没开机）
/// 推送成功了却没有任何人拉取，本机就只剩 `..._push_at`。只取一个键会**漏判**这批人，
/// 而漏判的代价是"该被告知的人没被告知"。多取几个键只会把"配过但没同步过"的用户
/// 多算进来一次（可接受，见 `ConfiguredOnly`），不会把"完全没配过"的人误算进来。
///
/// 【为什么不复用备份的 `CLOUD_SYNC_RESET_KEYS`（`services/backup/import.rs`）】
///
/// 那份清单是"恢复备份后必须重置"的键，它的成员会随备份语义变化（例如某个键将来
/// 不再由备份管理就会被删掉）。把安全判据挂在别人的重置清单上，等于让一次无关的
/// 备份改动静默改掉判定结果。两者**当前部分重叠是巧合，不是契约**。
pub(crate) const CREDENTIAL_EXPOSURE_EVIDENCE_KEYS: &[&str] = &[
    CLOUD_SYNC_WEBDAV_LAST_SNAPSHOT_PUSH_AT_KEY,
    "cloud_sync_settings_applied_at",
    CLOUD_SYNC_WEBDAV_LOCAL_SEQ_KEY,
    "cloud_sync_cursor",
];

/// 被外流过的三个键的**展示顺序**。
///
/// 顺序取自 `database::SENSITIVE_KEYS` 的语义分组（MQTT 凭据在前，AI 配置在后），
/// 与实际内容无关；测试按集合比较，不依赖这里的顺序。
pub(crate) const CREDENTIAL_EXPOSURE_SUBJECT_KEYS: &[&str] = &[
    "mqtt_password",
    "mqtt_username",
    "ai_profiles",
];

/// 一次性标记键：提示过一次就不再提示。
///
/// 取 `security.` 前缀而不是往 `app.*` 里加一条：本键回答的是"一条安全告知发过没有"，
/// 与外观/行为偏好无关。
///
/// **它必须同时被两道"不得云同步"的判据挡住**，否则这条安全告知可以被远端静默吞掉：
///
/// * `is_setting_sync_eligible` 里的 `security.` **整族前缀排除**（本文件）；
/// * MCP 的 `set_setting` 拒写清单（`services/mcp/mod.rs`），挡住"本机 agent 顺手改掉它"。
///
/// 【我最初写错的地方，留在这里当教训】第一版注释断言"不在白名单里，默认拒绝"——
/// **那是错的**：`is_setting_sync_eligible` 是**排除法**（`!matches!(...)`），
/// 未列出的键默认**放行**。于是这个标记键会被上传、也能被远端快照写回，
/// 一个被篡改的远端就能把它改成 `true` 来让用户永远看不到这条告知。
/// 这正是本文件反复在讲的同一个失效模式（"枚举式排除会随着新键悄悄失效"），
/// 我自己又踩了一次；`acknowledgement_key_cannot_be_synced_from_the_cloud` 现在把它钉住。
pub(crate) const CREDENTIAL_EXPOSURE_ACK_KEY: &str =
    "security.credential_exposure_2026_notice_ack";

/// 安全类设置的**整族前缀**：一律不参与云同步（按前缀，不逐键列举）。
///
/// 理由与 `mcp.*`、`auto_backup.*` 两个前缀完全相同：这组键描述的是"**这台机器**的安全
/// 处置状态"，而不是用户的跨机器偏好；而云同步会把远端写回的键持久化到本机，
/// 于是远端能改写本机的安全状态。
pub(crate) const SECURITY_SETTING_KEY_PREFIX: &str = "security.";

/// 纯判定：把"本机可观察的事实"映射成诊断结果。**不碰数据库，不碰网络。**
///
/// 抽成纯函数是刻意的：命令函数需要 `State<DbState>`，单测里造不出来；而这里全部
/// 需要被钉住的边界（配过 vs 没配过、有痕迹 vs 没痕迹、已提示 vs 未提示）都是纯粹
/// 的输入→输出关系。把判定留在纯函数里，"确实可能受影响必须提示 / 肯定没受影响
/// 不得提示"这两条才能真正被逐格断言。
pub(crate) fn credential_exposure_notice(
    cloud_sync_enabled: bool,
    cloud_sync_server_configured: bool,
    any_sync_evidence: bool,
    already_acknowledged: bool,
    stored_credential_keys: &[String],
) -> CredentialExposureNotice {
    // 【判定顺序：先看"动作痕迹"，再看"配置"】
    //
    // 顺序是这条判据里最容易写错的一处，因此显式写出来并有两格测试钉住
    // （`confirmed_snapshot_push_always_prompts` 与
    // `never_configured_cloud_sync_never_prompts`）：
    //
    // 若先把"开关关且地址空"判成 `NotConfigured`，那么**"配过、同步成功过、之后把开关
    // 关掉并把地址清空"**的机器（换云盘、迁移账号、清理界面时都很常见）会被判成
    // "从未配置过"而**不提示**——而它恰恰是最确定已经外流过的一批。痕迹键是上传动作
    // 留下的，它比任何"当前配置"都更接近事实，所以它优先。
    let evidence = if any_sync_evidence {
        ExposureEvidence::ConfirmedSyncHistory
    } else if cloud_sync_enabled || cloud_sync_server_configured {
        ExposureEvidence::ConfiguredOnly
    } else {
        ExposureEvidence::NotConfigured
    };

    let should_notify = matches!(
        evidence,
        ExposureEvidence::ConfiguredOnly | ExposureEvidence::ConfirmedSyncHistory
    ) && !already_acknowledged;

    CredentialExposureNotice {
        should_notify,
        evidence,
        acknowledged: already_acknowledged,
        stored_credential_keys: stored_credential_keys.to_vec(),
    }
}

/// 从"全部设置"这张表里算出诊断。**纯函数**（没有数据库、没有 Tauri）。
pub(crate) fn credential_exposure_notice_from_map(
    settings: &HashMap<String, String>,
) -> CredentialExposureNotice {
    let get = |key: &str| {
        settings
            .get(key)
            .map(|v| v.trim().to_string())
            .unwrap_or_default()
    };

    let enabled = get("cloud_sync_enabled").eq_ignore_ascii_case("true");
    // 两个地址键任一非空即算"配置过"：`get_config` 用 `cloud_sync_server` 作为基地址，
    // 而界面真正写的是 `cloud_sync_webdav_url`，两个都留着。
    let server_configured =
        !get("cloud_sync_server").is_empty() || !get("cloud_sync_webdav_url").is_empty();

    // 痕迹键都是毫秒/序号整数，"非零"即"写过"。解析失败按"没写过"处理：
    // 宁可漏判一档（CodeOnly 仍然提示），也不把垃圾值当成证据。
    let any_evidence = CREDENTIAL_EXPOSURE_EVIDENCE_KEYS.iter().any(|key| {
        get(key)
            .parse::<i64>()
            .map(|v| v > 0)
            .unwrap_or(false)
    });

    let acknowledged = get(CREDENTIAL_EXPOSURE_ACK_KEY).eq_ignore_ascii_case("true");

    let stored: Vec<String> = CREDENTIAL_EXPOSURE_SUBJECT_KEYS
        .iter()
        .filter(|key| !get(key).is_empty())
        .map(|key| (*key).to_string())
        .collect();

    credential_exposure_notice(
        enabled,
        server_configured,
        any_evidence,
        acknowledged,
        &stored,
    )
}

/// 从真实的设置仓储读出诊断（命令层与单测共用这一条读取路径）。
///
/// 【为什么这一层要存在，而不是在命令里 `get_all()` 一把】命令函数需要
/// `State<DbState>`（活的 Tauri 环境），单测里造不出来。把"读哪些键、怎么比"放在这一层，
/// 测试就能用**真实的内存 SQLite + 真实 `SqliteSettingsRepository`** 走完整条读取路径；
/// 换成一个记录调用的假仓储就会把被测对象换掉——本仓已经踩过这个坑
/// （见 `settings_cmd.rs` 里 `reset_settings_preserves_every_mcp_key` 的注释）。
pub(crate) fn assess_credential_exposure_from_repo(
    repo: &impl SettingsRepository,
) -> AppResult<CredentialExposureNotice> {
    let all = repo.get_all().map_err(AppError::from)?;
    Ok(credential_exposure_notice_from_map(&all))
}

/// 落下"已经提示过"的一次性标记。
///
/// 【为什么这里不做"先查再写"的判断】是否该写由调用方按 `notice.should_notify` 决定；
/// 本函数只负责把一个布尔写进去，写失败向上抛（命令层据此告诉前端"没记上"，
/// 于是下一次启动还会提示一次——重复一次无害，静默丢失一次有害）。
pub(crate) fn mark_credential_exposure_acknowledged(
    repo: &impl SettingsRepository,
) -> AppResult<()> {
    repo.set(CREDENTIAL_EXPOSURE_ACK_KEY, "true")
        .map_err(AppError::from)
}

pub(crate) fn is_setting_sync_eligible(key: &str) -> bool {
    // 【凭据类一律排除，且与加密侧共用同一个判据】
    //
    // `is_sensitive_key` 是"要不要加密存储"的判据，它命中的那批键正是最不该离开本机的
    // 一批。此前这里**没有**这道检查，而排除表只列出了 5 个敏感键里的 2 个
    // （`cloud_sync_api_key` / `cloud_sync_webdav_password`），于是
    // `mqtt_password`、`mqtt_username`、`ai_profiles` 三项会**上传到云端**，
    // 并且会被远端快照**写回本机**（`apply_synced_settings` 用同一个判据放行）。
    //
    // 用 `is_sensitive_key` 而不是往排除表里再补三行：SENSITIVE_KEYS 是"凭据"这件事的
    // 唯一定义处，往这里抄一份就是第二个定义，将来新增凭据键必然只改一处、另一处静默漏掉
    // ——本函数上方的两段注释正是在讲同一个失效模式（前缀排除优于逐键列举）。
    if is_sensitive_key(key) {
        return false;
    }
    // 【整族排除 MCP 设置】按前缀而不是逐键列举。
    //
    // 这些键里含有 `mcp.token`（访问令牌）与 `mcp.allow_write`、`mcp.allow_lan`
    // 这类**安全姿态**。云同步会把远端写回的设置落进本地 settings 并持久化，于是
    // 一个被篡改的远端快照就能把"免鉴权 + 允许写入 + 开放局域网"种进这台机器，
    // 等下次 MCP 重启时生效——而用户从未在本机做过这个选择。
    //
    // 逐键列举在这里是错的：`mcp.*` 会继续增长，新增一个键就会静默重新打开这个口子，
    // 而漏掉一个键不会有任何编译错误或测试失败。按前缀排除没有这个失效模式。
    if key.starts_with("mcp.") {
        return false;
    }
    // 【整族排除自动容灾备份设置】同样按前缀。
    //
    // 这组键（`auto_backup.enabled` / `interval_minutes` / `max_keep` / `backup_on_startup`）
    // 描述的是"**这台机器**怎么保管自己的容灾副本"：多久存一份、最多留几份。它有三个
    // 不该跨机器同步的理由：
    //
    // 1. **它是本机状态，不是用户偏好**。用户要求"自动备份路径独立于手动备份"、且与数据
    //    目录同级——换句话说这套副本天生属于这台机器。把它同步出去，等于让另一台机器的
    //    份数上限决定这台机器的留存策略。
    // 2. **远端能改写本机留存策略**。云同步会把远端写回的设置落进本地 settings 并持久化。
    //    一个被篡改（或只是另一台机器上被改过）的远端快照就能把 `max_keep` 从 200 变成 1：
    //    下一次轮换会把本机几乎所有容灾副本删掉。这是**用户从未在"这台"机器上做过的选择**。
    // 3. **方向与 mcp.\* 同理**：这两个前缀都保护"本机的安全/自保姿态不被远端改写"。
    //
    // 用前缀而不是逐键列举：这四个键将来会增长（例如新增"仅在有变更时备份"），逐键列举
    // 漏一个不会有编译错误或测试失败，前缀排除没有这个失效模式。
    if key.starts_with(crate::services::auto_backup::config::KEY_PREFIX) {
        return false;
    }
    // 【整族排除安全处置状态】——`security.*`，同样按前缀。
    //
    // 这一族回答的是"**这台机器**对某件安全事件的处置到了哪一步"，`security.*` 命名空间
    // 就是为它开的。当前成员是"存量凭据外流的告知是否已经展示过"这一个标记。
    //
    // 【为什么它必须被排除，而不是"顺手同步一下也无所谓"】
    //
    // `apply_synced_settings` 会把远端快照里的键写进本机并持久化。只要这个标记键在
    // 同步范围内，一个被篡改的（或只是另一台机器上被点过"知道了"的）远端快照就能把它
    // 写成 `true`，于是**这台机器的用户永远看不到那条凭据外流的告知**——而告知的全部
    // 意义就是让本人知道并换掉密码。安全告知的送达状态必须只由本机决定。
    //
    // 【这里是本任务真实踩过的坑，逐字记下来】
    //
    // 我第一版把这个标记当作"不在白名单里，因此默认拒绝"。**错**：本函数是排除法
    // （末尾 `!matches!(...)`），未列出的键默认**放行**。写这段注释时，作者本人刚在
    // 同一条推理上翻过一次车——所以这里用**前缀**而不是再加一行 `| "security.xxx"`：
    // 逐键列举漏一个不会有编译错误，前缀不会漏。
    if key.starts_with(SECURITY_SETTING_KEY_PREFIX) {
        return false;
    }
    !matches!(
        key,
        "app.anon_id"
            | "app.emoji_favorites"
            | "app.last_ping_date"
            | "app.window_width"
            | "app.window_height"
            | "app.tag_manager_size"
            | "cloud_sync_enabled"
            | "cloud_sync_auto"
            | "cloud_sync_provider"
            | "cloud_sync_server"
            | "cloud_sync_api_key"
            | "cloud_sync_interval_sec"
            | "cloud_sync_snapshot_interval_min"
            | "cloud_sync_cursor"
            | "cloud_sync_webdav_url"
            | "cloud_sync_webdav_username"
            | "cloud_sync_webdav_password"
            | "cloud_sync_webdav_base_path"
            | "cloud_sync_content_prefs"
            | "cloud_sync_webdav_local_seq"
            | "cloud_sync_webdav_op_cursor_map"
            | "cloud_sync_webdav_blob_cache"
            | "cloud_sync_webdav_last_snapshot_push_at"
            | "cloud_sync_webdav_last_snapshot_pull_at"
            | "cloud_sync_webdav_last_head_rebuild_at"
            | "cloud_sync_settings_applied_at"
    )
}

fn to_data_url_from_path(path: &str) -> Option<String> {
    let file_path = Path::new(path);
    if !file_path.exists() || !file_path.is_file() {
        return None;
    }

    let bytes = std::fs::read(file_path).ok()?;
    if bytes.is_empty() || bytes.len() > MAX_INLINE_IMAGE_BYTES {
        return None;
    }

    let mime = mime_guess::from_path(file_path)
        .first_or_octet_stream()
        .essence_str()
        .to_string();
    let payload = base64::engine::general_purpose::STANDARD.encode(bytes);
    Some(format!("data:{};base64,{}", mime, payload))
}

fn rewrite_rich_fallback_payload_to_data_url(html: &str) -> String {
    let Some(start) = html.rfind(RICH_IMAGE_FALLBACK_PREFIX) else {
        return html.to_string();
    };
    let marker_start = start + RICH_IMAGE_FALLBACK_PREFIX.len();
    let Some(end_rel) = html[marker_start..].find(RICH_IMAGE_FALLBACK_SUFFIX) else {
        return html.to_string();
    };

    let marker_end = marker_start + end_rel;
    let payload = html[marker_start..marker_end].trim();
    if payload.is_empty()
        || payload.starts_with("data:image/")
        || payload.starts_with("http://asset.localhost/")
        || payload.starts_with("https://asset.localhost/")
    {
        return html.to_string();
    }

    let Some(data_url) = to_data_url_from_path(payload) else {
        return html.to_string();
    };

    format!(
        "{}{}{}",
        &html[..marker_start],
        data_url,
        &html[marker_end..]
    )
}

fn rich_html_resource_path_to_data_url(raw: &str) -> Option<String> {
    let value = raw.trim();
    if value.is_empty()
        || value.starts_with("data:")
        || value.starts_with("http://")
        || value.starts_with("https://")
        || value.starts_with("//")
        || value.starts_with("asset:")
        || value.starts_with("tauri:")
        || value.starts_with("blob:")
    {
        return None;
    }

    let path_raw = if value.starts_with("file://") {
        value.trim_start_matches("file://")
    } else {
        value
    };

    let path_without_drive_prefix =
        if path_raw.starts_with('/') && path_raw.chars().nth(2) == Some(':') {
            &path_raw[1..]
        } else {
            path_raw
        };

    let decoded_path = decode(path_without_drive_prefix)
        .map(|p| p.into_owned())
        .unwrap_or_else(|_| path_without_drive_prefix.to_string());
    let clean_path = decoded_path
        .split('?')
        .next()
        .unwrap_or(&decoded_path)
        .split('#')
        .next()
        .unwrap_or(&decoded_path)
        .trim();

    if clean_path.is_empty() {
        return None;
    }

    let is_absolute = clean_path.starts_with('/')
        || (clean_path.len() >= 3
            && clean_path.as_bytes()[1] == b':'
            && (clean_path.as_bytes()[2] == b'\\' || clean_path.as_bytes()[2] == b'/'));
    if !is_absolute {
        return None;
    }

    to_data_url_from_path(clean_path)
}

fn rewrite_rich_html_image_sources_to_data_url(html: &str) -> String {
    static IMG_SRC_RE: OnceLock<Regex> = OnceLock::new();
    let re = IMG_SRC_RE
        .get_or_init(|| Regex::new(r#"(?is)(<img\b[^>]*\bsrc=["'])([^"']+)(["'][^>]*>)"#).unwrap());

    re.replace_all(html, |caps: &regex::Captures| {
        let prefix = &caps[1];
        let src = &caps[2];
        let suffix = &caps[3];

        if let Some(data_url) = rich_html_resource_path_to_data_url(src) {
            format!("{}{}{}", prefix, data_url, suffix)
        } else {
            caps[0].to_string()
        }
    })
    .into_owned()
}

fn rewrite_rich_html_resources_for_sync(html: &str) -> String {
    let with_inline_images = rewrite_rich_html_image_sources_to_data_url(html);
    rewrite_rich_fallback_payload_to_data_url(&with_inline_images)
}

fn encode_emoji_favorites_setting(raw: &str) -> Option<String> {
    let paths: Vec<String> = serde_json::from_str(raw).ok()?;
    let encoded: Vec<String> = paths
        .into_iter()
        .filter_map(|path| to_data_url_from_path(path.trim()))
        .collect();
    serde_json::to_string(&encoded).ok()
}

fn materialize_emoji_favorite_paths(app: &AppHandle, raw: &str) -> AppResult<Vec<String>> {
    let items: Vec<String> = serde_json::from_str(raw)
        .map_err(|e| AppError::Validation(format!("invalid emoji favorites payload: {}", e)))?;
    let data_dir = get_app_data_dir(app)
        .ok_or_else(|| AppError::Internal("App data dir unavailable".to_string()))?;
    let mut saved_paths: Vec<String> = Vec::new();

    for item in items {
        let trimmed = item.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !trimmed.starts_with("data:") {
            let path = Path::new(trimmed);
            if path.is_file() {
                saved_paths.push(trimmed.to_string());
            }
            continue;
        }

        let (mime, bytes) = decode_data_url(trimmed)?;
        if bytes.is_empty() || bytes.len() > MAX_INLINE_IMAGE_BYTES {
            continue;
        }
        let ext = image_ext_from_mime(mime).ok_or_else(|| {
            AppError::Validation(format!("unsupported emoji mime type: {}", mime))
        })?;
        let path = save_emoji_favorite_bytes_to_dir(&data_dir, &bytes, ext)?;
        saved_paths.push(path);
    }

    saved_paths.sort();
    saved_paths.dedup();
    Ok(saved_paths)
}

fn decode_data_url(data_url: &str) -> AppResult<(&str, Vec<u8>)> {
    let Some(header_and_payload) = data_url.strip_prefix("data:") else {
        return Err(AppError::Validation("invalid data url".to_string()));
    };
    let Some((meta, payload)) = header_and_payload.split_once(',') else {
        return Err(AppError::Validation("invalid data url payload".to_string()));
    };
    if !meta.contains(";base64") {
        return Err(AppError::Validation(
            "unsupported data url encoding".to_string(),
        ));
    }
    let mime = meta.split(';').next().unwrap_or("").trim();
    if mime.is_empty() {
        return Err(AppError::Validation("missing mime type".to_string()));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|e| AppError::Validation(format!("invalid base64 payload: {}", e)))?;
    Ok((mime, bytes))
}

fn image_mime_from_bytes(bytes: &[u8]) -> Option<&'static str> {
    match image::guess_format(bytes).ok()? {
        image::ImageFormat::Png => Some("image/png"),
        image::ImageFormat::Jpeg => Some("image/jpeg"),
        image::ImageFormat::Gif => Some("image/gif"),
        image::ImageFormat::WebP => Some("image/webp"),
        image::ImageFormat::Bmp => Some("image/bmp"),
        _ => None,
    }
}

fn image_data_url_from_blob_bytes(bytes: &[u8]) -> Option<String> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        let trimmed = text.trim();
        if trimmed.starts_with("data:image/") {
            return Some(trimmed.to_string());
        }
    }

    let mime = image_mime_from_bytes(bytes)?;
    let payload = base64::engine::general_purpose::STANDARD.encode(bytes);
    Some(format!("data:{};base64,{}", mime, payload))
}

fn decode_emoji_favorites_setting(app: &AppHandle, raw: &str) -> AppResult<String> {
    let saved_paths = materialize_emoji_favorite_paths(app, raw)?;
    serde_json::to_string(&saved_paths)
        .map_err(|e| AppError::Internal(format!("serialize emoji favorites failed: {}", e)))
}

fn normalize_item_for_sync(mut item: CloudSyncItem) -> Option<CloudSyncItem> {
    if item.deleted_at > 0 {
        return Some(item);
    }

    if item.content_type == "image" && !item.content.starts_with("data:image/") {
        item.content = to_data_url_from_path(&item.content)?;
    }

    if item.content_type == "rich_text" {
        if let Some(html) = item.html_content.as_ref() {
            item.html_content = Some(rewrite_rich_html_resources_for_sync(html));
        }
    }

    Some(item)
}

fn compute_sync_content_hash(content_type: &str, content: &str) -> i64 {
    match content_type {
        "image" => crate::database::calc_image_hash(content).unwrap_or(0),
        "text" | "code" | "url" | "rich_text" | "file" | "video" => {
            crate::database::calc_text_hash(content) as i64
        }
        _ => 0,
    }
}

fn resolved_content_hash(item: &CloudSyncItem) -> i64 {
    if item.content_hash != 0 {
        item.content_hash
    } else {
        compute_sync_content_hash(&item.content_type, &item.content)
    }
}

fn sync_key_for_item(item: &CloudSyncItem) -> Option<String> {
    let hash = resolved_content_hash(item);
    if hash == 0 {
        return None;
    }
    Some(format!("{}:{}", item.content_type, hash))
}

fn sync_digest_for_item(item: &CloudSyncItem) -> String {
    let tags_json = serde_json::to_string(&item.tags).unwrap_or_else(|_| "[]".to_string());
    let html_hash = item
        .html_content
        .as_ref()
        .map(|v| crate::database::calc_text_hash(v))
        .unwrap_or(0);
    let preview_hash = crate::database::calc_text_hash(&item.preview);
    let source_hash = crate::database::calc_text_hash(&item.source_app);
    let meta = format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        resolved_content_hash(item),
        item.timestamp,
        item.deleted_at,
        item.is_pinned,
        item.pinned_order,
        item.use_count,
        html_hash,
        preview_hash,
        source_hash,
        crate::database::calc_text_hash(&tags_json)
    );
    crate::database::calc_text_hash(&meta).to_string()
}

async fn process_items_blobs_before_push(
    client: &reqwest::Client,
    cfg: &CloudSyncConfig,
    blobs_path: &str,
    blob_cache: &mut HashMap<String, i64>,
    items: &mut [CloudSyncItem],
) -> AppResult<()> {
    for item in items {
        if item.deleted_at > 0 {
            continue;
        }

        if item.content_type == "image" {
            if !item.content.starts_with("data:image/") {
                item.content = to_data_url_from_path(&item.content).ok_or_else(|| {
                    AppError::Internal("convert image path to data url failed".to_string())
                })?;
            }
            if !item.content.is_empty() {
                let relative_hash = sha256_hex(item.content.as_bytes());
                let relative = get_blob_path(blobs_path, BLOB_KIND_IMAGE, &relative_hash);
                let cache_key = blob_cache_storage_key(cfg, &relative);
                if !blob_cache.contains_key(&cache_key) {
                    upload_webdav_blob(
                        client,
                        cfg,
                        blobs_path,
                        BLOB_KIND_IMAGE,
                        item.content.as_bytes(),
                    )
                    .await?;
                }
                blob_cache.insert(cache_key, now_ms());
                let hash = relative_hash;
                item.content_blob_hash = Some(hash);
                item.content = String::new();
            }
        } else {
            let bytes = item.content.as_bytes();
            if bytes.len() > BLOB_THRESHOLD_CONTENT {
                let relative_hash = sha256_hex(bytes);
                let relative = get_blob_path(blobs_path, BLOB_KIND_CONTENT, &relative_hash);
                let cache_key = blob_cache_storage_key(cfg, &relative);
                if !blob_cache.contains_key(&cache_key) {
                    upload_webdav_blob(client, cfg, blobs_path, BLOB_KIND_CONTENT, bytes).await?;
                }
                blob_cache.insert(cache_key, now_ms());
                let hash = relative_hash;
                item.content_blob_hash = Some(hash);
                item.content = String::new();
            }
            if let Some(html) = item.html_content.as_ref() {
                let hbytes = html.as_bytes();
                if hbytes.len() > BLOB_THRESHOLD_HTML {
                    let relative_hash = sha256_hex(hbytes);
                    let relative = get_blob_path(blobs_path, BLOB_KIND_HTML, &relative_hash);
                    let cache_key = blob_cache_storage_key(cfg, &relative);
                    if !blob_cache.contains_key(&cache_key) {
                        upload_webdav_blob(client, cfg, blobs_path, BLOB_KIND_HTML, hbytes).await?;
                    }
                    blob_cache.insert(cache_key, now_ms());
                    let hash = relative_hash;
                    item.html_blob_hash = Some(hash);
                    item.html_content = None;
                }
            }
        }
    }
    Ok(())
}

async fn enrich_item_blobs_after_pull(
    app: &tauri::AppHandle,
    client: &reqwest::Client,
    cfg: &CloudSyncConfig,
    blobs_path: &str,
    items: &mut [CloudSyncItem],
) -> AppResult<()> {
    for item in items {
        if let Some(hash) = item.content_blob_hash.as_ref() {
            let kind = if item.content_type == "image" {
                BLOB_KIND_IMAGE
            } else {
                BLOB_KIND_CONTENT
            };
            let bytes = download_webdav_blob(client, cfg, blobs_path, kind, hash).await?;
            if item.content_type == "image" {
                let data_url = image_data_url_from_blob_bytes(&bytes).ok_or_else(|| {
                    AppError::Validation(format!("unsupported image blob payload: {}", hash))
                })?;
                if let Some(data_dir) = get_app_data_dir(app) {
                    if let Some(path) = crate::database::save_image_to_file(&data_url, &data_dir) {
                        item.content = path;
                    } else {
                        item.content = data_url;
                    }
                } else {
                    item.content = data_url;
                }
            } else {
                item.content = String::from_utf8(bytes).unwrap_or_default();
            }
        }

        if let Some(hash) = item.html_blob_hash.as_ref() {
            let bytes = download_webdav_blob(client, cfg, blobs_path, BLOB_KIND_HTML, hash).await?;
            item.html_content = Some(String::from_utf8(bytes).unwrap_or_default());
        }
    }
    Ok(())
}

fn collapse_items_by_sync_key(items: &[CloudSyncItem]) -> BTreeMap<String, CloudSyncItem> {
    let mut map: BTreeMap<String, CloudSyncItem> = BTreeMap::new();
    for item in items {
        let Some(key) = sync_key_for_item(item) else {
            continue;
        };
        let mut normalized = item.clone();
        normalized.content_hash = resolved_content_hash(item);

        let replace = map
            .get(&key)
            .map(|old| normalized.timestamp >= old.timestamp)
            .unwrap_or(true);
        if replace {
            map.insert(key, normalized);
        }
    }
    map
}

fn load_local_sync_index(app: &AppHandle) -> AppResult<HashMap<String, String>> {
    let db_state = app
        .try_state::<DbState>()
        .ok_or_else(|| AppError::Internal("DB state unavailable".to_string()))?;
    let conn = db_state
        .conn
        .lock()
        .map_err(|e| AppError::Internal(e.to_string()))?;

    let mut stmt = conn
        .prepare("SELECT sync_key, digest FROM cloud_sync_local_index")
        .map_err(|e| AppError::Internal(e.to_string()))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| AppError::Internal(e.to_string()))?;

    let mut index = HashMap::new();
    for row in rows {
        let (k, v) = row.map_err(|e| AppError::Internal(e.to_string()))?;
        index.insert(k, v);
    }
    Ok(index)
}

fn replace_local_sync_index(
    app: &AppHandle,
    collapsed: &BTreeMap<String, CloudSyncItem>,
) -> AppResult<()> {
    let db_state = app
        .try_state::<DbState>()
        .ok_or_else(|| AppError::Internal("DB state unavailable".to_string()))?;
    let mut conn = db_state
        .conn
        .lock()
        .map_err(|e| AppError::Internal(e.to_string()))?;
    let tx = conn
        .transaction()
        .map_err(|e| AppError::Internal(e.to_string()))?;
    tx.execute("DELETE FROM cloud_sync_local_index", [])
        .map_err(|e| AppError::Internal(e.to_string()))?;
    for (sync_key, item) in collapsed {
        let digest = sync_digest_for_item(item);
        tx.execute(
            "INSERT INTO cloud_sync_local_index (sync_key, digest) VALUES (?1, ?2)",
            rusqlite::params![sync_key, digest],
        )
        .map_err(|e| AppError::Internal(e.to_string()))?;
    }
    tx.commit().map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(())
}

fn collect_local_incremental_items(
    app: &AppHandle,
    local_items: &[CloudSyncItem],
) -> AppResult<(Vec<CloudSyncItem>, BTreeMap<String, CloudSyncItem>)> {
    let collapsed = collapse_items_by_sync_key(local_items);
    let prev_index = load_local_sync_index(app)?;

    let mut deltas = Vec::new();
    for (sync_key, item) in &collapsed {
        let digest = sync_digest_for_item(item);
        let changed = prev_index
            .get(sync_key)
            .map(|old| old != &digest)
            .unwrap_or(true);
        if changed {
            deltas.push(item.clone());
        }
    }

    deltas.sort_by_key(|item| item.timestamp);
    Ok((deltas, collapsed))
}

fn get_setting_i64(app: &AppHandle, key: &str, default: i64) -> i64 {
    app.try_state::<DbState>()
        .and_then(|db| db.settings_repo.get(key).ok().flatten())
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(default)
}

fn set_setting_i64(app: &AppHandle, key: &str, value: i64) {
    if let Some(db_state) = app.try_state::<DbState>() {
        let _ = db_state.settings_repo.set(key, &value.to_string());
    }
}

fn get_local_webdav_op_seq(app: &AppHandle) -> i64 {
    get_setting_i64(app, CLOUD_SYNC_WEBDAV_LOCAL_SEQ_KEY, 0)
}

fn set_local_webdav_op_seq(app: &AppHandle, seq: i64) {
    set_setting_i64(app, CLOUD_SYNC_WEBDAV_LOCAL_SEQ_KEY, seq);
}

fn load_webdav_op_cursor_map(app: &AppHandle) -> HashMap<String, i64> {
    let raw = app
        .try_state::<DbState>()
        .and_then(|db| {
            db.settings_repo
                .get(CLOUD_SYNC_WEBDAV_OP_CURSOR_MAP_KEY)
                .ok()
                .flatten()
        })
        .unwrap_or_default();
    if raw.trim().is_empty() {
        return HashMap::new();
    }
    serde_json::from_str::<HashMap<String, i64>>(&raw).unwrap_or_default()
}

fn save_webdav_op_cursor_map(app: &AppHandle, map: &HashMap<String, i64>) {
    if let Some(db_state) = app.try_state::<DbState>() {
        let payload = serde_json::to_string(map).unwrap_or_else(|_| "{}".to_string());
        let _ = db_state
            .settings_repo
            .set(CLOUD_SYNC_WEBDAV_OP_CURSOR_MAP_KEY, &payload);
    }
}

fn load_webdav_blob_cache(app: &AppHandle) -> HashMap<String, i64> {
    let raw = app
        .try_state::<DbState>()
        .and_then(|db| {
            db.settings_repo
                .get(CLOUD_SYNC_WEBDAV_BLOB_CACHE_KEY)
                .ok()
                .flatten()
        })
        .unwrap_or_default();
    if raw.trim().is_empty() {
        return HashMap::new();
    }
    serde_json::from_str::<HashMap<String, i64>>(&raw).unwrap_or_default()
}

fn save_webdav_blob_cache(app: &AppHandle, cache: &HashMap<String, i64>) {
    if let Some(db_state) = app.try_state::<DbState>() {
        let mut entries: Vec<(String, i64)> = cache.iter().map(|(k, v)| (k.clone(), *v)).collect();
        entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        entries.truncate(WEBDAV_BLOB_CACHE_MAX_ENTRIES);
        let payload = serde_json::to_string(&entries.into_iter().collect::<HashMap<_, _>>())
            .unwrap_or_else(|_| "{}".to_string());
        let _ = db_state
            .settings_repo
            .set(CLOUD_SYNC_WEBDAV_BLOB_CACHE_KEY, &payload);
    }
}

fn get_app_data_dir(app: &AppHandle) -> Option<std::path::PathBuf> {
    let state = app.try_state::<crate::app_state::AppDataDir>()?;
    let guard = state.0.lock().ok()?;
    Some(guard.clone())
}

fn collect_local_syncable_items(
    app: &AppHandle,
    prefs: &CloudSyncContentPrefs,
) -> AppResult<Vec<CloudSyncItem>> {
    let db_state = app
        .try_state::<DbState>()
        .ok_or_else(|| AppError::Internal("DB state unavailable".to_string()))?;

    let mut entries: Vec<ClipboardEntry> = Vec::new();
    let mut offset: i32 = 0;

    loop {
        let batch = db_state
            .repo
            .get_history(SYNC_FETCH_PAGE_SIZE, offset, None)
            .map_err(AppError::Internal)?;

        if batch.is_empty() {
            break;
        }

        let fetched = batch.len() as i32;
        entries.extend(batch.into_iter().filter(|e| {
            is_cloud_clipboard_content_type(&e.content_type)
                && prefs.includes_content_type(&e.content_type)
        }));
        offset = offset.saturating_add(fetched);
        if fetched < SYNC_FETCH_PAGE_SIZE {
            break;
        }
    }

    let mut items: Vec<CloudSyncItem> = entries
        .into_iter()
        .filter_map(|e| {
            let normalized = normalize_item_for_sync(CloudSyncItem {
                content_type: e.content_type,
                content: e.content,
                content_hash: 0,
                deleted_at: 0,
                html_content: e.html_content,
                content_blob_hash: None,
                html_blob_hash: None,
                source_app: e.source_app,
                timestamp: e.timestamp,
                preview: e.preview,
                is_pinned: e.is_pinned,
                tags: e.tags,
                use_count: e.use_count,
                pinned_order: e.pinned_order,
                note: e.note.clone(),
            })?;
            let mut item = normalized;
            item.content_hash = compute_sync_content_hash(&item.content_type, &item.content);
            Some(item)
        })
        .collect();

    let mut tombstones = collect_local_tombstones(app, prefs)?;
    items.append(&mut tombstones);
    items.sort_by_key(|e| e.timestamp);
    Ok(items)
}

fn collect_local_changes(
    app: &AppHandle,
    cursor: i64,
    prefs: &CloudSyncContentPrefs,
) -> AppResult<Vec<CloudSyncItem>> {
    let mut items = collect_local_syncable_items(app, prefs)?;
    items.retain(|e| e.timestamp > cursor);
    Ok(items)
}

fn collect_local_tombstones(
    app: &AppHandle,
    prefs: &CloudSyncContentPrefs,
) -> AppResult<Vec<CloudSyncItem>> {
    let db_state = app
        .try_state::<DbState>()
        .ok_or_else(|| AppError::Internal("DB state unavailable".to_string()))?;
    let conn = db_state
        .conn
        .lock()
        .map_err(|e| AppError::Internal(e.to_string()))?;

    let mut stmt = conn
        .prepare(
            "SELECT content_type, content_hash, deleted_at
             FROM cloud_sync_tombstones
             ORDER BY deleted_at ASC",
        )
        .map_err(|e| AppError::Internal(e.to_string()))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(CloudSyncItem {
                content_type: row.get(0)?,
                content: String::new(),
                content_hash: row.get(1)?,
                deleted_at: row.get(2)?,
                html_content: None,
                content_blob_hash: None,
                html_blob_hash: None,
                source_app: "sync".to_string(),
                timestamp: row.get(2)?,
                preview: String::new(),
                is_pinned: false,
                tags: Vec::new(),
                use_count: 0,
                pinned_order: 0,
                note: String::new(),
            })
        })
        .map_err(|e| AppError::Internal(e.to_string()))?;

    let mut items = Vec::new();
    for row in rows {
        items.push(row.map_err(|e| AppError::Internal(e.to_string()))?);
    }
    items.retain(|item| {
        is_cloud_clipboard_content_type(&item.content_type)
            && prefs.includes_content_type(&item.content_type)
    });
    Ok(items)
}

fn update_existing_entry_from_sync(
    conn: &rusqlite::Connection,
    id: i64,
    item: &CloudSyncItem,
    effective_timestamp: i64,
) -> AppResult<bool> {
    let (local_timestamp, local_is_pinned, local_pinned_order, local_preview, local_source_app, local_use_count, local_tags_json, local_source_app_path, local_is_external): (i64, bool, i64, String, String, i32, String, Option<String>, bool) = conn
        .query_row(
            "SELECT timestamp, is_pinned, pinned_order, preview, source_app, use_count, tags, source_app_path, is_external FROM clipboard_history WHERE id = ?",
            rusqlite::params![id],
            |row| Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7).unwrap_or(None),
                row.get(8).unwrap_or(false),
            )),
        )
        .map_err(|e| AppError::Internal(e.to_string()))?;

    let mut changed = false;
    let mut timestamp = local_timestamp;
    let mut is_pinned = local_is_pinned;
    let mut pinned_order = local_pinned_order;
    let mut preview = local_preview;
    let mut source_app = local_source_app;
    let mut use_count = local_use_count;
    let mut tags_json = local_tags_json.clone();
    let mut source_app_path = local_source_app_path;

    if effective_timestamp > local_timestamp {
        timestamp = effective_timestamp;
        changed = true;
    }
    if item.is_pinned != local_is_pinned {
        is_pinned = item.is_pinned;
        changed = true;
    }
    if item.pinned_order != local_pinned_order {
        pinned_order = item.pinned_order;
        changed = true;
    }
    if !item.preview.is_empty() && item.preview != preview {
        preview = item.preview.clone();
        changed = true;
    }
    if item.source_app != "sync" && item.source_app != source_app && !item.source_app.is_empty() {
        source_app = item.source_app.clone();
        source_app_path = None;
        changed = true;
    }
    if item.use_count > local_use_count {
        use_count = item.use_count;
        changed = true;
    }
    let remote_tags_json = serde_json::to_string(&item.tags).unwrap_or_else(|_| "[]".to_string());
    if remote_tags_json != tags_json {
        tags_json = remote_tags_json;
        changed = true;
    }
    let remote_is_external =
        item.content_type == "image" || item.content_type == "file" || item.content_type == "video";
    if remote_is_external != local_is_external {
        changed = true;
    }

    if changed {
        conn.execute(
            "UPDATE clipboard_history SET 
                timestamp = ?, 
                is_pinned = ?, 
                pinned_order = ?, 
                preview = ?, 
                source_app = ?, 
                use_count = ?, 
                tags = ?,
                source_app_path = ?,
                is_external = ?
             WHERE id = ?",
            rusqlite::params![
                timestamp,
                is_pinned,
                pinned_order,
                preview,
                source_app,
                use_count,
                tags_json,
                source_app_path,
                if remote_is_external { 1 } else { 0 },
                id
            ],
        )
        .map_err(|e| AppError::Internal(e.to_string()))?;

        if tags_json != local_tags_json {
            conn.execute(
                "DELETE FROM entry_tags WHERE entry_id = ?",
                rusqlite::params![id],
            )
            .map_err(|e| AppError::Internal(e.to_string()))?;
            for tag in &item.tags {
                let _ = conn.execute(
                    "INSERT OR IGNORE INTO entry_tags (entry_id, tag) VALUES (?, ?)",
                    rusqlite::params![id, tag],
                );
            }
        }
    }

    Ok(changed)
}

fn apply_remote_changes(
    app: &AppHandle,
    remote_items: &[CloudSyncItem],
    prefs: &CloudSyncContentPrefs,
) -> AppResult<usize> {
    if remote_items.is_empty() {
        return Ok(0);
    }

    let db_state = app
        .try_state::<DbState>()
        .ok_or_else(|| AppError::Internal("DB state unavailable".to_string()))?;
    let mut applied = 0usize;
    let app_data_dir = get_app_data_dir(app);
    for item in remote_items {
        if item.content_type == "emoji_sync" {
            if !prefs.emoji {
                continue;
            }
            if let Err(e) = merge_remote_emojis(app, &item.content) {
                println!("Error merging remote emojis: {}", e);
            }
            applied += 1;
            continue;
        }

        if !is_cloud_clipboard_content_type(&item.content_type) {
            continue;
        }
        if !prefs.includes_content_type(&item.content_type) {
            continue;
        }

        let conn = db_state
            .conn
            .lock()
            .map_err(|e| AppError::Internal(e.to_string()))?;
        let effective_timestamp = if item.timestamp > 0 {
            item.timestamp
        } else {
            now_ms()
        };

        let remote_hash = if item.content_hash != 0 {
            item.content_hash
        } else {
            compute_sync_content_hash(&item.content_type, &item.content)
        };

        if item.deleted_at > 0 {
            if remote_hash == 0 {
                continue;
            }
            let tombstone_ts = item.deleted_at.max(effective_timestamp);
            let _ = conn.execute(
                "INSERT INTO cloud_sync_tombstones (content_type, content_hash, deleted_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(content_type, content_hash)
                 DO UPDATE SET deleted_at = MAX(cloud_sync_tombstones.deleted_at, excluded.deleted_at)",
                rusqlite::params![item.content_type, remote_hash, tombstone_ts],
            );

            let mut stmt = conn
                .prepare(
                    "SELECT id FROM clipboard_history
                     WHERE content_type = ?1 AND content_hash = ?2",
                )
                .map_err(|e| AppError::Internal(e.to_string()))?;
            let rows = stmt
                .query_map(rusqlite::params![item.content_type, remote_hash], |row| {
                    row.get::<_, i64>(0)
                })
                .map_err(|e| AppError::Internal(e.to_string()))?;
            for row in rows {
                let id = row.map_err(|e| AppError::Internal(e.to_string()))?;
                db_state
                    .repo
                    .delete_with_conn(&conn, id, app_data_dir.as_deref())
                    .map_err(AppError::Internal)?;
                applied += 1;
            }
            continue;
        }

        if item.content.trim().is_empty() {
            continue;
        }

        if remote_hash != 0 {
            let tombstone_deleted_at = conn
                .query_row(
                    "SELECT deleted_at FROM cloud_sync_tombstones WHERE content_type = ?1 AND content_hash = ?2 LIMIT 1",
                    rusqlite::params![item.content_type, remote_hash],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0);
            if tombstone_deleted_at >= effective_timestamp.max(item.deleted_at) {
                continue;
            }
        }

        let existing = db_state
            .repo
            .find_by_content_with_conn(&conn, &item.content, Some(&item.content_type))
            .map_err(AppError::Internal)?;

        if let Some(id) = existing {
            if update_existing_entry_from_sync(&conn, id, item, effective_timestamp)? {
                applied += 1;
            }
            if remote_hash != 0 {
                let _ = conn.execute(
                    "DELETE FROM cloud_sync_tombstones
                     WHERE content_type = ?1 AND content_hash = ?2 AND deleted_at <= ?3",
                    rusqlite::params![item.content_type, remote_hash, effective_timestamp],
                );
            }
            continue;
        }

        let preview = if item.preview.trim().is_empty() {
            if item.content_type == "image" {
                "[Image Content]".to_string()
            } else {
                item.content.chars().take(200).collect::<String>()
            }
        } else {
            item.preview.clone()
        };

        let entry = ClipboardEntry {
            id: 0,
            content_type: item.content_type.clone(),
            content: item.content.clone(),
            html_content: item.html_content.clone(),
            source_app: item.source_app.clone(),
            source_app_path: None,
            timestamp: effective_timestamp,
            preview,
            is_pinned: item.is_pinned,
            tags: item.tags.clone(),
            use_count: item.use_count,
            is_external: item.content_type == "image"
                || item.content_type == "file"
                || item.content_type == "video",
            pinned_order: item.pinned_order,
            note: item.note.clone(),
            file_preview_exists: true,
        };

        db_state
            .repo
            .save_with_conn(&conn, &entry, app_data_dir.as_deref())
            .map_err(AppError::Internal)?;
        if remote_hash != 0 {
            let _ = conn.execute(
                "DELETE FROM cloud_sync_tombstones
                 WHERE content_type = ?1 AND content_hash = ?2 AND deleted_at <= ?3",
                rusqlite::params![item.content_type, remote_hash, effective_timestamp],
            );
        }
        applied += 1;
    }

    Ok(applied)
}

fn cloud_sync_target_ready(cfg: &CloudSyncConfig) -> bool {
    match cfg.provider {
        CloudSyncProvider::Http => !cfg.base_url.trim().is_empty(),
        CloudSyncProvider::WebDav => !cfg.webdav_url.trim().is_empty(),
    }
}

fn cloud_sync_target_not_ready_message(cfg: &CloudSyncConfig) -> String {
    match cfg.provider {
        CloudSyncProvider::Http => "cloud_sync_server is empty".to_string(),
        CloudSyncProvider::WebDav => "cloud_sync_webdav_url is empty".to_string(),
    }
}

fn build_http_client() -> AppResult<Client> {
    Client::builder()
        .timeout(Duration::from_secs(WEBDAV_REQUEST_TIMEOUT_SECS))
        .build()
        .map_err(|e| AppError::Network(e.to_string()))
}

fn webdav_retry_delay(attempt: usize) -> Duration {
    let factor = 1u64 << attempt.min(4);
    Duration::from_millis(WEBDAV_RETRY_BASE_DELAY_MS.saturating_mul(factor))
}

fn is_retryable_webdav_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT
            | StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::GATEWAY_TIMEOUT
    )
}

fn check_webdav_status_for_backoff(status: StatusCode) {
    if matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
    ) {
        // 进入 5 分钟冷却期，避免激怒坚果云导致封禁时间被无限延长
        let cooldown = now_ms() + 300 * 1000;
        CLOUD_SYNC_BACKOFF_UNTIL.store(cooldown, Ordering::Relaxed);
    }
}

async fn webdav_send_with_retry<F>(mut build_request: F) -> AppResult<Response>
where
    F: FnMut() -> RequestBuilder,
{
    let mut last_error = None;

    for attempt in 0..=WEBDAV_MAX_RETRIES {
        match build_request().send().await {
            Ok(resp) => {
                check_webdav_status_for_backoff(resp.status());
                if is_retryable_webdav_status(resp.status()) && attempt < WEBDAV_MAX_RETRIES {
                    last_error = Some(format!("transient WebDAV status {}", resp.status()));
                    sleep(webdav_retry_delay(attempt)).await;
                    continue;
                }
                return Ok(resp);
            }
            Err(err) => {
                last_error = Some(err.to_string());
                if attempt < WEBDAV_MAX_RETRIES {
                    sleep(webdav_retry_delay(attempt)).await;
                    continue;
                }
            }
        }
    }

    Err(AppError::Network(
        last_error.unwrap_or_else(|| "webdav request failed".to_string()),
    ))
}

fn webdav_with_auth(req: RequestBuilder, cfg: &CloudSyncConfig) -> RequestBuilder {
    if cfg.webdav_username.trim().is_empty() {
        req
    } else {
        req.basic_auth(cfg.webdav_username.trim(), Some(cfg.webdav_password.trim()))
    }
}

fn encode_webdav_relative_path(relative_path: &str, collection: bool) -> String {
    let mut encoded = relative_path
        .replace('\\', "/")
        .split('/')
        .filter_map(|segment| {
            let trimmed = segment.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(urlencoding::encode(trimmed).into_owned())
            }
        })
        .collect::<Vec<_>>()
        .join("/");

    if collection && !encoded.is_empty() {
        encoded.push('/');
    }

    encoded
}

fn webdav_resource_url_for(cfg: &CloudSyncConfig, relative_path: &str) -> String {
    let encoded = encode_webdav_relative_path(relative_path, false);
    if encoded.is_empty() {
        cfg.webdav_url.trim_end_matches('/').to_string()
    } else {
        format!("{}/{}", cfg.webdav_url.trim_end_matches('/'), encoded)
    }
}

fn webdav_collection_url_for(cfg: &CloudSyncConfig, relative_path: &str) -> String {
    let encoded = encode_webdav_relative_path(relative_path, true);
    if encoded.is_empty() {
        format!("{}/", cfg.webdav_url.trim_end_matches('/'))
    } else {
        format!("{}/{}", cfg.webdav_url.trim_end_matches('/'), encoded)
    }
}

fn webdav_url_for(cfg: &CloudSyncConfig, relative_path: &str) -> String {
    webdav_resource_url_for(cfg, relative_path)
}

async fn webdav_collection_exists(
    client: &Client,
    cfg: &CloudSyncConfig,
    relative_path: &str,
) -> AppResult<bool> {
    let method = Method::from_bytes(b"PROPFIND")
        .map_err(|e| AppError::Internal(format!("invalid PROPFIND method: {}", e)))?;
    let url = webdav_collection_url_for(cfg, relative_path);
    let resp = webdav_send_with_retry(|| {
        webdav_with_auth(
            client
                .request(method.clone(), &url)
                .header("Depth", "0")
                .header("Content-Type", "application/xml; charset=utf-8"),
            cfg,
        )
    })
    .await?;

    Ok(resp.status().is_success() || resp.status().as_u16() == 207)
}

async fn mkcol_if_needed(
    client: &Client,
    cfg: &CloudSyncConfig,
    relative_path: &str,
) -> AppResult<()> {
    // 1. 生成唯一的缓存 Key（URL + 相对路径）
    let cache_key = format!("{}:{}", cfg.webdav_url, relative_path);
    // 2. 检查缓存中是否已经记录过该目录
    {
        let cache = WEBDAV_KNOWN_DIRS.get_or_init(|| Mutex::new(HashSet::new()));
        if cache.lock().unwrap().contains(&cache_key) {
            // 如果已在缓存中，直接返回成功，不产生任何网络请求
            return Ok(());
        }
    }

    let method = Method::from_bytes(b"MKCOL")
        .map_err(|e| AppError::Internal(format!("invalid MKCOL method: {}", e)))?;
    let url = webdav_collection_url_for(cfg, relative_path);
    let resp =
        webdav_send_with_retry(|| webdav_with_auth(client.request(method.clone(), &url), cfg))
            .await?;

    let code = resp.status().as_u16();
    if resp.status().is_success() {
        // 创建成功，写入缓存
        let cache = WEBDAV_KNOWN_DIRS.get_or_init(|| Mutex::new(HashSet::new()));
        cache.lock().unwrap().insert(cache_key);
        return Ok(());
    }

    if matches!(code, 301 | 302 | 307 | 308 | 405 | 409)
        && webdav_collection_exists(client, cfg, relative_path).await?
    {
        // 如果服务器反馈目录已存在 (405) 或者发生冲突 (409)，同样记录到缓存中
        let cache = WEBDAV_KNOWN_DIRS.get_or_init(|| Mutex::new(HashSet::new()));
        cache.lock().unwrap().insert(cache_key);
        return Ok(());
    }

    let text = resp.text().await.unwrap_or_default();
    Err(AppError::Network(format!(
        "webdav MKCOL failed for {}: {} {}",
        url, code, text
    )))
}

async fn delete_webdav_resource_if_exists(
    client: &Client,
    cfg: &CloudSyncConfig,
    relative_path: &str,
) -> AppResult<()> {
    let url = webdav_url_for(cfg, relative_path);
    let resp = webdav_send_with_retry(|| webdav_with_auth(client.delete(&url), cfg)).await?;

    if resp.status().is_success() || resp.status().as_u16() == 404 {
        return Ok(());
    }

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    Err(AppError::Network(format!(
        "webdav DELETE cleanup failed for {}: {} {}",
        url, status, text
    )))
}

async fn move_webdav_resource(
    client: &Client,
    cfg: &CloudSyncConfig,
    from_relative: &str,
    to_relative: &str,
) -> AppResult<bool> {
    let from_url = webdav_url_for(cfg, from_relative);
    let destination = webdav_url_for(cfg, to_relative);
    let resp = webdav_send_with_retry(|| {
        let method = Method::from_bytes(b"MOVE").expect("MOVE is a valid HTTP method");
        webdav_with_auth(
            client
                .request(method, &from_url)
                .header("Destination", destination.clone())
                .header("Overwrite", "T"),
            cfg,
        )
    })
    .await?;

    if resp.status().is_success() {
        return Ok(true);
    }

    if matches!(resp.status().as_u16(), 405 | 409 | 412 | 501) {
        return Ok(false);
    }

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    Err(AppError::Network(format!(
        "webdav MOVE publish failed for {} -> {}: {} {}",
        from_url, destination, status, text
    )))
}

async fn upload_webdav_bytes_resource(
    client: &Client,
    cfg: &CloudSyncConfig,
    relative_path: &str,
    body: Vec<u8>,
    content_type: &str,
    label: &str,
) -> AppResult<()> {
    async fn upload_target(
        client: &Client,
        cfg: &CloudSyncConfig,
        url: &str,
        payload: &[u8],
        content_type: &str,
        label: &str,
    ) -> AppResult<()> {
        let url_owned = url.to_string();
        let payload_owned = payload.to_vec();
        let content_type_owned = content_type.to_string();

        let resp = webdav_send_with_retry(|| {
            webdav_with_auth(
                client
                    .put(&url_owned)
                    .header("Content-Type", &content_type_owned)
                    .body(payload_owned.clone()),
                cfg,
            )
        })
        .await?;

        if resp.status().is_success() {
            return Ok(());
        }

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        Err(AppError::Network(format!(
            "webdav PUT {} failed: {} {}",
            label, status, text
        )))
    }

    let final_url = webdav_url_for(cfg, relative_path);
    let temp_relative = format!(
        "{}.uploading.{}.{}.tmp",
        relative_path.trim_end_matches('/'),
        cfg.device_id,
        now_ms()
    );
    let temp_url = webdav_url_for(cfg, &temp_relative);

    upload_target(client, cfg, &temp_url, &body, content_type, label).await?;

    match move_webdav_resource(client, cfg, &temp_relative, relative_path).await {
        Ok(true) => Ok(()),
        Ok(false) => {
            let fallback = upload_target(client, cfg, &final_url, &body, content_type, label).await;
            let _ = delete_webdav_resource_if_exists(client, cfg, &temp_relative).await;
            fallback
        }
        Err(err) => {
            let _ = delete_webdav_resource_if_exists(client, cfg, &temp_relative).await;
            Err(err)
        }
    }
}

async fn upload_webdav_json_resource(
    client: &Client,
    cfg: &CloudSyncConfig,
    relative_path: &str,
    body: Vec<u8>,
    label: &str,
) -> AppResult<()> {
    upload_webdav_bytes_resource(client, cfg, relative_path, body, "application/json", label).await
}

async fn fetch_webdav_json_resource<T, F>(
    mut make_request: F,
    missing_status: u16,
    fetch_error_label: &str,
    parse_error_label: &str,
) -> AppResult<Option<T>>
where
    T: for<'de> Deserialize<'de>,
    F: FnMut() -> RequestBuilder,
{
    for attempt in 0..=WEBDAV_JSON_READ_RETRIES {
        let resp = webdav_send_with_retry(|| make_request()).await?;

        let status_code = resp.status().as_u16();
        if status_code == missing_status {
            return Ok(None);
        }
        
        // 兼容坚果云：如果父目录不存在，GET 可能返回 409 Conflict (AncestorsNotFound)
        if status_code == 409 {
            return Ok(None);
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(AppError::Network(format!(
                "{}: {} {}",
                fetch_error_label, status, text
            )));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| AppError::Network(e.to_string()))?;
        match serde_json::from_slice::<T>(&bytes) {
            Ok(parsed) => return Ok(Some(parsed)),
            Err(err)
                if matches!(err.classify(), serde_json::error::Category::Eof)
                    && attempt < WEBDAV_JSON_READ_RETRIES =>
            {
                sleep(webdav_retry_delay(attempt)).await;
            }
            Err(err) => return Err(AppError::Network(format!("{}: {}", parse_error_label, err))),
        }
    }

    Err(AppError::Network(format!(
        "{}: exhausted retries",
        parse_error_label
    )))
}

async fn ensure_webdav_directories(
    client: &Client,
    cfg: &CloudSyncConfig,
) -> AppResult<WebDavPaths> {
    let base = normalize_webdav_base_path(&cfg.webdav_base_path);
    let mut current = String::new();

    let paths = WebDavPaths {
        devices_path: if base.is_empty() {
            "devices".into()
        } else {
            format!("{}/devices", base)
        },
        settings_path: if base.is_empty() {
            "settings".into()
        } else {
            format!("{}/settings", base)
        },
        ops_path: if base.is_empty() {
            "ops".into()
        } else {
            format!("{}/ops", base)
        },
        head_path: if base.is_empty() {
            WEBDAV_HEAD_FILENAME.into()
        } else {
            format!("{}/{}", base, WEBDAV_HEAD_FILENAME)
        },
        blobs_path: if base.is_empty() {
            "blobs".into()
        } else {
            format!("{}/blobs", base)
        },
    };

    // 注意：不再使用全局静态标识 WEBDAV_ROOT_INITIALIZED 来跳过初始化，
    // 因为这会导致在切换 WebDAV 配置时无法正确为新地址创建目录。
    // 性能优化现在完全依赖 WEBDAV_KNOWN_DIRS 缓存。

    for segment in base.split('/').filter(|s| !s.is_empty()) {
        current = if current.is_empty() {
            segment.to_string()
        } else {
            format!("{}/{}", current, segment)
        };
        mkcol_if_needed(client, cfg, &current).await?;
    }

    mkcol_if_needed(client, cfg, &paths.devices_path).await?;
    mkcol_if_needed(client, cfg, &paths.settings_path).await?;
    mkcol_if_needed(client, cfg, &paths.ops_path).await?;
    mkcol_if_needed(client, cfg, &paths.blobs_path).await?;


    Ok(paths)
}

async fn upload_webdav_blob(
    client: &Client,
    cfg: &CloudSyncConfig,
    base_blobs: &str,
    kind: &str,
    data: &[u8],
) -> AppResult<String> {
    let hash = sha256_hex(data);
    let prefix = if hash.len() >= 2 { &hash[0..2] } else { "xx" };
    let prefix_path = format!("{}/{}", base_blobs, prefix);
    mkcol_if_needed(client, cfg, &prefix_path).await?;

    let blob_file_path = get_blob_path(base_blobs, kind, &hash);
    upload_webdav_bytes_resource(
        client,
        cfg,
        &blob_file_path,
        data.to_vec(),
        "application/octet-stream",
        "blob",
    )
    .await?;
    Ok(hash)
}

async fn download_webdav_blob(
    client: &Client,
    cfg: &CloudSyncConfig,
    base_blobs: &str,
    kind: &str,
    hash: &str,
) -> AppResult<Vec<u8>> {
    let blob_file_path = get_blob_path(base_blobs, kind, hash);
    let url = webdav_url_for(cfg, &blob_file_path);
    let resp = webdav_with_auth(client.get(&url), cfg)
        .send()
        .await
        .map_err(|e| AppError::Network(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(AppError::Network(format!(
            "webdav GET blob failed: {} ({})",
            resp.status(),
            blob_file_path
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| AppError::Network(e.to_string()))?;
    Ok(bytes.to_vec())
}

fn parse_webdav_snapshot_ids(xml: &str) -> Vec<String> {
    let Ok(re) = Regex::new(r"(?is)<[^>]*href[^>]*>\s*([^<]+)\s*</[^>]*href>") else {
        return Vec::new();
    };

    let mut ids = Vec::new();
    for caps in re.captures_iter(xml) {
        let Some(raw_match) = caps.get(1) else {
            continue;
        };
        let raw_href = raw_match.as_str().trim();
        if raw_href.is_empty() {
            continue;
        }

        let decoded_href = urlencoding::decode(raw_href)
            .map(|v| v.into_owned())
            .unwrap_or_else(|_| raw_href.to_string());

        let normalized = decoded_href.trim_end_matches('/');
        let Some(file_name) = normalized.rsplit('/').next() else {
            continue;
        };

        let Some(device_id) = file_name.strip_suffix(".json") else {
            continue;
        };
        if device_id.is_empty() {
            continue;
        }
        if ids.iter().any(|existing| existing == device_id) {
            continue;
        }
        ids.push(device_id.to_string());
    }
    ids
}

async fn upload_webdav_snapshot(
    client: &Client,
    cfg: &CloudSyncConfig,
    devices_path: &str,
    latest_op_seq: i64,
    local_items: &[CloudSyncItem],
) -> AppResult<()> {
    let snapshot = WebDavDeviceSnapshot {
        device_id: cfg.device_id.clone(),
        updated_at: now_ms(),
        latest_op_seq,
        entries: local_items.to_vec(),
    };
    let body = serde_json::to_vec(&snapshot)
        .map_err(|e| AppError::Internal(format!("serialize snapshot failed: {}", e)))?;

    let relative = format!(
        "{}/{}.json",
        devices_path.trim_end_matches('/'),
        cfg.device_id
    );
    upload_webdav_json_resource(client, cfg, &relative, body, "snapshot").await
}

async fn list_webdav_snapshot_ids(
    client: &Client,
    cfg: &CloudSyncConfig,
    devices_path: &str,
) -> AppResult<Vec<String>> {
    let method = Method::from_bytes(b"PROPFIND")
        .map_err(|e| AppError::Internal(format!("invalid PROPFIND method: {}", e)))?;
    let url = webdav_collection_url_for(cfg, devices_path);
    let body = r#"<?xml version="1.0" encoding="utf-8" ?>
<d:propfind xmlns:d="DAV:">
  <d:prop>
    <d:getlastmodified />
  </d:prop>
</d:propfind>"#;

    let resp = webdav_send_with_retry(|| {
        webdav_with_auth(
            client
                .request(method.clone(), &url)
                .header("Depth", "1")
                .header("Content-Type", "application/xml; charset=utf-8")
                .body(body.to_string()),
            cfg,
        )
    })
    .await?;

    let status = resp.status();
    if !status.is_success() && status.as_u16() != 207 {
        let text = resp.text().await.unwrap_or_default();
        return Err(AppError::Network(format!(
            "webdav PROPFIND failed: {} {}",
            status, text
        )));
    }

    let text = resp
        .text()
        .await
        .map_err(|e| AppError::Network(e.to_string()))?;
    Ok(parse_webdav_snapshot_ids(&text))
}

async fn fetch_webdav_snapshot(
    client: &Client,
    cfg: &CloudSyncConfig,
    devices_path: &str,
    device_id: &str,
) -> AppResult<Option<WebDavDeviceSnapshot>> {
    let relative = format!("{}/{}.json", devices_path.trim_end_matches('/'), device_id);
    let url = webdav_url_for(cfg, &relative);
    fetch_webdav_json_resource(
        || webdav_with_auth(client.get(&url), cfg),
        404,
        "webdav GET snapshot failed",
        "parse snapshot json failed",
    )
    .await
}

fn webdav_ops_filename(device_id: &str, seq: i64) -> String {
    format!("{}__{:020}.json", device_id, seq.max(0))
}

async fn upload_webdav_ops_batch(
    client: &Client,
    cfg: &CloudSyncConfig,
    ops_path: &str,
    seq: i64,
    entries: &[CloudSyncItem],
) -> AppResult<()> {
    let batch = WebDavOpsBatch {
        device_id: cfg.device_id.clone(),
        seq,
        updated_at: now_ms(),
        entries: entries.to_vec(),
    };
    let body = serde_json::to_vec(&batch)
        .map_err(|e| AppError::Internal(format!("serialize ops batch failed: {}", e)))?;
    let relative = format!(
        "{}/{}",
        ops_path.trim_end_matches('/'),
        webdav_ops_filename(&cfg.device_id, seq)
    );
    upload_webdav_json_resource(client, cfg, &relative, body, "ops batch").await
}

fn parse_webdav_op_refs(xml: &str) -> Vec<WebDavOpRef> {
    let Ok(re_href) = Regex::new(r"(?is)<[^>]*href[^>]*>\s*([^<]+)\s*</[^>]*href>") else {
        return Vec::new();
    };
    let Ok(re_file) = Regex::new(r"^(.+)__(\d+)\.json$") else {
        return Vec::new();
    };

    let mut refs: HashMap<String, WebDavOpRef> = HashMap::new();
    let _start = std::time::Instant::now();
    for caps in re_href.captures_iter(xml) {
        let Some(raw_match) = caps.get(1) else {
            continue;
        };
        let raw_href = raw_match.as_str().trim();
        if raw_href.is_empty() {
            continue;
        }

        let decoded_href = urlencoding::decode(raw_href)
            .map(|v| v.into_owned())
            .unwrap_or_else(|_| raw_href.to_string());
        let normalized = decoded_href.trim_end_matches('/');
        let Some(file_name) = normalized.rsplit('/').next() else {
            continue;
        };
        let Some(file_caps) = re_file.captures(file_name) else {
            continue;
        };
        let Some(device_id_match) = file_caps.get(1) else {
            continue;
        };
        let Some(seq_match) = file_caps.get(2) else {
            continue;
        };
        let Ok(seq) = seq_match.as_str().parse::<i64>() else {
            continue;
        };
        let device_id = device_id_match.as_str().to_string();
        let dedup_key = format!("{}:{}", device_id, seq);
        refs.entry(dedup_key)
            .or_insert(WebDavOpRef { device_id, seq });
    }

    let mut out: Vec<WebDavOpRef> = refs.into_values().collect();

    out.sort_by(|a, b| a.device_id.cmp(&b.device_id).then(a.seq.cmp(&b.seq)));
    out
}

async fn list_webdav_op_refs(
    client: &Client,
    cfg: &CloudSyncConfig,
    ops_path: &str,
) -> AppResult<Vec<WebDavOpRef>> {
    let method = Method::from_bytes(b"PROPFIND")
        .map_err(|e| AppError::Internal(format!("invalid PROPFIND method: {}", e)))?;
    let url = webdav_collection_url_for(cfg, ops_path);
    let body = r#"<?xml version="1.0" encoding="utf-8" ?>
<d:propfind xmlns:d="DAV:">
  <d:prop>
    <d:getlastmodified />
  </d:prop>
</d:propfind>"#;

    let resp = webdav_send_with_retry(|| {
        webdav_with_auth(
            client
                .request(method.clone(), &url)
                .header("Depth", "1")
                .header("Content-Type", "application/xml; charset=utf-8")
                .body(body.to_string()),
            cfg,
        )
    })
    .await?;

    let status = resp.status();
    if !status.is_success() && status.as_u16() != 207 {
        let text = resp.text().await.unwrap_or_default();
        return Err(AppError::Network(format!(
            "webdav PROPFIND ops failed: {} {}",
            status, text
        )));
    }

    let text = resp
        .text()
        .await
        .map_err(|e| AppError::Network(e.to_string()))?;

    Ok(parse_webdav_op_refs(&text))
}

async fn fetch_webdav_ops_batch(
    client: &Client,
    cfg: &CloudSyncConfig,
    ops_path: &str,
    op_ref: &WebDavOpRef,
) -> AppResult<Option<WebDavOpsBatch>> {
    let relative = format!(
        "{}/{}",
        ops_path.trim_end_matches('/'),
        webdav_ops_filename(&op_ref.device_id, op_ref.seq)
    );
    let url = webdav_url_for(cfg, &relative);

    fetch_webdav_json_resource(
        || webdav_with_auth(client.get(&url), cfg),
        404,
        "webdav GET ops batch failed",
        "parse ops batch json failed",
    )
    .await
}

fn collect_syncable_settings(app: &AppHandle) -> AppResult<HashMap<String, String>> {
    let db_state = app
        .try_state::<DbState>()
        .ok_or_else(|| AppError::Internal("DB state unavailable".to_string()))?;
    let mut map = db_state.settings_repo.get_all().map_err(AppError::from)?;
    map.retain(|k, _| is_setting_sync_eligible(k));

    if let Some(raw) = map.get(EMOJI_FAVORITES_SETTING_KEY).cloned() {
        if let Some(encoded) = encode_emoji_favorites_setting(&raw) {
            map.insert(EMOJI_FAVORITES_SETTING_KEY.to_string(), encoded);
        }
    }

    Ok(map)
}

fn apply_synced_settings(app: &AppHandle, incoming: &HashMap<String, String>) -> AppResult<usize> {
    if incoming.is_empty() {
        return Ok(0);
    }
    let db_state = app
        .try_state::<DbState>()
        .ok_or_else(|| AppError::Internal("DB state unavailable".to_string()))?;
    let current = db_state.settings_repo.get_all().map_err(AppError::from)?;
    let mut changed = 0usize;
    for (key, value) in incoming {
        if !is_setting_sync_eligible(key) {
            continue;
        }

        let normalized_value = if key == EMOJI_FAVORITES_SETTING_KEY {
            decode_emoji_favorites_setting(app, value)?
        } else {
            value.clone()
        };

        if current
            .get(key)
            .map(|v| v == &normalized_value)
            .unwrap_or(false)
        {
            continue;
        }
        db_state
            .settings_repo
            .set(key, &normalized_value)
            .map_err(AppError::from)?;
        changed += 1;
    }
    Ok(changed)
}

async fn upload_webdav_settings_snapshot(
    app: &AppHandle,
    client: &Client,
    cfg: &CloudSyncConfig,
    settings_path: &str,
) -> AppResult<HashMap<String, String>> {
    let local_settings = collect_syncable_settings(app)?;

    let snapshot = WebDavSettingsSnapshot {
        device_id: cfg.device_id.clone(),
        updated_at: now_ms(),
        settings: local_settings.clone(),
    };
    let body = serde_json::to_vec(&snapshot)
        .map_err(|e| AppError::Internal(format!("serialize settings snapshot failed: {}", e)))?;
    let relative = format!(
        "{}/{}.json",
        settings_path.trim_end_matches('/'),
        cfg.device_id
    );
    upload_webdav_json_resource(client, cfg, &relative, body, "settings snapshot").await?;
    Ok(local_settings)
}

async fn fetch_webdav_settings_snapshot(
    client: &Client,
    cfg: &CloudSyncConfig,
    settings_path: &str,
    device_id: &str,
) -> AppResult<Option<WebDavSettingsSnapshot>> {
    let relative = format!("{}/{}.json", settings_path.trim_end_matches('/'), device_id);
    let url = webdav_url_for(cfg, &relative);
    fetch_webdav_json_resource(
        || webdav_with_auth(client.get(&url), cfg),
        404,
        "webdav GET settings snapshot failed",
        "parse settings snapshot json failed",
    )
    .await
}

async fn pull_remote_settings_snapshot(
    app: &AppHandle,
    client: &Client,
    cfg: &CloudSyncConfig,
    settings_path: &str,
) -> AppResult<usize> {
    let db_state = app
        .try_state::<DbState>()
        .ok_or_else(|| AppError::Internal("DB state unavailable".to_string()))?;
    let last_applied_ts = db_state
        .settings_repo
        .get("cloud_sync_settings_applied_at")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);

    let ids = list_webdav_snapshot_ids(client, cfg, settings_path).await?;
    let mut latest: Option<WebDavSettingsSnapshot> = None;
    for device_id in ids.into_iter().take(MAX_REMOTE_SNAPSHOTS) {
        if crate::app::system::same_anon_id(&device_id, &cfg.device_id) {
            continue;
        }
        if let Some(snapshot) =
            fetch_webdav_settings_snapshot(client, cfg, settings_path, &device_id).await?
        {
            let replace = latest
                .as_ref()
                .map(|cur| snapshot.updated_at > cur.updated_at)
                .unwrap_or(true);
            if replace {
                latest = Some(snapshot);
            }
        }
    }

    let Some(snapshot) = latest else {
        return Ok(0);
    };
    if snapshot.updated_at <= last_applied_ts {
        return Ok(0);
    }

    let changed = apply_synced_settings(app, &snapshot.settings)?;
    db_state
        .settings_repo
        .set(
            "cloud_sync_settings_applied_at",
            &snapshot.updated_at.to_string(),
        )
        .map_err(AppError::from)?;
    Ok(changed)
}

fn should_rebuild_webdav_head(app: &AppHandle, now: i64) -> bool {
    let last = get_setting_i64(app, CLOUD_SYNC_WEBDAV_LAST_HEAD_REBUILD_AT_KEY, 0);
    should_run_periodic_snapshot(last, now, WEBDAV_HEAD_REBUILD_INTERVAL_SECS)
}

fn touch_webdav_head_rebuild_at(app: &AppHandle, now: i64) {
    set_setting_i64(app, CLOUD_SYNC_WEBDAV_LAST_HEAD_REBUILD_AT_KEY, now);
}

fn update_webdav_head_device<F>(head: &mut WebDavSyncHead, device_id: &str, mut update: F)
where
    F: FnMut(&mut WebDavDeviceHead),
{
    let entry = head.devices.entry(device_id.to_string()).or_default();
    update(entry);
}

async fn fetch_webdav_sync_head(
    client: &Client,
    cfg: &CloudSyncConfig,
    head_path: &str,
) -> AppResult<Option<WebDavSyncHead>> {
    let url = webdav_url_for(cfg, head_path);
    fetch_webdav_json_resource(
        || webdav_with_auth(client.get(&url), cfg),
        404,
        "webdav GET head failed",
        "parse head json failed",
    )
    .await
}

async fn upload_webdav_sync_head(
    client: &Client,
    cfg: &CloudSyncConfig,
    head_path: &str,
    head: &WebDavSyncHead,
) -> AppResult<()> {
    let body = serde_json::to_vec(head)
        .map_err(|e| AppError::Internal(format!("serialize head failed: {}", e)))?;
    upload_webdav_json_resource(client, cfg, head_path, body, "sync head").await
}

async fn rebuild_webdav_sync_head(
    client: &Client,
    cfg: &CloudSyncConfig,
    paths: &WebDavPaths,
) -> AppResult<WebDavSyncHead> {
    let mut head = WebDavSyncHead {
        updated_at: now_ms(),
        devices: BTreeMap::new(),
    };

    for op_ref in list_webdav_op_refs(client, cfg, &paths.ops_path).await? {
        update_webdav_head_device(&mut head, &op_ref.device_id, |device| {
            device.latest_op_seq = device.latest_op_seq.max(op_ref.seq);
        });
    }

    for device_id in list_webdav_snapshot_ids(client, cfg, &paths.devices_path).await? {
        let snapshot = fetch_webdav_snapshot(client, cfg, &paths.devices_path, &device_id).await?;
        let updated_at = snapshot
            .as_ref()
            .map(|snapshot| snapshot.updated_at)
            .unwrap_or(0);
        let snapshot_op_seq = snapshot
            .as_ref()
            .map(|snapshot| snapshot.latest_op_seq)
            .unwrap_or(0);
        update_webdav_head_device(&mut head, &device_id, |device| {
            device.latest_op_seq = device.latest_op_seq.max(snapshot_op_seq);
            device.snapshot_updated_at = device.snapshot_updated_at.max(updated_at);
            device.snapshot_op_seq = device.snapshot_op_seq.max(snapshot_op_seq);
        });
    }

    for device_id in list_webdav_snapshot_ids(client, cfg, &paths.settings_path).await? {
        let updated_at =
            fetch_webdav_settings_snapshot(client, cfg, &paths.settings_path, &device_id)
                .await?
                .map(|snapshot| snapshot.updated_at)
                .unwrap_or(0);
        update_webdav_head_device(&mut head, &device_id, |device| {
            device.settings_updated_at = device.settings_updated_at.max(updated_at);
        });
    }

    Ok(head)
}

async fn resolve_webdav_sync_head(
    app: &AppHandle,
    client: &Client,
    cfg: &CloudSyncConfig,
    paths: &WebDavPaths,
    now: i64,
) -> AppResult<WebDavSyncHead> {
    let fetched = fetch_webdav_sync_head(client, cfg, &paths.head_path).await?;
    let needs_rebuild = fetched.is_none() || should_rebuild_webdav_head(app, now);

    if !needs_rebuild {
        return Ok(fetched.unwrap_or_default());
    }

    match rebuild_webdav_sync_head(client, cfg, paths).await {
        Ok(mut rebuilt) => {
            rebuilt.updated_at = now_ms();
            if fetched.as_ref() != Some(&rebuilt) {
                upload_webdav_sync_head(client, cfg, &paths.head_path, &rebuilt).await?;
            }
            touch_webdav_head_rebuild_at(app, now);
            Ok(rebuilt)
        }
        Err(err) => {
            if let Some(existing) = fetched {
                Ok(existing)
            } else {
                Err(err)
            }
        }
    }
}

async fn pull_remote_webdav_ops_from_head(
    app: &AppHandle,
    client: &Client,
    cfg: &CloudSyncConfig,
    blobs_path: &str,
    ops_path: &str,
    head: &WebDavSyncHead,
) -> AppResult<(usize, bool)> {
    let mut cursor_map = load_webdav_op_cursor_map(app);
    let mut received = 0usize;
    let mut head_stale = false;

    for (device_id, device_head) in &head.devices {
        if crate::app::system::same_anon_id(device_id, &cfg.device_id) {
            continue;
        }
        if device_head.latest_op_seq <= 0 {
            continue;
        }

        let mut last_seq = cursor_map.get(device_id).copied().unwrap_or(0);
        if device_head.latest_op_seq <= last_seq {
            continue;
        }

        for seq in (last_seq + 1)..=device_head.latest_op_seq {
            if cloud_sync_cancel_requested() {
                return Ok((received, head_stale));
            }

            let op_ref = WebDavOpRef {
                device_id: device_id.clone(),
                seq,
            };
            match fetch_webdav_ops_batch(client, cfg, ops_path, &op_ref).await? {
                Some(mut batch) if batch.device_id == op_ref.device_id => {
                    enrich_item_blobs_after_pull(app, client, cfg, blobs_path, &mut batch.entries)
                        .await?;
                    received += apply_remote_changes(app, &batch.entries, &cfg.content_prefs)?;
                    last_seq = last_seq.max(batch.seq).max(seq);
                    cursor_map.insert(device_id.clone(), last_seq);
                }
                Some(_) | None => {
                    head_stale = true;
                    break;
                }
            }
        }
    }

    save_webdav_op_cursor_map(app, &cursor_map);
    Ok((received, head_stale))
}

async fn pull_remote_webdav_snapshots_from_head(
    app: &AppHandle,
    client: &Client,
    cfg: &CloudSyncConfig,
    blobs_path: &str,
    devices_path: &str,
    head: &WebDavSyncHead,
) -> AppResult<usize> {
    let mut remote_items: Vec<CloudSyncItem> = Vec::new();
    let mut device_ids: Vec<(String, i64)> = head
        .devices
        .iter()
        .filter_map(|(device_id, device_head)| {
            if crate::app::system::same_anon_id(device_id, &cfg.device_id)
                || device_head.snapshot_updated_at <= 0
            {
                None
            } else {
                Some((device_id.clone(), device_head.snapshot_updated_at))
            }
        })
        .collect();

    device_ids.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    for (device_id, _) in device_ids.into_iter().take(MAX_REMOTE_SNAPSHOTS) {
        if cloud_sync_cancel_requested() {
            break;
        }
        if let Some(mut snapshot) =
            fetch_webdav_snapshot(client, cfg, devices_path, &device_id).await?
        {
            enrich_item_blobs_after_pull(app, client, cfg, blobs_path, &mut snapshot.entries)
                .await?;
            remote_items.extend(snapshot.entries);
        }
    }

    remote_items.sort_by_key(|item| item.timestamp);
    apply_remote_changes(app, &remote_items, &cfg.content_prefs)
}

async fn pull_remote_settings_snapshot_from_head(
    app: &AppHandle,
    client: &Client,
    cfg: &CloudSyncConfig,
    settings_path: &str,
    head: &WebDavSyncHead,
) -> AppResult<usize> {
    let db_state = app
        .try_state::<DbState>()
        .ok_or_else(|| AppError::Internal("DB state unavailable".to_string()))?;
    let last_applied_ts = db_state
        .settings_repo
        .get("cloud_sync_settings_applied_at")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);

    let Some((device_id, latest_ts)) = head
        .devices
        .iter()
        .filter(|(device_id, device_head)| {
            !crate::app::system::same_anon_id(device_id, &cfg.device_id)
                && device_head.settings_updated_at > 0
        })
        .map(|(device_id, device_head)| (device_id.clone(), device_head.settings_updated_at))
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)))
    else {
        return Ok(0);
    };

    if latest_ts <= last_applied_ts {
        return Ok(0);
    }

    let Some(snapshot) =
        fetch_webdav_settings_snapshot(client, cfg, settings_path, &device_id).await?
    else {
        return Ok(0);
    };
    if snapshot.updated_at <= last_applied_ts {
        return Ok(0);
    }

    let changed = apply_synced_settings(app, &snapshot.settings)?;
    db_state
        .settings_repo
        .set(
            "cloud_sync_settings_applied_at",
            &snapshot.updated_at.to_string(),
        )
        .map_err(AppError::from)?;
    Ok(changed)
}

async fn cleanup_local_webdav_ops(
    client: &Client,
    cfg: &CloudSyncConfig,
    ops_path: &str,
    max_seq_to_delete: i64,
) -> AppResult<usize> {
    if max_seq_to_delete <= 0 {
        return Ok(0);
    }

    let refs = list_webdav_op_refs(client, cfg, ops_path).await?;
    let mut deleted = 0usize;
    for op_ref in refs {
        if !crate::app::system::same_anon_id(&op_ref.device_id, &cfg.device_id)
            || op_ref.seq > max_seq_to_delete
        {
            continue;
        }

        let relative = format!(
            "{}/{}",
            ops_path.trim_end_matches('/'),
            webdav_ops_filename(&op_ref.device_id, op_ref.seq)
        );
        delete_webdav_resource_if_exists(client, cfg, &relative).await?;
        deleted += 1;
    }

    Ok(deleted)
}

fn should_run_periodic_snapshot(last_ts: i64, now: i64, interval_secs: i64) -> bool {
    if last_ts <= 0 {
        return true;
    }
    now.saturating_sub(last_ts) >= interval_secs.saturating_mul(1000)
}

fn should_push_webdav_snapshot(app: &AppHandle, now: i64, snapshot_interval_secs: i64) -> bool {
    let last = get_setting_i64(app, CLOUD_SYNC_WEBDAV_LAST_SNAPSHOT_PUSH_AT_KEY, 0);
    should_run_periodic_snapshot(last, now, snapshot_interval_secs)
}

fn should_pull_webdav_snapshot(
    app: &AppHandle,
    now: i64,
    has_remote_op_cursor: bool,
    snapshot_interval_secs: i64,
) -> bool {
    let last = get_setting_i64(app, CLOUD_SYNC_WEBDAV_LAST_SNAPSHOT_PULL_AT_KEY, 0);
    if !has_remote_op_cursor {
        // Cold-start fallback for new peers without op cursors yet.
        return should_run_periodic_snapshot(last, now, (5 * 60).min(snapshot_interval_secs));
    }
    should_run_periodic_snapshot(last, now, snapshot_interval_secs)
}

async fn pull_remote_webdav_ops(
    app: &AppHandle,
    client: &Client,
    cfg: &CloudSyncConfig,
    ops_path: &str,
    blobs_path: &str,
) -> AppResult<usize> {
    let refs = list_webdav_op_refs(client, cfg, ops_path).await?;
    if refs.is_empty() {
        return Ok(0);
    }

    let mut cursor_map = load_webdav_op_cursor_map(app);
    let mut received = 0usize;
    let _total_refs = refs.len();
    for (_index, op_ref) in refs.into_iter().enumerate() {
        if cloud_sync_cancel_requested() {
            break;
        }
        if crate::app::system::same_anon_id(&op_ref.device_id, &cfg.device_id) {
            continue;
        }
        let last_seq = cursor_map.get(&op_ref.device_id).copied().unwrap_or(0);
        if op_ref.seq <= last_seq {
            continue;
        }

        if let Some(mut batch) = fetch_webdav_ops_batch(client, cfg, ops_path, &op_ref).await? {
            if batch.device_id != op_ref.device_id {
                continue;
            }
            if cloud_sync_cancel_requested() {
                break;
            }
            enrich_item_blobs_after_pull(app, client, cfg, blobs_path, &mut batch.entries).await?;
            received += apply_remote_changes(app, &batch.entries, &cfg.content_prefs)?;
            let next_seq = batch.seq.max(op_ref.seq).max(last_seq);
            cursor_map.insert(op_ref.device_id.clone(), next_seq);
        }
    }
    save_webdav_op_cursor_map(app, &cursor_map);
    Ok(received)
}

async fn sync_once_http(app: &AppHandle, cfg: &CloudSyncConfig) -> AppResult<CloudSyncStatus> {
    let local_items = collect_local_changes(app, cfg.cursor, &cfg.content_prefs)?;
    let endpoint = format!(
        "{}/api/v1/clipboard/sync",
        cfg.base_url.trim_end_matches('/')
    );
    let request = CloudSyncRequest {
        device_id: cfg.device_id.clone(),
        cursor: cfg.cursor,
        entries: local_items.clone(),
    };

    let client = build_http_client()?;
    let mut http_req = client.post(&endpoint).json(&request);
    if !cfg.api_key.trim().is_empty() {
        http_req = http_req.bearer_auth(cfg.api_key.trim());
    }

    let resp = http_req
        .send()
        .await
        .map_err(|e| AppError::Network(e.to_string()))?;
    if !resp.status().is_success() {
        let status_code = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(AppError::Network(format!(
            "sync endpoint failed: {} {}",
            status_code, text
        )));
    }

    let body = resp
        .json::<CloudSyncResponse>()
        .await
        .map_err(|e| AppError::Network(e.to_string()))?;

    let received = apply_remote_changes(app, &body.entries, &cfg.content_prefs)?;
    if received > 0 {
        let _ = app.emit("clipboard-changed", ());
    }
    let local_max = local_items
        .iter()
        .map(|x| x.timestamp)
        .max()
        .unwrap_or(cfg.cursor);
    let remote_max = body
        .entries
        .iter()
        .map(|x| x.timestamp)
        .max()
        .unwrap_or(cfg.cursor);
    let next_cursor = body
        .cursor
        .unwrap_or(cfg.cursor)
        .max(local_max)
        .max(remote_max);

    if let Some(db_state) = app.try_state::<DbState>() {
        let _ = db_state
            .settings_repo
            .set("cloud_sync_cursor", &next_cursor.to_string());
    }

    let now = now_ms();
    CLOUD_SYNC_LAST_SYNC_AT.store(now, Ordering::Relaxed);
    Ok(CloudSyncStatus {
        state: "idle".to_string(),
        running: true,
        last_sync_at: Some(now),
        last_error: None,
        uploaded_items: local_items.len(),
        received_items: received,
    })
}

async fn sync_once_webdav(
    app: &AppHandle,
    cfg: &CloudSyncConfig,
    force_snapshot: bool,
) -> AppResult<CloudSyncStatus> {
    if cloud_sync_cancel_requested() {
        return Ok(disabled_status());
    }
    let now = now_ms();
    let local_items = collect_local_syncable_items(app, &cfg.content_prefs)?;
    let (delta_items, collapsed_index) = collect_local_incremental_items(app, &local_items)?;
    let client = build_http_client()?;
    let paths = ensure_webdav_directories(&client, cfg).await?;
    let mut sync_head = resolve_webdav_sync_head(app, &client, cfg, &paths, now).await?;
    let mut sync_head_dirty = false;
    let mut webdav_blob_cache = load_webdav_blob_cache(app);
    let should_pull_snapshot = force_snapshot
        || should_pull_webdav_snapshot(
            app,
            now,
            !load_webdav_op_cursor_map(app).is_empty(),
            cfg.snapshot_interval_secs,
        );
    let should_push_snapshot =
        force_snapshot || should_push_webdav_snapshot(app, now, cfg.snapshot_interval_secs);

    let mut uploaded_items = 0usize;
    if !delta_items.is_empty() {
        let mut next_seq = get_local_webdav_op_seq(app);
        let mut processed_delta = delta_items.clone();
        process_items_blobs_before_push(
            &client,
            cfg,
            &paths.blobs_path,
            &mut webdav_blob_cache,
            &mut processed_delta,
        )
        .await?;
        for chunk in processed_delta.chunks(WEBDAV_OP_BATCH_SIZE) {
            if cloud_sync_cancel_requested() {
                return Ok(disabled_status());
            }
            next_seq = next_seq.saturating_add(1);
            upload_webdav_ops_batch(&client, cfg, &paths.ops_path, next_seq, chunk).await?;
        }
        set_local_webdav_op_seq(app, next_seq);
        replace_local_sync_index(app, &collapsed_index)?;
        uploaded_items += delta_items.len();
        update_webdav_head_device(&mut sync_head, &cfg.device_id, |device| {
            device.latest_op_seq = device.latest_op_seq.max(next_seq);
        });
        sync_head_dirty = true;
    }

    if cloud_sync_cancel_requested() {
        return Ok(disabled_status());
    }

    let (mut received_items, head_stale) = pull_remote_webdav_ops_from_head(
        app,
        &client,
        cfg,
        &paths.blobs_path,
        &paths.ops_path,
        &sync_head,
    )
    .await?;
    if head_stale {
        let rebuilt = rebuild_webdav_sync_head(&client, cfg, &paths).await?;
        if rebuilt != sync_head {
            sync_head = rebuilt;
            sync_head.updated_at = now_ms();
            upload_webdav_sync_head(&client, cfg, &paths.head_path, &sync_head).await?;
            touch_webdav_head_rebuild_at(app, now);
        }
        received_items +=
            pull_remote_webdav_ops(app, &client, cfg, &paths.ops_path, &paths.blobs_path).await?;
        received_items += pull_remote_webdav_snapshots_from_head(
            app,
            &client,
            cfg,
            &paths.blobs_path,
            &paths.devices_path,
            &sync_head,
        )
        .await?;
    }

    // Incremental Emoji Sync check
    if let Ok(emoji_op) = check_and_create_emoji_sync_op(app) {
        if let Some(op) = emoji_op {
            let next_seq = get_local_webdav_op_seq(app).saturating_add(1);
            upload_webdav_ops_batch(&client, cfg, &paths.ops_path, next_seq, &[op]).await?;
            set_local_webdav_op_seq(app, next_seq);
            uploaded_items += 1;
            update_webdav_head_device(&mut sync_head, &cfg.device_id, |device| {
                device.latest_op_seq = device.latest_op_seq.max(next_seq);
            });
            sync_head_dirty = true;
        }
    }

    save_webdav_blob_cache(app, &webdav_blob_cache);

    if should_pull_snapshot {
        received_items += pull_remote_webdav_snapshots_from_head(
            app,
            &client,
            cfg,
            &paths.blobs_path,
            &paths.devices_path,
            &sync_head,
        )
        .await?;

        received_items += pull_remote_settings_snapshot_from_head(
            app,
            &client,
            cfg,
            &paths.settings_path,
            &sync_head,
        )
        .await?;
        set_setting_i64(app, CLOUD_SYNC_WEBDAV_LAST_SNAPSHOT_PULL_AT_KEY, now);
    }

    if should_push_snapshot {
        if cloud_sync_cancel_requested() {
            return Ok(disabled_status());
        }
        let latest_op_seq = get_local_webdav_op_seq(app);
        upload_webdav_snapshot(
            &client,
            cfg,
            &paths.devices_path,
            latest_op_seq,
            &local_items,
        )
        .await?;
        uploaded_items += local_items.len();
        update_webdav_head_device(&mut sync_head, &cfg.device_id, |device| {
            device.latest_op_seq = device.latest_op_seq.max(latest_op_seq);
            device.snapshot_updated_at = device.snapshot_updated_at.max(now_ms());
            device.snapshot_op_seq = device.snapshot_op_seq.max(latest_op_seq);
        });
        let local_settings =
            upload_webdav_settings_snapshot(app, &client, cfg, &paths.settings_path).await?;
        uploaded_items += local_settings.len();
        update_webdav_head_device(&mut sync_head, &cfg.device_id, |device| {
            device.settings_updated_at = device.settings_updated_at.max(now_ms());
        });
        sync_head_dirty = true;
        set_setting_i64(app, CLOUD_SYNC_WEBDAV_LAST_SNAPSHOT_PUSH_AT_KEY, now);
        let _ = cleanup_local_webdav_ops(&client, cfg, &paths.ops_path, latest_op_seq).await;
    }

    if sync_head_dirty {
        sync_head.updated_at = now_ms();
        upload_webdav_sync_head(&client, cfg, &paths.head_path, &sync_head).await?;
    }

    if received_items > 0 {
        let _ = app.emit("clipboard-changed", ());
    }
    CLOUD_SYNC_LAST_SYNC_AT.store(now, Ordering::Relaxed);

    if let Some(db_state) = app.try_state::<DbState>() {
        let _ = db_state
            .settings_repo
            .set("cloud_sync_cursor", &now.to_string());
    }

    Ok(CloudSyncStatus {
        state: "idle".to_string(),
        running: true,
        last_sync_at: Some(now),
        last_error: None,
        uploaded_items,
        received_items,
    })
}

async fn sync_once(
    app: &AppHandle,
    cfg: &CloudSyncConfig,
    force_snapshot: bool,
) -> AppResult<CloudSyncStatus> {
    let _run_guard = sync_run_lock().lock().await;
    if cloud_sync_cancel_requested() {
        let status = disabled_status();
        emit_status(Some(app), status.clone());
        return Ok(status);
    }

    if !cfg.enabled {
        let status = CloudSyncStatus {
            state: "disabled".to_string(),
            running: false,
            last_sync_at: None,
            last_error: None,
            uploaded_items: 0,
            received_items: 0,
        };
        emit_status(Some(app), status.clone());
        return Ok(status);
    }

    if !cloud_sync_target_ready(cfg) {
        let msg = cloud_sync_target_not_ready_message(cfg);
        let status = CloudSyncStatus {
            state: "error".to_string(),
            running: true,
            last_sync_at: None,
            last_error: Some(msg.clone()),
            uploaded_items: 0,
            received_items: 0,
        };
        emit_status(Some(app), status);
        return Err(AppError::Validation(msg));
    }

    emit_status(
        Some(app),
        CloudSyncStatus {
            state: "syncing".to_string(),
            running: true,
            last_sync_at: None,
            last_error: None,
            uploaded_items: 0,
            received_items: 0,
        },
    );

    let result = match cfg.provider {
        CloudSyncProvider::Http => sync_once_http(app, cfg).await,
        CloudSyncProvider::WebDav => sync_once_webdav(app, cfg, force_snapshot).await,
    };

    match result {
        Ok(status) => {
            emit_status(Some(app), status.clone());
            Ok(status)
        }
        Err(err) => {
            if cloud_sync_cancel_requested() {
                let status = disabled_status();
                emit_status(Some(app), status.clone());
                return Ok(status);
            }
            emit_status(
                Some(app),
                CloudSyncStatus {
                    state: "error".to_string(),
                    running: true,
                    last_sync_at: None,
                    last_error: Some(format!("[{}] {}", cfg.provider.as_str(), err)),
                    uploaded_items: 0,
                    received_items: 0,
                },
            );
            Err(err)
        }
    }
}

struct CloudSyncTaskGuard;

impl Drop for CloudSyncTaskGuard {
    fn drop(&mut self) {
        CLOUD_SYNC_TASK_ACTIVE.store(false, Ordering::Relaxed);
    }
}

pub fn get_cloud_sync_status() -> CloudSyncStatus {
    if let Ok(guard) = status_store().lock() {
        guard.clone()
    } else {
        CloudSyncStatus {
            state: "error".to_string(),
            running: false,
            last_sync_at: None,
            last_error: Some("status lock poisoned".to_string()),
            uploaded_items: 0,
            received_items: 0,
        }
    }
}

pub fn start_cloud_sync_client(app: AppHandle) {
    if CLOUD_SYNC_TASK_ACTIVE.swap(true, Ordering::Relaxed) {
        return;
    }

    tauri::async_runtime::spawn(async move {
        let _guard = CloudSyncTaskGuard;

        loop {
            let mut requested = CLOUD_SYNC_REQUESTED.swap(false, Ordering::Relaxed);
            let cfg = match get_config(&app) {
                Some(c) => c,
                None => {
                    emit_status(
                        Some(&app),
                        CloudSyncStatus {
                            state: "disabled".to_string(),
                            running: false,
                            last_sync_at: None,
                            last_error: None,
                            uploaded_items: 0,
                            received_items: 0,
                        },
                    );
                    sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };

            let now = now_ms();
            let backoff_until = CLOUD_SYNC_BACKOFF_UNTIL.load(Ordering::Relaxed);
            if backoff_until > now {
                let remaining_secs = (backoff_until - now) / 1000;
                if remaining_secs > 0 {
                    emit_status(
                        Some(&app),
                        CloudSyncStatus {
                            state: "idle".to_string(),
                            running: true,
                            last_sync_at: None,
                            last_error: Some(format!(
                                "WebDAV Cooldown (JianGuoYun Rate Limit): {}s remaining",
                                remaining_secs
                            )),
                            uploaded_items: 0,
                            received_items: 0,
                        },
                    );
                    sleep(Duration::from_secs(5)).await;
                    continue;
                }
            }

            if !cfg.enabled || !cloud_sync_target_ready(&cfg) {
                if !cfg.enabled {
                    emit_status(
                        Some(&app),
                        CloudSyncStatus {
                            state: "disabled".to_string(),
                            running: false,
                            last_sync_at: None,
                            last_error: None,
                            uploaded_items: 0,
                            received_items: 0,
                        },
                    );
                } else {
                    emit_status(
                        Some(&app),
                        CloudSyncStatus {
                            state: "error".to_string(),
                            running: false,
                            last_sync_at: None,
                            last_error: Some(cloud_sync_target_not_ready_message(&cfg)),
                            uploaded_items: 0,
                            received_items: 0,
                        },
                    );
                }
            } else if cfg.auto_sync || requested {
                if let Err(e) = sync_once(&app, &cfg, false).await {
                    emit_status(
                        Some(&app),
                        CloudSyncStatus {
                            state: "error".to_string(),
                            running: true,
                            last_sync_at: None,
                            last_error: Some(e.to_string()),
                            uploaded_items: 0,
                            received_items: 0,
                        },
                    );
                }
            } else {
                emit_status(
                    Some(&app),
                    CloudSyncStatus {
                        state: "idle".to_string(),
                        running: true,
                        last_sync_at: None,
                        last_error: None,
                        uploaded_items: 0,
                        received_items: 0,
                    },
                );
            }

            if cfg.auto_sync {
                let interval = cfg
                    .interval_secs
                    .clamp(MIN_INTERVAL_SECS, MAX_INTERVAL_SECS);
                let mut elapsed = 0u64;
                while elapsed < interval {
                    requested = CLOUD_SYNC_REQUESTED.swap(false, Ordering::Relaxed);
                    if requested {
                        break;
                    }
                    sleep(Duration::from_secs(1)).await;
                    elapsed += 1;
                }
            } else {
                loop {
                    requested = CLOUD_SYNC_REQUESTED.swap(false, Ordering::Relaxed);
                    if requested {
                        break;
                    }
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }
    });
}

pub fn restart_cloud_sync_client(app: AppHandle) {
    CLOUD_SYNC_CANCEL_REQUESTED.store(false, Ordering::Relaxed);
    start_cloud_sync_client(app);
    CLOUD_SYNC_REQUESTED.store(true, Ordering::Relaxed);
}

pub fn request_cloud_sync(app: AppHandle) {
    let Some(cfg) = get_config(&app) else {
        return;
    };
    if !cfg.enabled || !cfg.auto_sync || !cloud_sync_target_ready(&cfg) {
        return;
    }
    CLOUD_SYNC_CANCEL_REQUESTED.store(false, Ordering::Relaxed);
    start_cloud_sync_client(app);
    CLOUD_SYNC_REQUESTED.store(true, Ordering::Relaxed);
}

pub fn stop_cloud_sync_client(app: AppHandle) {
    CLOUD_SYNC_CANCEL_REQUESTED.store(true, Ordering::Relaxed);
    emit_status(Some(&app), disabled_status());
}

pub async fn cloud_sync_now(app: AppHandle) -> AppResult<CloudSyncStatus> {
    let current = get_cloud_sync_status();
    if current.state == "syncing" {
        return Ok(current);
    }
    CLOUD_SYNC_CANCEL_REQUESTED.store(false, Ordering::Relaxed);
    let cfg =
        get_config(&app).ok_or_else(|| AppError::Internal("DB state unavailable".to_string()))?;
    sync_once(&app, &cfg, true).await
}

fn check_and_create_emoji_sync_op(app: &AppHandle) -> AppResult<Option<CloudSyncItem>> {
    let db_state = app
        .try_state::<DbState>()
        .ok_or_else(|| AppError::Internal("DB unavailable".to_string()))?;

    let emoji_prefs = db_state
        .settings_repo
        .get("cloud_sync_content_prefs")
        .ok()
        .flatten()
        .map(|raw| serde_json::from_str::<CloudSyncContentPrefs>(&raw).unwrap_or_default())
        .unwrap_or_default();
    if !emoji_prefs.emoji {
        return Ok(None);
    }

    let emoji_json = db_state
        .settings_repo
        .get(EMOJI_FAVORITES_SETTING_KEY)
        .ok()
        .flatten()
        .unwrap_or_default();

    if emoji_json.trim().is_empty() || emoji_json == "[]" {
        return Ok(None);
    }

    let Some(sync_payload) = encode_emoji_favorites_setting(&emoji_json) else {
        return Ok(None);
    };
    if sync_payload.trim().is_empty() || sync_payload == "[]" {
        return Ok(None);
    }

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    sync_payload.hash(&mut hasher);
    let current_hash = hasher.finish() as i64;

    if current_hash == LAST_PUSHED_EMOJI_HASH.load(Ordering::Relaxed) {
        return Ok(None);
    }

    LAST_PUSHED_EMOJI_HASH.store(current_hash, Ordering::Relaxed);

    Ok(Some(CloudSyncItem {
        content_type: "emoji_sync".to_string(),
        content: sync_payload,
        content_hash: current_hash,
        deleted_at: 0,
        html_content: None,
        content_blob_hash: None,
        html_blob_hash: None,
        source_app: "Tiez-Next".to_string(),
        timestamp: now_ms(),
        preview: "⭐ Emoji Sync".to_string(),
        is_pinned: false,
        pinned_order: 0,
        note: String::new(),
        tags: vec![],
        use_count: 0,
    }))
}

fn merge_remote_emojis(app: &AppHandle, remote_json: &str) -> AppResult<()> {
    let db_state = app
        .try_state::<DbState>()
        .ok_or_else(|| AppError::Internal("DB unavailable".to_string()))?;
    let local_json = db_state
        .settings_repo
        .get(EMOJI_FAVORITES_SETTING_KEY)
        .ok()
        .flatten()
        .unwrap_or_default();

    let local_paths = if local_json.trim().is_empty() || local_json == "[]" {
        Vec::new()
    } else {
        materialize_emoji_favorite_paths(app, &local_json)?
    };
    let remote_paths = materialize_emoji_favorite_paths(app, remote_json)?;
    let normalized_local_json = serde_json::to_string(&local_paths).unwrap_or_default();

    let mut merged: std::collections::HashSet<String> = local_paths.iter().cloned().collect();
    for path in remote_paths {
        merged.insert(path);
    }

    let mut merged_paths: Vec<String> = merged.into_iter().collect();
    merged_paths.sort();

    if merged_paths != local_paths || normalized_local_json != local_json {
        let new_json = serde_json::to_string(&merged_paths).unwrap_or_default();
        db_state
            .settings_repo
            .set(EMOJI_FAVORITES_SETTING_KEY, &new_json)
            .map_err(AppError::from)?;

        // Update local hash to prevent echoing back the same data.
        let sync_payload =
            encode_emoji_favorites_setting(&new_json).unwrap_or_else(|| "[]".to_string());
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        use std::hash::{Hash, Hasher};
        sync_payload.hash(&mut hasher);
        LAST_PUSHED_EMOJI_HASH.store(hasher.finish() as i64, Ordering::Relaxed);

        let _ = app.emit("settings-changed", ());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    /// MCP 的安全姿态设置**不得**参与云同步。
    ///
    /// 云同步会把远端写回的键落进本地 settings 并持久化。若 `mcp.*` 能同步，
    /// 一个被篡改的远端快照就能把"免鉴权 + 允许写入 + 开放局域网"种进这台机器，
    /// 等 MCP 下次重启时生效——用户从未在本机做过这个选择。
    #[test]
    fn mcp_settings_never_sync_from_the_cloud() {
        for key in [
            "mcp.enabled",
            "mcp.allow_write",
            "mcp.allow_lan",
            "mcp.require_token",
            "mcp.token",
            "mcp.port",
            "mcp.autostart",
        ] {
            assert!(
                !is_setting_sync_eligible(key),
                "{key} 参与云同步会让远端快照改写本机的 MCP 安全姿态"
            );
        }
    }

    /// 前缀排除要能覆盖**将来新增**的 mcp 键，否则这条保护会随版本悄悄失效。
    #[test]
    fn unknown_mcp_keys_are_also_excluded() {
        assert!(!is_setting_sync_eligible("mcp.some_future_switch"));
    }

    /// 普通界面设置仍应正常同步——不能因为加排除把正常功能也关掉。
    #[test]
    fn ordinary_settings_still_sync() {
        assert!(is_setting_sync_eligible("app.theme"));
        assert!(is_setting_sync_eligible("app.language"));
    }

    /// 自动容灾备份的四个设置键**一律不参与云同步**。
    ///
    /// 最具破坏性的一条是 `max_keep`：云同步会把远端写回的设置落进本地并持久化，于是远端
    /// 把它改成 1 之后，本机下一次轮换就会把几乎所有容灾副本删掉——而用户从未在**这台**
    /// 机器上做过这个选择。`interval_minutes` 同理会把 30 分钟变成 1440。
    #[test]
    fn auto_backup_settings_never_sync_from_the_cloud() {
        for key in [
            "auto_backup.enabled",
            "auto_backup.interval_minutes",
            "auto_backup.max_keep",
            "auto_backup.backup_on_startup",
        ] {
            assert!(
                !is_setting_sync_eligible(key),
                "{key} 参与云同步会让远端快照改写本机的容灾留存策略"
            );
        }
    }

    /// 前缀排除要覆盖**将来新增**的自动备份键（枚举式排除会随版本悄悄失效）。
    #[test]
    fn unknown_auto_backup_keys_are_also_excluded() {
        assert!(!is_setting_sync_eligible("auto_backup.some_future_switch"));
    }

    /// **凭据类键一律不参与云同步**——逐项钉住 `database::SENSITIVE_KEYS`。
    ///
    /// 【为什么这条必须存在，且必须逐项断言】
    ///
    /// 本轮实测发现的缺口：`SENSITIVE_KEYS` 有 5 项，而本函数的排除表只列了其中 2 项
    /// （`cloud_sync_api_key` / `cloud_sync_webdav_password`）。剩下三项——
    /// `mqtt_password`、`mqtt_username`、`ai_profiles`——既会被
    /// `collect_syncable_settings` 放进上传快照，也会被 `apply_synced_settings` 放行写回
    /// 本机。也就是说：**MQTT 密码会离开这台机器，并且能被远端覆盖**。
    ///
    /// 逐项列举 5 个键而不是只测一个前缀：这道保护的正确性来源是"它覆盖了
    /// `SENSITIVE_KEYS` 的**全部**成员"。若将来往 `SENSITIVE_KEYS` 加第 6 项，
    /// 由于 `is_setting_sync_eligible` 直接调用 `is_sensitive_key`，本测试会**自动**
    /// 覆盖到新键——这正是"两个定义合一"带来的收益。
    #[test]
    fn credential_keys_never_sync_to_the_cloud() {
        for key in crate::database::SENSITIVE_KEYS {
            assert!(
                !is_setting_sync_eligible(key),
                "{key} 是凭据（database::SENSITIVE_KEYS 成员），参与云同步会让它离开本机                 并且能被远端快照覆盖"
            );
        }
    }

    /// 大小写变体同样要被挡住（`is_sensitive_key` 用 `eq_ignore_ascii_case`）。
    #[test]
    fn credential_keys_are_excluded_case_insensitively() {
        assert!(!is_setting_sync_eligible("MQTT_PASSWORD"));
        assert!(!is_setting_sync_eligible("Mqtt_Password"));
    }

    use super::{
        is_setting_sync_eligible, normalize_item_for_sync, rewrite_rich_html_resources_for_sync,
        CloudSyncItem, RICH_IMAGE_FALLBACK_PREFIX, RICH_IMAGE_FALLBACK_SUFFIX,
    };
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    // =====================================================================
    // 存量凭据外流的"升级告知"判据
    //
    // 这一组测试要钉住的方向有**两个**，缺一不可：
    //
    // * **该提示的必须提示**：配置过云同步的用户，哪怕本机已经看不到任何"确实推过"的
    //   记录（关掉了、清空过、从备份恢复过），也必须被告知。少提示一个人的代价是
    //   "他的 MQTT 密码还被别人知道着，而他一无所知"。
    // * **不该提示的绝不能提示**：从未配置过云同步的用户，任何情况下都不得看到这条
    //   警告。多提示所有人的代价是这条警告变成噪音，连真正受影响的人也不会再读。
    //
    // 因此每个 case 都同时断言 `should_notify` 与 `evidence`：只看 bool 的话，
    // "因为没配过所以不提示"和"因为提示过了所以不提示"会混成一个结果，
    // 而这正是最容易写错的地方。
    // =====================================================================

    use super::{
        credential_exposure_notice, credential_exposure_notice_from_map,
        CREDENTIAL_EXPOSURE_ACK_KEY, CREDENTIAL_EXPOSURE_EVIDENCE_KEYS,
        CREDENTIAL_EXPOSURE_SUBJECT_KEYS,
    };

    /// 造一份"从未配置过云同步"的设置表（只含出厂默认值）。
    fn settings_never_configured() -> std::collections::HashMap<String, String> {
        let mut map = std::collections::HashMap::new();
        map.insert("cloud_sync_enabled".to_string(), "false".to_string());
        map.insert("cloud_sync_server".to_string(), String::new());
        map.insert("cloud_sync_webdav_url".to_string(), String::new());
        for key in CREDENTIAL_EXPOSURE_EVIDENCE_KEYS {
            // 出厂默认：痕迹键全是 0。
            map.insert((*key).to_string(), "0".to_string());
        }
        map.insert("mqtt_password".to_string(), String::new());
        map.insert("mqtt_username".to_string(), String::new());
        map.insert("ai_profiles".to_string(), String::new());
        map
    }

    /// **肯定没受影响 ⇒ 不得提示。**
    ///
    /// 反方向：把 `credential_exposure_notice` 改成恒真（或在 `NotConfigured` 分支
    /// 返回 `ConfiguredOnly`），本测试第一行就变红——实测见任务报告。
    #[test]
    fn never_configured_cloud_sync_never_prompts() {
        let map = settings_never_configured();
        let notice = credential_exposure_notice_from_map(&map);
        assert!(
            !notice.should_notify,
            "从未配置过云同步的机器不得看到这条安全警告（否则它会变成对所有用户的噪音）"
        );
        assert_eq!(notice.evidence, super::ExposureEvidence::NotConfigured);
        // 出厂默认下三个键都是空的，文案材料也应当为空。
        assert!(notice.stored_credential_keys.is_empty());
    }

    /// **确实可能受影响 ⇒ 必须提示**（本机留有"确实推送过设置快照"的记录）。
    #[test]
    fn confirmed_snapshot_push_always_prompts() {
        let mut map = settings_never_configured();
        map.insert(
            "cloud_sync_webdav_last_snapshot_push_at".to_string(),
            "1760000000000".to_string(),
        );
        let notice = credential_exposure_notice_from_map(&map);
        assert!(
            notice.should_notify,
            "推送过设置快照的机器必须被告知：那 3 项凭据已经在它自己的云端存储里了"
        );
        assert_eq!(
            notice.evidence,
            super::ExposureEvidence::ConfirmedSyncHistory
        );
    }

    /// **配过但本机已无痕迹 ⇒ 仍然提示。**
    ///
    /// 这一格是"判据太窄就会漏判"的具体形状：用户配好云同步、同步成功、随后**关掉并
    /// 清空了痕迹**（或从不含痕迹键的旧备份恢复）。只看痕迹键的判据会在这里放行，
    /// 而那正是最需要被告知的一批人。
    #[test]
    fn configured_but_no_trace_still_prompts() {
        for (enabled, server) in [(true, ""), (false, "https://dav.example.com/dav")] {
            let mut map = settings_never_configured();
            map.insert("cloud_sync_enabled".to_string(), enabled.to_string());
            map.insert("cloud_sync_server".to_string(), server.to_string());
            let notice = credential_exposure_notice_from_map(&map);
            assert!(
                notice.should_notify,
                "配置过云同步（enabled={enabled}, server={server:?}）就必须提示，\
                 因为本机看不到的那段历史里可能已经推过一次快照"
            );
            assert_eq!(notice.evidence, super::ExposureEvidence::ConfiguredOnly);
        }
    }

    /// **【关键一格】被外流的键"本机还有没有值"不参与判定。**
    ///
    /// 用户后来把 MQTT 密码删了，不代表它没被上传过；本机现在有值，也不代表当初上传过。
    /// 这条断言把"那不是判据、只是文案材料"从注释变成会红的测试：谁要是把
    /// `stored_credential_keys` 接进 `should_notify`，这里立刻失败。
    #[test]
    fn stored_credential_keys_never_drive_the_decision() {
        for stored in [vec![], vec!["mqtt_password".to_string()], vec![
            "mqtt_password".to_string(),
            "mqtt_username".to_string(),
            "ai_profiles".to_string(),
        ]] {
            // 从未配置过：即使本机三个键都有值，也不得提示。
            assert!(
                !credential_exposure_notice(false, false, false, false, &stored).should_notify,
                "本机存着凭据 ≠ 用云同步传过它（stored={stored:?}）"
            );
            // 配置过：即使三个键都已被删空，也必须提示。
            assert!(
                credential_exposure_notice(false, true, false, false, &stored).should_notify,
                "用户删掉了本机的凭据，不代表它没有被上传过（stored={stored:?}）"
            );
        }
    }

    /// **一次性**：提示过之后不再提示，且判据本身仍然成立。
    ///
    /// 反方向：去掉 `already_acknowledged` 这一项（把 `should_notify` 写成只看
    /// evidence），本测试变红。
    #[test]
    fn acknowledged_notice_never_prompts_again() {
        let mut map = settings_never_configured();
        map.insert(
            "cloud_sync_webdav_last_snapshot_push_at".to_string(),
            "1760000000000".to_string(),
        );

        let first = credential_exposure_notice_from_map(&map);
        assert!(first.should_notify, "前置：首次应当提示");
        assert!(!first.acknowledged);

        // 复现命令层的动作：写入一次性标记。
        map.insert(CREDENTIAL_EXPOSURE_ACK_KEY.to_string(), "true".to_string());

        let second = credential_exposure_notice_from_map(&map);
        assert!(!second.should_notify, "已经提示过一次，不得再提示");
        assert!(second.acknowledged);
        assert_eq!(
            second.evidence,
            super::ExposureEvidence::ConfirmedSyncHistory,
            "标记只影响是否提示，不得改变判定依据（否则无法区分两件事）"
        );
    }

    /// 标记键本身**不得**参与云同步——否则一个被篡改的远端快照就能把用户的告知
    /// 静默吞掉（`apply_synced_settings` 只放行 `is_setting_sync_eligible` 的键）。
    #[test]
    fn acknowledgement_key_cannot_be_synced_from_the_cloud() {
        assert!(!is_setting_sync_eligible(CREDENTIAL_EXPOSURE_ACK_KEY));
    }

    /// 外流对象清单必须**恰好**是 `SENSITIVE_KEYS` 里曾经漏掉的那三个。
    ///
    /// 这条把"要对用户说清是哪三项"与"当时真正的缺口是哪三项"绑在一起：将来若有人
    /// 顺手往清单里加第四项（或漏掉一项），文案就会开始说错话，这里会红。
    #[test]
    fn exposure_subject_keys_are_exactly_the_three_that_leaked() {
        let subjects: std::collections::HashSet<&str> =
            CREDENTIAL_EXPOSURE_SUBJECT_KEYS.iter().copied().collect();
        let expected: std::collections::HashSet<&str> =
            ["mqtt_password", "mqtt_username", "ai_profiles"]
                .into_iter()
                .collect();
        assert_eq!(subjects, expected);

        // 与真实来源核对：这三个键现在都已经是凭据（会被加密），也都不再参与同步。
        for key in CREDENTIAL_EXPOSURE_SUBJECT_KEYS {
            assert!(
                crate::database::is_sensitive_key(key),
                "{key} 应当是 database::SENSITIVE_KEYS 的成员"
            );
            assert!(
                !is_setting_sync_eligible(key),
                "{key} 已经修复，不得再参与云同步"
            );
        }
    }

    /// 痕迹键必须是**由上传动作写入**的那几个，而不是随手挑的。
    ///
    /// 本测试用真实的写入点作为证据来源：`..._push_at` 就写在
    /// `upload_webdav_settings_snapshot` 成功返回之后。这里断言它在清单里，
    /// 并断言清单不会长到把 `cloud_sync_enabled` 这种"用户偏好"键也吃进来
    /// （那会让判据退化成"开关一开就提示"，从而漏掉"配过又关掉"以外的语义）。
    #[test]
    fn evidence_keys_are_the_upload_written_ones() {
        assert!(CREDENTIAL_EXPOSURE_EVIDENCE_KEYS
            .contains(&"cloud_sync_webdav_last_snapshot_push_at"));
        assert!(CREDENTIAL_EXPOSURE_EVIDENCE_KEYS.contains(&"cloud_sync_settings_applied_at"));
        assert!(!CREDENTIAL_EXPOSURE_EVIDENCE_KEYS.contains(&"cloud_sync_enabled"));
        assert!(!CREDENTIAL_EXPOSURE_EVIDENCE_KEYS.contains(&"app.anon_id"));
    }

    const TEST_PNG_BYTES: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 248, 15, 4, 0, 9,
        251, 3, 253, 160, 164, 95, 122, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    fn make_temp_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tiez-cloud-sync-{name}-{unique}"));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn rewrite_rich_html_resources_for_sync_inlines_local_images_and_fallbacks() {
        let dir = make_temp_dir("rich-html");
        let image_path = dir.join("inline.png");
        fs::write(&image_path, TEST_PNG_BYTES).expect("write test png");

        let image_path_str = image_path.to_string_lossy().replace('\\', "/");
        let html = format!(
            "<div><img src=\"file://{}\"></div>\n{}{}{}",
            image_path_str, RICH_IMAGE_FALLBACK_PREFIX, image_path_str, RICH_IMAGE_FALLBACK_SUFFIX
        );

        let rewritten = rewrite_rich_html_resources_for_sync(&html);

        assert!(rewritten.contains("src=\"data:image/png;base64,"));
        assert!(rewritten.contains(RICH_IMAGE_FALLBACK_PREFIX));
        assert!(rewritten.contains("data:image/png;base64,"));
        assert!(!rewritten.contains(&format!("file://{}", image_path_str)));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn normalize_item_for_sync_rewrites_rich_html_local_resources() {
        let dir = make_temp_dir("normalize-item");
        let image_path = dir.join("entry.png");
        fs::write(&image_path, TEST_PNG_BYTES).expect("write test png");

        let item = CloudSyncItem {
            content_type: "rich_text".to_string(),
            content: "hello".to_string(),
            content_hash: 0,
            deleted_at: 0,
            html_content: Some(format!(
                "<p>Hello</p><img src=\"{}\">",
                image_path.to_string_lossy()
            )),
            content_blob_hash: None,
            html_blob_hash: None,
            source_app: "Test".to_string(),
            timestamp: 1,
            preview: "hello".to_string(),
            is_pinned: false,
            tags: vec![],
            use_count: 0,
            pinned_order: 0,
            note: String::new(),
        };

        let normalized = normalize_item_for_sync(item).expect("normalized item");
        let html = normalized.html_content.expect("html content");

        assert!(html.contains("src=\"data:image/png;base64,"));
        assert!(!html.contains("entry.png"));

        let _ = fs::remove_dir_all(dir);
    }
}
