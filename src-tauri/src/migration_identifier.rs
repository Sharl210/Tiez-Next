//! 应用标识符变更的数据目录迁移（`com.tiez` / `com.tiez.app` → `com.tieznext`）。
//!
//! ## 为什么需要它
//!
//! Tauri 的应用数据目录由 `tauri.conf.json` 的 `identifier` 推导。本项目从上游
//! TieZ 改名而来，`identifier` 由 `com.tiez.app`（Windows 主线 v0.3.1–v0.3.3）
//! 与 `com.tiez`（macOS/beta 分支）改为 `com.tieznext`。若不做处理，老用户升级后
//! 会看到一个空的新目录，历史剪贴板、标签、附件与表情收藏全部"消失"（实际仍在旧
//! 目录里，但应用不再读取）。
//!
//! 既有的 `perform_migration_v028`（`贴汁` → `TieZ`）只处理更早的一次改名，不覆盖
//! 标识符变更，因此单独实现本模块。
//!
//! ## 安全契约（硬约束，不得放宽）
//!
//! 用户明确要求「十分稳健，即使迁移失败也不会损失原数据」。据此：
//!
//! 1. **源目录全程只读**——不删除、不改名、不写入源内任何文件；
//! 2. **先暂存后交付**——先完整复制到独立暂存目录，校验一致后才提升为正式目录，
//!    避免半成品被当成有效数据；
//! 3. **失败即回滚**——任何一步失败都只清理暂存目录，源目录与既有目标目录保持不变；
//! 4. **目标已有数据则不迁移**——绝不覆盖既有用户数据（宁可少迁，不可覆盖）；
//! 5. **迁移成功后也不删除源目录**——保留为可回退副本，由用户自行清理。
//!
//! 第 1 与第 5 条共同保证：**本模块在最坏情况下只会"没迁成"，不会造成数据丢失**。
//!
//! ## 两条入口：自动候选 vs 用户手动指定
//!
//! - [`migrate_legacy_identifier_data`]：扫描白名单历史标识符目录（`com.tiez` /
//!   `com.tiez.app`）。**本函数不再由启动流程调用**——用户明确要求迁移必须由自己
//!   手动触发，应用启动不得自作主张搬数据。
//! - [`migrate_from_source_dir`]：迁移用户在界面上**手动选定的任意旧数据目录**。
//!   与前者共用同一套安全契约与交付实现（本模块只有 [`migrate_from`] 一条实现路径，
//!   两条入口不会漂移）。源路径由参数给出，因此必须做更严格的防御性检查：
//!   源不能等于目标、不能是目标已包含的目录、不能是既有目标的祖先。
//!
//! ## 依赖约束
//!
//! 本模块**只依赖 `std`**，不引用 crate 内任何其他模块。这样它可以脱离 Tauri 与
//! Windows 专用代码独立编译与测试（本 crate 在 Linux 上因 Windows 代码缺 cfg 门控
//! 而无法整体编译），从而使迁移逻辑能够被真实文件操作验证。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 历史标识符对应的目录名。
///
/// 只认这个白名单，不做前缀/模糊匹配——避免误伤同级的其他应用目录。
/// - `com.tiez.app`：Windows 主线 v0.3.1–v0.3.3
/// - `com.tiez`：macOS 与 beta 分支
pub const LEGACY_IDENTIFIERS: &[&str] = &["com.tiez.app", "com.tiez"];

/// 迁移是否成功由"新目录存在可用数据库"作为最终判据。
const DB_FILE: &str = "clipboard.db";

/// 一次迁移的结果。调用方据此决定是否继续做数据库内路径重写。
#[derive(Debug)]
pub enum MigrationOutcome {
    /// 无需迁移（无旧目录、目标已有数据、路径重合等）。
    Skipped(SkipReason),
    /// 迁移成功。`source` 仍然保留，未被删除。
    Migrated {
        source: PathBuf,
        target: PathBuf,
        /// 源侧条目总数（**含目录条目**，与内部一致性校验同一口径）。
        files: u64,
        /// 源侧全部条目的字节数之和。
        bytes: u64,
        /// 本次**新交付**的文件数（不含目录条目，也不含目标里已存在而未被覆盖的文件）。
        ///
        /// 界面展示"复制了多少"应该用这个数，而不是 `files`——后者含目录条目、也含
        /// 被沿用的条目，会让人高估实际交付量（复核 G-6）。
        delivered_files: u64,
        /// 本次**新交付**的字节数（与 `delivered_files` 同口径）。
        delivered_bytes: u64,
        /// 目标里**原本就存在、本次未覆盖**的文件数（按设计沿用目标版本）。
        kept_existing: u64,
        /// 目标里"从未使用过的空库"被改名让位后的路径（若有）。
        ///
        /// 只在目标原先只有空库、本次接管了它时出现；界面据此说明"新版原先的空数据
        /// 已留档"。恒为改名而非删除。
        yielded_db: Option<PathBuf>,
    },
    /// 迁移失败。源目录与既有目标目录均未被破坏。
    Failed { source: PathBuf, error: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// 新目录下已有数据库——用户已在用新版本，不能覆盖。
    TargetAlreadyHasData,
    /// 未找到任何旧目录。
    NoLegacyDir,
    /// 旧目录与新目录是同一个路径（防御性检查）。
    SamePath,
    /// 源路径根本不存在（移动硬盘未插、网络盘断开、路径打错）。
    ///
    /// 与 [`SkipReason::EmptySource`] 必须分开：两者给用户的诊断完全不同——"不存在"
    /// 要去检查盘/路径，"空的"说明路径对了但没数据。历史上两者被合并成一个原因码，
    /// 导致用户选到不存在的路径时看到「所选目录是空的」这种误导性提示（复核 G-3）。
    SourceMissing,
    /// 源路径存在但不是目录（例如用户手选到了一个文件）。
    NotADirectory,
    /// 源目录为空：没有可迁移的内容，不迁移也不删除。
    EmptySource,
    /// 源目录就是目标目录的祖先（迁移会把目标卷进源内，造成自复制）。
    SourceIsAncestorOfTarget,
    /// 源目录位于目标目录内部（迁移目标是自己的子目录，同样会造成自复制）。
    SourceInsideTarget,
}

impl SkipReason {
    /// 稳定的机器可读原因码，供界面按语言映射文案。
    pub fn code(&self) -> &'static str {
        match self {
            SkipReason::TargetAlreadyHasData => "target_already_has_data",
            SkipReason::NoLegacyDir => "no_legacy_dir",
            SkipReason::SamePath => "same_path",
            SkipReason::SourceMissing => "source_missing",
            SkipReason::NotADirectory => "not_a_directory",
            SkipReason::EmptySource => "empty_source",
            SkipReason::SourceIsAncestorOfTarget => "source_is_ancestor_of_target",
            SkipReason::SourceInsideTarget => "source_inside_target",
        }
    }
}

/// 由新数据目录推导同名父目录下的历史目录候选。
///
/// 例如新目录为 `%APPDATA%\com.tieznext`，则返回
/// `[%APPDATA%\com.tiez.app, %APPDATA%\com.tiez]`。
pub fn legacy_dirs_for(new_dir: &Path) -> Vec<PathBuf> {
    let Some(parent) = new_dir.parent() else {
        return Vec::new();
    };
    LEGACY_IDENTIFIERS
        .iter()
        .map(|id| parent.join(id))
        .collect()
}

/// 迁移入口：扫描历史目录并把数据安全地迁到 `new_dir`。
///
/// 本函数**不返回致命错误**：迁移失败只记录并返回 [`MigrationOutcome::Failed`]，
/// 绝不阻断应用启动。调用方应把"是否成功"仅用于决定后续的可选动作（如数据库内
/// 绝对路径重写）。
pub fn migrate_legacy_identifier_data(new_dir: &Path) -> MigrationOutcome {
    // 逐个候选尝试，第一个成功的即返回。若某个候选失败，继续尝试下一个候选
    // （不同候选对应不同平台的旧标识符，互不影响）。
    let mut last_failure: Option<MigrationOutcome> = None;

    for legacy in legacy_dirs_for(new_dir) {
        match migrate_from(&legacy, new_dir, false) {
            // 该候选路径不存在 → 继续看下一个候选（候选是白名单枚举出来的，
            // 本来就可能只有其中一个存在）。
            Outcome::Absent | Outcome::Preserve => {}
            Outcome::Skipped(r) => return MigrationOutcome::Skipped(r),
            Outcome::Migrated {
                files,
                bytes,
                delivered_files,
                delivered_bytes,
                kept_existing,
                ..
            } => {
                return MigrationOutcome::Migrated {
                    source: legacy,
                    target: new_dir.to_path_buf(),
                    files,
                    bytes,
                    delivered_files,
                    delivered_bytes,
                    kept_existing,
                    yielded_db: None,
                }
            }
            Outcome::Failed(error) => {
                last_failure = Some(MigrationOutcome::Failed {
                    source: legacy,
                    error,
                });
            }
        }
    }

    match last_failure {
        Some(f) => f,
        None => MigrationOutcome::Skipped(SkipReason::NoLegacyDir),
    }
}

