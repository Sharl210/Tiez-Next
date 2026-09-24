//! 自动容灾备份的**存储与轮换层**。
//!
//! # 它解决什么
//!
//! 用户要的是一层"保险"：定时往一个自己的目录里放一份完整数据副本，超量时把**最老的
//! 未固定**那份顶掉；某几份可以像行车记录仪那样"固定"住不许删。本模块只负责这件事的
//! **存储与轮换**，打包本身复用既有的 [`crate::services::backup::export`]，恢复复用既有的
//! [`crate::services::backup::import`]——这里一行 zip 代码都不另写。
//!
//! # 目录放在哪（`<数据目录父级>/Tiez-Next/auto_backups/`）
//!
//! 三个候选的实际取舍：
//!
//! | 候选 | 结论 |
//! |---|---|
//! | `<数据目录>/auto_backups/` | **不可行**。`create_backup` 里有一道硬护栏 `guard_output_outside_data_dir`（并有专门测试守着），禁止把备份写进数据目录内部：写同名文件会**截断**目标，若路径被指向 `clipboard.db` 就是不可逆的数据破坏。 |
//! | 系统推荐位置（如 `%LOCALAPPDATA%\com.tieznext\auto_backups\`） | **就是数据目录内**，同上被护栏拒绝；且用户若用 `datapath.txt` 把数据目录改到 D 盘，自动备份却仍留在 C 盘，两者分离本身也不合理。 |
//! | `<数据目录父级>/Tiez-Next/auto_backups/` | **采用**。与数据目录是**兄弟**关系：同盘、同分区、同用户权限，因此 rename/copy 都是同文件系统操作，且不受"程序目录会被卸载器清理"的影响；又因为不是"数据目录内部"，不与既有护栏冲突。用户改数据目录时，自动备份跟着走。 |
//!
//! ## 容灾的边界：防的是数据损坏/误操作，不是磁盘故障
//!
//! 这一点必须说清，否则会引出过度设计（加密、异地、双盘、云上传……）：
//!
//! - **要防的**：误删、误改、导入坏包、数据库被写坏、升级把数据搞崩——这时同盘的另一份
//!   zip 就是完整的回购路径。这类事故是**逻辑损坏**，同盘副本完全够用，而且恢复最快。
//! - **不防的**：整盘故障、勒索软件加密全盘、机器丢失。同盘副本在这些场景下和原数据一起
//!   消失。要覆盖它们，需要的是"用户把 zip 拷到别处"或云备份，那是**另一件事**：它要么
//!   需要用户选路径（就是既有的手动导出），要么需要网络与凭据（就是既有的云同步）。
//! - 因此本模块**不做**：自动写用户选定的外部路径、加密打包、上传远端、跨盘镜像。不是做
//!   不到，而是那会把"保险"变成"又一个会失败、会要权限、会要空间的后台任务"，而收益
//!   （多防一类低频事故）与复杂度不成比例。用户的手动导出与云同步仍然各自把这两类覆盖住。
//!
//! # 固定状态存在哪：**文件名标记 + 同名侧车索引，两份**
//!
//! 这是本模块最需要想清楚的一处，因为**改一次名就可能把固定状态弄丢**。三条路线：
//!
//! | 方案 | 问题 |
//! |---|---|
//! | 只放 `settings` / 数据库 | 固定状态与文件**分离**：用户手工搬动、复制、清理目录后，索引说"这份固定"而文件已不在，或反过来——**静默把用户固定过的备份删掉**。 |
//! | 只放文件名（如 `…-p.zip`） | 一旦有任何一方改了名，状态就无声丢失；目录里也看不出"这份为什么没被删"。 |
//! | **两份都放**（采用） | 见下。 |
//!
//! 采用的做法是：**固定 = 在文件名尾部加一个 `-p` 标记，同时把文件名记进目录里的
//! `pinned-paths.json`**。原因有两条，都很实际：
//!
//! 1. **重启后必须还在**：索引是磁盘文件而不是进程内存，因此重启、崩溃、换进程都能读到；
//!    文件名里的 `-p` 是同一事实的第二份拷贝，索引损坏时还能从名字恢复（见
//!    [`AutoBackupStore::load`] 的并集逻辑），**永不静默取消固定**。
//! 2. **用户看得见**：用户直接打开这个目录时，固定住的文件名里就有标记，不需要装懂内部索引。
//!
//! 反过来，**未固定**的备份名字里没有标记，因此"文件名带 `-p`"和"索引里有"取**并集**——
//! 只会多保护，不会少保护。取消固定时必须**两处同时去掉**（两个动作都做完才算成功，
//! 否则回退），这就是"改名会让固定状态丢失"那个坑的堵法。
//!
//! # 只管理自己认识的文件
//!
//! 目录里不是本模块命名规则的文件（用户自己丢进来的 zip、别的程序放的说明文件）**一律
//! 不列表、不轮换、不删除**。理由很直白：轮换会删文件，一个"看不懂就跳过"的规则是这类
//! 代码里最便宜也最必要的保险。

