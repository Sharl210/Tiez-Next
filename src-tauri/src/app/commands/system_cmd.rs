use crate::app_state::AppDataDir;
use crate::database::ENCRYPT_PREFIX;
use crate::error::{AppError, AppResult};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json;
use tauri::{AppHandle, Manager, State};

// ---------------------------------------------------------------------------
// 迁移进度：把"复制到暂存"的过程变成界面能显示的进度
// ---------------------------------------------------------------------------

/// 进度事件的发射器。
///
/// ## 为什么要有它（而不是就地 `emit`）
///
/// 三件事必须同时成立，散在各处几乎必然漏掉一件：
///
/// 1. **节流**：`copying` 阶段按条目推进，最快每 150ms 发一次。上千个小文件时
///    逐条发事件会把通道刷爆，界面反而渲染不过来（比没有进度更糟）。
/// 2. **末条强制**：最后一个条目必须无条件发一次，否则进度条永远停在
///    `999/1000`，用户以为迁移卡住了（判据见 `migration_identifier::should_emit_progress`）。
/// 3. **结束必发**：`migration-done` 无论成功、跳过、`deferred` 还是失败都要发一次，
///    否则前端会永远停在"进行中"的禁用态。
///
/// 拿不到 `AppHandle` 时不报错、不阻断迁移（日志已留下全过程），只是界面看不到进度。
pub struct MigrationProgressEmitter {
    app: Option<AppHandle>,
    last_emit: std::time::Instant,
}

impl MigrationProgressEmitter {
    pub fn new(app: AppHandle) -> Self {
        Self {
            app: Some(app),
            last_emit: std::time::Instant::now(),
        }
    }

    /// 无宿主时的空实现（纯逻辑单测用）。
    pub fn detached() -> Self {
        Self {
            app: None,
            last_emit: std::time::Instant::now(),
        }
    }

    fn emit_payload(&self, event: &str, p: &crate::migration_identifier::MigrationProgress) {
        let Some(app) = self.app.as_ref() else {
            return;
        };
        use tauri::Emitter;
        if let Err(e) = app.emit(event, MigrateProgressPayload::from(p)) {
            crate::error!("[MIGRATION] 进度事件发送失败（不影响迁移本身）：{}", e);
        }
    }

    /// 阶段推进：**一定**发一次（阶段数量很少，不需要节流）。
    pub fn stage(&mut self, p: crate::migration_identifier::MigrationProgress) {
        self.last_emit = std::time::Instant::now();
        self.emit_payload(crate::migration_identifier::EVENT_MIGRATION_PROGRESS, &p);
    }

    /// 条目推进：按 `PROGRESS_THROTTLE_MS` 节流；`is_last` 时强制发一次。
    pub fn item(&mut self, p: crate::migration_identifier::MigrationProgress, is_last: bool) {
        let elapsed = self.last_emit.elapsed().as_millis() as u64;
        if !crate::migration_identifier::should_emit_progress(elapsed, is_last) {
            return;
        }
        self.last_emit = std::time::Instant::now();
        self.emit_payload(crate::migration_identifier::EVENT_MIGRATION_PROGRESS, &p);
    }

    /// 结束（成功 / 跳过 / deferred / 失败都调用它）。
    pub fn finish(&mut self, p: crate::migration_identifier::MigrationProgress) {
        self.emit_payload(crate::migration_identifier::EVENT_MIGRATION_PROGRESS, &p);
        self.emit_payload(crate::migration_identifier::EVENT_MIGRATION_DONE, &p);
    }
}

/// 进度事件的 payload（**冻结契约 §2**：字段名必须是 camelCase）。
///
/// `#[serde(rename_all = "camelCase")]` 漏了不会编译报错，只会让 `stageLabel` 变成
/// `undefined`——用户看到一根没有说明文字的进度条，"迁移显示"这个核心诉求等于没做。
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct MigrateProgressPayload {
    stage: &'static str,
    stage_label: &'static str,
    done: u64,
    total: u64,
    bytes: u64,
    bytes_total: u64,
    message: Option<String>,
}

impl From<&crate::migration_identifier::MigrationProgress> for MigrateProgressPayload {
    fn from(p: &crate::migration_identifier::MigrationProgress) -> Self {
        Self {
            stage: p.stage,
            stage_label: p.stage_label,
            done: p.done,
            total: p.total,
            bytes: p.bytes,
            bytes_total: p.bytes_total,
            message: p.message.clone(),
        }
    }
}

/// 组装一条进度快照（阶段文案的唯一来源在 `migration_identifier::stage_label`）。
fn progress(
    stage: &'static str,
    done: u64,
    total: u64,
    bytes: u64,
    bytes_total: u64,
    message: Option<String>,
) -> crate::migration_identifier::MigrationProgress {
    crate::migration_identifier::MigrationProgress {
        stage,
        stage_label: crate::migration_identifier::stage_label(stage),
        done,
        total,
        bytes,
        bytes_total,
        message,
    }
}

/// 带进度上报的两阶段迁移。
///
/// ## 为什么走 `stage_takeover`（运行期只复制、不交付）
///
/// 迁移入口是应用内界面 ⇒ 用户点它时应用必定在运行 ⇒ 目标库已被 `init_db` 打开、
/// 连接常驻 `DbState`。Windows 不允许给已打开的文件改名，因此"给目标空库让位"在
/// 运行期**必然**失败（os error 32）。所以运行期只做"复制到暂存 + 写标记"，
/// 由下次启动在 `init_db` 之前完成交换。
///
/// `takeover == false`（目标里已有用户数据）时不进入两阶段：那本来就是"跳过"，
/// 走既有入口以保持与 v0.5.2 相同的语义与原因码。
///
/// ## 进度是怎么算出来的（不是装样子）
///
/// 迁移**第一步就扫描源目录**（`scan_source_summary`），所以开始复制之前总数与总字节
/// 都已已知。`precheck` 阶段报的就是这个总数。
fn migrate_with_progress(
    source: &std::path::Path,
    target: &std::path::Path,
    takeover: bool,
    sink: &mut MigrationProgressEmitter,
) -> crate::migration_identifier::MigrationOutcome {
    use crate::migration_identifier as mi;

    // ---- 阶段 1：只读预检（总量在这里算出来）----
    let scanned = mi::scan_source_summary(source);
    let entries = scanned.map(|s| s.entries).unwrap_or(0);
    let bytes_total = scanned.map(|s| s.bytes).unwrap_or(0);
    sink.stage(progress("precheck", 0, entries, 0, bytes_total, None));

    // ---- 阶段 2：复制到暂存（每复制完一个文件回调一次）----
    //
    // `takeover == false`（目标里已有用户数据）时，`stage_takeover_with_progress`
    // 会在预检阶段就回 `Skipped(TargetAlreadyHasData)`——与一次性交付路径的语义
    // 完全一致，因此这里不需要再分一次支。
    sink.stage(progress("copying", 0, entries, 0, bytes_total, None));

    // 判据：是否是最后一个条目。`stage_takeover_with_progress` 保证最后一定会用
    // `files_total == done` 回调一次，这里据此把"末条强制发出"接上。
    let outcome = {
        let mut on_item = |done: u64, total: u64, bytes: u64, bytes_total: u64| {
            // `total > 0 && done >= total` = "这是最后一个条目"：节流对它**无条件放行**，
            // 保证进度条一定会走到 100%（否则它会停在 999/1000，用户以为卡死）。
            let is_last = total > 0 && done >= total;
            sink.item(
                progress("copying", done, total, bytes, bytes_total, None),
                is_last,
            );
        };
        mi::stage_takeover_with_progress(source, target, takeover, &mut on_item)
    };

    match outcome {
        mi::MigrationOutcome::Deferred {
            source,
            target,
            staging,
            files,
            bytes,
        } => {
            // ---- 阶段 3：校验（暂存已与源逐项比对过，这里只做呈现）----
            sink.stage(progress("verifying", files, files, bytes, bytes_total, None));
            sink.finish(progress(
                "deferred",
                0,
                0,
                0,
                0,
                Some("源数据已复制就绪。重启应用后自动完成接管，无需其他操作。".to_string()),
            ));
            mi::MigrationOutcome::Deferred {
                source,
                target,
                staging,
                files,
                bytes,
            }
        }
        other => {
            sink.finish(finish_progress(&other, entries, bytes_total));
            other
        }
    }
}

/// 非 `Deferred` 结果对应的结束进度。
fn finish_progress(
    outcome: &crate::migration_identifier::MigrationOutcome,
    total: u64,
    bytes_total: u64,
) -> crate::migration_identifier::MigrationProgress {
    use crate::migration_identifier::MigrationOutcome as M;
    match outcome {
        M::Migrated { files, bytes, .. } => progress(
            "done",
            *files,
            total.max(*files),
            *bytes,
            bytes_total.max(*bytes),
            None,
        ),
        M::Deferred { files, bytes, .. } => progress(
            "deferred",
            0,
            0,
            0,
            0,
            Some(format!("{} 项 / {} 字节已就绪", files, bytes)),
        ),
        M::Skipped(reason) => progress(
            "done",
            0,
            total,
            0,
            bytes_total,
            Some(format!("无需迁移：{}", skip_reason_human(*reason))),
        ),
        M::Failed { error, .. } => {
            progress("failed", 0, total, 0, bytes_total, Some(error.clone()))
        }
    }
}

/// 原因码 → 人话（**只用于进度事件的 `message`**；界面上的正式文案仍按 `skipReason`
/// 查多语言词条，两者互不替代）。
fn skip_reason_human(reason: crate::migration_identifier::SkipReason) -> String {
    use crate::migration_identifier::SkipReason as R;
    match reason {
        R::TargetAlreadyHasData => "新版数据目录里已经有你自己的记录".to_string(),
        R::NoLegacyDir => "没有找到可迁移的旧数据目录".to_string(),
        R::SamePath => "源目录与目标目录是同一个".to_string(),
        R::SourceMissing => "源目录不存在".to_string(),
        R::NotADirectory => "所选路径不是目录".to_string(),
        R::EmptySource => "源目录里没有数据".to_string(),
        R::NotADataDirectory => "所选目录不是数据目录，请往下选一层".to_string(),
        R::SourceIsAncestorOfTarget => "源目录是目标目录的上级".to_string(),
        R::SourceInsideTarget => "源目录在目标目录内部".to_string(),
    }
}

/// 把 `Deferred` 结果落成"待接管"标记；标记写失败时**如实改为失败**。
///
/// 【为什么失败必须改状态】标记是下次启动唯一能知道"有活要干"的凭据。若对用户说
/// "重启后自动完成"、而标记其实没写成功，用户重启后什么都没发生，只会认为这个功能
/// 又一次骗了他。此时唯一诚实的表达是"这次没成"，并保留暂存目录（下次还能重试）。
///
/// 非 `Deferred` 的结果原样返回，不做任何处理。
fn finalize_deferred(
    report: &mut crate::app::IdentifierMigrationReport,
    native_dir: Option<&std::path::Path>,
    source: &std::path::Path,
    target: &std::path::Path,
) {
    if report.status != "deferred" {
        return;
    }
    let Some(native) = native_dir else {
        report.status = "failed".to_string();
        report.pending_until_restart = false;
        report.error = Some(
            "取不到应用的原生数据目录，无法记录「待接管」状态；已复制到暂存的数据仍保留，请重试本次迁移。"
                .to_string(),
        );
        return;
    };

    let staging = crate::migration_identifier::takeover_staging_dir(target);
    // 【必须记**归一化之后**的源目录，不能记用户原选的那一层】
    //
    // 用户常常停在便携版的**外层**目录上（解压出来是两层同名目录，真正的数据在
    // `外层/内层/data/`）。接管之后要按"源 → 目标"改写库里记录的附件/表情绝对路径，
    // 而记录里写的是**内层 `data/`** 的绝对路径。
    //
    // 若这里记的是外层：`source.join("attachments")` 与外层不匹配 ⇒ 一条都改不到
    // （路径改写静默无效）；更糟的是若外层恰好也有个 `attachments/` 目录，改写会把
    // 记录指向一个**不存在**的位置。两种后果都表现为"数据在、图片全打不开"。
    //
    // 归一化结果与 `stage_takeover` 内部用的那一个完全一致（同一个只读函数），
    // 因此记它才是"真正被复制的那一层"。
    let resolved_source = crate::migration_identifier::resolve_source_dir(source);
    let pending = crate::migration_pending::PendingMigration::new(
        resolved_source,
        staging,
        target.to_path_buf(),
        env!("CARGO_PKG_VERSION"),
    );
    match crate::migration_pending::write(native, &pending) {
        Ok(path) => {
            crate::info!(">>> [MIGRATION] 已写入待接管标记：{:?}", path);
            report.pending_until_restart = true;
        }
        Err(e) => {
            crate::error!("[MIGRATION] 待接管标记写入失败：{}", e);
            report.status = "failed".to_string();
            report.pending_until_restart = false;
            report.error = Some(format!(
                "数据已复制到暂存目录，但「待接管」状态未能记下（{}）；请重试本次迁移。源目录未被改动。",
                e
            ));
        }
    }
}

/// 供界面在 `listen` 之后拉一次当前快照的命令。
///
/// 【为什么需要它】冻结契约只规定了两个**事件**，没规定"初值怎么拉"。若前端只
/// `listen` 不拉初值，那一轮里已经发过的事件会永久丢失，用户看到进度条卡在第一帧。
/// 本命令返回**最近一次**进度快照；从未有过迁移时返回 `None`（前端据此显示空闲态，
/// **不伪造进度**）。
///
/// 快照在内存里，进程重启即清空——这是对的：迁移进度本来就是"本次运行"的状态。
#[tauri::command]
pub fn get_migration_progress() -> AppResult<Option<serde_json::Value>> {
    let guard = LAST_MIGRATION_PROGRESS.lock().unwrap();
    Ok(guard.clone())
}

