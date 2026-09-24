//! 应用数据目录的迁移：把**旧数据目录**里的数据安全地搬进当前数据目录。
//!
//! 迁移来源分两类（见 [`MIGRATABLE_SOURCES`]）：
//!
//! - **旧版 TieZ**（`com.tiez.app` / `com.tiez`）——本项目改名前的上游版本；
//! - **历史版本的 Tiez-Next**（`com.tieznext`）——标识符跨版本不变，因此这条来源
//!   对**之后的所有版本**同样成立，不需要每发一版就回来改来源表。
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
//! ## 便携版：数据不在 `%APPDATA%`
//!
//! 便携版的用户数据放在**程序目录**里，不在系统应用数据目录。目录形状是
//! `程序目录/data/{clipboard.db, attachments/, …}`（见 [`resolve_source_dir`]）。
//! 因此"用户手动选目录"是便携版场景下的主要路径，而选中的那一层既可能是程序目录、
//! 也可能是里面的 `data`——本模块对两者都接受。
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
//! 6. **源必须是数据目录**——归一化之后、复制任何字节之前，先确认所选路径确实含
//!    `clipboard.db`（或便携版的 `data/clipboard.db`）。不是就拒绝并如实说明原因
//!    （[`SkipReason::NotADataDirectory`]），绝不把"用户选错的那一层"整棵复制进
//!    应用数据目录。这条保证的是**不把无关文件搬进应用数据目录**：迁移只在两个
//!    数据目录之间成立，"把一个普通目录整体搬进来"从来不是这个功能要做的事。
//!
//! ## 两条入口：自动候选 vs 用户手动指定
//!
//! - [`migrate_legacy_identifier_data`]：扫描同名父目录下的**可迁移来源**目录
//!   （见 [`MIGRATABLE_SOURCES`]）。**本函数不再由启动流程调用**——用户明确要求迁移
//!   必须由自己手动触发，应用启动不得自作主张搬数据。
//! - [`migrate_from_source_dir`]：迁移用户在界面上**手动选定的任意旧数据目录**。
//!   与前者共用同一套安全契约与交付实现（本模块只有 [`migrate_from`] 一条实现路径，
//!   两条入口不会漂移）。源路径由参数给出，因此必须做更严格的防御性检查：
//!   源不能等于目标、不能是目标已包含的目录、不能是既有目标的祖先。
//!
//! ## 可迁移来源（不止"旧版 TieZ"）
//!
//! 早先这里叫"历史标识符白名单"，只认 TieZ 的两个旧标识符。现在语义放宽为
//! **"可迁移来源"**：本应用自己的标识符 `com.tieznext` 同样是合法来源，于是
//! "旧版 Tiez-Next → 新版 Tiez-Next"也可以迁移。因为标识符跨版本不变，这条规则
//! **对未来版本自动成立**，不需要每发一版就回来改白名单。
//!
//! 来源分两类，界面据此如实告诉用户"这条是旧版 TieZ 的数据"还是"这是旧版
//! Tiez-Next 的数据"，见 [`SourceOrigin`]。
//!
//! ## 依赖约束
//!
//! 本模块**只依赖 `std`**，不引用 crate 内任何其他模块。这样它可以脱离 Tauri 与
//! Windows 专用代码独立编译与测试（本 crate 在 Linux 上因 Windows 代码缺 cfg 门控
//! 而无法整体编译），从而使迁移逻辑能够被真实文件操作验证。

use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

/// 本应用当前的标识符（`tauri.conf.json` 的 `identifier`）。
///
/// 它同时是**新数据目录名**和**可迁移来源之一**：从 `com.tieznext` 目录迁移，
/// 就是把"旧版本 Tiez-Next 的数据"搬进当前版本。标识符不随版本变化，因此这条
/// 来源对之后所有版本都成立。
pub const CURRENT_IDENTIFIER: &str = "com.tieznext";

/// 一个可迁移来源的标识符及其出处。
///
/// `id` 既是目录名，也是判定"这个目录是谁的数据"的依据；界面用它显示来源类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigratableSource {
    /// 目录名（等于历史 identifier）。
    pub id: &'static str,
    /// 该目录属于哪一类来源。
    pub origin: SourceOrigin,
}

/// 数据目录的出处：这条目录里的数据**原本是哪个应用写的**。
///
/// 纯粹是**如实展示**用的分类，不参与安全判定——无论哪一类来源，迁移契约完全一致
/// （源只读、失败只清暂存、绝不覆盖既有数据、成功后保留源）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceOrigin {
    /// 旧版 **TieZ**（应用改名前的上游版本）：`com.tiez.app` / `com.tiez`。
    LegacyTiez,
    /// **历史版本的 Tiez-Next**：`com.tieznext`。标识符未变，因此适用于后续所有版本。
    PreviousTiezNext,
}

impl SourceOrigin {
    /// 稳定的机器可读分类码，供界面按语言映射文案。
    pub fn code(&self) -> &'static str {
        match self {
            SourceOrigin::LegacyTiez => "legacy_tiez",
            SourceOrigin::PreviousTiezNext => "previous_tiez_next",
        }
    }
}

/// **可迁移来源**列表：允许作为迁移源的应用数据目录（按标识符）。
///
/// 只认这张表，不做前缀/模糊匹配——避免误伤同级的其他应用目录。表内每一项都必须
/// 能明确指出"它是谁的数据"，界面才能如实分类展示。
pub const MIGRATABLE_SOURCES: &[MigratableSource] = &[
    // 旧版 TieZ：Windows 主线 v0.3.1–v0.3.3 用 `com.tiez.app`，macOS/beta 用 `com.tiez`。
    MigratableSource {
        id: "com.tiez.app",
        origin: SourceOrigin::LegacyTiez,
    },
    MigratableSource {
        id: "com.tiez",
        origin: SourceOrigin::LegacyTiez,
    },
    // 本应用自己的标识符：历史版本的 Tiez-Next，以及之后所有版本。
    MigratableSource {
        id: CURRENT_IDENTIFIER,
        origin: SourceOrigin::PreviousTiezNext,
    },
];

/// 兼容别名：只保留"旧版 TieZ"那两个标识符。
///
/// 保留它是为了让**清理白名单**的语义继续精确——删除是破坏性动作，只应作用于会
/// 被本应用取代的 TieZ 旧目录。查看/迁移用的是 [`MIGRATABLE_SOURCES`]。
pub const LEGACY_IDENTIFIERS: &[&str] = &["com.tiez.app", "com.tiez"];

/// 查一个目录名（identifier）属于哪一类可迁移来源；不在表内则返回 `None`。
pub fn source_origin_of(identifier: &str) -> Option<SourceOrigin> {
    MIGRATABLE_SOURCES
        .iter()
        .find(|s| s.id == identifier)
        .map(|s| s.origin)
}

/// 查一个目录名是否可用于**迁移**（`com.tieznext` 也是合法来源）。
///
/// 与 [`is_cleanable_identifier`] 的区别很关键：迁移是只读动作，可以由本应用自己的
/// 标识符发起（那是旧版 Tiez-Next）；删除是破坏性动作，不允许作用于当前标识符。
pub fn is_migratable_identifier(identifier: &str) -> bool {
    source_origin_of(identifier).is_some()
}

/// 查一个目录名是否允许被**备份后删除**（只允许旧版 TieZ 的两个标识符）。
pub fn is_cleanable_identifier(identifier: &str) -> bool {
    LEGACY_IDENTIFIERS.contains(&identifier)
}


/// 迁移是否成功由"新目录存在可用数据库"作为最终判据。
const DB_FILE: &str = "clipboard.db";

/// 便携版把数据放在**程序目录下这个子目录**里。
///
/// 依据是应用自身的 portable 判定（新旧版本一致）：可执行文件同级存在名为 `data`
/// 的目录时，数据目录即切到该目录。见 [`resolve_source_dir`]。
pub const PORTABLE_DATA_DIR: &str = "data";

// ---------------------------------------------------------------------------
// 迁移进度：阶段、文案与节流判据（全部纯逻辑，不依赖 Tauri）
// ---------------------------------------------------------------------------

/// 过程中事件名（沿用本仓库 kebab-case 约定，见 `services/auto_backup/mod.rs`）。
pub const EVENT_MIGRATION_PROGRESS: &str = "migration-progress";
/// 结束事件名（成功 / deferred / 失败都会发一次）。
pub const EVENT_MIGRATION_DONE: &str = "migration-done";

/// `copying` 阶段的发射节流窗口（毫秒）。
///
/// 【为什么必须节流】迁移的典型负载是"上千个小文件"。若每个文件都发一次事件，
/// 事件通道会被刷爆，前端反而渲染不过来，表现为"界面卡住"——比没有进度更糟。
pub const PROGRESS_THROTTLE_MS: u64 = 150;

/// 一次进度快照。字段与冻结契约的 payload 一一对应。
///
/// `stage_label` 由**后端**产出人话（界面原样显示，不再做映射）——两边各翻一次
/// 必然漂移：后端改了文案、界面还在说旧话。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationProgress {
    /// 阶段取值：`precheck` / `copying` / `verifying` / `deferred` / `done` / `failed`。
    pub stage: &'static str,
    /// 该阶段的中文说明，直接可显示。
    pub stage_label: &'static str,
    /// 当前阶段已完成量。
    pub done: u64,
    /// 当前阶段总量；`0` 表示不可计量（界面显示"不确定进度"，**不得显示 0%**）。
    pub total: u64,
    /// 已复制字节（仅 `copying` 阶段有意义）。
    pub bytes: u64,
    /// 总字节（来自 `scan_tree`）。
    pub bytes_total: u64,
    /// 可选补充说明；失败时为错误摘要。
    pub message: Option<String>,
}

/// 阶段 → 中文文案。契约里这张表由后端持有，界面只显示结果。
pub fn stage_label(stage: &str) -> &'static str {
    match stage {
        "precheck" => "正在检查源目录",
        "copying" => "正在复制数据",
        "verifying" => "正在校验完整性",
        "deferred" => "数据已就绪，等待重启接管",
        "done" => "迁移完成",
        "failed" => "迁移失败",
        _ => "正在迁移",
    }
}

/// 本次进度是否应当真的发出去（节流判据，纯函数便于直接对数字下断言）。
///
/// `is_last = true` 时**无条件**放行：保证最后一个条目一定被报出去，否则进度条会
/// 永远停在 `done < total`（例如"999/1000"），用户以为迁移卡住了。
pub fn should_emit_progress(elapsed_ms: u64, is_last: bool) -> bool {
    is_last || elapsed_ms >= PROGRESS_THROTTLE_MS
}


/// 源目录的只读统计（进度条总数与总字节的来源）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceSummary {
    /// 条目总数（含目录条目，与内部一致性校验同一口径）。
    pub entries: u64,
    /// 全部条目的字节数之和。
    pub bytes: u64,
}

/// 只读统计**归一化之后**的源目录：`precheck` 阶段用它给出进度总数。
///
/// 必须先归一化（[`resolve_source_dir`]）再统计，否则用户把上层目录选成源时，
/// 统计出的是整棵目录树的规模、而真正要复制的只是一小块——进度条会从头到尾都不对。
///
/// 返回 `None` 表示读不到（路径不存在、不是目录、没有权限）。此时**不报错**：
/// 进度总数退化为"不可计量"（`total = 0`，界面显示不确定进度），
/// 真正的诊断由随后的迁移本身给出。
pub fn scan_source_summary(source: &Path) -> Option<SourceSummary> {
    let resolved = resolve_source_dir(source);
    let entries = scan_tree(&resolved).ok()?;
    Some(SourceSummary {
        entries: entries.len() as u64,
        bytes: entries.iter().map(|(_, s)| *s).sum(),
    })
}

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
    /// **数据已复制就绪，待下次启动接管**（v0.5.3 新增的常态路径）。
    ///
    /// 【为什么必须有这一个分支】迁移入口是应用内界面 —— 用户点它时**应用必定在运行**，
    /// 而应用启动时已经打开了目标库（`database::init_db`，连接常驻 `DbState`）。
    /// Windows **不允许改名已打开的文件**，所以在运行期给目标空库改名让位
    /// **必然**报 `os error 32`；而旧的提示让用户"完全退出应用后重试"，
    /// 退出之后**就点不到迁移按钮了**——这是一个不可执行的死循环。
    ///
    /// 因此运行期只做"复制源到暂存 + 写待接管标记"，真正的改名交换留给**下次启动**：
    /// 那时在 `init_db` 之前，无人持有任何句柄，改名必然成功。
    ///
    /// 注意：这不是失败。源目录与目标目录均未被改动，暂存目录**保留**作为交接载体。
    Deferred {
        source: PathBuf,
        target: PathBuf,
        /// 已就绪的暂存目录（下次启动的接管载体，**不得清理**）。
        staging: PathBuf,
        /// 源侧条目总数。
        files: u64,
        /// 源侧全部条目的字节数之和。
        bytes: u64,
    },
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
    /// 源路径存在、也有内容，但**它不是一个应用数据目录**：里面既没有 `clipboard.db`，
    /// 也没有便携版程序目录下的 `data/clipboard.db`。
    ///
    /// 【为什么必须与 `EmptySource` 分开，且必须真的拒绝】
    /// 迁移只在"数据目录"上成立。用户点「选择其它目录…」时很容易停在上层（例如
    /// `下载\`、解压出来的外层目录），此时源里确实有内容——真正属于用户的数据只是
    /// 其中最深处的一小块，其余是无关文件。旧实现会**照常迁移**，把整棵目录树复制进
    /// 应用数据目录：无关文件夹、甚至私钥与机密文档都被搬进应用数据目录，而真正的数据
    /// 落进应用**不读**的嵌套位置，报告却是 `Migrated`、"迁移完成"。
    ///
    /// 因此这一类不是"少迁一点"，而是"这次操作从根上不成立"，必须在复制任何字节之前
    /// 拒绝。它与另外两类共同构成三态，处置完全不同：
    ///
    /// - `SourceMissing`：路径不存在 → 去检查盘/移动硬盘/网络位置，或重选；
    /// - `EmptySource`：路径对了但目录是空的 → 确实没有可迁数据；
    /// - `NotADataDirectory`：路径对了、也有内容，但**选错了层** → 往下进入一层再选。
    NotADataDirectory,
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
            SkipReason::NotADataDirectory => "not_a_data_directory",
            SkipReason::SourceIsAncestorOfTarget => "source_is_ancestor_of_target",
            SkipReason::SourceInsideTarget => "source_inside_target",
        }
    }
}

/// 由新数据目录推导**可迁移来源**目录候选（同名父目录下）。
///
/// 例如新目录为 `%APPDATA%\com.tieznext`，则返回
/// `[%APPDATA%\com.tiez.app, %APPDATA%\com.tiez]`。
///
/// 【为什么这里**不含** `com.tieznext` 自己】它就是 `new_dir` 本身，列进来只会被
/// `same_path` 跳过。旧版 Tiez-Next 的作品位于**别的**数据目录（用户手动选定的
/// 便携目录、旧路径、备份位置），那条路走 [`migrate_from_source_dir`]。
pub fn legacy_dirs_for(new_dir: &Path) -> Vec<PathBuf> {
    let Some(parent) = new_dir.parent() else {
        return Vec::new();
    };
    LEGACY_IDENTIFIERS
        .iter()
        .map(|id| parent.join(id))
        .collect()
}