use crate::services::backup::export::{create_backup, BackupRequest};
use crate::services::backup::format::BackupError;
use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use serde_json::json;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::config::{AutoBackupConfig, ConfigError};

/// 自动备份根目录的固定名字（与数据目录同级）。
pub const AUTO_DIR_SIBLING_NAME: &str = "Tiez-Next";
/// 自动备份子目录名。
pub const AUTO_DIR_LEAF_NAME: &str = "auto_backups";
/// 文件名统一前缀。
pub const NAME_PREFIX: &str = "Tiez-Next-auto-";
/// 文件名扩展名。
pub const NAME_EXT: &str = ".zip";
/// 文件名里的"已固定"标记（`-p`）。
pub const PIN_TOKEN: &str = "p";
/// 固定状态索引文件名（放在备份目录内，与备份一起被用户搬走）。
pub const PIN_INDEX_NAME: &str = "pinned-paths.json";

/// 备份的来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupOrigin {
    /// 定时备份。
    Scheduled,
    /// 软件启动时的自动备份。**与定时备份同目录**（同属容灾保险），但不受定时开关约束。
    Startup,
    /// 用户手动"立即备份一次"。
    Manual,
}

impl BackupOrigin {
    pub fn tag(self) -> &'static str {
        match self {
            BackupOrigin::Scheduled => "timed",
            BackupOrigin::Startup => "startup",
            BackupOrigin::Manual => "manual",
        }
    }

    pub fn parse(tag: &str) -> Option<Self> {
        match tag {
            "timed" => Some(BackupOrigin::Scheduled),
            "startup" => Some(BackupOrigin::Startup),
            "manual" => Some(BackupOrigin::Manual),
            _ => None,
        }
    }

    /// 是否属于"容灾自动保险"（定时 + 启动）。
    ///
    /// 这两类是列表要展示的；手动触发的那一份属于同一条保险链，也一并展示，但来源标签不同。
    pub fn is_automatic(self) -> bool {
        matches!(self, BackupOrigin::Scheduled | BackupOrigin::Startup)
    }
}

/// 解析后的文件名信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedName {
    pub origin: BackupOrigin,
    /// 精确到秒的创建时刻（本地时区）。
    pub stamp: NaiveDateTime,
    /// 同秒内的序号（从 1 开始）。
    pub seq: u32,
    /// 文件名里是否带"已固定"标记。
    pub pinned_token: bool,
}

/// 一条自动备份在界面上的完整表示。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupEntry {
    /// 文件名（界面用它作为唯一标识来调用固定/删除/恢复命令）。
    pub archive_name: String,
    /// 绝对路径。
    pub path: String,
    /// 来源：`scheduled` / `startup` / `manual`。
    pub origin: String,
    /// 创建时刻，RFC3339 且**带秒**（用户要求"时间精确到秒"）。
    pub created_at: String,
    /// 创建时刻的毫秒时间戳（本地时区）。
    ///
    /// 与 [`Self::created_at`] 同时给出：字符串供界面显示与排序，数值供**纯函数**做
    /// "是否到了下一次备份时间"的判断，避免在每轮调度里解析字符串。
    pub created_at_ms: i64,
    /// 人类可读的本地时间 `YYYY-MM-DD HH:MM:SS`，界面直接显示。
    pub created_at_local: String,
    /// 文件字节数。
    pub size_bytes: u64,
    /// 是否已固定（不会被轮换删除）。
    pub pinned: bool,
    /// 同秒内的序号，用于同秒多条时的稳定排序。
    pub seq: u32,
}

/// 一次轮换的执行结果。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RotationOutcome {
    /// 已删除（超限的最老未固定）备份。
    pub deleted: Vec<String>,
    /// 因**全部被固定**而删不掉、导致总数仍超过上限的份数。
    pub undelatable_excess: u32,
    /// 需要让用户知道的非致命情况。
    pub warnings: Vec<String>,
}

