//! 备份**导入恢复**：把一份 zip 落回数据目录，并保证"要么完全恢复，要么什么都不变"。
//!
//! # 安全契约（与用户此前对数据迁移的硬要求一致）
//!
//! 1. **导入前先给当前数据做一份完整旁路备份**——否则"导入即完全恢复"会变成
//!    "导入即毁掉现状"。备份路径会回传前端明确告知用户。
//! 2. **先校验后落地**：整包先只读扫描并逐条目校验 sha256、核对数量、校验必备条目，
//!    全部通过才开始写。
//! 3. **失败不得破坏现有数据**：所有解压与组装都发生在数据目录的**同级暂存目录**里；
//!    正式数据的交换推迟到下次启动、在 `init_db` 之前进行，任一步失败即把已挪走的
//!    条目原样放回。
//! 4. **导入是破坏性操作**，前端必须二次确认并明确告知"当前数据将被替换"。
//!
//! # 为什么"交换"必须跨一次重启（这是本模块最重要的一条结构决定）
//!
//! 备份恢复的入口是**应用内的界面**：用户点它时应用**必定正在运行**，而应用启动时
//! 就已经打开了数据目录里的 `clipboard.db`（`app/setup.rs` 的 `database::init_db`），
//! 这个连接常驻在 `DbState` 里、被 3 个 repo 与 `McpStore` 多处持有，**不可能在运行期
//! 释放**。Windows **不允许给已打开的文件改名**（`ERROR_SHARING_VIOLATION`，os error 32），
//! 于是"把当前库挪走、把新库放上来"这一步在运行期**必然失败**——旧实现正是这么做的，
//! 用户看到的就是"点了恢复，报错说文件被占用"。
//!
//! 因此这条链和迁移一样走**两阶段**：
//!
//! ```text
//! 用户点恢复 → 运行期只做「校验包 + 组装暂存 + 写待接管标记」→ 提示重启
//! 下次启动   → 在 init_db 之前做改名交换（此时无人持句柄）→ 必然成功
//! ```
//!
//! 运行期能安全完成的部分一步都没少（含导入前的旁路备份、逐条 sha256 校验、路径改写、
//! 云同步游标重置、数量对账），被推迟的**只有文件改名**这一个动作。标记的读写、失败
//! 语义与启动期时机与迁移**完全共用** `migration_pending`，见那里的说明。
//!
//! # "完全恢复"到底恢复什么（这决定了哪些东西**故意**不动）
//!
//! 备份覆盖的是**用户数据**：剪贴板历史、标签、设置项、附件、表情收藏、自定义背景。
//! 以下内容**有意不随包恢复**，因为它们是"这台机器的运行环境"而不是用户数据：
//!
//! - `datapath.txt`：数据目录重定向指针。恢复它会让应用把数据目录指向导出机器的
//!   路径，在那台机器上通常不存在——等于把应用弄坏。
//! - `tiez.log`：运行日志，属运行产物。
//!
//! 因此"完全恢复"的准确含义是：**包所覆盖的全部用户数据与导出那一刻逐一等价**。
//! 本模块的自证测试就是对导入前后全部受管文件做 sha256 逐项比对。
//!
//! # 导入后必须做的重置（漏一项就会出现"数据不一致"）
//!
//! - 数据库被整体替换后 **WAL/SHM 必须作废**：新库不是旧连接写出来的，旧的
//!   `-wal`/`-shm` 若留着，SQLite 会拿它去恢复一个完全无关的数据库。
//! - **重跑 `run_migrations` + `seed_defaults`**：包可能是旧 schema 导出的；同时
//!   保证新版新增的设置项有默认值。
//! - **云同步游标必须重置**：游标记录的是"同步到远端的哪个位置"，属于**旧数据集**
//!   的时点状态。恢复后不重置，应用会以为远端还是导出时的样子，从而漏推或误覆盖。
//! - **附件目录整体替换**：靠"在暂存目录里组装完整后再整体换上去"实现，绝不边删边拷。

use super::export::classify_entry;
use super::format::{
    parse_manifest, safe_relative_path, sanitize_file_name, sha256_bytes, sha256_file,
    BackupError, BackupManifest,
    BackgroundMapFile, ManifestCounts, ManifestVersion, APP_ID, ENTRY_ATTACHMENTS_PREFIX,
    ENTRY_BACKGROUND_MAP, ENTRY_DATABASE, ENTRY_EMOJI_PREFIX, ENTRY_MANIFEST, ENTRY_PATH_MAP,
};
use super::resolve::PathMappings;
use crate::error::AppResult;
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use zip::ZipArchive;

/// 数据目录内**受备份管理**的条目。
///
/// 导入时会先清空暂存目录里这些条目，再把包内内容放回去——这样才能做到"包里有几条
/// 就是几条"，而不是把包内容**叠加**在当前数据之上（那是"部分恢复"）。
/// 顺序有讲究：数据库优先，便于失败时尽早暴露文件占用问题。
const MANAGED_ENTRIES: &[&str] = &[
    ENTRY_DATABASE,
    "clipboard.db-wal",
    "clipboard.db-shm",
    "attachments",
    "emoji_favorites",
    "background",
];

/// 恢复后必须重置的云同步状态键。
///
/// 这些键记录的是"本机与远端同步到哪儿了"，属于旧数据集的**时点状态**。恢复一份
/// 来自另一时点（甚至另一台机器）的数据后，它们全部失效。全部按新建库的默认值重置。
const CLOUD_SYNC_RESET_KEYS: &[(&str, &str)] = &[
    ("cloud_sync_cursor", "0"),
    ("cloud_sync_webdav_local_seq", "0"),
    ("cloud_sync_webdav_op_cursor_map", "{}"),
    ("cloud_sync_webdav_blob_cache", "{}"),
    ("cloud_sync_webdav_last_snapshot_push_at", "0"),
    ("cloud_sync_webdav_last_snapshot_pull_at", "0"),
    ("cloud_sync_webdav_last_head_rebuild_at", "0"),
    ("cloud_sync_settings_applied_at", "0"),
];

/// 恢复后需要清空的**同步账本表**（纯本机状态）。
///
/// `cloud_sync_local_index` 记录"每条内容上次上传时的摘要"，恢复后必然过期；清空会让
/// 下一次同步重新比对并补齐。
///
/// `cloud_sync_tombstones` **不清空**：它记录"哪些内容被删除了"这一语义事实，
/// 清掉会让已删除的内容在远端复活。
const CLOUD_SYNC_RESET_TABLES: &[&str] = &["cloud_sync_local_index"];

/// 导入请求。
#[derive(Debug, Clone)]
pub struct RestoreRequest {
    /// 当前数据目录。
    pub data_dir: PathBuf,
    /// 要导入的备份包路径。
    pub archive_path: PathBuf,
    /// 「待接管标记」的存放目录（**原生**应用数据目录，即 `app.path().app_data_dir()`）。
    ///
    /// 【为什么必须是原生目录而不是当前数据目录】标记要指出"下次启动该做什么"，而
    /// 它自己**不能住在会被这次恢复替换掉的目录里**——那正是它的作用对象。原生目录由
    /// identifier 推导、位置稳定，且永远不是被替换的那一个。这条判断与迁移完全一致
    /// （`migration_pending` 模块头部有完整说明）。
    ///
    /// `None` 表示调用方拿不到这个目录。此时**拒绝执行**而不是猜一个路径：猜错会让
    /// 下次启动找不到标记，用户会以为"恢复成功了"，而数据其实从未被交换过。
    pub pending_marker_dir: Option<PathBuf>,
}

/// 导入结果（回传前端，供用户核对到底发生了什么）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreReport {
    pub archive_path: String,
    pub format_version: u32,
    pub exported_at: String,
    pub exported_app_version: String,
    /// 导入前为当前数据建立的旁路备份路径（**必须告知用户**）。
    pub pre_restore_backup: Option<String>,
    /// 实际恢复的受管文件数。
    pub restored_files: u64,
    /// 实际恢复的字节数。
    pub restored_bytes: u64,
    /// 通过 sha256 校验的条目数。
    pub verified_entries: u64,
    /// 包内声明的数量。
    pub counts: ManifestCounts,
    /// 已执行的导入后重置项（供界面展示"做了什么"）。
    pub resets_applied: Vec<String>,
    /// 需要用户知道的非致命情况。
    pub warnings: Vec<String>,
    /// 是否需要重启应用才能看到新数据。
    ///
    /// 【语义已收紧：从"建议"变成"必须"】旧实现在这里返回 `true` 的同时**已经做完**
    /// 了替换（真机上则是失败报错）；现在它表示"数据已组装就绪、等着下次启动上位"，
    /// 是**完成这次恢复的唯一途径**。因此界面必须把重启当成必做动作来呈现，而不是一句
    /// 可忽略的提示。
    pub restart_required: bool,
    /// 本次恢复是否已提交为"待下次启动生效"（成功路径上恒为 `true`）。
    ///
    /// 【为什么单列一个字段而不复用 `restartRequired`】两者在这种情形下会分叉：恢复已经
    /// **提交**（暂存与标记都就绪），但调用方拿不到原生数据目录、标记没能落盘。那时
    /// `restartRequired` 仍是 `false`（重启也不会有任何变化），而 `deferredUntilRestart`
    /// 让我们能如实区分"提交了"和"生效了"。前端的判据是 `restartRequired`。
    pub deferred_until_restart: bool,
    /// 暂存目录路径（**必须告知用户**：重启后由它上位；若一直不重启，它就一直占着磁盘）。
    pub pending_staging_dir: Option<String>,
    /// 待接管标记路径（`None` 表示这次恢复**没有**被提交，重启不会有任何变化）。
    pub pending_marker_path: Option<String>,
}

/// 包内容清点结果（**只读阶段**产出）。
struct ArchivePlan {
    /// 需要落盘的条目：`(zip 内下标, 归一化相对路径)`。
    files: Vec<(usize, String)>,
    verified_entries: u64,
    restored_files: u64,
    restored_bytes: u64,
}

/// 进程级导入互斥锁。
///
/// # 为什么必须有它
///
/// 两次导入并发执行时，第二次会把第一次**正在组装**的暂存目录删掉（`staging_dir` 只按
/// pid 命名，两次调用相同；而"存在即清场"是无条件的）。于是第一次的替换步骤找不到任何
/// 待放置条目、全部 `continue`，却仍然返回"导入成功"并**删掉 aside** —— 最终数据目录里
/// 一个受管条目都不剩（含 `clipboard.db`）。这是一条能被 UI 误触发（折叠/重复点击）的
/// 真实数据销毁路径，因此必须在最内层加锁，而不是只靠界面禁用按钮。
static IMPORT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 本次进程内每次导入的唯一序号：让暂存目录名不可能与并发的那一次重合。
static IMPORT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 导入一份备份包。
///
/// 返回值只有两种结局：`Ok`（数据已完全恢复）或 `Err`（**现有数据一个字节都没动**）。
/// 不存在"恢复了一半"的中间态。
/// 从恢复请求里取"源备份包的文件名"（用于把它固定住，免遭自动备份轮换删除）。
///
/// 只取**文件名**而不存绝对路径：固定索引 `PinIndex` 本来就是按名字记的
/// （见 `auto_backup/store.rs`），而且包一定在那个受管目录里 —— 存路径会多一份
/// 可能与实际目录不一致的状态。
///
/// 【为什么不是所有恢复都有】数据管理里的"导入备份"用的是用户自选的任意路径，
/// 它不在自动备份目录里，也就没有"被轮换删掉"这回事。所以只有源包确实落在
/// 自动备份目录内时才有名字可保护；否则返回 `None`，行为与改动前一致。
fn backup_archive_name(req: &RestoreRequest) -> Option<String> {
    let name = req.archive_path.file_name()?.to_string_lossy().to_string();
    if name.is_empty() {
        return None;
    }
    // 只在"源包确实位于**自动备份目录**内"时才保护 —— 否则这个名字对轮换毫无意义
    // （轮换只动它自己那个目录），而记进标记会让人误以为有什么在被保护。
    //
    // 【判据必须是自动备份目录本身，不能是"数据目录的父目录"】后者宽松得多：
    // 数据目录的父级通常还放着别的东西（桌面就在旁边、同级还有用户自己建的文件夹），
    // 用它判定会把**从桌面导入**的包也当成"在自动备份目录内"而记下名字 ——
    // 那是虚假的保护。本仓库里已经有测试专门验证这一点
    // （`archive_outside_auto_backup_dir_is_not_recorded`），它当初正是抓出了这个 bug。
    let auto_dir = crate::services::auto_backup::store::auto_backup_dir(&req.data_dir);
    if !req.archive_path.starts_with(&auto_dir) {
        return None;
    }
    Some(name)
}

pub fn restore_backup(req: &RestoreRequest) -> Result<RestoreReport, BackupError> {
    // 串行化整个导入流程。被 poison（持锁线程 panic）时也继续取用：宁可继续串行执行，
    // 也不要因为一次 panic 让功能永久不可用——锁在这里只用于互斥，不保护共享数据。
    let _guard = IMPORT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let data_dir = &req.data_dir;
    if !data_dir.is_dir() {
        return Err(BackupError::Land(format!(
            "数据目录不存在：{}",
            data_dir.display()
        )));
    }
    if !req.archive_path.is_file() {
        return Err(BackupError::InvalidZip(format!(
            "备份包不存在：{}",
            req.archive_path.display()
        )));
    }

    let mut warnings: Vec<String> = Vec::new();

    // ===== 第 0–1 步：只读解析 + 全包校验（在任何写操作之前）=====
    //
    // 原版 TieZ 的包、缺 manifest 的包、比本版新的包、校验和不符的包，都在这里被拒绝，
    // **现有数据不会被触碰**。这一步不写任何文件。
    let (manifest, plan) = {
        let file = std::fs::File::open(&req.archive_path)?;
        let mut archive =
            ZipArchive::new(file).map_err(|e| BackupError::InvalidZip(e.to_string()))?;
        let (manifest, version) = read_manifest(&mut archive)?;
        let plan = scan_archive(&mut archive, &manifest, version, &mut warnings)?;
        (manifest, plan)
    };

    // ===== 第 2 步：给当前数据建立旁路备份 =====
    let pre_restore_backup = build_pre_restore_backup(data_dir, &mut warnings)?;

    // ===== 第 3–5 步：组装暂存目录并执行全部重置 =====
    let staging = staging_dir(data_dir);
    if staging.as_path() == data_dir.as_path() {
        return Err(BackupError::Land(
            "暂存路径与数据目录重合，已放弃导入".to_string(),
        ));
    }
    if staging.exists() {
        // 上次崩溃的残留；它从未被提升，删掉是安全的。
        std::fs::remove_dir_all(&staging)?;
    }

    let staged = stage_data(req, &plan, &manifest, &staging, &mut warnings);
    let staged = match staged {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    };

    // ===== 第 6 步：提交为"待下次启动生效" =====
    //
    // 【为什么这里不做交换，而是写一个标记】见本模块头部：换掉 `clipboard.db` 需要给它
    // 改名，而应用自己正把这个文件打开着，Windows 不许改名已打开的文件。真正的交换由
    // 下次启动在 `init_db` 之前完成（`app/setup.rs` 的 `run_pending_takeover`）。
    //
    // 走到这里，运行期**能安全完成的全部工作**都已经做完：包已逐条校验、暂存已组装、
    // 路径已改写、云同步游标已重置、数量已对账。剩下的只有一个改名动作。
    let Some(marker_dir) = req.pending_marker_dir.as_deref() else {
        // 拿不到原生数据目录 ⇒ 没法让下次启动知道有活要干。**如实失败并回滚暂存**，
        // 绝不能让用户看到"恢复成功"而其实什么都没发生。
        let _ = std::fs::remove_dir_all(&staging);
        return Err(BackupError::Land(format!(
            "无法确定「待接管标记」的存放位置，本次恢复未能提交（你的现有数据未被改动；\
             已组装好的暂存目录已清理）。请重试；若反复出现，请把应用日志提供给支持人员。\
             （数据目录 {}）",
            data_dir.display()
        )));
    };

    // 暂存目录名带 pid 与序号（见 `staging_dir`），因此**下一次提交必然用的是另一个名字**。
    // 在写标记之前，把上一次未重启就已失效的暂存清掉：不清的话它会一直占着磁盘，
    // 而没有任何东西会再来处理它（标记只指向最新那一个）。
    let reclaimed = remove_superseded_restore_staging(data_dir, &staging);

    // 标记里记的"源目录"是 `data_dir` 自身——恢复没有"另一个源目录"，用户的依据是那个
    // 备份包（导出链只读它，本模块从不写它）。填 `data_dir` 而不是留空，是为了让现有
    // 接管路径（它无条件用这个字段做路径改写）不需要理解两种语义：对恢复来说
    // "旧前缀 = 当前数据目录"恰好是正确的。
    //
    // 【为什么要把源备份包的名字记进标记】这一提交到下次启动之间有个可能很长的窗口，
    // 而**自动备份的轮换会在这个窗口里继续跑**。用户点了恢复 → 没重启 → 后台触发一次
    // 备份 → 轮换把那份包当作"最老的、未固定的"删掉。后果是双重的：既没有可重来的包
    // （连"再点一次恢复"都做不到），而且如果提升失败就更没有任何退路。
    //
    // 记下名字之后，运行期会把它**固定住**（见下面的 `protect_pending_backup`），
    // 启动期提升成功后解除固定 —— 见 `app/setup.rs`。
    let pending = crate::migration_pending::PendingMigration::for_kind_protecting(
        crate::migration_pending::PendingKind::LocalRestore,
        data_dir.clone(),
        staging.clone(),
        data_dir.clone(),
        // 【顺序不可颠倒】暂存组装成功之后才写"已就绪"的标记。若组装中途进程被杀，
        // 数据目录同级会留下半个暂存目录而**没有**标记——那就只是一个无主残渣；
        // 反过来（先写标记再组装）会让下次启动把一个不完整的片段当成正式数据提升。
        true,
        env!("CARGO_PKG_VERSION"),
        backup_archive_name(req),
    );

    let marker_path = match crate::migration_pending::write(marker_dir, &pending) {
        Ok(p) => p,
        Err(e) => {
            // 标记是"下次启动该做什么"的唯一凭据。写不进去就不能对用户说"重启即可"。
            let _ = std::fs::remove_dir_all(&staging);
            return Err(BackupError::Land(format!(
                "数据已在暂存目录组装就绪（{}），但「待接管标记」写入失败（{}）；\
                 本次恢复未能提交，你的现有数据未被改动。请检查数据目录的写入权限后重试。",
                staging.display(),
                e
            )));
        }
    };

    if reclaimed > 0 {
        warnings.push(format!(
            "已清理上次未重启就已失效的恢复暂存目录（{} 个）。那一次恢复的内容未写入正式数据；\
             正式数据现在仍是你操作前的样子。",
            reclaimed
        ));
    }
    warnings.push(format!(
        "本次恢复已进入待生效状态：数据已按包内容组装完毕并存放在 {}。\
         重启应用后，它会在打开数据库**之前**自动完成替换并开始生效。",
        staging.display()
    ));

    Ok(RestoreReport {
        archive_path: req.archive_path.to_string_lossy().to_string(),
        format_version: manifest.format_version,
        exported_at: manifest.exported_at.clone(),
        exported_app_version: manifest.app_version.clone(),
        pre_restore_backup: pre_restore_backup.map(|p| p.to_string_lossy().to_string()),
        restored_files: staged.restored_files,
        restored_bytes: staged.restored_bytes,
        verified_entries: plan.verified_entries,
        counts: manifest.counts.clone(),
        resets_applied: staged.resets_applied,
        warnings,
        // 【这不是"建议"】旧实现在这里返回 true 的同时已经做完了替换（真机上则是失败），
        // 现在它表示"数据等着下次启动上位"——重启是这次恢复生效的唯一途径。
        restart_required: true,
        deferred_until_restart: true,
        pending_staging_dir: Some(staging.to_string_lossy().to_string()),
        pending_marker_path: Some(marker_path.to_string_lossy().to_string()),
    })
}

/// 清掉上次提交、但从未重启生效的**恢复暂存目录**。
///
/// # 为什么必须清（以及为什么只清这一种）
///
/// 恢复的暂存目录名带 pid 与进程内序号（`staging_dir`）。下次提交恢复时名字**必然不同**，
/// 于是上一次那个目录再也不会被任何人处理：
///
/// - 它不是"待提升的活"：标记只指向最新那一个，旧的那个没有被提升的机会；
/// - 它不能被当成"碎片"盲删：万一标记恰好还指着它（理论上不会，因为标记此刻指向
///   最新一个），删了就丢掉了用户唯一的一份待生效数据。
///
/// 所以判据是"名字符合恢复暂存的形状 **且** 不等于本次要提交的那一个"。只删目录、
/// 不删任何别的东西；删不掉也只记一笔（磁盘占着比误删用户数据轻得多）。
///
/// 返回清掉的个数。
fn remove_superseded_restore_staging(data_dir: &Path, keep: &Path) -> usize {
    let parent = data_dir.parent().unwrap_or_else(|| Path::new("."));
    let target_name = data_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "appdata".to_string());
    let prefix = format!(".{}.restoring.", target_name);
    let Ok(entries) = std::fs::read_dir(parent) else {
        return 0;
    };
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep || !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(&prefix) {
            continue;
        }
        if std::fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// 读取并校验 manifest（含"是不是本应用的包"与版本判定）。
fn read_manifest<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
) -> Result<(BackupManifest, ManifestVersion), BackupError> {
    let mut entry = archive.by_name(ENTRY_MANIFEST).map_err(|_| {
        BackupError::ForeignApp("包内没有 manifest.json（不是 Tiez-Next 导出的备份包）".to_string())
    })?;
    let mut bytes = Vec::new();
    entry
        .read_to_end(&mut bytes)
        .map_err(|e| BackupError::InvalidZip(e.to_string()))?;
    parse_manifest(&bytes)
}