/// 用户手动指定源目录的迁移入口。
///
/// 与 [`migrate_legacy_identifier_data`] 的唯一区别是**源路径来自用户**（界面上手选的
/// 任意旧数据目录），因此候选不再受白名单限制；其余安全契约（源只读、暂存→校验→
/// 提升、失败只清暂存、成功后不删源）完全一致——两条入口共用 [`migrate_from`] 这一条
/// 实现路径。
///
/// 【`allow_takeover`：为什么手动迁移需要它】新版应用**一启动就会在数据目录里创建
/// 空白 `clipboard.db`**。若沿用"目标有库就跳过"的保守语义，用户的手动迁移会被自己
/// 刚装好的空库永远挡住，功能形同虚设。因此手动入口额外接受一个由**调用方**（命令层）
/// 判定的开关：
///
/// - `allow_takeover = true`：调用方已确认目标那个库是"从未使用过的空库"（判定需要读
///   SQLite，由依赖 rusqlite 的命令层完成，以保持本模块只依赖 `std`）。此时本函数会
///   把该空库改名留档后接管目标目录——**改名，不是删除**，且仅当它确实空无一记录时才
///   会被允许走到这里。
/// - `allow_takeover = false`：只要目标根层存在 `clipboard.db` 就跳过，绝不覆盖。
///
/// 本函数**不返回致命错误**：失败只回报 [`MigrationOutcome::Failed`]，调用方据此提示
/// 用户"源目录完好、未做任何改动"。
pub fn migrate_from_source_dir(
    source: &Path,
    target: &Path,
    allow_takeover: bool,
) -> MigrationOutcome {
    migrate_from_inner(source, target, allow_takeover)
}

/// 迁移的完整入口（带"目标空库是否允许接管"开关）。
///
/// - `allow_pristine_target = true`：允许接管目标目录里那个**从未使用过的空库**
///   （改名留档后让位）。**只有用户手动发起的迁移可以走这条**——因为用户此刻就在
///   界面上，会看到"已接管空的新版数据目录"的提示，动作是可解释、可回退的。
/// - `allow_pristine_target = false`：只要目标根层存在数据库就跳过。启动期候选扫描
///   （[`migrate_legacy_identifier_data`]）走的是这条更保守的语义，保持其原有行为与
///   测试基线不变。
///
/// 真正的迁移实现在 [`migrate_from`]；本函数只是把内部三态 [`Outcome`] 转成公开的
/// [`MigrationOutcome`]。保留为独立公开入口，便于命令层按策略调用与测试。
pub fn migrate_from_source_dir_with_policy(
    source: &Path,
    target: &Path,
    allow_pristine_target: bool,
) -> MigrationOutcome {
    migrate_from_inner(source, target, allow_pristine_target)
}

fn migrate_from_inner(
    source: &Path,
    target: &Path,
    allow_pristine_target: bool,
) -> MigrationOutcome {
    match migrate_from(source, target, allow_pristine_target) {
        // 用户手选的路径不存在：必须与"目录为空"区分开，否则会告诉用户一个错误的
        // 诊断（移动硬盘没插 vs 目录里没数据，处置完全不同）。
        Outcome::Absent => MigrationOutcome::Skipped(SkipReason::SourceMissing),
        // 路径存在但为空目录：没有可迁移内容。
        Outcome::Preserve => MigrationOutcome::Skipped(SkipReason::EmptySource),
        Outcome::Skipped(r) => MigrationOutcome::Skipped(r),
        Outcome::Migrated {
            files,
            bytes,
            delivered_files,
            delivered_bytes,
            kept_existing,
            yielded_db,
        } => MigrationOutcome::Migrated {
            source: source.to_path_buf(),
            target: target.to_path_buf(),
            files,
            bytes,
            delivered_files,
            delivered_bytes,
            kept_existing,
            yielded_db,
        },
        Outcome::Failed(error) => MigrationOutcome::Failed {
            source: source.to_path_buf(),
            error,
        },
    }
}

/// 迁移在源目录上的**只读检查**：把源目录里会被迁移的文件逐个打开读取一遍。
///
/// 存在的理由：安全契约的第一条是"源目录全程只读"，而唯一可靠的证明方式是**实际
/// 读取源目录并比对迁移前后的指纹**。本函数提供程序化的读取证据（返回文件数与字节
/// 数），配合测试里的"调用前后源目录逐项扫描完全一致"断言，构成可重复的自证。
///
/// 只做 `File::open` + 读取，不写、不改名、不删除源内的任何东西。
pub fn check_source_is_readable(source: &Path) -> io::Result<(u64, u64)> {
    let entries = scan_tree(source)?;
    let mut files = 0u64;
    let mut bytes = 0u64;
    for (rel, size) in &entries {
        if rel.ends_with('/') {
            continue;
        }
        let path = source.join(rel);
        let mut file = fs::File::open(&path)?;
        let read = io::copy(&mut file, &mut io::sink())?;
        files += 1;
        bytes += read.min(*size);
    }
    Ok((files, bytes))
}

// ---------------------------------------------------------------------------
// 迁移中心：供 UI 展示旧目录占用，并在用户明确要求时备份后清理。
// ---------------------------------------------------------------------------

/// 一个历史数据目录的现状快照，供"迁移中心"展示。
#[derive(Debug, Clone)]
pub struct LegacyDirInfo {
    /// 目录绝对路径。
    pub path: PathBuf,
    /// 该目录对应的历史标识符（目录名）。
    pub identifier: String,
    /// 占用的总字节数。
    pub bytes: u64,
    /// 文件总数。
    pub files: u64,
    /// 是否包含主数据库（含则说明是真实数据目录，而非残留空壳）。
    pub has_database: bool,
    /// 是否与当前数据目录重合（重合时不得视为可清理的旧目录）。
    pub is_current: bool,
}

/// 列出当前存在的历史数据目录及其占用情况。
///
/// 只做只读统计，不做任何修改。`current_dir` 用于标记"与当前数据目录重合"的条目，
/// 避免 UI 把正在使用的目录误列为可清理对象。
pub fn list_legacy_dirs(current_dir: &Path) -> Vec<LegacyDirInfo> {
    let mut out = Vec::new();

    for legacy in legacy_dirs_for(current_dir) {
        if !legacy.is_dir() {
            continue;
        }
        let identifier = legacy
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        let (files, bytes) = match scan_tree(&legacy) {
            Ok(entries) => (
                entries.iter().filter(|(k, _)| !k.ends_with('/')).count() as u64,
                entries.iter().map(|(_, s)| *s).sum(),
            ),
            Err(_) => (0, 0),
        };

        out.push(LegacyDirInfo {
            has_database: legacy.join(DB_FILE).exists(),
            path: legacy,
            identifier,
            bytes,
            files,
            is_current: false, // legacy_dirs_for 只产出历史标识符，天然不等于当前目录
        });
    }

    out
}