impl RotationOutcome {
    /// 当前是否仍有删不掉的多余份数（全部被固定住，导致总数超上限）。
    pub fn has_undelatable_excess(&self) -> bool {
        self.undelatable_excess > 0
    }
}

/// 固定上限的可区分原因：前端据此弹出用户要求的那段提示，而不是一句泛泛的失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedLimit {
    /// 当前配置的最大留存份数。
    pub max_keep: u32,
    /// 允许固定的最大条数（= `max_keep - 1`）。
    pub max_pinned: u32,
    /// 当前已固定的条数。
    pub current_pinned: u32,
}

/// 本模块的错误。每个变体都带**稳定的机器可读码**与结构化载荷。
#[derive(Debug, Clone)]
pub enum AutoBackupError {
    /// 固定数已达上限（`max_keep - 1`）。
    PinnedLimitReached(PinnedLimit),
    /// 备份目录里不存在这个名字。
    NotFound(String),
    /// 传进来的名字不是本模块管理的备份名（含路径分隔符、扩展名不对等）。
    InvalidName(String),
    /// 备份目录落在数据目录内部，会被成品代码里的护栏拒绝。
    DirInsideDataDir { dir: PathBuf, data_dir: PathBuf },
    /// 打包失败。
    Export(String),
    /// 目录操作失败。
    Io(String),
    /// 配置越界。
    Config(ConfigError),
}

impl AutoBackupError {
    pub fn code(&self) -> &'static str {
        match self {
            AutoBackupError::PinnedLimitReached(_) => "auto_backup_pinned_limit_reached",
            AutoBackupError::NotFound(_) => "auto_backup_not_found",
            AutoBackupError::InvalidName(_) => "auto_backup_invalid_name",
            AutoBackupError::DirInsideDataDir { .. } => "auto_backup_dir_inside_data_dir",
            AutoBackupError::Export(_) => "auto_backup_export_failed",
            AutoBackupError::Io(_) => "auto_backup_io",
            AutoBackupError::Config(c) => c.code(),
        }
    }

    /// 结构化载荷。前端拿到 `code` 就能查到当前语言的那句话；数值字段用于**在文案里填数**
    /// （"当前配置最大留存备份数量为 {max_keep}，而当前您已固定 {current_pinned} 个"）。
    pub fn payload(&self) -> serde_json::Value {
        match self {
            AutoBackupError::PinnedLimitReached(l) => json!({
                "code": self.code(),
                "maxKeep": l.max_keep,
                "maxPinned": l.max_pinned,
                "currentPinned": l.current_pinned,
            }),
            AutoBackupError::NotFound(name) => json!({"code": self.code(), "name": name}),
            AutoBackupError::InvalidName(detail) => {
                json!({"code": self.code(), "detail": detail})
            }
            AutoBackupError::DirInsideDataDir { dir, data_dir } => json!({
                "code": self.code(),
                "dir": dir.to_string_lossy(),
                "dataDir": data_dir.to_string_lossy(),
            }),
            AutoBackupError::Export(detail) => json!({"code": self.code(), "detail": detail}),
            AutoBackupError::Io(detail) => json!({"code": self.code(), "detail": detail}),
            AutoBackupError::Config(c) => c.payload(),
        }
    }
}

impl std::fmt::Display for AutoBackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AutoBackupError::PinnedLimitReached(l) => write!(
                f,
                "当前配置最大留存备份数量为 {}，而当前您已固定 {} 个，不能全部设置为固定，否则后续新增备份没有轮换位可用于存储。",
                l.max_keep, l.current_pinned
            ),
            AutoBackupError::NotFound(name) => write!(f, "找不到这份自动备份：{}", name),
            AutoBackupError::InvalidName(detail) => {
                write!(f, "这不是一份自动备份的文件名：{}", detail)
            }
            AutoBackupError::DirInsideDataDir { dir, data_dir } => write!(
                f,
                "自动备份目录不能位于数据目录内部（{} ⊂ {}）：备份必须放在数据之外，否则备份和原数据会一起损坏。",
                dir.display(),
                data_dir.display()
            ),
            AutoBackupError::Export(detail) => write!(f, "生成自动备份失败：{}", detail),
            AutoBackupError::Io(detail) => write!(f, "自动备份目录操作失败：{}", detail),
            AutoBackupError::Config(c) => write!(f, "{}", c),
        }
    }
}

impl std::error::Error for AutoBackupError {}

impl From<ConfigError> for AutoBackupError {
    fn from(c: ConfigError) -> Self {
        AutoBackupError::Config(c)
    }
}