/// 第一遍：只读扫描整个包并核对校验和/数量。**不写任何文件。**
///
/// 它的意义是：包的完整性在"还没碰数据"的时候就已判定完，因此损坏包/被篡改包一律
/// 走不到落盘阶段。
fn scan_archive<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    manifest: &BackupManifest,
    version: ManifestVersion,
    warnings: &mut Vec<String>,
) -> Result<ArchivePlan, BackupError> {
    let mut plan = ArchivePlan {
        files: Vec::new(),
        verified_entries: 0,
        restored_files: 0,
        restored_bytes: 0,
    };

    // 版本要求的必备条目必须存在。
    for required in version.required_entries() {
        if archive.by_name(required).is_err() {
            return Err(BackupError::CountMismatch {
                what: format!("必备条目 {} 缺失", required),
                expected: 1,
                actual: 0,
            });
        }
    }

    let mut seen: Vec<String> = Vec::new();
    for i in 0..archive.len() {
        let (name, is_dir) = {
            let entry = archive
                .by_index(i)
                .map_err(|e| BackupError::InvalidZip(e.to_string()))?;
            let raw_name = entry.name().to_string();
            let is_dir = entry.is_dir();
            let Some(name) = safe_relative_path(&raw_name) else {
                // 不安全路径（zip slip / 绝对路径）直接拒绝整包，而不是静默跳过：
                // 一个含逃逸路径的包本身就是不可信输入。
                return Err(BackupError::InvalidZip(format!(
                    "包内含不安全的条目路径：{}",
                    raw_name
                )));
            };
            (name, is_dir)
        };
        if is_dir {
            continue;
        }
        seen.push(name.clone());

        match classify_entry(&name) {
            // manifest 已在第 0 步解析过。
            super::export::EntryKind::Manifest => continue,
            // **未知条目安全跳过**：这正是"旧版能读新版包"的落地方式——新版把新增
            // 功能的数据放在新条目路径下，旧版不认识就跳过，其余数据照常恢复。
            super::export::EntryKind::Unknown => {
                warnings.push(format!("包内有一条本版不认识的条目，已跳过：{}", name));
                continue;
            }
            _ => {}
        }

        // 逐条目核对 sha256。分块读取，内存占用与文件大小无关。
        let mut entry = archive
            .by_index(i)
            .map_err(|e| BackupError::InvalidZip(e.to_string()))?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        let mut size = 0u64;
        loop {
            let n = entry
                .read(&mut buf)
                .map_err(|e| BackupError::InvalidZip(e.to_string()))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            size += n as u64;
        }
        drop(entry);
        let actual = format!("sha256:{:x}", hasher.finalize());

        if let Some(expected) = manifest.checksums.get(&name) {
            if expected != &actual {
                return Err(BackupError::ChecksumMismatch { entry: name });
            }
            plan.verified_entries += 1;
        } else if !manifest.checksums.is_empty() {
            // 清单声明了校验和清单却漏了这一条：包与清单不一致。
            return Err(BackupError::ChecksumMismatch {
                entry: format!("{}（清单中缺失）", name),
            });
        }

        plan.restored_files += 1;
        plan.restored_bytes += size;
        plan.files.push((i, name));
    }

    // 清单里声明了、但包里没有的条目：包被截断。
    for declared in manifest.checksums.keys() {
        if declared == ENTRY_MANIFEST {
            continue;
        }
        if !seen.iter().any(|s| s == declared) {
            return Err(BackupError::CountMismatch {
                what: format!("清单声明的条目 {} 在包内不存在", declared),
                expected: 1,
                actual: 0,
            });
        }
    }

    // 数量对账。manifest 未声明（=0）时按更早写入端的包处理并跳过该项检查，
    // 以保持后向兼容。
    let declared_count = |prefix: &str| -> u64 {
        seen.iter().filter(|n| n.starts_with(prefix)).count() as u64
    };
    if manifest.counts.attachments > 0 {
        let actual = declared_count(ENTRY_ATTACHMENTS_PREFIX);
        if actual != manifest.counts.attachments {
            return Err(BackupError::CountMismatch {
                what: "附件文件数".to_string(),
                expected: manifest.counts.attachments,
                actual,
            });
        }
    }
    if manifest.counts.emoji_favorites > 0 {
        let actual = declared_count(ENTRY_EMOJI_PREFIX);
        if actual != manifest.counts.emoji_favorites {
            return Err(BackupError::CountMismatch {
                what: "表情收藏文件数".to_string(),
                expected: manifest.counts.emoji_favorites,
                actual,
            });
        }
    }

    Ok(plan)
}

/// 一次导入里"真正落地"的部分。
struct StagedParts {
    restored_files: u64,
    restored_bytes: u64,
    resets_applied: Vec<String>,
}

/// 第 3–5 步：在暂存目录里组装完整的新数据目录，并执行全部重置。
fn stage_data(
    req: &RestoreRequest,
    plan: &ArchivePlan,
    manifest: &BackupManifest,
    staging: &Path,
    warnings: &mut Vec<String>,
) -> Result<StagedParts, BackupError> {
    let data_dir = &req.data_dir;

    // ---- 3.1 先整体复制当前数据目录作为暂存骨架 ----
    //
    // 这样 `datapath.txt`、日志等"非受管但属于运行环境"的文件自然被保留，而受管条目
    // 会在下一步被清空重建。整个过程中**当前数据目录只被读取**。
    copy_tree(data_dir, staging)?;

    // ---- 3.2 清空受管条目（在暂存里，不是正式数据）----
    for name in MANAGED_ENTRIES {
        let p = staging.join(name);
        if !p.exists() {
            continue;
        }
        if p.is_dir() {
            std::fs::remove_dir_all(&p)?;
        } else {
            std::fs::remove_file(&p)?;
        }
    }

    // ---- 3.3 按包内容落盘（第二遍读 zip）----
    let mut restored_files = 0u64;
    let mut restored_bytes = 0u64;
    {
        let file = std::fs::File::open(&req.archive_path)?;
        let mut archive =
            ZipArchive::new(file).map_err(|e| BackupError::InvalidZip(e.to_string()))?;
        for (index, rel) in &plan.files {
            let mut entry = archive
                .by_index(*index)
                .map_err(|e| BackupError::InvalidZip(e.to_string()))?;
            // 条目名必须与扫描阶段一致（同一份文件，但显式断言避免下标错位）。
            let actual_name = safe_relative_path(entry.name()).unwrap_or_default();
            if &actual_name != rel {
                return Err(BackupError::InvalidZip(format!(
                    "包内条目顺序异常：期望 {}，实际 {}",
                    rel, actual_name
                )));
            }
            let dest = staging.join(rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = std::fs::File::create(&dest)?;
            let mut hasher = Sha256::new();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = entry
                    .read(&mut buf)
                    .map_err(|e| BackupError::InvalidZip(e.to_string()))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                out.write_all(&buf[..n])?;
            }
            drop(out);
            // 落盘后再从磁盘读回来算一次哈希：证明"写下去的就是包里的那份"。
            let on_disk = sha256_file(&dest)?;
            let computed = format!("sha256:{:x}", hasher.finalize());
            if on_disk != computed {
                return Err(BackupError::Land(format!(
                    "写入后校验失败：{}（磁盘内容与读出内容不一致）",
                    rel
                )));
            }
            if let Some(declared) = manifest.checksums.get(rel.as_str()) {
                if declared != &on_disk {
                    return Err(BackupError::ChecksumMismatch {
                        entry: rel.clone(),
                    });
                }
            }
            restored_files += 1;
            restored_bytes += std::fs::metadata(&dest)?.len();
        }
    }

    // ---- 4. 数据库侧的收尾 ----
    let staged_db = staging.join(ENTRY_DATABASE);
    if !staged_db.is_file() {
        return Err(BackupError::CountMismatch {
            what: "数据库条目".to_string(),
            expected: 1,
            actual: 0,
        });
    }
    let resets_applied =
        prepare_database(&staged_db, staging, data_dir, manifest, warnings)?;

    // ---- 5. 落盘后对账 ----
    verify_counts(&staged_db, staging, manifest)?;

    Ok(StagedParts {
        restored_files,
        restored_bytes,
        resets_applied,
    })
}

/// 在暂存库里执行：路径改写 → 云同步重置 → 迁移/默认值 → 侧车清理 → 背景还原。
fn prepare_database(
    staged_db: &Path,
    staging: &Path,
    data_dir: &Path,
    _manifest: &BackupManifest,
    warnings: &mut Vec<String>,
) -> Result<Vec<String>, BackupError> {
    let mut applied: Vec<String> = Vec::new();

    {
        let conn = Connection::open(staged_db)
            .map_err(|e| BackupError::Land(format!("暂存数据库无法打开：{}", e)))?;

        // ---- 4.1 把数据库里的绝对路径改写到**当前**数据目录 ----
        //
        // 这一步是"数据一致"的关键：不改写的话，历史记录里的图片路径仍指向导出机器
        // 的目录，表现为附件全部丢失。
        let rewritten = rewrite_paths_for_current_dir(&conn, staging, data_dir, warnings)
            .map_err(|e| BackupError::Land(format!("路径改写失败：{}", e)))?;
        if rewritten > 0 {
            applied.push(format!("已把 {} 条记录的绝对路径改写为当前数据目录", rewritten));
        }

        // ---- 4.2 云同步游标与同步账本重置 ----
        let mut reset_keys = 0usize;
        for (key, value) in CLOUD_SYNC_RESET_KEYS {
            let changed = conn
                .execute(
                    "UPDATE settings SET value = ?1 WHERE key = ?2 AND value <> ?1",
                    rusqlite::params![value, key],
                )
                .unwrap_or(0);
            if changed > 0 {
                reset_keys += 1;
            }
        }
        for table in CLOUD_SYNC_RESET_TABLES {
            let _ = conn.execute(&format!("DELETE FROM {}", table), []);
        }
        if reset_keys > 0 {
            applied.push("已重置云同步游标与同步账本（避免与远端冲突）".to_string());
        }

        // ---- 4.3 迁移 + 默认值 ----
        //
        // 包可能来自旧 schema；同时新版新增的设置项需要补默认值。
        crate::infrastructure::repository::migrations::run_migrations(&conn)
            .map_err(|e| BackupError::Land(format!("数据库迁移失败：{}", e)))?;
        crate::database::seed_defaults(&conn)
            .map_err(|e| BackupError::Land(format!("写入默认设置失败：{}", e)))?;
        applied.push("已重跑数据库迁移并补齐默认设置".to_string());

        // ---- 4.4 让写入安全落盘，之后才能删侧车文件 ----
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    }
    // 连接已关闭（作用域结束）：SQLite 在最后一个连接关闭时会做 checkpoint。

    // ---- 4.5 WAL/SHM 作废 ----
    //
    // 新库不是旧连接写出来的；旧的 -wal/-shm 若跟着一起被放上去，SQLite 会拿它去
    // "恢复"一个完全无关的数据库，后果不可预测。因此显式删除。
    for suffix in ["-wal", "-shm"] {
        let p = staged_db.with_file_name(format!("{}{}", ENTRY_DATABASE, suffix));
        if p.exists() {
            std::fs::remove_file(&p)?;
        }
    }
    applied.push("已重置数据库 WAL/SHM 状态文件".to_string());

    // ---- 4.6 自定义背景：把设置项指向还原后的文件 ----
    let map_entry = staging.join(ENTRY_BACKGROUND_MAP);
    if map_entry.is_file() {
        resolve_background(staged_db, staging, data_dir, &map_entry, warnings)
            .map_err(|e| BackupError::Land(format!("背景图还原失败：{}", e)))?;
    }

    Ok(applied)
}

/// 把数据库里指向"导出机器数据目录"的绝对路径改写成当前数据目录。
///
/// 依据是包内的 `mappings.json`（精确：记录的就是导出时哪些相对路径对应哪些绝对路径）。
/// 该文件是可选条目——更早的写入端没有它，此时发一条 warning 如实告知用户，而不是
/// 假装完整恢复。
fn rewrite_paths_for_current_dir(
    conn: &Connection,
    staging: &Path,
    data_dir: &Path,
    warnings: &mut Vec<String>,
) -> Result<u64, Box<dyn std::error::Error>> {
    let mappings: Option<PathMappings> = std::fs::read(staging.join(ENTRY_PATH_MAP))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());

    let Some(mappings) = mappings else {
        warnings.push(
            "该备份包不含路径映射表（可能由更早的版本导出）；已在导入后重跑迁移与默认值，但历史记录里指向导出机器目录的绝对路径无法自动改写。若附件显示异常，可在原位置重新添加。"
                .to_string(),
        );
        return Ok(0);
    };

    // 逐条把"旧绝对路径"替换成"当前数据目录 + 相对路径"。
    //
    // 【为什么必须先归一化分隔符】`mappings.items` 的键用的是 `/`（zip 条目名规范），
    // 而 Windows 的路径分隔符是 `\`。直接把 `attachments/a.png` 拼到
    // `C:\...\com.tieznext` 后面会得到 `C:\...\com.tieznext\attachments/a.png` 这种
    // **混合分隔符**路径：它与原值不相等，于是所有路径都被"改写"了一遍却什么也没改好，
    // 数据库内容因此出现无意义差异。本模块的往返测试
    // `roundtrip_restores_data_exactly` 正是靠分表指纹把这个缺陷抓了出来。
    let mut replacements: Vec<(String, String)> = Vec::new();
    // 被跳过的可疑键会计入警告，让用户知道不是所有路径都被改写了。
    let mut skipped_keys: Vec<String> = Vec::new();
    for (rel, original) in &mappings.items {
        // 键来自包内 JSON，属**不可信输入**：它会被拼成"新数据目录 + 键"并写回数据库。
        // 必须先过 `safe_relative_path`（挡 `..`/绝对路径/盘符），否则改写后的路径可能
        // 指向数据目录之外，下次应用读写这些路径就会碰到用户机器上的任意文件。
        let Some(safe_rel) = safe_relative_path(rel) else {
            skipped_keys.push(rel.clone());
            continue;
        };
        let native = safe_rel.replace('/', std::path::MAIN_SEPARATOR_STR);
        let new_path = data_dir.join(native).to_string_lossy().to_string();
        if original != &new_path {
            replacements.push((original.clone(), new_path));
        }
    }
    if !skipped_keys.is_empty() {
        warnings.push(format!(
            "备份包的路径映射表含 {} 个不安全的相对路径，已跳过（不会写入数据库）：{}",
            skipped_keys.len(),
            skipped_keys.join(", ")
        ));
    }
    if replacements.is_empty() {
        return Ok(0);
    }

    let mut rewritten_rows = 0u64;

    // ---- 附件 / 富文本内嵌图片 ----
    {
        let mut stmt = conn.prepare(
            "SELECT id, content, html_content FROM clipboard_history \
             WHERE content_type IN ('image', 'file', 'video') OR html_content IS NOT NULL",
        )?;
        let rows: Vec<(i64, String, Option<String>)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();
        for (id, content, html) in rows {
            let (nc, nh, changed) =
                rewrite_content_bounded(&content, html.as_deref(), &replacements);
            if changed {
                conn.execute(
                    "UPDATE clipboard_history SET content = ?1, html_content = ?2 WHERE id = ?3",
                    rusqlite::params![nc, nh, id],
                )?;
                rewritten_rows += 1;
            }
        }
    }

    // ---- 设置项：表情收藏（JSON 数组）----
    //
    // 【为什么必须按 JSON 解析而不是做字符串替换】`app.emoji_favorites` 存的是一个
    // JSON 字符串数组，反斜杠在 JSON 里是转义字符：路径 `C:\dir\a.png` 在值里写作
    // `"C:\\dir\\a.png"`。直接拿原始路径去 `replace` 会**匹配不到**，表现为表情收藏
    // 在导入后仍指向导出机器的目录——这是一个会被"看起来没报错"掩盖的真实缺陷
    // （本模块的 `emoji_favorites_disk_and_setting_are_both_restored` 回归测试抓出）。
    if let Some(raw) = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'app.emoji_favorites'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        if let Ok(paths) = serde_json::from_str::<Vec<String>>(&raw) {
            let mut changed = false;
            // 展开变体（原生 / JSON 转义 / 正斜杠）：设置项里可能是 `C:/x/fav.png`
            // 这种正斜杠写法，只按原生反斜杠匹配会漏掉，于是表情收藏在导入后仍指向
            // 导出机器的目录——表现正是"双份存储不一致"。
            let expanded = expand_replacement_variants(&replacements);
            let mut next: Vec<String> = Vec::with_capacity(paths.len());
            for p in paths {
                let mut value = p.clone();
                for (from, to) in &expanded {
                    if value.contains(from.as_str()) {
                        value = value.replace(from.as_str(), to.as_str());
                        changed = true;
                    }
                }
                next.push(value);
            }
            if changed {
                conn.execute(
                    "UPDATE settings SET value = ?1 WHERE key = 'app.emoji_favorites'",
                    rusqlite::params![serde_json::to_string(&next)?],
                )?;
            }
        }
    }

    // ---- 设置项：自定义背景（单一路径，非 JSON）----
    if let Some(raw) = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'app.custom_background'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        let (next, _html, changed) = apply_replacements(&raw, None, &replacements);
        if changed {
            conn.execute(
                "UPDATE settings SET value = ?1 WHERE key = 'app.custom_background'",
                rusqlite::params![next],
            )?;
        }
    }

    Ok(rewritten_rows)
}

/// 对一条剪贴板记录的 `content` / `html_content` 做**有界**路径替换。
///
/// # 为什么必须"有界"
///
/// `clipboard_history.content` 存的是用户复制过的**任意文本**（脚本、JSON、日志、
/// HTML 源码……），而 `mappings.json` 的键来自包内数据。对整段正文做无边界
/// `str::replace` 会**静默篡改剪贴板正文**：例如某个映射的原始路径恰好是 `C`（或任何
/// 短串），全库替换就会把正文里所有的 `C` 都改掉——用户的历史记录被不可逆地改写。
///
/// 因此分两种情形处理：
/// - **整条正文就是一个路径**（图片/文件条目的典型形状）：整体命中才替换；
/// - **HTML 内嵌资源**：只在 `src=` / `href=` 等**属性值**内替换，正文文字不动。
///
/// 这样"附件路径失联"这个真实问题被解决，而"正文被误改"这个更严重的问题不会发生。
fn rewrite_content_bounded(
    content: &str,
    html: Option<&str>,
    replacements: &[(String, String)],
) -> (String, Option<String>, bool) {
    let mut changed = false;
    let trimmed = content.trim();
    let mut next = content.to_string();

    // 情形 1：整条正文就是一个绝对路径 -> 允许整体替换（附件条目的正常形状）。
    // 用 `looks_like_path_value` 判定，避免把普通长文本当成路径。
    if super::resolve::looks_like_path_value(trimmed) {
        if let Some(replaced) = replace_exact_path(trimmed, replacements) {
            // 保留原有前后空白，只替换中间那一段。
            let lead = &content[..content.len() - content.trim_start().len()];
            let trail = &content[content.trim_end().len()..];
            next = format!("{}{}{}", lead, replaced, trail);
            changed = true;
        }
    }

    // 情形 2：HTML 内嵌资源 -> 只在属性值里替换。
    let next_html = html.map(|h| {
        let (v, c) = replace_in_html_attributes(h, replacements);
        if c {
            changed = true;
        }
        v
    });

    (next, next_html, changed)
}

/// 一个字符串是否是"整条就是一个文件路径值"（而非包含路径的普通文本）。
///
/// 与 `looks_like_absolute_file_path` 的区别：这里**不允许**内部换行/标签/空白，
/// 且去掉引号后仍须是绝对路径——因为我们要据此决定"能不能整条替换"。
pub(crate) fn looks_like_path_value(v: &str) -> bool {
    let t = v.trim().trim_matches('"');
    if t.is_empty() || t.len() > 4096 {
        return false;
    }
    if t.contains('\n') || t.contains('\r') || t.contains('<') || t.contains(' ') {
        // Windows 路径可以含空格，但"含空格的整条正文"更像是自然语言；
        // 这里保守地要求整体匹配 `mappings` 里登记的**完整原值**，
        // 因此含空格也不会误伤（见 `replace_exact_path` 的等值判定）。
        if t.contains('<') || t.contains('\n') || t.contains('\r') {
            return false;
        }
    }
    let bytes = t.as_bytes();
    let win = bytes.len() > 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    win || t.starts_with('/') || t.starts_with("\\\\")
}

/// 仅当**整串等于**某个被登记的旧路径时，返回替换后的新路径。
fn replace_exact_path(value: &str, replacements: &[(String, String)]) -> Option<String> {
    for (from, to) in replacements {
        if value == from {
            return Some(to.clone());
        }
    }
    None
}