/// 备份并删除一个历史数据目录。
///
/// 安全设计（与迁移同一套思路：宁可没做成，不可丢数据）：
/// 1. **先完整备份**到同级带时间戳的目录，备份校验通过后才删除原目录；
/// 2. 备份失败则**不删除**源目录，直接返回错误；
/// 3. 删除目标**必须在白名单标识符之内**——防止误传路径删掉用户的其他数据；
/// 4. 拒绝删除当前正在使用的数据目录。
///
/// 返回备份目录路径，便于 UI 告知用户"备份在哪"。
pub fn backup_and_remove_legacy_dir(
    current_dir: &Path,
    target: &Path,
) -> Result<PathBuf, String> {
    // ---- 安全校验：只允许删除白名单内的历史标识符目录 ----
    let allowed: Vec<PathBuf> = legacy_dirs_for(current_dir);
    if !allowed.iter().any(|p| p == target) {
        return Err(format!(
            "拒绝操作：{} 不在允许清理的历史数据目录白名单内",
            target.display()
        ));
    }
    if target == current_dir {
        return Err("拒绝操作：不能删除当前正在使用的数据目录".to_string());
    }
    if !target.is_dir() {
        return Err(format!("目录不存在或不是目录：{}", target.display()));
    }

    // 空目录直接删，无需备份（没有数据可保）。
    let entries = scan_tree(target).map_err(|e| format!("读取目录失败：{}", e))?;
    if entries.is_empty() {
        fs::remove_dir_all(target).map_err(|e| format!("删除空目录失败：{}", e))?;
        return Ok(PathBuf::new());
    }

    // ---- 第一步：备份到同级带时间戳的目录 ----
    let backup = backup_path_for(target);
    if backup.exists() {
        let _ = fs::remove_dir_all(&backup);
    }
    copy_tree(target, &backup).map_err(|e| format!("创建备份失败（未删除任何数据）：{}", e))?;

    // ---- 第二步：校验备份完整，不完整则不删源 ----
    let backup_entries = scan_tree(&backup).map_err(|e| format!("校验备份失败：{}", e))?;
    if backup_entries != entries {
        let _ = fs::remove_dir_all(&backup);
        return Err(format!(
            "备份内容与源不一致（源 {} 项 / 备份 {} 项），已放弃删除，源数据未改动",
            entries.len(),
            backup_entries.len()
        ));
    }

    // ---- 第三步：备份完好，才删除源目录 ----
    fs::remove_dir_all(target).map_err(|e| {
        format!(
            "备份已保存在 {}，但删除源目录失败：{}",
            backup.display(),
            e
        )
    })?;

    Ok(backup)
}

/// 备份目录路径：同名 + `.backup-<时间戳>`，与源目录同级。
fn backup_path_for(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "appdata".to_string());
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    parent.join(format!("{}.backup-{}", name, stamp))
}

/// 内部三态：无此候选 / 跳过 / 成功 / 失败。
enum Outcome {
    /// 该候选**路径不存在**。对候选扫描意味着"换下一个候选"；对用户手选路径意味着
    /// 要回报 [`SkipReason::SourceMissing`]（而不是"目录为空"）。
    Absent,
    /// 该候选存在但无需处理（空目录）。同样继续试下一个候选。
    Preserve,
    Skipped(SkipReason),
    Migrated {
        files: u64,
        bytes: u64,
        delivered_files: u64,
        delivered_bytes: u64,
        kept_existing: u64,
        /// 目标里那个"从未使用过的空库"被改名让位后的路径（若有）。
        yielded_db: Option<PathBuf>,
    },
    Failed(String),
}

/// 把目标目录里**从未使用过的空库**改名让位，好让真正的旧数据进来。
///
/// 【为什么需要它】新版应用一启动就会创建空白 `clipboard.db`（WAL 模式，还会带
/// `-wal` / `-shm`）。用户随后去点「迁移数据」时，目标里已经躺着一个空库；直接
/// 覆盖它虽然内容上无损（一条记录都没有），但改名留档比删除更符合本模块"什么都
/// 不销毁"的一贯风格——用户事后仍能在目标目录里看到那个被让位的文件。
///
/// **只在调用方已确认那个库从未被使用过（0 条记录）时才会被调用**，因此绝不会丢弃
/// 任何用户记录。判定需要读 SQLite，由依赖 rusqlite 的命令层完成（见
/// `system_cmd::target_db_is_pristine`），本模块据此保持只依赖 `std`。
///
/// 返回被改名的文件列表；调用方在交付失败时据此还原。
fn yield_target_db(source: &Path, target: &Path) -> Result<Vec<PathBuf>, String> {
    let mut yielded = Vec::new();

    // 幂等细节：若目标里那个"空库"（连同 WAL 侧车）与源里的同名文件**内容完全一致**
    // （典型场景是用户迁移成功后重启，应用又创建/打开了一模一样的库），就没有必要改名
    // 留档——否则用户反复验证时会看到一堆 `.unused-<时间戳>` 文件堆积。此时源文件覆盖
    // 它即可：两者本来就一模一样，覆盖不等于改变任何用户数据。
    // 注意这仍然只发生在目标那个库已被命令层判定为"0 条记录"的前提下。
    let same_as_source = |name: &str| -> bool {
        let a = target.join(name);
        let b = source.join(name);
        if !a.is_file() || !b.is_file() {
            return false;
        }
        match (fs::read(&a), fs::read(&b)) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    };

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // WAL 模式下 -wal / -shm 是数据库状态的一部分，必须一起让位，否则新库会读到
    // 上一份空库留下的 WAL 内容。
    for suffix in ["", "-wal", "-shm"] {
        let name = format!("{}{}", DB_FILE, suffix);
        let from = target.join(&name);
        if !from.is_file() {
            continue;
        }
        // 与源同名文件内容一致 → 无需留档，交给交付步骤按原样覆盖。
        if same_as_source(&name) {
            let _ = fs::remove_file(&from);
            continue;
        }
        let to = target.join(format!("{}.unused-{}", name, stamp));
        fs::rename(&from, &to).map_err(|e| {
            format!(
                "无法让位目标里未使用过的空库 {}：{}（源目录未改动）",
                from.display(),
                e
            )
        })?;
        yielded.push(to);
    }
    Ok(yielded)
}

/// 还原被 [`yield_target_db`] 改名让位的空库（仅用于交付失败回滚）。
fn restore_yielded_target_db(yielded: &[PathBuf], target: &Path) {
    for to in yielded {
        // 文件名形如 `clipboard.db.unused-<ts>`，去掉后缀即可还原。
        let Some(name) = to.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        let Some(original) = name.split(".unused-").next() else {
            continue;
        };
        let from = target.join(original);
        if !from.exists() {
            let _ = fs::rename(to, &from);
        }
    }
}