/// 最近一次进度快照（`serde_json::Value`，与事件 payload 同构）。
static LAST_MIGRATION_PROGRESS: std::sync::Mutex<Option<serde_json::Value>> =
    std::sync::Mutex::new(None);


#[tauri::command]
pub fn get_data_path(state: State<'_, AppDataDir>) -> AppResult<String> {
    let path = state.0.lock().unwrap();
    Ok(path.to_string_lossy().to_string())
}

#[tauri::command]
pub fn open_folder(path: String) -> AppResult<()> {
    use std::process::Command;
    Command::new("explorer")
        .arg(path)
        .spawn()
        .map_err(|e| AppError::Internal(format!("Failed to open folder: {}", e)))?;
    Ok(())
}

#[tauri::command]
pub fn open_data_folder(state: State<'_, AppDataDir>) -> AppResult<()> {
    let path = state.0.lock().unwrap();
    let path_str = path.to_string_lossy().to_string();

    use std::process::Command;
    Command::new("explorer")
        .arg(path_str)
        .spawn()
        .map_err(|e| AppError::Internal(format!("Failed to open data folder: {}", e)))?;
    Ok(())
}

/// 供"迁移中心"展示的一条**可迁移来源**目录信息（前端友好格式）。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyDirView {
    pub path: String,
    pub identifier: String,
    /// 这条数据原本属于哪个应用：`legacy_tiez`（旧版 TieZ）或
    /// `previous_tiez_next`（历史版本的 Tiez-Next）。界面按此如实分类展示。
    ///
    /// 是稳定的机器可读码，不是文案——界面负责按语言映射（`legacy_origin_*`）。
    pub origin: String,
    pub bytes: u64,
    pub files: u64,
    pub has_database: bool,
    /// 是否允许"备份后删除"。本应用自己标识符的目录恒为 `false`。
    pub can_delete: bool,
}

/// 应用数据目录的**原生位置**（Tauri 由 `identifier` 推导）。
///
/// 【为什么命令层要自己算一遍】用户改了数据目录（`datapath.txt`）或使用便携版后，
/// `AppDataDir` 指向的是那个自定义目录，而原生位置里可能留着**旧版本 Tiez-Next 的
/// 数据**——它不在自定义目录同级，只扫 `AppDataDir` 会漏掉这条迁移来源。
///
/// 这里不新增全局状态位，而是按需推导：`app_data_dir()` 只做路径拼接，不读写磁盘，
/// 重复调用没有副作用。
fn native_data_dir(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path().app_data_dir().ok()
}

/// 迁移中心：列出可迁移来源目录（旧版 TieZ + 历史版本的 Tiez-Next）及其占用。
///
/// 只读操作，不修改任何数据。用户据此决定是否迁移或清理。
#[tauri::command]
pub fn list_legacy_data_dirs(
    app: AppHandle,
    state: State<'_, AppDataDir>,
) -> AppResult<Vec<LegacyDirView>> {
    let current = state.0.lock().unwrap().clone();
    let extra: Vec<std::path::PathBuf> = native_data_dir(&app).into_iter().collect();
    Ok(crate::migration_identifier::list_legacy_dirs(&current, &extra)
        .into_iter()
        .map(|i| LegacyDirView {
            path: i.path.to_string_lossy().to_string(),
            identifier: i.identifier,
            origin: i.origin.code().to_string(),
            bytes: i.bytes,
            files: i.files,
            has_database: i.has_database,
            can_delete: i.can_delete,
        })
        .collect())
}

/// 迁移中心：备份后删除一个遗留数据目录。
///
/// 安全边界：仅允许删除**旧版 TieZ**（`com.tiez.app` / `com.tiez`）的目录；先完整备份
/// 并校验，通过后才删除源目录；备份失败则不删除任何数据。返回备份目录路径供界面告知。
///
/// 本应用自己标识符（`com.tieznext`）的目录一律拒绝：那是用户留着的旧版 Tiez-Next
/// 数据，不是被取代的旧应用，清理按钮不该销毁它。
#[tauri::command]
pub fn remove_legacy_data_dir(
    app: AppHandle,
    state: State<'_, AppDataDir>,
    path: String,
) -> AppResult<String> {
    let current = state.0.lock().unwrap().clone();
    let target = std::path::PathBuf::from(&path);
    let extra: Vec<std::path::PathBuf> = native_data_dir(&app).into_iter().collect();

    // 额外守卫（只对**非白名单**路径生效）：新版那边还没有任何数据时，不允许删掉
    // 用户手选的旧目录——否则用户等于把剪贴板历史从应用会读的位置彻底抹掉。
    // 白名单内的历史标识符目录保持既有行为不变，避免影响原本就存在的清理流程。
    let is_whitelisted = crate::migration_identifier::migratable_source_dirs(&current, &extra)
        .iter()
        .any(|p| p == &target);
    if !is_whitelisted {
        can_remove_source_safely(&current).map_err(AppError::Validation)?;
    }

    crate::migration_identifier::backup_and_remove_legacy_dir(&current, &extra, &target)
        .map(|p| p.to_string_lossy().to_string())
        .map_err(AppError::Validation)
}

/// 判定某个数据目录里的数据库是不是**从未使用过的空库**。
///
/// 放在命令层而不是 `migration_identifier` 里，是因为它需要读 SQLite（rusqlite），
/// 而后者按设计**只依赖 `std`**，以便脱离 Tauri 与平台专用代码独立编译验证。
///
/// ## 为什么不能只看剪贴板条数
///
/// 复核（G-2）实证：用户完全可能在新版里**一条剪贴板记录都没有**，但已经建了自己的
/// 标签、调过设置。若只看 `clipboard_history` 条数就判定"没用过"并接管，源库会**整体
/// 替换**目标库，用户在新版里建的标签/设置被静默丢弃（实测报
/// `no such table: saved_tags`）。这与本任务"绝不伤害用户既有数据"的目标直接冲突。
///
/// ## 判定方式：与"刚装好的新版"逐键逐值比对
///
/// 不硬编码"哪些设置算默认"，而是**现场造一个全新的种子库**（同样的迁移 + 同样的
/// `seed_defaults`），把它当作"刚装好的新版"基线，再与目标逐 key、逐 value 比较。
/// 这样将来新增任何设置项都会自动被基线覆盖，**不需要回来维护一份白名单**——白名单
/// 一旦漏项就会把用户数据误判成空库，是本项目最不能承受的错误方向。
///
/// 只有三者**同时**成立才判定"没用过"：
/// 1. `clipboard_history` 条数为 0；
/// 2. `saved_tags` 里没有用户自建标签（默认的 `sensitive` / `密码` 不算）；
/// 3. `settings` 与全新种子库**完全一致**（key 集合与每个 value 都相同）。
///
/// ## 副作用
///
/// `seed_defaults` 是幂等的 `INSERT OR IGNORE`，因此把它作用在**探针副本**上是安全的；
/// 目标目录从未被本函数写过。探针是**文件层完整复制**（含 `-wal`/`-shm`），因此不会
/// 像"只复制主库"那样在 WAL 模式下漏读尚未 checkpoint 的记录；也避免了直接打开目标库
/// 会改写其 `clipboard.db-shm` 的副作用（该副作用由 `migration_identifier` 的回归测试
/// 抓出）。
///
/// ## 保守性
///
/// 库存在但复制失败、打不开、缺表、查询失败或基线库造不出来时，一律返回 `false`
/// （按"有数据"处理）。宁可少迁，不可覆盖。
fn target_db_is_pristine(target: &std::path::Path) -> bool {
    let db = target.join("clipboard.db");
    if !db.is_file() {
        return true; // 连库都没有 = 完全没用过
    }

    let stamp = std::process::id().to_string()
        + "-"
        + &std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
            .to_string();
    let probe_dir = std::env::temp_dir().join(format!("tiez-db-probe-{}", stamp));
    if std::fs::create_dir_all(&probe_dir).is_err() {
        return false;
    }

    // 文件层完整复制（含 WAL 侧车）：只读目标，不改目标。
    let mut copy_failed = false;
    for suffix in ["", "-wal", "-shm"] {
        let name = format!("clipboard.db{}", suffix);
        let from = target.join(&name);
        if from.is_file() && std::fs::copy(&from, probe_dir.join(&name)).is_err() {
            copy_failed = true;
            break;
        }
    }
    if copy_failed {
        let _ = std::fs::remove_dir_all(&probe_dir);
        return false;
    }

    let probe_db = probe_dir.join("clipboard.db");

    // 1) 剪贴板历史必须一条都没有。
    let clips: Option<i64> = rusqlite::Connection::open(&probe_db)
        .ok()
        .and_then(|c| {
            c.query_row("SELECT COUNT(*) FROM clipboard_history", [], |r| r.get(0))
                .ok()
        });
    if clips != Some(0) {
        let _ = std::fs::remove_dir_all(&probe_dir);
        return false;
    }

    // 2) 用户自建标签必须为空（默认的两个标签不算）。
    let user_tags: Option<i64> = rusqlite::Connection::open(&probe_db)
        .ok()
        .and_then(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM saved_tags WHERE name NOT IN ('sensitive', ?1)",
                rusqlite::params!["密码"],
                |r| r.get(0),
            )
            .ok()
        });
    if user_tags != Some(0) {
        let _ = std::fs::remove_dir_all(&probe_dir);
        return false;
    }

    // 3) 判定的核心是"里面有没有用户自己产生的东西"，不是"设置是否还是出厂值"。
    //
    // 【为什么删掉了"settings 必须与全新库完全一致"这一条】
    //
    // 那条判据要求用户的 settings 与刚装好的新版**逐键相等**。但应用启动与日常使用
    // 会**主动写入**大量设置：窗口尺寸（`setup.rs` 在窗口大小变化时写
    // `app.window_width` / `app.window_height`）、`app.anon_id`、粘贴方式等等。于是
    // 用户只要**调整过一次窗口大小**，判定即为 false，手动迁移从此**永远**被
    // `target_already_has_data` 挡掉——而界面只说"新版数据目录里已经有你自己的记录"，
    // 用户看得到的记录数却是 0，完全无从理解。
    //
    // 判据本身也前后矛盾：这里要判断的是"这个库能不能让位"，而与设置有关的证据
    // （`clipboard_history` / `saved_tags`）已经在上面两条查过了。设置被改过，
    // 恰恰说明用户在**用**这个应用，但那两条已经覆盖了"有没有数据"。
    //
    // 保留这一条会造成"数据明明还没进来、迁移却永久拒绝"的死局，因此移除。
    let pristine = true;

    let _ = std::fs::remove_dir_all(&probe_dir); // 探针目录始终清理；目标目录从未被写过
    pristine
}

/// 现场造一个"刚装好的新版"数据库，返回它的 settings 映射作为比对基线。
///
/// 复用产品自己的 `init_db`（迁移 + `seed_defaults`），因此基线永远与当前版本一致，
/// 新增设置项会自动进入基线，无需维护白名单。失败返回 `None`，调用方按"在用"处理。
fn build_seed_settings_baseline(
    path: &std::path::Path,
) -> Option<std::collections::HashMap<String, String>> {
    let path_str = path.to_string_lossy().to_string();
    // init_db 会建表、跑迁移并写入默认设置；对全新文件而言这是纯创建操作。
    crate::database::init_db(&path_str).ok()?;
    read_settings(path)
}

/// 读取一个数据库里的全部 settings 键值（只读）。
///
/// 返回 `None` 表示读不到（打不开 / 缺 `settings` 表）——调用方须按"在用"处理。
fn read_settings(db: &std::path::Path) -> Option<std::collections::HashMap<String, String>> {
    let conn = rusqlite::Connection::open(db).ok()?;
    let mut stmt = conn.prepare("SELECT key, value FROM settings").ok()?;
    let rows = stmt
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .ok()?
        .collect::<Result<std::collections::HashMap<_, _>, _>>()
        .ok()?;
    Some(rows)
}

/// 删除旧目录前的守卫：**只有新版确实存有可用数据时，才允许删掉用户手选的旧目录**。
///
/// 【为什么需要它】"备份后删除旧目录"与"把旧数据迁进新版"是两件独立的事：只要新版
/// 那边还没有任何数据，删掉旧目录就等于把用户的剪贴板历史从应用会读的位置抹掉
/// （备份虽然还在，但已不在应用读取的位置）。因此在用户手选路径的删除上再加一道
/// 拦截——新版没有数据就先别删。
fn can_remove_source_safely(target: &std::path::Path) -> Result<(), String> {
    let db = target.join("clipboard.db");
    if !db.is_file() {
        return Err(
            "新版数据目录里还没有数据库——先执行一次「从此目录迁移」，确认新版能正常看到旧记录之后再来清理。"
                .to_string(),
        );
    }
    if target_db_is_pristine(target) {
        return Err(
            "新版数据目录里还没有任何剪贴板记录——先执行一次「从此目录迁移」并重启确认，再来清理旧目录。"
                .to_string(),
        );
    }
    Ok(())
}

