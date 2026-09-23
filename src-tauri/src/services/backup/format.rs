//! 备份包的**格式契约**：manifest 字段、版本策略、校验和、错误码。
//!
//! # 版本策略（前向 + 后向兼容）
//!
//! 本应用自有版本之间必须**双向可读**：新版能读旧包，旧版也能读新包。做法是把
//! "变化"分为三档，只有最后一档才允许升 `format_version`：
//!
//! | 变化类型 | 做法 | 是否升 `format_version` | 旧版能读吗 |
//! |---|---|---|---|
//! | 新增**可选字段** | manifest 里加字段，读取端用 `#[serde(default)]`；写入端在不需要时**不写** | 否 | 能（旧版忽略未知字段） |
//! | 新增**附加数据** | zip 里加**新的条目路径**（如 `background/`）；旧版只找它认识的路径 | 否 | 能（旧版跳过未知条目） |
//! | 破坏性变更（旧版读到会误判/损坏数据） | 提升 `format_version` | 是 | **不能**，这是明示的取舍 |
//!
//! 换句话说：**`format_version` 只标记"旧版读不了"的变更**，而正常的增量功能增强
//! 走"可选字段 + 新条目路径"这两条无痛通道。这样"旧版能读新版包"就成了一项可以
//! 长期维持的工程约束，而不是一句口号。
//!
//! 当前 `format_version = 1`。本版新增的 `background_map.json` 与 `background/`
//! 恰好就是"新条目路径"这一档：一个只知道 v1 三条路径的旧版读取端会直接跳过它们，
//! 因此**不需要**升版本号。

use crate::error::AppError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

/// 备份包的应用标识，用于**拒绝非本应用的包**（尤其是 TieZ 原版）。
///
/// 取值刻意与 Tauri 的 `identifier`（`com.tieznext`）区分开：它是"包的格式归属"，
/// 不是"安装身份"。原版 TieZ 的 `com.tiez` / `com.tiez.app` 都不等于它。
pub const APP_ID: &str = "com.tieznext.backup";

/// 本版写入端写出的格式版本。
///
/// 注意：**不要为了让读取端"知道这是新版"而随意提升它**——提升的语义是"旧版读不了
/// 这个包"。纯增量请走"可选字段 + 新条目路径"。
pub const FORMAT_VERSION_CURRENT: u32 = 1;

/// 本版读取端仍能正确解析的最老格式版本。
///
/// 读取端按 `format_version` 分支（[`ManifestVersion`]），旧版本包在语义缺少的
/// 地方用默认值补齐。
pub const FORMAT_VERSION_MIN: u32 = 1;

// ---------------------------------------------------------------------------
// zip 内的条目路径（v1 契约）
// ---------------------------------------------------------------------------

/// 清单文件路径。**必须有**，缺了直接拒绝，不做任何猜测解析。
pub const ENTRY_MANIFEST: &str = "manifest.json";
/// 数据库快照。由 `VACUUM INTO` 生成，因此包含未 checkpoint 的 WAL 数据。
pub const ENTRY_DATABASE: &str = "clipboard.db";
/// 附件目录前缀。
pub const ENTRY_ATTACHMENTS_PREFIX: &str = "attachments/";
/// 表情收藏目录前缀（磁盘那一份）。
pub const ENTRY_EMOJI_PREFIX: &str = "emoji_favorites/";
/// 自定义背景图的存放前缀（**本版新增的附加条目**，v1 旧读取端会跳过）。
pub const ENTRY_BACKGROUND_PREFIX: &str = "background/";
/// 自定义背景图的映射表（**本版新增的附加条目**）。
pub const ENTRY_BACKGROUND_MAP: &str = "background_map.json";

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// 备份导出/导入的失败原因。
///
/// 每个变体都有稳定的 [`BackupError::code`]，前端据此映射成当前语言的人话——
/// 与项目里 `legacy_migrate_notice_<code>` 的既有约定一致，避免把内部英文串
/// 甩给用户。
#[derive(Debug, Clone)]
pub enum BackupError {
    /// 包不是本应用导出的（含 TieZ 原版），或 manifest 缺失/不可解析。
    ForeignApp(String),
    /// 包由**更新**的格式版本导出，本版读不懂。提示用户升级应用。
    FormatTooNew { found: u32, supported: u32 },
    /// 不是合法 zip / 读不出中央目录。
    InvalidZip(String),
    /// 某个条目的校验和与 manifest 不符（包损坏或被改动）。
    ChecksumMismatch { entry: String },
    /// manifest 里声明的条目数与包里实际数量不符。
    CountMismatch { what: String, expected: u64, actual: u64 },
    /// 数据落盘阶段的失败（会把已做的改动回滚）。
    Land(String),
    /// 纯 I/O 失败。
    Io(String),
}