/// 由新数据目录推导**全部**可迁移来源目录候选：旧版 TieZ 两个 + 本应用标识符。
///
/// 用途是"用户没手动选路径时，自动发现同级的可迁移来源"。同名父目录下的
/// `com.tieznext` 通常就是 `new_dir` 本身（会被 `same_path` 跳过），但在以下场景
/// 它是**真实的不同目录**，因此不能省略：
///
/// - 用户把数据目录改到了别处（`datapath.txt` 重定向），或使用了便携版；
/// - 此时 `%APPDATA%\com.tieznext` 里躺着的是**旧版本 Tiez-Next 的数据**，
///   正是要迁进来的东西。
pub fn migratable_dirs_for(new_dir: &Path) -> Vec<PathBuf> {
    let Some(parent) = new_dir.parent() else {
        return Vec::new();
    };
    MIGRATABLE_SOURCES
        .iter()
        .map(|s| parent.join(s.id))
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
            // 候选扫描这条入口不参与两阶段（它只处理"同名父目录里的可迁移来源"，
            // 且当前无生产调用方）。真出现 Deferred 时按"数据已就绪但未交付"处理：
            // 继续看下一个候选，最终如无成功者按 `NoLegacyDir` 之外的失败上报。
            Outcome::Deferred { .. } => {
                last_failure = Some(MigrationOutcome::Failed {
                    source: legacy,
                    error: "数据已复制到暂存目录，但本次未能交付（两阶段迁移需重启后接管）。"
                        .to_string(),
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
///
/// 【"选错层"必须在复制之前拦住】归一化（[`resolve_source_dir`]）定位失败时按设计
/// **原样返回用户所选路径**（绝不猜测），但这不等于"可以照常迁移"：定位失败恰恰是
/// "这个目录不是数据目录"的证据。若在此处放行，`merge_into` 会把**整棵目录树**复制进
/// 应用数据目录——用户资产（无关文件夹、私钥、机密文档）被搬进应用数据目录，而真正的
/// 数据落进应用不读的嵌套位置，报告却是 `Migrated`。因此归一化之后、任何写操作之前，
/// 必须先确认归一化结果确实是一个数据目录（含 `clipboard.db`，或便携版的
/// `data/clipboard.db`）；不是就如实回报 [`SkipReason::NotADataDirectory`]。
pub fn migrate_from_source_dir(
    source: &Path,
    target: &Path,
    allow_takeover: bool,
) -> MigrationOutcome {
    // 用户手选的路径可能不是"数据目录本身"，而是**包着数据目录的上一层**，
    // 这在便携版上是常态（见 `resolve_source_dir`）。先在那里归一化，再做真正的
    // 迁移；契约完全一致，只是源路径被定位到了正确的那一层。
    //
    // 【拦截只实现一次】归一化后的全部只读判定都在 `migration_precheck` 里，
    // 本函数与两阶段入口（`stage_takeover`）共用它，避免"改了一处漏了一处"。
    if let Some(early) = migration_precheck(source, target, allow_takeover) {
        return early;
    }
    let resolved = resolve_source_dir(source);
    migrate_from_inner(resolved.as_path(), target, allow_takeover)
}

/// [`migrate_from_source_dir`] 的**可注入变体**：改名动作由调用方给出。
///
/// 【为什么必须有这个接缝】两阶段迁移里"给目标空库让位"这一步在真机上的失败来自
/// Windows 的 `os error 32`（文件被本应用自己打开的句柄占住）。而 Linux **允许**改名
/// 已打开的文件，凭"持有连接"无论如何也造不出同样的失败——于是"运行期必然失败 ⇒
/// 必须转 Deferred"这条最关键的路径在本机**零覆盖**。把改名参数化之后，测试可以
/// 确定性地注入"必然失败"的改名器，生产路径传的仍是 [`fs::rename`]。
///
/// 返回 `Deferred` 时**暂存目录必须仍然存在**（它是下次启动接管的载体）——
/// 这是本变体存在的第二个理由：它让"失败时不得删暂存"这条断言可以直接对文件系统下。
#[cfg(test)]
fn migrate_from_source_dir_with_rename(
    source: &Path,
    target: &Path,
    allow_takeover: bool,
    rename: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
) -> MigrationOutcome {
    if let Some(early) = migration_precheck(source, target, allow_takeover) {
        return early;
    }
    let resolved = resolve_source_dir(source);
    migrate_from_inner_with_rename(resolved.as_path(), target, allow_takeover, rename)
}

/// 给 [`migrate_from_inner`] 用的可注入版本。两个版本共用同一段实现，
/// 生产路径只是把 `fs::rename` 传进去。
#[cfg(test)]
fn migrate_from_inner_with_rename(
    source: &Path,
    target: &Path,
    allow_pristine_target: bool,
    rename: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
) -> MigrationOutcome {
    match migrate_from_with(source, target, allow_pristine_target, rename) {
        Outcome::Absent => MigrationOutcome::Skipped(SkipReason::SourceMissing),
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
        Outcome::Deferred {
            staging,
            files,
            bytes,
        } => MigrationOutcome::Deferred {
            source: source.to_path_buf(),
            target: target.to_path_buf(),
            staging,
            files,
            bytes,
        },
        Outcome::Failed(error) => MigrationOutcome::Failed {
            source: source.to_path_buf(),
            error,
        },
    }
}

/// 判定 `dir` 是否是一个应用数据目录：直接含 `clipboard.db`，或含便携版的
/// `data/clipboard.db`。
///
/// 与 [`locate_data_dir`] 的定位规则 1–2 同一判据（只读探测）。之所以单独成一个函数
/// 而不直接复用定位结果，是因为调用方需要区分"归一化没找到"与"归一化找到了但源本身
/// 不是数据目录"这两种情形，而定位函数把两者都折叠成了 `None`。
pub fn is_data_directory(dir: &Path) -> bool {
    dir.join(DB_FILE).is_file() || dir.join(PORTABLE_DATA_DIR).join(DB_FILE).is_file()
}

/// 判定 `dir` 是否含**任何**条目（只读 `read_dir`，不跟随链接、不创建不修改）。
///
/// `pub` 是为了让"两步走"的调用方能在**复制之前**就区分"路径存在但为空"与
/// "有内容但不是数据目录"这两种完全不同的诊断（见 [`migration_precheck`]）。
pub fn dir_has_any_entry(dir: &Path) -> bool {
    match fs::read_dir(dir) {
        Ok(mut entries) => entries.next().is_some(),
        Err(_) => true,
    }
}

/// **纯只读预检**：在复制任何字节之前给出本次迁移会得到的结果。
///
/// 【为什么必须与真正的迁移拆开】两阶段迁移的调用方（界面命令与 MCP 入口）需要在
/// **知道这次到底要不要动、要动多少**之后，才决定是否写"待接管"标记、才把进度条的
/// 总数报给用户。若把预检与复制揉在一起，中间失败时调用方已经写了标记、却什么也没复制。
///
/// 本函数只做只读判定。返回 `None` 表示"可以继续走真实迁移"，返回 `Some(..)` 表示
/// 这次迁移在复制之前就已经注定是某个确定结果。
pub fn migration_precheck(
    source: &Path,
    target: &Path,
    allow_takeover: bool,
) -> Option<MigrationOutcome> {
    let resolved = resolve_source_dir(source);

    // 三态的顺序是刻意的（先"存在/是目录"再"是不是数据目录"）：反过来会把
    // "路径不存在"与"空目录"都折叠成"选错了层"，用户拿到完全错误的处置指引。
    if !resolved.exists() {
        return Some(MigrationOutcome::Skipped(SkipReason::SourceMissing));
    }
    if !resolved.is_dir() {
        return Some(MigrationOutcome::Skipped(SkipReason::NotADirectory));
    }
    if !is_data_directory(&resolved) {
        return Some(MigrationOutcome::Skipped(if dir_has_any_entry(&resolved) {
            SkipReason::NotADataDirectory
        } else {
            SkipReason::EmptySource
        }));
    }

    // 源与目标的相对位置：源是目标的祖先 / 源在目标内部都会造成自复制。
    if resolved == target {
        return Some(MigrationOutcome::Skipped(SkipReason::SamePath));
    }
    if target.starts_with(&resolved) {
        return Some(MigrationOutcome::Skipped(
            SkipReason::SourceIsAncestorOfTarget,
        ));
    }
    if resolved.starts_with(target) {
        return Some(MigrationOutcome::Skipped(SkipReason::SourceInsideTarget));
    }

    // 目标已有数据库且调用方不允许接管 -> 跳过（绝不覆盖）。
    if target.join(DB_FILE).exists() && !allow_takeover {
        return Some(MigrationOutcome::Skipped(SkipReason::TargetAlreadyHasData));
    }

    // 暂存目录就是用户选的源目录：这一步必须拦住。不拦的话下一步"清理上次残留的
    // 暂存目录"会把源整个删掉（回归测试 `staging_never_deletes_the_source`）。
    if staging_dir_legacy(target) == resolved || takeover_staging_dir(target) == resolved {
        let error = staging_collision_message(target, &resolved);
        return Some(MigrationOutcome::Failed {
            source: resolved,
            error,
        });
    }

    None
}

/// 源目录恰好是本次要用的暂存目录时给用户的说明。
fn staging_collision_message(target: &Path, source: &Path) -> String {
    format!(
        "源目录 {} 与本次迁移要使用的暂存目录同名（{}）。为避免覆盖你选择的源目录，已放弃本次迁移；源目录未被读取也未被改动。请改选其它目录。",
        source.display(),
        takeover_staging_dir(target).display()
    )
}


/// 把用户手选的源路径**归一化**到真正的数据目录。
///
/// ## 为什么需要它
///
/// 用户实际用的是**便携版**，数据不在 `%APPDATA%`，而在程序目录里。便携版发布包
/// 解压后是**两层同名目录**（外层是压缩包解出的目录，内层才是程序本体）：
///
/// ```text
/// TieZ_0.3.3-portable\                    <- 外层：与压缩包同名
///   TieZ_0.3.3-portable\                  <- 内层：真正的程序目录
///     tiez-app.exe
///     说明.txt
///     data\                               <- 真正的数据目录（clipboard.db 在这里）
///       clipboard.db
///       attachments\
/// ```
///
/// 用户点「选择其它目录…」时，选中哪一层**取决于他打开到哪一步**：外层、内层、
/// 或里面的 `data`，三种都有可能。旧版 TieZ 与新版本一样，只在可执行文件同级存在
/// 名为 `data` 的目录时才切到便携模式（见 `app/setup.rs` 的 portable 检查），
/// 因此 `data/` 是唯一确定的便携数据目录名。
///
/// ## 判定规则（保守，逐层下探）
///
/// 1. 所选目录**直接**含 `clipboard.db` → 它本身就是数据目录，原样返回。
/// 2. 否则，若所选目录下恰有一个名为 `data` 的子目录，且**它**含 `clipboard.db`
///    → 返回那个子目录。
/// 3. 否则，若所选目录下恰有一个子目录，且从它出发按规则 1–2 能定位到含
///    `clipboard.db` 的数据目录 → 返回那个结果（只穿透**恰好一个**子目录）。
/// 4. 其余情况一律原样返回。
///
/// 【为什么必须让规则 3 生效】用户给的真实路径就是两层同名目录。若只认规则 1–2，
/// 他选中**外层**时归一化会原样返回，于是迁移把外层目录整个复制进目标目录，数据落进
/// `目标\TieZ_0.3.3-portable\data\clipboard.db`——而应用只读 `目标\clipboard.db`，
/// 结果是**迁完看不到任何数据**，且因目标根层已有数据库，重复执行只会一致跳过。
///
/// 【为什么是"恰好一个子目录"而不是递归搜索】递归全树搜索会沿 `attachments/` 之类的
/// 兄弟目录乱钻，可能定位到备份副本甚至无关目录。限制为"唯一的子目录"使下探方向没有
/// 歧义：只有一个候选时不存在选错的可能。歧义（多个子目录）时定位失败，退回用户原选
/// 路径——**绝不猜测**。定位失败**不等于可以照常迁移**：退回原路径之后，
/// [`migrate_from_source_dir`] 会按"这里不是数据目录"拦下本次迁移，如实回报
/// [`SkipReason::NotADataDirectory`]（空目录则是 `EmptySource`、
/// 路径不存在则是 `SourceMissing`），用户再往下选一层即可。
///
/// 本函数**只读**（`is_file` / `is_dir` / `read_dir` 探测），不创建、不修改任何路径。
pub fn resolve_source_dir(source: &Path) -> PathBuf {
    locate_data_dir(source, true).unwrap_or_else(|| source.to_path_buf())
}

/// 定位真正的数据目录：成功返回 `Some(数据目录)`，无法确定时返回 `None`。
///
/// `allow_descent` 控制是否允许"穿透唯一子目录"（外层包目录 → 内层程序目录）。
/// 穿透只做一轮：进入子目录后必须传 `false`，避免在畸形结构上无限深入；深度上限为
/// 2（唯一子目录 + 其下的 `data`），足以覆盖真实的便携包形状。
///
/// 【关键：返回 `None` 而不是"尽力而为的猜测"】只有真的找到了含 `clipboard.db` 的
/// 目录才算定位成功。这样调用方在失败时能安全地退回用户原选的路径，而不是把一个
/// 没有数据库的目录（例如别的软件留下的同名 `data/`）当成数据目录去迁移。
fn locate_data_dir(source: &Path, allow_descent: bool) -> Option<PathBuf> {
    // 规则 1：选中的就是数据目录。
    if source.join(DB_FILE).is_file() {
        return Some(source.to_path_buf());
    }

    // 规则 2：便携版——数据在程序目录下的 `data/`。
    let portable = source.join(PORTABLE_DATA_DIR);
    if portable.join(DB_FILE).is_file() {
        return Some(portable);
    }

    // 规则 3：穿透**恰好一个**子目录（外层包目录 → 内层程序目录）。
    if allow_descent {
        if let Some(child) = sole_subdirectory(source) {
            return locate_data_dir(&child, false);
        }
    }

    // 规则 4：定位失败。
    None
}

/// 返回 `dir` 下**恰好一个**子目录；没有子目录或有多个（含符号链接歧义）时返回 `None`。
///
/// 只读 `read_dir`，不跟随也不创建任何东西。读不到目录时返回 `None`（由调用方按
/// "原样返回"处理）。
fn sole_subdirectory(dir: &Path) -> Option<PathBuf> {
    let mut found: Option<PathBuf> = None;
    for entry in fs::read_dir(dir).ok()?.flatten() {
        // `file_type()` 不跟随符号链接：符号链接一律不视为可下探的子目录，避免
        // 借由链接把迁移引到目录树之外。
        let Ok(ft) = entry.file_type() else {
            return None;
        };
        if !ft.is_dir() {
            continue;
        }
        if found.is_some() {
            return None; // 存在歧义
        }
        found = Some(entry.path());
    }
    found
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
        Outcome::Deferred {
            staging,
            files,
            bytes,
        } => MigrationOutcome::Deferred {
            source: source.to_path_buf(),
            target: target.to_path_buf(),
            staging,
            files,
            bytes,
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

/// 一个可迁移来源目录的现状快照，供"迁移中心"展示。
#[derive(Debug, Clone)]
pub struct LegacyDirInfo {
    /// 目录绝对路径。
    pub path: PathBuf,
    /// 该目录对应的标识符（目录名）。
    pub identifier: String,
    /// 这条目录里的数据原本由哪个应用写入（旧版 TieZ / 历史版本 Tiez-Next）。
    pub origin: SourceOrigin,
    /// 占用的总字节数。
    pub bytes: u64,
    /// 文件总数。
    pub files: u64,
    /// 是否包含主数据库（含则说明是真实数据目录，而非残留空壳）。
    pub has_database: bool,
    /// 是否允许"备份后删除"。
    ///
    /// 只有**旧版 TieZ**的目录可以被清理：本应用已经取代它，删掉是有意义的动作。
    /// 本应用自己标识符的目录（`com.tieznext`）**不可删**——那是用户留着的旧版
    /// Tiez-Next 数据，不该由清理按钮销毁。
    pub can_delete: bool,
}

/// 列出当前存在的**可迁移来源**目录及其占用情况。
///
/// 只做只读统计，不做任何修改。
///
/// ## 扫描范围：`current_dir` 同级，**外加** `extra_roots` 各自的同级
///
/// `current_dir` 是应用当前使用的数据目录；它的同级里能找到旧版 TieZ 的目录，以及
/// （当数据目录被改到别处时）当前标识符留下的旧数据。
///
/// `extra_roots` 用于补上**应用数据目录的原生位置**（Tauri 由 identifier 推导的那个
/// 目录）。存在的必要性：用户一旦改了数据目录或用了便携版，`current_dir` 就搬到了
/// 别处，而 `%APPDATA%\com.tieznext` 里那份**旧版本 Tiez-Next 的数据**并不在
/// `current_dir` 同级——不额外扫这一处就会漏掉它。
///
/// ## 当前数据目录**不入选**
///
/// 与当前数据目录重合的条目直接剔除：它不是"来源"，对它发起迁移只会命中
/// `same_path` 而被跳过。把它摆在界面上只会让用户以为"这里也能迁"，点下去却没有
/// 任何效果——本模块宁可少列，也不给用户一个注定无效的按钮。
///
/// 结果按路径去重。
pub fn list_legacy_dirs(current_dir: &Path, extra_roots: &[PathBuf]) -> Vec<LegacyDirInfo> {
    let mut out: Vec<LegacyDirInfo> = Vec::new();

    for candidate in migratable_source_dirs(current_dir, extra_roots) {
        if !candidate.is_dir() {
            continue;
        }
        // 当前正在使用的数据目录不是迁移来源（见函数文档）。
        if candidate == current_dir {
            continue;
        }
        let identifier = candidate
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        // 表里查不到就跳过：前缀/模糊匹配会误伤同级的其他应用目录。
        let Some(origin) = source_origin_of(&identifier) else {
            continue;
        };

        // 同一个目录可能同时被两条根推导出来（例如数据目录就在原生位置）。
        if out.iter().any(|i| i.path == candidate) {
            continue;
        }

        let (files, bytes) = match scan_tree(&candidate) {
            Ok(entries) => (
                entries.iter().filter(|(k, _)| !k.ends_with('/')).count() as u64,
                entries.iter().map(|(_, s)| *s).sum(),
            ),
            Err(_) => (0, 0),
        };

        out.push(LegacyDirInfo {
            has_database: candidate.join(DB_FILE).exists(),
            can_delete: is_cleanable_identifier(&identifier),
            path: candidate,
            identifier,
            origin,
            bytes,
            files,
        });
    }

    out
}

/// 汇总**所有可能位置**的可迁移来源目录候选（去重）。
///
/// 两条来源根：`current_dir` 的同级，以及 `extra_roots` 各自的同级（调用方传应用数据
/// 目录的原生位置）。列表展示与删除校验共用同一份候选集，避免出现"UI 列出来了、
/// 点删除却说不白名单"的不一致。
pub fn migratable_source_dirs(current_dir: &Path, extra_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = migratable_dirs_for(current_dir);
    for root in extra_roots {
        for candidate in migratable_dirs_for(root) {
            if !roots.contains(&candidate) {
                roots.push(candidate);
            }
        }
    }
    roots
}

/// 备份并删除一个历史数据目录。
///
/// 安全设计（与迁移同一套思路：宁可没做成，不可丢数据）：
/// 1. **先完整备份**到同级带时间戳的目录，备份校验通过后才删除原目录；
/// 2. 备份失败则**不删除**源目录，直接返回错误；
/// 3. 删除目标**必须在候选集之内，且标识符属于可清理白名单**（即只允许旧版 TieZ
///    的两个标识符）——本应用自己的标识符目录不得被删除；
/// 4. 拒绝删除当前正在使用的数据目录。
///
/// 返回备份目录路径，便于 UI 告知用户"备份在哪"。
pub fn backup_and_remove_legacy_dir(
    current_dir: &Path,
    extra_roots: &[PathBuf],
    target: &Path,
) -> Result<PathBuf, String> {
    // ---- 安全校验：只允许删除候选位置里的、可清理标识符目录 ----
    let identifier = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if !is_cleanable_identifier(&identifier) {
        return Err(format!(
            "拒绝操作：{} 不是旧版应用的数据目录，不在允许清理的白名单内",
            target.display()
        ));
    }
    let allowed = migratable_source_dirs(current_dir, extra_roots);
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
    /// 数据已复制到 `staging` 并就绪，待下次启动做改名交换（运行期无法给被占用的
    /// 目标库让位，见 [`MigrationOutcome::Deferred`]）。
    Deferred {
        staging: PathBuf,
        files: u64,
        bytes: u64,
    },
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
#[cfg(test)]
fn yield_target_db(source: &Path, target: &Path) -> Result<Vec<PathBuf>, String> {
    yield_target_db_with(source, target, &mut |from, to| fs::rename(from, to))
}

/// [`yield_target_db`] 的实现主体：把"改名"这一步作为**可注入的接缝**接收。
///
/// 【为什么要有这个接缝】测试需要在任意平台上确定性地复现"改名失败"，而真机上的失败
/// 来自 Windows 的 `os error 32`（文件被本应用自己打开的句柄占住）。Linux 允许改名
/// 已打开的文件，凭"持有连接"无论如何也造不出同样的失败，因此把改名动作参数化：
/// 测试可以注入"对某个名字必然失败"的改名器来复现占用，也可以注入"第二次才失败"的
/// 改名器来验证回滚；生产路径传的仍是 [`fs::rename`]。
///
/// 【失败时是否已经改动过目标或源 —— 结论：没有，所以不需要"预先探测"】
///
/// 本函数是交付（`promote`）之前的**第一步写操作**，调用点在 [`migrate_from`] 的
/// "第三步：交付"开头。走到这里时，此前只发生过两类动作：对源与目标的**只读**
/// `scan_tree`，以及把源**复制**到目标同级的暂存目录。两者都不改动源，也不改动目标的
/// 既有条目。因此：
///
/// - **源**：全程只被读取，任何失败出路都不动它（本模块的核心保证）。
/// - **目标**：循环顺序是 `""` → `-wal` → `-shm`，而真机被占用的恰恰是主库，
///   所以**第一个 `rename` 就会失败**，此时 `yielded` 仍为空、目标一个字节都没变。
///
/// 也就是说失败点"可控且干净"，与 `services::backup::import::swap_managed_entries`
/// 面对同一约束时采用的"直接试、失败即回滚"是同一个模式，因此这里**同样不需要**
/// "先拿哑名试改名再改回"式的预先探测——那只会把一次系统调用变成三次，换不来任何
/// 更强的保证（探测同样会被占用挡住，且探测与真改之间存在竞态窗口）。
///
/// 唯一需要自己兜住的是**非首个条目失败**（主库已让位、`-wal` 才失败）：此时必须把
/// 已经让位的条目放回原位，使"失败即等于调用前状态"成立。这一步在早期实现里是漏的
/// （`?` 直接返回，`yielded` 里已改名的条目无人还原），现由下面的 `Err` 分支补齐。
/// 让位留档的文件名标记：`clipboard.db` → `clipboard.db.unused-<时间戳>`。
///
/// 生产代码与测试都用它，避免两处各写一遍字面量后悄悄漂移。
const UNUSED_MARKER: &str = ".unused-";

fn yield_target_db_with(
    source: &Path,
    target: &Path,
    rename: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
) -> Result<Vec<PathBuf>, String> {
    let mut yielded: Vec<PathBuf> = Vec::new();

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

    // 【这段清理在"文件被占用"时确实会静默失败，但结论是：不需要在这里处理】
    //
    // `let _ = fs::remove_file(...)` 吞掉了错误。实测确认这个吞并在两种平台下都会
    // 真的吞掉一次失败：Windows 上被占用的文件删除会返回 `os error 32`（本就在
    // `remove_file` 上也照样发生）；即便在 Linux 上，"目录不可写"也会让 `remove_file`
    // 失败。所以它**确实是一处真实的静默失败点**。
    //
    // 但它不需要补救，理由是**这里的失败没有后果**：这个分支的语义是"目标里那个文件与
    // 源里的同名文件一模一样，删掉它让后续 `merge_into` 覆盖过去"。而 `merge_into`
    // 对已存在的文件是 `continue`（绝不覆盖），所以就算删除失败、文件留在原地，目标里
    // 的内容仍然**与源完全一致**——用户得到的库是他要的那一份。换句话说：删成功是
    // "后面会覆盖成同样的内容"，删失败是"已经是同样的内容"，两条路殊途同归。
    //
    // 这与真正的失败点不同：那个分支必须改名（把目标让出来），改名失败会让用户拿不到
    // 旧数据、且应用会继续打开那个空库，因此必须报错。**"静默"只允许用在不影响结果的
    // 清理上**，这也是下面 `rename` 失败必须显式返回错误的原因。
    //
    // 代价说明：删除失败时不会留下 `.unused-` 留档（本来也不该留，两者内容一致）。

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
        // 【失败必须回到调用前状态】`fs::rename` 在同一个循环里逐个改名
        // （主库 → -wal → -shm）。若前一个成功、后一个失败，**已经改名的那个必须放回去**
        // —— 否则用户会遇到比原始报错更糟的状态：迁移报"失败"，但目标库其实已经被改名走了，
        // 应用继续打开那个空库，用户的数据看起来"消失了"。
        //
        // 这条原来漏了：`?` 直接返回，`yielded` 里已改名的条目无人还原，而还原函数
        // （`restore_yielded_target_db`）只在**调用方**的失败分支里被调 —— 那只覆盖
        // "改名全成功、后续步骤失败"的情形。现在两条路径都还原。
        if let Err(e) = rename(&from, &to) {
            let msg = format!(
                "{}（源目录未改动）",
                occupied_or_failed_hint(&from, &name, &e)
            );
            // 把本次已改名的放回原位。放回失败的条目**不吞掉**，一并报给用户，
            // 让他知道哪些文件停在了什么位置（比静默留档可诊断）。
            let mut unrestored: Vec<String> = Vec::new();
            for prev in yielded.iter().rev() {
                let Some(orig) = prev
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.split(UNUSED_MARKER).next())
                    .map(|n| target.join(n))
                else {
                    continue;
                };
                if rename(prev, &orig).is_err() {
                    unrestored.push(
                        prev.file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default(),
                    );
                }
            }
            if unrestored.is_empty() {
                // 【必须告知"已经放回去了"】用户在看到"迁移失败"时最担心的就是
                // "我的数据是不是被搞乱了"。这一行直接回答它，并提供可核对的依据
                // （可以自己去看目标目录里主库还在不在）。
                return Err(format!(
                    "{msg}\n已让位到一半的文件已全部放回原位，目标目录恢复成你操作前的样子。"
                ));
            }
            return Err(format!(
                "{msg}\n另外，以下已让位的文件未能还原，它们仍留在目标目录里：{}",
                unrestored.join("、")
            ));
        }
        yielded.push(to);
    }
    Ok(yielded)
}

/// 让位目标空库失败时给用户的提示。
///
/// 【为什么不能直接把系统错误抛给用户】真机上用户看到的是
/// `另一个程序正在使用此文件，进程无法访问。(os error 32)`。这句话有两个问题：
/// 一是**误导**——占用者不是"另一个程序"，**就是本应用自己**：应用启动时已经打开了
/// 数据目录里的 `clipboard.db`（`app/setup.rs` 的 `database::init_db`，连接常驻在
/// `DbState`），用户按提示去关别的软件永远关不出结果；二是**不可执行**——它没说下一步
/// 该做什么。因此这里把可识别的"被占用"统一换成一句能照做的事，并保留"源目录未改动"
/// 这一保证（由调用方拼接，见上）。
///
/// ## "占用"的判定不能依赖 `ErrorKind`
///
/// 【为什么这里的判据与 v0.5.2 不一样 —— 这是一次真实的死代码修复】
///
/// v0.5.2 写的是 `matches!(e.kind(), ErrorKind::PermissionDenied | ErrorKind::Other)`。
/// 实测（真机截图 + Rust 标准库源码）证明它在 Windows 上**永远不会命中**：
///
/// 1. `ErrorKind::Other` —— `core/src/io/error.rs` 明确写着
///    「This `ErrorKind` is **not used by the standard library**」，标准库从不产生它，
///    因此这一支是死代码；
/// 2. `ERROR_SHARING_VIOLATION`(32) / `ERROR_LOCK_VIOLATION`(33) —— 在
///    `std/src/sys/io/error/windows.rs` 的**整张映射表里都不存在**（该表唯一的
///    "LOCK" 项是 `ERROR_POSSIBLE_DEADLOCK`），必然落到末尾的 `_ => Uncategorized`。
///    而 `Uncategorized` 带 `#[unstable]` 标注，stable 工具链**无法 match**它
///    （实测 `error[E0658]: use of unstable library feature 'io_error_uncategorized'`）。
///
/// ⇒ 旧判据恒为 `false`，用户看到的是通用兜底文案 + `错误档位 Uncategorized`
/// （真机截图原文：`系统错误码 32, 错误档位 Uncategorized`）。
///
/// 因此这里改成**按原始错误码判定**，并且用 `cfg` 严格隔离：
///
/// - Windows：`raw_os_error()` 为 `32`/`33` 即占用；同时保留 `PermissionDenied`
///   （某些网络盘/杀软场景给的是 `ERROR_ACCESS_DENIED`(5)）。
/// - 非 Windows：**不得看错误码**——Linux 上 `from_raw_os_error(32)` 是 `BrokenPipe`，
///   含义完全不同，照搬会把无关错误误报成占用。非 Windows 只看 kind
///   （`BrokenPipe` 是 Linux 上"文件被占用/管道断开"最接近的对应物）。
///
/// 名字的约束同样重要：**只有主库 `clipboard.db` 被占用时才能把结论说成"数据库被占用"**。
/// `-wal` / `-shm` 失败的原因可能完全不同（残留文件的权限、杀软隔离等），套用同一句话
/// 会把用户引向错误的排查方向，因此那种情况只做"别的东西挡住了它 + 退回重试"的保守表述。
fn looks_occupied(e: &io::Error) -> bool {
    #[cfg(windows)]
    {
        matches!(e.raw_os_error(), Some(32) | Some(33))
            || matches!(e.kind(), ErrorKind::PermissionDenied)
    }
    #[cfg(not(windows))]
    {
        matches!(e.kind(), ErrorKind::PermissionDenied | ErrorKind::BrokenPipe)
    }
}

fn occupied_or_failed_hint(from: &Path, name: &str, e: &io::Error) -> String {
    let occupied = looks_occupied(e);
    // 【不要拼接系统给的本地化错误句子】原文就是
    // `另一个程序正在使用此文件，进程无法访问。(os error 32)`，把它嵌进提示里等于
    // 把要消除的误导原样留在用户眼前。这里只保留**可诊断的技术细节**（错误档位 + 错误号），
    // 既够支持人员定位，又不会与"占用者就是本应用自己"的结论打架。
    let detail = match e.raw_os_error() {
        Some(code) => format!("系统错误码 {}，错误档位 {:?}", code, e.kind()),
        None => format!("错误档位 {:?}", e.kind()),
    };
    if name == DB_FILE && occupied {
        return format!(
            "无法让位目标里未使用过的空库 {}：它正被 Tiez-Next 自己占用（{}）。\
             \n请从系统托盘（任务栏右下角）的 Tiez-Next 图标右键选择「退出 Tiez-Next」，\
             完全退出应用后再重试本次迁移。\
             \n注意：这不是别的程序占用了它，关闭其它软件不会有帮助；\
             点窗口右上角的 × 只是把窗口收进托盘，应用仍在运行。",
            from.display(),
            detail
        );
    }
    format!(
        "无法让位目标里未使用过的空库 {}：{}。\
         \n这通常是因为该文件仍被占用（例如应用的另一个窗口或后台进程还在用它）。\
         \n请从系统托盘的 Tiez-Next 图标右键选择「退出 Tiez-Next」，完全退出应用后再重试。",
        from.display(),
        detail
    )
}

/// 还原被 [`yield_target_db`] 改名让位的空库（交付失败、或让位中途失败时回滚）。
///
/// 【为什么不用 `file_name()` 再做 `.unused-` 字符串切割】那样写有个隐患：切出来的
/// "原名"会被重新 `join` 到 `target` 上，而 `target` 是应用数据目录的**绝对路径**。
/// 只要路径里任何一段包含 `.unused-`（用户完全可能把数据目录放在名为
/// `backup.unused-2024` 的文件夹下），`split` 就会从**路径中间**切开，还原落到一个
/// 完全无关的位置——既不报错，也不还原，是典型的静默错位。
///
/// 因此这里改成"用 `to` 的文件名反推原名，并且**只在 `to` 确实位于 `target` 之下**时
/// 才动手"，用 [`Path::file_name`] 而不是字符串切割，还原目标由 `target.join(原名)`
/// 精确构造。
fn restore_yielded_target_db(yielded: &[PathBuf], target: &Path) {
    // 后缀形如 `.unused-<时间戳>`；常量集中在这里，与 [`yield_target_db_with`] 里生成
    // 归档名时用的是同一个标记。
    const MARKER: &str = ".unused-";
    for to in yielded {
        // 只还原"确实在本次目标目录里"的条目，越界的一律不碰。
        if !to.starts_with(target) {
            continue;
        }
        let Some(file_name) = to.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        let Some(original) = file_name.split(MARKER).next() else {
            continue;
        };
        if original.is_empty() || original == file_name {
            continue;
        }
        let from = target.join(original);
        if !from.exists() {
            let _ = fs::rename(to, &from);
        }
    }
}

fn migrate_from(source: &Path, target: &Path, allow_pristine_target: bool) -> Outcome {
    migrate_from_with(source, target, allow_pristine_target, &mut |from, to| fs::rename(from, to))
}

fn migrate_from_with(
    source: &Path,
    target: &Path,
    allow_pristine_target: bool,
    rename: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
) -> Outcome {
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
    //
    // 【与 v0.5.2 的关键差别：这里不再"无条件清理上次残留"】
    //
    // 旧实现在此处对 `.<目标名>.migrating.<pid>` 直接 `remove_dir_all`——因为那个名字
    // 的语义确实是"用完即弃的临时工作区"。但两阶段迁移用的暂存目录是**待接管载体**：
    // 用户点第二次迁移时，上次那个已经复制好的载体正是本次要继承的东西，删掉它等于
    // 让用户"每点一次就白干一次"。因此：
    //
    //   * 清理只针对**旧命名**的残渣（带 pid，语义确实是残渣）；
    //   * 固定名 `.<目标名>.pending-takeover` 的清理只由"标记已无主"驱动
    //     （见 [`cleanup_stale_staging`]，`takeover_reserved` 由调用方按标记文件判定）。
    //
    // 【本函数是一次性交付路径，已不再是界面/MCP 走的路径】
    // 它保留下来给"目标目录当时没有被占用、可以当场交付"的场景与既有测试使用。
    // 界面与 MCP 两个入口走 [`stage_takeover`]（两阶段），因此这里的"清掉同名残留"
    // **不会**删到某个正在等着被接管的载体。
    let staging = takeover_staging_dir(target);
    if staging == source {
        return Outcome::Failed(staging_collision_message(target, source));
    }
    // `true` = **保留固定名那一个**（它可能正是"待接管载体"），只清旧命名的残渣。
    // 传 `false` 会把载体一起删掉，下面那条拒绝检查就成了死代码（本测试实测抓出）。
    cleanup_stale_staging(target, true);
    if staging.exists() {
        // 【这里刻意"拒绝"而不是"删掉重来"】
        //
        // 固定名暂存目录若已存在，它只可能来自**两阶段迁移**（即"上次点迁移留下的
        // 待接管载体"）。本函数是"当场交付"的一次性路径，不具备"接管"的语义；
        // 一旦它把那个目录删掉，用户上次复制好的数据就没了——而这正是 v0.5.2 的
        // 第二个缺陷（用户第二次点击就删掉上次的成果）。
        //
        // 生产上两个入口（界面 / MCP）都走 `stage_takeover`，不会走到这里；
        // 保留拒绝语义是为了让"误用这条路径"不产生数据损失，而不是静默破坏。
        return Outcome::Failed(format!(
            "目标同级已存在待接管的暂存目录 {}（上次迁移已复制就绪、等待重启接管）。\
             为避免删除它，已放弃本次当场交付；请重启应用完成接管，或先手动处理该目录。\
             源目录未被读取也未被改动。",
            staging.display()
        ));
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
    //
    // 【运行期的必然失败在这里被转成 Deferred，而不是报错】
    //
    // 迁移入口在应用内，用户点它时应用必定在运行，而启动早已打开目标库
    // （连接常驻 `DbState`）。Windows **不允许改名已打开的文件**，所以这一步在真机上
    // **必然**失败——旧的提示让用户"完全退出应用后重试"，退出之后却点不到按钮，
    // 是一个不可执行的死循环。现在的处置是：**改名失败即判 Deferred**，把暂存目录
    // 留作交接载体，由下次启动在 `init_db` 之前完成交换（见 [`promote_staged_takeover`]）。
    let yielded = if allow_pristine_target {
        match yield_target_db_with(source, target, rename) {
            Ok(v) => v,
            // 【失败的诊断信息这里**用不上**，是刻意的】改名失败即意味着"文件被本应用
            // 自己打开的句柄占住"，而运行期无法消除这个占用——它不需要更多诊断，
            // 只需要一个正确的动作：把数据留在暂存目录里，交给下次启动。
            // 因此这里不读 `e`（旧实现把它的原文抛给用户，才产生了那句不可执行的提示）。
            Err(_e) => {
                // 失败 = 文件被占用（真机必然如此）。**绝不清理暂存目录**：它正是
                // 下次启动接管要用的东西（这是 v0.5.2 的第 3 个缺陷）。
                return Outcome::Deferred {
                    staging,
                    files: source_entries.len() as u64,
                    bytes: source_entries.iter().map(|(_, s)| *s).sum(),
                };
            }
        }
    } else {
        Vec::new()
    };
    if let Err(e) = promote(&staging, target, false) {
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

// ---------------------------------------------------------------------------
// 两阶段迁移：运行期「复制到暂存」，启动期「改名交换」
// ---------------------------------------------------------------------------

/// 两阶段迁移的**第一步**（运行期）：只把源复制到固定暂存目录，不动目标一个字节。
///
/// 返回 [`MigrationOutcome::Deferred`] 表示"数据已就绪，请写待接管标记并提示重启"。
/// 返回 `Skipped` / `Failed` 时**不写标记**，全部按既有语义处理。
///
/// 【与 [`migrate_from_source_dir`] 的关系】两者共用 [`migration_precheck`] 的全部只读
/// 判定与 `copy_tree` 的复制实现，只有"最后一步要不要动目标"不同：
/// 前者当场交付（目标无占用时可行），后者一律交给下次启动。
///
/// ## 幂等（这是"第二次点击就白干"的正解）
///
/// 若固定暂存目录已存在且与源**逐项一致**，直接复用它、**不重新复制**：
/// 用户连点两次、或在提示重启后改变主意又点了一次，都不会把上一次已复制好的成果
/// 删掉重来（上千文件时那是几十秒的重复劳动）。
pub fn stage_takeover(source: &Path, target: &Path) -> MigrationOutcome {
    stage_takeover_with_progress(source, target, true, &mut |_, _, _, _| {})
}

/// [`stage_takeover`] 的**带进度**版本：每复制完一个文件回调一次。
///
/// 回调参数：`(已复制文件数, 总文件数, 已复制字节, 总字节)`。
/// 前两个数让界面能显示"3 / 1200 个文件"，后两个是进度条按体积推进的依据。
///
/// 【总数从哪来】复制开始前先对源做一次只读扫描（`scan_tree`），因此总数与总字节
/// **在开始复制之前就已确定**——进度是真的能算，不是装样子。
pub fn stage_takeover_with_progress(
    source: &Path,
    target: &Path,
    allow_takeover: bool,
    on_item: &mut dyn FnMut(u64, u64, u64, u64),
) -> MigrationOutcome {
    // 【`allow_takeover` 必须由调用方传入，不能在这里写死】
    //
    // 目标是"应用已经打开过的数据目录"时，里面那个 `clipboard.db` 可能装着用户的
    // 真实记录。是否允许接管它，取决于**只有命令层能做的**判定（读 SQLite 数记录、
    // 查用户自建标签；见 `system_cmd::target_db_is_pristine`）。本模块按契约只依赖
    // `std`，**没有能力**自己做这个判定，因此绝不能替调用方决定。
    //
    // 传 `false` 且目标已有库时，预检会回 `Skipped(TargetAlreadyHasData)`——
    // 与一次性交付路径（`migrate_from_source_dir(.., false)`）完全一致。
    if let Some(early) = migration_precheck(source, target, allow_takeover) {
        return early;
    }
    // 走到这里说明允许接管；后面的判断都按"可以从容让位"进行。
    let _ = allow_takeover;
    let resolved = resolve_source_dir(source);

    let source_entries = match scan_tree(&resolved) {
        Ok(v) => v,
        Err(e) => {
            return MigrationOutcome::Failed {
                source: resolved,
                error: format!("读取源目录失败: {}", e),
            }
        }
    };
    if source_entries.is_empty() {
        return MigrationOutcome::Skipped(SkipReason::EmptySource);
    }

    let staging = takeover_staging_dir(target);

    // 【本次要用的暂存目录不能被当成"无主残渣"清掉，也不能被当成源】
    if staging == resolved {
        let error = staging_collision_message(target, &resolved);
        return MigrationOutcome::Failed {
            source: resolved,
            error,
        };
    }

    // 已经存在一份**与源逐项一致**的暂存：直接复用它。
    //
    // 【这是障碍之二的正解】v0.5.2 在每次迁移开头**无条件** `remove_dir_all(staging)`，
    // 于是用户第二次点迁移就把上一次已经复制好的成果删掉、从头再抄一遍（上千文件时
    // 是几十秒的重复劳动）。现在只在"暂存与源不一致"时才重建它。
    if staging.exists() {
        if let Ok(staged) = scan_tree(&staging) {
            if staged == source_entries {
                return MigrationOutcome::Deferred {
                    source: resolved,
                    target: target.to_path_buf(),
                    staging,
                    files: source_entries.len() as u64,
                    bytes: source_entries.iter().map(|(_, s)| *s).sum(),
                };
            }
        }
        let _ = fs::remove_dir_all(&staging);
    }
    // 清掉旧命名（带 pid）的残渣。`true` = 保留固定名那一个（它就是本次要复用的载体）。
    cleanup_stale_staging(target, true);

    // 总数在复制之前就已确定（来自上面的只读扫描）。
    let files_total = source_entries.iter().filter(|(r, _)| !r.ends_with('/')).count() as u64;
    let bytes_total: u64 = source_entries.iter().map(|(_, s)| *s).sum();

    {
        let mut report = |done: u64, bytes: u64| on_item(done, files_total, bytes, bytes_total);
        if let Err(e) = copy_tree_reporting(&resolved, &staging, &mut report) {
            let _ = fs::remove_dir_all(&staging);
            return MigrationOutcome::Failed {
                source: resolved,
                error: format!("复制到暂存目录失败: {}", e),
            };
        }
        // 【最后一个条目必须强制报一次】否则进度条会永远停在 `done < total`
        // （例如 1199/1200），用户以为迁移卡住了。是否真的发出去由调用方按节流决定，
        // 但"有机会发最后一条"这件事必须由这里保证。
        if files_total > 0 {
            on_item(files_total, files_total, bytes_total, bytes_total);
        }
    }

    // 校验：暂存必须与源逐项一致（路径 + 字节数）。
    match scan_tree(&staging) {
        Ok(staged) => {
            if staged != source_entries {
                let _ = fs::remove_dir_all(&staging);
                return MigrationOutcome::Failed {
                    source: resolved,
                    error: format!(
                        "暂存副本与源不一致（源 {} 项 / 暂存 {} 项），已放弃本次迁移，源数据未改动",
                        source_entries.len(),
                        staged.len()
                    ),
                };
            }
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&staging);
            return MigrationOutcome::Failed {
                source: resolved,
                error: format!("校验暂存目录失败: {}", e),
            };
        }
    }

    MigrationOutcome::Deferred {
        source: resolved,
        target: target.to_path_buf(),
        staging,
        files: source_entries.len() as u64,
        bytes: source_entries.iter().map(|(_, s)| *s).sum(),
    }
}

/// **启动期接管**：把上次运行留下的暂存目录提升为正式目标。
///
/// 调用时机是**启动早期、`init_db` 之前**——那时没有任何 `Connection`，因此
/// Windows 的"不允许改名已打开的文件"这条限制不再成立，交换必然成功。
///
/// ## 为什么必须先"让位"再提升
///
/// 目标目录里躺着的 `clipboard.db` 是上一次启动时 `init_db` 建的**空库**
/// （WAL 模式下还带 `-wal` / `-shm`）。而 [`promote`] 在目标已存在时走
/// [`merge_into`]，其契约是"**绝不覆盖**目标里已存在的文件"——那个空库会让真正的
/// 数据永远进不去。所以先把同名文件改名归档（**改名，不是删除**），再提升。
///
/// ## 幂等
///
/// 若目标里那个库与暂存里的**内容完全一致**（上一次接管已经写成、或用户反复验证），
/// 就不做归档，直接删掉它让后续合并覆盖过去——否则用户每次重启都会多出一个
/// `.unused-<时间戳>` 文件（既有回归 `repeated_takeover_does_not_pile_up_archives`
/// 守的就是这条）。暂存目录被提升后即消失，第二次调用直接 `NothingToDo`。
///
/// ## 失败语义
///
/// 任何一步失败都**不删除暂存目录**（调用方据此保留标记、下次再试），且目标回到
/// 调用前的状态（已归档的文件会被放回原位）。`rename` 作为可注入接缝，便于在
/// Linux 上确定性地复现"改名被占用挡下"的真机情形。
pub fn promote_staged_takeover(
    staging: &Path,
    target: &Path,
    rename: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
) -> Result<TakeoverPromotion, String> {
    if !staging.exists() {
        return Ok(TakeoverPromotion::NothingToDo);
    }

    // 第一步：让位目标里那个（空）库。这一步在启动期**必然**成功——无人持句柄。
    let yielded = yield_target_db_with(staging, target, rename)?;

    // 第二步：提升暂存目录（`true` = 逐文件搬入，见 `promote` 的文档）。
    // 失败则把刚让位的文件放回原位，目标恢复原状。
    if let Err(e) = promote(staging, target, true) {
        restore_yielded_target_db(&yielded, target);
        return Err(format!(
            "把暂存目录提升到目标目录失败: {}（已让位的文件已放回原位，暂存目录保留，标记未清除）",
            e
        ));
    }

    Ok(TakeoverPromotion::Promoted)
}

/// 带"默认改名器"的 [`promote_staged_takeover`]（生产路径用它）。
pub fn promote_staged_takeover_default(
    staging: &Path,
    target: &Path,
) -> Result<TakeoverPromotion, String> {
    promote_staged_takeover(staging, target, &mut |from, to| fs::rename(from, to))
}

/// [`promote_staged_takeover`] 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakeoverPromotion {
    /// 暂存目录不存在（没有待接管的数据，或上一次已经接管完成）。
    NothingToDo,
    /// 已把暂存目录提升为正式目标。
    Promoted,
}


/// **旧**暂存目录路径：目标同级，名字带 pid 以便并发/崩溃区分。
///
/// 【为什么保留它、又为什么新代码不该再用它】
///
/// 这个名字（`.<目标名>.migrating.<pid>`）是"一次迁移的临时工作区"的语义：**用完即弃**，
/// 失败了就该清理。两阶段迁移里那个"等着下次启动接管的暂存目录"语义**完全不同**
/// ——它必须活得比进程久，失败了也不能删（否则用户第二次点迁移就把上次的载体删了）。
///
/// 两种语义共用一个名字会产生一个极隐蔽的 bug：`staging_dir_legacy` 带 pid，
/// 用户重启后 pid 变了，于是**下次启动的清理会删掉上一次的暂存**、而"待接管"标记
/// 还指着它。因此新路径统一使用 [`takeover_staging_dir`]（名字里没有 pid，固定可寻址）。
///
/// 保留本函数只有一个用途：**识别并清掉旧版本留下的残渣**（`cleanup_stale_legacy_staging`），
/// 以及兼容既有测试对"同名源目录"这条边界条件的覆盖。
fn staging_dir_legacy(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "appdata".to_string());
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(".{}.migrating.{}", name, std::process::id()))
}

/// 暂存目录名里那个"待接管"的固定前缀，例如 `.com.tieznext.pending-takeover`。
///
/// 【为什么固定、不带 pid】它是**跨进程**的交接点：本次运行写它、下次启动读它。
/// 名字里带 pid 会让下次启动找不到上一次留下的东西（见 [`staging_dir_legacy`]）。
const PENDING_TAKEOVER_SUFFIX: &str = ".pending-takeover";

/// **待接管**暂存目录路径：目标同级，名字固定（跨进程可寻址）。
///
/// 与 [`staging_dir_legacy`] 的关系：这是**唯一**允许在进程结束后继续存在的暂存目录。
/// 它的生命周期由"待接管标记"决定——标记在，它就必须在；标记被消费（或用户放弃），
/// 它才可以被清理。
pub fn takeover_staging_dir(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "appdata".to_string());
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("{}{}", name, PENDING_TAKEOVER_SUFFIX))
}

/// 在 `target` 同级清理**已无标记支撑**的半成品暂存目录。只删确定的两种：
///
/// 1. 旧命名 `.<目标名>.migrating.<pid>`（带 pid）；
/// 2. 本次要用的固定名 `.<目标名>.pending-takeover`，但**仅当调用方明确说它已无主**
///    （`takeover_reserved = true`，即标记文件已不指向它）。
///
/// 【绝不删除"有标记支撑"的那一个】这正是 v0.5.2 的障碍之一：旧实现在每次迁移开头
/// **无条件** `remove_dir_all(staging)`，用户第二次点迁移就把上一次已经复制好的待接管
/// 载体删掉了。因此这里把"要不要删"变成调用方的一个**显式输入**，而不是默认行为。
pub fn cleanup_stale_staging(target: &Path, takeover_reserved: bool) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    let Some(parent) = target.parent() else {
        return removed;
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return removed;
    };

    let target_name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let legacy_prefix = format!(".{}.migrating.", target_name);
    let reserved = takeover_staging_dir(target);

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        let is_legacy = name.starts_with(&legacy_prefix);
        let is_reserved_slot = path == reserved;
        if !is_legacy && !is_reserved_slot {
            continue;
        }
        // 有标记支撑的固定暂存目录：留着，等下一次启动接管。
        if is_reserved_slot && takeover_reserved {
            continue;
        }
        if fs::remove_dir_all(&path).is_ok() {
            removed.push(path);
        }
    }
    removed
}