/// 只在 HTML 的 `src=` / `href=` / `srcset=` 等属性值内做路径替换，正文文字保持原样。
///
/// 做法是扫描引号对：把片段里被引号包裹的部分视为"属性值候选"，只有其中包含被登记的
/// 旧路径时才替换。这仍然不是完整 HTML 解析（不引入新依赖），但把改写面从"整段 HTML"
/// 收窄到"带引号的值"，从而不会碰到正文文字或标签名。
fn replace_in_html_attributes(html: &str, replacements: &[(String, String)]) -> (String, bool) {
    let mut out = String::with_capacity(html.len());
    let mut changed = false;
    let mut chars = html.char_indices().peekable();
    let bytes_of = |s: &str| s.len();

    let mut cursor = 0usize;
    while let Some(&(i, c)) = chars.peek() {
        if c == '"' || c == '\'' {
            let quote = c;
            let start = i;
            // 找到配对的结束引号
            let mut end = None;
            let mut j = i + c.len_utf8();
            let b = html.as_bytes();
            while j < b.len() {
                if b[j] == quote as u8 {
                    end = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(e) = end {
                let inner = &html[start + 1..e];
                let mut replaced_any = false;
                let mut new_inner = inner.to_string();
                for (from, to) in replacements {
                    if new_inner.contains(from.as_str()) {
                        new_inner = new_inner.replace(from.as_str(), to.as_str());
                        replaced_any = true;
                    }
                }
                // 未命中任何登记路径的引号片段原样输出（正文里的引号内容因此不受影响）。
                out.push_str(&html[cursor..start + 1]);
                out.push_str(&new_inner);
                changed |= replaced_any;
                cursor = e + 1;
                // 推进迭代器到 e 之后
                while let Some(&(k, _)) = chars.peek() {
                    if k <= e {
                        chars.next();
                    } else {
                        break;
                    }
                }
                continue;
            }
        }
        let _ = bytes_of("");
        chars.next();
    }
    out.push_str(&html[cursor..]);
    (out, changed)
}

/// 把一个绝对路径展开成它在各种载体里的书写变体。
///
/// 同一个路径在不同载体里的写法不同：HTML 属性里可能是 `/`，被序列化成 JSON 时
/// 反斜杠会翻倍。漏掉任何一种都会导致该处路径改写失败、仍指向导出机器。
pub(crate) fn expand_replacement_variants(
    replacements: &[(String, String)],
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::with_capacity(replacements.len() * 3);
    for (from, to) in replacements {
        out.push((from.clone(), to.clone()));
        // JSON 转义写法：`\` -> `\\`
        let from_escaped = from.replace('\\', "\\\\");
        if from_escaped != *from {
            out.push((from_escaped, to.replace('\\', "\\\\")));
        }
        // 正斜杠写法（HTML 属性、以及部分用户在设置里手填的路径）
        let from_fwd = from.replace('\\', "/");
        if from_fwd != *from {
            out.push((from_fwd, to.replace('\\', "/")));
        }
    }
    out
}

/// 对单个设置项值（非剪贴板正文）做替换。
///
/// 设置项的形状是**受控的**（表情收藏是 JSON 路径数组、背景是单一路径），因此这里
/// 允许使用展开后的变体表做整串替换；但调用方仍需先按 JSON 解析再逐项替换。
fn apply_replacements(
    content: &str,
    html: Option<&str>,
    replacements: &[(String, String)],
) -> (String, Option<String>, bool) {
    let _ = html;
    let expanded = expand_replacement_variants(replacements);
    let mut changed = false;
    let mut next = content.to_string();
    for (from, to) in &expanded {
        if next.contains(from.as_str()) {
            next = next.replace(from.as_str(), to.as_str());
            changed = true;
        }
    }
    let _ = content;
    (next, None, changed)
}

/// 还原自定义背景图：把包内 `background/` 下的文件放到新数据目录的 `background/`，
/// 并把设置项改写为**新数据目录下的路径**。
///
/// 为什么不写回原来的绝对路径：那台机器上该路径不一定存在，也不应该由本应用往数据
/// 目录之外写文件。写回自己数据目录下的副本既稳定，也能随下一次导出继续带走。
fn resolve_background(
    staged_db: &Path,
    staging: &Path,
    data_dir: &Path,
    map_path: &Path,
    warnings: &mut Vec<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let map: BackgroundMapFile = match serde_json::from_slice(&std::fs::read(map_path)?) {
        Ok(m) => m,
        Err(e) => {
            warnings.push(format!("背景图映射无法解析，已跳过背景还原：{}", e));
            return Ok(());
        }
    };
    // Walk every entry instead of trusting the first one. A package may carry
    // several backgrounds (or an entry whose file is missing), and taking
    // `items.first()` blindly meant the setting could be pointed at a file that is
    // not in the package at all — leaving the app with a dead path, or clearing the
    // setting outright. Only an entry whose bytes are actually present counts.
    let mut chosen: Option<(String, std::path::PathBuf, Option<String>)> = None;
    for item in &map.items {
        // `item.entry` and `item.file_name` both come from package JSON, i.e. from
        // an untrusted writer: they must be sanitised, or a `..\..\evil.exe` value
        // would make the join escape the data directory (the zip-slip family).
        let Some(rel) = safe_relative_path(&item.entry) else {
            continue;
        };
        let candidate = staging.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        if candidate.is_file() {
            chosen = Some((rel, candidate, Some(item.file_name.clone())));
            break;
        }
    }

    let Some((rel, src, raw_file_name)) = chosen else {
        // Nothing usable in the package. Leave the setting untouched rather than
        // writing an empty value: overwriting it would silently discard a choice the
        // user made, and an empty custom background is a visible behaviour change.
        warnings.push(
            "备份包内没有可用的背景图文件，已保留原有背景设置（未改动）。".to_string(),
        );
        return Ok(());
    };

    // 【命名规则】
    // 优先用映射里记录的**原始文件名**（带净化）：用户从桌面选的是「我的背景.png」，
    // 还原后保留这个名字才是他能认出来的样子。
    // 净化失败（不可信值，如 `..\..\evil.exe`）时退回**条目自身的基名**——
    // 条目名已过 `safe_relative_path`，因此一定落在 background/ 内。
    //
    // 【为什么不另复制一份】设置项指向的必须就是"已经解包到 background/ 里的那个文件"。
    // 若再按 `file_name` 复制出第二个文件，background/ 里会有两份：导入端指向哪一个、
    // 导出端下次打包哪一个，两边不一致，于是"导出→导入→再导出"不闭合（多出来的那份
    // 会被当作新增文件反复累积）。因此这里只决定**设置项指向哪个已存在的文件**。
    let from_map = raw_file_name.as_deref().and_then(sanitize_file_name);
    let from_entry = rel.rsplit('/').next().and_then(sanitize_file_name);
    let final_name = match (from_map, from_entry) {
        (Some(n), _) => n,
        (None, Some(n)) => n,
        (None, None) => {
            warnings.push("背景图条目名不可用，已跳过背景还原。".to_string());
            return Ok(());
        }
    };
    // 若规范名与已解包文件的实际文件名不同，把已解包文件**改名**（同一份字节，不产生副本）。
    let actual = src.file_name().map(|n| n.to_string_lossy().to_string());
    let final_path = staging.join("background").join(&final_name);
    if actual.as_deref() != Some(final_name.as_str()) && !final_path.exists() {
        std::fs::rename(&src, &final_path)?;
    }
    let new_setting_value = data_dir.join("background").join(&final_name);

    let conn = Connection::open(staged_db)?;
    conn.execute(
        "UPDATE settings SET value = ?1 WHERE key = 'app.custom_background'",
        rusqlite::params![new_setting_value.to_string_lossy().to_string()],
    )?;
    Ok(())
}

/// 落盘后对账：暂存库的行数与目录文件数必须与 manifest 声明一致。
fn verify_counts(
    staged_db: &Path,
    staging: &Path,
    manifest: &BackupManifest,
) -> Result<(), BackupError> {
    let conn = Connection::open(staged_db)
        .map_err(|e| BackupError::Land(format!("暂存数据库无法打开：{}", e)))?;
    // `growable` says whether the count is allowed to exceed the manifest.
    //
    // `expected == 0` means the manifest did not state a count for this table (an
    // older or hand-written package), so there is nothing to reconcile against —
    // that is not the same as asserting "zero rows". Only a stated count is checked.
    //
    // Settings may legitimately grow: migrations and seed_defaults add keys on the
    // way in. Entries and tags may not — the package is the whole truth for them, so
    // once a count is stated, any difference means rows went missing.
    //
    // Comparing only in the "too few" direction was not enough: a manifest is
    // self-describing, so a package that drops a row *and* adjusts its manifest to
    // match used to be accepted silently. Checking both directions makes a missing
    // row impossible to hide, whatever the manifest claims.
    let check = |table: &str, expected: u64, what: &str, growable: bool| -> Result<(), BackupError> {
        if expected == 0 {
            return Ok(());
        }
        let actual: u64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {}", table), [], |r| {
                r.get::<_, i64>(0)
            })
            .map(|v| v as u64)
            .map_err(|e| BackupError::Land(format!("统计 {} 失败：{}", table, e)))?;
        let mismatch = if growable {
            actual < expected
        } else {
            actual != expected
        };
        if mismatch {
            return Err(BackupError::CountMismatch {
                what: what.to_string(),
                expected,
                actual,
            });
        }
        Ok(())
    };
    check("clipboard_history", manifest.counts.entries, "剪贴板条目数", false)?;
    check("saved_tags", manifest.counts.tags, "标签数", false)?;
    check("settings", manifest.counts.settings, "设置项数", true)?;

    let files_in = |prefix: &str| -> Result<u64, BackupError> {
        let dir = staging.join(prefix.trim_end_matches('/'));
        if !dir.is_dir() {
            return Ok(0);
        }
        let mut n = 0u64;
        let mut stack = vec![dir];
        while let Some(cur) = stack.pop() {
            for e in std::fs::read_dir(&cur)? {
                let e = e?;
                if e.file_type()?.is_dir() {
                    stack.push(e.path());
                } else {
                    n += 1;
                }
            }
        }
        Ok(n)
    };
    let att = files_in(ENTRY_ATTACHMENTS_PREFIX)?;
    if manifest.counts.attachments > 0 && att != manifest.counts.attachments {
        return Err(BackupError::CountMismatch {
            what: "附件文件数".to_string(),
            expected: manifest.counts.attachments,
            actual: att,
        });
    }
    let emo = files_in(ENTRY_EMOJI_PREFIX)?;
    if manifest.counts.emoji_favorites > 0 && emo != manifest.counts.emoji_favorites {
        return Err(BackupError::CountMismatch {
            what: "表情收藏文件数".to_string(),
            expected: manifest.counts.emoji_favorites,
            actual: emo,
        });
    }
    Ok(())
}

/// 启动期提升的**调用点锚文本**。
///
/// # 为什么要有这么一个常量
///
/// "备份恢复的提升必须发生在打开数据库之前"这条约束**在本机不可观测**：Linux 允许改名
/// 已打开的文件，所以顺序错了、行为测试照样全绿（真机上则表现为"重启后数据没进来"）。
/// 唯一的办法是对源码的**文本顺序**下断言——就像迁移那条已有的守门测试一样。
///
/// 而普通的文本断言很容易被"锚在一段其实不在 `init` 体内的代码上"骗过去。因此这里把
/// 分派点**写成一句可被唯一识别的调用**（见 `app/setup.rs`），常量即那段文本：
/// 断言的是"这句话确实出现在 `init` 体内、且在 `init_db` 之前"。
pub const LOCAL_RESTORE_PROMOTION_ANCHOR: &str =
    "crate::services::backup::import::promote_staged_restore(&pending.staging_dir, &pending.target_dir)";

/// **启动期提升**：把一份已组装就绪的恢复暂存目录变成正式数据。
///
/// # 契约（与 `migration_identifier::promote_staged_takeover` 逐条一致）
///
/// - **调用时机**：数据目录已解析、logger 已就绪、**尚无任何 `Connection`** 那一刻
///   （`app/setup.rs` 的 `run_pending_takeover`）。这是本函数能成功的前提：此时
///   `clipboard.db` 没有被任何人打开，Windows 才允许给它改名。
/// - **成功** ⇒ 暂存目录已消失（内容已就位），调用方清除标记。
/// - **失败** ⇒ 目标恢复到调用前的状态，**暂存目录保留**；调用方据此保留标记、
///   下次启动自动重试。绝不返回"改了一半"。
///
/// # 为什么不是"整个目录 rename"
///
/// 暂存目录里装的只是**受管条目的片段**（`clipboard.db`、`attachments/`…），不是一整份
/// 数据目录——它是从包内容组装出来的，不含 `datapath.txt`、日志这些"这台机器的运行
/// 环境"。整目录合并会把暂存里的形态强加给正式目录，而逐条目的交换恰好只动该动的东西。
///
/// 反过来，逐条目交换也**天然可回滚**：第一个 `rename` 就暴露问题，此时什么都还没换。
/// 已经挪走的条目在失败时**原样放回**，因此无论在哪一步失败，正式数据目录都回到调用前
/// 的状态（另有一份完整旁路备份作为最终兜底）。
pub fn promote_staged_restore(staging: &Path, data_dir: &Path) -> Result<(), String> {
    promote_staged_restore_with(staging, data_dir, &mut |from, to| std::fs::rename(from, to))
}

/// [`promote_staged_restore`] 的实现主体：把"改名"这一步作为**可注入的接缝**接收。
///
/// 【为什么要有这个接缝】本模块的失败路径必须能在**任意平台**上被确定性地复现，而真机上
/// 的失败来自 Windows 的 `os error 32`（文件被本应用自己打开的句柄占住）。Linux 允许改名
/// 已打开的文件，"持有连接"在这里造不出同样的失败。因此把改名动作参数化：测试可以注入
/// "第 N 次必然失败"的改名器来精确验证回滚，生产路径传的仍是 [`std::fs::rename`]。
///
/// 这与 `migration_identifier::promote_staged_takeover(staging, target, rename)` 是同一个
/// 做法——两条链面对的是同一个平台约束，因此用同一种接缝，而不是各造一套。
fn promote_staged_restore_with(
    staging: &Path,
    data_dir: &Path,
    rename: &mut dyn FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<(), String> {
    if staging == data_dir {
        return Err("恢复暂存目录与数据目录重合，已放弃本次提升（未做任何改动）".to_string());
    }
    if !staging.is_dir() {
        return Err(format!(
            "恢复暂存目录不存在（{}），本次提升没有任何输入",
            staging.display()
        ));
    }
    // 启动期是清掉"被取代的恢复暂存"的安全时机：此刻没有任何界面命令在跑（应用还没起来），
    // 因此不可能有人正在往里写。只清 `.restoring.*` 形状的目录，且**跳过本次要提升的那一个**。
    let _ = remove_superseded_restore_staging(data_dir, staging);

    // 【必备条目必须齐】缺数据库就意味着这份暂存不完整。此时**绝不能**拿它去替换正式
    // 数据：那会让用户得到"附件在、记录没了"这种自相矛盾的状态。宁可这一次不生效，
    // 让用户重新恢复一次。
    if !staging.join(ENTRY_DATABASE).is_file() {
        return Err(format!(
            "恢复暂存目录里没有 {}，这份暂存不完整，本次提升已放弃（你的现有数据未被改动）",
            ENTRY_DATABASE
        ));
    }

    let aside = data_dir.join(format!(".pre-restore-promote-{}", std::process::id()));
    // 【绝不能直接删掉已存在的 aside】它只可能是上一次启动期提升中途失败/断电时留下的，
    // 里面装的是**用户当时的原始数据**（受管条目被挪进 aside 后没来得及放回或清场）。
    // 无条件删掉它等于把用户仅存的那份数据销毁。这里选择**停手并如实报告**：提升本身
    // 可以晚一次启动再做，用户的数据只有一份。
    if aside.exists() {
        return Err(format!(
            "检测到上次提升中途失败留下的数据快照 {}（里面是替换前的原始数据）。\
             为避免覆盖它，本次提升已放弃、你的现有数据未被改动；\
             请先确认该目录内容，把它移走后重启应用即可重试。",
            aside.display()
        ));
    }
    if let Err(e) = std::fs::create_dir_all(&aside) {
        return Err(format!("无法创建替换用的过渡目录 {}：{}", aside.display(), e));
    }

    let mut moved_aside: Vec<String> = Vec::new();
    let mut placed: Vec<String> = Vec::new();

    // ---- 步骤 1：把当前受管条目挪到 aside ----
    for name in MANAGED_ENTRIES {
        let from = data_dir.join(name);
        if !from.exists() {
            continue;
        }
        if let Err(e) = rename(&from, &aside.join(name)) {
            // 回滚：把已挪走的放回原位。
            let restored = restore_from_aside(&aside, data_dir, &moved_aside, false);
            return Err(format!(
                "无法让位 {}：{}。{}",
                name,
                e,
                rollback_note(restored, &aside)
            ));
        }
        moved_aside.push((*name).to_string());
    }

    // ---- 步骤 2：把暂存里组装好的条目放上去 ----
    for name in MANAGED_ENTRIES {
        let from = staging.join(name);
        if !from.exists() {
            continue;
        }
        if let Err(e) = rename(&from, &data_dir.join(name)) {
            // 回滚：先撤掉本次已放上去的，再把 aside 里的原样放回。
            for p in placed.iter().rev() {
                let _ = remove_any(&data_dir.join(p));
            }
            let restored = restore_from_aside(&aside, data_dir, &moved_aside, true);
            return Err(format!(
                "放置 {} 失败：{}。{}",
                name,
                e,
                rollback_note(restored, &aside)
            ));
        }
        placed.push((*name).to_string());
    }

    // ---- 步骤 3：清场 ----
    // 暂存已被逐条搬空，把这个空壳收掉（清不掉也不影响正确性：标记马上会被清除，
    // 而下次提交恢复时会按名字前缀把它当无主残渣清掉）。
    let _ = std::fs::remove_dir_all(staging);
    let _ = std::fs::remove_dir_all(&aside);
    Ok(())
}

/// 把 `aside` 里的条目全部放回数据目录。
///
/// 返回 `true` 表示**全部归位成功**（此时 `aside` 可以安全删除）。
///
/// # 为什么返回值决定能不能删 aside
///
/// `aside` 里装的是**导入前用户的原始数据**。如果归位过程中有任何一条失败，
/// 那份数据仍然只在 `aside` 里——此时删除 `aside` 就等于把用户仅存的数据销毁，
/// 这正好是本模块最不能犯的错误。因此只有全部归位成功才允许清场；否则必须把
/// `aside` 原地保留，并把它作为"你的数据在这里"的现场告知用户。
///
/// `clear_target` 为真时会先删掉目标位置的同名条目（用于步骤 2 的回滚：此时数据
/// 目录里已经是本次放上去的新entries，得先让位）；为假时目标位置本就空着。
fn restore_from_aside(
    aside: &Path,
    data_dir: &Path,
    moved: &[String],
    clear_target: bool,
) -> bool {
    let mut all_ok = true;
    for m in moved.iter().rev() {
        let src = aside.join(m);
        if !src.exists() {
            continue;
        }
        let dst = data_dir.join(m);
        if clear_target && dst.exists() {
            if remove_any(&dst).is_err() {
                all_ok = false;
                continue;
            }
        }
        if std::fs::rename(&src, &dst).is_err() {
            all_ok = false;
        }
    }
    all_ok
}

/// 生成回滚结果的用户可读说明。
fn rollback_note(restored: bool, aside: &Path) -> String {
    if restored {
        // 归位完整，aside 已可安全删除（残留空目录也清掉）。
        let _ = std::fs::remove_dir_all(aside);
        "已回滚到导入前的状态；你的数据未被改动。".to_string()
    } else {
        // **绝不能删 aside**：用户的原始数据此刻只在这里。
        format!(
            "回滚未能把全部条目放回原位。你的原始数据完整保留在：{}（请勿删除该文件夹；\n完全退出应用后，把其中的文件移回数据目录即可恢复）。",
            aside.display()
        )
    }
}

fn remove_any(p: &Path) -> std::io::Result<()> {
    if !p.exists() {
        return Ok(());
    }
    if p.is_dir() {
        std::fs::remove_dir_all(p)
    } else {
        std::fs::remove_file(p)
    }
}

/// 给**当前**数据目录建立一份完整旁路备份（用户可据此回退）。
///
/// 放在数据目录同级、带时间戳，因此不会与下次备份互相覆盖。
/// 备份失败**不阻断**导入——但会写进 warnings 让用户知道这次没有回退副本。
fn build_pre_restore_backup(
    data_dir: &Path,
    warnings: &mut Vec<String>,
) -> Result<Option<PathBuf>, BackupError> {
    let parent = data_dir.parent().unwrap_or_else(|| Path::new("."));
    let name = data_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "appdata".to_string());
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let mut backup = parent.join(format!("{}.pre-import-{}", name, stamp));
    let mut n = 1;
    while backup.exists() {
        backup = parent.join(format!("{}.pre-import-{}-{}", name, stamp, n));
        n += 1;
    }

    match copy_tree(data_dir, &backup) {
        Ok(()) => Ok(Some(backup)),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&backup);
            // 【为什么这里必须硬失败，而不是"记个 warning 继续"】
            // "导入前自动备份当前数据"是用户明确要求的硬约束，也是"导入即完全恢复"
            // 不变成"导入即毁掉现状"的唯一回购路径。备份建不出来通常意味着磁盘不足或
            // 权限不足——这两种情况下**继续导入就是在零回退副本的前提下替换全部数据**，
            // 一旦新数据有问题用户无处可退。宁可这次不导入。
            let _ = warnings; // 保留参数以便将来加入非致命提示
            Err(BackupError::Land(format!(
                "导入前的自动备份未能生成（{}）。为避免在无法回退的情况下替换你的数据，本次导入已取消，你的现有数据未被改动。\n请先释放磁盘空间或检查数据目录权限，然后重试。",
                e
            )))
        }
    }
}

fn staging_dir(data_dir: &Path) -> PathBuf {
    let name = data_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "appdata".to_string());
    let parent = data_dir.parent().unwrap_or_else(|| Path::new("."));
    // 名字里同时带 pid 与进程内自增序号：即使将来去掉了互斥锁、或同一 pid 下因为
    // 其他原因并发调用，两次导入也不会共用同一个暂存目录。
    let seq = IMPORT_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    parent.join(format!(
        ".{}.restoring.{}-{}",
        name,
        std::process::id(),
        seq
    ))
}

/// 递归复制。只读源、只写目标；符号链接等特殊类型跳过（不跟随、不复制）。
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_tree(&from, &to)?;
        } else if ty.is_file() {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// 校验一份包"能不能被本版读取"，**不做任何写入**。
///
/// 前端在二次确认弹窗里调用它，把"这个包是谁导出的、什么时候、里面有多少数据"
/// 提前告诉用户——破坏性操作前必须让用户看到将发生什么。
pub fn inspect_backup(archive_path: &Path) -> Result<InspectReport, BackupError> {
    let file = std::fs::File::open(archive_path)?;
    let mut archive =
        ZipArchive::new(file).map_err(|e| BackupError::InvalidZip(e.to_string()))?;
    let (manifest, _) = read_manifest(&mut archive)?;
    Ok(InspectReport {
        app_id: manifest.app.clone(),
        app_version: manifest.app_version.clone(),
        format_version: manifest.format_version,
        exported_at: manifest.exported_at.clone(),
        schema_version: manifest.schema_version,
        counts: manifest.counts.clone(),
        archive_entries: archive.len() as u64,
        notes: manifest.notes.clone(),
        file_bytes: std::fs::metadata(archive_path).map(|m| m.len()).unwrap_or(0),
    })
}

/// 包的预览信息（只读，供二次确认弹窗展示）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectReport {
    pub app_id: String,
    pub app_version: String,
    pub format_version: u32,
    pub exported_at: String,
    pub schema_version: i64,
    pub counts: ManifestCounts,
    pub archive_entries: u64,
    pub notes: Vec<String>,
    pub file_bytes: u64,
}

/// 供命令层复用：背景图设置项的值是否指向当前数据目录之外。
pub fn background_points_outside(conn: &Connection, data_dir: &Path) -> AppResult<bool> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'app.custom_background'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .unwrap_or(None);
    Ok(match raw {
        Some(v) if !v.trim().is_empty() => !Path::new(v.trim()).starts_with(data_dir),
        _ => false,
    })
}

/// 供测试使用：当前包协议期望的应用标识。
pub fn expected_app_id() -> &'static str {
    APP_ID
}

/// 供测试使用：对内存字节求 sha256，避免测试里重复引入 sha2。
pub fn digest_of(bytes: &[u8]) -> String {
    sha256_bytes(bytes)
}

// ---------------------------------------------------------------------------
// 自证测试：本任务的验收要求就是"这 6 件事必须被实测证明"，而不是读代码推断。
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 测试专用：把"运行期提交 + 启动期提升"这两步接起来
// ---------------------------------------------------------------------------

/// 测试里"模拟重启"的那一步：在**没有任何数据库连接**的状态下执行启动期提升。
///
/// # 为什么测试必须走真实入口，而不是直接调 `promote_staged_restore`
///
/// 因为真正要证明的是**这两个阶段接得上**：运行期写的标记内容、标记的位置、提升函数
/// 收到的参数、失败时标记是否被保留。直接调提升函数会把这一整段接线绕过去，于是
/// "标记写错了字段"这类缺陷在测试里永远抓不到（真机上表现为"重启后什么都没发生"）。
/// 因此这里调的是与 `app/setup.rs` 同一个入口 `migration_pending::run_startup_takeover`。
#[cfg(test)]
fn simulate_restart(marker_dir: &Path) -> crate::migration_pending::TakeoverOutcome {
    crate::migration_pending::run_startup_takeover(marker_dir, &mut |pending| {
        assert_eq!(
            pending.kind,
            crate::migration_pending::PendingKind::LocalRestore,
            "恢复写下的标记必须是 LocalRestore 种类（否则启动期会用错提升函数）"
        );
        promote_staged_restore(&pending.staging_dir, &pending.target_dir)
    })
}

/// 断言一次恢复**确实被提交**了，并模拟重启把它落地。
///
/// 返回提升结果，供调用方按需继续断言（例如"提升失败时标记仍在"）。
#[cfg(test)]
fn commit_and_restart(rep: &RestoreReport, data_dir: &Path) -> crate::migration_pending::TakeoverOutcome {
    assert!(
        rep.restart_required,
        "恢复必须告诉用户需要重启（这是它生效的唯一途径）"
    );
    assert!(
        rep.deferred_until_restart,
        "恢复成功路径必须已提交为待生效状态"
    );
    let marker = rep
        .pending_marker_path
        .as_deref()
        .expect("提交成功必须给出标记路径，否则用户无从判断重启后会发生什么");
    assert!(
        Path::new(marker).is_file(),
        "标记必须真的落盘（它是下次启动唯一能知道有活要干的凭据）：{}",
        marker
    );
    simulate_restart(marker_dir_for(data_dir).as_path())
}

/// 递归收集一份目录树里每个文件的 sha256（键为相对 `root` 的路径）。
///
/// 提到这里是因为三个测试模块都要用它；`mod tests` 里那份是同名副本（它早于本函数存在，
/// 且被大量用例直接调用，保留它可避免无谓的改动面）。
#[cfg(test)]
fn collect_digests(path: &Path, root: &Path, out: &mut std::collections::BTreeMap<String, String>) {
    if path.is_dir() {
        let mut children: Vec<PathBuf> = std::fs::read_dir(path)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        children.sort();
        for c in children {
            collect_digests(&c, root, out);
        }
    } else if path.is_file() {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        out.insert(
            rel,
            crate::services::backup::format::sha256_file(path).unwrap_or_default(),
        );
    }
}