impl BackupError {
    /// 稳定的机器可读错误码，供前端做多语言映射。
    pub fn code(&self) -> &'static str {
        match self {
            BackupError::ForeignApp(_) => "foreign_app",
            BackupError::FormatTooNew { .. } => "format_too_new",
            BackupError::InvalidZip(_) => "invalid_zip",
            BackupError::ChecksumMismatch { .. } => "checksum_mismatch",
            BackupError::CountMismatch { .. } => "count_mismatch",
            BackupError::Land(_) => "land_failed",
            BackupError::Io(_) => "io",
        }
    }
}

impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackupError::ForeignApp(detail) => write!(
                f,
                "这不是 Tiez-Next 的备份包（{}）。原版 TieZ 的备份格式不同，无法导入；请确认选择的是由 Tiez-Next 导出的 .zip 文件。",
                detail
            ),
            BackupError::FormatTooNew { found, supported } => write!(
                f,
                "备份包的格式版本为 {}，本版应用最高支持 {}。包由更新版本的应用导出，请升级应用后再导入。",
                found, supported
            ),
            BackupError::InvalidZip(detail) => {
                write!(f, "备份包无法读取（可能已损坏或不是 zip 文件）：{}", detail)
            }
            BackupError::ChecksumMismatch { entry } => write!(
                f,
                "备份包内容校验失败（{} 与清单记录不符），包可能已损坏或被改动；未改动你的任何现有数据。",
                entry
            ),
            BackupError::CountMismatch {
                what,
                expected,
                actual,
            } => write!(
                f,
                "备份包内容数量不符（{}：清单记录 {}，实际 {}）；未改动你的任何现有数据。",
                what, expected, actual
            ),
            BackupError::Land(detail) => write!(f, "写入数据时失败，已回滚到导入前的状态：{}", detail),
            BackupError::Io(detail) => write!(f, "文件系统错误：{}", detail),
        }
    }
}

impl std::error::Error for BackupError {}

impl From<std::io::Error> for BackupError {
    fn from(e: std::io::Error) -> Self {
        BackupError::Io(e.to_string())
    }
}

impl From<BackupError> for AppError {
    fn from(e: BackupError) -> Self {
        AppError::Validation(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// manifest
// ---------------------------------------------------------------------------

/// 备份包内各项内容的数量，用于导入后**对账**。
///
/// 全部字段都带 `#[serde(default)]`：字段缺失时按 0 处理并跳过该项的严格对账，
/// 而不是报错——这样旧版（写得更少）的包仍能被新版接受。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestCounts {
    /// `clipboard_history` 行数。
    #[serde(default)]
    pub entries: u64,
    /// `saved_tags` 行数。
    #[serde(default)]
    pub tags: u64,
    /// `attachments/` 下的文件数。
    #[serde(default)]
    pub attachments: u64,
    /// `emoji_favorites/` 下的文件数。
    #[serde(default)]
    pub emoji_favorites: u64,
    /// `settings` 行数。
    #[serde(default)]
    pub settings: u64,
}

/// 包的清单。
///
/// **没有** `deny_unknown_fields`：新版写入端可能添加了本版不认识的字段，读取端必须
/// 安全忽略而不是报错。这正是"新版能读旧包、旧版能读新包"在字段层面的实现方式。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    /// 格式版本。读取端据此分支，并拒绝**比自己新**的版本。
    pub format_version: u32,
    /// 应用标识。不等于 [`APP_ID`] 一律拒绝（含 TieZ 原版）。
    pub app: String,
    /// 导出时的应用版本，仅用于展示与排障。
    #[serde(default)]
    pub app_version: String,
    /// 导出时刻（RFC3339）。
    #[serde(default)]
    pub exported_at: String,
    /// 数据库 schema 版本（`schema_migrations` 的最大值）。
    #[serde(default)]
    pub schema_version: i64,
    /// 各项数量。
    #[serde(default)]
    pub counts: ManifestCounts,
    /// 条目名 → `sha256:<hex>`。用于"先校验后落地"。
    #[serde(default)]
    pub checksums: std::collections::BTreeMap<String, String>,
    /// 导出侧的说明性备注，仅用于展示。旧版读到会忽略。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// 背景图映射表里的**一条**记录：设置项里的原绝对路径 → 包内条目名。
///
/// 这套附加条目是本版新增的，因此不需要升 `format_version`：不认识它的旧读取端
/// 只会跳过这两个文件，其余数据照常恢复。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundMapEntry {
    /// 导出时设置项 `app.custom_background` 里的绝对路径（原样保留，仅供展示与排障）。
    #[serde(default)]
    pub original_path: String,
    /// 文件字节在包内的条目名（`background/<sha256>.<ext>`）。
    #[serde(default)]
    pub entry: String,
    /// 该条目的 `sha256:<hex>`。
    #[serde(default)]
    pub sha256: String,
    /// 导出的原始文件名（含扩展名），导入时据此还原文件名。
    #[serde(default)]
    pub file_name: String,
}

/// `background_map.json` 的文件结构。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackgroundMapFile {
    #[serde(default = "default_one")]
    pub map_version: u32,
    #[serde(default)]
    pub items: Vec<BackgroundMapEntry>,
}