/// 迁移中心：从**用户手动指定**的源目录迁移数据到当前数据目录。
///
/// 这是用户明确要求的手动迁移入口：应用启动时不会自动搬任何数据，只有用户在新版
/// 应用里亲自选好旧数据目录并确认后，才会执行（`src-tauri/src/app/setup.rs` 的
/// `resolve_data_dir` 已去掉启动期自动迁移）。
///
/// ## 与 `migrate_legacy_identifier_data` 的差异
///
/// 源路径来自用户参数，因此不受白名单限制（用户可以选 D 盘、移动硬盘、任意备份目录）。
/// 安全性由 `crate::migration_identifier::migrate_from_source_dir` 的同一套契约保证：
///
/// 1. **源目录全程只读**——只 `read_dir` / `File::open` / `fs::copy`，不删除、不改名、
///    不写入源内任何文件。**本命令不会备份后删除源目录**，源目录永远留给用户自己处置。
/// 2. **先暂存后交付**——完整复制到目标同级的 `.…migrating.<pid>` 暂存目录，逐项校验
///    （相对路径 + 字节数）一致后才提升为正式目录。
/// 3. **失败只清暂存**——任何一步失败都只删暂存目录，源与既有目标保持原状。
/// 4. **幂等**：目标已有非空数据库一律跳过；只有目标那个库确实一条记录都没有时，
///    才允许本次手动迁移接管它（见下文"幂等语义"）。
/// 5. **源必须是数据目录**——用户很容易把上层目录（例如「下载」或解压出来的外层目录）
///    选成源。那种情况下源里确实有内容，但应用的数据只是其中最深处的一小块；若照常
///    迁移，整棵目录树（含无关文件夹、私钥、机密文档）都会被复制进应用数据目录，而
///    真正的数据落进应用**不读**的嵌套位置，界面却报"迁移完成"。因此
///    `migrate_from_source_dir` 在复制任何字节之前先确认源含 `clipboard.db`，不是就
///    回报 `skip_reason = "not_a_data_directory"`，界面据此告诉用户"往下选一层"。
///    本命令不做重复判定，拦截只在该模块里实现一次（见其"安全契约"第 6 条）。
///
/// ## 幂等语义（重复调用同一源路径）
///
/// 选择「**目标已有非空数据库则整体跳过**」，而不是合并或报错：
///
/// - 新版应用一旦启动过一次，就会在数据目录里创建空白 `clipboard.db`。因此"目标
///   已有数据库"是**常态**，不是异常——若判为错误，用户第一次启动后重试就只会看到
///   报错，与"可反复手动验证"的要求直接冲突。跳过是唯一能让重复迁移既不报错、
///   也不堆积数据的语义。
/// - 但**空库要区分对待**：目标里那个库若一条记录都没有，说明用户从没在新版里存过
///   东西，本次手动迁移允许接管它（空库改名留档后让位，判据见本文件
///   `target_db_is_pristine`）。不这样做的话，手动迁移会被用户自己刚装好的空库
///   永远挡住，功能形同虚设。
/// - 不能合并：合并要复制的恰好是 `clipboard.db`、`attachments/`、`emoji_favorites/`，
///   而目标已有自己的数据库与附件；覆盖它们就是破坏用户当前数据，与本应用"绝不覆盖
///   既有数据"的一贯契约相悖。跳过虽"少迁"，但绝不会迁坏。
/// - 报告里的 `skipReason` / `error` 是机器可读原因码，界面负责把它翻译成人话并告诉
///   用户"接下来该做什么"（例如先切换数据目录或清理新版数据后再迁移）。
///
/// ## 迁移成功后
///
/// 改写目标数据库内的绝对路径（`rewrite_data_paths_in_db`：只改数据库里的字符串，
/// 绝不移动或删除源目录中的文件），否则附件与表情收藏仍指向旧目录而失联。
/// 数据库连接在启动时已建立，界面应提示用户重启以加载新数据。
#[tauri::command]
pub fn migrate_from_data_dir(
    app: AppHandle,
    state: State<'_, AppDataDir>,
    path: String,
) -> AppResult<crate::app::IdentifierMigrationReport> {
    // 取当前数据目录后立即释放锁：迁移可能耗时，不应长时间占着全局状态锁。
    let current = {
        let guard = state.0.lock().unwrap();
        guard.clone()
    };

    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(AppError::Validation("未指定源数据目录".to_string()));
    }
    let source = std::path::PathBuf::from(trimmed);

    // 只读预检：把源目录会被迁移的文件逐个读一遍，作为"源可读"的证据写进日志。
    // 任何写入源目录的动作都不在此处，也不在后续任何一步。
    match crate::migration_identifier::check_source_is_readable(&source) {
        Ok((files, bytes)) => {
            crate::info!(
                ">>> [MIGRATION] 只读预检通过：源 {:?} 共 {} 个文件 / {} 字节（未做任何写入）。",
                source,
                files,
                bytes
            );
        }
        Err(e) => {
            crate::error!(
                "[MIGRATION] 只读预检未通过（未做任何写入）：源={:?} 原因={}",
                source,
                e
            );
        }
    }

    // 只有确认目标那个库从未被使用过（0 条记录），才允许本次迁移接管它。
    // 这是"应用一启动就建空库，不区分则手动迁移永远被挡"这个现实问题的唯一解，
    // 且判定保守：读不到、认不出、有任何记录都按"在用"处理。
    let takeover = target_db_is_pristine(&current);
    if takeover && current.join("clipboard.db").is_file() {
        crate::info!(
            ">>> [MIGRATION] 新版数据目录 {} 里的数据库尚无任何记录，本次迁移将接管它（原空库改名留档）。",
            current.display()
        );
    }

    // 进度发射器：阶段推进必发，`copying` 按条目节流（最快 150ms 一次），结束必发。
    let mut sink = MigrationProgressEmitter::new(app.clone());

    let outcome = migrate_with_progress(&source, &current, takeover, &mut sink);
    let mut report = crate::app::apply_identifier_migration(&source, &current, outcome);

    // 拿到 Deferred（数据已复制就绪、待下次启动接管）时**必须**写标记：
    // 标记是下次启动唯一能知道"有活要干"的凭据。写失败要如实回报为失败——
    // 不能对用户说"重启就行"，而重启后什么都没发生。
    let native = app.path().app_data_dir().ok();
    finalize_deferred(&mut report, native.as_deref(), &source, &current);

    // 快照：让晚一步连上 `listen` 的界面也能拿到当前状态（否则它会卡在第一帧）。
    if let Ok(value) = serde_json::to_value(MigrateProgressPayload::from(&progress(
        report_status_stage(&report),
        0,
        0,
        0,
        0,
        report.error.clone(),
    ))) {
        *LAST_MIGRATION_PROGRESS.lock().unwrap() = Some(value);
    }

    match report.status.as_str() {
        "migrated" => crate::info!(
            ">>> [MIGRATION] 手动迁移完成：源 {:?} 已复制 {} 项 / {} 字节到 {:?}；源目录未被改动，可重复验证。",
            report.source,
            report.files,
            report.bytes,
            report.target
        ),
        "deferred" => crate::info!(
            ">>> [MIGRATION] 数据已复制就绪，等待下次启动接管：源 {:?}（标记已写入）。",
            report.source
        ),
        "skipped" => crate::info!(
            ">>> [MIGRATION] 手动迁移跳过（源与目标均未被改动）：源={:?} 原因码={:?}",
            report.source,
            report.skip_reason
        ),
        _ => {}
    }

    Ok(report)
}

/// 报告状态 → 进度事件里对应的阶段名（快照用）。
fn report_status_stage(report: &crate::app::IdentifierMigrationReport) -> &'static str {
    match report.status.as_str() {
        "deferred" => "deferred",
        "failed" => "failed",
        _ => "done",
    }
}

#[tauri::command]
pub fn open_file_with_default_app(file_path: String) -> AppResult<()> {
    use std::process::Command;
    Command::new("explorer")
        .arg(&file_path)
        .spawn()
        .map_err(|e| AppError::Internal(format!("Failed to open file: {}", e)))?;
    Ok(())
}

#[tauri::command]
pub fn open_file_location(file_path: String) -> AppResult<()> {
    use std::process::Command;
    Command::new("explorer")
        .arg("/select,")
        .arg(&file_path)
        .spawn()
        .map_err(|e| AppError::Internal(format!("Failed to open file location: {}", e)))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 开机自启动：写后回读（消灭"界面显示已开、实际没生效"的静默失败）
// ---------------------------------------------------------------------------

/// `HKCU\...\Run` 下本应用使用的键名。
///
/// - `AUTOSTART_VALUE_NAME` 是**当前版本写入**的名字，也是唯一被认作"已开启"的名字；
/// - 另外两个是**旧版遗留名**（改名前的 `TieZ` / `tie-z`）。它们只在关闭时被顺手清理，
///   **不参与"是否已开启"的判定**——判定要认值内容，见 [`autostart_state_from`]。
const AUTOSTART_VALUE_NAME: &str = "Tiez-Next";
const AUTOSTART_LEGACY_NAMES: [&str; 2] = ["TieZ", "tie-z"];

/// 读回的注册表自启动状态：界面据此显示"真的生效了吗"，而不是乐观置位。
///
/// 【为什么要把原始值也回传】"设置成功"的唯一可信证据是注册表里那串命令本身。
/// 只回一个 `bool` 时，用户看到的仍然是一个开关——那正是缺陷的形态：开关亮了，
/// 但没人能证明系统真的会在开机时拉起这个路径。回传 `registeredCommand` 后，
/// 界面可以把**读回来的原文**显示给用户，或至少给出"指向哪里"的说明。

/// `is_autostart_enabled` 的返回形状（camelCase，与前端 `AutostartState` 对应）。
///
/// 【为什么不让前端自己判断"指向是否正确"】判据只允许存在一处。前端若复制一份
/// "剥引号 + 比路径"的逻辑，两处迟早分叉（比如后端支持了正反斜杠混用而前端没跟上），
/// 于是同一台机器上开关与实际状态出现第二种矛盾。前端只负责**展示**回读结果。
#[derive(Debug, serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AutostartState {
    pub enabled: bool,
    pub registered_command: Option<String>,
    pub current_exe: String,
    pub stale_names: Vec<String>,
    pub readable: bool,
}

/// 判定一个注册表自启动值是否**指向当前这个 exe**。
///
/// 【为什么不能只看值是否存在】改名/换安装位置/便携版搬目录之后，注册表里会留下
/// 指向**已失效旧路径**的值。只要它在，老判据就报"已开启"，而系统开机时拉起的是一个
/// 不存在的文件——用户看到开关是亮的，实际什么都没发生。
///
/// 匹配方式：把命令串里的引号剥掉，取其中的路径部分与当前 exe 做**大小写无关**比较
/// （Windows 路径大小写不敏感）。参数（如 `--minimized`）不参与比较。
fn command_targets_current_exe(command: &str, current_exe: &str) -> bool {
    let trimmed = command.trim();
    if trimmed.is_empty() || current_exe.is_empty() {
        return false;
    }
    // 逐段比较优于字符串相等：注册表里可能用 `C:/` 而进程报 `C:\`，也可能正反斜杠混用。
    let norm = |s: &str| s.replace('/', "\\").to_lowercase();

    // 情况 1：整串就是那个路径（没有参数、也没有引号）。含空格的路径常被这样写，
    // 直接整体比一次，避免被下面的"按空白切第一段"切坏。
    if norm(trimmed) == norm(current_exe) {
        return true;
    }

    // 情况 2：`"路径" 参数...` —— 引号内的才是路径（含空格路径的唯一可靠写法）。
    let path_part = if let Some(rest) = trimmed.strip_prefix('"') {
        match rest.find('"') {
            Some(end) => &rest[..end],
            None => rest,
        }
    } else {
        // 情况 3：`路径 参数...`（无引号）。此时路径本身不能含空格，
        // 否则无法与参数区分——这种写法本身就是无效的，不予猜测。
        trimmed.split_whitespace().next().unwrap_or("")
    };

    if path_part.is_empty() {
        return false;
    }
    norm(path_part) == norm(current_exe)
}

/// 由"注册表读到的原始值"判定自启动状态（**纯函数**，可在任何平台上单测）。
///
/// 输入是 `(值名, 值内容)` 列表与当前 exe 路径，输出是判定结果。把判定从注册表读取里
/// 拆出来，是为了让"旧名残留不该算已开启""指向旧路径不该算已开启"这两条关键判据
/// 能在没有 Windows 注册表的机器上被真正断言——否则它们只能靠真机人工验证。
pub fn autostart_state_from(entries: &[(String, String)], current_exe: &str) -> AutostartState {
    let find = |name: &str| {
        entries
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    };

    // 已开启 = **当前键名存在** 且 **值指向当前 exe**。旧名不参与这个判定。
    let own = find(AUTOSTART_VALUE_NAME);
    let enabled = own
        .as_deref()
        .map(|v| command_targets_current_exe(v, current_exe))
        .unwrap_or(false);

    let stale_names = AUTOSTART_LEGACY_NAMES
        .iter()
        .filter(|n| find(n).is_some())
        .map(|n| n.to_string())
        .collect();

    AutostartState {
        enabled,
        // 只有**判定为生效**时才把命令原文当作"生效证据"回传；否则回传它也没意义，
        // 反而会让界面把一条失效的旧命令当成成功证据显示出来。
        registered_command: if enabled { own } else { None },
        current_exe: current_exe.to_string(),
        stale_names,
        readable: true,
    }
}

/// 读取 `HKCU\...\Run` 下与自启动相关（本应用名与旧版遗留名）的全部值。
#[cfg(target_os = "windows")]
fn read_autostart_entries() -> Result<Vec<(String, String)>, String> {
    use winreg::enums::*;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu
        .open_subkey_with_flags(
            "Software\\Microsoft\\Windows\\CurrentVersion\\Run",
            KEY_READ | KEY_WRITE,
        )
        .map_err(|e| e.to_string())?;

    let mut out = Vec::new();
    let mut names = vec![AUTOSTART_VALUE_NAME.to_string()];
    names.extend(AUTOSTART_LEGACY_NAMES.iter().map(|s| s.to_string()));
    for name in names {
        if let Ok(value) = key.get_value::<String, _>(&name) {
            out.push((name, value));
        }
    }
    Ok(out)
}