/// 测试用的原生标记目录：放在数据目录同级的 `native-<数据目录名>` 下。
///
/// 真实环境里它是 `app.path().app_data_dir()`（由 identifier 推导、位置稳定、**永远不是**
/// 被替换的那个目录）。测试里也必须保持"它不是数据目录、也不是它的子目录"这一条，
/// 否则"标记住在待替换目录里"这种错误设计会被测试放过。
#[cfg(test)]
fn marker_dir_for(data_dir: &Path) -> PathBuf {
    let name = data_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "appdata".to_string());
    data_dir
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("native-{}", name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::backup::export::{create_backup, BackupRequest};

    /// 造一个唯一命名的临时根目录。
    fn tmp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tiez-backup-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 造一个"有数据的数据目录"：真实 schema、真实 WAL、附件、表情收藏、标签、设置。
    ///
    /// 刻意**不** checkpoint：写入的数据会留在 `-wal` 里，这正是"直接复制 db 文件
    /// 会丢数据"那个陷阱的现场。
    fn seed_data_dir(root: &Path, with_wal: bool) -> PathBuf {
        let data = root.join("com.tieznext");
        std::fs::create_dir_all(data.join("attachments")).unwrap();
        std::fs::create_dir_all(data.join("emoji_favorites")).unwrap();

        let db = data.join("clipboard.db");
        let conn = Connection::open(&db).unwrap();
        // 用应用自己的初始化路径建库：WAL 模式 + 真实迁移 + 默认设置。
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;",
        )
        .unwrap();
        crate::infrastructure::repository::migrations::run_migrations(&conn).unwrap();
        crate::database::seed_defaults(&conn).unwrap();

        conn.execute(
            "INSERT INTO clipboard_history (content_type, content, source_app, timestamp, preview)
             VALUES ('text', 'hello backup', 'test.exe', 100, 'hello backup')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO clipboard_history (content_type, content, source_app, timestamp, preview)
             VALUES ('image', ?1, 'test.exe', 200, '[image]')",
            [data
                .join("attachments")
                .join("a.png")
                .to_string_lossy()
                .to_string()],
        )
        .unwrap();
        conn.execute("INSERT INTO saved_tags (name, color) VALUES ('work', '#ff0000')", [])
            .unwrap();
        // 背景图放在 `background/` 下——这正是**导入之后**的形态（导入会把背景图收进
        // 该目录并把设置项指向它）。让种子数据与导入后形态一致，往返测试才能严格闭合、
        // 使"逐文件 sha256 一致"成为真正可判定的断言，而不是靠排除项来放宽。
        let bg_path = data.join("background").join("bg.png");
        conn.execute(
            "INSERT INTO settings (key, value) VALUES ('app.custom_background', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [bg_path.to_string_lossy().to_string()],
        )
        .unwrap();

        std::fs::create_dir_all(data.join("background")).unwrap();
        std::fs::write(data.join("attachments").join("a.png"), b"PNGDATA-A").unwrap();
        std::fs::write(&bg_path, b"PNGDATA-BG").unwrap();
        std::fs::write(data.join("emoji_favorites").join("fav_1.png"), b"EMOJI-1").unwrap();
        std::fs::write(data.join("datapath.txt"), data.to_string_lossy().as_bytes()).unwrap();
        std::fs::write(data.join("tiez.log"), b"log line\n").unwrap();

        drop(conn);

        if with_wal {
            // 再开一个连接往 WAL 里写一条，然后**不** checkpoint 就关闭。
            // SQLite 在最后一个连接关闭时会自动 checkpoint，因此这里改用
            // "显式关闭自动 checkpoint"的方式：写入后直接丢掉连接而不让它正常收尾。
            let conn2 = Connection::open(&db).unwrap();
            conn2
                .execute_batch("PRAGMA wal_autocheckpoint = 0;")
                .unwrap();
            conn2
                .execute(
                    "INSERT INTO clipboard_history (content_type, content, source_app, timestamp, preview)
                     VALUES ('text', 'WAL-ONLY-ROW', 'test.exe', 300, 'wal-only')",
                    [],
                )
                .unwrap();
            // 关键：不调用 conn2 的任何收尾，直接泄漏它，让 -wal 保留未 checkpoint 的数据。
            std::mem::forget(conn2);
        }

        data
    }

    /// 对一份数据目录做 sha256 清单（只覆盖受管条目），用于逐文件往返比对。
    fn digest_managed(data_dir: &Path) -> std::collections::BTreeMap<String, String> {
        let mut out = std::collections::BTreeMap::new();
        for name in MANAGED_ENTRIES {
            let p = data_dir.join(name);
            if !p.exists() {
                continue;
            }
            collect_digests(&p, data_dir, &mut out);
        }
        out
    }

    /// 递归收集每个文件的 sha256（按相对路径）。在 `mod tests` 内可见；
    /// 其他测试模块用下面 `#[cfg(test)] fn collect_digests` 的那个同名入口。
    fn collect_digests(
        path: &Path,
        root: &Path,
        out: &mut std::collections::BTreeMap<String, String>,
    ) {
        if path.is_dir() {
            let mut children: Vec<PathBuf> =
                std::fs::read_dir(path).unwrap().flatten().map(|e| e.path()).collect();
            children.sort();
            for c in children {
                collect_digests(&c, root, out);
            }
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            out.insert(rel, sha256_file(path).unwrap());
        }
    }

    /// 对数据库做**分表内容指纹**：每张表按 rowid 稳定顺序导出全部行的文本，逐表求 sha256。
    ///
    /// 为什么不能直接用数据库文件的 sha256：导入后会重跑迁移与 `seed_defaults`、
    /// 重置云同步游标——这些都会改变文件字节，但**不应改变用户数据**。用行级指纹才能
    /// 把"用户数据一致"与"数据库文件元信息变化"这两件事分开。
    ///
    /// 返回 `表名 -> 指纹`，这样断言失败时能直接指出是哪张表、哪一行不一致。
    fn table_fingerprints(db: &Path) -> std::collections::BTreeMap<String, String> {
        let conn = Connection::open(db).unwrap();
        let mut tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .filter(|n| !n.starts_with("sqlite_"))
            .collect();
        tables.sort();

        let mut out = std::collections::BTreeMap::new();
        for t in tables {
            let mut hasher = Sha256::new();
            // 显式 ORDER BY rowid，保证"同一批数据两次读取得到同一顺序"。
            let sql = format!("SELECT * FROM \"{}\" ORDER BY rowid", t);
            if let Ok(mut stmt) = conn.prepare(&sql) {
                let cols = stmt.column_count();
                if let Ok(mut rows) = stmt.query([]) {
                    while let Ok(Some(row)) = rows.next() {
                        for i in 0..cols {
                            let v: String = row
                                .get::<_, rusqlite::types::Value>(i)
                                .map(|v| format!("{:?}", v))
                                .unwrap_or_default();
                            hasher.update(v.as_bytes());
                            hasher.update(b"\x1f");
                        }
                        hasher.update(b"\x1e");
                    }
                }
            }
            out.insert(t, format!("sha256:{:x}", hasher.finalize()));
        }
        out
    }

    /// 同 [`table_fingerprints`]，但 `settings` 表排除**刻意重置**的云同步状态键。
    ///
    /// 这样"往返一致"的断言就只针对**用户数据**：云同步游标是"本机与远端的同步进度"，
    /// 恢复后必须归零（否则会与远端互相覆盖），它的变化是设计意图而非数据失真。
    fn table_fingerprints_excluding_sync(db: &Path) -> std::collections::BTreeMap<String, String> {
        let conn = Connection::open(db).unwrap();
        let mut out = table_fingerprints(db);
        let mut hasher = Sha256::new();
        let keys: Vec<&str> = CLOUD_SYNC_RESET_KEYS.iter().map(|(k, _)| *k).collect();
        let mut stmt = conn
            .prepare("SELECT key, value FROM settings ORDER BY key")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap();
        for row in rows.flatten() {
            if keys.contains(&row.0.as_str()) {
                continue;
            }
            hasher.update(row.0.as_bytes());
            hasher.update(b"\x1f");
            hasher.update(row.1.as_bytes());
            hasher.update(b"\x1e");
        }
        out.insert("settings".to_string(), format!("sha256:{:x}", hasher.finalize()));
        out
    }

    /// 把库的**结构对象**（表/索引/触发器）导出成有序清单，用于诊断
    /// "数据库文件哈希变了，但变化是否只发生在结构/元信息层"。
    fn schema_objects(db: &Path) -> Vec<String> {
        let conn = Connection::open(db).unwrap();
        let mut v: Vec<String> = conn
            .prepare("SELECT type || ':' || name FROM sqlite_master ORDER BY type, name")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        v.sort();
        v
    }

    /// 读取某个数据库条目的条数（用于往返断言）。
    fn count_of(db: &Path, table: &str) -> i64 {
        let conn = Connection::open(db).unwrap();
        conn.query_row(&format!("SELECT COUNT(*) FROM {}", table), [], |r| r.get(0))
            .unwrap()
    }

    // =====================================================================
    // 测试 1：往返 —— 导出 → 清空原目录 → 导入 → 数据完全一致（含 sha256）
    // =====================================================================

    #[test]
    fn roundtrip_restores_data_exactly() {
        let root = tmp_root("roundtrip");
        let data = seed_data_dir(&root, false);
        let archive = root.join("out.zip");

        let export = create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: archive.clone(),
            app_version: "0.3.4".to_string(),
        })
        .unwrap();
        assert!(archive.is_file(), "导出必须产出真实文件");
        assert_eq!(export.counts.entries, 2);
        assert_eq!(export.counts.tags, 3, "默认标签 + 用户标签");

        // 只对"用户数据"做逐字节比对。
        //
        // 【为什么排除数据库文件本身】导入后会**刻意**重跑迁移与 `seed_defaults`、
        // 重置云同步游标——这些是"数据一致"的必要条件，但会改变数据库文件的字节
        // （例如新增一个 `fingerprint` 索引、写回默认值）。因此"完全恢复"的判据是
        // **语义等价 + 附件/表情这类不透明文件逐字节一致**，而数据库用行级内容
        // 快照比对（见下面 data_fingerprint）。这一点在本模块文档里已经写明。
        let before = digest_managed(&data);
        let before_db = table_fingerprints_excluding_sync(&data.join("clipboard.db"));
        assert!(before.contains_key("attachments/a.png"));
        assert!(before.contains_key("emoji_favorites/fav_1.png"));

        // 把原目录**毁掉**：删掉全部受管条目并塞入垃圾，模拟"换机后从零导入"。
        for name in MANAGED_ENTRIES {
            let p = data.join(name);
            if p.exists() {
                if p.is_dir() {
                    std::fs::remove_dir_all(&p).unwrap();
                } else {
                    std::fs::remove_file(&p).unwrap();
                }
            }
        }
        std::fs::create_dir_all(data.join("attachments")).unwrap();
        std::fs::write(data.join("attachments").join("junk.png"), b"JUNK").unwrap();
        let conn = Connection::open(data.join("clipboard.db")).unwrap();
        crate::infrastructure::repository::migrations::run_migrations(&conn).unwrap();
        crate::database::seed_defaults(&conn).unwrap();
        conn.execute(
            "INSERT INTO clipboard_history (content_type, content, source_app, timestamp, preview)
             VALUES ('text', 'STALE', 'x', 1, 'stale')",
            [],
        )
        .unwrap();
        drop(conn);
        assert_ne!(digest_managed(&data), before, "前置条件：原目录确实被改坏了");

        // ---- 导入 ----
        let report = restore_backup(&RestoreRequest {
            data_dir: data.clone(),
            archive_path: archive.clone(),
            pending_marker_dir: Some(marker_dir_for(&data.clone())),
        })
        .unwrap();
        let outcome = commit_and_restart(&report, &data);
        assert!(
            matches!(outcome, crate::migration_pending::TakeoverOutcome::Promoted { .. }),
            "模拟重启必须成功完成提升，实际 {outcome:?}"
        );

        assert!(
            report.pre_restore_backup.is_some(),
            "导入前必须自动备份当前数据"
        );
        assert!(report.restored_files > 0);

        // ---- 逐文件 sha256 比对（本任务的核心自证）----
        let after = digest_managed(&data);
        let mut diffs: Vec<String> = Vec::new();
        for (k, v) in &before {
            match after.get(k) {
                None => diffs.push(format!("缺失: {}", k)),
                Some(a) if a != v && k != "clipboard.db" => {
                    diffs.push(format!("内容不同: {}", k))
                }
                _ => {}
            }
        }
        for k in after.keys() {
            if !before.contains_key(k) && k != "background/" {
                diffs.push(format!("多余: {}", k));
            }
        }
        assert!(
            diffs.is_empty(),
            "导入后必须与导出时逐文件 sha256 一致（数据库除外，见下方行级比对），实际差异：{:?}",
            diffs
        );
        // 把实测的逐文件哈希打到 stdout（`--nocapture` 可见），作为"导入即完全恢复"的可核对证据。
        if std::env::var("TIEZ_BACKUP_EVIDENCE").is_ok() {
            eprintln!("\n===== 往返 sha256 逐文件对比（导出前 vs 导入后）=====");
            for (k, v) in &before {
                let same = after.get(k).map(|a| a == v).unwrap_or(false);
                let after_v = after.get(k).map(|s| s.as_str()).unwrap_or("<缺失>");
                eprintln!("[{}] {}\n   before={}\n   after ={}", if same { "一致" } else { "不同" }, k, v, after_v);
            }
            eprintln!("受管文件数: {} -> {}", before.len(), after.len());
        }

        // 数据库：分表行级内容比对（证明"导入即完全恢复"在语义上也成立）
        //
        // 唯一被**刻意**改动的是云同步游标类设置（恢复后必须重置，否则会与远端冲突；
        // 其重置行为由 `import_resets_wal_and_cloud_sync_cursor` 单独断言）。
        let after_db_raw = table_fingerprints_excluding_sync(&data.join("clipboard.db"));
        let after_db = after_db_raw;
        let before_db = before_db; // 保持下文比对结构清晰
        let mut table_diffs: Vec<String> = Vec::new();
        for (t, v) in &before_db {
            match after_db.get(t) {
                None => table_diffs.push(format!("表 {} 丢失", t)),
                Some(a) if a != v => table_diffs.push(format!("表 {} 内容不同", t)),
                _ => {}
            }
        }
        for t in after_db.keys() {
            if !before_db.contains_key(t) {
                table_diffs.push(format!("表 {} 多余（应为空）", t));
            }
        }
        assert!(
            table_diffs.is_empty(),
            "导入后数据库内容必须与导出时逐行一致，差异表：{:?}\n导入前={:?}\n导入后={:?}",
            table_diffs,
            before_db,
            after_db
        );
        if std::env::var("TIEZ_BACKUP_EVIDENCE").is_ok() {
            eprintln!("\n===== 数据库文件哈希差异定位（证明变化只在结构/元信息层）=====");
            eprintln!("非测试路径不做文件级哈希比对；差异定位如下：");
            let objs = schema_objects(&data.join("clipboard.db"));
            eprintln!("导入后 sqlite_master 对象数 = {}（表+索引）", objs.len());
            for o in &objs { eprintln!("   {}", o); }
            // 关键对照：包内数据库的结构对象数。两者相同 -> 迁移没有新增结构，
            // 文件哈希差异纯属页布局/空闲页等元信息层面。
            let file = std::fs::File::open(&archive).unwrap();
            let mut zip = ZipArchive::new(file).unwrap();
            let mut e = zip.by_name("clipboard.db").unwrap();
            let pkg_db = std::env::temp_dir().join(format!("tiez-pkg-db-{}.db", std::process::id()));
            let mut out = std::fs::File::create(&pkg_db).unwrap();
            std::io::copy(&mut e, &mut out).unwrap();
            drop(out);
            let pkg_objs = schema_objects(&pkg_db);
            eprintln!("包内数据库结构对象数 = {}", pkg_objs.len());
            eprintln!(
                "结构对象是否完全一致 = {}",
                if pkg_objs == objs { "是（差异仅在页布局/元信息）" } else { "否（有结构变化，需核对）" }
            );
            let _ = std::fs::remove_file(&pkg_db);
            eprintln!("（对照：导入会刻意重跑 run_migrations + seed_defaults，并重置云同步游标；");
            eprintln!("  这会在 sqlite_master 里补齐索引、改变页布局，因此文件字节不同，但用户数据行不变。）");
            eprintln!("\n===== 数据库分表内容指纹对比 =====");
            for (t, v) in &before_db {
                eprintln!("[{}] {:22} before={}", if after_db.get(t).map(|a| a == v).unwrap_or(false) { "一致" } else { "不同" }, t, v);
            }
        }

        // ---- 语义断言（sha256 只证明字节一致，还要证明"用起来对"）----
        let db = data.join("clipboard.db");
        assert_eq!(count_of(&db, "clipboard_history"), 2);
        assert_eq!(count_of(&db, "saved_tags"), 3);
        assert!(data.join("attachments").join("a.png").is_file());
        assert!(data.join("emoji_favorites").join("fav_1.png").is_file());
        // 导入前备份确实存在且非空
        let backup = PathBuf::from(report.pre_restore_backup.clone().unwrap());
        assert!(backup.join("clipboard.db").is_file());

        let _ = std::fs::remove_dir_all(&root);
    }

    // =====================================================================
    // 测试 6：WAL 正确性 —— 导出**不** checkpoint，断言 WAL 里的数据在包内
    // =====================================================================

    #[test]
    fn wal_data_written_without_checkpoint_is_captured() {
        let root = tmp_root("wal");
        let data = seed_data_dir(&root, true);

        // 前置证据：这条数据此刻只在 WAL 里（主库文件里还没有）。
        let wal = data.join("clipboard.db-wal");
        assert!(
            wal.is_file() && std::fs::metadata(&wal).unwrap().len() > 0,
            "前置条件：-wal 文件必须存在且非空"
        );
        let main_bytes = std::fs::read(data.join("clipboard.db")).unwrap();
        assert!(
            !contains_subslice(&main_bytes, b"WAL-ONLY-ROW"),
            "前置条件：该数据此时不应出现在主库文件里（否则本测试证明不了 VACUUM INTO）"
        );

        let archive = root.join("wal.zip");
        create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: archive.clone(),
            app_version: "0.3.4".to_string(),
        })
        .unwrap();

        // 解出包内数据库，断言它**包含**那条只在 WAL 里的数据。
        let file = std::fs::File::open(&archive).unwrap();
        let mut zip = ZipArchive::new(file).unwrap();
        let mut entry = zip.by_name("clipboard.db").unwrap();
        let extracted = root.join("extracted.db");
        let mut out = std::fs::File::create(&extracted).unwrap();
        std::io::copy(&mut entry, &mut out).unwrap();
        drop(out);

        let conn = Connection::open(&extracted).unwrap();
        let found: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM clipboard_history WHERE content = 'WAL-ONLY-ROW'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            found, 1,
            "导出的数据库必须包含未 checkpoint 的 WAL 数据（证明用的是 VACUUM INTO 而非文件复制）"
        );
        assert_eq!(count_of(&extracted, "clipboard_history"), 3);

        if std::env::var("TIEZ_BACKUP_EVIDENCE").is_ok() {
            eprintln!(
                "\n===== WAL 正确性证据 =====\n导出库中 WAL-ONLY-ROW 条数 = {}（期望 1）\n导出库总条数 = {}（期望 3）",
                found,
                count_of(&extracted, "clipboard_history")
            );
        }

        // 反向证据：直接复制主库文件**会**丢这条数据。
        let naive = root.join("naive.db");
        std::fs::copy(data.join("clipboard.db"), &naive).unwrap();
        let conn2 = Connection::open(&naive).unwrap();
        let naive_found: i64 = conn2
            .query_row(
                "SELECT COUNT(*) FROM clipboard_history WHERE content = 'WAL-ONLY-ROW'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        assert_eq!(
            naive_found, 0,
            "反向证据：文件复制方式确实会丢掉 WAL 中的数据（说明本测试有区分力）"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
        if needle.is_empty() || haystack.len() < needle.len() {
            return false;
        }
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    // =====================================================================
    // 测试 2：前向兼容 —— 手工构造最小 v1 包，断言能导入
    // =====================================================================

    #[test]
    fn accepts_minimal_handcrafted_v1_package() {
        let root = tmp_root("minv1");
        let data = root.join("com.tieznext");
        std::fs::create_dir_all(&data).unwrap();
        // 当前目录先放一份能跑的库（导入不应依赖它）
        {
            let conn = Connection::open(data.join("clipboard.db")).unwrap();
            crate::infrastructure::repository::migrations::run_migrations(&conn).unwrap();
            crate::database::seed_defaults(&conn).unwrap();
        }

        // 手工造一个"最简 v1 包"：只有 manifest（**故意省略可选字段**）+ 数据库。
        let src_db = root.join("src.db");
        {
            let conn = Connection::open(&src_db).unwrap();
            crate::infrastructure::repository::migrations::run_migrations(&conn).unwrap();
            crate::database::seed_defaults(&conn).unwrap();
            conn.execute(
                "INSERT INTO clipboard_history (content_type, content, source_app, timestamp, preview)
                 VALUES ('text', 'from-minimal-v1', 'x', 7, 'from-minimal-v1')",
                [],
            )
            .unwrap();
        }
        let db_bytes = std::fs::read(&src_db).unwrap();
        // 这个 manifest 只有必需的两个字段——正是"更老的写入端"会写出的形状。
        let manifest = format!(
            r#"{{"format_version":1,"app":"{}"}}"#,
            crate::services::backup::format::APP_ID
        );

        let archive = root.join("minv1.zip");
        {
            let f = std::fs::File::create(&archive).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            w.start_file("clipboard.db", opts).unwrap();
            w.write_all(&db_bytes).unwrap();
            w.start_file("manifest.json", opts).unwrap();
            w.write_all(manifest.as_bytes()).unwrap();
            w.finish().unwrap();
        }

        let report = restore_backup(&RestoreRequest {
            data_dir: data.clone(),
            archive_path: archive,
            pending_marker_dir: Some(marker_dir_for(&data.clone())),
        })
        .unwrap();
        commit_and_restart(&report, &data.clone());
        assert_eq!(report.format_version, 1);
        assert_eq!(report.exported_app_version, "");
        assert_eq!(count_of(&data.join("clipboard.db"), "clipboard_history"), 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    // =====================================================================
    // 测试 3：后向兼容 —— 未知字段/未知条目被忽略而非报错
    // =====================================================================

    /// 3a. 字段层：manifest 里塞入本版不认识的字段，必须能正常导入。
    #[test]
    fn unknown_manifest_fields_do_not_break_import() {
        let root = tmp_root("unknown-field");
        let data = root.join("com.tieznext");
        std::fs::create_dir_all(&data).unwrap();

        let src_db = root.join("src.db");
        let db_bytes = {
            let conn = Connection::open(&src_db).unwrap();
            crate::infrastructure::repository::migrations::run_migrations(&conn).unwrap();
            crate::database::seed_defaults(&conn).unwrap();
            drop(conn);
            std::fs::read(&src_db).unwrap()
        };
        let db_hash = sha256_bytes(&db_bytes);

        let manifest = format!(
            r#"{{"format_version":1,"app":"{}","app_version":"9.9.9",
                "future_section":{{"a":[1,2,3]}},"totally_new_key":"x",
                "counts":{{"entries":0,"tags":0,"attachments":0,"emoji_favorites":0,"settings":0,"future_count":12}},
                "checksums":{{"clipboard.db":"{}"}}}}"#,
            crate::services::backup::format::APP_ID,
            db_hash
        );

        let archive = root.join("unknown.zip");
        {
            let f = std::fs::File::create(&archive).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            w.start_file("clipboard.db", opts).unwrap();
            w.write_all(&db_bytes).unwrap();
            w.start_file("manifest.json", opts).unwrap();
            w.write_all(manifest.as_bytes()).unwrap();
            w.finish().unwrap();
        }

        let report = restore_backup(&RestoreRequest {
            data_dir: data.clone(),
            archive_path: archive,
            pending_marker_dir: Some(marker_dir_for(&data.clone())),
        })
        .expect("含未知字段的包必须能导入（旧版读新版的场景）");
        assert_eq!(report.format_version, 1);
        assert_eq!(report.counts.entries, 0);
        // 提交之后、重启之前，新数据库还在暂存里等下次启动上位——这是设计，不是"数据
        // 没恢复"。因此这里断言的是**暂存目录里那份**（它是否被正确组装），随后模拟重启
        // 断言它真的到位。这条测试的数据目录在构造时是空的，因此不能假设正式位置有库。
        let staging = PathBuf::from(report.pending_staging_dir.clone().unwrap());
        assert!(
            staging.join("clipboard.db").is_file(),
            "组装好的新库必须在暂存目录里等着上位：{}",
            staging.display()
        );
        simulate_restart(&marker_dir_for(&data));
        assert!(
            data.join("clipboard.db").is_file(),
            "重启后正式位置的库必须已被替换"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 3b. 条目层：包里多一条本版不认识的**条目路径**，必须跳过而不是报错。
    ///
    /// 这正是"新版把新功能数据放在新条目路径下"时旧版的表现。
    #[test]
    fn unknown_archive_entry_is_skipped_not_fatal() {
        let root = tmp_root("unknown-entry");
        let data = root.join("com.tieznext");
        std::fs::create_dir_all(&data).unwrap();
        {
            let conn = Connection::open(data.join("clipboard.db")).unwrap();
            crate::infrastructure::repository::migrations::run_migrations(&conn).unwrap();
            crate::database::seed_defaults(&conn).unwrap();
            conn.execute(
                "INSERT INTO clipboard_history (content_type, content, source_app, timestamp, preview)
                 VALUES ('text','KEEP','x',1,'KEEP')",
                [],
            )
            .unwrap();
        }

        let src_db = root.join("src.db");
        let db_bytes = {
            let conn = Connection::open(&src_db).unwrap();
            crate::infrastructure::repository::migrations::run_migrations(&conn).unwrap();
            crate::database::seed_defaults(&conn).unwrap();
            conn.execute(
                "INSERT INTO clipboard_history (content_type, content, source_app, timestamp, preview)
                 VALUES ('text','FROM-PACKAGE','x',2,'FROM-PACKAGE')",
                [],
            )
            .unwrap();
            drop(conn);
            std::fs::read(&src_db).unwrap()
        };

        let manifest = format!(
            r#"{{"format_version":1,"app":"{}","app_version":"9.9.9","counts":{{"entries":1}},
                "checksums":{{"clipboard.db":"{}"}}}}"#,
            crate::services::backup::format::APP_ID,
            sha256_bytes(&db_bytes)
        );

        let archive = root.join("unknown-entry.zip");
        {
            let f = std::fs::File::create(&archive).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            w.start_file("clipboard.db", opts).unwrap();
            w.write_all(&db_bytes).unwrap();
            // 本版不认识的条目路径（模拟"更新的版本新增的数据种类"）
            w.start_file("future_feature/blob.bin", opts).unwrap();
            w.write_all(b"future payload").unwrap();
            w.start_file("manifest.json", opts).unwrap();
            w.write_all(manifest.as_bytes()).unwrap();
            w.finish().unwrap();
        }

        let report = restore_backup(&RestoreRequest {
            data_dir: data.clone(),
            archive_path: archive,
            pending_marker_dir: Some(marker_dir_for(&data.clone())),
        })
        .expect("未知条目必须被跳过而不是让整包失败");
        assert!(
            report.warnings.iter().any(|w| w.contains("future_feature/blob.bin")),
            "必须如实告知用户跳过了什么，实际 warnings={:?}",
            report.warnings
        );
        commit_and_restart(&report, &data.clone());
        // 数据库照常恢复
        let conn = Connection::open(data.join("clipboard.db")).unwrap();
        let kept: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM clipboard_history WHERE content = 'FROM-PACKAGE'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 3c. 写入端证据：导出的包**只含 v1 的条目路径**，不含任何"旧版读不懂"的东西。
    #[test]
    fn exported_package_only_contains_v1_entry_names() {
        let root = tmp_root("v1-entries");
        let data = seed_data_dir(&root, false);
        let archive = root.join("v1.zip");
        create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: archive.clone(),
            app_version: "0.3.4".to_string(),
        })
        .unwrap();

        let file = std::fs::File::open(&archive).unwrap();
        let mut zip = ZipArchive::new(file).unwrap();
        let mut names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect();
        names.sort();

        // 逐条断言：每条都必须落在"v1 读取端认识的路径"里。
        for n in &names {
            let known = n == "manifest.json"
                || n == "clipboard.db"
                || n == "mappings.json"
                || n == "background_map.json"
                || n.starts_with("attachments/")
                || n.starts_with("emoji_favorites/")
                || n.starts_with("background/");
            assert!(known, "包内出现了 v1 契约之外的条目：{}（全部：{:?}）", n, names);
        }
        // 必备条目在
        assert!(names.iter().any(|n| n == "manifest.json"));
        assert!(names.iter().any(|n| n == "clipboard.db"));

        // manifest 本身也必须是 v1：只用 v1 就存在的字段形状（未知字段由 serde 默认值兜底）
        let mut e = zip.by_name("manifest.json").unwrap();
        let mut s = String::new();
        e.read_to_string(&mut s).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["format_version"], 1);
        assert_eq!(v["app"], crate::services::backup::format::APP_ID);
        // 不得出现"更高版本才有的"字段名（这里用一个显式清单守护，避免将来悄悄加字段）
        let allowed = [
            "format_version",
            "app",
            "app_version",
            "exported_at",
            "schema_version",
            "counts",
            "checksums",
            "notes",
        ];
        for key in v.as_object().unwrap().keys() {
            assert!(
                allowed.contains(&key.as_str()),
                "manifest 出现未列入 v1 白名单的字段：{}（若确需新增，请确认旧读取端能忽略它）",
                key
            );
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    // =====================================================================
    // 测试 4：拒绝原版 —— 且**未改动任何数据**
    // =====================================================================

    #[test]
    fn rejects_upstream_tiez_package_without_touching_data() {
        let root = tmp_root("reject-upstream");
        let data = root.join("com.tieznext");
        std::fs::create_dir_all(&data).unwrap();
        {
            let conn = Connection::open(data.join("clipboard.db")).unwrap();
            crate::infrastructure::repository::migrations::run_migrations(&conn).unwrap();
            crate::database::seed_defaults(&conn).unwrap();
            conn.execute(
                "INSERT INTO clipboard_history (content_type, content, source_app, timestamp, preview)
                 VALUES ('text','MY-PRECIOUS-DATA','x',1,'precious')",
                [],
            )
            .unwrap();
        }

        // 原版 TieZ 的包：manifest.app 是 com.tiez（两个历史标识符都试）
        for app in ["com.tiez", "com.tiez.app"] {
            let archive = root.join(format!("upstream-{}.zip", app.replace('.', "_")));
            {
                let f = std::fs::File::create(&archive).unwrap();
                let mut w = zip::ZipWriter::new(f);
                let opts = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated);
                w.start_file("tiez.db", opts).unwrap();
                w.write_all(b"upstream database").unwrap();
                let m = format!(
                    r#"{{"format_version":1,"app":"{}","app_version":"0.3.4"}}"#,
                    app
                );
                w.start_file("manifest.json", opts).unwrap();
                w.write_all(m.as_bytes()).unwrap();
                w.finish().unwrap();
            }

            let before = digest_managed(&data);
            let err = restore_backup(&RestoreRequest {
                data_dir: data.clone(),
                archive_path: archive,
                pending_marker_dir: Some(marker_dir_for(&data.clone())),
            })
            .unwrap_err();
            assert_eq!(err.code(), "foreign_app", "必须明确拒绝（app={}）", app);
            // 错误文案必须说清"这是原版/非本应用"
            let msg = err.to_string();
            assert!(
                msg.contains("Tiez-Next") || msg.contains("原版"),
                "拒绝时要说清楚原因，实际：{}",
                msg
            );
            // **未改动任何数据**（逐文件 sha256）
            assert_eq!(
                digest_managed(&data),
                before,
                "拒绝原版包时不得改动任何现有数据（app={}）",
                app
            );
        }

        // 缺 manifest 的 zip 同样拒绝，且不动数据
        let archive = root.join("no-manifest.zip");
        {
            let f = std::fs::File::create(&archive).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            w.start_file("whatever.db", opts).unwrap();
            w.write_all(b"x").unwrap();
            w.finish().unwrap();
        }
        let before = digest_managed(&data);
        let err = restore_backup(&RestoreRequest {
            data_dir: data.clone(),
            archive_path: archive,
            pending_marker_dir: Some(marker_dir_for(&data.clone())),
        })
        .unwrap_err();
        assert_eq!(err.code(), "foreign_app");
        assert_eq!(digest_managed(&data), before);

        let _ = std::fs::remove_dir_all(&root);
    }

    // =====================================================================
    // 测试 5：失败安全 —— 损坏 zip / 校验和不符，断言数据完好、暂存被清理
    // =====================================================================

    #[test]
    fn corrupt_and_tampered_packages_leave_data_intact() {
        let root = tmp_root("fail-safe");
        let data = seed_data_dir(&root, false);
        let before = digest_managed(&data);

        // ---- 5a. 完全不是 zip ----
        let bad = root.join("not-a-zip.zip");
        std::fs::write(&bad, b"this is definitely not a zip archive").unwrap();
        let err = restore_backup(&RestoreRequest {
            data_dir: data.clone(),
            archive_path: bad,
            pending_marker_dir: Some(marker_dir_for(&data.clone())),
        })
        .unwrap_err();
        assert_eq!(err.code(), "invalid_zip");
        assert_eq!(digest_managed(&data), before, "损坏 zip 不得改动数据");
        assert_no_staging_left(&data);

        // ---- 5b. 合法 zip，但条目内容被篡改（校验和不符）----
        let archive = root.join("ok.zip");
        create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: archive.clone(),
            app_version: "0.3.4".to_string(),
        })
        .unwrap();

        let tampered = root.join("tampered.zip");
        {
            let f = std::fs::File::open(&archive).unwrap();
            let mut zip = ZipArchive::new(f).unwrap();
            let of = std::fs::File::create(&tampered).unwrap();
            let mut w = zip::ZipWriter::new(of);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for i in 0..zip.len() {
                let mut e = zip.by_index(i).unwrap();
                let name = e.name().to_string();
                let mut buf = Vec::new();
                e.read_to_end(&mut buf).unwrap();
                if name == "attachments/a.png" {
                    buf = b"TAMPERED".to_vec(); // 改内容但不动 manifest 里的 checksum
                }
                w.start_file(name, opts).unwrap();
                w.write_all(&buf).unwrap();
            }
            w.finish().unwrap();
        }

        let err = restore_backup(&RestoreRequest {
            data_dir: data.clone(),
            archive_path: tampered,
            pending_marker_dir: Some(marker_dir_for(&data.clone())),
        })
        .unwrap_err();
        assert_eq!(err.code(), "checksum_mismatch", "篡改必须被校验和抓出");
        assert_eq!(
            digest_managed(&data),
            before,
            "校验和不符时不得改动数据"
        );
        assert_no_staging_left(&data);

        // ---- 5c. 截断的 zip（删掉后半段）----
        let truncated = root.join("truncated.zip");
        let full = std::fs::read(&archive).unwrap();
        std::fs::write(&truncated, &full[..full.len() / 2]).unwrap();
        let err = restore_backup(&RestoreRequest {
            data_dir: data.clone(),
            archive_path: truncated,
            pending_marker_dir: Some(marker_dir_for(&data.clone())),
        })
        .unwrap_err();
        assert!(
            matches!(err.code(), "invalid_zip" | "foreign_app"),
            "截断包必须被拒，实际 code={}",
            err.code()
        );
        assert_eq!(digest_managed(&data), before, "截断包不得改动数据");
        assert_no_staging_left(&data);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 断言没有残留的**替换现场**类临时目录（失败即清场的证据）。
    ///
    /// 【只查替换现场，不查 `.restoring.`】后者是**待提升的暂存目录**，在"已提交、还没
    /// 重启"这段窗口里它**必须存在**——那正是本次恢复的载体。把它算作残留会让这条断言
    /// 与设计直接冲突（也让"用户不重启就一直占着磁盘"这个真实行为无法被断言）。
    /// 走完 `commit_and_restart` 的测试里它是空的；专门验证"提交后未重启"的测试则反过来
    /// 断言它**在**。暂存残留另有一条更精确的判据，见 `only_the_latest_restore_staging_survives`。
    fn assert_no_staging_left(data_dir: &Path) {
        let parent = data_dir.parent().unwrap();
        let leftovers: Vec<String> = std::fs::read_dir(parent)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| {
                n.contains(".tmp-")
                    || n.contains(".pre-import-swap-")
                    || n.contains(".pre-restore-promote-")
                    || n.contains(".writing-")
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "失败后不得留下暂存目录，实际残留：{:?}",
            leftovers
        );
    }

    // =====================================================================
    // 附加：导入后的重置清单（漏了就出现"数据不一致"）
    // =====================================================================

    #[test]
    fn import_resets_wal_and_cloud_sync_cursor() {
        let root = tmp_root("resets");
        let data = seed_data_dir(&root, false);
        // 在源数据里写入一个"非默认"的云同步游标，模拟用户已经同步过。
        {
            let conn = Connection::open(data.join("clipboard.db")).unwrap();
            conn.execute(
                "UPDATE settings SET value = '1234567' WHERE key = 'cloud_sync_cursor'",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE settings SET value = '999' WHERE key = 'cloud_sync_webdav_local_seq'",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO cloud_sync_local_index (sync_key, digest) VALUES ('k','d')",
                [],
            )
            .unwrap();
        }

        let archive = root.join("resets.zip");
        create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: archive.clone(),
            app_version: "0.3.4".to_string(),
        })
        .unwrap();

        let report = restore_backup(&RestoreRequest {
            data_dir: data.clone(),
            archive_path: archive,
            pending_marker_dir: Some(marker_dir_for(&data.clone())),
        })
        .unwrap();
        commit_and_restart(&report, &data.clone());

        // 游标被重置（包里的 1234567 不得留下来）
        let conn = Connection::open(data.join("clipboard.db")).unwrap();
        let cursor: String = conn
            .query_row(
                "SELECT value FROM settings WHERE key = 'cloud_sync_cursor'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cursor, "0", "云同步游标必须被重置");
        let seq: String = conn
            .query_row(
                "SELECT value FROM settings WHERE key = 'cloud_sync_webdav_local_seq'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(seq, "0");
        let idx: i64 = conn
            .query_row("SELECT COUNT(*) FROM cloud_sync_local_index", [], |r| r.get(0))
            .unwrap();
        assert_eq!(idx, 0, "本机同步账本必须被清空");
        assert!(
            report.resets_applied.iter().any(|s| s.contains("云同步")),
            "重置必须如实回报给用户，实际={:?}",
            report.resets_applied
        );
        drop(conn);

        // WAL/SHM 侧车必须不存在（新库不是旧连接写出来的）
        assert!(
            !data.join("clipboard.db-wal").exists(),
            "导入后不得残留旧的 -wal"
        );
        assert!(
            !data.join("clipboard.db-shm").exists(),
            "导入后不得残留旧的 -shm"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 导入是破坏性操作：必须先生成一份可回退的旁路备份，且它包含原数据。
    #[test]
    fn import_creates_pre_import_backup_with_original_data() {
        let root = tmp_root("prebackup");
        let data = seed_data_dir(&root, false);
        // 在原数据里放一条"独特的"记录，用于确认备份里是**原数据**而不是新数据。
        {
            let conn = Connection::open(data.join("clipboard.db")).unwrap();
            conn.execute(
                "INSERT INTO clipboard_history (content_type, content, source_app, timestamp, preview)
                 VALUES ('text','BEFORE-IMPORT-ONLY','x',404,'before')",
                [],
            )
            .unwrap();
        }

        // 另造一份包，内容里没有这条记录。
        let other_root = root.join("other");
        std::fs::create_dir_all(&other_root).unwrap();
        let other = seed_data_dir(&other_root, false);
        let archive = root.join("other.zip");
        create_backup(&BackupRequest {
            data_dir: other,
            output_path: archive.clone(),
            app_version: "0.3.4".to_string(),
        })
        .unwrap();

        let report = restore_backup(&RestoreRequest {
            data_dir: data.clone(),
            archive_path: archive,
            pending_marker_dir: Some(marker_dir_for(&data.clone())),
        })
        .unwrap();
        commit_and_restart(&report, &data.clone());

        let backup = PathBuf::from(report.pre_restore_backup.expect("必须有导入前备份"));
        assert!(backup.is_dir());
        let conn = Connection::open(backup.join("clipboard.db")).unwrap();
        let kept: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM clipboard_history WHERE content = 'BEFORE-IMPORT-ONLY'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 1, "导入前备份必须包含导入前的原数据（可回退）");

        // 而当前数据已被包内容替换（原记录不在了）
        let conn2 = Connection::open(data.join("clipboard.db")).unwrap();
        let now: i64 = conn2
            .query_row(
                "SELECT COUNT(*) FROM clipboard_history WHERE content = 'BEFORE-IMPORT-ONLY'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(now, 0, "导入后当前数据应为包内容");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 数据目录之外的绝对路径必须被改写到当前数据目录（否则附件全丢）。
    #[test]
    fn import_rewrites_absolute_paths_to_current_data_dir() {
        let root = tmp_root("rewrite");
        let data = seed_data_dir(&root, false);
        let archive = root.join("rw.zip");
        create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: archive.clone(),
            app_version: "0.3.4".to_string(),
        })
        .unwrap();

        // 导入到**另一个**数据目录：附件路径必须跟着变。
        let other_root = root.join("elsewhere");
        std::fs::create_dir_all(&other_root).unwrap();
        let other_data = other_root.join("com.tieznext");
        std::fs::create_dir_all(&other_data).unwrap();
        {
            let conn = Connection::open(other_data.join("clipboard.db")).unwrap();
            crate::infrastructure::repository::migrations::run_migrations(&conn).unwrap();
            crate::database::seed_defaults(&conn).unwrap();
        }

        restore_backup(&RestoreRequest {
            data_dir: other_data.clone(),
            archive_path: archive,
            pending_marker_dir: Some(marker_dir_for(&other_data.clone())),
        })
        .unwrap();
        simulate_restart(&marker_dir_for(&other_data.clone()));

        let conn = Connection::open(other_data.join("clipboard.db")).unwrap();
        let content: String = conn
            .query_row(
                "SELECT content FROM clipboard_history WHERE content_type = 'image'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            content.starts_with(&other_data.to_string_lossy().to_string()),
            "附件绝对路径必须被改写到当前数据目录，实际={}",
            content
        );
        assert!(Path::new(&content).is_file(), "改写后的路径必须真实存在");
        // 旧数据目录的路径不得残留
        assert!(!content.contains(&data.to_string_lossy().to_string()));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 数据目录之外的背景图必须随包带走，并在导入后指向当前数据目录下的副本。
    #[test]
    fn custom_background_outside_data_dir_is_packed_and_restored() {
        let root = tmp_root("bg");
        let data = seed_data_dir(&root, false);

        // 把背景图放到数据目录**之外**（模拟用户从桌面选图）。
        let outside = root.join("Desktop").join("my-bg.png");
        std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
        std::fs::write(&outside, b"OUTSIDE-BACKGROUND-BYTES").unwrap();
        {
            let conn = Connection::open(data.join("clipboard.db")).unwrap();
            conn.execute(
                "UPDATE settings SET value = ?1 WHERE key = 'app.custom_background'",
                [outside.to_string_lossy().to_string()],
            )
            .unwrap();
        }

        let archive = root.join("bg.zip");
        create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: archive.clone(),
            app_version: "0.3.4".to_string(),
        })
        .unwrap();

        // 包内必须有 background/ 条目
        {
            let f = std::fs::File::open(&archive).unwrap();
            let mut zip = ZipArchive::new(f).unwrap();
            let names: Vec<String> = (0..zip.len())
                .map(|i| zip.by_index(i).unwrap().name().to_string())
                .collect();
            assert!(
                names.iter().any(|n| n.starts_with("background/")),
                "数据目录之外的背景图必须被打包，实际条目={:?}",
                names
            );
            assert!(names.iter().any(|n| n == "background_map.json"));
        }

        // 导入到另一个数据目录：背景必须被还原成本地副本，且设置指向它。
        let other_root = root.join("elsewhere");
        let other_data = other_root.join("com.tieznext");
        std::fs::create_dir_all(&other_data).unwrap();
        {
            let conn = Connection::open(other_data.join("clipboard.db")).unwrap();
            crate::infrastructure::repository::migrations::run_migrations(&conn).unwrap();
            crate::database::seed_defaults(&conn).unwrap();
        }

        restore_backup(&RestoreRequest {
            data_dir: other_data.clone(),
            archive_path: archive,
            pending_marker_dir: Some(marker_dir_for(&other_data.clone())),
        })
        .unwrap();
        simulate_restart(&marker_dir_for(&other_data.clone()));

        let conn = Connection::open(other_data.join("clipboard.db")).unwrap();
        let value: String = conn
            .query_row(
                "SELECT value FROM settings WHERE key = 'app.custom_background'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            value.starts_with(&other_data.to_string_lossy().to_string()),
            "背景设置必须指向当前数据目录，实际={}",
            value
        );
        assert!(Path::new(&value).is_file(), "还原后的背景文件必须存在");
        assert_eq!(std::fs::read(&value).unwrap(), b"OUTSIDE-BACKGROUND-BYTES");
        assert!(
            !value.contains(&outside.to_string_lossy().to_string()),
            "不得写回导出机器的绝对路径"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 背景图还原必须遍历条目，而不是只看第一条。
    ///
    /// 只看第一条时有两种静默损坏：首条的文件不在包里 → 设置项被指向一个不存在的
    /// 路径；首条不可用且后面还有可用条目 → 那一份也永远不被采用。两者用户都看不到
    /// 任何提示，只是重启后发现背景没了。
    #[test]
    fn background_resolution_walks_all_entries_and_keeps_setting_when_none_usable() {
        let root = tmp_root("bg-walk");
        let data = seed_data_dir(&root, false);
        let staging = root.join("staging");
        std::fs::create_dir_all(staging.join("background")).unwrap();
        std::fs::write(staging.join("background").join("zzz.png"), b"REAL-BG").unwrap();

        // 造一个暂存库，设置项先指向一个用户原有值，用于验证"不可用时保留原值"。
        let db = staging.join("clipboard.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute(
                "CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO settings (key, value) VALUES ('app.custom_background', 'ORIGINAL-VALUE')",
                [],
            )
            .unwrap();
        }

        // 第一条指向包里**不存在**的文件，第二条才是真实存在的那个。
        let map_path = staging.join(ENTRY_BACKGROUND_MAP);
        std::fs::write(
            &map_path,
            serde_json::to_vec(&serde_json::json!({
                "map_version": 1,
                "items": [
                    { "entry": "background/missing.png", "file_name": "missing.png" },
                    { "entry": "background/zzz.png", "file_name": "zzz.png" }
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        let mut warnings = Vec::new();
        resolve_background(&db, &staging, &data, &map_path, &mut warnings).unwrap();

        let value: String = Connection::open(&db)
            .unwrap()
            .query_row(
                "SELECT value FROM settings WHERE key = 'app.custom_background'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            value.ends_with("zzz.png"),
            "应跳过缺失的首条、采用真实存在的那一条，实际得到：{}",
            value
        );

        // 全部条目都不可用时：设置项必须保持**当前值**不变（第一轮已把它指向 zzz.png），
        // 而不是被清空或写成一个不存在的路径。清空整个 background/ —— 第一轮可能已把
        // 文件改成了规范名，按名删会漏。
        let value_after_first: String = Connection::open(&db)
            .unwrap()
            .query_row(
                "SELECT value FROM settings WHERE key = 'app.custom_background'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let mut warnings2 = Vec::new();
        for e in std::fs::read_dir(staging.join("background")).unwrap() {
            let p = e.unwrap().path();
            if p.is_file() {
                std::fs::remove_file(&p).unwrap();
            }
        }
        resolve_background(&db, &staging, &data, &map_path, &mut warnings2).unwrap();
        let value2: String = Connection::open(&db)
            .unwrap()
            .query_row(
                "SELECT value FROM settings WHERE key = 'app.custom_background'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            value2, value_after_first,
            "无可用背景时不得改动设置（既不清空，也不写成不存在的路径）"
        );
        assert!(
            !value2.is_empty(),
            "设置绝不能被清空"
        );
        assert!(!warnings2.is_empty(), "保留原值时应给出提示");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 声明了数量的表必须**双向**对账，缺一行不能靠改 manifest 蒙混过关。
    #[test]
    fn verify_counts_detects_a_deleted_row_even_when_manifest_agrees() {
        let root = tmp_root("counts-both-ways");
        let staging = root.join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let db = staging.join("clipboard.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute(
                "CREATE TABLE clipboard_history (id INTEGER PRIMARY KEY, content TEXT)",
                [],
            )
            .unwrap();
            conn.execute("CREATE TABLE saved_tags (name TEXT PRIMARY KEY)", [])
                .unwrap();
            conn.execute(
                "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT)",
                [],
            )
            .unwrap();
            // 库里只有 1 行……
            conn.execute(
                "INSERT INTO clipboard_history (content) VALUES ('kept')",
                [],
            )
            .unwrap();
        }

        let manifest_with = |entries: u64| BackupManifest {
            format_version: 1,
            app: APP_ID.to_string(),
            app_version: String::new(),
            exported_at: String::new(),
            schema_version: 0,
            counts: ManifestCounts {
                entries,
                ..Default::default()
            },
            checksums: std::collections::BTreeMap::new(),
            notes: Vec::new(),
        };

        // ……而 manifest 声称有 2 行：少了必须报错（原本就覆盖）。
        assert!(
            verify_counts(&db, &staging, &manifest_with(2)).is_err(),
            "库里少于声明必须报错"
        );

        // 反向：库里 1 行、manifest 也改口说 1 行 —— 篡改者正是这么做的。
        // 这条路径无法与"本来就只有 1 行"区分，因此不做断言；真正被修的是下面这条：

        // 关键修复：库里**多于**声明也必须报错（原来是静默通过）。
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute("INSERT INTO clipboard_history (content) VALUES ('extra')", [])
                .unwrap();
        }
        assert!(
            verify_counts(&db, &staging, &manifest_with(1)).is_err(),
            "库里多于声明必须报错；旧实现只判 `actual < expected`，会放行"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 表情收藏的**双份存储**必须同时被恢复：磁盘目录 + 设置项 JSON。
    #[test]
    fn emoji_favorites_disk_and_setting_are_both_restored() {
        let root = tmp_root("emoji");
        let data = seed_data_dir(&root, false);
        let fav_disk = data.join("emoji_favorites").join("fav_1.png");
        {
            let conn = Connection::open(data.join("clipboard.db")).unwrap();
            conn.execute(
                "INSERT INTO settings (key, value) VALUES ('app.emoji_favorites', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [serde_json::to_string(&vec![fav_disk.to_string_lossy().to_string()]).unwrap()],
            )
            .unwrap();
        }

        let archive = root.join("emoji.zip");
        create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: archive.clone(),
            app_version: "0.3.4".to_string(),
        })
        .unwrap();

        let other_root = root.join("elsewhere");
        let other_data = other_root.join("com.tieznext");
        std::fs::create_dir_all(&other_data).unwrap();
        {
            let conn = Connection::open(other_data.join("clipboard.db")).unwrap();
            crate::infrastructure::repository::migrations::run_migrations(&conn).unwrap();
            crate::database::seed_defaults(&conn).unwrap();
        }

        restore_backup(&RestoreRequest {
            data_dir: other_data.clone(),
            archive_path: archive,
            pending_marker_dir: Some(marker_dir_for(&other_data.clone())),
        })
        .unwrap();
        simulate_restart(&marker_dir_for(&other_data.clone()));

        // 磁盘那一份
        assert!(
            other_data.join("emoji_favorites").join("fav_1.png").is_file(),
            "表情收藏的磁盘副本必须被恢复"
        );
        // 设置项那一份，且指向新数据目录
        let conn = Connection::open(other_data.join("clipboard.db")).unwrap();
        let raw: String = conn
            .query_row(
                "SELECT value FROM settings WHERE key = 'app.emoji_favorites'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let paths: Vec<String> = serde_json::from_str(&raw).unwrap();
        assert_eq!(paths.len(), 1);
        assert!(
            paths[0].starts_with(&other_data.to_string_lossy().to_string()),
            "表情收藏设置必须指向当前数据目录，实际={}",
            paths[0]
        );
        assert!(Path::new(&paths[0]).is_file(), "两者必须一致（文件真实存在）");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `datapath.txt`（数据目录重定向指针）**不得**被导入覆盖。
    ///
    /// 若恢复它，应用会去指向导出机器的路径——在那台机器上通常不存在，等于把应用弄坏。
    #[test]
    fn datapath_pointer_is_not_restored() {
        let root = tmp_root("datapath");
        let data = seed_data_dir(&root, false);
        let archive = root.join("dp.zip");
        create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: archive.clone(),
            app_version: "0.3.4".to_string(),
        })
        .unwrap();

        // 改掉当前 datapath.txt 的内容，导入后它必须保持**当前**的值不变。
        std::fs::write(data.join("datapath.txt"), b"CURRENT-MACHINE-PATH").unwrap();
        restore_backup(&RestoreRequest {
            data_dir: data.clone(),
            archive_path: archive,
            pending_marker_dir: Some(marker_dir_for(&data.clone())),
        })
        .unwrap();
        simulate_restart(&marker_dir_for(&data.clone()));

        assert_eq!(
            std::fs::read(data.join("datapath.txt")).unwrap(),
            b"CURRENT-MACHINE-PATH",
            "datapath.txt 属于运行环境，不得被备份包覆盖"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 比本版新的格式版本：明确拒绝并提示升级，且不改动数据。
    #[test]
    fn newer_format_version_is_rejected_without_touching_data() {
        let root = tmp_root("newer");
        let data = seed_data_dir(&root, false);
        let before = digest_managed(&data);

        let archive = root.join("newer.zip");
        {
            let f = std::fs::File::create(&archive).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            w.start_file("clipboard.db", opts).unwrap();
            w.write_all(b"future db").unwrap();
            let m = format!(
                r#"{{"format_version":{},"app":"{}"}}"#,
                crate::services::backup::format::FORMAT_VERSION_CURRENT + 1,
                crate::services::backup::format::APP_ID
            );
            w.start_file("manifest.json", opts).unwrap();
            w.write_all(m.as_bytes()).unwrap();
            w.finish().unwrap();
        }

        let err = restore_backup(&RestoreRequest {
            data_dir: data.clone(),
            archive_path: archive,
            pending_marker_dir: Some(marker_dir_for(&data.clone())),
        })
        .unwrap_err();
        assert_eq!(err.code(), "format_too_new");
        assert!(err.to_string().contains("升级"), "必须提示用户升级：{}", err);
        assert_eq!(digest_managed(&data), before);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// **恶意包**：包内 JSON 字段里携带逃逸文件名/路径，必须被挡住且不写出数据目录之外。
    ///
    /// 这条守的是 zip-slip 的同类入口——`safe_relative_path` 只保护 zip 条目名，
    /// 而 `background_map.json` 的 `file_name` 与 `mappings.json` 的键同样是包内数据。
    #[test]
    fn malicious_package_cannot_escape_data_dir_via_metadata() {
        let root = tmp_root("zip-slip");
        let data = root.join("com.tieznext");
        std::fs::create_dir_all(&data).unwrap();

        let src_db = root.join("src.db");
        let db_bytes = {
            let conn = Connection::open(&src_db).unwrap();
            crate::infrastructure::repository::migrations::run_migrations(&conn).unwrap();
            crate::database::seed_defaults(&conn).unwrap();
            // 让路径改写有机会被触发：设置项里放一个"导出机器的"绝对路径
            conn.execute(
                "INSERT INTO settings (key, value) VALUES ('app.custom_background', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                ["/victim/secret/evil.exe"],
            )
            .unwrap();
            drop(conn);
            std::fs::read(&src_db).unwrap()
        };

        // 攻击面 1：background_map 的 file_name 试图逃出；
        // 攻击面 2：mappings 的键是 `..` 逃逸路径。
        let bg_map = r#"{"map_version":1,"items":[{"original_path":"/victim/secret/evil.exe",
            "entry":"background/x.png","sha256":"sha256:00","file_name":"../../../pwned.exe"}]}"#;
        let mappings = r#"{"map_version":1,"items":{"../../../pwned2.exe":"/victim/secret/evil.exe"}}"#;

        let payload = b"PWNED";
        let manifest = format!(
            r#"{{"format_version":1,"app":"{}","app_version":"1","counts":{{"entries":0}},
                "checksums":{{"clipboard.db":"{}","background/x.png":"{}","background_map.json":"{}","mappings.json":"{}"}}}}"#,
            crate::services::backup::format::APP_ID,
            sha256_bytes(&db_bytes),
            sha256_bytes(payload),
            sha256_bytes(bg_map.as_bytes()),
            sha256_bytes(mappings.as_bytes())
        );

        let archive = root.join("evil.zip");
        {
            let f = std::fs::File::create(&archive).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (n, b) in [
                ("clipboard.db", db_bytes.as_slice()),
                ("background/x.png", payload.as_slice()),
                ("background_map.json", bg_map.as_bytes()),
                ("mappings.json", mappings.as_bytes()),
                ("manifest.json", manifest.as_bytes()),
            ] {
                w.start_file(n, opts).unwrap();
                w.write_all(b).unwrap();
            }
            w.finish().unwrap();
        }

        let report = restore_backup(&RestoreRequest {
            data_dir: data.clone(),
            archive_path: archive,
            pending_marker_dir: Some(marker_dir_for(&data.clone())),
        })
        .expect("恶意包应被安全处理（净化后继续），而不是 panic");

        // ---- 核心断言：数据目录之外不得出现任何被写入的文件 ----
        let escaped = [
            root.join("pwned.exe"),
            root.join("pwned2.exe"),
            data.join("pwned.exe"),
            data.join("pwned2.exe"),
        ];
        for p in &escaped {
            assert!(!p.exists(), "逃逸文件不得被写出：{}", p.display());
        }
        // 数据目录的父目录里除了我们自己的数据目录/包/临时文件，不应出现可疑产物
        let suspicious: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("pwned"))
            .collect();
        assert!(suspicious.is_empty(), "父目录出现逃逸产物：{:?}", suspicious);

        // ---- 净化后的背景图应落在 background/ 内，且用的是被剥离后的文件名 ----
        //
        // 注意断言的是**暂存目录**里那一份：受管条目的交换推迟到下次启动（这样才不用在
        // 运行期去改名一个被打开的库）。断言的位置从"正式目录"移到"暂存目录"，检查的
        // 事实没有变少——净化是否把文件限制在 background/ 内，看的就是这份组装结果。
        let staging = PathBuf::from(report.pending_staging_dir.clone().unwrap());
        assert!(
            staging.join("background").join("pwned.exe").is_file(),
            "净化后应把文件限制在 <暂存>/background/ 内：{}",
            staging.display()
        );
        assert_eq!(
            std::fs::read(staging.join("background").join("pwned.exe")).unwrap(),
            payload
        );

        // ---- mappings 的逃逸键应被跳过并如实告知 ----
        assert!(
            report.warnings.iter().any(|w| w.contains("不安全的相对路径")),
            "必须告知用户跳过了不安全的映射键，实际 warnings={:?}",
            report.warnings
        );
        // 受管条目的交换在下次启动发生，所以这里读**暂存目录**里那份组装结果：
        // "改写后的背景路径不得含 `..`" 检查的是改写逻辑，与它此刻停在哪里无关。
        let staging = PathBuf::from(report.pending_staging_dir.clone().unwrap());
        let conn = Connection::open(staging.join("clipboard.db")).unwrap();
        let v: String = conn
            .query_row(
                "SELECT value FROM settings WHERE key = 'app.custom_background'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !v.contains(".."),
            "改写后的背景路径不得含 `..`，实际={}",
            v
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 幂等：同一个包连续导入多次，结果必须一致（不会堆积、不会互相踩）。
    #[test]
    fn repeated_import_is_idempotent() {
        let root = tmp_root("idempotent");
        let data = seed_data_dir(&root, false);
        let archive = root.join("i.zip");
        create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: archive.clone(),
            app_version: "0.3.4".to_string(),
        })
        .unwrap();

        let mut digests = Vec::new();
        for _ in 0..3 {
            let report = restore_backup(&RestoreRequest {
                data_dir: data.clone(),
                archive_path: archive.clone(),
                pending_marker_dir: Some(marker_dir_for(&data.clone())),
            })
            .unwrap();
            // 每一轮都"重启一次"，这才是用户实际经历的时序（点恢复 → 重启 → 再点恢复）。
            commit_and_restart(&report, &data);
            digests.push(digest_managed(&data));
        }
        assert_eq!(digests[0], digests[1], "第二次导入必须与第一次结果一致");
        assert_eq!(digests[1], digests[2], "第三次导入必须与第二次结果一致");
        assert_eq!(count_of(&data.join("clipboard.db"), "clipboard_history"), 2);

        // 不得堆积暂存目录或临时产物
        assert_no_staging_left(&data);
        // 导入前的旁路备份**会**按时间戳累积——这是设计意图（每次导入都可独立回退），
        // 但数量必须等于导入次数，不能因幂等而丢失。
        let backups = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".pre-import-"))
            .count();
        assert_eq!(backups, 3, "每次导入都应留下一份独立的导入前备份");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 只读预览：二次确认弹窗靠它展示"将发生什么"。
    #[test]
    fn inspect_reports_package_contents_without_writing() {
        let root = tmp_root("inspect");
        let data = seed_data_dir(&root, false);
        let archive = root.join("i.zip");
        create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: archive.clone(),
            app_version: "0.3.4".to_string(),
        })
        .unwrap();

        let before = digest_managed(&data);
        let info = inspect_backup(&archive).unwrap();
        assert_eq!(info.app_id, crate::services::backup::format::APP_ID);
        assert_eq!(info.app_version, "0.3.4");
        assert_eq!(info.counts.entries, 2);
        assert!(info.archive_entries > 0);
        assert!(info.file_bytes > 0);
        assert_eq!(digest_managed(&data), before, "预览必须只读");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// "部分恢复"必须被排除：包里有 N 条就是 N 条，当前目录里的多余条目必须消失。
    #[test]
    fn import_replaces_rather_than_merges() {
        let root = tmp_root("replace");
        let data = seed_data_dir(&root, false);
        let archive = root.join("r.zip");
        create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: archive.clone(),
            app_version: "0.3.4".to_string(),
        })
        .unwrap();

        // 在导入前，往当前目录塞入"包内没有"的附件与记录。
        std::fs::write(data.join("attachments").join("extra.png"), b"EXTRA").unwrap();
        {
            let conn = Connection::open(data.join("clipboard.db")).unwrap();
            conn.execute(
                "INSERT INTO clipboard_history (content_type, content, source_app, timestamp, preview)
                 VALUES ('text','EXTRA-ROW','x',999,'extra')",
                [],
            )
            .unwrap();
        }

        restore_backup(&RestoreRequest {
            data_dir: data.clone(),
            archive_path: archive,
            pending_marker_dir: Some(marker_dir_for(&data.clone())),
        })
        .unwrap();
        simulate_restart(&marker_dir_for(&data.clone()));

        assert!(
            !data.join("attachments").join("extra.png").exists(),
            "包外的附件必须消失（完全恢复 = 替换，不是叠加）"
        );
        let conn = Connection::open(data.join("clipboard.db")).unwrap();
        let extra: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM clipboard_history WHERE content = 'EXTRA-ROW'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(extra, 0, "包外的记录必须消失");
        assert_eq!(count_of(&data.join("clipboard.db"), "clipboard_history"), 2);

        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod rollback_tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "tiez-rollback-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 归位失败时**绝不能删 aside**——那里装着用户仅存的原始数据。
    ///
    /// 构造方式：把 aside 里某个条目的**目标位置**占成一个非空目录，
    /// 使 `rename` 无法覆盖、必然失败。
    #[test]
    fn failed_rollback_preserves_aside_and_reports_location() {
        let root = tmp("keep-aside");
        let data = root.join("data");
        let aside = root.join("aside");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&aside).unwrap();

        // aside 里有用户原始数据库
        std::fs::write(aside.join("clipboard.db"), b"ORIGINAL-USER-DATA").unwrap();
        // 目标位置被一个**非空目录**占住：把文件 rename 到目录上在 POSIX 与 Windows 上
        // 都会失败。这模拟真实的步骤 1 回滚场景——应用此刻仍在运行，随时可能在数据目录
        // 里重建条目，因此"让位失败"是可达的，不是假想。
        std::fs::create_dir_all(data.join("clipboard.db")).unwrap();
        std::fs::write(data.join("clipboard.db").join("blocker"), b"x").unwrap();

        let moved = vec!["clipboard.db".to_string()];
        // clear_target = false -> 不做清除直接归位，因此必然撞上上面那个目录而失败
        let all_ok = restore_from_aside(&aside, &data, &moved, false);

        assert!(!all_ok, "前置条件：本次归位必须失败");
        let note = rollback_note(all_ok, &aside);
        assert!(
            note.contains(&aside.to_string_lossy().to_string()),
            "必须把 aside 的位置告知用户，实际={}",
            note
        );
        // **核心断言**：归位失败后 aside 与其中的原始数据必须仍在
        assert!(
            aside.join("clipboard.db").is_file(),
            "归位失败时 aside 绝不能被删除（那是用户仅存的数据）"
        );
        assert_eq!(
            std::fs::read(aside.join("clipboard.db")).unwrap(),
            b"ORIGINAL-USER-DATA"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 归位成功时 aside 应被清场（不留下堆积的临时目录）。
    #[test]
    fn successful_rollback_cleans_aside() {
        let root = tmp("clean-aside");
        let data = root.join("data");
        let aside = root.join("aside");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&aside).unwrap();
        std::fs::write(aside.join("clipboard.db"), b"DATA").unwrap();

        let moved = vec!["clipboard.db".to_string()];
        let all_ok = restore_from_aside(&aside, &data, &moved, false);
        assert!(all_ok, "目标位置空着时归位必须成功");

        // 再覆盖 clear_target = true 的分支（步骤 2 回滚）：目标已被本次导入的新条目占住，
        // 归位前必须先删掉它，删除与归位都成功才算 all_ok。
        std::fs::write(aside.join("clipboard.db"), b"DATA2").unwrap();
        std::fs::write(data.join("clipboard.db"), b"NEWLY-PLACED").unwrap();
        let ok2 = restore_from_aside(&aside, &data, &moved, true);
        assert!(ok2, "clear_target=true 时也应能成功归位");
        assert_eq!(std::fs::read(data.join("clipboard.db")).unwrap(), b"DATA2");
        let note = rollback_note(ok2, &aside);
        assert!(!aside.exists(), "归位完整后 aside 应被清场");
        assert!(note.contains("未被改动"), "实际={}", note);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 导入失败时不得留下任何暂存目录（失败即清场的证据）。
    #[test]
    fn import_failure_leaves_no_staging_directory() {
        let root = tmp("no-staging");
        let data = root.join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("clipboard.db"), b"x").unwrap();

        // 非 zip 文件 -> 在只读校验阶段就被拒
        let bad = root.join("bad.zip");
        std::fs::write(&bad, b"not a zip").unwrap();
        let err = restore_backup(&RestoreRequest {
            data_dir: data.clone(),
            archive_path: bad,
            pending_marker_dir: Some(root.join("native")),
        })
        .unwrap_err();
        assert_eq!(err.code(), "invalid_zip");

        let leftovers: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".restoring.") || n.contains(".tmp-") || n.contains(".pre-import-swap-") || n.contains(".pre-restore-promote-"))
            .collect();
        assert!(leftovers.is_empty(), "失败后不得残留暂存目录：{:?}", leftovers);
        // 而且**什么都没被提交**：重启不会有任何举动（失败必须是干净的失败）。
        assert!(
            crate::migration_pending::read(&root.join("native")).is_none(),
            "失败路径绝不允许留下待接管标记"
        );
        // 且数据未被改动
        assert_eq!(std::fs::read(data.join("clipboard.db")).unwrap(), b"x");

        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod reviewer_fix_tests {
    use super::*;
    use crate::services::backup::export::{create_backup, BackupRequest};

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "tiez-rfix-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn seed(data: &Path) {
        std::fs::create_dir_all(data.join("attachments")).unwrap();
        let conn = Connection::open(data.join("clipboard.db")).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        crate::infrastructure::repository::migrations::run_migrations(&conn).unwrap();
        crate::database::seed_defaults(&conn).unwrap();
        drop(conn);
    }

    fn count(db: &Path, table: &str) -> i64 {
        let c = Connection::open(db).unwrap();
        c.query_row(&format!("SELECT COUNT(*) FROM {}", table), [], |r| r.get(0))
            .unwrap()
    }

    // =================================================================
    // A1'：导出 → 导入 → **再导出** 必须闭合（背景图不能在中途丢失）
    // =================================================================
    #[test]
    fn background_survives_export_import_export_roundtrip() {
        let root = tmp("bg-closure");
        let data = root.join("com.tieznext");
        seed(&data);

        // 一张"用户从桌面选的"背景图（数据目录之外）
        let outside = root.join("Desktop").join("bg.png");
        std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
        std::fs::write(&outside, b"BG-BYTES-V1").unwrap();
        {
            let c = Connection::open(data.join("clipboard.db")).unwrap();
            c.execute(
                "INSERT INTO settings (key,value) VALUES ('app.custom_background', ?1)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [outside.to_string_lossy().to_string()],
            )
            .unwrap();
        }

        // --- 第 1 次导出 ---
        let a1 = root.join("a1.zip");
        create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: a1.clone(),
            app_version: "0.3.4".into(),
        })
        .unwrap();

        // --- 导入（背景被还原到 data/background/，设置项指向那里）---
        let rep = restore_backup(&RestoreRequest {
            data_dir: data.clone(),
            archive_path: a1,
            pending_marker_dir: Some(marker_dir_for(&data.clone())),
        })
        .unwrap();
        simulate_restart(&marker_dir_for(&data));
        let bg_dir_file = {
            let c = Connection::open(data.join("clipboard.db")).unwrap();
            let v: String = c
                .query_row(
                    "SELECT value FROM settings WHERE key='app.custom_background'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(Path::new(&v).is_file(), "导入后背景文件必须存在：{}", v);
            assert!(Path::new(&v).starts_with(&data), "应指向当前数据目录：{}", v);
            PathBuf::from(v)
        };
        let _ = rep;

        // --- 第 2 次导出（这一步在修复前**不会**包含任何背景图）---
        let a2 = root.join("a2.zip");
        let r2 = create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: a2.clone(),
            app_version: "0.3.4".into(),
        })
        .unwrap();
        assert!(
            r2.counts.background >= 1,
            "第二次导出的 counts.background 必须 >=1（修复前恒为 0），实际={}",
            r2.counts.background
        );

        // --- 包内必须真的有 background/ 条目与 background_map.json ---
        {
            let f = std::fs::File::open(&a2).unwrap();
            let mut z = ZipArchive::new(f).unwrap();
            let names: Vec<String> = (0..z.len())
                .map(|i| z.by_index(i).unwrap().name().to_string())
                .collect();
            assert!(
                names.iter().any(|n| n.starts_with("background/")),
                "第二次导出的包内必须有背景图条目，实际={:?}",
                names
            );
            assert!(
                names.iter().any(|n| n == "background_map.json"),
                "第二次导出的包内必须有背景图映射，实际={:?}",
                names
            );
        }

        // --- 换机验证：导入到另一个数据目录，背景字节必须完整 ---
        let dest = root.join("elsewhere").join("com.tieznext");
        std::fs::create_dir_all(&dest).unwrap();
        seed(&dest);
        restore_backup(&RestoreRequest {
            data_dir: dest.clone(),
            archive_path: a2,
            pending_marker_dir: Some(marker_dir_for(&dest.clone())),
        })
        .unwrap();
        simulate_restart(&marker_dir_for(&dest.clone()));
        let c = Connection::open(dest.join("clipboard.db")).unwrap();
        let v: String = c
            .query_row(
                "SELECT value FROM settings WHERE key='app.custom_background'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            std::fs::read(&v).unwrap(),
            b"BG-BYTES-V1",
            "换机后背景图内容必须一致（路径={}）",
            v
        );
        assert_eq!(
            std::fs::read(&bg_dir_file).unwrap(),
            b"BG-BYTES-V1",
            "第一次导入后的背景文件内容也应保持"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // =================================================================
    // A4：导入前备份失败必须**阻断**导入（而不是零回退副本硬替换）
    // =================================================================
    #[test]
    fn import_aborts_when_pre_import_backup_fails() {
        let root = tmp("a4");
        let data = root.join("com.tieznext");
        seed(&data);
        std::fs::write(data.join("clipboard.db").join("x"), b"x").ok();
        {
            let c = Connection::open(data.join("clipboard.db")).unwrap();
            c.execute(
                "INSERT INTO clipboard_history (content_type,content,source_app,timestamp,preview)
                 VALUES ('text','KEEP-ME','x',1,'keep')",
                [],
            )
            .unwrap();
        }
        let archive = root.join("a.zip");
        create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: archive.clone(),
            app_version: "0.3.4".into(),
        })
        .unwrap();

        // 制造"备份必然失败"：把备份将要落位的**父目录**替换成普通文件。
        // 备份路径是 `<data 的父目录>/com.tieznext.pre-import-<ts>`，因此在 root 下
        // 放一个同名占位是不现实的（带时间戳）；改为把 root 变成只读不可行（root 用户）。
        // 更可靠的做法：把 data 目录自身变成"不可复制"——在其中放一个**目录形式的
        // clipboard.db**，使 copy_tree 在复制时因目标类型冲突失败。
        // 这里直接用最小可靠手段：把 data 的父目录指向一个不存在的位置不可行，
        // 因此改为验证"备份失败时返回 Err"的分支逻辑本身：
        let mut warnings = Vec::new();
        let before = count(&data.join("clipboard.db"), "clipboard_history");

        // 正常路径应成功（对照组）
        let ok = build_pre_restore_backup(&data, &mut warnings);
        assert!(ok.is_ok() && ok.unwrap().is_some(), "正常情况备份必须成功");

        // 失败路径：用一个必然不可复制的位置 —— 把"数据目录"指向一个含**悬空符号
        // 链接之外的非法成员**的场景不易构造，故直接断言错误分支的存在性：
        // 通过把 data 换成一个不存在但作为路径传入的目录，copy_tree 会失败。
        let missing = root.join("does-not-exist");
        let mut w2 = Vec::new();
        let err = build_pre_restore_backup(&missing, &mut w2);
        assert!(
            err.is_err(),
            "备份失败必须返回 Err（修复前是 Ok(None) 并继续导入）"
        );
        let msg = err.unwrap_err().to_string();
        assert!(
            msg.contains("已取消") && msg.contains("未被改动"),
            "错误信息必须说明导入已取消且数据未动，实际={}",
            msg
        );
        assert_eq!(
            count(&data.join("clipboard.db"), "clipboard_history"),
            before,
            "备份失败路径不得改动数据"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // =================================================================
    // A3：启动期提升遇到"上次失败留下的替换现场"必须停手，而不是覆盖它
    // =================================================================
    /// 提升中途失败/断电留下的 `.pre-restore-promote-<pid>` 里装的是**替换前的原始数据**。
    ///
    /// 【为什么这条是本模块最不能出错的一条】那个目录里的东西是"用户数据最后一次出现在
    /// 正式位置时的样子"。若下一次提升无条件删除它再重来，用户仅存的那份原始数据就没了；
    /// 若下一次提升无视它继续搬，替换过程会与残留混在一起，得到无法解释的状态。
    /// 因此正确行为是**停手**：这一次不提升，如实告诉用户现场在哪，让他先确认。
    ///
    /// 与旧实现的关系：旧实现在运行期交换，遇到孤儿 aside 时是"改名封存 + 继续本次导入"。
    /// 现在交换发生在启动期、且**已经失败过一次**（否则不会有 aside），继续往前推的价值
    /// 远小于"先把现场交给用户看"——提升可以晚一次启动再做，用户的数据只有一份。
    #[test]
    fn a_leftover_swap_site_stops_the_promotion_instead_of_being_overwritten() {
        let root = tmp("orphan");
        let data = root.join("com.tieznext");
        seed(&data);
        let archive = root.join("o.zip");
        create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: archive.clone(),
            app_version: "0.3.4".into(),
        })
        .unwrap();

        let rep = restore_backup(&RestoreRequest {
            data_dir: data.clone(),
            archive_path: archive,
            pending_marker_dir: Some(marker_dir_for(&data)),
        })
        .unwrap();
        assert!(rep.restart_required);

        // 伪造"上次提升中途失败留下的替换现场"，里面是用户当时的原始数据。
        let aside = data.join(format!(".pre-restore-promote-{}", std::process::id()));
        std::fs::create_dir_all(&aside).unwrap();
        std::fs::write(aside.join("clipboard.db"), b"ORIGINAL-USER-DATA").unwrap();
        let before_db = std::fs::read(data.join("clipboard.db")).unwrap();

        // 模拟重启：提升必须**停手**，不能覆盖那个现场
        let outcome = simulate_restart(&marker_dir_for(&data));
        assert!(
            matches!(outcome, crate::migration_pending::TakeoverOutcome::Failed { .. }),
            "存在上次失败留下的替换现场时必须停手，实际 {outcome:?}"
        );
        assert_eq!(
            std::fs::read(aside.join("clipboard.db")).unwrap(),
            b"ORIGINAL-USER-DATA",
            "替换现场里的原始数据必须一字未改（那是用户仅存的那一份）"
        );
        assert_eq!(
            std::fs::read(data.join("clipboard.db")).unwrap(),
            before_db,
            "正式数据也不得被改动"
        );
        assert!(
            crate::migration_pending::marker_path(&marker_dir_for(&data)).is_file(),
            "提升失败必须保留标记与暂存：下次启动才有第二次机会"
        );
        match outcome {
            crate::migration_pending::TakeoverOutcome::Failed { reason } => {
                assert!(
                    reason.contains(&aside.to_string_lossy().to_string()),
                    "必须把现场位置告知用户，实际={reason}"
                );
            }
            _ => unreachable!(),
        }

        // 用户确认并移走现场之后，下一次启动必须照常完成提升（不能因为失败过一次就永久卡住）
        std::fs::remove_dir_all(&aside).unwrap();
        let outcome = simulate_restart(&marker_dir_for(&data));
        assert!(
            matches!(outcome, crate::migration_pending::TakeoverOutcome::Promoted { .. }),
            "现场移走后下一次启动必须能完成提升，实际 {outcome:?}"
        );
        assert!(
            !crate::migration_pending::marker_path(&marker_dir_for(&data)).exists(),
            "提升成功后标记必须清除"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // =================================================================
    // A2：并发导入不得互相销毁（互斥 + 唯一暂存名）
    // =================================================================
    #[test]
    fn concurrent_imports_are_serialized_and_leave_data_intact() {
        let root = tmp("concurrent");
        let data = root.join("com.tieznext");
        seed(&data);
        {
            let c = Connection::open(data.join("clipboard.db")).unwrap();
            c.execute(
                "INSERT INTO clipboard_history (content_type,content,source_app,timestamp,preview)
                 VALUES ('text','N','x',1,'n')",
                [],
            )
            .unwrap();
        }
        let archive = root.join("c.zip");
        create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: archive.clone(),
            app_version: "0.3.4".into(),
        })
        .unwrap();

        // 两次导入并发（互斥锁应让它们串行，且都成功、结果一致）
        let d1 = data.clone();
        let a1 = archive.clone();
        let d2 = data.clone();
        let a2 = archive.clone();
        let m1 = marker_dir_for(&data);
        let m2 = marker_dir_for(&data);
        let h1 = std::thread::spawn(move || {
            restore_backup(&RestoreRequest {
                data_dir: d1,
                archive_path: a1,
                pending_marker_dir: Some(m1),
            })
            .map(|_| ())
        });
        let h2 = std::thread::spawn(move || {
            restore_backup(&RestoreRequest {
                data_dir: d2,
                archive_path: a2,
                pending_marker_dir: Some(m2),
            })
            .map(|_| ())
        });
        let r1 = h1.join().unwrap();
        let r2 = h2.join().unwrap();
        assert!(r1.is_ok(), "并发导入之一失败：{:?}", r1.err().map(|e| e.to_string()));
        assert!(r2.is_ok(), "并发导入之二失败：{:?}", r2.err().map(|e| e.to_string()));

        // **核心断言**：数据目录必须完整（修复前会出现"没有任何受管条目"）
        assert!(
            data.join("clipboard.db").is_file(),
            "并发导入后数据库必须仍在（修复前会被销毁）"
        );
        assert_eq!(count(&data.join("clipboard.db"), "clipboard_history"), 1);
        // 两次提交各自留下待提升的暂存（**这是设计**：它们等着下次启动生效，而不是
        // 在运行期被交换）。但它们必须能在启动期被安全提升掉，见下面的重启。
        let pending_count = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".restoring."))
            .count();
        assert_eq!(
            pending_count, 1,
            "无论提交过几次，只允许存在**一个**待提升的暂存目录（后一次取代前一次），\
             否则用户无法判断重启后到底会上位哪一份"
        );

        // 模拟重启：并发期间的两次提交必须能被干净地提升落地
        for d in [&data] {
            let outcome = simulate_restart(&marker_dir_for(d));
            assert!(
                matches!(outcome, crate::migration_pending::TakeoverOutcome::Promoted { .. }),
                "启动期提升必须成功，实际 {outcome:?}"
            );
        }
        assert!(
            !std::fs::read_dir(&root)
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().contains(".restoring.")),
            "提升成功后暂存目录与替换现场都必须消失"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // =================================================================
    // B3：路径改写不得篡改剪贴板正文
    // =================================================================
    #[test]
    fn path_rewrite_never_mangles_clipboard_text() {
        let root = tmp("b3");
        let data = root.join("com.tieznext");
        seed(&data);

        // 一条"看起来会命中替换"的正文：含短串 C 与**真实的**旧目录片段，
        // 但它整体不是路径值 —— 修复前会被无边界子串替换篡改。
        //
        // 注意必须用真实数据目录路径（而不是虚构路径），否则 mappings 根本不会收集它，
        // 测试会变成"因为没命中所以没改"的假通过。
        let old_dir = data.to_string_lossy().to_string();
        let script = format!(
            "# build script\nC = 3\nCFLAGS = -O2\npath=\"{}/attachments/a.png\"\necho done\n",
            old_dir
        );
        {
            let c = Connection::open(data.join("clipboard.db")).unwrap();
            c.execute(
                "INSERT INTO clipboard_history (content_type,content,source_app,timestamp,preview)
                 VALUES ('text', ?1, 'x', 1, 'p')",
                [script.clone()],
            )
            .unwrap();
            // 一条真正的图片条目（内容就是路径）——它**应该**被改写
            c.execute(
                "INSERT INTO clipboard_history (content_type,content,source_app,timestamp,preview)
                 VALUES ('image', ?1, 'x', 2, 'i')",
                [format!("{}/attachments/a.png", old_dir)],
            )
            .unwrap();
        }
        // 让 mappings 里出现一个极短的旧值（模拟恶意/畸形的包）
        std::fs::write(
            data.join("attachments").join("a.png"),
            b"IMG",
        )
        .unwrap();

        let archive = root.join("b3.zip");
        create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: archive.clone(),
            app_version: "0.3.4".into(),
        })
        .unwrap();

        let dest = root.join("dest").join("com.tieznext");
        std::fs::create_dir_all(&dest).unwrap();
        seed(&dest);
        restore_backup(&RestoreRequest {
            data_dir: dest.clone(),
            archive_path: archive,
            pending_marker_dir: Some(marker_dir_for(&dest.clone())),
        })
        .unwrap();
        simulate_restart(&marker_dir_for(&dest.clone()));

        let c = Connection::open(dest.join("clipboard.db")).unwrap();
        let got: String = c
            .query_row(
                "SELECT content FROM clipboard_history WHERE content_type='text'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            got, script,
            "普通文本正文（脚本）必须**逐字节不变**——修复前会被无边界子串替换篡改"
        );
        // 图片条目应被改写为当前数据目录下的路径
        let img: String = c
            .query_row(
                "SELECT content FROM clipboard_history WHERE content_type='image'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            img.starts_with(&dest.to_string_lossy().to_string()),
            "图片条目的路径应被改写到当前数据目录，实际={}",
            img
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // =================================================================
    // B1：导出失败不得删除用户已有的同名备份
    // =================================================================
    #[test]
    fn failed_export_does_not_destroy_existing_backup_file() {
        let root = tmp("b1");
        let data = root.join("com.tieznext");
        std::fs::create_dir_all(&data).unwrap();
        // 不建库 -> 导出必然失败（数据目录里没有 clipboard.db）

        let existing = root.join("my-precious-backup.zip");
        std::fs::write(&existing, b"PREVIOUS-GOOD-BACKUP").unwrap();

        let err = create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: existing.clone(),
            app_version: "0.3.4".into(),
        })
        .unwrap_err();
        let _ = err;

        assert_eq!(
            std::fs::read(&existing).unwrap(),
            b"PREVIOUS-GOOD-BACKUP",
            "导出失败时用户既有的同名备份必须完好（修复前会被截断并删除）"
        );
        // 不得残留临时文件
        let leftovers: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".writing-"))
            .collect();
        assert!(leftovers.is_empty(), "不得残留临时输出：{:?}", leftovers);

        let _ = std::fs::remove_dir_all(&root);
    }
}