/// `mappings.json` 里的附加条目路径（数据目录内引用的路径改写表）。
pub const ENTRY_PATH_MAP: &str = "mappings.json";

fn default_one() -> u32 {
    1
}

/// 读取端对 `format_version` 的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestVersion {
    /// v1：基础结构（数据库 + 附件 + 表情收藏）。当前所有包都落在这里。
    V1,
}

impl ManifestVersion {
    /// 校验并归一化格式版本号。
    ///
    /// - 低于 [`FORMAT_VERSION_MIN`]：本版已不再支持，拒绝。
    /// - 高于 [`FORMAT_VERSION_CURRENT`]：包比本版新，拒绝并提示升级（**不猜测解析**）。
    fn resolve(found: u32) -> Result<Self, BackupError> {
        if found > FORMAT_VERSION_CURRENT {
            return Err(BackupError::FormatTooNew {
                found,
                supported: FORMAT_VERSION_CURRENT,
            });
        }
        if found < FORMAT_VERSION_MIN {
            return Err(BackupError::ForeignApp(format!(
                "format_version={} 低于本版支持的最低版本 {}",
                found, FORMAT_VERSION_MIN
            )));
        }
        // 目前只有 v1。将来新增版本时在这里加分支，并让更老的版本落到对应处理臂。
        Ok(ManifestVersion::V1)
    }

    /// 该版本下必须存在的 zip 条目（缺失即视为包不完整）。
    pub fn required_entries(self) -> &'static [&'static str] {
        match self {
            ManifestVersion::V1 => &[ENTRY_DATABASE],
        }
    }
}