#[cfg(not(target_os = "windows"))]
fn read_autostart_entries() -> Result<Vec<(String, String)>, String> {
    // 非 Windows 目标没有这个注册表位置。返回空表（= 未开启）而不是报错：
    // 报错会让界面把"这个平台没有开机自启动"显示成一次故障。
    Ok(Vec::new())
}

/// 当前进程 exe 路径（判定基准）。取不到时回空串，判定会安全地落到"未开启"。
fn current_exe_string() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// 读一次自启动状态（含回读证据）。
pub fn read_autostart_state() -> AutostartState {
    let exe = current_exe_string();
    match read_autostart_entries() {
        Ok(entries) => autostart_state_from(&entries, &exe),
        Err(e) => {
            crate::error!("[AUTOSTART] 读取 Run 键失败：{}", e);
            AutostartState {
                enabled: false,
                registered_command: None,
                current_exe: exe,
                stale_names: Vec::new(),
                readable: false,
            }
        }
    }
}

/// 开关开机自启动，并**在写入之后立即回读注册表**确认真的生效。
///
/// ## 为什么必须回读（这条命令的存在理由）
///
/// 旧实现 `key.set_value(...)` 成功即 `Ok(())`，前端拿到成功就把开关点亮。但
/// "写 API 没报错"与"系统真的会在开机时拉起这个路径"是两件事：值可能被组策略、
/// 安全软件或权限问题挡住，也可能被写成了一个**指向已失效旧路径**的内容。用户看到的
/// 是一个亮着的开关，实际什么都没发生——这正是用户反馈的"纯应用里面显示设置了
/// 不一定生效"。
///
/// 因此本命令的返回值是**回读后的真实状态**（[`AutostartState`]）：
/// - 期望开启但回读不通过 → 返回 `Err`，并把回读到的内容写进错误里；
/// - 期望关闭但回读仍为开启 → 同样返回 `Err`（不能谎报关闭成功）。
///
/// 判据与 `hotkey_cmd::test_hotkey_available`（注册→读回→注销）同源：
/// **一步不省，成功后立刻验证**。
#[tauri::command]
pub fn toggle_autostart(enabled: bool) -> AppResult<AutostartState> {
    #[cfg(target_os = "windows")]
    {
        use winreg::enums::*;
        use winreg::RegKey;

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let key = hkcu
            .open_subkey_with_flags(
                "Software\\Microsoft\\Windows\\CurrentVersion\\Run",
                KEY_WRITE | KEY_READ,
            )
            .map_err(|e| AppError::Internal(e.to_string()))?;
        let app_path = std::env::current_exe()
            .map_err(|e| AppError::Internal(e.to_string()))?
            .to_string_lossy()
            .to_string();
        let cmd = format!("\"{}\" --minimized", app_path);

        if enabled {
            key.set_value(AUTOSTART_VALUE_NAME, &cmd)
                .map_err(|e| AppError::Internal(e.to_string()))?;
        } else {
            let _ = key.delete_value(AUTOSTART_VALUE_NAME);
            // 关闭时顺手清掉旧版遗留值：它们会让系统在开机时多拉一个已失效的旧路径。
            // 清理失败**不算失败**（那不是本次操作的判据），但会被回读如实报出。
            for legacy in AUTOSTART_LEGACY_NAMES {
                let _ = key.delete_value(legacy);
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        // 非 Windows 平台永远无法真正注册开机自启动：如实返回失败，
        // 而不是让界面把开关点亮成一个假的"已开启"。
        return Err(AppError::Internal(
            "当前平台不支持开机自启动设置".to_string(),
        ));
    }

    // ── 写后回读：唯一能证明"真的生效"的步骤 ──
    let state = read_autostart_state();
    if !state.readable {
        return Err(AppError::Internal(
            "自启动设置已写入，但回读注册表失败，无法确认是否生效".to_string(),
        ));
    }

    if enabled {
        if !state.enabled {
            // 回读到的内容要如实带回：用户据此能判断是"没写进去"还是"写进去了但指向旧路径"。
            let actual = read_autostart_entries()
                .ok()
                .and_then(|entries| {
                    entries
                        .into_iter()
                        .find(|(n, _)| n == AUTOSTART_VALUE_NAME)
                        .map(|(_, v)| v)
                })
                .unwrap_or_else(|| "（注册表里没有该值）".to_string());
            return Err(AppError::Internal(format!(
                "自启动写入后回读未通过：期望指向 {}，实际 {}",
                state.current_exe, actual
            )));
        }
    } else if state.enabled {
        return Err(AppError::Internal(
            "自启动关闭后回读仍显示已开启".to_string(),
        ));
    }

    crate::info!(
        "[AUTOSTART] 设置 enabled={} 已回读确认；注册表命令={:?}；旧版残留值={:?}",
        enabled,
        state.registered_command,
        state.stale_names
    );

    Ok(state)
}

/// 查询当前自启动状态（含回读证据，供界面显示"真的生效了"的凭据）。
#[tauri::command]
pub fn is_autostart_enabled() -> AppResult<AutostartState> {
    Ok(read_autostart_state())
}

/// 兼容旧调用点：只要布尔值的自启动查询。
///
/// 保留它是为了让 `useAppBootstrap` 这类只关心一个开关的地方不必解析结构体；
/// 但**界面显示"是否真的生效"必须用 `is_autostart_enabled`**，因为只有它能给出
/// 注册表原文与旧版残留值。
pub fn autostart_enabled_bool() -> bool {
    read_autostart_state().enabled
}

#[tauri::command]
pub fn set_windows_clipboard_history(enabled: bool) -> AppResult<()> {
    use winreg::enums::*;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let mut needs_restart = false;

    if let Ok((key, _)) = hkcu.create_subkey("Software\\Microsoft\\Clipboard") {
        let value: u32 = if enabled { 1 } else { 0 };
        let _ = key.set_value("EnableClipboardHistory", &value);
        let _ = key.set_value("EnableCloudClipboard", &value);
    }

    if let Ok((adv_key, _)) =
        hkcu.create_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced")
    {
        let current_disabled: String = adv_key.get_value("DisabledHotkeys").unwrap_or_default();
        if current_disabled.to_uppercase().contains('V') {
            let new_val = current_disabled.to_uppercase().replace('V', "");
            if new_val.is_empty() {
                let _ = adv_key.delete_value("DisabledHotkeys");
            } else {
                let _ = adv_key.set_value("DisabledHotkeys", &new_val);
            }
            needs_restart = true;
        }
    }

    if let Ok((policy_key, _)) =
        hkcu.create_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Policies\\Explorer")
    {
        if policy_key
            .get_value::<u32, _>("DisallowClipboardHistory")
            .unwrap_or(0)
            != 0
        {
            let _ = policy_key.delete_value("DisallowClipboardHistory");
            needs_restart = true;
        }
    }

    // Policy-based clipboard lock can also exist under Software\Policies\Microsoft\Windows\System.
    // Clear blocking values when restoring system Win+V behavior.
    if enabled {
        if let Ok((sys_policy, _)) =
            hkcu.create_subkey("Software\\Policies\\Microsoft\\Windows\\System")
        {
            if sys_policy
                .get_value::<u32, _>("AllowClipboardHistory")
                .unwrap_or(1)
                == 0
            {
                let _ = sys_policy.delete_value("AllowClipboardHistory");
                needs_restart = true;
            }
            if sys_policy
                .get_value::<u32, _>("AllowCrossDeviceClipboard")
                .unwrap_or(1)
                == 0
            {
                let _ = sys_policy.delete_value("AllowCrossDeviceClipboard");
                needs_restart = true;
            }
        }
    }

    if needs_restart {
        restart_explorer().ok();
    }
    Ok(())
}

#[tauri::command]
pub fn get_windows_clipboard_history() -> AppResult<bool> {
    use winreg::enums::*;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);

    let v_disabled = match hkcu
        .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced")
    {
        Ok(key) => key
            .get_value::<String, _>("DisabledHotkeys")
            .unwrap_or_default()
            .to_uppercase()
            .contains('V'),
        Err(_) => false,
    };
    let history_enabled = match hkcu.open_subkey("Software\\Microsoft\\Clipboard") {
        Ok(key) => {
            key.get_value::<u32, _>("EnableClipboardHistory")
                .unwrap_or(1)
                != 0
        }
        Err(_) => true,
    };
    Ok(history_enabled && !v_disabled)
}

#[tauri::command]
pub fn set_win_clipboard_disabled(_disabled: bool) -> AppResult<()> {
    set_windows_clipboard_history(!_disabled)
}

#[tauri::command]
pub fn trigger_registry_win_v_optimization(enable: bool) -> AppResult<bool> {
    use winreg::enums::*;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let mut changed = false;

    if let Ok((adv_key, _)) =
        hkcu.create_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced")
    {
        let current: String = adv_key.get_value("DisabledHotkeys").unwrap_or_default();
        if enable && !current.to_uppercase().contains('V') {
            let _ = adv_key.set_value("DisabledHotkeys", &format!("{}V", current));
            changed = true;
        } else if !enable && current.to_uppercase().contains('V') {
            let clean = current.to_uppercase().replace('V', "");
            if clean.is_empty() {
                let _ = adv_key.delete_value("DisabledHotkeys");
            } else {
                let _ = adv_key.set_value("DisabledHotkeys", &clean);
            }
            changed = true;
        }
    }

    if let Ok((cb_key, _)) = hkcu.create_subkey("Software\\Microsoft\\Clipboard") {
        let val: u32 = if enable { 0 } else { 1 };
        let prev_history = cb_key.get_value::<u32, _>("EnableClipboardHistory").ok();
        let prev_cloud = cb_key.get_value::<u32, _>("EnableCloudClipboard").ok();
        let _ = cb_key.set_value("EnableClipboardHistory", &val);
        let _ = cb_key.set_value("EnableCloudClipboard", &val);
        if prev_history != Some(val) || prev_cloud != Some(val) {
            changed = true;
        }
    }

    // When disabling Win+V takeover, also clear policy-level lock that can keep Win+V unavailable
    // until a full reboot on some systems.
    if !enable {
        if let Ok((policy_key, _)) =
            hkcu.create_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Policies\\Explorer")
        {
            if policy_key
                .get_value::<u32, _>("DisallowClipboardHistory")
                .unwrap_or(0)
                != 0
            {
                let _ = policy_key.delete_value("DisallowClipboardHistory");
                changed = true;
            }
        }

        if let Ok((sys_policy, _)) =
            hkcu.create_subkey("Software\\Policies\\Microsoft\\Windows\\System")
        {
            if sys_policy
                .get_value::<u32, _>("AllowClipboardHistory")
                .unwrap_or(1)
                == 0
            {
                let _ = sys_policy.delete_value("AllowClipboardHistory");
                changed = true;
            }
            if sys_policy
                .get_value::<u32, _>("AllowCrossDeviceClipboard")
                .unwrap_or(1)
                == 0
            {
                let _ = sys_policy.delete_value("AllowCrossDeviceClipboard");
                changed = true;
            }
        }
    }
    Ok(changed)
}

#[tauri::command]
pub fn is_registry_win_v_optimized() -> AppResult<bool> {
    Ok(get_registry_win_v_optimized_status())
}

pub fn get_registry_win_v_optimized_status() -> bool {
    use winreg::enums::*;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok(key) =
        hkcu.open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced")
    {
        return key
            .get_value::<String, _>("DisabledHotkeys")
            .unwrap_or_default()
            .to_uppercase()
            .contains('V');
    }
    false
}

/// 重启 Windows 资源管理器，让 `DisabledHotkeys` 这类注册表改动真正生效。
///
/// ## 为什么不再吞掉错误
///
/// `DisabledHotkeys`（Win+V 接管用的就是它）**只在 explorer 启动时读一次**。改了注册表
/// 而不重启 explorer，等于什么都没发生——而界面文案却写着"会重启资源管理器"。
/// 旧实现 `let _ = ...spawn(); Ok(())` 把失败也变成成功：用户点完开关看到"已开启"，
/// 实际系统仍然占用着 Win+V，且没有任何提示。
///
/// 现在：spawn 失败如实返回错误。**但要说清"重启失败不等于设置失败"**——
/// 注册表已经写好了，只是要等用户下次登录（explorer 自然会重启）才生效，
/// 所以这里是可降级的告知，不是致命错误（错误文案由调用方按此措辞）。
#[tauri::command]
pub fn restart_explorer() -> AppResult<()> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    #[cfg(target_os = "windows")]
    {
        Command::new("cmd")
            .args(["/C", "taskkill /F /IM explorer.exe & start explorer.exe"])
            .creation_flags(0x08000000)
            .spawn()
            .map_err(|e| {
                AppError::Internal(format!(
                    "重启资源管理器失败：{}。注册表改动已写入，下次登录后会自动生效。",
                    e
                ))
            })?;
        return Ok(());
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err(AppError::Internal(
            "当前平台没有资源管理器".to_string(),
        ))
    }
}

#[tauri::command]
pub fn quit(app: AppHandle) {
    app.exit(0);
}

#[tauri::command]
pub fn relaunch(app: AppHandle) {
    use std::process::Command;
    if let Ok(exe) = std::env::current_exe() {
        let _ = Command::new(exe).spawn();
    }
    app.exit(0);
}