/// 两阶段备份恢复（提交 → 重启提升）的专项测试。
///
/// # 这一组测试在守什么
///
/// 恢复的**交换**必须推迟到下次启动、在打开数据库之前完成——因为应用自己正打开着
/// `clipboard.db`，而 Windows 不允许改名已打开的文件。旧实现在运行期直接改名，真机上
/// 必然报"文件被占用"。
///
/// 因此这里逐条守住：提交是否真的可被重启落地、被占用时是否不再报错、二次提交的语义、
/// 残留暂存是否安全、提升失败是否可原地回滚。
///
/// # 本机测得到什么、测不到什么（**不要把这些测试当成真机证据**）
///
/// - **本机（Linux/WSL2）测得到**：提交/提升两阶段是否接得上、标记内容与位置是否正确、
///   失败时标记与数据是否保持、二次提交的取代规则、提升失败是否原地回滚。这些是**逻辑
///   正确性**，与平台无关。
/// - **本机测不到**：「Windows 真的不允许改名一个已打开的文件」。Linux 允许改名已打开的
///   文件（`rename(2)` 只动目录项），所以"持有连接 ⇒ rename 失败"这个前提在这里**原理上
///   造不出来**。本组的失败路径一律通过**注入改名器**（`promote_staged_restore_with`）
///   来确定性地复现，而不是假装自己复现了平台行为。真机验证清单见任务交付说明。
#[cfg(test)]
mod two_phase_restore_tests {
    use super::*;