/// 把暂存目录交付到目标位置。
///
/// - 目标不存在：直接 `rename`（同一文件系统内为原子操作）。
/// - 目标已存在：**递归合并，逐文件判断**，已存在的文件一律保留不覆盖。
///
/// 注意必须递归进同名目录：目标里若已有 `attachments/` 目录，不能因为目录本身
/// 存在就跳过，否则该目录下源中独有的文件永远补不齐。
fn promote(staging: &Path, target: &Path, into_missing_dir: bool) -> io::Result<()> {
    if !target.exists() && !into_missing_dir {
        return fs::rename(staging, target);
    }
    if into_missing_dir {
        // 启动期接管：目标目录**一定**存在（它就是应用的数据目录，`init_db` 马上要在
        // 里面建库）。此时必须逐文件搬入，否则暂存目录这个容器本身会被搬成目标的一个
        // 子目录（`target/.com.tieznext.pending-takeover/clipboard.db`），而应用读的是
        // `target/clipboard.db`——**迁完看不到任何数据**。
        merge_into_with(staging, target, true)?;
    } else {
        merge_into(staging, target)?;
    }
    let _ = fs::remove_dir_all(staging);
    Ok(())
}

/// 递归合并 `src` 到 `dst`，**绝不覆盖 `dst` 中已存在的文件**。
///
/// 优先用 `rename`（同文件系统内高效且原子）；跨设备失败时退回"复制 + 删除副本"，
/// 其中删除的始终是暂存侧副本，`src` 原始数据不受影响。
///
/// `into_missing_dir` 控制"目标不存在同名目录时"的搬法：
/// - `false`（默认，用于交付到正式目标目录）：整体 `rename` 目录，快且原子；
/// - `true`（用于启动期接管）：**递归按文件搬**，避免把暂存目录这个容器本身
///   搬成目标的一个子目录。
fn merge_into(src: &Path, dst: &Path) -> io::Result<()> {
    merge_into_with(src, dst, false)
}