#[tauri::command]
pub fn restart_as_admin(app_handle: AppHandle) -> AppResult<()> {
    use std::env;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    // Get current executable path
    let exe_path = env::current_exe().map_err(AppError::from)?;

    // Convert to wide string
    let exe_wide: Vec<u16> = OsStr::new(&exe_path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // "runas" verb for elevation
    let runas: Vec<u16> = OsStr::new("runas")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let result = ShellExecuteW(
            None,
            PCWSTR::from_raw(runas.as_ptr()),
            PCWSTR::from_raw(exe_wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );

        // ShellExecuteW returns > 32 on success
        if result.0 as usize <= 32 {
            return Err(AppError::Internal(
                "Failed to restart as administrator. User may have cancelled UAC prompt."
                    .to_string(),
            ));
        }
    }

    // Close current instance
    app_handle.exit(0);

    Ok(())
}

#[tauri::command]
pub fn check_is_admin() -> bool {
    use std::ffi::c_void;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token_handle = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token_handle).is_ok() {
            let mut elevation = TOKEN_ELEVATION::default();
            let mut return_length = 0;
            let success = GetTokenInformation(
                token_handle,
                TokenElevation,
                Some(&mut elevation as *mut _ as *mut c_void),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut return_length,
            );

            let _ = windows::Win32::Foundation::CloseHandle(token_handle);

            if success.is_ok() {
                return elevation.TokenIsElevated != 0;
            }
        }
    }
    false
}

#[tauri::command]
pub fn set_data_path(app_handle: AppHandle, new_path: String) -> AppResult<()> {
    let clean_path = new_path.trim().to_string();
    let new_data_path = std::path::Path::new(&clean_path);
    if !new_data_path.exists() {
        return Err(AppError::Validation("Directory does not exist".to_string()));
    }

    let old_path_buf = app_handle.state::<AppDataDir>().0.lock().unwrap().clone();

    // 1. Migrate data folders if they exist in the OLD path
    {
        for folder in ["attachments", "emoji_favorites"] {
            let old_folder = old_path_buf.join(folder);
            let new_folder = new_data_path.join(folder);

            if old_folder.exists() && old_folder.is_dir() {
                if let Err(_) = std::fs::rename(&old_folder, &new_folder) {
                    if let Err(copy_err) = copy_dir_recursive(&old_folder, &new_folder) {
                        return Err(AppError::Internal(format!(
                            "Failed to copy {}: {}",
                            folder, copy_err
                        )));
                    } else {
                        let _ = std::fs::remove_dir_all(&old_folder);
                    }
                }
            }
        }

        // 1.2 Migrate database files (main + WAL/SHM)
        let db_files = ["clipboard.db", "clipboard.db-wal", "clipboard.db-shm"];
        for name in db_files {
            let old_db = old_path_buf.join(name);
            if !old_db.exists() {
                continue;
            }
            let new_db = new_data_path.join(name);
            if new_db.exists() {
                // Avoid overwriting any existing DB in new path
                let backup = new_data_path.join(format!("{}.backup", name));
                if backup.exists() {
                    let _ = std::fs::remove_file(&backup);
                }
                let _ = std::fs::rename(&new_db, &backup);
            }
            if let Err(_) = std::fs::rename(&old_db, &new_db) {
                if let Err(copy_err) = std::fs::copy(&old_db, &new_db) {
                    return Err(AppError::Internal(format!(
                        "Failed to copy {}: {}",
                        name, copy_err
                    )));
                } else {
                    let _ = std::fs::remove_file(&old_db);
                }
            }
        }
    }

    // 1.3 Rewrite internal attachment paths inside DB (if DB exists in new path)
    let new_db_path = new_data_path.join("clipboard.db");
    if new_db_path.exists() {
        rewrite_attachment_paths_in_db(&new_db_path, &old_path_buf, new_data_path)?;
        rewrite_emoji_favorites_in_db(&new_db_path, &old_path_buf, new_data_path)?;
        rewrite_custom_background_in_db(&new_db_path, &old_path_buf, new_data_path)?;
    }

    // 2. Save new path to a persistent config file
    let config_dir = app_handle.path().app_data_dir().map_err(AppError::from)?;
    if !config_dir.exists() {
        std::fs::create_dir_all(&config_dir).map_err(AppError::from)?;
    }

    let redirect_file = config_dir.join("datapath.txt");
    std::fs::write(&redirect_file, &clean_path).map_err(AppError::from)?;

    Ok(())
}

fn rewrite_attachment_paths_in_db(
    db_path: &std::path::Path,
    old_base: &std::path::Path,
    new_base: &std::path::Path,
) -> AppResult<()> {
    let old_attach = old_base.join("attachments");
    let new_attach = new_base.join("attachments");
    let old_prefix = old_attach.to_string_lossy().to_string();
    let new_prefix = new_attach.to_string_lossy().to_string();
    if old_prefix == new_prefix {
        return Ok(());
    }

    let old_prefix_slash = old_prefix.replace('\\', "/");
    let new_prefix_slash = new_prefix.replace('\\', "/");

    let conn = Connection::open(db_path).map_err(AppError::from)?;

    let mut stmt = conn
        .prepare("SELECT id, content, html_content FROM clipboard_history WHERE is_external = 1 OR html_content IS NOT NULL")
        .map_err(AppError::from)?;

    let rows = stmt
        .query_map([], |row| {
            let id: i64 = row.get(0)?;
            let content: String = row.get(1)?;
            let html_content: Option<String> = row.get(2)?;
            Ok((id, content, html_content))
        })
        .map_err(AppError::from)?;

    for row in rows {
        let (id, content_raw, html_raw) = row.map_err(AppError::from)?;
        let mut content_new: Option<String> = None;
        let mut html_new: Option<String> = None;

        if let Some(updated) = rewrite_content_path(
            &content_raw,
            &old_prefix,
            &new_prefix,
            &old_prefix_slash,
            &new_prefix_slash,
        ) {
            content_new = Some(updated);
        }

        if let Some(html) = html_raw.as_ref() {
            if let Some(updated) = rewrite_html_paths(
                html,
                &old_prefix,
                &new_prefix,
                &old_prefix_slash,
                &new_prefix_slash,
            ) {
                html_new = Some(updated);
            }
        }

        if content_new.is_some() || html_new.is_some() {
            let content_final = content_new.as_ref().unwrap_or(&content_raw);
            let html_final = match html_new.as_ref() {
                Some(v) => Some(v.as_str()),
                None => html_raw.as_deref(),
            };
            conn.execute(
                "UPDATE clipboard_history SET content = ?1, html_content = ?2 WHERE id = ?3",
                params![content_final, html_final, id],
            )
            .map_err(AppError::from)?;
        }
    }

    Ok(())
}

fn rewrite_emoji_favorites_in_db(
    db_path: &std::path::Path,
    old_base: &std::path::Path,
    new_base: &std::path::Path,
) -> AppResult<()> {
    let old_dir = old_base.join("emoji_favorites");
    let new_dir = new_base.join("emoji_favorites");
    let old_prefix = old_dir.to_string_lossy().to_string();
    let new_prefix = new_dir.to_string_lossy().to_string();
    if old_prefix == new_prefix {
        return Ok(());
    }

    let old_prefix_slash = old_prefix.replace('\\', "/");
    let new_prefix_slash = new_prefix.replace('\\', "/");

    let conn = Connection::open(db_path).map_err(AppError::from)?;
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'app.emoji_favorites'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(AppError::from)?;

    let Some(raw) = value else {
        return Ok(());
    };
    let parsed: Vec<String> = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };

    let mut changed = false;
    let mut updated: Vec<String> = Vec::with_capacity(parsed.len());
    for path in parsed {
        let mut next = path.clone();
        if next.starts_with(&old_prefix) {
            next = format!("{}{}", new_prefix, &next[old_prefix.len()..]);
        } else if next.starts_with(&old_prefix_slash) {
            next = format!("{}{}", new_prefix_slash, &next[old_prefix_slash.len()..]);
        }
        if next != path {
            changed = true;
        }
        updated.push(next);
    }

    if changed {
        let serialized = serde_json::to_string(&updated).unwrap_or(raw);
        conn.execute(
            "UPDATE settings SET value = ?1 WHERE key = 'app.emoji_favorites'",
            params![serialized],
        )
        .map_err(AppError::from)?;
    }

    Ok(())
}

fn rewrite_custom_background_in_db(
    db_path: &std::path::Path,
    old_base: &std::path::Path,
    new_base: &std::path::Path,
) -> AppResult<()> {
    rewrite_custom_background_in_db_impl(db_path, old_base, new_base, true)
}

/// 供标识符迁移使用的**只读源**版本：只改写数据库里的路径字符串，
/// **绝不移动或删除源目录中的文件**。
///
/// 迁移的安全契约要求源目录全程只读（见 `crate::migration_identifier`）。用户主动
/// 切换数据目录时移动源文件是合理的（那时是显式操作），但启动期自动迁移不应改动
/// 旧目录——否则"迁移失败也不损失原数据"的保证就不成立了。
pub fn rewrite_data_paths_in_db(
    db_path: &std::path::Path,
    old_base: &std::path::Path,
    new_base: &std::path::Path,
) -> AppResult<()> {
    rewrite_attachment_paths_in_db(db_path, old_base, new_base)?;
    rewrite_emoji_favorites_in_db(db_path, old_base, new_base)?;
    rewrite_custom_background_in_db_impl(db_path, old_base, new_base, false)?;
    Ok(())
}

fn rewrite_custom_background_in_db_impl(
    db_path: &std::path::Path,
    old_base: &std::path::Path,
    new_base: &std::path::Path,
    move_source_file: bool,
) -> AppResult<()> {
    let conn = Connection::open(db_path).map_err(AppError::from)?;
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'app.custom_background'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(AppError::from)?;

    let Some(raw_path) = value else {
        return Ok(());
    };
    let trimmed = raw_path.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    let old_path = std::path::PathBuf::from(trimmed);
    if !old_path.starts_with(old_base) {
        return Ok(());
    }

    let Ok(relative) = old_path.strip_prefix(old_base) else {
        return Ok(());
    };
    let new_path = new_base.join(relative);

    // 只在"用户主动切换数据目录"的场景下移动源文件；自动迁移时跳过，
    // 文件已由迁移逻辑复制到新目录，这里仅改写引用。
    if move_source_file && old_path != new_path && old_path.exists() {
        if let Some(parent) = new_path.parent() {
            std::fs::create_dir_all(parent).map_err(AppError::from)?;
        }
        if !new_path.exists() {
            if let Err(_) = std::fs::rename(&old_path, &new_path) {
                std::fs::copy(&old_path, &new_path).map_err(AppError::from)?;
                let _ = std::fs::remove_file(&old_path);
            }
        }
    }

    let new_value = new_path.to_string_lossy().to_string();
    if new_value != raw_path {
        conn.execute(
            "UPDATE settings SET value = ?1 WHERE key = 'app.custom_background'",
            params![new_value],
        )
        .map_err(AppError::from)?;
    }

    Ok(())
}

fn rewrite_content_path(
    value: &str,
    old_prefix: &str,
    new_prefix: &str,
    old_prefix_slash: &str,
    new_prefix_slash: &str,
) -> Option<String> {
    let replace_prefix = |v: &str| -> Option<String> {
        if v.starts_with(old_prefix) {
            return Some(format!("{}{}", new_prefix, &v[old_prefix.len()..]));
        }
        if v.starts_with(old_prefix_slash) {
            return Some(format!(
                "{}{}",
                new_prefix_slash,
                &v[old_prefix_slash.len()..]
            ));
        }
        None
    };

    if value.starts_with(ENCRYPT_PREFIX) {
        #[cfg(not(feature = "portable"))]
        {
            let plain = crate::database::encryption::decrypt_value(value)
                .unwrap_or_else(|| value.to_string());
            if let Some(updated_plain) = replace_prefix(&plain) {
                let encrypted = crate::database::encryption::encrypt_value(&updated_plain)
                    .unwrap_or(updated_plain);
                return Some(encrypted);
            }
        }
        return None;
    }

    replace_prefix(value)
}

fn rewrite_html_paths(
    value: &str,
    old_prefix: &str,
    new_prefix: &str,
    old_prefix_slash: &str,
    new_prefix_slash: &str,
) -> Option<String> {
    let replace_any = |v: &str| -> Option<String> {
        let mut updated = v.replace(old_prefix, new_prefix);
        updated = updated.replace(old_prefix_slash, new_prefix_slash);
        if updated == v {
            None
        } else {
            Some(updated)
        }
    };

    if value.starts_with(ENCRYPT_PREFIX) {
        #[cfg(not(feature = "portable"))]
        {
            let plain = crate::database::encryption::decrypt_value(value)
                .unwrap_or_else(|| value.to_string());
            if let Some(updated_plain) = replace_any(&plain) {
                let encrypted = crate::database::encryption::encrypt_value(&updated_plain)
                    .unwrap_or(updated_plain);
                return Some(encrypted);
            }
        }
        return None;
    }

    replace_any(value)
}

fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    if !dst.exists() {
        std::fs::create_dir_all(dst)?;
    }
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 备份导出 / 导入恢复
// ---------------------------------------------------------------------------

/// 把内部错误转成前端可读的 [`AppError`]。
///
/// 错误信息序列化成 `{"code":"...","detail":"..."}` 的 JSON 串：`code` 是稳定的
/// 机器可读原因码，前端据此映射成当前语言的人话（与项目里既有的
/// `legacy_migrate_notice_<code>` 约定一致）；`detail` 是后端原文，用于排障与
/// 在界面折叠展示。这样"拒绝原版"这类提示不会出现半英文，也不会因为后端只写中文
/// 而让英/繁用户看不懂。
fn backup_err(e: crate::services::backup::BackupError) -> AppError {
    let payload = serde_json::json!({
        "code": e.code(),
        "detail": e.to_string(),
    });
    // 必须用 `Raw`：`Validation` 的 Display 会加 "验证错误: " 前缀，前端拿到
    // `验证错误: {"code":...}` 后 JSON.parse 失败，只能把整串（含中文前缀）显示给
    // 用户 —— 三语词条全部失效，且英文/繁用户看到中文。`Raw` 保证 Display 就是裸 JSON。
    AppError::Raw(payload.to_string())
}