impl From<std::io::Error> for AutoBackupError {
    fn from(e: std::io::Error) -> Self {
        AutoBackupError::Io(e.to_string())
    }
}

impl From<BackupError> for AutoBackupError {
    fn from(e: BackupError) -> Self {
        AutoBackupError::Export(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// 命名
// ---------------------------------------------------------------------------

/// 时间戳格式：`YYYYMMDDTHHMMSS`（本地时间，**精确到秒**）。
///
/// 刻意不用 `:`（Windows 文件名不允许）也刻意不截到分钟：用户要求"时间精确到秒"，
/// 而且同分钟内连点两次"立即备份"必须能各留一份，文件名必须能区分它们。
const STAMP_FMT: &str = "%Y%m%dT%H%M%S";

/// 生成一个备份文件名。
fn build_name(origin: BackupOrigin, stamp: NaiveDateTime, seq: u32, pinned: bool) -> String {
    let pin = if pinned {
        format!("-{}", PIN_TOKEN)
    } else {
        String::new()
    };
    format!(
        "{}{}-{}-{:02}{}{}",
        NAME_PREFIX,
        origin.tag(),
        stamp.format(STAMP_FMT),
        seq.max(1),
        pin,
        NAME_EXT
    )
}

/// 解析一个备份文件名。不是本模块命名的返回 `None`（于是它既不会被列出来，也不会被删）。
pub fn parse_name(file_name: &str) -> Option<ParsedName> {
    let rest = file_name.strip_prefix(NAME_PREFIX)?;
    let rest = rest.strip_suffix(NAME_EXT)?;
    let parts: Vec<&str> = rest.split('-').collect();
    if parts.len() < 2 || parts.len() > 4 {
        return None;
    }
    let origin = BackupOrigin::parse(parts[0])?;
    let stamp = NaiveDateTime::parse_from_str(parts[1], STAMP_FMT).ok()?;

    let mut seq = 1u32;
    let mut pinned_token = false;
    match &parts[2..] {
        [] => {}
        [one] => {
            if *one == PIN_TOKEN {
                pinned_token = true;
            } else {
                seq = one.parse::<u32>().ok()?;
            }
        }
        [a, b] => {
            seq = a.parse::<u32>().ok()?;
            if *b != PIN_TOKEN {
                return None;
            }
            pinned_token = true;
        }
        _ => return None,
    }
    Some(ParsedName {
        origin,
        stamp,
        seq,
        pinned_token,
    })
}

/// 校验前端传来的名字：必须是一个**纯文件名**，且属于本模块的命名规则。
///
/// 两道都要：`invalid_name` 挡 "不是自动备份"，`..`/分隔符挡路径穿越——名字会被直接
/// `join` 到目录上做删除，是不可信输入。
pub fn validate_archive_name(name: &str) -> Result<(), AutoBackupError> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains(':') {
        return Err(AutoBackupError::InvalidName(name.to_string()));
    }
    if name == "." || name == ".." || name.contains("..") {
        return Err(AutoBackupError::InvalidName(name.to_string()));
    }
    if parse_name(name).is_none() {
        return Err(AutoBackupError::InvalidName(name.to_string()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 轮换算法（纯函数，可在没有文件系统的条件下被直接断言）
// ---------------------------------------------------------------------------

/// 计算"为了满足 `max_keep` 应该删除哪些备份"。
///
/// **规则**（三条，都是用户原话的直接落点）：
/// 1. 按创建时间**从新到旧**排；总数不超过 `max_keep` 就什么都不删。
/// 2. 超出的部分从**最老**的开始删。
/// 3. **遇到固定的一律跳过**，继续往新的方向找下一个"最老未固定"的那份。
///
/// 返回 `(要删的, 删不掉的多余份数)`。第二项只在"未固定的份数仍不足以降到上限"时大于 0
/// ——正常路径下它不是 0 就是超过上限（见模块文档与 [`AutoBackupStore::enforce_rotation`]）。
pub fn plan_rotation(entries: &[BackupEntry], max_keep: u32) -> (Vec<BackupEntry>, u32) {
    let max_keep = max_keep.max(1) as usize;
    if entries.len() <= max_keep {
        return (Vec::new(), 0);
    }

    // 从新到旧排，用**毫秒时间戳**而不是 RFC3339 字符串：字符串里的时区偏移在夏令时
    // 切换的那一天会变，按字典序比较会把新旧关系判反。同秒内再用序号与名字兜底，保证
    // 顺序**确定**（否则"删最老"在秒内多份时会变成看文件系统返回顺序的随机行为，
    // 测试也就成了偶然通过）。
    let mut sorted: Vec<&BackupEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| {
        b.created_at_ms
            .cmp(&a.created_at_ms)
            .then(b.seq.cmp(&a.seq))
            .then(b.archive_name.cmp(&a.archive_name))
    });

    let excess = sorted.len() - max_keep;
    let mut doomed: Vec<BackupEntry> = Vec::new();
    // 从最老的一端（排序的尾部）向新的一端走，只收"未固定"的。
    for entry in sorted.iter().rev() {
        if doomed.len() >= excess {
            break;
        }
        if entry.pinned {
            continue;
        }
        doomed.push((*entry).clone());
    }

    let deficit = excess.saturating_sub(doomed.len()) as u32;
    (doomed, deficit)
}

// ---------------------------------------------------------------------------
// 存储
// ---------------------------------------------------------------------------

/// 固定状态索引文件的结构。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct PinIndex {
    #[serde(default = "default_one")]
    version: u32,
    #[serde(default)]
    pinned: Vec<String>,
}

fn default_one() -> u32 {
    1
}

/// 自动备份目录的读写入口。
///
/// 持有"当前固定集合"的内存视图，但**权威来源始终是磁盘**：每次 [`Self::open`] 都从
/// 文件名与索引文件重建它，因此重启不会丢固定状态。
#[derive(Debug)]
pub struct AutoBackupStore {
    /// 备份目录。
    pub dir: PathBuf,
    /// 已固定的文件名集合（内存视图；磁盘是权威）。
    pinned: BTreeSet<String>,
    /// 读索引时的降级提示（例如索引损坏、已退回按文件名恢复），供上层如实告知用户。
    pub load_warnings: Vec<String>,
}

/// 自动备份目录 = `<数据目录父级>/Tiez-Next/auto_backups/`。
///
/// 数据目录取不到父级时（极少见的相对路径情形）退回数据目录自身——此时 [`AutoBackupStore::open`]
/// 会因"目录落在数据目录内"而明确报错，而不是悄悄把备份写进数据目录。
pub fn auto_backup_dir(data_dir: &Path) -> PathBuf {
    match data_dir.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => {
            parent.join(AUTO_DIR_SIBLING_NAME).join(AUTO_DIR_LEAF_NAME)
        }
        _ => data_dir.join(AUTO_DIR_SIBLING_NAME).join(AUTO_DIR_LEAF_NAME),
    }
}

impl AutoBackupStore {
    /// 打开（必要时创建）备份目录，并**从磁盘**重建固定状态。
    pub fn open(dir: PathBuf, data_dir: &Path) -> Result<Self, AutoBackupError> {
        // 先判"落在数据目录内"再创建：否则会先在数据目录里造出一个空目录再去拒绝，
        // 留下垃圾。
        if let (Ok(canon_dir_parent), Ok(canon_data)) = (
            canonicalize_for_guard(&dir),
            data_dir.canonicalize(),
        ) {
            if canon_dir_parent.starts_with(&canon_data) {
                return Err(AutoBackupError::DirInsideDataDir {
                    dir,
                    data_dir: data_dir.to_path_buf(),
                });
            }
        }
        std::fs::create_dir_all(&dir)?;
        let mut store = Self {
            dir,
            pinned: BTreeSet::new(),
            load_warnings: Vec::new(),
        };
        store.reload_pins()?;
        Ok(store)
    }

    /// 便捷入口：按数据目录算出目录并打开。
    pub fn open_for_data_dir(data_dir: &Path) -> Result<Self, AutoBackupError> {
        Self::open(auto_backup_dir(data_dir), data_dir)
    }

    fn pin_index_path(&self) -> PathBuf {
        self.dir.join(PIN_INDEX_NAME)
    }

    /// 从磁盘重建固定集合。
    ///
    /// 取**并集**：索引里有的 ∪ 文件名带 `-p` 的。方向是刻意选的——只会多保护，
    /// 永远不会因为索引损坏/丢失就把用户固定过的备份交给轮换删掉。
    fn reload_pins(&mut self) -> Result<(), AutoBackupError> {
        self.pinned.clear();
        self.load_warnings.clear();

        let mut from_names: BTreeSet<String> = BTreeSet::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(parsed) = parse_name(&name) {
                if parsed.pinned_token {
                    from_names.insert(name);
                }
            }
        }

        let idx_path = self.pin_index_path();
        let mut from_index: BTreeSet<String> = BTreeSet::new();
        if idx_path.is_file() {
            match std::fs::read(&idx_path) {
                Ok(bytes) => match serde_json::from_slice::<PinIndex>(&bytes) {
                    Ok(idx) => {
                        for name in idx.pinned {
                            if parse_name(&name).is_some() {
                                from_index.insert(name);
                            }
                        }
                    }
                    Err(e) => self.load_warnings.push(format!(
                        "固定状态索引无法解析（{}）；已按文件名里的固定标记恢复固定状态，未取消任何固定。",
                        e
                    )),
                },
                Err(e) => self.load_warnings.push(format!(
                    "固定状态索引无法读取（{}）；已按文件名里的固定标记恢复固定状态，未取消任何固定。",
                    e
                )),
            }
        }

        self.pinned = from_names.union(&from_index).cloned().collect();
        Ok(())
    }

    /// 把固定集合写回索引（原子替换，失败不影响既有备份文件）。
    fn persist_pins(&self) -> Result<(), AutoBackupError> {
        let payload = serde_json::to_vec_pretty(&PinIndex {
            version: 1,
            pinned: self.pinned.iter().cloned().collect(),
        })
        .map_err(|e| AutoBackupError::Io(format!("序列化固定状态失败：{}", e)))?;

        let target = self.pin_index_path();
        let tmp = self.dir.join(format!(".{}.tmp-{}", PIN_INDEX_NAME, std::process::id()));
        std::fs::write(&tmp, &payload)?;
        // rename 在同一文件系统内是原子的：索引要么是旧的、要么是完整的新内容，
        // 不会出现半截 JSON 被下次启动读成"全部未固定"。
        std::fs::rename(&tmp, &target)?;
        Ok(())
    }

    /// 列出目录里所有**本模块管理的**备份，按时间从新到旧。
    pub fn list(&self) -> Result<Vec<BackupEntry>, AutoBackupError> {
        let mut out: Vec<BackupEntry> = Vec::new();
        for dir_entry in std::fs::read_dir(&self.dir)? {
            let dir_entry = dir_entry?;
            if !dir_entry.file_type()?.is_file() {
                continue;
            }
            let name = dir_entry.file_name().to_string_lossy().to_string();
            let Some(parsed) = parse_name(&name) else {
                continue;
            };
            let meta = match dir_entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            // 文件名里的固定标记与索引取并集——与 reload_pins 同一口径，
            // 于是"索引坏了但名字里有标记"这一份在列表里也显示为已固定。
            let pinned = parsed.pinned_token || self.pinned.contains(&name);
            out.push(BackupEntry {
                archive_name: name.clone(),
                path: dir_entry.path().to_string_lossy().to_string(),
                origin: origin_key(parsed.origin).to_string(),
                created_at: to_rfc3339_local(parsed.stamp),
                created_at_ms: to_millis(parsed.stamp),
                created_at_local: parsed.stamp.format("%Y-%m-%d %H:%M:%S").to_string(),
                size_bytes: meta.len(),
                pinned,
                seq: parsed.seq,
            });
        }
        out.sort_by(|a, b| {
            b.created_at_ms
                .cmp(&a.created_at_ms)
                .then(b.seq.cmp(&a.seq))
                .then(b.archive_name.cmp(&a.archive_name))
        });
        Ok(out)
    }

    /// 当前已固定的条数（按磁盘上的并集口径）。
    pub fn pinned_names(&self) -> Vec<String> {
        self.pinned.iter().cloned().collect()
    }

    /// 解析一个备份的绝对路径，并确认它确实存在于目录内。
    pub fn resolve_existing(&self, archive_name: &str) -> Result<PathBuf, AutoBackupError> {
        validate_archive_name(archive_name)?;
        let path = self.dir.join(archive_name);
        if !path.is_file() {
            return Err(AutoBackupError::NotFound(archive_name.to_string()));
        }
        Ok(path)
    }

    /// 生成一份新的自动备份。
    ///
    /// 打包完全交给既有的 [`create_backup`]（它内部是 `VACUUM INTO` + 原子 rename，
    /// 并且自己带"不要写进数据目录"的护栏）。**本函数不复制任何打 zip 的代码。**
    pub fn create(
        &mut self,
        data_dir: &Path,
        origin: BackupOrigin,
        app_version: &str,
    ) -> Result<BackupEntry, AutoBackupError> {
        self.create_at(data_dir, origin, app_version, Local::now().naive_local())
    }

    /// [`Self::create`] 的可注入时刻版本（测试用真实时间会让"精确到秒"的排序不可控）。
    pub fn create_at(
        &mut self,
        data_dir: &Path,
        origin: BackupOrigin,
        app_version: &str,
        stamp: NaiveDateTime,
    ) -> Result<BackupEntry, AutoBackupError> {
        if !data_dir.is_dir() {
            return Err(AutoBackupError::Io(format!(
                "数据目录不存在：{}",
                data_dir.display()
            )));
        }
        std::fs::create_dir_all(&self.dir)?;

        // 同秒内已经有几条就在其后编号：这样"一分钟内点了三次立即备份"会得到 3 份而不是
        // 互相覆盖（也避免与既有文件的 rename 覆盖冲突）。
        let seq = self
            .list()?
            .iter()
            .map(|e| parse_name(&e.archive_name))
            .filter_map(|p| p)
            .filter(|p| p.stamp == stamp && p.origin_equals(origin))
            .map(|p| p.seq)
            .max()
            .unwrap_or(0)
            + 1;

        let archive_name = build_name(origin, stamp, seq, false);
        let output_path = self.dir.join(&archive_name);
        if output_path.exists() {
            return Err(AutoBackupError::Io(format!(
                "目标备份已存在，未覆盖：{}",
                output_path.display()
            )));
        }

        let report = create_backup(&BackupRequest {
            data_dir: data_dir.to_path_buf(),
            output_path: output_path.clone(),
            app_version: app_version.to_string(),
        })?;

        Ok(BackupEntry {
            archive_name,
            path: report.output_path,
            origin: origin_key(origin).to_string(),
            created_at: to_rfc3339_local(stamp),
            created_at_ms: to_millis(stamp),
            created_at_local: stamp.format("%Y-%m-%d %H:%M:%S").to_string(),
            size_bytes: std::fs::metadata(&output_path).map(|m| m.len()).unwrap_or(0),
            pinned: false,
            seq,
        })
    }

    /// 执行一次轮换：删掉超出 `max_keep` 的最老**未固定**备份。
    ///
    /// 永不删除固定项。若"剩下的全是固定的"导致删不够，**不静默吞掉**：返回的
    /// [`RotationOutcome::undelatable_excess`] 与 `warnings` 会把这件事说清楚。
    /// （正常情况下到不了这一步——固定数被 [`Self::set_pinned`] 卡在 `max_keep - 1`，
    /// 因此总有一个轮换位。这里仍如实处理，是因为用户可以把 `max_keep` 调小、
    /// 也可以手工往目录里放文件。）
    pub fn enforce_rotation(&mut self, max_keep: u32) -> Result<RotationOutcome, AutoBackupError> {
        let entries = self.list()?;
        let (doomed, deficit) = plan_rotation(&entries, max_keep);
        let mut outcome = RotationOutcome::default();

        for entry in doomed {
            let path = self.dir.join(&entry.archive_name);
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    self.pinned.remove(&entry.archive_name);
                    outcome.deleted.push(entry.archive_name);
                }
                Err(e) => outcome.warnings.push(format!(
                    "删除超量的旧备份失败（{}）：{}。会在下次轮换时重试。",
                    entry.archive_name, e
                )),
            }
        }
        if !outcome.deleted.is_empty() {
            self.persist_pins()?;
        }

        if deficit > 0 {
            outcome.undelatable_excess = deficit;
            outcome.warnings.push(format!(
                "还有 {} 份备份超出上限，但它们全部处于固定状态，未删除。请先取消其中一部分的固定，或调大最大留存份数。",
                deficit
            ));
        }
        Ok(outcome)
    }