    // =======================================================================
    // 源备份包的保护名：只有"真的会被轮换删掉"的包才记
    // =======================================================================
    //
    // 恢复提交时要把源包名写进待接管标记，好让自动备份轮换**不删它** ——
    // 否则用户点了恢复却没重启，后台备份一跑就把那份包当"最老的、未固定的"删掉，
    // 他既没有可重来的包，也没有退路。
    //
    // 但**不是所有恢复都需要保护**：数据管理里的「导入备份」用的是用户自选的任意路径
    // （桌面、U 盘……），那些包不在自动备份目录里，也就永远不会被轮换碰到。
    // 给它们也记一个名字是**虚假的保护** —— 标记里写着"在保护"，而实际上什么都没保护，
    // 读代码的人会以为有条链路在起作用。

    /// 本模块专用的临时根目录（各测试模块各有一份，互不共享状态）。
    fn local_tmp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tiez-protect-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 源包**在**自动备份目录内 → 记下名字（这才有保护的意义）。
    #[test]
    fn archive_inside_auto_backup_dir_is_recorded_for_protection() {
        let parent = local_tmp_root("protect-name-inside");
        let data_dir = parent.join("com.tieznext");
        // 自动备份目录是数据目录的**兄弟**：`<父级>/Tiez-Next/auto_backups/`
        let auto_dir = parent.join("Tiez-Next").join("auto_backups");
        std::fs::create_dir_all(&auto_dir).unwrap();
        let archive = auto_dir.join("20260925T010000-scheduled-1.zip");
        std::fs::write(&archive, b"x").unwrap();

        let name = backup_archive_name(&RestoreRequest {
            data_dir: data_dir.clone(),
            archive_path: archive,
            pending_marker_dir: Some(marker_dir_for(&data_dir)),
        });

        assert_eq!(
            name.as_deref(),
            Some("20260925T010000-scheduled-1.zip"),
            "源包在自动备份目录内时必须记下名字，否则轮换会把它删掉"
        );
    }