fn migrate_from(source: &Path, target: &Path, allow_pristine_target: bool) -> Outcome {
    // ---- 前置检查：任何一项不满足都保持原状 ----
    if !source.exists() {
        return Outcome::Absent;
    }
    if !source.is_dir() {
        return Outcome::Skipped(SkipReason::NotADirectory);
    }
    if source == target {
        return Outcome::Skipped(SkipReason::SamePath);
    }
    // 源是目标的祖先 / 源在目标内部：两者都会让"复制源到目标"变成自复制（目标可能
    // 位于源之内），必须先拒绝。用户手动选路径时可能误选成数据目录的上级目录。
    if target.starts_with(source) {
        return Outcome::Skipped(SkipReason::SourceIsAncestorOfTarget);
    }
    if source.starts_with(target) {
        return Outcome::Skipped(SkipReason::SourceInsideTarget);
    }
    // 目标已有数据库 -> 用户已经在用新版本，绝不覆盖。
    //
    // 例外：`allow_pristine_target` 为真时允许接管。该标志**只由命令层在确认目标那个
    // 库一条记录都没有之后才置位**（见 `migrate_from_source_dir` 的文档）；本模块不
    // 自行放宽这条判据，因为读 SQLite 需要 rusqlite，会破坏本模块只依赖 `std` 的契约。
    if target.join(DB_FILE).exists() && !allow_pristine_target {
        return Outcome::Skipped(SkipReason::TargetAlreadyHasData);
    }

    // ---- 统计源目录（只读）----
    let source_entries = match scan_tree(source) {
        Ok(v) => v,
        Err(e) => return Outcome::Failed(format!("读取源目录失败: {}", e)),
    };
    if source_entries.is_empty() {
        // 空目录没有迁移价值，也不删它。
        return Outcome::Preserve;
    }

    // ---- 记录"迁移前目标里就已存在"的条目 ----
    // 这些条目不属于本次交付范围：既不覆盖它们，交付后也不拿它们的
    // 大小去和源比对（用户可能已在其中写入了更新的内容）。
    // 注：目标里那个"空库"（及 -wal/-shm）也在其中，因此这两类状态文件既不会被
    // 源里的同名文件覆盖、也不参与大小比对——只有下一步真正让位后才会被替换。
    let preexisting: std::collections::HashSet<String> = if target.exists() {
        match scan_tree(target) {
            Ok(v) => v.into_iter().map(|(k, _)| k).collect(),
            Err(e) => return Outcome::Failed(format!("读取既有目标目录失败: {}", e)),
        }
    } else {
        std::collections::HashSet::new()
    };

    // ---- 第一步：复制到独立暂存目录 ----
    // 暂存目录放在目标同级，保证后续 rename 是同一文件系统内的原子操作。
    let staging = staging_dir(target);
    // 【为什么必须先查这一条】暂存目录名是 `.<目标名>.migrating.<pid>`，用户手选源路径
    // 时**完全可能恰好选中这个目录**（例如上次崩溃后残留的暂存目录，或用户自己起了
    // 同名目录）。而下面"清理上次崩溃残留的暂存目录"是无条件的 `remove_dir_all`——
    // 若不先拦住，源目录会在这一行被整个删掉，随后校验必然失败，用户还会收到一句
    // "源数据未改动"的错误信息。实测（本文件回归测试 `staging_never_deletes_the_source`
    // 抓出）：源 2 个文件 → 目录消失、文件全丢。
    //
    // 这是"源的每一条失败出路都必须保留源"这条核心保证的一部分，因此与 SamePath /
    // ancestor / inside 并列，放在任何写操作之前。
    if staging == source {
        return Outcome::Failed(format!(
            "源目录与本次迁移要使用的暂存目录同名（{}）。为避免覆盖你选择的源目录，已放弃本次迁移；源目录未被读取也未被改动。请改选其它目录。",
            staging.display()
        ));
    }
    // 清理上次崩溃残留的暂存目录（它从未被提升，删掉是安全的）。
    if staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    if let Err(e) = copy_tree(source, &staging) {
        let _ = fs::remove_dir_all(&staging);
        return Outcome::Failed(format!("复制到暂存目录失败: {}", e));
    }

    // ---- 第二步：校验一致后才允许交付 ----
    match scan_tree(&staging) {
        Ok(staged) => {
            if staged != source_entries {
                let _ = fs::remove_dir_all(&staging);
                return Outcome::Failed(format!(
                    "暂存副本与源不一致（源 {} 项 / 暂存 {} 项），已放弃本次迁移，源数据未改动",
                    source_entries.len(),
                    staged.len()
                ));
            }
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&staging);
            return Outcome::Failed(format!("校验暂存目录失败: {}", e));
        }
    }

    // ---- 第三步：交付（提升暂存目录到目标）----
    // 交付前先把"从未使用过的空库"改名让位（若适用）；让位失败则本步直接放弃，
    // 源目录与目标原状保持不变。
    let yielded = if allow_pristine_target {
        match yield_target_db(source, target) {
            Ok(v) => v,
            Err(e) => {
                let _ = fs::remove_dir_all(&staging);
                return Outcome::Failed(e);
            }
        }
    } else {
        Vec::new()
    };
    if let Err(e) = promote(&staging, target) {
        let _ = fs::remove_dir_all(&staging);
        // 交付没成：把刚才让位的空库还原回去，目标恢复原状（源本就未被改动）。
        restore_yielded_target_db(&yielded, target);
        return Outcome::Failed(format!("提升到目标目录失败: {}", e));
    }

    // ---- 第四步：交付后复核 ----
    // 判据分两类：
    //   * 本次新交付的项：必须存在且大小与源一致；
    //   * 迁移前目标里就已存在的项：只要求仍然存在，不比对大小
    //     （用户可能已在其中写入了比源更新的内容，覆盖它才是错的）。
    match verify_delivered(target, &source_entries, &preexisting, &yielded) {
        Ok(()) => {}
        Err(e) => {
            // 复核失败时不回删目标——目标里的数据是从源复制来的，删掉目标同样
            // 不合理；源目录始终未动，用户数据仍完整可回退。
            return Outcome::Failed(format!(
                "交付后复核未通过: {}（源目录保持完整，未删除任何数据）",
                e
            ));
        }
    }

    let bytes = source_entries.iter().map(|(_, s)| *s).sum();

    // 精确计数（复核 G-6）：把"本次真正交付的"与"目标里原本就有、按设计沿用的"分开。
    // 目录条目（`rel` 以 '/' 结尾）不计入文件数。
    let mut delivered_files = 0u64;
    let mut delivered_bytes = 0u64;
    let mut kept_existing = 0u64;
    for (rel, size) in &source_entries {
        if rel.ends_with('/') {
            continue;
        }
        if preexisting.contains(rel) {
            kept_existing += 1;
        } else {
            delivered_files += 1;
            delivered_bytes += *size;
        }
    }

    Outcome::Migrated {
        files: source_entries.len() as u64,
        bytes,
        delivered_files,
        delivered_bytes,
        kept_existing,
        yielded_db: yielded.first().cloned(),
    }
}

/// 暂存目录路径：目标同级，名字带 pid 以便并发/崩溃区分。
fn staging_dir(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "appdata".to_string());
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(".{}.migrating.{}", name, std::process::id()))
}

/// 把暂存目录交付到目标位置。
///
/// - 目标不存在：直接 `rename`（同一文件系统内为原子操作）。
/// - 目标已存在：**递归合并，逐文件判断**，已存在的文件一律保留不覆盖。
///
/// 注意必须递归进同名目录：目标里若已有 `attachments/` 目录，不能因为目录本身
/// 存在就跳过，否则该目录下源中独有的文件永远补不齐。
fn promote(staging: &Path, target: &Path) -> io::Result<()> {
    if !target.exists() {
        return fs::rename(staging, target);
    }
    merge_into(staging, target)?;
    let _ = fs::remove_dir_all(staging);
    Ok(())
}

/// 递归合并 `src` 到 `dst`，**绝不覆盖 `dst` 中已存在的文件**。
///
/// 优先用 `rename`（同文件系统内高效且原子）；跨设备失败时退回"复制 + 删除副本"，
/// 其中删除的始终是暂存侧副本，`src` 原始数据不受影响。
fn merge_into(src: &Path, dst: &Path) -> io::Result<()> {
    if !dst.exists() {
        fs::create_dir_all(dst)?;
    }
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ty = entry.file_type()?;

        if ty.is_dir() {
            if to.exists() {
                // 目录已存在：递归进去继续补齐，而不是整目录跳过
                merge_into(&from, &to)?;
            } else {
                fs::rename(&from, &to).or_else(|_| {
                    copy_tree(&from, &to)?;
                    fs::remove_dir_all(&from)
                })?;
            }
        } else if ty.is_file() {
            if to.exists() {
                continue; // 绝不覆盖既有文件
            }
            fs::rename(&from, &to).or_else(|_| {
                fs::copy(&from, &to)?;
                fs::remove_file(&from)
            })?;
        }
        // 符号链接等特殊类型一律跳过：不跟随、不复制。
    }
    Ok(())
}

/// 递归复制。只读源，只写目标。
fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_tree(&from, &to)?;
        } else if ty.is_file() {
            fs::copy(&from, &to)?;
        }
        // 符号链接等特殊类型一律跳过：不跟随、不复制，避免把外部路径卷进来。
    }
    Ok(())
}

/// 扫描目录树，返回按相对路径排序的 `(相对路径, 字节数)` 列表。
///
/// 用于迁移前后的**内容一致性**判定：条数与每项大小都相同才认为一致。
fn scan_tree(root: &Path) -> io::Result<Vec<(String, u64)>> {
    let mut out = Vec::new();
    collect(root, root, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, u64)>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/"); // 统一分隔符，便于跨平台比较
        if ty.is_dir() {
            out.push((format!("{}/", rel), 0));
            collect(root, &path, out)?;
        } else if ty.is_file() {
            out.push((rel, entry.metadata().map(|m| m.len()).unwrap_or(0)));
        }
    }
    Ok(())
}