/// 对"可能尚不存在"的输出路径做规范化，用于安全检查。
///
/// `canonicalize` 要求路径存在，因此这里规范化**父目录**再把文件名拼回去。
/// 连父目录都取不到时返回 `Err`，调用方按"无法判定"处理（不拦截，避免误伤）。
fn canonicalize_for_guard(p: &std::path::Path) -> Result<std::path::PathBuf, std::io::Error> {
    if let Ok(c) = p.canonicalize() {
        return Ok(c);
    }
    let parent = p.parent().unwrap_or_else(|| std::path::Path::new("."));
    let file = p.file_name().unwrap_or_default();
    Ok(parent.canonicalize()?.join(file))
}

/// 导出备份包的前置检查结果。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupPreflight {
    /// 当前数据目录。
    pub data_dir: String,
    /// 受管数据的总字节数（数据库 + 附件 + 表情收藏 + 背景）。
    pub managed_bytes: u64,
    /// 受管文件总数。
    pub managed_files: u64,
    /// 是否需要用户为背景图单独做决定（设置指向数据目录之外）。
    pub background_outside: bool,
    /// 该背景图的绝对路径（若有）。
    pub background_path: Option<String>,
}

/// 导出前的只读清点：让用户在点"导出"之前看到**将要打包多少东西**。
///
/// 尤其重要的是把"自定义背景图在数据目录之外"这件事提前告知——不然用户会以为
/// 背景图没进包，或者以为进了包却发现导入后没背景。
#[tauri::command]
pub fn backup_preflight(state: State<'_, AppDataDir>) -> AppResult<BackupPreflight> {
    let data_dir = state.0.lock().unwrap().clone();
    let db_path = data_dir.join("clipboard.db");

    // 【为什么用"复制成探针再读"而不是直接 `Connection::open`】这是一个**声称只读**的
    // 清点命令，但 SQLite 即便以只读方式打开一个带 WAL 侧车的库，也会**改写 `-shm`**
    // （128B -> 32768B，本项目 migration_identifier 的回归测试抓到过同一现象）。
    // 更糟的是会与正在运行的应用持有的连接互相干扰。
    // 因此把库连同 `-wal`/`-shm` 复制到临时目录再读，读完删掉——目标目录全程不被写。
    let background_path = read_background_setting_via_probe(&db_path);

    let background_outside = background_path
        .as_ref()
        .map(|p| !std::path::Path::new(p).starts_with(&data_dir))
        .unwrap_or(false);

    let (managed_files, managed_bytes) = count_managed(&data_dir);

    Ok(BackupPreflight {
        data_dir: data_dir.to_string_lossy().to_string(),
        managed_bytes,
        managed_files,
        background_outside,
        background_path: if background_outside { background_path } else { None },
    })
}

/// 通过"文件层复制成探针"的方式只读一个设置项，**不触碰**原库（含其 `-wal`/`-shm`）。
///
/// 复制失败或解析失败一律返回 `None`（按"没有设置"处理）——这是展示性的清点信息，
/// 不值得为它冒险去写用户的库。
fn read_background_setting_via_probe(db_path: &std::path::Path) -> Option<String> {
    if !db_path.is_file() {
        return None;
    }
    let dir = db_path.parent()?;
    let stamp = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let probe_dir = std::env::temp_dir().join(format!("tiez-preflight-probe-{}", stamp));
    if std::fs::create_dir_all(&probe_dir).is_err() {
        return None;
    }
    let db_name = db_path.file_name()?.to_string_lossy().to_string();
    for suffix in ["", "-wal", "-shm"] {
        let from = dir.join(format!("{}{}", db_name, suffix));
        if from.is_file() {
            let _ = std::fs::copy(&from, probe_dir.join(format!("{}{}", db_name, suffix)));
        }
    }
    let probe_db = probe_dir.join(&db_name);
    let value = Connection::open(&probe_db)
        .ok()
        .and_then(|conn| {
            conn.query_row(
                "SELECT value FROM settings WHERE key = 'app.custom_background'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
        })
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let _ = std::fs::remove_dir_all(&probe_dir);
    value
}

/// 统计受管数据的文件数与字节数（只读）。
fn count_managed(data_dir: &std::path::Path) -> (u64, u64) {
    let mut files = 0u64;
    let mut bytes = 0u64;
    // 数据库三件套
    for name in ["clipboard.db", "clipboard.db-wal", "clipboard.db-shm"] {
        let p = data_dir.join(name);
        if let Ok(m) = std::fs::metadata(&p) {
            if m.is_file() {
                files += 1;
                bytes += m.len();
            }
        }
    }
    // 受管目录：必须与 `backup::import::MANAGED_ENTRIES` 的口径一致，
    // 否则界面显示的"当前受管数据"会少算背景图，与包内实际内容对不上。
    for dir_name in ["attachments", "emoji_favorites", "background"] {
        let dir = data_dir.join(dir_name);
        let mut stack = vec![dir];
        while let Some(cur) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&cur) else { continue };
            for entry in rd.flatten() {
                let Ok(ty) = entry.file_type() else { continue };
                if ty.is_dir() {
                    stack.push(entry.path());
                } else if ty.is_file() {
                    files += 1;
                    bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
                }
            }
        }
    }
    (files, bytes)
}

/// 为导出选一个**默认输出路径**并回传（只计算，不写文件）。
///
/// 【为什么不弹系统保存对话框】保存对话框需要额外的 fs/dialog 权限（`dialog:allow-save`），
/// 而本功能要的是"用户确切知道文件落在哪里"。改为由后端挑一个稳妥位置（优先用户
/// 「文档」目录，退回数据目录同级），把完整路径回传界面**明示**给用户，并允许一键
/// 打开所在文件夹。这样既不扩权限，也不牺牲透明度。
#[tauri::command]
pub fn suggest_backup_path(
    state: State<'_, AppDataDir>,
    app_version: String,
) -> AppResult<String> {
    let data_dir = state.0.lock().unwrap().clone();
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let file_name = format!("Tiez-Next-backup-{}-{}.zip", app_version, stamp);

    // 优先「文档」目录；拿不到就退回数据目录同级（保证一定可写）。
    let base = dirs_documents_dir()
        .filter(|p| p.is_dir())
        .or_else(|| data_dir.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| data_dir.clone());

    Ok(base.join(file_name).to_string_lossy().to_string())
}

/// 取用户「文档」目录。不引入新依赖：Windows 上读注册表 shell 文件夹是常见做法，
/// 但这里只需一个"尽力而为"的候选，因此用环境变量 + 约定路径即可。
fn dirs_documents_dir() -> Option<std::path::PathBuf> {
    if let Ok(v) = std::env::var("USERPROFILE") {
        let p = std::path::PathBuf::from(v).join("Documents");
        if p.is_dir() {
            return Some(p);
        }
    }
    if let Ok(v) = std::env::var("HOME") {
        let p = std::path::PathBuf::from(v).join("Documents");
        if p.is_dir() {
            return Some(p);
        }
    }
    None
}

/// 打开某个文件所在的位置（导出成功后让用户立刻找到产物）。
///
/// 是**尽力而为**的操作：失败只记日志、不向用户报错——定位文件失败不该让"导出成功"
/// 这个事实变成一次错误弹窗。
#[tauri::command]
pub fn reveal_path(path: String) -> AppResult<()> {
    let p = std::path::PathBuf::from(path.trim());

    #[cfg(target_os = "windows")]
    {
        // `/select,<完整路径>` 让资源管理器打开父目录并高亮该文件本身。
        let _ = std::process::Command::new("explorer")
            .arg(format!("/select,{}", p.to_string_lossy()))
            .spawn();
    }
    #[cfg(not(target_os = "windows"))]
    {
        // 其他平台没有统一的"定位文件"入口，退化为打开父目录。
        let dir = if p.is_dir() {
            p.clone()
        } else {
            p.parent().map(|v| v.to_path_buf()).unwrap_or(p.clone())
        };
        let _ = dir;
    }

    Ok(())
}

/// 导出备份包。
///
/// `output_path` 由前端用系统保存对话框取得，因此用户确切知道文件会落在哪里。
/// 导出全程只读数据目录（数据库用 `VACUUM INTO` 在线快照）。
#[tauri::command]
pub fn export_backup(
    state: State<'_, AppDataDir>,
    output_path: String,
    app_version: String,
) -> AppResult<crate::services::backup::export::BackupReport> {
    let data_dir = state.0.lock().unwrap().clone();
    let out = std::path::PathBuf::from(output_path.trim());
    if out.as_os_str().is_empty() {
        // 走结构化错误（`Raw` + code）：`Validation` 的 Display 会加中文前缀
        // "验证错误: "，那串前缀对英文/繁体用户既看不懂、又会破坏前端的错误码映射。
        return Err(backup_err(crate::services::backup::BackupError::NoOutputPath));
    }
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            return Err(backup_err(
                crate::services::backup::BackupError::OutputDirMissing(
                    parent.display().to_string(),
                ),
            ));
        }
    }

    // 【拒绝把备份写进数据目录】写目标文件会截断同名文件；若该路径被指向
    // `.../com.tieznext/clipboard.db`，就会**把正在用的数据库截断**——不可逆的用户
    // 数据破坏。备份包本就该是"数据之外的一份副本"，落在数据目录内也不合理。
    if let Ok(canon_out) = canonicalize_for_guard(&out) {
        if let Ok(canon_data) = data_dir.canonicalize() {
            if canon_out.starts_with(&canon_data) {
                return Err(AppError::Validation(format!(
                    "不能把备份导出到数据目录内部（{}）。请选择数据目录以外的位置，例如「文档」或桌面。",
                    data_dir.display()
                )));
            }
        }
    }

    crate::services::backup::create_backup(&crate::services::backup::BackupRequest {
        data_dir,
        output_path: out,
        app_version,
    })
    .map_err(backup_err)
}

/// 只读预览一份备份包：在**二次确认弹窗**里告诉用户这个包是谁导出的、里面有多少数据。
///
/// 这是破坏性操作四层防护里的"让用户看到将发生什么"：拒绝原版的判定也在这里发生，
/// 用户点确认之前就知道包能不能用。
#[tauri::command]
pub fn inspect_backup_package(
    path: String,
) -> AppResult<crate::services::backup::import::InspectReport> {
    let p = std::path::PathBuf::from(path.trim());
    if !p.is_file() {
        return Err(backup_err(
            crate::services::backup::BackupError::ArchiveMissing(
                p.display().to_string(),
            ),
        ));
    }
    crate::services::backup::import::inspect_backup(&p).map_err(backup_err)
}

/// 导入备份包并完全恢复。
///
/// # 安全链（任一步失败，现有数据保持完好）
///
/// 1. 只读校验整包（归属 / 版本 / sha256 / 数量对账）——**不写任何文件**；
/// 2. 给当前数据目录建立带时间戳的旁路备份，路径随结果回传；
/// 3. 在数据目录同级的暂存目录里组装完整的新数据；
/// 4. 执行导入后重置（路径改写 / 云同步游标 / 迁移 / 默认值 / WAL 作废 / 背景还原）；
/// 5. 逐个受管条目换上去，失败即原样放回。
///
/// 返回后界面必须提示用户**重启应用**：进程内的数据库连接仍指向替换前的数据。
#[tauri::command]
pub fn import_backup(
    state: State<'_, AppDataDir>,
    archive_path: String,
) -> AppResult<crate::services::backup::import::RestoreReport> {
    let data_dir = state.0.lock().unwrap().clone();
    let archive = std::path::PathBuf::from(archive_path.trim());
    crate::services::backup::restore_backup(&crate::services::backup::RestoreRequest {
        data_dir,
        archive_path: archive,
    })
    .map_err(backup_err)
}

// ---------------------------------------------------------------------------
// 迁移安全回归测试（复核 G-2 的直接证据）
//
// 这些测试守护本任务里**最危险**的一条路径：把"用户已经用过的新版数据目录"误判成
// "从未使用过的空库"，进而在手动迁移接管时把用户自己建的标签/设置静默丢弃。
// ---------------------------------------------------------------------------

#[cfg(test)]
mod migration_pristine_tests {
    use super::*;