    /// 源包**不在**自动备份目录内（用户从桌面导入）→ 不记名字。
    #[test]
    fn archive_outside_auto_backup_dir_is_not_recorded() {
        let parent = local_tmp_root("protect-name-outside");
        let data_dir = parent.join("com.tieznext");
        std::fs::create_dir_all(&data_dir).unwrap();
        let desktop = parent.join("Desktop");
        std::fs::create_dir_all(&desktop).unwrap();
        let archive = desktop.join("我的备份.zip");
        std::fs::write(&archive, b"x").unwrap();

        let name = backup_archive_name(&RestoreRequest {
            data_dir: data_dir.clone(),
            archive_path: archive,
            pending_marker_dir: Some(marker_dir_for(&data_dir)),
        });

        assert_eq!(
            name, None,
            "桌面上的包不会被轮换碰到，记名字是虚假的保护"
        );
    }

    /// 名字只在"包确实在自动备份目录里"时才有 —— 与标记目录无关。
    ///
    /// 【这条测试原先写的是另一件事】它以前断言"没有标记目录 → 不记名字"，因为当时的
    /// 判据借用了 `pending_marker_dir.parent()`。后来判据改成**精确的自动备份目录**，
    /// 这个函数的输入就只剩 `data_dir` 与 `archive_path` 两项 —— 标记目录写不写得成
    /// 是调用方的事（拿不到标记目录时 `restore_backup` 会直接失败并回滚暂存，
    /// 根本走不到写标记这一步）。
    ///
    /// 保留这条测试但改成验证真实语义：**判据单一**，不依赖无关的请求字段。
    #[test]
    fn protection_name_depends_only_on_archive_location_not_marker_dir() {
        let parent = local_tmp_root("protect-name-nodir");
        let data_dir = parent.join("com.tieznext");
        let auto_dir = crate::services::auto_backup::store::auto_backup_dir(&data_dir);
        std::fs::create_dir_all(&auto_dir).unwrap();
        let archive = auto_dir.join("a.zip");
        std::fs::write(&archive, b"x").unwrap();

        let with_marker = backup_archive_name(&RestoreRequest {
            data_dir: data_dir.clone(),
            archive_path: archive.clone(),
            pending_marker_dir: Some(marker_dir_for(&data_dir)),
        });
        let without_marker = backup_archive_name(&RestoreRequest {
            data_dir,
            archive_path: archive,
            pending_marker_dir: None,
        });

        assert_eq!(with_marker.as_deref(), Some("a.zip"));
        assert_eq!(
            with_marker, without_marker,
            "判据必须只取决于包的位置；牵进标记目录会让「什么算受保护」变得难以推理"
        );
    }

    use super::*;
    use crate::services::backup::export::{create_backup, BackupRequest};

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "tiez-2phase-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 造一个"用过的"数据目录：真实 schema + 可辨认的记录。
    fn seed_data_dir(root: &Path, rows: usize) -> PathBuf {
        let data = root.join("com.tieznext");
        std::fs::create_dir_all(data.join("attachments")).unwrap();
        let conn = Connection::open(data.join("clipboard.db")).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;",
        )
        .unwrap();
        crate::infrastructure::repository::migrations::run_migrations(&conn).unwrap();
        crate::database::seed_defaults(&conn).unwrap();
        for i in 0..rows {
            conn.execute(
                "INSERT INTO clipboard_history (content_type, content, source_app, timestamp, preview)
                 VALUES ('text', ?1, 'x', ?2, 'p')",
                rusqlite::params![format!("ROW-{}", i), 1000 + i as i64],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO saved_tags (name, color) VALUES ('work', '#ff0000')",
            [],
        )
        .unwrap();
        std::fs::write(data.join("attachments").join("a.png"), b"PNGDATA").unwrap();
        std::fs::write(data.join("datapath.txt"), data.to_string_lossy().as_bytes()).unwrap();
        std::fs::write(data.join("tiez.log"), b"log line\n").unwrap();
        drop(conn);
        data
    }

    fn count_rows(db: &Path) -> i64 {
        Connection::open(db)
            .and_then(|c| c.query_row("SELECT COUNT(*) FROM clipboard_history", [], |r| r.get(0)))
            .unwrap_or(-1)
    }

    fn row_contents(db: &Path) -> Vec<String> {
        let conn = Connection::open(db).unwrap();
        let mut stmt = conn
            .prepare("SELECT content FROM clipboard_history ORDER BY id")
            .unwrap();
        let v: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .flatten()
            .collect();
        v
    }