/// 交付后复核。
///
/// - `expected`：源目录的全部条目（相对路径 + 大小）。
/// - `preexisting`：迁移前目标里就已存在的条目集合；这些只验存在性，不验大小。
fn verify_delivered(
    target: &Path,
    expected: &[(String, u64)],
    preexisting: &std::collections::HashSet<String>,
    yielded: &[PathBuf],
) -> Result<(), String> {
    let actual: Vec<(String, u64)> = scan_tree(target).map_err(|e| e.to_string())?;
    let map: std::collections::HashMap<&str, u64> =
        actual.iter().map(|(k, v)| (k.as_str(), *v)).collect();

    // 交付后只做检查，不删除目标里的任何文件。被改名让位的空库（
    // `clipboard.db.unused-<ts>` 等）属于本次操作的产物，不在源目录条目清单里，
    // 因此天然不会被下面的比对命中，无需特殊处理。
    let _ = yielded;

    let mut missing = Vec::new();
    let mut mismatched = Vec::new();
    for (rel, size) in expected {
        match map.get(rel.as_str()) {
            None => missing.push(rel.clone()),
            Some(actual_size) => {
                // 预先存在且未被本次覆盖的条目，允许与源大小不同
                if actual_size != size && !preexisting.contains(rel) {
                    mismatched.push(format!("{} (源 {} / 目标 {})", rel, size, actual_size));
                }
            }
        }
    }

    if missing.is_empty() && mismatched.is_empty() {
        return Ok(());
    }
    Err(format!(
        "缺失 {} 项{:?}，大小不符 {} 项{:?}",
        missing.len(),
        missing.iter().take(5).collect::<Vec<_>>(),
        mismatched.len(),
        mismatched.iter().take(5).collect::<Vec<_>>()
    ))
}