    fn tmp_root(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tiez-pristine-test-{}-{}-{}",
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

    // ------------------------------------------------------------------
    // 进度：payload 字段名与节流判据（都是前后端并行实施的接缝）
    // ------------------------------------------------------------------

    /// 进度 payload 的**字段名必须是契约里的 camelCase**。
    ///
    /// 【为什么值得一条独立测试】`#[serde(rename_all = "camelCase")]` 漏了**不会编译
    /// 报错**，只会让 `stageLabel` 变成 `undefined`——于是用户看到一根没有说明文字的
    /// 进度条，而"迁移显示"这个核心诉求恰恰是"要有人话说明现在在干什么"。
    /// 这种缺陷在编译期与类型检查里都抓不到，只能靠对序列化结果下断言。
    #[test]
    fn progress_payload_uses_the_contract_field_names() {
        let payload = MigrateProgressPayload::from(&progress(
            "copying",
            12,
            40,
            4096,
            65536,
            Some("正在复制".to_string()),
        ));
        let value = serde_json::to_value(&payload).unwrap();
        let obj = value.as_object().expect("payload 必须是 JSON 对象");

        // 契约 §2 的七个字段，一个不能少、一个不能多。
        for key in [
            "stage",
            "stageLabel",
            "done",
            "total",
            "bytes",
            "bytesTotal",
            "message",
        ] {
            assert!(obj.contains_key(key), "payload 缺少契约字段 `{key}`：{value}");
        }
        assert_eq!(
            obj.len(),
            7,
            "payload 只应有契约里的 7 个字段，多出来的是没约定的内部信息：{value}"
        );
        // snake_case 拼写**不得**出现（那正是漏加 rename_all 时的症状）。
        for wrong in ["stage_label", "bytes_total"] {
            assert!(
                !obj.contains_key(wrong),
                "字段名必须是 camelCase，出现了 `{wrong}`：{value}"
            );
        }
        // 阶段文案由后端产出人话，界面原样显示。
        assert_eq!(obj["stageLabel"], "正在复制数据");
    }

    /// 结束事件的阶段文案与"是否可计量"必须与契约表一致。
    #[test]
    fn stage_labels_match_the_frozen_contract() {
        use crate::migration_identifier::stage_label;
        assert_eq!(stage_label("precheck"), "正在检查源目录");
        assert_eq!(stage_label("copying"), "正在复制数据");
        assert_eq!(stage_label("verifying"), "正在校验完整性");
        assert_eq!(stage_label("deferred"), "数据已就绪，等待重启接管");
        assert_eq!(stage_label("done"), "迁移完成");
        assert_eq!(stage_label("failed"), "迁移失败");
    }

    /// `copying` 的节流判据：未到窗口**不发**，最后一条**必发**。
    ///
    /// 【两个方向的错都有真实后果】
    /// - 不节流：上千个小文件逐条发事件，通道被刷爆，界面反而渲染不过来；
    /// - 末尾不强制：进度条永远停在 `999/1000`，用户以为迁移卡死了。
    #[test]
    fn throttling_is_skipped_until_the_window_but_the_last_item_always_goes_out() {
        use crate::migration_identifier::{should_emit_progress, PROGRESS_THROTTLE_MS};

        assert!(!should_emit_progress(0, false), "刚发过就不该再发");
        assert!(
            !should_emit_progress(PROGRESS_THROTTLE_MS - 1, false),
            "窗口内不得发"
        );
        assert!(
            should_emit_progress(PROGRESS_THROTTLE_MS, false),
            "到窗口就该发"
        );
        assert!(
            should_emit_progress(0, true),
            "最后一个条目必须**无条件**发一次（否则进度条停在 done < total）"
        );
    }

    /// 无宿主时进度发射器不得 panic（MCP / 单测场景走 `detached`）。
    #[test]
    fn detached_progress_emitter_is_a_silent_no_op() {
        let mut sink = MigrationProgressEmitter::detached();
        sink.stage(progress("precheck", 0, 0, 0, 0, None));
        sink.item(progress("copying", 1, 2, 10, 20, None), false);
        sink.item(progress("copying", 2, 2, 20, 20, None), true);
        sink.finish(progress("done", 2, 2, 20, 20, None));
    }

    /// **快照命令的语义**：没有进行过迁移时返回 `None`，**绝不伪造进度**。
    ///
    /// 前端按契约在 `listen` 之后调它拉初值；若这里返回一个"0%"的假快照，
    /// 用户一进设置页就会看到一根停在 0% 的进度条——比什么都不显示更糟。
    #[test]
    fn progress_snapshot_is_empty_before_any_migration() {
        *LAST_MIGRATION_PROGRESS.lock().unwrap() = None;
        let snapshot = get_migration_progress().unwrap();
        assert!(snapshot.is_none(), "从未迁移过时必须返回 None，不得伪造进度");

        // 写入一次之后应能读回同一份。
        *LAST_MIGRATION_PROGRESS.lock().unwrap() =
            Some(serde_json::json!({"stage": "copying", "stageLabel": "正在复制数据"}));
        let snapshot = get_migration_progress().unwrap().unwrap();
        assert_eq!(snapshot["stageLabel"], "正在复制数据");
        *LAST_MIGRATION_PROGRESS.lock().unwrap() = None;
    }

    /// 造一个**真 SQLite 库**并写入 `rows` 条剪贴板记录。
    ///
    /// 【为什么必须是真库】本仓库踩过两次"夹具写了个假库（100 字节文件头）、
    /// `rusqlite::open` 打不开、报错却指向被测代码"的坑（见维护文档与候选记忆
    /// `AB2-G3-086`）。端到端测试里一旦有"打开并读一下"的动作，假库就会以
    /// `file is not a database` 失败，而那与用户真实遇到的问题**不是同一件事**。
    /// 因此这里一律走产品自己的 `init_db` 建库。
    fn seeded_source(dir: &std::path::Path, rows: usize) -> std::path::PathBuf {
        std::fs::create_dir_all(dir.join("attachments")).unwrap();
        let db = dir.join("clipboard.db");
        let conn = crate::database::init_db(&db.to_string_lossy()).unwrap();
        {
            let mut stmt = conn
                .prepare(
                    "INSERT INTO clipboard_history (content_type, content, source_app, timestamp, preview) \
                     VALUES ('text', ?, 'OldApp', ?, '')",
                )
                .unwrap();
            for i in 0..rows {
                stmt.execute(rusqlite::params![
                    format!("旧版第 {i} 条"),
                    1_700_000_000i64 + i as i64
                ])
                .unwrap();
            }
        }
        drop(conn);
        std::fs::write(dir.join("attachments/old.png"), vec![b'a'; 512]).unwrap();
        dir.to_path_buf()
    }

    fn count_rows(db: &std::path::Path) -> i64 {
        rusqlite::Connection::open(db)
            .and_then(|c| {
                c.query_row("SELECT COUNT(*) FROM clipboard_history", [], |r| r.get(0))
            })
            .unwrap_or(-1)
    }

    /// **端到端：一次完整的"两阶段迁移 + 重启接管"**（不经过 Tauri 运行时）。
    ///
    /// 这条测试直接复现用户真机上的完整时序：
    ///
    /// ```text
    /// 1. 用户点迁移          -> stage_takeover：只复制到暂存，返回 Deferred
    /// 2. 后端写"待接管"标记   -> finalize_deferred：status = deferred，标记落盘
    /// 3. 用户重启应用        -> run_startup_takeover：在开库之前完成交换
    /// 4. 应用打开库          -> 120 条真实记录就在目标根层
    /// ```
    ///
    /// 【为什么这条最重要】真机上"必然失败"的那一步（给被占用的目标库改名）在这里
    /// 被**真的执行了一次**——只不过时机换到了"无人持句柄"的启动期。若两阶段设计有
    /// 任何一环接不上（标记没写、暂存被删、提升进了子目录、路径没改写），本条会红。
    #[test]
    fn two_phase_migration_survives_a_simulated_restart() {
        let root = tmp_root("two-phase-e2e");
        let native = root.join("native-appdata");
        std::fs::create_dir_all(&native).unwrap();
        let target = seeded_dir(&root); // 应用已启动过：空库躺在目标里（真实 init_db）
        let source = seeded_source(&root.join("old-data"), 120);

        // ---- 第 1 步：运行期只复制到暂存 ----
        let outcome = crate::migration_identifier::stage_takeover(&source, &target);
        let mut report = crate::app::apply_identifier_migration(&source, &target, outcome);
        assert_eq!(
            report.status, "deferred",
            "运行期必须得到 deferred（目标库被占用，当场交付不可能）"
        );

        // ---- 第 2 步：写"待接管"标记 ----
        finalize_deferred(&mut report, Some(&native), &source, &target);
        assert_eq!(report.status, "deferred", "标记写成功后状态仍是 deferred");
        assert!(
            report.pending_until_restart,
            "必须告诉界面'待重启接管'（否则界面会以为迁移失败了）"
        );
        assert!(
            crate::migration_pending::marker_path(&native).is_file(),
            "待接管标记必须真的落盘——它是下次启动唯一的线索"
        );
        let staging = crate::migration_identifier::takeover_staging_dir(&target);
        assert!(staging.is_dir(), "暂存目录必须保留");

        // 此刻目标根层的库里**一条都没有**（运行期一个字节都没动过目标）。
        assert_eq!(count_rows(&target.join("clipboard.db")), 0);

        // ---- 第 3 步：模拟重启，在"开库之前"完成接管 ----
        let outcome = crate::migration_pending::run_startup_takeover(
            &native,
            &mut |staging: &std::path::Path, target: &std::path::Path| {
                crate::migration_identifier::promote_staged_takeover_default(staging, target)
                    .map(|_| ())
            },
        );
        assert!(
            matches!(
                outcome,
                crate::migration_pending::TakeoverOutcome::Promoted { .. }
            ),
            "启动期接管必须成功（此时无人持句柄），实际 {outcome:?}"
        );

        // ---- 第 4 步：数据真的在目标根层 ----
        assert_eq!(
            count_rows(&target.join("clipboard.db")),
            120,
            "接管后目标根层的库里必须有那 120 条真实记录"
        );
        assert!(
            target.join("attachments/old.png").is_file(),
            "附件也必须一并到位（否则图片全打不开）"
        );
        // 标记已被消费、暂存已被提升
        assert!(
            !crate::migration_pending::marker_path(&native).exists(),
            "接管成功后标记必须清除，否则每次启动都会白跑一遍"
        );
        assert!(!staging.exists(), "接管成功后暂存目录必须消失");
        // 源目录全程只读
        assert_eq!(count_rows(&source.join("clipboard.db")), 120, "源库必须完好");
    }

    /// **标记里记的源目录必须是归一化之后那一层**（否则便携版用户图片全打不开）。
    ///
    /// 【这条守的是一个静默失败】用户选便携版**外层**目录时，真正的数据在
    /// `外层/内层/data/`。接管成功后要按"源 → 目标"改写库里记录的附件绝对路径，
    /// 而那些记录写的是内层 `data/` 的路径。
    ///
    /// 若标记里记的是外层：`外层/attachments` 与记录前缀**不匹配** ⇒ 一条也改不到
    /// （静默无效）。后果是"记录都在、图片全打不开"——用户一定会报"迁移把图片弄丢了"，
    /// 而实际上数据全在、只是指针没改。
    #[test]
    fn marker_records_the_resolved_data_dir_not_the_layer_the_user_picked() {
        let root = tmp_root("marker-resolved-source");
        let native = root.join("native-appdata");
        std::fs::create_dir_all(&native).unwrap();
        let target = seeded_dir(&root);

        // 造一份真实的便携版两层同名目录：外层 / 内层 / data。
        let outer = root.join("TieZ_0.3.3-portable");
        let inner = outer.join("TieZ_0.3.3-portable");
        let data = inner.join("data");
        seeded_source(&data, 30);

        // 用户点的是**外层**。
        let outcome = crate::migration_identifier::stage_takeover(&outer, &target);
        let mut report = crate::app::apply_identifier_migration(&outer, &target, outcome);
        finalize_deferred(&mut report, Some(&native), &outer, &target);
        assert_eq!(report.status, "deferred");

        let pending = crate::migration_pending::read(&native).expect("标记必须写得进");
        assert_eq!(
            pending.source_dir,
            crate::migration_identifier::resolve_source_dir(&outer),
            "标记必须记归一化之后的那一层"
        );
        // 归一化的结果就是内层的 `data/`。
        assert!(
            pending.source_dir.ends_with("data"),
            "便携版外层应被归一化到内层的 data/，实际记的是 {}",
            pending.source_dir.display()
        );
        // 且这个位置上**确实有** attachments（路径改写要有东西可改）。
        assert!(
            pending.source_dir.join("attachments").is_dir(),
            "记录下来的源目录下必须真的有 attachments/，否则改写必然落空"
        );
    }

    /// 标记**写不进去**时，`Deferred` 必须如实降级成 `failed`。
    ///
    /// 【为什么不能含糊】若对界面说"重启后自动完成"、而标记其实没写成功，用户重启后
    /// 什么都不会发生——那正是 v0.5.2 那个"提示不可执行"的老毛病换了个形式。
    /// 唯一诚实的表达是"这次没成"，并保留暂存目录（下次还能重试）。
    #[test]
    fn deferred_without_a_writable_marker_becomes_failed_not_a_false_promise() {
        let root = tmp_root("marker-unwritable");
        let target = seeded_dir(&root);
        let source = seeded_source(&root.join("old-data"), 5);

        let outcome = crate::migration_identifier::stage_takeover(&source, &target);
        let mut report = crate::app::apply_identifier_migration(&source, &target, outcome);
        assert_eq!(report.status, "deferred");

        // 原生数据目录取不到（`None`）＝标记无从写入。
        finalize_deferred(&mut report, None, &source, &target);

        assert_eq!(
            report.status, "failed",
            "标记写不下时必须如实报失败，不能给一个不可执行的承诺"
        );
        assert!(!report.pending_until_restart);
        assert!(
            report
                .error
                .as_deref()
                .is_some_and(|e| e.contains("待接管") || e.contains("暂存")),
            "错误里必须说清'数据已复制但状态没记下'，实际：{:?}",
            report.error
        );
        // 暂存必须保留（它是重试的唯一输入）
        assert!(crate::migration_identifier::takeover_staging_dir(&target).is_dir());
    }

    /// 造一个"刚装好的新版"数据目录：走真实的 init_db（迁移 + seed_defaults）。
    fn seeded_dir(root: &std::path::Path) -> std::path::PathBuf {
        let dir = root.join("fresh-install");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("clipboard.db");
        crate::database::init_db(&db.to_string_lossy()).unwrap();
        dir
    }

    /// 全新装好的数据目录 → 判定为"从未使用过"（否则手动迁移会被自己刚装好的空库挡住）。
    #[test]
    fn fresh_install_is_pristine() {
        let root = tmp_root("fresh");
        let dir = seeded_dir(&root);
        assert!(
            target_db_is_pristine(&dir),
            "刚装好的新版数据目录应判定为未使用过"
        );
    }

    /// **G-2 核心回归**：0 条剪贴板记录、但用户建了自己的标签 → 必须判定为"在用"。
    ///
    /// 修复前判据只看 `clipboard_history` 条数，这种情况会被判成空库并接管，源库整体
    /// 替换目标库，用户新建的标签被静默丢弃（实测 `no such table: saved_tags`）。
    #[test]
    fn user_created_tag_makes_target_non_pristine() {
        let root = tmp_root("tag");
        let dir = seeded_dir(&root);
        assert!(target_db_is_pristine(&dir), "前提：先确认基线是空库");

        let conn = rusqlite::Connection::open(dir.join("clipboard.db")).unwrap();
        conn.execute(
            "INSERT INTO saved_tags (name, color) VALUES ('我自己建的标签', '#ff0000')",
            [],
        )
        .unwrap();
        drop(conn);

        assert!(
            !target_db_is_pristine(&dir),
            "用户已建标签时绝不能判定为空库——否则接管会丢弃它"
        );
    }

    /// **G-2 核心回归**：0 条剪贴板记录、但用户改过设置 → 必须判定为"在用"。
    /// 改过设置的库**仍然**算空库——判据是"有没有用户数据"，不是"设置是否还是出厂值"。
    ///
    /// 【这条测试曾经断言的是相反的行为，而那个行为是一个真实缺陷】
    ///
    /// 旧判据要求 settings 与全新库**逐键相等**。但应用启动与日常使用会主动写入设置，
    /// 其中最平凡的是**窗口尺寸**（`setup.rs` 在窗口大小变化时写 `app.window_width` /
    /// `app.window_height`）。于是用户只要调整过一次窗口，判定即为 false，手动迁移
    /// 从此**永远**被 `target_already_has_data` 挡掉：旧数据一条都没进来，界面却说
    /// "新版数据目录里已经有你自己的记录"。用户实测撞到的就是这个。
    ///
    /// 旧测试把那个行为写成了"绝不能判定为空库"，等于用一条断言把缺陷保护了起来——
    /// 测试全绿，功能全坏。这里改为断言**正确**行为。
    #[test]
    fn customized_setting_still_counts_as_pristine() {
        let root = tmp_root("setting");
        let dir = seeded_dir(&root);
        assert!(target_db_is_pristine(&dir), "前提：先确认基线是空库");

        let conn = rusqlite::Connection::open(dir.join("clipboard.db")).unwrap();
        // 模拟用户改主题——以及更平凡地，把窗口拖动一下（应用会自动写入尺寸）。
        conn.execute(
            "UPDATE settings SET value = 'definitely-not-the-default' WHERE key = 'app.theme'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('app.window_width', '1234')",
            [],
        )
        .unwrap();
        drop(conn);

        assert!(
            target_db_is_pristine(&dir),
            "只改过设置（没有剪贴板记录、没有用户标签）时，这个库仍应允许被迁移接管；\
             否则用户调整过一次窗口大小就再也迁不进旧数据了"
        );
    }

    /// 但**有用户数据**时，改没改设置都不算空库——上面放宽判据不能把这条也放过去。
    #[test]
    fn settings_alone_do_not_override_real_user_data() {
        let root = tmp_root("setting-with-data");
        let dir = seeded_dir(&root);
        let conn = rusqlite::Connection::open(dir.join("clipboard.db")).unwrap();
        conn.execute(
            "INSERT INTO clipboard_history
                (content_type, content, source_app, timestamp, preview)
             VALUES ('text', 'hello', 'TestApp', 1, 'hello')",
            [],
        )
        .unwrap();
        drop(conn);

        assert!(
            !target_db_is_pristine(&dir),
            "有真实剪贴板记录时必须判为在用，不能被设置这一条放宽掉"
        );
    }

    /// 有一条剪贴板记录 → 判定为"在用"（最基础的判据仍生效）。
    #[test]
    fn existing_clipboard_record_makes_target_non_pristine() {
        let root = tmp_root("record");
        let dir = seeded_dir(&root);
        let conn = rusqlite::Connection::open(dir.join("clipboard.db")).unwrap();
        conn.execute(
            "INSERT INTO clipboard_history (content_type, content, source_app, timestamp, preview)
             VALUES ('text', 'x', 'app', 1, 'x')",
            [],
        )
        .unwrap();
        drop(conn);

        assert!(!target_db_is_pristine(&dir));
    }

    /// 目录里压根没有数据库 → "从未使用过"（接管是安全的）。
    #[test]
    fn missing_database_counts_as_pristine() {
        let root = tmp_root("nodir");
        let dir = root.join("no-db-here");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(target_db_is_pristine(&dir));
    }

    /// 数据库打不开（不是合法 SQLite 文件）→ **保守判为"在用"**，绝不接管。
    #[test]
    fn unreadable_database_is_treated_as_in_use() {
        let root = tmp_root("broken");
        let dir = root.join("broken-db");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("clipboard.db"), b"this is not a sqlite database").unwrap();
        assert!(
            !target_db_is_pristine(&dir),
            "无法确认是空库时必须按'在用'处理（宁可少迁，不可覆盖）"
        );
    }