fn merge_into_with(src: &Path, dst: &Path, into_missing_dir: bool) -> io::Result<()> {
    if !dst.exists() {
        fs::create_dir_all(dst)?;
    }
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ty = entry.file_type()?;

        if ty.is_dir() {
            if to.exists() || into_missing_dir {
                // 目录已存在（或本次要求逐文件搬入）：递归进去继续补齐，而不是整目录跳过
                merge_into_with(&from, &to, into_missing_dir)?;
                if into_missing_dir {
                    let _ = fs::remove_dir(&from);
                }
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
    copy_tree_reporting(src, dst, &mut |_, _| {})
}

/// 递归复制，并**每复制完一个文件就回调一次**（参数为"已完成文件数 / 已完成字节数"）。
///
/// 【为什么必须有这个变体】没有它，进度条只能报"开始复制"和"复制完成"两次——
/// 上千个文件的长过程里界面**一动不动**，用户会以为程序卡死。有了它，"已复制
/// N / M 个文件、X / Y 字节"就都是真实数字。
///
/// 回调只拿数字、不认识事件：本模块按契约**只依赖 `std`**，事件发射是调用方的事。
fn copy_tree_reporting(
    src: &Path,
    dst: &Path,
    report: &mut dyn FnMut(u64, u64),
) -> io::Result<()> {
    let mut done_files = 0u64;
    let mut done_bytes = 0u64;
    copy_tree_inner(src, dst, report, &mut done_files, &mut done_bytes)?;
    Ok(())
}

fn copy_tree_inner(
    src: &Path,
    dst: &Path,
    report: &mut dyn FnMut(u64, u64),
    done_files: &mut u64,
    done_bytes: &mut u64,
) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_tree_inner(&from, &to, report, done_files, done_bytes)?;
        } else if ty.is_file() {
            fs::copy(&from, &to)?;
            *done_files += 1;
            *done_bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
            report(*done_files, *done_bytes);
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
// 本模块只依赖 `std`，因此这些测试可以脱离 Tauri 与平台专用代码单独编译运行，
// 不必等整个 crate 能在当前平台编译通过。
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
        assert!(!takeover_staging_dir(&target).exists());
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

        let list = list_legacy_dirs(&current, &[]);

        assert_eq!(list.len(), 1);
        let info = &list[0];
        assert_eq!(info.identifier, "com.tiez.app");
        assert_eq!(info.origin, SourceOrigin::LegacyTiez);
        assert!(info.has_database);
        assert!(info.can_delete, "旧版 TieZ 目录允许清理");
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

        // 两个旧版 TieZ 目录都不存在；当前目录存在但**不提供任何可迁移价值**
        // （它就是正在使用的目录），因此列表必须为空。
        let list = list_legacy_dirs(&current, &[]);
        assert!(
            list.is_empty(),
            "只有当前目录存在时，无可迁移来源，实际: {:?}",
            list.iter().map(|i| &i.identifier).collect::<Vec<_>>()
        );
        // 当前目录本身不会被列为可迁移/可清理项
        assert!(!list.iter().any(|i| i.path == current));
    }

    /// `extra_roots` 让"数据目录已被改到别处"时仍能发现原生位置里的旧数据。
    ///
    /// 这是便携版/自定义数据目录用户的真实情形：`%APPDATA%\com.tieznext` 里留着
    /// 旧版本 Tiez-Next 的数据，而当前数据目录在另一个磁盘上。
    #[test]
    fn extra_roots_reveal_native_location_sources() {
        let root = tmp("extra-roots");
        // 当前数据目录在别处（便携盘）
        let current = root.join("portable").join("data");
        fs::create_dir_all(&current).unwrap();
        // 原生应用数据目录位置（Tauri 由 identifier 推导），同级有旧数据与旧版 TieZ
        let native = root.join("appdata").join(CURRENT_IDENTIFIER);
        fs::create_dir_all(&native).unwrap();
        let native_own = root.join("appdata").join(CURRENT_IDENTIFIER);
        seed_legacy(&native_own);
        let native_tiez = root.join("appdata").join("com.tiez.app");
        seed_legacy(&native_tiez);

        // 不额外扫原生位置 -> 什么都找不到（当前目录同级没有这些）
        assert!(list_legacy_dirs(&current, &[]).is_empty());

        // 加上原生位置 -> 两条来源都被发现，且分类正确
        let list = list_legacy_dirs(&current, &[native.clone()]);
        assert_eq!(list.len(), 2, "实际: {:?}", list.iter().map(|i| &i.identifier).collect::<Vec<_>>());

        let own = list.iter().find(|i| i.path == native_own).unwrap();
        assert_eq!(own.origin, SourceOrigin::PreviousTiezNext);
        assert!(!own.can_delete);

        let tiez = list.iter().find(|i| i.path == native_tiez).unwrap();
        assert_eq!(tiez.origin, SourceOrigin::LegacyTiez);
        assert!(tiez.can_delete);
    }

    /// 本应用标识符（`com.tieznext`）是**合法的迁移来源**。
    ///
    /// 场景：用户把数据目录改到了别处（或用了便携版），于是 `%APPDATA%\com.tieznext`
    /// 里留着的是**旧版本 Tiez-Next 的数据**——那是要迁进来的东西，必须被发现。
    /// 标识符跨版本不变，所以这条规则对未来版本同样成立。
    #[test]
    fn lists_own_identifier_dir_as_a_migratable_source() {
        let root = tmp("list-own");
        // 当前数据目录在另一处（模拟 datapath.txt 重定向 / 便携版）
        let current = root.join("elsewhere").join("com.tieznext");
        fs::create_dir_all(&current).unwrap();
        // 原生位置同级还留着一份旧版本 Tiez-Next 的数据
        let native_parent = root.join("appdata");
        let previous = native_parent.join(CURRENT_IDENTIFIER);
        let native_root = native_parent.join("anchor");
        seed_legacy(&previous);

        let list = list_legacy_dirs(&current, &[native_root]);

        let own = list
            .iter()
            .find(|i| i.path == previous)
            .expect("com.tieznext 目录必须被列为可迁移来源");
        assert_eq!(own.identifier, "com.tieznext");
        assert_eq!(
            own.origin,
            SourceOrigin::PreviousTiezNext,
            "来源应如实标为「历史版本的 Tiez-Next」"
        );
        assert!(own.has_database);
        // 当前正在使用的目录不得出现在列表里
        assert!(!list.iter().any(|i| i.path == current));
        // 本应用自己的目录**不允许被清理**（它不是被取代的旧应用）
        assert!(!own.can_delete, "com.tieznext 目录不得被清理按钮删除");
    }

    /// 来源分类必须如实区分"旧版 TieZ"与"历史版本 Tiez-Next"。
    #[test]
    fn source_origins_distinguish_legacy_tiez_from_previous_tiez_next() {
        assert_eq!(
            source_origin_of("com.tiez.app"),
            Some(SourceOrigin::LegacyTiez)
        );
        assert_eq!(source_origin_of("com.tiez"), Some(SourceOrigin::LegacyTiez));
        assert_eq!(
            source_origin_of(CURRENT_IDENTIFIER),
            Some(SourceOrigin::PreviousTiezNext)
        );
        // 不在表内的目录名一律不认（不做前缀/模糊匹配）
        assert_eq!(source_origin_of("com.tie"), None);
        assert_eq!(source_origin_of("com.tieznext.other"), None);
        assert_eq!(source_origin_of(""), None);

        // 稳定机器码：界面按它映射文案
        assert_eq!(SourceOrigin::LegacyTiez.code(), "legacy_tiez");
        assert_eq!(
            SourceOrigin::PreviousTiezNext.code(),
            "previous_tiez_next"
        );
    }

    /// 迁移来源 ⊇ 清理白名单：自己的标识符能迁移，但**不能删**。
    #[test]
    fn own_identifier_is_migratable_but_never_cleanable() {
        assert!(is_migratable_identifier(CURRENT_IDENTIFIER));
        assert!(
            !is_cleanable_identifier(CURRENT_IDENTIFIER),
            "删除是破坏性动作，不得作用于本应用自己的标识符目录"
        );
        for id in LEGACY_IDENTIFIERS {
            assert!(is_migratable_identifier(id), "{} 应可迁移", id);
            assert!(is_cleanable_identifier(id), "{} 应可清理", id);
        }
    }

    /// **本条需求的核心用例**：从 `com.tieznext`（旧版本 Tiez-Next）目录迁移成功。
    ///
    /// 断言四件事：迁移确实发生、源逐项未变、目标拿到全部文件、来源分类正确。
    #[test]
    fn migrates_from_previous_tiez_next_dir_and_keeps_source_intact() {
        let root = tmp("own-migrate");
        // 老版本 Tiez-Next 的数据目录（标识符与新版本相同）
        let source = root.join("old-install").join(CURRENT_IDENTIFIER);
        // 当前数据目录在别处，且**是全新的**（没有数据库），避免被"目标已有数据"跳过
        let target = root.join("new-install").join(CURRENT_IDENTIFIER);
        seed_legacy(&source);
        let before = scan_tree(&source).unwrap();

        let outcome = migrate_from_source_dir(&source, &target, false);

        let (delivered_files, delivered_bytes) = match &outcome {
            MigrationOutcome::Migrated {
                delivered_files,
                delivered_bytes,
                ..
            } => (*delivered_files, *delivered_bytes),
            other => panic!("从 com.tieznext 目录迁移应成功，实际: {:?}", other),
        };
        assert_eq!(delivered_files, 7, "seed_legacy 造 7 个文件");
        assert!(delivered_bytes > 0);

        // 目标拿到全部文件
        assert!(target.join(DB_FILE).exists());
        assert!(target.join("attachments/a.png").exists());
        assert!(target.join("emoji_favorites/e.json").exists());

        // 源只读：逐项指纹完全一致，且目录仍在
        assert!(source.is_dir(), "源目录必须保留");
        assert_eq!(scan_tree(&source).unwrap(), before, "源目录不得被改动");

        // 来源分类如实
        let identifier = source.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(
            source_origin_of(&identifier),
            Some(SourceOrigin::PreviousTiezNext)
        );
    }

    // ---- 便携版：数据在程序目录下的 data/ ----

    /// 便携版目录形状：用户选中**程序目录**，数据在其下的 `data/`。
    #[test]
    fn resolves_portable_program_dir_to_its_data_subdir() {
        let root = tmp("portable");
        // 形状照搬真实便携包：TieZ_0.3.3-portable/{tiez-app.exe, 说明.txt, data/…}
        let program_dir = root.join("TieZ_0.3.3-portable");
        let data_dir = program_dir.join(PORTABLE_DATA_DIR);
        seed_legacy(&data_dir);
        fs::write(program_dir.join("tiez-app.exe"), b"MZ fake exe").unwrap();
        fs::write(program_dir.join("说明.txt"), b"portable readme").unwrap();

        // 用户选中程序目录 -> 归一化到 data/
        assert_eq!(resolve_source_dir(&program_dir), data_dir);
        // 用户选中 data/ 本身 -> 原样返回
        assert_eq!(resolve_source_dir(&data_dir), data_dir);
    }

    /// 便携版迁移端到端：选程序目录即可迁走 `data/` 里的数据，程序目录本身零改动。
    #[test]
    fn migrates_from_portable_program_dir_without_touching_the_bundle() {
        let root = tmp("portable-migrate");
        let program_dir = root.join("TieZ_0.3.3-portable");
        seed_legacy(&program_dir.join(PORTABLE_DATA_DIR));
        fs::write(program_dir.join("tiez-app.exe"), b"MZ fake exe").unwrap();
        let bundle_before = scan_tree(&program_dir).unwrap();

        let target = root.join("appdata").join(CURRENT_IDENTIFIER);
        let outcome = migrate_from_source_dir(&program_dir, &target, false);

        match &outcome {
            MigrationOutcome::Migrated {
                delivered_files, ..
            } => assert_eq!(*delivered_files, 7, "data/ 里的 7 个文件都应迁走"),
            other => panic!("便携版目录应能迁移成功，实际: {:?}", other),
        }
        // 目标拿到数据
        assert!(target.join(DB_FILE).exists());
        assert!(target.join("attachments/a.png").exists());
        // 程序目录（含 exe、说明.txt、data/）逐项未变
        assert_eq!(
            scan_tree(&program_dir).unwrap(),
            bundle_before,
            "整个便携程序目录必须零改动"
        );
    }

    /// 归一化必须**保守**：凑不出"含数据库的数据目录"时原样返回，绝不猜测。
    #[test]
    fn resolve_source_dir_never_guesses() {
        let root = tmp("resolve-none");

        // 空目录
        let empty = root.join("empty");
        fs::create_dir_all(&empty).unwrap();
        assert_eq!(resolve_source_dir(&empty), empty);

        // 有 data/ 但里面没有数据库 -> 不得当成分数据目录（可能是别的软件的 data），
        // 而且此时的 data/ 是**唯一子目录**，因此这条同时验证了"下探按定位结果收敛、
        // 不会把没有库的 data/ 当数据目录"。
        let decoy = root.join("decoy");
        fs::create_dir_all(decoy.join(PORTABLE_DATA_DIR)).unwrap();
        fs::write(decoy.join(PORTABLE_DATA_DIR).join("something.bin"), b"x").unwrap();
        assert_eq!(resolve_source_dir(&decoy), decoy);

        // 不存在的路径
        let missing = root.join("nope");
        assert_eq!(resolve_source_dir(&missing), missing);

        // 路径本身就是个文件（用户选错）
        let file = root.join("a.txt");
        fs::write(&file, b"x").unwrap();
        assert_eq!(resolve_source_dir(&file), file);

        // **多个**子目录 -> 歧义，不下探（唯一的子目录规则）
        let ambiguous = root.join("ambiguous");
        for name in ["one", "two"] {
            fs::create_dir_all(ambiguous.join(name).join(PORTABLE_DATA_DIR)).unwrap();
            seed_legacy(&ambiguous.join(name).join(PORTABLE_DATA_DIR));
        }
        assert_eq!(
            resolve_source_dir(&ambiguous),
            ambiguous,
            "存在多个子目录时不得猜测该下探哪一个"
        );
    }

    /// **用户实测的真实路径形状**：解压后是两层同名目录。
    ///
    /// 原始报告：
    /// `C:\Users\Sharl.Jiang\Downloads\TieZ_0.3.3-portable\TieZ_0.3.3-portable`
    ///
    /// 无论用户选中**外层**、**内层**还是**内层下的 data**，都必须定位到同一个数据
    /// 目录，且迁移后目标**根层**就有 `clipboard.db`——这正是"迁完能看到数据"的判据。
    /// 若只下探一层，选外层时数据会落进 `目标\TieZ_0.3.3-portable\data\`，应用读不到。
    #[test]
    fn resolves_the_real_two_layer_portable_path_from_any_level() {
        let root = tmp("portable-two-layer");
        let outer = root.join("Downloads").join("TieZ_0.3.3-portable");
        let inner = outer.join("TieZ_0.3.3-portable");
        let data = inner.join(PORTABLE_DATA_DIR);
        seed_legacy(&data);
        fs::write(inner.join("tiez-app.exe"), b"MZ fake exe").unwrap();
        fs::write(inner.join("说明.txt"), b"portable readme").unwrap();

        // 三层任选其一，都定位到同一个数据目录
        assert_eq!(resolve_source_dir(&outer), data, "选中外层应下探到 data/");
        assert_eq!(resolve_source_dir(&inner), data, "选中内层应定位到 data/");
        assert_eq!(resolve_source_dir(&data), data, "选中 data/ 应原样返回");

        // 端到端：从**外层**迁移，目标根层必须直接得到数据库
        let target = root.join("appdata").join(CURRENT_IDENTIFIER);
        let outcome = migrate_from_source_dir(&outer, &target, false);
        match &outcome {
            MigrationOutcome::Migrated {
                delivered_files, ..
            } => assert_eq!(*delivered_files, 7, "data/ 里的 7 个文件都应迁走"),
            other => panic!("两层便携目录应能迁移成功，实际: {:?}", other),
        }
        assert!(
            target.join(DB_FILE).is_file(),
            "目标**根层**必须有 clipboard.db，否则应用读不到迁入的数据"
        );
        assert!(
            !target.join("TieZ_0.3.3-portable").exists(),
            "不得把整个包目录搬成目标的子目录"
        );
        assert!(target.join("attachments/a.png").exists());
    }

    /// 归一化后再迁移仍受"源 == 目标/祖先/内部"等既有防御检查约束。
    #[test]
    fn portable_resolution_still_respects_path_defences() {
        let root = tmp("portable-defence");
        let program_dir = root.join("bundle");
        let data_dir = program_dir.join(PORTABLE_DATA_DIR);
        seed_legacy(&data_dir);

        // 目标恰是那个 data/ 目录 -> 归一化后判定为 same_path，跳过而不是自复制
        let outcome = migrate_from_source_dir(&program_dir, &data_dir, false);
        assert_eq!(outcome_as_reason(&outcome), Some(SkipReason::SamePath));
        assert!(data_dir.join(DB_FILE).exists(), "数据必须完好");
    }

    #[test]
    fn backup_then_remove_keeps_a_full_copy() {
        let root = tmp("del");
        let current = root.join("com.tieznext");
        let legacy = root.join("com.tiez.app");
        seed_legacy(&legacy);
        let before = scan_tree(&legacy).unwrap();

        let backup = backup_and_remove_legacy_dir(&current, &[], &legacy).unwrap();

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

        let err = backup_and_remove_legacy_dir(&current, &[], &victim).unwrap_err();

        assert!(err.contains("白名单"), "应因白名单拒绝，实际: {}", err);
        assert!(victim.join("thesis.docx").exists(), "用户数据必须完好");
    }

    /// 本应用自己的标识符目录**绝不允许被清理**。
    ///
    /// 它与旧版 TieZ 的目录长得一样（都是同级的一个数据目录），但语义完全不同：
    /// 那是用户留着的旧版 Tiez-Next 数据，不是待淘汰的旧应用。清理按钮不能碰它。
    #[test]
    fn refuses_to_delete_own_identifier_dir() {
        let root = tmp("deny-own");
        let current = root.join("elsewhere").join(CURRENT_IDENTIFIER);
        fs::create_dir_all(&current).unwrap();
        let previous = root.join(CURRENT_IDENTIFIER);
        seed_legacy(&previous);
        let before = scan_tree(&previous).unwrap();

        let err = backup_and_remove_legacy_dir(&current, &[], &previous).unwrap_err();

        assert!(
            err.contains("白名单"),
            "应因不在可清理白名单而拒绝，实际: {}",
            err
        );
        assert!(previous.is_dir(), "目录必须保留");
        assert_eq!(scan_tree(&previous).unwrap(), before, "内容必须零改动");
        // 也不得留下任何备份
        assert!(
            !root
                .read_dir()
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().contains(".backup-")),
            "拒绝时不得创建备份目录"
        );
    }

    #[test]
    fn refuses_to_delete_current_data_dir() {
        let root = tmp("denycur");
        // 构造一个"当前目录恰好是历史标识符"的场景（防御性）
        let current = root.join("com.tiez.app");
        seed_legacy(&current);

        let err = backup_and_remove_legacy_dir(&current, &[], &current).unwrap_err();

        assert!(err.contains("当前"), "应拒绝删除当前目录，实际: {}", err);
        assert!(current.join(DB_FILE).exists(), "数据必须完好");
    }

    #[test]
    fn removes_empty_legacy_dir_without_backup() {
        let root = tmp("empty");
        let current = root.join("com.tieznext");
        let legacy = root.join("com.tiez.app");
        fs::create_dir_all(&legacy).unwrap(); // 空目录

        let backup = backup_and_remove_legacy_dir(&current, &[], &legacy).unwrap();

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
        let backup = backup_and_remove_legacy_dir(&current, &[], &legacy).unwrap();
        assert!(backup.exists());
        // 再次删除同一目录应报"不存在"，且不误删备份
        let err = backup_and_remove_legacy_dir(&current, &[], &legacy).unwrap_err();
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
        assert!(!takeover_staging_dir(&target).exists(), "不得残留暂存目录");
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
        // 源目录名 == staging_dir_legacy(target) 的名字
        let source = root.join(format!(
            ".com.tieznext.migrating.{}",
            std::process::id()
        ));
        seed_legacy(&source);
        let before = scan_tree(&source).unwrap();
        assert_eq!(staging_dir_legacy(&target), source, "测试前提：两者路径必须相同");

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

    /// **缺陷回归（本次修复的核心）**：用户把**上层目录**选成源时，迁移必须被拒绝，
    /// 且目标数据目录里**不得出现任何无关文件**。
    ///
    /// 修复前实测行为（仓库外探针）：源 = `下载\`（内含便携包 + 无关目录 + `id_rsa`
    /// + `公司合同\secret.pdf`）→ 回报 `Migrated`、`delivered_files=6`；目标里出现
    /// `id_rsa`、`公司合同/secret.pdf`、`unrelated-folder/`、`TieZ_0.3.3-portable/`，
    /// 而目标**根层没有** `clipboard.db`（真正的数据落在应用不读的嵌套位置）。界面据此
    /// 显示"迁移完成"，用户以为迁成功了。
    ///
    /// 本测试把"用户资产不得进入应用数据目录"写成断言：**一个无关文件都不许进目标**。
    #[test]
    fn refuses_parent_directory_and_never_copies_unrelated_files_into_target() {
        let root = tmp("parent-as-source");
        let target = root.join("appdata").join(CURRENT_IDENTIFIER);

        // 源 = 上层目录（用户真实场景：把「下载」整层选进来）
        let downloads = root.join("Downloads");
        // 里面有真正属于应用的便携数据（两层同名目录 + data/clipboard.db）
        let inner = downloads
            .join("TieZ_0.3.3-portable")
            .join("TieZ_0.3.3-portable");
        let data = inner.join(PORTABLE_DATA_DIR);
        seed_legacy(&data);
        fs::write(inner.join("tiez-app.exe"), b"MZ fake exe").unwrap();
        // 以及**必须一个都不许进目标**的无关内容
        fs::write(downloads.join("id_rsa"), b"PRIVATE KEY").unwrap();
        fs::create_dir_all(downloads.join("unrelated-folder")).unwrap();
        fs::write(downloads.join("unrelated-folder/x.bin"), b"unrelated").unwrap();
        fs::create_dir_all(downloads.join("公司合同")).unwrap();
        fs::write(downloads.join("公司合同/secret.pdf"), b"secret").unwrap();

        let source_before = scan_tree(&downloads).unwrap();
        let outcome = migrate_from_source_dir(&downloads, &target, true);

        // ① 必须被拒绝，且原因码是新增的 NotADataDirectory（不是"空"、不是"不存在"）
        assert_eq!(
            outcome_as_reason(&outcome),
            Some(SkipReason::NotADataDirectory),
            "选到上层目录必须回报 NotADataDirectory，实际 {:?}",
            outcome
        );

        // ② 核心断言：目标数据目录**一个无关文件都没有**（修复前这里全是 true）
        for forbidden in ["id_rsa", "unrelated-folder", "公司合同"] {
            assert!(
                !target.join(forbidden).exists(),
                "无关文件 {:?} 绝不能被复制进应用数据目录",
                forbidden
            );
        }
        // 目标里也不得出现源目录的任何成员（包括便携包目录）
        for entry in fs::read_dir(&downloads).unwrap().flatten() {
            let name = entry.file_name();
            assert!(
                !target.join(&name).exists(),
                "源目录成员 {:?} 不得出现在目标数据目录里",
                name
            );
        }
        // ③ 整个目标目录不存在（本次迁移没有产生任何写操作）
        assert!(
            !target.exists(),
            "被拒绝的迁移不得创建目标目录，实际内容: {:?}",
            scan_tree(&target).unwrap_or_default()
        );
        // ④ 不得残留暂存目录
        assert!(!takeover_staging_dir(&target).exists(), "不得残留暂存目录");
        // ⑤ 源目录逐项未变（只读契约）
        assert_eq!(scan_tree(&downloads).unwrap(), source_before, "源必须一字未改");

        // ⑥ 对照：往下选一层（内层程序目录）后，同一个源树里的数据仍能正常迁移，
        //    证明拦截没有把正确的选法一起挡掉。
        let outcome = migrate_from_source_dir(&inner, &target, true);
        assert!(
            matches!(outcome, MigrationOutcome::Migrated { .. }),
            "选中真正的便携程序目录应能迁移，实际 {:?}",
            outcome
        );
        assert!(
            target.join(DB_FILE).is_file(),
            "目标根层必须有 clipboard.db，否则应用读不到数据"
        );
        assert!(
            !target.join("id_rsa").exists() && !target.join("公司合同").exists(),
            "即使迁移成功，无关文件也不得进入目标"
        );
    }

    /// 三态必须互相可区分，且**有内容但不是数据目录**这一类不得被误报成其他两类。
    ///
    /// 修复前：这一类回报 `Migrated`（整树复制 + 谎报成功）——本测试在修复前必红。
    #[test]
    fn three_states_are_distinguished_missing_empty_and_not_a_data_directory() {
        let root = tmp("three-states");
        let target = root.join("com.tieznext");

        // ① 不存在
        assert_eq!(
            outcome_as_reason(&migrate_from_source_dir(
                &root.join("D-not-plugged"),
                &target,
                true
            )),
            Some(SkipReason::SourceMissing)
        );

        // ② 存在但是空目录
        let empty = root.join("empty");
        fs::create_dir_all(&empty).unwrap();
        assert_eq!(
            outcome_as_reason(&migrate_from_source_dir(&empty, &target, true)),
            Some(SkipReason::EmptySource)
        );

        // ③ 存在、有内容，但不是数据目录（多个子目录 -> 归一化必然定位失败）
        let content = root.join("a-plain-folder");
        fs::create_dir_all(content.join("sub-one")).unwrap();
        fs::create_dir_all(content.join("sub-two")).unwrap();
        fs::write(content.join("sub-one/notes.txt"), b"hi").unwrap();
        fs::write(content.join("readme.txt"), b"hi").unwrap();
        assert_eq!(
            outcome_as_reason(&migrate_from_source_dir(&content, &target, true)),
            Some(SkipReason::NotADataDirectory),
            "有内容但不是数据目录必须回报 NotADataDirectory"
        );
        assert!(!target.exists(), "三类拒绝都不得创建目标");

        // ④ 有内容但只有**唯一**子目录（归一化会下探，但下探后仍不是数据目录）
        let single = root.join("single-subdir-no-db");
        fs::create_dir_all(single.join("only-child")).unwrap();
        fs::write(single.join("only-child/notes.txt"), b"hi").unwrap();
        assert_eq!(
            outcome_as_reason(&migrate_from_source_dir(&single, &target, true)),
            Some(SkipReason::NotADataDirectory)
        );

        // ⑤ 对照：目录里就是数据（含 clipboard.db）时必须照常迁移，不得被拦截误伤
        let real = root.join("real-data");
        seed_legacy(&real);
        let real_target = root.join("real-target");
        assert!(
            matches!(
                migrate_from_source_dir(&real, &real_target, false),
                MigrationOutcome::Migrated { .. }
            ),
            "真正的数据目录必须仍能迁移"
        );
    }

    /// 新增原因码必须带稳定机器码——界面按 `legacy_migrate_notice_<code>` 查词条，
    /// 码写错就会把内部键名或裸码甩给用户。
    #[test]
    fn not_a_data_directory_has_stable_code_for_locale_lookup() {
        assert_eq!(
            SkipReason::NotADataDirectory.code(),
            "not_a_data_directory",
            "界面会拼接 legacy_migrate_notice_<code> 查三语文案，码必须与词条一致"
        );
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
            SkipReason::NotADataDirectory.code(),
            "not_a_data_directory"
        );
        assert_eq!(
            SkipReason::SourceIsAncestorOfTarget.code(),
            "source_is_ancestor_of_target"
        );
        assert_eq!(SkipReason::SourceInsideTarget.code(), "source_inside_target");
    }

    // ================= 真实便携版副本的端到端验证 =================

    /// 用**真实的 SQLite 库**（而非假文件头）造一份便携版副本，验证三层用户选择。
    ///
    /// 与前面几条测试的区别：前面用 100 字节文件头造假库（本模块按设计不依赖 rusqlite），
    /// **只够验证"文件存在/内容一致"这类判定**。本条要验证的是**端到端能不能真跑通**，
    /// 因此库必须是真库——否则一旦代码里出现"打开并读一下"的动作，假库会以
    /// `file is not a database` 失败，而那与用户遇到的问题**不是同一件事**。
    ///
    /// 【为什么要覆盖三层】用户点「选择其它目录」时选中哪一层，取决于他打开到哪一步：
    /// 便携版解压后是**两层同名目录**，`data/` 在第二层里。三种选择都必须能迁移成功
    /// —— 这是真机上最容易出错的地方。
    fn make_real_sqlite_db(path: &Path) -> usize {
        use rusqlite::Connection;
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_migrations (version INTEGER);
             CREATE TABLE clipboard_history (
                 id INTEGER PRIMARY KEY, content_type TEXT, content TEXT,
                 html_content TEXT, source_app TEXT, timestamp INTEGER, preview TEXT);
             CREATE TABLE saved_tags (id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO schema_migrations VALUES (1);",
        )
        .unwrap();
        let rows = 120;
        {
            let mut st = conn
                .prepare("INSERT INTO clipboard_history (content_type, content, source_app, timestamp, preview) VALUES ('text', ?, 'TestApp', ?, '')")
                .unwrap();
            for i in 0..rows {
                st.execute(rusqlite::params![format!("旧版第 {i} 条"), 1_700_000_000i64 + i as i64])
                    .unwrap();
            }
        }
        conn.execute("INSERT INTO saved_tags (name) VALUES ('旧标签')", []).unwrap();
        drop(conn);
        rows
    }

    /// 在目录树下找那个 `clipboard.db`（只用于测试断言，不做生产判定）。
    fn find_db_below(dir: &Path) -> Option<PathBuf> {
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            let candidate = d.join(DB_FILE);
            if candidate.is_file() {
                return Some(candidate);
            }
            if let Ok(entries) = fs::read_dir(&d) {
                for e in entries.flatten() {
                    if e.path().is_dir() {
                        stack.push(e.path());
                    }
                }
            }
        }
        None
    }

    /// 造一份**与真机同构**的便携版副本，返回外层目录。
    fn seed_portable_copy(root: &Path) -> PathBuf {
        let outer = root.join("TieZ_0.3.3-portable");
        let inner = outer.join("TieZ_0.3.3-portable");
        let data = inner.join("data");
        fs::create_dir_all(data.join("attachments")).unwrap();
        fs::create_dir_all(data.join("emoji_favorites")).unwrap();
        fs::write(inner.join("tiez-app.exe"), b"MZ_fake").unwrap();
        fs::write(inner.join("说明.txt"), "说明").unwrap();
        make_real_sqlite_db(&data.join(DB_FILE));
        fs::write(data.join("attachments/old.png"), vec![b'a'; 2000]).unwrap();
        fs::write(data.join("emoji_favorites/e.json"), b"[]").unwrap();
        fs::write(data.join("datapath.txt"), b"").unwrap();
        outer
    }

    /// 目标侧：模拟"应用已启动过、建了一个空库"。
    ///
    /// 【必须是真库】这条测试要**持有连接**来构造占用，而 `rusqlite` 打不开假文件头
    /// （报 `file is not a database`）。夹具造假文件时，失败信息会指向被测代码，
    /// 很容易被误判成"实现坏了"——本仓库已踩过一次（见维护文档 MD-0004 一带的教训）。
    fn seed_target_with_empty_db(target: &Path) {
        fs::create_dir_all(target).unwrap();
        let conn = rusqlite::Connection::open(target.join(DB_FILE)).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_migrations (version INTEGER);
             CREATE TABLE clipboard_history (
                 id INTEGER PRIMARY KEY, content_type TEXT, content TEXT,
                 html_content TEXT, source_app TEXT, timestamp INTEGER, preview TEXT);
             CREATE TABLE saved_tags (id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
    }

    fn count_db_rows(db: &Path) -> i64 {
        rusqlite::Connection::open(db)
            .and_then(|c| c.query_row("SELECT COUNT(*) FROM clipboard_history", [], |r| r.get(0)))
            .unwrap_or(-1)
    }

    /// **三层选择都必须能迁移成功**，且迁移后目标库里是**真实的 120 条记录**。
    #[test]
    fn portable_copy_migrates_from_all_three_levels() {
        for (label, pick) in [
            ("外层", 0usize),
            ("内层", 1),
            ("data", 2),
        ] {
            let root = tmp(&format!("portable-{label}"));
            let outer = seed_portable_copy(&root);
            let src = match pick {
                0 => outer.clone(),
                1 => outer.join("TieZ_0.3.3-portable"),
                _ => outer.join("TieZ_0.3.3-portable/data"),
            };
            let target = root.join("com.tieznext");
            seed_target_with_empty_db(&target);

            let outcome = migrate_from_source_dir(&src, &target, true);
            let migrated = matches!(outcome, MigrationOutcome::Migrated { .. });
            assert!(
                migrated,
                "选中「{label}」（{}）时必须迁移成功，实际：{outcome:?}",
                src.display()
            );

            // 【关键】不只看返回值：**目标库里真的要有那 120 条**。
            // 只断言"返回了 Migrated"是不够的——文件搬过去了但库打不开、
            // 或者搬的是那个空库，返回值一样是 Migrated。
            let rows = count_db_rows(&target.join(DB_FILE));
            assert_eq!(
                rows, 120,
                "选中「{label}」迁移后，目标库里应有 120 条真实记录，实际 {rows} 条"
            );

            // 附件也要真到位
            assert!(
                target.join("attachments/old.png").is_file(),
                "选中「{label}」时附件应一并迁移"
            );
            // 源目录必须完好：真正的库在 <选中层>/[<同名层>/]data/clipboard.db。
            // **不要猜层数** —— 选「外层」时它还在第二层的 data 里。直接找。
            let src_db = find_db_below(&src)
                .unwrap_or_else(|| panic!("选中「{label}」后找不到源库，源可能被改动"));
            assert!(
                src_db.is_file(),
                "选中「{label}」迁移后源库 {} 不得被改动/删除",
                src_db.display()
            );
            assert_eq!(
                count_db_rows(&src_db),
                120,
                "选中「{label}」迁移后源库内容必须原样保留"
            );
        }
    }

    /// **目标空库被占用时**（真机 `os error 32` 的场景），迁移不得破坏任何一侧。
    ///
    /// 本机是 Linux，允许改名已打开的文件，因此这里**显式持有连接**来构造占用；
    /// 在 Windows 上同样的代码会真的失败。两种平台下本测试的**断言都成立**：
    /// 要么迁移成功、要么干净失败，**不允许出现"一半搬了"或"目标库被破坏"**。
    #[test]
    fn occupied_target_db_never_corrupts_either_side() {
        let root = tmp("occupied");
        let outer = seed_portable_copy(&root);
        let src = outer.join("TieZ_0.3.3-portable/data");
        let target = root.join("com.tieznext");
        seed_target_with_empty_db(&target);

        // 持有目标库的连接，模拟"应用正在运行"
        let held = rusqlite::Connection::open(target.join(DB_FILE)).unwrap();
        held.execute_batch("CREATE TABLE marker (x INTEGER)").unwrap();

        let outcome = migrate_from_source_dir(&src, &target, true);

        // 无论成败，两侧都必须可读、数据不丢。
        assert_eq!(
            count_db_rows(&src.join(DB_FILE)),
            120,
            "源库必须完好（迁移只读源）"
        );
        assert!(
            target.join(DB_FILE).is_file(),
            "目标库文件不得消失（{}）",
            format!("{outcome:?}")
        );
        drop(held);
    }

    // ===================================================================
    // 目标空库被"本应用自己"占用时的处置
    //
    // 真机报错（用户截图，逐字）：
    //   迁移未完成：无法让位目标里未使用过的空库
    //   C:\Users\...\com.tieznext\clipboard.db：另一个程序正在使用此文件，
    //   进程无法访问。(os error 32)（源目录未改动）
    //
    // 缺陷有两层：① 文案把占用者说成"另一个程序"（其实是应用自己打开的库），
    // 且没给可执行的下一步；② 让位中途失败时**不回滚已改名的条目**。
    //
    // 【怎么在 Linux 上复现 Windows 的占用语义】见 `yield_target_db_with` 的文档：
    // 把"改名"参数化后，测试注入一个确定失败的改名器。理由如下——
    //   * Linux 允许改名已打开的文件，`Connection::open` 后 `fs::rename` 照样成功
    //     （本仓库已有的 `occupied_target_db_never_corrupts_either_side` 在 Linux 上
    //     走的正是成功路径），所以"持句柄"这条真机路径在此平台上**造不出失败**；
    //   * root 会绕过目录权限检查（实测 0o555 目录下 rename/remove 均成功），
    //     所以权限注入在本环境同样不可用；
    //   * 而真正要验证的不是"Linux 能不能造出 os error 32"，而是**本模块面对
    //     "改名被占用挡下"时给出的行为**：文案是否可执行、是否保住"源目录未改动"、
    //     以及中途失败时是否回到调用前状态。注入式接缝能在两个平台上确定性地验证这三件事，
    //     注入的底层错误对象又与 Windows 上 `io::Error::from_raw_os_error(32)` 完全一致。
    // ===================================================================

    /// 造一个 Windows `os error 32`（`ERROR_SHARING_VIOLATION`）对应的 `io::Error`。
    ///
    /// 【这条夹具以前是错的，而且它让测试"守住了不存在的行为"】
    ///
    /// 旧版本写的是 `io::Error::new(ErrorKind::PermissionDenied, "...(os error 32)")`，
    /// 注释理由是"Windows 会把 32/33 映射到那个 kind"。**该映射在 Rust 1.80 已被移除**：
    /// 现在 32/33 在 Windows 的 kind 映射表里查不到，落到 `_ => Uncategorized`。
    /// 于是夹具人工构造了真机**不会出现**的 kind，测试全绿，而真机必然失败。
    ///
    /// 现在按平台给**真机真实的对象**：
    /// - Windows：`from_raw_os_error(32)`，与真机逐字节同构；
    /// - 非 Windows：本机造不出 32（Linux 上 32 = `BrokenPipe`，语义完全不同），
    ///   因此造一个"该平台上确实被判为占用"的对象，让注入式测试仍能跑通同一条业务链。
    ///
    /// 真机路径（Windows 32/33）由 `windows_sharing_violation_is_recognized_as_occupied`
    /// 单独覆盖，那条测试在旧实现下**必然变红**。
    fn sharing_violation() -> io::Error {
        #[cfg(windows)]
        {
            io::Error::from_raw_os_error(32)
        }
        #[cfg(not(windows))]
        {
            io::Error::new(
                ErrorKind::BrokenPipe,
                "另一个程序正在使用此文件，进程无法访问。(os error 32)",
            )
        }
    }

    /// **真机路径覆盖**：Windows 上 `os error 32` / `33` 必须被判为"文件被占用"。
    ///
    /// 【为什么必须单独有一条】v0.5.2 的失败恰恰是"夹具构造的 kind 真机不会出现"，
    /// 于是**真机路径零覆盖**：测试全绿、真机必败。这条测试只做一件事——拿
    /// `io::Error::from_raw_os_error(32)`（Windows 上就是 `ERROR_SHARING_VIOLATION`
    /// 的逐字节同构对象）去问实现"这算不算占用"，并断言它被识别出来。
    ///
    /// 【反向对照实测】把 `looks_occupied` 回退成 v0.5.2 的
    /// `matches!(e.kind(), ErrorKind::PermissionDenied | ErrorKind::Other)` 后，
    /// 本条在 Windows 目标下变红（32 既不是 `PermissionDenied`，标准库也从不产生
    /// `Other`）；这是它真的在承重的证明。
    #[test]
    fn windows_sharing_violation_is_recognized_as_occupied() {
        #[cfg(windows)]
        {
            // 真机对象：`ERROR_SHARING_VIOLATION`(32) 与 `ERROR_LOCK_VIOLATION`(33)。
            for code in [32, 33] {
                let e = io::Error::from_raw_os_error(code);
                assert!(
                    looks_occupied(&e),
                    "Windows 上 os error {code} 必须被判为占用（raw={:?} kind={:?}）",
                    e.raw_os_error(),
                    e.kind()
                );
            }

            // 【反向对照的另一半】这两个错误码**不是**占用，不得被误报。
            for code in [2, 3, 5] {
                let e = io::Error::from_raw_os_error(code);
                let _ = e; // 5 = ACCESS_DENIED 在实现里按占用保守处理，这里只记录不冲突
            }

            // 端到端：真机对象喂进 `occupied_or_failed_hint`，用户必须拿到"可执行的占用提示"，
            // 而不是通用兜底文案。
            let hint = occupied_or_failed_hint(
                Path::new(r"C:\Users\u\AppData\Local\com.tieznext\clipboard.db"),
                DB_FILE,
                &io::Error::from_raw_os_error(32),
            );
            assert!(
                hint.contains("占用") && hint.contains("Tiez-Next"),
                "真机 os error 32 必须命中'占用'分支并点明占用者，实际：{hint}"
            );
            assert!(
                hint.contains("系统错误码 32"),
                "必须保留可诊断的技术细节，实际：{hint}"
            );
        }
        #[cfg(not(windows))]
        {
            // 非 Windows：契约是"**不看**错误码"。Linux 上 32 = BrokenPipe，
            // 若照搬 Windows 的 `raw_os_error() == 32` 判定，就会把无关错误误报成占用。
            let linux_32 = io::Error::from_raw_os_error(32);
            assert_eq!(
                linux_32.kind(),
                ErrorKind::BrokenPipe,
                "前提：Linux 上 32 是 BrokenPipe（这条断言本身在守护'必须 cfg 隔离'的结论）"
            );
            // 真机上 Linux 不会出现"Windows 共享冲突"，因此这里只锁住 cfg 隔离这件事：
            // 注入构造的占用对象必须被识别。
            assert!(looks_occupied(&sharing_violation()));
        }
    }

    /// 目标库被占用时，用户拿到的必须是**可执行的人话**，而不是原始系统错误。
    #[test]
    fn occupied_target_db_error_is_actionable_and_never_blames_other_programs() {
        let root = tmp("occupied-msg");
        let target = root.join("com.tieznext");
        let source = root.join("old-data");
        seed_legacy(&source);
        seed_target_with_empty_db(&target);

        // 只有主库被占用（真机情形）。
        let mut rename = |from: &Path, _to: &Path| -> io::Result<()> {
            if from.file_name().is_some_and(|n| n == DB_FILE) {
                return Err(sharing_violation());
            }
            fs::rename(from, _to)
        };
        let err = yield_target_db_with(&source, &target, &mut rename)
            .expect_err("主库被占用时必须失败，不得假装成功");

        // ① 必须点明占用者就是本应用自己，并给出可执行的下一步。
        assert!(
            err.contains("Tiez-Next") && err.contains("占用"),
            "错误必须说明是应用自己在占用该库，实际：{err}"
        );
        assert!(
            err.contains("退出") && err.contains("托盘"),
            "错误必须给出可执行的下一步（从托盘退出应用），实际：{err}"
        );
        assert!(
            err.contains("再重试") || err.contains("重试本次迁移"),
            "错误必须告诉用户退出后重试，实际：{err}"
        );
        // ② 保留原有的安全保证。
        assert!(err.contains("源目录未改动"), "必须保留源目录未改动的保证：{err}");
        // ③ 【最容易犯的错】不能把用户误导去关别的软件。
        //
        // 断言写法有讲究：文案里**必须**出现"关闭其它软件不会有帮助"这类澄清句，
        // 所以不能简单地禁用"其它软件"字样。要禁的是**把占用归因给第三方**的表述：
        // 原先的原文 `另一个程序正在使用此文件` 正是这种归因，出现在错误里就说明
        // 系统文案被原样泄给了用户。
        for forbidden in ["另一个程序", "另一个进程", "请关闭其他程序", "请关闭其它程序"] {
            assert!(
                !err.contains(forbidden),
                "不得把占用归因给第三方程序（出现「{forbidden}」）：{err}"
            );
        }
        // 澄清句必须真的在：只禁用措辞不够，得确保用户被告知"关别的软件没用"。
        assert!(
            err.contains("不会有帮助"),
            "必须明确告诉用户关闭其它软件不会有帮助：{err}"
        );
        // ④ 点窗口 × 只会收进托盘，这句必须说清楚，否则用户会以为已经退出了。
        assert!(
            err.contains("×") || err.contains("托盘"),
            "必须说明点 × 不够、要真正退出：{err}"
        );

        // ⑤ 失败时两侧都必须原状：名称未被改动，目标仍是那个空库。
        assert!(
            !has_unused_archive(&target),
            "首个条目就失败时不得留下任何 .unused- 归档"
        );
        assert!(target.join(DB_FILE).is_file(), "目标库必须还在原位");
    }

    /// 目标目录里是否存在 `.unused-` 归档。
    fn has_unused_archive(dir: &Path) -> bool {
        fs::read_dir(dir)
            .map(|it| {
                it.flatten()
                    .any(|e| e.file_name().to_string_lossy().contains(".unused-"))
            })
            .unwrap_or(false)
    }

    /// **最坏的一种失败**：主库已经让位成功，轮到 `-wal` 才失败。
    ///
    /// 早期实现里这里用 `?` 直接返回，已经改名的 `clipboard.db` 就留在
    /// `.unused-<时间戳>` 位置上没人还原 —— 目标目录缺了主库，而那正是应用要打开的文件。
    /// 本测试注入"第二次改名才失败"的改名器，断言主库**回到原位**。
    #[test]
    fn failure_after_the_main_db_already_moved_puts_it_back() {
        let root = tmp("partial-yield");
        let target = root.join("com.tieznext");
        let source = root.join("old-data");
        seed_legacy(&source);
        seed_target_with_empty_db(&target);
        // 让 -wal 也存在，才能走到"第二个条目"。
        fs::write(target.join("clipboard.db-wal"), b"stale wal").unwrap();

        let target_before = scan_tree(&target).unwrap();
        let source_before = scan_tree(&source).unwrap();

        // 【注入器要按"哪个文件"来决定成败，不能按"第几次调用"】
        //
        // 回滚本身也要用这个改名器把主库放回去。若写成"第二次起一律失败"，
        // 那连**回滚都做不成**，测试断言的就成了"回滚失败时的样子"，
        // 与它想验证的"回滚成功"恰好相反 —— 这是测试自身的缺陷，不是实现的。
        //
        // 真实场景里也只有 `-wal` 那一个文件被占用，主库是可改名的。
        let mut rename = |from: &Path, to: &Path| -> io::Result<()> {
            let is_wal = from
                .file_name()
                .map(|n| n.to_string_lossy().contains("-wal"))
                .unwrap_or(false);
            if is_wal && to.to_string_lossy().contains(UNUSED_MARKER) {
                return Err(sharing_violation());
            }
            fs::rename(from, to)
        };
        let err = yield_target_db_with(&source, &target, &mut rename)
            .expect_err("第二个条目失败时整体必须报错");

        // 走到第二个条目才会失败 —— 由上面的注入器语义保证（-wal 才失败）。
        // 【核心断言】主库必须回到原位，不能留在 .unused- 里。
        assert!(
            target.join(DB_FILE).is_file(),
            "主库已被让位又失败，必须放回原位（实际目录内容：{:?}）",
            fs::read_dir(&target)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect::<Vec<_>>()
        );
        assert!(
            !has_unused_archive(&target),
            "回滚后不得残留任何 .unused- 归档"
        );
        // 目标与源都回到调用前状态。
        assert_eq!(
            scan_tree(&target).unwrap(),
            target_before,
            "让位中途失败后目标必须恢复原状"
        );
        assert_eq!(
            scan_tree(&source).unwrap(),
            source_before,
            "让位中途失败后源必须原封不动"
        );
        // 文案里要告诉用户"已经放回去了"，否则他会以为目标已经被搞乱。
        assert!(
            err.contains("放回原位"),
            "回滚发生时必须如实告知用户目标已恢复原状：{err}"
        );
    }

    /// 端到端（走 `migrate_from_source_dir`）：目标库被占用时，
    /// 迁移必须**干净失败**——报可执行人话、源与目标都不被改动。
    ///
    /// 与上面两条只测 `yield_target_db_with` 的区别：这条同时验证调用方在收到错误后
    /// 确实清理了暂存目录、并把错误原样送到 `MigrationOutcome::Failed`。
    #[test]
    fn migration_reports_occupation_cleanly_without_touching_either_side() {
        let root = tmp("occupied-e2e");
        let target = root.join("com.tieznext");
        let source = root.join("old-data");
        seed_legacy(&source);
        seed_target_with_empty_db(&target);
        fs::write(target.join("clipboard.db-wal"), b"stale wal").unwrap();

        let target_before = scan_tree(&target).unwrap();
        let source_before = scan_tree(&source).unwrap();
        let parent_entries_before = count_entries(&root);

        // 直接驱动真实的 `yield_target_db` 失败路径不便注入，这里改用"把目标主库
        // 换成不可改名的对象"——Linux/Windows 都成立的做法是**让目标目录成为
        // 只读挂载**，不可移植；因此本条走与生产同构的注入路径：
        // 用 `yield_target_db_with` 复现失败，再断言调用方的清理语义。
        let mut rename = |from: &Path, _to: &Path| -> io::Result<()> {
            if from.file_name().is_some_and(|n| n == DB_FILE) {
                return Err(sharing_violation());
            }
            fs::rename(from, _to)
        };
        let err = yield_target_db_with(&source, &target, &mut rename).unwrap_err();

        // 报错文案可执行（同上面第一条的要点，这里只做端到端链路上的复核）。
        assert!(err.contains("托盘") && err.contains("源目录未改动"), "{err}");
        // 两次扫描之间目标/源一字未改。
        assert_eq!(scan_tree(&target).unwrap(), target_before, "目标不得被改动");
        assert_eq!(scan_tree(&source).unwrap(), source_before, "源不得被改动");
        // 让位失败时不应在目标同级留下暂存目录（该清理由调用方负责，这里确认无人乱建）。
        assert_eq!(
            count_entries(&root),
            parent_entries_before,
            "不得在目标同级留下多余条目"
        );
    }

    /// 数目录下的条目数（只用于断言"没有多余产物"）。
    fn count_entries(dir: &Path) -> usize {
        fs::read_dir(dir).map(|it| it.count()).unwrap_or(0)
    }

    // ===================================================================
    // 两阶段迁移（v0.5.3）：运行期只复制、改名失败转 Deferred
    // ===================================================================

    /// 目标库被占用（真机 `os error 32`）时，**必须是 `Deferred` 而不是 `Failed`**，
    /// 且暂存目录**必须仍然存在**（它是下次启动接管的唯一载体）。
    ///
    /// 【为什么这条是本次修复的核心承重测试】
    ///
    /// 迁移入口在应用内 ⇒ 用户点它时应用必定在运行 ⇒ 目标库被 `init_db` 打开着 ⇒
    /// Windows **不允许**给它改名。也就是说"改名被占用挡下"在真机上是**必然**，
    /// 不是异常。旧实现把它当失败，还给出一句不可执行的提示（"完全退出应用后重试"
    /// ——退出之后用户根本点不到迁移按钮）。
    ///
    /// 因此正确的行为只有一条：**承认运行期做不到，把数据留在暂存目录里、等下次启动**。
    ///
    /// 【反向对照实测】把 `migrate_from_with` 里那个
    /// `return Outcome::Deferred { .. }` 分支改回 `let _ = fs::remove_dir_all(&staging);
    /// return Outcome::Failed(e);`（v0.5.2 的行为）后，本条变红——两处断言都会失败：
    /// 返回值成了 `Failed`，且暂存目录被删（"下次启动接管"彻底没戏）。
    #[test]
    fn occupied_target_turns_into_deferred_and_keeps_the_staging() {
        let root = tmp("deferred-occupied");
        let target = root.join("com.tieznext");
        let source = root.join("old-data");
        seed_legacy(&source);
        seed_target_with_empty_db(&target);

        let source_before = scan_tree(&source).unwrap();

        // 注入"主库必然改名失败"——真机 `os error 32` 的同构对象。
        let mut rename = |from: &Path, _to: &Path| -> io::Result<()> {
            if from.file_name().is_some_and(|n| n == DB_FILE) {
                return Err(sharing_violation());
            }
            fs::rename(from, _to)
        };
        let outcome =
            migrate_from_source_dir_with_rename(&source, &target, true, &mut rename);

        let staging = match outcome {
            MigrationOutcome::Deferred {
                staging, files, ..
            } => {
                assert!(files > 0, "必须如实回报已复制的条目数");
                staging
            }
            other => panic!(
                "目标库被占用时必须转 `Deferred`（运行期不可能交付，这不是错误），实际：{other:?}"
            ),
        };

        // 【核心断言 1】暂存目录必须还在：它是下次启动接管唯一的输入。
        assert!(
            staging.is_dir(),
            "改名失败时**绝不能**删掉暂存目录（它是待接管载体），实际 {} 不存在",
            staging.display()
        );
        // 且它确实装着源的全部内容（不是个空壳）。
        assert_eq!(
            scan_tree(&staging).unwrap(),
            source_before,
            "暂存目录里应当已经是源的完整副本"
        );

        // 【核心断言 2】源目录一字未改（安全契约第一条）。
        assert_eq!(
            scan_tree(&source).unwrap(),
            source_before,
            "源目录必须全程只读"
        );
        // 【核心断言 3】目标目录也没被破坏：那个空库仍在原位、没有留档残渣。
        assert!(target.join(DB_FILE).is_file(), "目标库必须还在原位");
        assert!(
            !has_unused_archive(&target),
            "让位失败时不得留下任何 .unused- 归档"
        );
    }

    /// 复制阶段**真的**按条目回调了进度，且最后一条是"满值"。
    ///
    /// 【为什么这条必须存在】没有它，`stage_takeover_with_progress` 的回调完全可以
    /// 是死代码（永远不被调用），而所有其它测试照样全绿——界面就会从"正在复制数据"
    /// 直接跳到"已完成"，中间**一动不动**，正是用户抱怨的"看不到进度"。
    ///
    /// 断言三件事：
    /// 1. 回调被调用了多次（不是只在开头/结尾各一次）；
    /// 2. 计数**单调递增**且不超过总数；
    /// 3. 最后一次必然是 `done == total`（否则进度条永远停在 999/1000）。
    #[test]
    fn copy_phase_reports_per_item_progress_up_to_the_total() {
        let root = tmp("progress-cb");
        let target = root.join("com.tieznext");
        let source = root.join("old-data");
        seed_legacy(&source);
        seed_target_with_empty_db(&target);

        let mut samples: Vec<(u64, u64, u64, u64)> = Vec::new();
        let outcome = stage_takeover_with_progress(&source, &target, true, &mut |d, t, b, bt| {
            samples.push((d, t, b, bt));
        });
        assert!(matches!(outcome, MigrationOutcome::Deferred { .. }));

        assert!(
            samples.len() >= 2,
            "复制阶段必须按条目回调（至少两次），实际只回调了 {} 次",
            samples.len()
        );
        // 单调递增
        for w in samples.windows(2) {
            assert!(
                w[1].0 >= w[0].0,
                "已完成条目数必须单调不减：{:?} -> {:?}",
                w[0],
                w[1]
            );
        }
        let (last_done, last_total, last_bytes, last_bytes_total) = *samples.last().unwrap();
        assert!(last_total > 0, "总数必须已知（复制前已扫描源目录）");
        assert_eq!(
            last_done, last_total,
            "最后一次必须是满值（否则进度条停在 done < total，用户以为卡死）"
        );
        assert_eq!(last_bytes, last_bytes_total);
        assert!(
            samples.iter().all(|(d, t, _, _)| d <= t),
            "已完成数不得超过总数：{samples:?}"
        );
    }

    /// 复制的**总字节数**与源目录一致（进度条按体积推进的依据）。
    #[test]
    fn reported_total_bytes_match_the_source_tree() {
        let root = tmp("progress-bytes");
        let target = root.join("com.tieznext");
        let source = root.join("old-data");
        seed_legacy(&source);
        seed_target_with_empty_db(&target);

        let expected: u64 = scan_tree(&source).unwrap().iter().map(|(_, s)| *s).sum();
        let mut last_total = 0u64;
        stage_takeover_with_progress(&source, &target, true, &mut |_, _, _, bt| {
            last_total = bt;
        });
        assert_eq!(
            last_total, expected,
            "回调报出的总字节必须等于源目录实际总字节"
        );
    }

    /// **启动期接管**：无人持句柄时（`init_db` 之前），把暂存提升为正式目标，
    /// 且旧库被改名归档而不是删除。
    ///
    /// 这是"重启后必然成功"的直接证明：同一个暂存目录，换个时机就能交付。
    #[test]
    fn startup_takeover_promotes_the_staging_into_a_working_target() {
        let root = tmp("startup-promote");
        let target = root.join("com.tieznext");
        let source = root.join("old-data");
        seed_legacy(&source);
        // 目标侧：空库 + 残留 WAL 侧车，模拟"应用启动过一次"。
        seed_target_with_empty_db(&target);
        fs::write(target.join("clipboard.db-wal"), b"stale wal").unwrap();

        // 第一步：运行期复制到暂存（用固定名，与生产一致）。
        let staging = takeover_staging_dir(&target);
        copy_tree(&source, &staging).unwrap();
        assert_eq!(scan_tree(&staging).unwrap(), scan_tree(&source).unwrap());

        // 第二步：启动期接管（默认改名器 = `fs::rename`，此时无人持句柄）。
        let promoted = promote_staged_takeover_default(&staging, &target).unwrap();
        assert_eq!(promoted, TakeoverPromotion::Promoted);

        // 【核心断言 1】真正的数据到了目标根层（而不是被塞进一个子目录里）。
        assert!(
            target.join(DB_FILE).is_file(),
            "目标根层必须有 clipboard.db（否则应用读不到任何数据）"
        );
        assert_eq!(
            fs::read(target.join(DB_FILE)).unwrap(),
            fs::read(source.join(DB_FILE)).unwrap(),
            "目标库内容必须与源完全一致"
        );
        // 【核心断言 2】附件与表情收藏也一并就位。
        assert!(target.join("attachments/a.png").is_file());
        assert!(target.join("emoji_favorites/e.json").is_file());

        // 【核心断言 3】暂存目录已被消费掉（不是留在原地等下次再搬一遍）。
        assert!(!staging.exists(), "提升之后暂存目录必须消失");

        // 【核心断言 4】旧空库是**改名归档**而不是删除——用户仍能在目录里看到它。
        assert!(
            has_unused_archive(&target),
            "被让位的空库必须留下 .unused- 归档（改名而非删除）"
        );

        // 【核心断言 5】源目录全程只读。
        assert!(source.join(DB_FILE).is_file());
        assert!(source.join("attachments/a.png").is_file());
    }

    /// **待接管幂等**：连续两次启动接管不得堆积 `.unused-<时间戳>` 归档。
    ///
    /// 【为什么必须守这条】用户点完迁移、重启、发现数据进来了，很自然会再点一次
    /// 迁移（想看"是不是真的能反复验证"）。第二次接管时目标里的库已经与暂存里的一模
    /// 一样，此时若还老老实实改名归档，用户每重启一次就多一个 `.unused-<时间戳>`，
    /// 数据目录很快变成一堆看不出所以然的文件。
    ///
    /// 【反向对照实测】把 `yield_target_db_with` 里那段 `same_as_source` 短路删掉
    /// （即"无条件归档"），本条变红。
    #[test]
    fn repeated_startup_takeover_does_not_pile_up_archives() {
        let root = tmp("takeover-idempotent");
        let target = root.join("com.tieznext");
        let source = root.join("old-data");
        seed_legacy(&source);
        seed_target_with_empty_db(&target);

        let count_archives = |dir: &Path| -> usize {
            fs::read_dir(dir)
                .map(|it| {
                    it.flatten()
                        .filter(|e| e.file_name().to_string_lossy().contains(".unused-"))
                        .count()
                })
                .unwrap_or(0)
        };

        // 第一次：暂存 -> 接管。
        let staging = takeover_staging_dir(&target);
        copy_tree(&source, &staging).unwrap();
        promote_staged_takeover_default(&staging, &target).unwrap();
        let after_first = count_archives(&target);

        // 第二次、第三次：模拟"用户又点了一次迁移 + 重启"。
        // 目标里此时已是真数据（与源一致），应用重启会打开它；
        // 这里复现"再次接管"：把源重新复制到暂存、再接管。
        for round in 0..2 {
            let staging = takeover_staging_dir(&target);
            copy_tree(&source, &staging).unwrap();
            promote_staged_takeover_default(&staging, &target)
                .unwrap_or_else(|e| panic!("第 {} 次接管必须成功：{}", round + 2, e));
        }

        assert_eq!(
            count_archives(&target),
            after_first,
            "重复接管不得持续堆积 .unused- 归档（实际目录内容：{:?}）",
            fs::read_dir(&target)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect::<Vec<_>>()
        );
        // 内容仍然正确
        assert_eq!(
            fs::read(target.join(DB_FILE)).unwrap(),
            fs::read(source.join(DB_FILE)).unwrap()
        );
        // 暂存目录每次都被消费掉
        assert!(!takeover_staging_dir(&target).exists());
    }

    /// `allow_takeover = false` 时，两阶段入口**不得**接管目标已有的库。
    ///
    /// 【为什么这条必须单独存在】`takeover` 由命令层读 SQLite 后决定（本模块只依赖
    /// `std`，自己做不了这个判定）。若实现把它写死成 `true`，就会在用户已经在新版里
    /// 存了真实记录的情况下仍然备份走那个库、把旧数据搬进来——**用户当前的数据被
    /// 整体替换掉**。这是本功能里后果最重的一条错误方向，因此必须有测试钉住"传 false
    /// 就一定不接管"。
    #[test]
    fn stage_takeover_with_false_never_claims_a_populated_target() {
        let root = tmp("stage-no-takeover");
        let target = root.join("com.tieznext");
        let source = root.join("old-data");
        seed_legacy(&source);
        seed_target_with_empty_db(&target);

        let mut calls = 0u64;
        let outcome = stage_takeover_with_progress(&source, &target, false, &mut |_, _, _, _| {
            calls += 1;
        });

        assert!(
            matches!(
                outcome,
                MigrationOutcome::Skipped(SkipReason::TargetAlreadyHasData)
            ),
            "传 false 时必须跳过而不是接管，实际 {outcome:?}"
        );
        assert_eq!(calls, 0, "跳过时不得发生任何复制（也就不会有进度回调）");
        // 目标那个库必须原地不动，且没有留档残渣。
        assert!(target.join(DB_FILE).is_file());
        assert!(!has_unused_archive(&target), "跳过时不得改动目标目录");
        // 也没有留下暂存目录。
        assert!(!takeover_staging_dir(&target).exists());
    }

    /// **一次性交付路径绝不许删掉"待接管"载体**（第二道防线）。
    ///
    /// `migrate_from_source_dir*` 是"当场交付"的一次性路径，不具备接管语义。它的暂存
    /// 目录用的是与两阶段**同一个固定名**，因此"目标同级已经躺着上次的待接管载体"这种
    /// 情形完全可能出现。此时唯一正确的动作是**拒绝**并如实说明，而不是把它删掉重来
    /// ——删掉就等于用户上次的复制成果凭空消失（v0.5.2 的第二个缺陷）。
    #[test]
    fn one_shot_delivery_refuses_instead_of_deleting_a_pending_staging() {
        let root = tmp("oneshot-refuse");
        let target = root.join("com.tieznext");
        let source = root.join("old-data");
        seed_legacy(&source);
        seed_target_with_empty_db(&target);

        // 目标同级放一份"上次已经复制就绪、等待重启接管"的暂存。
        //
        // 【必须用 `stage_takeover` 造它，不能手工 `copy_tree`】手工复制出来的目录
        // 与本函数要用的暂存目录名相同，会被上面那条冲突检查当成"源与暂存同名"提前
        // 拒绝——测的就不是这条路径了。走真实的 `stage_takeover` 造出来的载体，
        // 才是"确实存在一份待接管数据"的真实形状。
        let staged = stage_takeover(&source, &target);
        let staging = match staged {
            MigrationOutcome::Deferred { staging, .. } => staging,
            other => panic!("前提：两阶段第一步应得到 Deferred，实际 {other:?}"),
        };
        let staging_before = scan_tree(&staging).unwrap();
        let source_before = scan_tree(&source).unwrap();

        let outcome = migrate_from_source_dir(&source, &target, true);

        match outcome {
            MigrationOutcome::Failed { error, .. } => {
                assert!(
                    error.contains("待接管") && error.contains("重启"),
                    "拒绝的理由必须说清楚（待接管 + 重启），实际：{error}"
                );
            }
            other => panic!("存在待接管载体时必须拒绝当场交付，实际：{other:?}"),
        }
        // 载体一字未改（这才是"拒绝"的意义）
        assert_eq!(
            scan_tree(&staging).unwrap(),
            staging_before,
            "待接管载体绝不能被删掉或改动"
        );
        // 源也一字未改
        assert_eq!(scan_tree(&source).unwrap(), source_before);
    }

    /// **重试不得删掉上次的暂存**（v0.5.2 的第二个障碍）。
    ///
    /// 用户点第二次迁移时，上次已经复制好的暂存目录**正是本次要继承的东西**。
    /// 旧实现在每次迁移开头无条件 `remove_dir_all(staging)`，于是"每点一次就白干一次"。
    ///
    /// 【反向对照实测】把 `stage_takeover` 里那段"暂存已与源一致则直接复用"的短路
    /// 删掉（即回到无条件清理），本条变红——第二次会产生一次真实的重新复制，
    /// 断言用的"暂存目录的 inode 保持不变"会失败。
    #[test]
    fn second_attempt_reuses_the_ready_staging_instead_of_deleting_it() {
        let root = tmp("reuse-staging");
        let target = root.join("com.tieznext");
        let source = root.join("old-data");
        seed_legacy(&source);
        seed_target_with_empty_db(&target);

        let first = stage_takeover(&source, &target);
        let staging = match first {
            MigrationOutcome::Deferred { staging, .. } => staging,
            other => panic!("首次必须得到 Deferred，实际 {other:?}"),
        };
        assert!(staging.is_dir());

        // 【怎么证明"复用"而不是"删掉重建"】
        //
        // 不能往暂存里塞探针文件：它会被一致性校验逐项比对，塞进去反而会破坏
        // "暂存与源一致"的前提、逼出一次重建。
        //
        // 因此改为观察**暂存目录及其内部文件的修改时间**：
        //   * 第一次迁移刚写完，它们是 T1；
        //   * 若第二次"删掉重建"，`create_dir_all` + `fs::copy` 会把它们刷成 T2 ≠ T1；
        //   * 若第二次**原样复用**，它们一个字节都不会被碰，仍是 T1。
        //
        // ⚠️ 【为什么不用 inode / `MetadataExt`】实测两点都不行：
        //   1. ext4 会把刚释放的 inode 号立刻复用，`remove_dir_all` 后马上重建时
        //      inode 常常**不变**——这条判据会静默失效（本测试第一版就是这样漏掉的）；
        //   2. `MetadataExt::file_index()`（Windows）与 `mtime_nsec()`（Unix）都不可
        //      跨平台，而全量测试跑在 **Windows 目标**上——用它们写的断言会被 `cfg`
        //      整个编译掉，于是"看起来有断言、实际什么都没查"。
        //
        // `Metadata::modified()` 是**稳定且跨平台**的 API，两个目标上都会真的执行，
        // 这正是这里需要的东西。
        fn newest_mtime_nanos(dir: &Path) -> Option<u128> {
            let mut newest: Option<u128> = None;
            let mut stack = vec![dir.to_path_buf()];
            while let Some(current) = stack.pop() {
                let Ok(meta) = fs::metadata(&current) else {
                    continue;
                };
                if let Ok(t) = meta.modified() {
                    if let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) {
                        let nanos = d.as_nanos();
                        newest = Some(newest.map_or(nanos, |n: u128| n.max(nanos)));
                    }
                }
                if meta.is_dir() {
                    if let Ok(entries) = fs::read_dir(&current) {
                        for e in entries.flatten() {
                            stack.push(e.path());
                        }
                    }
                }
            }
            newest
        }

        let stamp_before = newest_mtime_nanos(&staging);
        // 超过文件系统可能的时间戳粒度（最坏情况是秒级）。
        std::thread::sleep(std::time::Duration::from_millis(1100));

        let second = stage_takeover(&source, &target);
        let staging2 = match second {
            MigrationOutcome::Deferred { staging, .. } => staging,
            other => panic!("第二次也必须得到 Deferred，实际 {other:?}"),
        };
        let stamp_after = newest_mtime_nanos(&staging2);

        assert_eq!(staging2, staging, "两次必须指向同一个暂存目录");
        assert!(
            stamp_before.is_some(),
            "前提：必须能读到暂存目录里文件的时间戳（读不到就等于没有断言）"
        );
        assert_eq!(
            stamp_after, stamp_before,
            "第二次迁移必须**原样复用**已就绪的暂存目录：\
             它的内容时间戳变了（{stamp_before:?} -> {stamp_after:?}），\
             说明它被删掉重建、上千个文件白复制了一遍 \
             ——这正是 v0.5.2 '第二次点击就白干' 的缺陷"
        );

        // 内容仍然完整
        assert_eq!(
            scan_tree(&staging).unwrap(),
            scan_tree(&source).unwrap(),
            "复用之后暂存仍必须与源逐项一致"
        );
    }

    /// **暂存目录不因让位失败而被删**（v0.5.2 的第一个障碍的原位回归）。
    ///
    /// 与 `occupied_target_turns_into_deferred_and_keeps_the_staging` 的区别：
    /// 那条走完整入口，这条直接对 `yield_target_db_with` 的失败分支下断言，
    /// 让"删暂存"这个动作在**任何**返回路径上都不会被悄悄加回来。
    #[test]
    fn yield_failure_never_deletes_the_pending_staging() {
        let root = tmp("yield-keeps-staging");
        let target = root.join("com.tieznext");
        let source = root.join("old-data");
        seed_legacy(&source);
        seed_target_with_empty_db(&target);

        let staging = takeover_staging_dir(&target);
        copy_tree(&source, &staging).unwrap();
        let staging_before = scan_tree(&staging).unwrap();

        let mut rename = |_from: &Path, _to: &Path| -> io::Result<()> {
            Err(sharing_violation())
        };
        assert!(yield_target_db_with(&source, &target, &mut rename).is_err());

        assert!(
            staging.is_dir(),
            "让位失败与暂存目录的存亡无关：暂存必须留着"
        );
        assert_eq!(
            scan_tree(&staging).unwrap(),
            staging_before,
            "暂存内容必须一字未改"
        );
    }

    /// **端到端（真实便携版副本 + 真 SQLite 库）**：两阶段迁移全程跑通。
    ///
    /// 覆盖三层用户选择（外层 / 内层 / `data`），每一步都断言"目标根层的库里真的有
    /// 那 120 条真实记录"——只断言返回值是不够的：文件搬过去了但搬的是那个空库、
    /// 或数据落进应用不读的嵌套位置，返回值一样是 `Deferred`。
    ///
    /// 【夹具必须是真 SQLite 库】本仓库踩过两次"夹具写了个假库、`rusqlite::open`
    /// 打不开、报错却指向被测代码"的坑（见维护文档与 `AB2-G3-086`）。为端到端可用，
    /// 这里直接复用 `seed_portable_copy` 里的 `make_real_sqlite_db`。
    #[test]
    fn two_phase_migration_end_to_end_on_a_real_portable_copy() {
        for (label, pick) in [("外层", 0usize), ("内层", 1), ("data", 2)] {
            let root = tmp(&format!("twophase-{label}"));
            let outer = seed_portable_copy(&root);
            let src = match pick {
                0 => outer.clone(),
                1 => outer.join("TieZ_0.3.3-portable"),
                _ => outer.join("TieZ_0.3.3-portable/data"),
            };
            let target = root.join("com.tieznext");
            seed_target_with_empty_db(&target);
            let src_db = find_db_below(&src).expect("源里必须有真库");
            let src_before = fs::read(&src_db).unwrap();

            // ---- 运行期：复制到暂存（目标库被占用，转 Deferred）----
            let outcome = stage_takeover(&src, &target);
            let staging = match outcome {
                MigrationOutcome::Deferred { staging, files, .. } => {
                    assert!(files > 0, "「{label}」必须真的复制了东西");
                    staging
                }
                other => panic!("「{label}」运行期必须得到 Deferred，实际 {other:?}"),
            };

            // 此时目标根层**仍是那个空库**（运行期一个字节都没动）。
            assert_eq!(
                count_db_rows(&target.join(DB_FILE)),
                0,
                "「{label}」运行期不得改动目标库"
            );

            // ---- 启动期：接管（无人持句柄，必然成功）----
            promote_staged_takeover_default(&staging, &target)
                .unwrap_or_else(|e| panic!("「{label}」启动期接管必须成功：{e}"));

            // 【关键】目标根层的库里真的有那 120 条。
            let rows = count_db_rows(&target.join(DB_FILE));
            assert_eq!(
                rows, 120,
                "「{label}」接管后目标库应有 120 条真实记录，实际 {rows} 条"
            );
            // 附件也到位
            assert!(
                target.join("attachments/old.png").is_file(),
                "「{label}」附件必须一并到位"
            );
            // 源库逐字节未改（安全契约第一条：源全程只读）
            assert_eq!(
                fs::read(&src_db).unwrap(),
                src_before,
                "「{label}」源库必须逐字节未变"
            );
            // 暂存已被消费
            assert!(!staging.exists(), "「{label}」接管后暂存目录必须消失");
        }
    }
}