    /// 固定 / 取消固定，并做 **A11** 校验：固定数不得超过 `最大留存份数 − 1`。
    ///
    /// 校验失败返回 [`AutoBackupError::PinnedLimitReached`]，其结构化载荷带
    /// `maxKeep / maxPinned / currentPinned` 三个数，前端可直接填进用户要求的那段提示。
    pub fn set_pinned(
        &mut self,
        archive_name: &str,
        pinned: bool,
        config: &AutoBackupConfig,
    ) -> Result<bool, AutoBackupError> {
        // 目录里必须真的有这一份：对不存在的名字改状态毫无意义，也会污染索引。
        let _ = self.resolve_existing(archive_name)?;

        let parsed = parse_name(archive_name)
            .ok_or_else(|| AutoBackupError::InvalidName(archive_name.to_string()))?;

        // 【口径必须与 `list()` 一致】当前是否已固定 = **文件名标记 ∪ 索引**。
        //
        // 只看索引会出一个真实缺陷：目录里出现一个"名字带 `-p`、索引里没有"的备份时
        // （用户手工把固定过的备份拷进来、或索引曾被删掉而 `open` 之后文件才出现），
        // `list()` 显示它是已固定，而这里以为还没固定 → 去改成一个**同名**文件 →
        // 撞上 rename 的"目标已存在"守卫并报错。用户的动作（点固定）于是失败，
        // 而它本应是一次幂等的空操作。
        let already = parsed.pinned_token || self.pinned.contains(archive_name);

        if pinned && !already {
            let max_pinned = config.max_pinned();
            let current_pinned = self.pinned.len() as u32;
            if current_pinned >= max_pinned {
                return Err(AutoBackupError::PinnedLimitReached(PinnedLimit {
                    max_keep: config.max_keep,
                    max_pinned,
                    current_pinned,
                }));
            }
        }

        if pinned == already {
            // 幂等返回。顺带把索引补齐（自愈）：文件名里有标记却没有索引条目，
            // 正是"索引损坏/被删"之后的现场，现在把它记回来，下次启动不用再靠文件名兜底。
            if pinned && !self.pinned.contains(archive_name) {
                self.pinned.insert(archive_name.to_string());
                self.persist_pins()?;
            }
            return Ok(already);
        }

        let new_name = build_name(parsed.origin, parsed.stamp, parsed.seq, pinned);
        let from = self.dir.join(archive_name);
        let to = self.dir.join(&new_name);
        if pinned && to.exists() {
            return Err(AutoBackupError::Io(format!(
                "目标文件名已被占用：{}",
                to.display()
            )));
        }

        // 改名与索引两处必须一致：任何一步失败都**回退已做成的一半**，
        // 否则会出现"文件名说有、索引说没有"的分叉。
        std::fs::rename(&from, &to)?;
        if pinned {
            self.pinned.insert(new_name.clone());
        } else {
            self.pinned.remove(archive_name);
        }
        if let Err(e) = self.persist_pins() {
            // 回退：把名字改回去、把内存集合改回去。
            let _ = std::fs::rename(&to, &from);
            if pinned {
                self.pinned.remove(&new_name);
            } else {
                self.pinned.insert(archive_name.to_string());
            }
            return Err(e);
        }
        Ok(pinned)
    }