    /// 判定过程**不得改动目标目录**（文件层复制探针 + 探针目录清理）。
    ///
    /// SQLite 打开库时会改写 `-shm`，因此这里对目标的字节内容做前后比对。
    #[test]
    fn pristine_check_leaves_target_untouched() {
        let root = tmp_root("untouched");
        let dir = seeded_dir(&root);

        let snapshot = |d: &std::path::Path| -> Vec<(String, u64)> {
            let mut v: Vec<(String, u64)> = std::fs::read_dir(d)
                .unwrap()
                .flatten()
                .filter(|e| e.path().is_file())
                .map(|e| {
                    (
                        e.file_name().to_string_lossy().to_string(),
                        e.metadata().unwrap().len(),
                    )
                })
                .collect();
            v.sort();
            v
        };
        let before = snapshot(&dir);

        let _ = target_db_is_pristine(&dir);

        assert_eq!(
            snapshot(&dir),
            before,
            "空库判定不得在目标目录留下任何改动（含 -shm 大小变化）"
        );
    }

    /// 删除守卫：新版还没有数据时必须拒绝删除用户手选的旧目录。
    #[test]
    fn delete_guard_blocks_until_new_version_has_data() {
        let root = tmp_root("guard");
        // 目录不存在 → 拒绝
        assert!(can_remove_source_safely(&root.join("nope")).is_err());
        // 全新装好（无记录）→ 仍然拒绝
        let dir = seeded_dir(&root);
        assert!(
            can_remove_source_safely(&dir).is_err(),
            "新版尚无记录时必须拦住删除，否则用户会丢掉唯一的旧数据"
        );
        // 有记录 → 放行
        let conn = rusqlite::Connection::open(dir.join("clipboard.db")).unwrap();
        conn.execute(
            "INSERT INTO clipboard_history (content_type, content, source_app, timestamp, preview)
             VALUES ('text', 'x', 'app', 1, 'x')",
            [],
        )
        .unwrap();
        drop(conn);
        assert!(can_remove_source_safely(&dir).is_ok());
    }
}

// ---------------------------------------------------------------------------
// 开机自启动：写后回读的判据测试
//
// 这一组测试守护的是用户反馈里最核心的那句话：「纯应用里面显示设置了不一定生效」。
// 旧判据是"三个名字任一存在即算已开启"，它有两个方向的错：
//   - **误报开启**：旧版残留名（`TieZ` / `tie-z`）还在，就报"已开启"，
//     哪怕新值根本没写成功；
//   - **误报开启（更隐蔽）**：值存在但指向**改名/搬家前的旧路径**，
//     系统开机时拉起的是一个不存在的文件，用户看到的开关却是亮的。
//
// 【为什么用纯函数 + 构造的注册表快照，而不是真注册表】
// 判定逻辑与"读注册表"是两件事：把判定抽成 `autostart_state_from` 后，
// 这些关键判据可以在任何平台上被真正断言，而不是只能靠真机人工点一遍。
// 读注册表那一段（`read_autostart_entries`）没有可移植的模拟物，
// 因此在非 Windows 目标上不冒充覆盖——见本模块末尾的说明。
// ---------------------------------------------------------------------------

#[cfg(test)]
mod autostart_readback_tests {
    use super::*;

    const EXE: &str = r"C:\Program Files\Tiez-Next\tiez-next.exe";

    fn entries(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// 值存在且指向当前 exe → 判定为已开启，并把命令原文作为证据回传。
    #[test]
    fn enabled_only_when_the_value_points_at_this_exe() {
        let state = autostart_state_from(
            &entries(&[("Tiez-Next", &format!("\"{}\" --minimized", EXE))]),
            EXE,
        );
        assert!(state.enabled, "值指向当前 exe 时必须判定为已开启");
        assert_eq!(
            state.registered_command.as_deref(),
            Some(format!("\"{}\" --minimized", EXE).as_str()),
            "必须把读回的注册表原文作为证据回传（界面要显示的就是它）"
        );
        assert!(state.readable);
        assert!(state.stale_names.is_empty());
    }

    /// **旧名残留绝不等于已开启**。
    ///
    /// 这是旧判据（`Tiez-Next` / `TieZ` / `tie-z` 任一存在即为 true）的直接反例：
    /// 机器上只剩改名前留下的 `TieZ`，而新版的值**根本没写成功**。
    /// 用户此时的真实处境是"开机不会自启"，开关不能是亮的。
    #[test]
    fn legacy_leftovers_alone_do_not_count_as_enabled() {
        for legacy in ["TieZ", "tie-z"] {
            let state = autostart_state_from(
                &entries(&[(legacy, r"C:\Old\TieZ\tiez.exe")]),
                EXE,
            );
            assert!(
                !state.enabled,
                "只剩旧版残留名 `{}` 时必须判为**未开启**（旧判据会在此误报）",
                legacy
            );
            assert!(
                state.registered_command.is_none(),
                "未开启时不得回传命令原文，否则界面会把失效的旧命令当成生效证据"
            );
            assert_eq!(
                state.stale_names,
                vec![legacy.to_string()],
                "旧名要如实列出（界面据此提示用户可顺手清理）"
            );
        }
    }

    /// 值指向**旧路径**时不算已开启（改名 / 换安装位置 / 便携版搬目录之后）。
    #[test]
    fn value_pointing_at_a_stale_path_is_not_enabled() {
        let state = autostart_state_from(
            &entries(&[(
                "Tiez-Next",
                r#""C:\Old\Tiez-Next\tiez-next.exe" --minimized"#,
            )]),
            EXE,
        );
        assert!(
            !state.enabled,
            "指向已失效旧路径自启动会在开机时拉起一个不存在的文件，不得判为已开启"
        );
        assert!(state.registered_command.is_none());
    }

    /// 路径比较必须是**大小写无关、分隔符无关**的（Windows 语义），
    /// 否则同一台机器上会因大小写差异被判成"未生效"。
    #[test]
    fn path_comparison_is_case_and_separator_insensitive() {
        for variant in [
            r#""c:\program files\tiez-next\tiez-next.exe" --minimized"#,
            r#""C:/Program Files/Tiez-Next/Tiez-Next.exe" --minimized"#,
        ] {
            let state = autostart_state_from(&entries(&[("Tiez-Next", variant)]), EXE);
            assert!(state.enabled, "`{}` 应被认作指向当前 exe", variant);
        }
    }

    /// 空 exe（`current_exe()` 取不到）时必须安全地落到"未开启"，
    /// 而不是因为"路径比较两边都空"而误判为相等 → 报已开启。
    #[test]
    fn empty_current_exe_never_reports_enabled() {
        let state = autostart_state_from(&entries(&[("Tiez-Next", r#""C:\a.exe" --minimized"#)]), "");
        assert!(!state.enabled, "取不到当前 exe 时不得声称已开启");
        let empty_value = autostart_state_from(&entries(&[("Tiez-Next", "")]), EXE);
        assert!(!empty_value.enabled, "空值不得判为已开启");
    }

    /// 旧名与新名**同时存在**且新名正确时才为已开启；旧名不参与判定，只被列出。
    #[test]
    fn stale_names_are_reported_without_affecting_the_verdict() {
        let state = autostart_state_from(
            &entries(&[
                ("TieZ", r"C:\Old\TieZ\tiez.exe"),
                ("tie-z", r"C:\Old\tie-z\tiez.exe"),
                ("Tiez-Next", &format!("\"{}\" --minimized", EXE)),
            ]),
            EXE,
        );
        assert!(state.enabled);
        assert_eq!(state.stale_names, vec!["TieZ".to_string(), "tie-z".to_string()]);
    }

    /// 带引号与不带引号的命令串都要能取出路径；参数不参与比较。
    #[test]
    fn command_target_matching_handles_quotes_and_arguments() {
        assert!(command_targets_current_exe(
            &format!("\"{}\" --minimized", EXE),
            EXE
        ));
        assert!(command_targets_current_exe(EXE, EXE));
        assert!(!command_targets_current_exe(
            &format!("\"{}\" --minimized", EXE),
            r"C:\Other\app.exe"
        ));
    }

    /// **反向对照锚点**：若把判据回退成"任一名字存在即为真"，
    /// `legacy_leftovers_alone_do_not_count_as_enabled` 与
    /// `value_pointing_at_a_stale_path_is_not_enabled` 必须变红。
    ///
    /// 这里用同一份快照把"老判据会给出的答案"显式写出来，作为该反向对照的**书面依据**
    /// （老判据的实现是 `exists(a) || exists(b) || exists(c)`）。
    #[test]
    fn old_loose_criterion_would_have_reported_these_as_enabled() {
        let snapshot = entries(&[("TieZ", r"C:\Old\Tiez.exe")]);
        let loose = snapshot.iter().any(|(n, _)| {
            n == "Tiez-Next" || n == "TieZ" || n == "tie-z"
        });
        assert!(loose, "老判据在这份快照上确实会给出 true（这正是它误报的场景）");
        let strict = autostart_state_from(&snapshot, EXE);
        assert!(
            !strict.enabled,
            "严格判据必须与老判据给出**不同**的答案，否则这条测试就没有区分力"
        );
    }
}