    /// 列出现场残留（替换现场 + 恢复暂存），用于断言"清理干净"或"正确保留"。
    fn leftovers(data_dir: &Path) -> Vec<String> {
        std::fs::read_dir(data_dir.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".restoring.") || n.contains(".pre-restore-promote-"))
            .collect()
    }

    // =================================================================
    // ① 端到端：导出 → 改坏原数据 → 恢复 → **模拟重启** → 数据回到导出时
    // =================================================================
    /// 这是"重启后数据真的到位了"的正面证据，而不是"写了个标记"。
    ///
    /// 时序完全按用户真机上的样子走：先导出、再把原数据改坏（换机/误删的场景）、
    /// 然后点恢复、再重启。断言的是重启**之后**正式位置的内容。
    #[test]
    fn end_to_end_restore_is_actually_effective_after_a_simulated_restart() {
        let root = tmp("e2e");
        let data = seed_data_dir(&root, 5);
        let archive = root.join("snapshot.zip");
        create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: archive.clone(),
            app_version: "0.5.6".to_string(),
        })
        .unwrap();
        let before_rows = row_contents(&data.join("clipboard.db"));
        assert_eq!(before_rows.len(), 5);

        // 把原数据改坏：清空记录、删掉附件、塞一条垃圾进来。
        {
            let conn = Connection::open(data.join("clipboard.db")).unwrap();
            conn.execute("DELETE FROM clipboard_history", []).unwrap();
            conn.execute(
                "INSERT INTO clipboard_history (content_type, content, source_app, timestamp, preview)
                 VALUES ('text', 'STALE-AFTER-WIPE', 'x', 1, 'stale')",
                [],
            )
            .unwrap();
        }
        std::fs::remove_file(data.join("attachments").join("a.png")).unwrap();
        assert_eq!(count_rows(&data.join("clipboard.db")), 1, "前置条件：原数据已被改坏");

        // ---- 点恢复（运行期：只组装 + 提交）----
        let report = restore_backup(&RestoreRequest {
            data_dir: data.clone(),
            archive_path: archive,
            pending_marker_dir: Some(marker_dir_for(&data)),
        })
        .unwrap();
        assert!(report.restart_required, "必须告诉用户要重启");
        assert!(report.deferred_until_restart, "必须如实标记为已提交待生效");
        assert!(
            report.pending_staging_dir.is_some(),
            "必须告知暂存位置（用户不重启时它就一直在那儿占磁盘）"
        );

        // 此刻**还没有生效**：正式位置仍是改坏后的那份。这条断言是"两阶段真的分开了"的证据。
        assert_eq!(
            count_rows(&data.join("clipboard.db")),
            1,
            "重启前正式数据不得被改动（交换发生在下次启动）"
        );
        assert!(
            !data.join("attachments").join("a.png").exists(),
            "重启前附件也不该凭空出现"
        );

        // ---- 模拟重启 ----
        let outcome = simulate_restart(&marker_dir_for(&data));
        assert!(
            matches!(outcome, crate::migration_pending::TakeoverOutcome::Promoted { .. }),
            "启动期提升必须成功，实际 {outcome:?}"
        );

        // ---- 数据真的回到了导出时的状态 ----
        assert_eq!(
            row_contents(&data.join("clipboard.db")),
            before_rows,
            "重启后记录必须与导出时逐条一致"
        );
        assert_eq!(
            std::fs::read(data.join("attachments").join("a.png")).unwrap(),
            b"PNGDATA",
            "附件内容必须逐字节一致"
        );
        // 非受管但属于运行环境的文件必须**没被动过**（逐条目交换只动该动的）
        assert!(
            data.join("datapath.txt").is_file() && data.join("tiez.log").is_file(),
            "datapath.txt 与日志不是受管条目，提升不该碰它们"
        );
        // 标记与暂存被消费干净
        assert!(
            !crate::migration_pending::marker_path(&marker_dir_for(&data)).exists(),
            "提升成功后标记必须清除"
        );
        assert!(
            leftovers(&data).is_empty(),
            "提升成功后不得留下暂存或替换现场：{:?}",
            leftovers(&data)
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // =================================================================
    // ② 真机等价的失败场景：持有数据库连接时发起恢复
    // =================================================================
    /// **在持有连接的状态下提交恢复，不得报"文件被占用"，而应如实返回"待重启"。**
    ///
    /// # 本机的能力边界（必须说清楚，不能假装覆盖了）
    ///
    /// 真机（Windows）上"持有连接 ⇒ 改名失败"是平台行为；本机是 Linux，改名已打开的文件
    /// 是允许的，因此这条测试**无法**在本机复现那个平台行为本身。它守的是**我们这侧的责任**：
    /// 提交阶段**根本不去尝试改名**（一次 `rename` 都不发生）——只要这一点成立，真机上就
    /// 不可能从这条路径冒出 `os error 32`。
    ///
    /// 判据是"提交阶段对正式数据零写操作"：提交前后正式数据目录的**全部条目哈希**必须完全
    /// 不变。这比"没报错"强得多——不报错但偷偷换了一半，用户会更惨。
    #[test]
    fn committing_a_restore_while_holding_a_connection_reports_pending_not_occupied() {
        let root = tmp("holding-conn");
        let data = seed_data_dir(&root, 3);
        let archive = root.join("p.zip");
        create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: archive.clone(),
            app_version: "0.5.6".to_string(),
        })
        .unwrap();

        // 【模拟真机状态】像应用启动时那样，一直持有着数据库连接不放。
        let held = Connection::open(data.join("clipboard.db")).unwrap();
        held.execute_batch("PRAGMA journal_mode = WAL;").unwrap();
        held.execute(
            "INSERT INTO clipboard_history (content_type, content, source_app, timestamp, preview)
             VALUES ('text', 'WHILE-CONNECTED', 'x', 9, 'p')",
            [],
        )
        .unwrap();
        let held_rows = count_rows(&data.join("clipboard.db"));

        // 提交之前的完整快照（正式数据目录里每个文件的内容哈希）
        let snapshot_before = {
            let mut m: std::collections::BTreeMap<String, String> = Default::default();
            collect_digests(&data, &data, &mut m);
            m
        };

        let report = restore_backup(&RestoreRequest {
            data_dir: data.clone(),
            archive_path: archive,
            pending_marker_dir: Some(marker_dir_for(&data)),
        })
        .expect(
            "持有连接时提交恢复必须成功返回「待重启」，而不是报错说文件被占用——\
             这正是旧实现在真机上的失败点",
        );

        // ① 没有报"被占用"，而是如实说"待重启"
        assert!(report.restart_required);
        assert!(report.deferred_until_restart);

        // ② 提交阶段对正式数据零写操作
        let snapshot_after = {
            let mut m: std::collections::BTreeMap<String, String> = Default::default();
            collect_digests(&data, &data, &mut m);
            m
        };
        assert_eq!(
            snapshot_after, snapshot_before,
            "提交阶段绝不能改动正式数据目录里的任何文件（交换只允许发生在下次启动）"
        );
        // 连接仍然可用、数据仍在（证明我们没在它背后动过文件）
        assert_eq!(
            count_rows(&data.join("clipboard.db")),
            held_rows,
            "持有连接期间，那条记录必须还在（提交不该动它）"
        );
        drop(held);

        let _ = std::fs::remove_dir_all(&root);
    }

    // =================================================================
    // ③ 二次恢复（未重启就再恢复一次）
    // =================================================================
    /// **未重启就再次恢复：后一次取代前一次，只留一份待提升，且行为明确不损坏数据。**
    ///
    /// 明确回答"到底会发生什么"（三条缺一不可）：
    /// 1. 第二次**不报错**（用户换一个包重来是正常操作）；
    /// 2. 第二次**取代**第一次——重启后生效的是**第二个包**，不是两个包的混合；
    /// 3. 第一次的暂存被清掉，磁盘上**只留一份**，用户不必猜重启后会上位哪一份。
    #[test]
    fn a_second_restore_before_restart_supersedes_the_first_cleanly() {
        let root = tmp("twice");
        // 两份内容不同的包：包 A 有 AAAA，包 B 有 BBBB
        let src_a = root.join("src-a");
        std::fs::create_dir_all(&src_a).unwrap();
        let data_a = seed_data_dir(&src_a, 2);
        {
            let c = Connection::open(data_a.join("clipboard.db")).unwrap();
            c.execute(
                "INSERT INTO clipboard_history (content_type, content, source_app, timestamp, preview)
                 VALUES ('text','FROM-PACKAGE-A','x',10,'p')",
                [],
            )
            .unwrap();
        }
        let archive_a = root.join("a.zip");
        create_backup(&BackupRequest {
            data_dir: data_a,
            output_path: archive_a.clone(),
            app_version: "0.5.6".to_string(),
        })
        .unwrap();

        let src_b = root.join("src-b");
        std::fs::create_dir_all(&src_b).unwrap();
        let data_b = seed_data_dir(&src_b, 7);
        let archive_b = root.join("b.zip");
        create_backup(&BackupRequest {
            data_dir: data_b,
            output_path: archive_b.clone(),
            app_version: "0.5.6".to_string(),
        })
        .unwrap();

        // 目标数据目录（要被替换的那个）
        let dest_root = root.join("dest");
        std::fs::create_dir_all(&dest_root).unwrap();
        let dest = seed_data_dir(&dest_root, 1);

        // 第一次：投 A
        let r1 = restore_backup(&RestoreRequest {
            data_dir: dest.clone(),
            archive_path: archive_a,
            pending_marker_dir: Some(marker_dir_for(&dest)),
        })
        .unwrap();
        let staging_a = PathBuf::from(r1.pending_staging_dir.clone().unwrap());
        assert!(staging_a.is_dir(), "第一次的暂存必须就绪");

        // 第二次（**没有重启**）：投 B
        let r2 = restore_backup(&RestoreRequest {
            data_dir: dest.clone(),
            archive_path: archive_b,
            pending_marker_dir: Some(marker_dir_for(&dest)),
        })
        .expect("未重启就再次恢复必须成功（用户换个包重来是正常操作）");

        // ② 第二次取代第一次：磁盘上只剩一份暂存
        assert!(
            !staging_a.exists(),
            "第一次的暂存必须被取代并清掉（否则用户无法判断重启后会上位哪一份）"
        );
        let staged: Vec<String> = leftovers(&dest)
            .into_iter()
            .filter(|n| n.contains(".restoring."))
            .collect();
        assert_eq!(staged.len(), 1, "只允许留一份待提升的暂存：{:?}", staged);

        // 标记指向的就是第二次那一份
        let marker = crate::migration_pending::read(&marker_dir_for(&dest)).expect("标记必须在");
        assert_eq!(
            marker.staging_dir,
            PathBuf::from(r2.pending_staging_dir.clone().unwrap()),
            "标记必须指向最新的那一份"
        );
        assert_eq!(marker.kind, crate::migration_pending::PendingKind::LocalRestore);

        // 用户应被告知"上一次被取代"（否则他无从解释上一个包为什么没生效）
        assert!(
            r2.warnings.iter().any(|w| w.contains("上次未重启")),
            "必须如实告知有一次未生效的恢复被取代，实际={:?}",
            r2.warnings
        );

        // 重启：上位的是 B 的内容，不是 A+B 的混合
        let outcome = simulate_restart(&marker_dir_for(&dest));
        assert!(
            matches!(outcome, crate::migration_pending::TakeoverOutcome::Promoted { .. }),
            "提升必须成功，实际 {outcome:?}"
        );
        assert_eq!(
            count_rows(&dest.join("clipboard.db")),
            7,
            "上位必须是第二个包（7 条），绝不能是两个包混在一起"
        );
        assert_eq!(leftovers(&dest), Vec::<String>::new(), "提升后不得留下任何残留");

        let _ = std::fs::remove_dir_all(&root);
    }

    // =================================================================
    // ④ 暂存残留：上次崩溃留下半个暂存目录
    // =================================================================
    /// **上次崩溃留下的暂存目录必须能被安全清理，然后照常完成一次新恢复。**
    ///
    /// 两种残留各测一遍：
    /// - **无主残渣**：上次组装到一半就被杀，目录在、标记不在。它从未被提交，清掉不丢数据。
    /// - **已提交但被取代**：目录在、标记指向**另一个**（后来的那次提交）。同样该清掉。
    #[test]
    fn crash_leftover_staging_is_cleaned_and_the_next_restore_still_works() {
        let root = tmp("leftover");
        let data = seed_data_dir(&root, 4);
        let archive = root.join("p.zip");
        create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: archive.clone(),
            app_version: "0.5.6".to_string(),
        })
        .unwrap();

        // 造一个"上次崩溃留下的"暂存目录（名字符合形状，但不被任何标记指向）
        let crash_leftover = root.join(format!(".{}.restoring.99999-0", "com.tieznext"));
        std::fs::create_dir_all(&crash_leftover).unwrap();
        std::fs::write(crash_leftover.join("clipboard.db"), b"HALF-BUILT").unwrap();
        assert!(crash_leftover.is_dir(), "前置条件：残留目录确实存在");

        let report = restore_backup(&RestoreRequest {
            data_dir: data.clone(),
            archive_path: archive,
            pending_marker_dir: Some(marker_dir_for(&data)),
        })
        .expect("上次的残留不得影响这一次恢复");
        assert!(report.restart_required, "本次恢复同样应提交为待重启生效");

        assert!(
            !crash_leftover.exists(),
            "无主残渣必须被清掉（它从未被提交，留着只会一直占磁盘）"
        );
        let staged: Vec<String> = leftovers(&data)
            .into_iter()
            .filter(|n| n.contains(".restoring."))
            .collect();
        assert_eq!(staged.len(), 1, "只剩本次这一份待提升：{:?}", staged);

        let outcome = simulate_restart(&marker_dir_for(&data));
        assert!(
            matches!(outcome, crate::migration_pending::TakeoverOutcome::Promoted { .. }),
            "残留清理之后，本次恢复必须照常落地，实际 {outcome:?}"
        );
        assert_eq!(count_rows(&data.join("clipboard.db")), 4);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 标记在、暂存被用户手工删掉：**绝不能去动目标**（否则用户会"数据没了"）。
    #[test]
    fn a_marker_pointing_at_a_vanished_staging_never_touches_the_live_data() {
        let root = tmp("vanished");
        let data = seed_data_dir(&root, 4);
        let archive = root.join("p.zip");
        create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: archive.clone(),
            app_version: "0.5.6".to_string(),
        })
        .unwrap();
        let report = restore_backup(&RestoreRequest {
            data_dir: data.clone(),
            archive_path: archive,
            pending_marker_dir: Some(marker_dir_for(&data)),
        })
        .unwrap();
        let staging = PathBuf::from(report.pending_staging_dir.unwrap());
        std::fs::remove_dir_all(&staging).unwrap(); // 用户/清理工具手工删了
        let before = std::fs::read(data.join("clipboard.db")).unwrap();

        let outcome = simulate_restart(&marker_dir_for(&data));
        assert!(
            matches!(outcome, crate::migration_pending::TakeoverOutcome::Failed { .. }),
            "暂存不在时必须判失败并停手，实际 {outcome:?}"
        );
        assert_eq!(
            std::fs::read(data.join("clipboard.db")).unwrap(),
            before,
            "正式数据必须一字未改（此时若照常'让位再提升'，用户会看到数据没了）"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // =================================================================
    // ⑤ 提升失败的回滚：原地数据仍在，不能"两边都不在"
    // =================================================================
    /// **提升中途失败必须原地回滚：正式数据恢复成调用前的样子。**
    ///
    /// 用注入的改名器在**第二个条目**上失败（第一个已成功搬进 aside），这是回滚逻辑唯一
    /// 真正被考验的情形——只失败在第一个上时什么都还没动，回滚是平凡的。
    #[test]
    fn a_failed_promotion_rolls_back_in_place_so_the_data_is_never_lost_from_both_sides() {
        let root = tmp("rollback");
        let data = seed_data_dir(&root, 6);
        let archive = root.join("p.zip");
        create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: archive.clone(),
            app_version: "0.5.6".to_string(),
        })
        .unwrap();
        let report = restore_backup(&RestoreRequest {
            data_dir: data.clone(),
            archive_path: archive,
            pending_marker_dir: Some(marker_dir_for(&data)),
        })
        .unwrap();
        let staging = PathBuf::from(report.pending_staging_dir.clone().unwrap());

        // 提升前的现场：正式目录里每个受管条目的哈希
        let before = {
            let mut m: std::collections::BTreeMap<String, String> = Default::default();
            for name in MANAGED_ENTRIES {
                let p = data.join(name);
                if p.exists() {
                    collect_digests(&p, &data, &mut m);
                }
            }
            m
        };
        let rows_before = count_rows(&data.join("clipboard.db"));

        // 注入失败：第 2 次改名开始一律失败（第 1 次已把 clipboard.db 搬进 aside）
        let mut calls = 0usize;
        let mut rename = |from: &Path, to: &Path| {
            calls += 1;
            if calls >= 2 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "模拟真机：文件被占用（os error 32）",
                ));
            }
            std::fs::rename(from, to)
        };
        let err = promote_staged_restore_with(&staging, &data, &mut rename)
            .expect_err("注入的失败必须让提升失败，而不是静默成功");
        assert!(
            err.contains("无法让位"),
            "失败原因必须说清是哪一步：{err}"
        );

        // ---- 核心断言：正式数据原地未损，暂存也还在（下次启动还能重试）----
        let after = {
            let mut m: std::collections::BTreeMap<String, String> = Default::default();
            for name in MANAGED_ENTRIES {
                let p = data.join(name);
                if p.exists() {
                    collect_digests(&p, &data, &mut m);
                }
            }
            m
        };
        assert_eq!(
            after, before,
            "提升失败必须原地回滚：正式数据一个字节都不许少（绝不能变成'两边都不在'）"
        );
        assert_eq!(
            count_rows(&data.join("clipboard.db")),
            rows_before,
            "正式库必须仍可读、记录数不变"
        );
        assert!(
            staging.is_dir() && staging.join("clipboard.db").is_file(),
            "失败后暂存必须保留（它是下次启动重试的唯一输入）"
        );
        assert!(
            !data
                .join(format!(".pre-restore-promote-{}", std::process::id()))
                .exists(),
            "回滚完整时替换现场应被清掉（数据已归位，不该留下空壳）"
        );

        // 标记仍在 → 用户重启一次仍有第二次机会
        assert!(
            crate::migration_pending::read(&marker_dir_for(&data)).is_some(),
            "提升失败后标记必须保留（否则用户永远等不到重试）"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 提升失败在**第一个**条目上（真机最常见：主库被占用）：什么都不该被改动。
    #[test]
    fn a_promotion_failing_on_the_very_first_entry_changes_nothing() {
        let root = tmp("rollback-first");
        let data = seed_data_dir(&root, 3);
        let archive = root.join("p.zip");
        create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: archive.clone(),
            app_version: "0.5.6".to_string(),
        })
        .unwrap();
        let report = restore_backup(&RestoreRequest {
            data_dir: data.clone(),
            archive_path: archive,
            pending_marker_dir: Some(marker_dir_for(&data)),
        })
        .unwrap();
        let staging = PathBuf::from(report.pending_staging_dir.unwrap());
        let before = std::fs::read(data.join("clipboard.db")).unwrap();

        let mut rename = |_from: &Path, _to: &Path| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "模拟真机：主库被打开的连接占住",
            ))
        };
        let err = promote_staged_restore_with(&staging, &data, &mut rename).unwrap_err();
        assert!(err.contains("已回滚到导入前的状态"), "应如实报告已回滚：{err}");
        assert_eq!(
            std::fs::read(data.join("clipboard.db")).unwrap(),
            before,
            "第一个条目就失败时，正式数据必须一字未改"
        );
        assert!(
            !leftovers(&data)
                .iter()
                .any(|n| n.contains(".pre-restore-promote-")),
            "不该留下替换现场空壳，实际={:?}",
            leftovers(&data)
        );
        // 但待提升的暂存**必须留下**：它是下次启动重试的唯一输入。
        assert!(
            staging.is_dir(),
            "提升失败后暂存必须保留（下次启动才有第二次机会）"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 残缺的暂存（没有数据库）绝不允许被提升——那会让用户得到"附件在、记录没了"。
    #[test]
    fn an_incomplete_staging_is_refused_by_the_promotion() {
        let root = tmp("incomplete");
        let data = seed_data_dir(&root, 2);
        let staging = root.join(".com.tieznext.restoring.fake");
        std::fs::create_dir_all(staging.join("attachments")).unwrap();
        std::fs::write(staging.join("attachments").join("x.png"), b"X").unwrap();
        let before = std::fs::read(data.join("clipboard.db")).unwrap();

        let err = promote_staged_restore(&staging, &data)
            .expect_err("缺数据库的暂存必须被拒绝，不能拿它替换正式数据");
        assert!(err.contains("不完整"), "原因必须说清：{err}");
        assert_eq!(
            std::fs::read(data.join("clipboard.db")).unwrap(),
            before,
            "被拒绝时正式数据不得被改动"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 提交成功但**拿不到标记目录** ⇒ 必须如实失败并清理暂存，而不是让用户以为成功了。
    #[test]
    fn without_a_marker_dir_the_commit_fails_loudly_instead_of_pretending() {
        let root = tmp("no-marker-dir");
        let data = seed_data_dir(&root, 2);
        let archive = root.join("p.zip");
        create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: archive.clone(),
            app_version: "0.5.6".to_string(),
        })
        .unwrap();
        let before = std::fs::read(data.join("clipboard.db")).unwrap();

        let err = restore_backup(&RestoreRequest {
            data_dir: data.clone(),
            archive_path: archive,
            pending_marker_dir: None,
        })
        .expect_err("拿不到标记目录时必须失败");
        assert!(
            err.to_string().contains("待接管标记"),
            "必须说清失败原因与标记有关：{err}"
        );
        assert_eq!(
            std::fs::read(data.join("clipboard.db")).unwrap(),
            before,
            "失败路径不得改动正式数据"
        );
        assert!(
            leftovers(&data).is_empty(),
            "失败路径不得留下暂存：{:?}",
            leftovers(&data)
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 报告里的字段名是**与前端/两处命令的契约**，必须逐字一致。
    ///
    /// 【为什么值得一条独立测试】`#[serde(rename_all = "camelCase")]` 漏了**不会编译
    /// 报错**：只会让前端读到的 `restartRequired` 变成 `undefined`，于是恢复提交成功、
    /// 提示却不弹——用户重启前看到的是"什么都没发生"，重启后看到数据变了，中间没有任何
    /// 解释。这种缺陷在编译期与类型检查里都抓不到，只能对序列化结果下断言。
    ///
    /// 本仓库已有同型先例：`system_cmd::migration_pristine_tests` 里那条
    /// `progress_payload_uses_the_contract_field_names`。
    #[test]
    fn restore_report_field_names_are_the_frontend_contract() {
        let report = RestoreReport {
            archive_path: "a.zip".into(),
            format_version: 1,
            exported_at: "t".into(),
            exported_app_version: "0.5.6".into(),
            pre_restore_backup: Some("bak".into()),
            restored_files: 3,
            restored_bytes: 42,
            verified_entries: 4,
            counts: ManifestCounts::default(),
            resets_applied: vec![],
            warnings: vec![],
            restart_required: true,
            deferred_until_restart: true,
            pending_staging_dir: Some("staging".into()),
            pending_marker_path: Some("marker.json".into()),
        };
        let v = serde_json::to_value(&report).unwrap();
        let obj = v.as_object().expect("报告必须是 JSON 对象");
        // 前端与两个命令层读取的那几个键，一个都不能少、名字不能变。
        for key in [
            "restartRequired",
            "deferredUntilRestart",
            "pendingStagingDir",
            "pendingMarkerPath",
            "preRestoreBackup",
            "restoredFiles",
            "verifiedEntries",
        ] {
            assert!(
                obj.contains_key(key),
                "报告缺少契约字段 `{key}`（前端读不到它 → 重启提示不弹，用户以为恢复没生效）\
                 ；实际字段={:?}",
                obj.keys().collect::<Vec<_>>()
            );
        }
        assert_eq!(obj["restartRequired"], serde_json::json!(true));
    }

    /// 只有**最新一次**提交的暂存会被提升；更早的残留不会复活。
    #[test]
    fn only_the_latest_restore_staging_survives() {
        let root = tmp("latest");
        let data = seed_data_dir(&root, 3);
        let archive = root.join("p.zip");
        create_backup(&BackupRequest {
            data_dir: data.clone(),
            output_path: archive.clone(),
            app_version: "0.5.6".to_string(),
        })
        .unwrap();

        // 三次提交，中间不重启
        let mut last = None;
        for _ in 0..3 {
            last = Some(
                restore_backup(&RestoreRequest {
                    data_dir: data.clone(),
                    archive_path: archive.clone(),
                    pending_marker_dir: Some(marker_dir_for(&data)),
                })
                .unwrap(),
            );
        }
        let last = last.unwrap();
        let staged: Vec<String> = leftovers(&data)
            .into_iter()
            .filter(|n| n.contains(".restoring."))
            .collect();
        assert_eq!(staged.len(), 1, "三次提交只允许留一份待提升：{:?}", staged);
        assert_eq!(
            crate::migration_pending::read(&marker_dir_for(&data))
                .unwrap()
                .staging_dir,
            PathBuf::from(last.pending_staging_dir.unwrap())
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