    /// 删除一份备份（固定与否都可删——用户显式要求的动作，界面负责二次确认）。
    pub fn delete(&mut self, archive_name: &str) -> Result<(), AutoBackupError> {
        let path = self.resolve_existing(archive_name)?;
        std::fs::remove_file(&path)?;
        if self.pinned.remove(archive_name) {
            self.persist_pins()?;
        }
        Ok(())
    }
}

impl ParsedName {
    fn origin_equals(&self, other: BackupOrigin) -> bool {
        self.origin == other
    }
}

fn origin_key(origin: BackupOrigin) -> &'static str {
    match origin {
        BackupOrigin::Scheduled => "scheduled",
        BackupOrigin::Startup => "startup",
        BackupOrigin::Manual => "manual",
    }
}

/// 把本地朴素时间转成带本地时区偏移的 RFC3339（秒级）。
fn to_rfc3339_local(stamp: NaiveDateTime) -> String {
    match Local.from_local_datetime(&stamp).single() {
        Some(dt) => dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        // 夏令时切换造成的"这个本地时刻不存在/不唯一"：退回不带偏移的写法，
        // 保证排序键始终可解析而不是变成空串。
        None => stamp.format("%Y-%m-%dT%H:%M:%S").to_string(),
    }
}

/// 把本地朴素时间转成本地时区的毫秒时间戳（排序与调度用）。
fn to_millis(stamp: NaiveDateTime) -> i64 {
    match Local.from_local_datetime(&stamp).single() {
        Some(dt) => dt.timestamp_millis(),
        None => stamp.and_utc().timestamp_millis(),
    }
}

/// 对"可能尚不存在"的路径做规范化（规范化存在的父级再拼回文件名）。
fn canonicalize_for_guard(p: &Path) -> Result<PathBuf, std::io::Error> {
    if let Ok(c) = p.canonicalize() {
        return Ok(c);
    }
    let parent = p.parent().unwrap_or_else(|| Path::new("."));
    let file = p.file_name().unwrap_or_default();
    Ok(parent.canonicalize()?.join(file))
}

/// 把 `DateTime<Local>` 转成文件名用的朴素时间（供调度层使用）。
pub fn naive_local(now: DateTime<Local>) -> NaiveDateTime {
    now.naive_local()
}