/// 解析 manifest 字节，并完成"这是不是本应用的包"与版本判定。
///
/// 判定顺序刻意是"先归属、后版本"：一个原版 TieZ 的包哪怕版本号看起来正常，也会在
/// 第一步被拒，不会走到任何解析或落盘逻辑。
pub fn parse_manifest(bytes: &[u8]) -> Result<(BackupManifest, ManifestVersion), BackupError> {
    let manifest: BackupManifest = serde_json::from_slice(bytes).map_err(|e| {
        BackupError::ForeignApp(format!("manifest.json 无法解析：{}", e))
    })?;

    // 归属判定：只认本应用标识。原版 TieZ（com.tiez / com.tiez.app）与任何第三方
    // 应用在这里被明确拒绝，且不做"尝试兼容解析"。
    if manifest.app != APP_ID {
        return Err(BackupError::ForeignApp(format!(
            "manifest.app = {:?}，本应用要求 {:?}",
            manifest.app, APP_ID
        )));
    }

    let version = ManifestVersion::resolve(manifest.format_version)?;
    Ok((manifest, version))
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

/// 流式计算文件的 sha256，返回 `sha256:<hex>`。
///
/// 分块读取，内存占用与文件大小无关——备份可能包含大量附件，不能整文件读进内存。
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

/// 对内存字节求 sha256，返回 `sha256:<hex>`。
pub fn sha256_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// 把 zip 条目路径做**安全归一化**：统一分隔符、去掉前导 `./`，并拒绝任何会逃出
/// 目标目录的路径（`..`、绝对路径、盘符）。
///
/// 这是防"zip slip"的关键一步：恶意或损坏的包可能带 `../../../etc/passwd` 这类
/// 条目名。返回 `None` 表示该条目不安全，调用方会跳过它（而不是写出去）。
pub fn safe_relative_path(entry_name: &str) -> Option<String> {
    let normalized = entry_name.replace('\\', "/");
    let trimmed = normalized.trim_start_matches("./");
    if trimmed.is_empty() {
        return None;
    }
    // 绝对路径 / 盘符 / UNC：一律拒绝。
    if trimmed.starts_with('/') || trimmed.contains(':') {
        return None;
    }
    for segment in trimmed.split('/') {
        if segment == ".." {
            return None;
        }
    }
    Some(trimmed.to_string())
}

/// 把**包内元数据里携带的文件名**净化成一个安全的单层文件名。
///
/// # 为什么必须有这一步
///
/// `background_map.json` 里的 `file_name` 与 `mappings.json` 里的键都是**包内数据**，
/// 也就是不可信输入。它们会被拼到数据目录下做文件操作：
///
/// ```text
/// staging/background/<file_name>      // 还原背景图时的写入目标
/// data_dir/<mappings 的键>            // 路径改写后的目标
/// ```
///
/// 若直接拼接，一个恶意/损坏的包只要把 `file_name` 写成 `..\..\..\evil.exe`，
/// 就能让 `join` 产生逃出数据目录的路径，把包内任意字节写到用户机器上的任意位置
/// ——这是 zip-slip 的同类漏洞，只是入口从 zip 条目名换成了 JSON 字段。
/// `safe_relative_path` 只保护了 zip 条目名，**不覆盖**这两个 JSON 字段。
///
/// 净化规则（任一条不满足即拒绝，返回 `None`）：
/// - 只取最后一段路径分量（剥离任何目录成分）；
/// - 不得是 `.` / `..` / 空；
/// - 不得含 `/`、`\`、`:`（Windows 盘符与 NTFS 数据流）、空字节；
/// - 不得是 Windows 保留设备名（`CON`/`NUL`/`COM1`…，它们会被解释成设备而非文件）。
pub fn sanitize_file_name(raw: &str) -> Option<String> {
    let cleaned = raw.trim();
    if cleaned.is_empty() || cleaned.contains('\0') {
        return None;
    }
    // 先把两种分隔符统一，再取最后一段：任何目录成分都被丢弃而不是"检查后放行"，
    // 这样即使写法千奇百怪，结果也只可能是当前目录下的一个文件名。
    let unified = cleaned.replace('\\', "/");
    let last = unified.rsplit('/').next().unwrap_or("");
    if last.is_empty() || last == "." || last == ".." {
        return None;
    }
    if last.contains(':') {
        return None;
    }
    // Windows 保留设备名（大小写不敏感，且带扩展名也算，如 `NUL.txt`）。
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
        "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = last.split('.').next().unwrap_or("").to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        return None;
    }
    Some(last.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_json(extra: &str) -> String {
        let base = serde_json::json!({
            "format_version": 1,
            "app": APP_ID,
            "app_version": "0.3.4",
            "exported_at": "2026-09-23T00:00:00Z",
            "schema_version": 11,
            "counts": {
                "entries": 2, "tags": 1, "attachments": 1,
                "emoji_favorites": 0, "settings": 5
            }
        });
        let mut s = serde_json::to_string(&base).unwrap();
        if !extra.is_empty() {
            // `extra` 是 `{"k":v,"k2":v2}` 形式；直接拼接会得到非法 JSON，
            // 因此把它解析后并入对象。
            let extra_value: serde_json::Value = serde_json::from_str(extra).unwrap();
            let mut obj = base;
            if let (Some(dst), Some(src)) = (obj.as_object_mut(), extra_value.as_object()) {
                for (k, v) in src {
                    dst.insert(k.clone(), v.clone());
                }
            }
            s = serde_json::to_string(&obj).unwrap();
        }
        s
    }

    #[test]
    fn accepts_own_manifest() {
        let (m, v) = parse_manifest(manifest_json("").as_bytes()).unwrap();
        assert_eq!(v, ManifestVersion::V1);
        assert_eq!(m.counts.entries, 2);
        assert_eq!(m.counts.settings, 5);
    }

    /// 后向兼容的字段层证据：新版写入端多写的字段必须被**忽略**而不是报错。
    #[test]
    fn ignores_unknown_manifest_fields() {
        let json = r#"{"format_version":1,"app":"com.tieznext.backup",
            "future_feature":{"nested":[1,2,3]},"another":"x","counts":{"entries":7}}"#;
        let (m, _) = parse_manifest(json.as_bytes()).unwrap();
        assert_eq!(m.counts.entries, 7);
    }

    /// 旧版写入端写得更少时，新版读取端必须能用默认值补齐。
    #[test]
    fn tolerates_missing_optional_fields_from_older_writer() {
        let json = r#"{"format_version":1,"app":"com.tieznext.backup"}"#;
        let (m, _) = parse_manifest(json.as_bytes()).unwrap();
        assert_eq!(m.counts.entries, 0);
        assert_eq!(m.schema_version, 0);
        assert!(m.checksums.is_empty());
        assert!(m.notes.is_empty());
    }

    /// 拒绝 TieZ 原版：app 标识不同即明确拒绝，不做猜测解析。
    #[test]
    fn rejects_upstream_tiez_package() {
        for app in ["com.tiez", "com.tiez.app", "tiez", ""] {
            let json = format!(
                r#"{{"format_version":1,"app":"{}","counts":{{"entries":9}}}}"#,
                app
            );
            let err = parse_manifest(json.as_bytes()).unwrap_err();
            assert_eq!(err.code(), "foreign_app", "app={:?} 必须被拒绝", app);
        }
    }

    /// 缺 manifest / 不是 JSON：拒绝，不尝试猜测。
    #[test]
    fn rejects_unparsable_manifest() {
        let err = parse_manifest(b"not json at all").unwrap_err();
        assert_eq!(err.code(), "foreign_app");
    }

    /// 比本版新的格式版本必须被拒绝并提示升级（而不是硬解析）。
    #[test]
    fn rejects_newer_format_version() {
        let json = format!(
            r#"{{"format_version":{},"app":"{}"}}"#,
            FORMAT_VERSION_CURRENT + 1,
            APP_ID
        );
        let err = parse_manifest(json.as_bytes()).unwrap_err();
        assert_eq!(err.code(), "format_too_new");
    }

    #[test]
    fn rejects_version_below_min() {
        let json = format!(r#"{{"format_version":0,"app":"{}"}}"#, APP_ID);
        let err = parse_manifest(json.as_bytes()).unwrap_err();
        assert_eq!(err.code(), "foreign_app");
    }

    /// zip-slip 防护：逃出目标目录的条目名一律判为不安全。
    #[test]
    fn rejects_escaping_entry_names() {
        assert_eq!(safe_relative_path("attachments/a.png").as_deref(), Some("attachments/a.png"));
        assert_eq!(safe_relative_path("./clipboard.db").as_deref(), Some("clipboard.db"));
        assert_eq!(safe_relative_path("a\\b\\c.png").as_deref(), Some("a/b/c.png"));
        assert!(safe_relative_path("../../etc/passwd").is_none());
        assert!(safe_relative_path("/etc/passwd").is_none());
        assert!(safe_relative_path("C:/Windows/x.dll").is_none());
        assert!(safe_relative_path("a/../../b").is_none());
        assert!(safe_relative_path("").is_none());
    }

    /// 包内 JSON 字段携带的文件名必须被净化——这是 zip-slip 的同类入口。
    #[test]
    fn sanitizes_package_supplied_file_names() {
        // 正常名保留
        assert_eq!(sanitize_file_name("bg.png").as_deref(), Some("bg.png"));
        assert_eq!(sanitize_file_name(" 我的 背景.png ").as_deref(), Some("我的 背景.png"));
        // 路径成分被剥离，只剩最后一段（而不是"检查后放行"）
        assert_eq!(sanitize_file_name("a/b/c.png").as_deref(), Some("c.png"));
        assert_eq!(sanitize_file_name("..\\..\\evil.exe").as_deref(), Some("evil.exe"));
        assert_eq!(sanitize_file_name("C:\\Windows\\x.dll").as_deref(), Some("x.dll"));
        // 无法得到合法文件名的一律拒绝
        assert!(sanitize_file_name("..").is_none());
        assert!(sanitize_file_name("a/..").is_none());
        assert!(sanitize_file_name("").is_none());
        assert!(sanitize_file_name("   ").is_none());
        assert!(sanitize_file_name("dir/").is_none());
        assert!(sanitize_file_name("with:colon.png").is_none());
        // Windows 保留设备名
        assert!(sanitize_file_name("NUL").is_none());
        assert!(sanitize_file_name("con.txt").is_none());
        assert!(sanitize_file_name("COM1").is_none());
        assert!(sanitize_file_name("lpt9.png").is_none());
        assert!(sanitize_file_name("CONSOLE.png").is_some(), "非保留名不应误伤");
    }

    #[test]
    fn sha256_is_stable_and_streamed() {
        let dir = std::env::temp_dir().join(format!(
            "tiez-sha-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.bin");
        std::fs::write(&f, b"hello").unwrap();
        // 空字符串的 sha256 与被广泛引用的常量一致，可交叉核对
        assert_eq!(
            sha256_bytes(b""),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(sha256_file(&f).unwrap(), sha256_bytes(b"hello"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