// ---------------------------------------------------------------------------
// 测试：使用真实文件系统操作，覆盖成功路径与各类失败路径。
// 这些测试只依赖 std，可在独立 harness 中运行（见 README/提交说明）。
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tiez-mig-test-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 造一个带真实 SQLite 文件头的空库文件（**只依赖 std**）。
    ///
    /// 本模块按设计不依赖 rusqlite，因此这里写的是 SQLite 的固定 100 字节文件头：
    /// 迁移逻辑只按"文件是否存在 / 内容是否一致"判定，不需要能真正打开它。
    fn make_empty_db_file(path: &Path) {
        let mut header = Vec::with_capacity(4096);
        header.extend_from_slice(b"SQLite format 3\0");
        header.extend_from_slice(&[0u8; 92]); // 头部其余字段
        header.resize(4096, 0u8); // 一个空页
        fs::write(path, &header).unwrap();
    }

    /// 造一个典型的旧数据目录：数据库 + WAL + 日志 + 附件 + 表情收藏 + 重定向文件。
    fn seed_legacy(dir: &Path) {
        fs::create_dir_all(dir.join("attachments")).unwrap();
        fs::create_dir_all(dir.join("emoji_favorites")).unwrap();
        fs::write(dir.join(DB_FILE), vec![b'x'; 4096]).unwrap();
        fs::write(dir.join("clipboard.db-wal"), vec![b'w'; 512]).unwrap();
        fs::write(dir.join("clipboard.db-shm"), vec![b's'; 128]).unwrap();
        fs::write(dir.join("tiez.log"), b"log line\n").unwrap();
        fs::write(dir.join("datapath.txt"), b"").unwrap();
        fs::write(dir.join("attachments/a.png"), vec![b'a'; 1000]).unwrap();
        fs::write(dir.join("emoji_favorites/e.json"), b"[]").unwrap();
    }

    #[test]
    fn migrates_all_files_and_keeps_source() {
        let root = tmp("ok");
        let target = root.join("com.tieznext");
        let legacy = root.join("com.tiez.app");
        seed_legacy(&legacy);
        let before = scan_tree(&legacy).unwrap();

        let outcome = migrate_legacy_identifier_data(&target);

        match outcome {
            MigrationOutcome::Migrated { files, .. } => assert_eq!(files, before.len() as u64),
            other => panic!("应迁移成功，实际 {:?}", other),
        }
        // 目标内容与源逐项一致
        assert_eq!(scan_tree(&target).unwrap(), before);
        // 源目录必须仍然完好（安全契约第 5 条）
        assert!(legacy.exists() && legacy.join(DB_FILE).exists());
        assert_eq!(scan_tree(&legacy).unwrap(), before);
        // 暂存目录不残留
        assert!(!staging_dir(&target).exists());
    }

    #[test]
    fn skips_when_target_already_has_data() {
        let root = tmp("skip-data");
        let target = root.join("com.tieznext");
        let legacy = root.join("com.tiez.app");
        seed_legacy(&legacy);
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join(DB_FILE), b"existing-user-data").unwrap();
        let target_before = scan_tree(&target).unwrap();

        let outcome = migrate_legacy_identifier_data(&target);

        assert!(matches!(
            outcome,
            MigrationOutcome::Skipped(SkipReason::TargetAlreadyHasData)
        ));
        // 既有目标数据一字未改
        assert_eq!(scan_tree(&target).unwrap(), target_before);
        // 源数据也一字未改
        assert!(legacy.join(DB_FILE).exists());
    }

    #[test]
    fn skips_when_no_legacy_dir() {
        let root = tmp("none");
        let target = root.join("com.tieznext");
        let outcome = migrate_legacy_identifier_data(&target);
        assert!(matches!(
            outcome,
            MigrationOutcome::Skipped(SkipReason::NoLegacyDir)
        ));
        assert!(!target.exists());
    }

    #[test]
    fn is_idempotent_and_never_overwrites() {
        let root = tmp("idem");
        let target = root.join("com.tieznext");
        let legacy = root.join("com.tiez.app");
        seed_legacy(&legacy);

        assert!(matches!(
            migrate_legacy_identifier_data(&target),
            MigrationOutcome::Migrated { .. }
        ));
        let after_first = scan_tree(&target).unwrap();
        // 用户在新版本里继续写入
        fs::write(target.join(DB_FILE), b"newer-user-data-longer").unwrap();

        // 第二次运行：目标已有数据库 -> 跳过，绝不覆盖
        let outcome = migrate_legacy_identifier_data(&target);
        assert!(matches!(
            outcome,
            MigrationOutcome::Skipped(SkipReason::TargetAlreadyHasData)
        ));
        assert_eq!(
            fs::read(target.join(DB_FILE)).unwrap(),
            b"newer-user-data-longer"
        );
        // 源目录依旧完好
        assert_eq!(scan_tree(&legacy).unwrap().len(), after_first.len());
    }

    #[test]
    fn merges_into_existing_target_without_overwriting() {
        let root = tmp("merge");
        let target = root.join("com.tieznext");
        let legacy = root.join("com.tiez.app");
        seed_legacy(&legacy);
        // 目标已存在，但没有数据库（例如只有窗口状态之类的零散文件）
        fs::create_dir_all(target.join("attachments")).unwrap();
        fs::write(target.join("existing-keep.txt"), b"do-not-clobber").unwrap();
        fs::write(target.join("attachments/a.png"), b"TARGET-KEEPS-THIS").unwrap();

        let outcome = migrate_legacy_identifier_data(&target);

        assert!(matches!(outcome, MigrationOutcome::Migrated { .. }));
        // 目标原有的文件未被覆盖
        assert_eq!(
            fs::read(target.join("existing-keep.txt")).unwrap(),
            b"do-not-clobber"
        );
        assert_eq!(
            fs::read(target.join("attachments/a.png")).unwrap(),
            b"TARGET-KEEPS-THIS"
        );
        // 源中独有、目标缺失的项被补齐
        assert!(target.join(DB_FILE).exists());
        assert!(target.join("emoji_favorites/e.json").exists());
        // 源完好
        assert!(legacy.join(DB_FILE).exists());
    }

    #[test]
    fn picks_com_tiez_when_only_that_exists() {
        let root = tmp("alt");
        let target = root.join("com.tieznext");
        let legacy = root.join("com.tiez");
        seed_legacy(&legacy);

        let outcome = migrate_legacy_identifier_data(&target);

        match outcome {
            MigrationOutcome::Migrated { source, .. } => {
                assert_eq!(source.file_name().unwrap(), std::ffi::OsStr::new("com.tiez"))
            }
            other => panic!("应迁移成功，实际 {:?}", other),
        }
        assert!(target.join(DB_FILE).exists());
    }

    #[test]
    fn ignored_when_source_is_a_file() {
        let root = tmp("file");
        let target = root.join("com.tieznext");
        fs::write(root.join("com.tiez.app"), b"not a dir").unwrap();

        let outcome = migrate_legacy_identifier_data(&target);

        // 不是目录：明确回报 NotADirectory，且不会把该文件删掉
        assert!(matches!(
            outcome,
            MigrationOutcome::Skipped(SkipReason::NotADirectory)
        ));
        assert!(root.join("com.tiez.app").exists());
        assert!(!target.exists());
    }

    #[test]
    fn failed_verification_leaves_source_intact() {
        // 用一个不可写的目标父目录制造交付失败：源必须完好。
        let root = tmp("failverify");
        let legacy = root.join("com.tiez.app");
        seed_legacy(&legacy);
        let before = scan_tree(&legacy).unwrap();

        // 目标父路径是一个"文件"，无法创建目录 -> 交付必然失败
        let bogus_parent = root.join("blocked");
        fs::write(&bogus_parent, b"i am a file").unwrap();
        let target = bogus_parent.join("com.tieznext");

        let outcome = migrate_legacy_identifier_data(&target);

        assert!(matches!(
            outcome,
            MigrationOutcome::Failed { .. } | MigrationOutcome::Skipped(_)
        ));
        // 核心断言：源数据一字未改
        assert_eq!(scan_tree(&legacy).unwrap(), before);
    }

    /// 回归测试：目标里已有同名子目录时，必须递归进该目录补齐源中独有的文件。
    /// 此前的实现"目录已存在就整目录跳过"，会导致这些文件永远补不齐。
    #[test]
    fn merges_into_preexisting_subdir_without_skipping_it() {
        let root = tmp("subdir");
        let target = root.join("com.tieznext");
        let legacy = root.join("com.tiez.app");
        seed_legacy(&legacy);

        // 目标里已有一个同名子目录，但里面只有别的文件
        fs::create_dir_all(target.join("attachments")).unwrap();
        fs::write(target.join("attachments/other.png"), b"preexisting").unwrap();

        let outcome = migrate_legacy_identifier_data(&target);

        assert!(matches!(outcome, MigrationOutcome::Migrated { .. }));
        // 源中独有的 attachments/a.png 必须被补齐进已存在的子目录
        assert!(
            target.join("attachments/a.png").exists(),
            "同名子目录内的缺失文件必须被递归补齐"
        );
        // 目标原有的文件保留
        assert_eq!(
            fs::read(target.join("attachments/other.png")).unwrap(),
            b"preexisting"
        );
        // 源完好
        assert!(legacy.join("attachments/a.png").exists());
    }

    // ---- 迁移中心：列表统计与备份删除 ----

    #[test]
    fn lists_legacy_dirs_with_size_and_db_flag() {
        let root = tmp("list");
        let current = root.join("com.tieznext");
        seed_legacy(&root.join("com.tiez.app"));

        let list = list_legacy_dirs(&current);

        assert_eq!(list.len(), 1);
        let info = &list[0];
        assert_eq!(info.identifier, "com.tiez.app");
        assert!(info.has_database);
        // seed_legacy 造 7 个文件：db, db-wal, db-shm, tiez.log, datapath.txt,
        // attachments/a.png, emoji_favorites/e.json（目录不计入 files）
        assert_eq!(info.files, 7);
        assert!(info.bytes > 0);
    }

    #[test]
    fn list_excludes_nonexistent_and_marks_current_dir() {
        let root = tmp("list2");
        let current = root.join("com.tieznext");
        fs::create_dir_all(&current).unwrap();

        // 两个历史目录都不存在
        assert!(list_legacy_dirs(&current).is_empty());
        // 当前目录本身不会被列为可清理项
        let list = list_legacy_dirs(&current);
        assert!(!list.iter().any(|i| i.path == current));
    }

    #[test]
    fn backup_then_remove_keeps_a_full_copy() {
        let root = tmp("del");
        let current = root.join("com.tieznext");
        let legacy = root.join("com.tiez.app");
        seed_legacy(&legacy);
        let before = scan_tree(&legacy).unwrap();

        let backup = backup_and_remove_legacy_dir(&current, &legacy).unwrap();

        // 源目录已删除
        assert!(!legacy.exists(), "源目录应已被删除");
        // 备份存在且内容完整
        assert!(backup.exists(), "备份必须存在");
        assert_eq!(scan_tree(&backup).unwrap(), before, "备份内容必须与源一致");
    }

    #[test]
    fn refuses_to_delete_paths_outside_whitelist() {
        let root = tmp("deny");
        let current = root.join("com.tieznext");
        // 这不是历史标识符目录，绝不允许删
        let victim = root.join("user-important-docs");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("thesis.docx"), b"irreplaceable").unwrap();

        let err = backup_and_remove_legacy_dir(&current, &victim).unwrap_err();

        assert!(err.contains("白名单"), "应因白名单拒绝，实际: {}", err);
        assert!(victim.join("thesis.docx").exists(), "用户数据必须完好");
    }

    #[test]
    fn refuses_to_delete_current_data_dir() {
        let root = tmp("denycur");
        // 构造一个"当前目录恰好是历史标识符"的场景（防御性）
        let current = root.join("com.tiez.app");
        seed_legacy(&current);

        let err = backup_and_remove_legacy_dir(&current, &current).unwrap_err();

        assert!(err.contains("当前"), "应拒绝删除当前目录，实际: {}", err);
        assert!(current.join(DB_FILE).exists(), "数据必须完好");
    }

    #[test]
    fn removes_empty_legacy_dir_without_backup() {
        let root = tmp("empty");
        let current = root.join("com.tieznext");
        let legacy = root.join("com.tiez.app");
        fs::create_dir_all(&legacy).unwrap(); // 空目录

        let backup = backup_and_remove_legacy_dir(&current, &legacy).unwrap();

        assert!(!legacy.exists());
        assert!(backup.as_os_str().is_empty(), "空目录无需备份");
    }

    #[test]
    fn delete_failure_leaves_backup_and_reports_path() {
        let root = tmp("delmiss");
        let current = root.join("com.tieznext");
        let legacy = root.join("com.tiez.app");
        seed_legacy(&legacy);

        // 正常删除应成功并留下备份
        let backup = backup_and_remove_legacy_dir(&current, &legacy).unwrap();
        assert!(backup.exists());
        // 再次删除同一目录应报"不存在"，且不误删备份
        let err = backup_and_remove_legacy_dir(&current, &legacy).unwrap_err();
        assert!(err.contains("不存在"), "实际: {}", err);
        assert!(backup.exists(), "备份不得被后续调用删除");
    }

    #[test]
    fn scan_tree_is_stable_and_uses_forward_slashes() {
        let root = tmp("scan");
        seed_legacy(&root);
        let a = scan_tree(&root).unwrap();
        let b = scan_tree(&root).unwrap();
        assert_eq!(a, b);
        assert!(a.iter().all(|(k, _)| !k.contains('\\')));
        // 目录项以 / 结尾
        assert!(a.iter().any(|(k, _)| k == "attachments/"));
    }

    // ---- 用户手动指定源目录（迁移中心）----

    /// 手动入口必须能迁任意路径的目录，并且**源目录逐项不变**（只读契约的核心）。
    #[test]
    fn manual_migration_accepts_arbitrary_source_and_keeps_it_byte_identical() {
        let root = tmp("manual");
        let target = root.join("com.tieznext");
        // 任意路径：既不是白名单标识符，也不在目标同级
        let source = root.join("my-old-tiez-data");
        seed_legacy(&source);
        let before = scan_tree(&source).unwrap();

        let outcome = migrate_from_source_dir(&source, &target, false);

        match &outcome {
            MigrationOutcome::Migrated { files, bytes, .. } => {
                assert_eq!(*files, before.len() as u64);
                assert!(*bytes > 0);
            }
            other => panic!("应迁移成功，实际 {:?}", other),
        }
        assert_eq!(scan_tree(&target).unwrap(), before, "目标必须与源逐项一致");
        // 核心断言：源目录逐项（含目录结构与大小）完全未变
        assert_eq!(scan_tree(&source).unwrap(), before, "源目录必须逐字节未变");
        assert!(!staging_dir(&target).exists(), "不得残留暂存目录");
    }

    /// 重复迁移同一源目录：第一次成功，之后每次都安全跳过；源与目标都不受影响。
    #[test]
    fn manual_migration_is_idempotent_across_repeats() {
        let root = tmp("manual-idem");
        let target = root.join("com.tieznext");
        let source = root.join("old-data");
        seed_legacy(&source);
        let source_before = scan_tree(&source).unwrap();

        assert!(matches!(
            migrate_from_source_dir(&source, &target, false),
            MigrationOutcome::Migrated { .. }
        ));
        let target_after_first = scan_tree(&target).unwrap();

        // 连跑多次：每一次都必须安全跳过，且不改变任何一侧
        for round in 0..3 {
            let outcome = migrate_from_source_dir(&source, &target, false);
            assert!(
                matches!(
                    outcome,
                    MigrationOutcome::Skipped(SkipReason::TargetAlreadyHasData)
                ),
                "第 {} 次重复迁移应跳过，实际 {:?}",
                round + 2,
                outcome
            );
            assert_eq!(
                scan_tree(&target).unwrap(),
                target_after_first,
                "第 {} 次重复迁移后目标必须一字未改",
                round + 2
            );
            assert_eq!(
                scan_tree(&source).unwrap(),
                source_before,
                "第 {} 次重复迁移后源必须一字未改",
                round + 2
            );
        }
    }

    /// 目标已有数据库但**缺附件**：手动迁移必须拒绝，且目标已有数据一字不改。
    ///
    /// 这是本设计下最关键的一条否决断言——它正是"合并模式"会产生坏结果的场景
    /// （附件被硬链接式搬走后源目录就不再完整，用户后续再迁移会数据错位）。
    #[test]
    fn manual_migration_refuses_when_target_has_db_and_target_stays_untouched() {
        let root = tmp("manual-refuse");
        let target = root.join("com.tieznext");
        let source = root.join("old-data");
        seed_legacy(&source);
        // 目标已有自己的数据库（模拟新版已经启动并建库），但没有 attachments
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join(DB_FILE), b"new-version-own-db").unwrap();
        let target_before = scan_tree(&target).unwrap();
        let source_before = scan_tree(&source).unwrap();

        let outcome = migrate_from_source_dir(&source, &target, false);

        assert!(matches!(
            outcome,
            MigrationOutcome::Skipped(SkipReason::TargetAlreadyHasData)
        ));
        assert_eq!(scan_tree(&target).unwrap(), target_before, "目标不得被改动");
        assert_eq!(scan_tree(&source).unwrap(), source_before, "源不得被改动");
    }

    /// 源是目标的祖先目录时必须拒绝：否则复制会把目标自身卷进源里（自复制）。
    #[test]
    fn manual_migration_refuses_ancestor_source() {
        let root = tmp("ancestor");
        let target = root.join("com.tieznext");
        seed_legacy(&root); // 源就是 root，target 在 root 之下

        let outcome = migrate_from_source_dir(&root, &target, false);

        assert!(matches!(
            outcome,
            MigrationOutcome::Skipped(SkipReason::SourceIsAncestorOfTarget)
        ));
        assert!(!target.exists(), "拒绝后不得创建目标");
        assert!(root.join(DB_FILE).exists(), "源必须完好");
    }

    /// 源位于目标内部时必须拒绝（同样会自复制）。
    #[test]
    fn manual_migration_refuses_source_inside_target() {
        let root = tmp("inside");
        let target = root.join("com.tieznext");
        let source = target.join("nested-old-data");
        seed_legacy(&source);

        let outcome = migrate_from_source_dir(&source, &target, false);

        assert!(matches!(
            outcome,
            MigrationOutcome::Skipped(SkipReason::SourceInsideTarget)
        ));
        assert!(source.join(DB_FILE).exists(), "源必须完好");
    }

    /// 空源目录：跳过，不迁移也不删除。
    #[test]
    fn manual_migration_skips_empty_source_without_deleting_it() {
        let root = tmp("manual-empty");
        let target = root.join("com.tieznext");
        let source = root.join("empty-old-data");
        fs::create_dir_all(&source).unwrap();

        let outcome = migrate_from_source_dir(&source, &target, false);

        assert!(matches!(
            outcome,
            MigrationOutcome::Skipped(SkipReason::EmptySource)
        ));
        assert!(source.exists(), "空源目录也不得被删除");
        assert!(!target.exists());
    }

    /// 指定路径本身是个文件：不迁移、不破坏，且该文件仍然存在。
    #[test]
    fn manual_migration_rejects_file_path_without_touching_it() {
        let root = tmp("manual-file");
        let target = root.join("com.tieznext");
        let source = root.join("not-a-dir.txt");
        fs::write(&source, b"user file").unwrap();

        let outcome = migrate_from_source_dir(&source, &target, false);

        assert!(
            matches!(outcome, MigrationOutcome::Skipped(SkipReason::NotADirectory)),
            "选到文件应回报 NotADirectory，实际 {:?}",
            outcome
        );
        assert!(source.exists(), "用户选错的文件不得被删除");
        assert_eq!(fs::read(&source).unwrap(), b"user file");
        assert!(!target.exists());
    }

    /// 交付必然失败时（目标父路径是文件），源目录仍逐项不变。
    #[test]
    fn manual_migration_failure_keeps_source_intact() {
        let root = tmp("manual-fail");
        let source = root.join("old-data");
        seed_legacy(&source);
        let before = scan_tree(&source).unwrap();

        let blocked = root.join("blocked");
        fs::write(&blocked, b"i am a file").unwrap();
        let target = blocked.join("com.tieznext");

        let outcome = migrate_from_source_dir(&source, &target, false);

        assert!(matches!(outcome, MigrationOutcome::Failed { .. }));
        assert_eq!(scan_tree(&source).unwrap(), before, "失败时源必须一字未改");
    }

    /// `allow_takeover = true` 时：目标里那个"空库"被改名留档，源数据接管目标。
    ///
    /// 这是本设计的关键行为——应用一启动就会建空库，不做这个区分的话手动迁移会被
    /// 用户自己刚装好的空库永远挡住。真实调用里 `allow_takeover` 由命令层在确认
    /// 目标库 0 条记录之后才置位（见 `system_cmd::target_db_is_pristine`）。
    #[test]
    fn manual_migration_takes_over_target_db_when_allowed() {
        let root = tmp("takeover");
        let target = root.join("com.tieznext");
        let source = root.join("old-data");
        seed_legacy(&source);
        let source_before = scan_tree(&source).unwrap();

        // 目标 = 新版首次启动后的样子：只有空库 + WAL 侧车
        fs::create_dir_all(&target).unwrap();
        make_empty_db_file(&target.join(DB_FILE));
        fs::write(target.join("clipboard.db-wal"), b"stale wal").unwrap();

        let outcome = migrate_from_source_dir(&source, &target, true);

        assert!(
            matches!(outcome, MigrationOutcome::Migrated { .. }),
            "允许接管时应迁移成功，实际 {:?}",
            outcome
        );
        // 目标数据库换成源里的那份
        assert_eq!(
            fs::read(target.join(DB_FILE)).unwrap(),
            fs::read(source.join(DB_FILE)).unwrap()
        );
        assert!(target.join("attachments/a.png").exists());
        // 原先的空库被**改名留档**，而不是删除
        let leftovers: Vec<String> = fs::read_dir(&target)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".unused-"))
            .collect();
        assert!(
            leftovers
                .iter()
                .any(|n| n.starts_with("clipboard.db.unused-")),
            "空库应被改名留档，实际残留: {:?}",
            leftovers
        );
        // 源目录逐项未变
        assert_eq!(scan_tree(&source).unwrap(), source_before);
    }

    /// `allow_takeover = false`（保守语义）时：目标存在数据库就跳过，两侧都不动。
    #[test]
    fn manual_migration_skips_existing_target_db_by_default() {
        let root = tmp("no-takeover");
        let target = root.join("com.tieznext");
        let source = root.join("old-data");
        seed_legacy(&source);
        fs::create_dir_all(&target).unwrap();
        make_empty_db_file(&target.join(DB_FILE));
        let target_before = scan_tree(&target).unwrap();
        let source_before = scan_tree(&source).unwrap();

        let outcome = migrate_from_source_dir(&source, &target, false);

        assert!(matches!(
            outcome,
            MigrationOutcome::Skipped(SkipReason::TargetAlreadyHasData)
        ));
        assert_eq!(scan_tree(&target).unwrap(), target_before, "目标不得被改动");
        assert_eq!(scan_tree(&source).unwrap(), source_before, "源不得被改动");
    }

    /// 反复接管必须幂等：目标里那个库已经与源一模一样时，不再堆积 `.unused-` 留档。
    #[test]
    fn repeated_takeover_does_not_pile_up_archives() {
        let root = tmp("takeover-twice");
        let target = root.join("com.tieznext");
        let source = root.join("old-data");
        seed_legacy(&source);
        let source_before = scan_tree(&source).unwrap();

        fs::create_dir_all(&target).unwrap();
        make_empty_db_file(&target.join(DB_FILE));
        assert!(matches!(
            migrate_from_source_dir(&source, &target, true),
            MigrationOutcome::Migrated { .. }
        ));

        let count_archives = |dir: &Path| -> usize {
            fs::read_dir(dir)
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().contains(".unused-"))
                .count()
        };
        let after_first = count_archives(&target);

        // 再跑两次：目标库此时与源一致，应直接覆盖而不再留档
        for _ in 0..2 {
            assert!(matches!(
                migrate_from_source_dir(&source, &target, true),
                MigrationOutcome::Migrated { .. }
            ));
        }

        assert_eq!(
            count_archives(&target),
            after_first,
            "重复迁移不得持续堆积 .unused- 留档文件"
        );
        // 内容仍然正确、源仍然一字未改
        assert_eq!(
            fs::read(target.join(DB_FILE)).unwrap(),
            fs::read(source.join(DB_FILE)).unwrap()
        );
        assert_eq!(scan_tree(&source).unwrap(), source_before);
    }

    /// **最高风险路径回归**：目标里的数据库无论大小、无论是否像是"空库"，
    /// 只要 `allow_takeover = false`，就绝不能被丢弃或改名。
    ///
    /// 这条断言直接守护最坏结果——把用户真实数据当成空壳让位。`allow_takeover` 由命令层
    /// 依据一次只读查询决定；本模块必须保证"调用方说不行就一定不行"。
    #[test]
    fn takeover_never_discards_a_target_database_unless_explicitly_allowed() {
        let root = tmp("never-discard");
        let source = root.join("old-data");
        seed_legacy(&source);

        // 目标库做得"看起来很空"：有真实 SQLite 头、0 条记录也是 4096 字节
        for (label, takeover) in [("保守（allow=false）", false), ("允许接管", true)] {
            let target = root.join(format!("tgt-{}", takeover));
            fs::create_dir_all(&target).unwrap();
            make_empty_db_file(&target.join(DB_FILE));
            let target_db_before = fs::read(target.join(DB_FILE)).unwrap();

            let _ = migrate_from_source_dir(&source, &target, takeover);

            if takeover {
                // 允许接管：旧文件被改名留档（内容仍可找回），而非消失
                let archived: Vec<PathBuf> = fs::read_dir(&target)
                    .unwrap()
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.to_string_lossy().contains(".unused-"))
                    .collect();
                assert!(
                    !archived.is_empty(),
                    "{}：接管时目标原库必须被改名留档",
                    label
                );
                assert!(
                    archived
                        .iter()
                        .any(|p| fs::read(p).unwrap() == target_db_before),
                    "{}：留档文件必须与目标原库逐字节一致（内容未丢失）",
                    label
                );
            } else {
                // 保守：目标库一字未动，且没有被改名
                assert_eq!(
                    fs::read(target.join(DB_FILE)).unwrap(),
                    target_db_before,
                    "{}：目标库必须一字未改",
                    label
                );
                let any_archive = fs::read_dir(&target)
                    .unwrap()
                    .flatten()
                    .any(|e| e.file_name().to_string_lossy().contains(".unused-"));
                assert!(!any_archive, "{}：不得产生任何留档或改名", label);
            }
        }
    }

    /// **数据丢失回归（复核 G-1）**：源路径恰好等于本次迁移要用的暂存路径时，
    /// 源目录绝不能被"清理残留暂存目录"那一步删掉。
    ///
    /// 历史实现：`staging_dir(target)` 先算出 `.<目标名>.migrating.<pid>`，随后在
    /// `staging.exists()` 时**无条件** `remove_dir_all`。用户若手选到同名目录（例如上次
    /// 崩溃残留的暂存目录），源会被整个删除，而错误信息还写着"源数据未改动"。实测：
    /// 源 2 个文件 → 结果 Failed，但源目录已不存在、文件全丢。
    #[test]
    fn staging_never_deletes_the_source() {
        let root = tmp("staging-collision");
        let target = root.join("com.tieznext");
        // 源目录名 == staging_dir(target) 的名字
        let source = root.join(format!(
            ".com.tieznext.migrating.{}",
            std::process::id()
        ));
        seed_legacy(&source);
        let before = scan_tree(&source).unwrap();
        assert_eq!(staging_dir(&target), source, "测试前提：两者路径必须相同");

        let outcome = migrate_from_source_dir(&source, &target, true);

        // 核心断言：源目录及其全部内容必须完好
        assert!(source.exists(), "源目录绝不能被删除");
        assert_eq!(
            scan_tree(&source).unwrap(),
            before,
            "源目录内容必须逐项未变"
        );
        assert!(source.join(DB_FILE).exists(), "源数据库必须还在");
        // 且必须明确失败并说明原因，不得谎称"源数据未改动"后其实删了它
        match outcome {
            MigrationOutcome::Failed { error, .. } => {
                assert!(
                    error.contains("同名"),
                    "错误信息应说明是暂存目录同名，实际: {}",
                    error
                );
            }
            other => panic!("应明确失败，实际 {:?}", other),
        }
    }

    /// **诊断准确性回归（复核 G-3）**：源路径**不存在**时必须回报 `SourceMissing`，
    /// 不能与"目录为空"混为一谈——前者要用户检查盘/路径，后者说明路径对了但没数据。
    #[test]
    fn missing_source_reports_source_missing_not_empty_source() {
        let root = tmp("missing-src");
        let target = root.join("com.tieznext");
        let source = root.join("D-does-not-exist");

        let outcome = migrate_from_source_dir(&source, &target, true);

        assert_eq!(
            outcome_as_reason(&outcome),
            Some(SkipReason::SourceMissing),
            "不存在的路径应回报 SourceMissing，实际 {:?}",
            outcome
        );
        assert!(!target.exists());
    }

    /// 对照：源目录**存在但为空**时回报 `EmptySource`（两个原因码必须可区分）。
    #[test]
    fn existing_but_empty_source_reports_empty_source() {
        let root = tmp("empty-src");
        let target = root.join("com.tieznext");
        let source = root.join("present-but-empty");
        fs::create_dir_all(&source).unwrap();

        let outcome = migrate_from_source_dir(&source, &target, true);

        assert_eq!(
            outcome_as_reason(&outcome),
            Some(SkipReason::EmptySource),
            "空目录应回报 EmptySource，实际 {:?}",
            outcome
        );
        assert!(source.exists(), "空源目录也不得被删除");
    }

    /// 便捷取值：把 `Skipped` 的原因抽出来，便于断言。
    fn outcome_as_reason(o: &MigrationOutcome) -> Option<SkipReason> {
        match o {
            MigrationOutcome::Skipped(r) => Some(*r),
            _ => None,
        }
    }

    /// 只读检查必须能读完源目录全部文件，且读完不改动源。
    #[test]
    fn read_only_check_reads_every_file_and_changes_nothing() {
        let root = tmp("readonly");
        let source = root.join("old-data");
        seed_legacy(&source);
        let before = scan_tree(&source).unwrap();

        let (files, bytes) = check_source_is_readable(&source).unwrap();

        assert_eq!(files, 7, "seed_legacy 造 7 个文件");
        assert!(bytes > 0);
        assert_eq!(scan_tree(&source).unwrap(), before, "只读检查不得改动源");
    }

    /// 跳过原因必须带稳定机器码，供界面按语言映射文案。
    #[test]
    fn skip_reason_codes_are_stable() {
        assert_eq!(
            SkipReason::TargetAlreadyHasData.code(),
            "target_already_has_data"
        );
        assert_eq!(SkipReason::NoLegacyDir.code(), "no_legacy_dir");
        assert_eq!(SkipReason::SamePath.code(), "same_path");
        assert_eq!(SkipReason::SourceMissing.code(), "source_missing");
        assert_eq!(SkipReason::NotADirectory.code(), "not_a_directory");
        assert_eq!(SkipReason::EmptySource.code(), "empty_source");
        assert_eq!(
            SkipReason::SourceIsAncestorOfTarget.code(),
            "source_is_ancestor_of_target"
        );
        assert_eq!(SkipReason::SourceInsideTarget.code(), "source_inside_target");
    }
}
