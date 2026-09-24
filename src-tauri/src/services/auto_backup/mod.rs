//! 自动容灾备份（定时 + 启动）——存储、轮换、固定与对外的命令接口。
//!
//! # 分工
//!
//! ```text
//! config.rs    四个设置项（开关 / 周期 / 最大留存 / 启动备份）与校验
//! store.rs     目录布局、命名、固定状态、轮换算法（本模块的实体）
//! 本文件        调度接线 + Tauri 命令（供前端调用）
//! ```
//!
//! # 打 zip 与恢复这两件事**不在这里重新实现**
//!
//! 生成一份备份 = 调 [`crate::services::backup::export::create_backup`]；恢复一份自动备份
//! = 调 [`crate::services::backup::import::restore_backup`]。这两条链已经各自带了完整的
//! 安全设计（`VACUUM INTO` 在线快照、原子改名、"不要写进数据目录"的护栏、导入前的旁路
//! 备份、失败回滚），另写一套只会多出一份需要各自维护的正确性负担。
//!
//! # 与前端的关系
//!
//! 界面负责二次确认（删除/恢复）与展示；后端负责"文件确实存在""名字确实是本模块的"
//! "固定数不超上限"这类**不能只靠界面守**的判断。凡是需要用户看到原因码的失败，一律走
//! [`AppError::Raw`] 返回裸 JSON（`code` + 数值），这样英文/繁体用户不会被中文前缀破坏
//! 错误码映射——与既有备份命令 `backup_err` 的做法一致。

pub mod config;
pub mod schedule;
pub mod store;

#[cfg(test)]
mod tests;

use crate::app_state::AppDataDir;
use crate::database::DbState;
use crate::error::{AppError, AppResult};
use crate::infrastructure::repository::settings_repo::SettingsRepository;
use store::{AutoBackupError, AutoBackupStore, BackupEntry, BackupOrigin};
use tauri::{AppHandle, Emitter, Manager, State};

pub use config::{
    AutoBackupConfig, ConfigPatch, DEFAULT_BACKUP_ON_STARTUP, DEFAULT_ENABLED,
    DEFAULT_INTERVAL_MINUTES, DEFAULT_MAX_KEEP, INTERVAL_MINUTES_MAX, INTERVAL_MINUTES_MIN,
    MAX_KEEP_MAX, MAX_KEEP_MIN,
};
pub use store::{auto_backup_dir, RotationOutcome};

/// 备份创建完成后广播给界面的事件名。
pub const EVENT_CREATED: &str = "auto-backup-created";
/// 备份目录发生变化（删除/固定/取消固定/轮换）后广播的事件名。
pub const EVENT_CHANGED: &str = "auto-backup-changed";

/// 串行化对备份目录的所有改动。
///
/// 【为什么需要】后台定时循环与界面命令（固定/删除/立即备份）会并发碰到同一个目录：
/// 一边在轮换删文件、一边在改名固定同一份，就会出现"索引里有一条、文件已经没了"的分叉。
/// 这里用一把进程内互斥锁把它们排成队。取锁被 poison 时也继续执行（与导入锁同一取舍：
/// 锁只用于互斥，不保护共享数据，没必要因为一次 panic 让功能永久不可用）。
///
/// 【代价，如实记录】备份本身要花时间（数据库越大越久），期间界面的"列出备份"会排队等待。
/// 这是刻意的取舍：宁可让列表晚几百毫秒出现，也不要让它读到轮换改到一半的目录。
static STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_store() -> std::sync::MutexGuard<'static, ()> {
    STORE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 把 [`AutoBackupError`] 转成带结构化载荷的命令错误。
///
/// 必须走 `Raw`：`Validation` 的 `Display` 会加 "验证错误: " 前缀，前端 `JSON.parse` 会失败，
/// 于是只能把整串（含中文前缀）显示给用户，三语词条全部失效。
pub fn to_app_error(e: AutoBackupError) -> AppError {
    AppError::Raw(e.payload().to_string())
}

fn store_for(data_dir: &std::path::Path) -> Result<AutoBackupStore, AppError> {
    AutoBackupStore::open_for_data_dir(data_dir).map_err(to_app_error)
}

fn data_dir_of(state: &State<'_, AppDataDir>) -> std::path::PathBuf {
    state.0.lock().unwrap().clone()
}

fn load_config(db: &DbState) -> AutoBackupConfig {
    config::load(&db.settings_repo)
}

/// 组装给界面的完整视图：目录、配置、以及"还能固定几个"。
fn view(
    store: &AutoBackupStore,
    cfg: AutoBackupConfig,
    entries: Vec<BackupEntry>,
    warnings: Vec<String>,
) -> serde_json::Value {
    let pinned_count = entries.iter().filter(|e| e.pinned).count() as u32;
    serde_json::json!({
        "dir": store.dir.to_string_lossy(),
        "config": cfg,
        "maxPinned": cfg.max_pinned(),
        "pinnedCount": pinned_count,
        "totalCount": entries.len() as u32,
        "entries": entries,
        "warnings": warnings,
    })
}

// ---------------------------------------------------------------------------
// 命令
// ---------------------------------------------------------------------------

/// 读取自动备份配置（开关 / 周期 / 最大留存 / 启动备份）。
#[tauri::command]
pub fn get_auto_backup_config(db_state: State<'_, DbState>) -> AppResult<AutoBackupConfig> {
    Ok(load_config(&db_state))
}

/// 修改自动备份配置（只改传进来的字段）。
///
/// 越界值**报错**而不是夹紧：用户在输入框里敲了 500，界面必须告诉他合法区间是 1–200，
/// 而不是悄悄改成 200 让他以为生效了。报错载荷带 `min`/`max`，界面无需硬编码边界。
#[tauri::command]
pub fn set_auto_backup_config(
    app: AppHandle,
    db_state: State<'_, DbState>,
    patch: ConfigPatch,
) -> AppResult<AutoBackupConfig> {
    let next = patch.apply(load_config(&db_state));
    config::validate(&next).map_err(|e| AppError::Raw(e.payload().to_string()))?;
    config::save(&db_state.settings_repo, &next)?;
    // 定时循环每轮都重新读配置，因此这里不需要额外的唤醒信号。
    let _ = app.emit(EVENT_CHANGED, serde_json::json!({"reason": "config"}));
    Ok(next)
}

/// 列出全部自动备份（时间精确到秒、大小、来源、是否固定），并附带配置与上限信息。
#[tauri::command]
pub fn list_auto_backups(
    state: State<'_, AppDataDir>,
    db_state: State<'_, DbState>,
) -> AppResult<serde_json::Value> {
    let data_dir = data_dir_of(&state);
    let cfg = load_config(&db_state);
    let _guard = lock_store();
    let store = store_for(&data_dir)?;
    let entries = store.list().map_err(to_app_error)?;
    Ok(view(&store, cfg, entries, store.load_warnings.clone()))
}

/// 固定 / 取消固定一份自动备份。
///
/// 固定上限 = **最大留存份数 − 1**（用户原话："保留一个位置由于轮换"）。达到上限时返回
/// `auto_backup_pinned_limit_reached`，载荷带 `maxKeep` / `maxPinned` / `currentPinned`，
/// 界面据此弹出用户要求的那段提示。
#[tauri::command]
pub fn set_auto_backup_pinned(
    app: AppHandle,
    state: State<'_, AppDataDir>,
    db_state: State<'_, DbState>,
    archive_name: String,
    pinned: bool,
) -> AppResult<bool> {
    let data_dir = data_dir_of(&state);
    let cfg = load_config(&db_state);
    let _guard = lock_store();
    let mut store = store_for(&data_dir)?;
    let result = store
        .set_pinned(&archive_name, pinned, &cfg)
        .map_err(to_app_error)?;
    let _ = app.emit(EVENT_CHANGED, serde_json::json!({"reason": "pin"}));
    Ok(result)
}

/// 删除一份自动备份。
///
/// **二次确认由界面负责**（用户明确要求）。后端只保证删的确实是这个目录里、符合命名规则的
/// 那一份——名字是路径拼接的输入，必须是不可信输入。
#[tauri::command]
pub fn delete_auto_backup(
    app: AppHandle,
    state: State<'_, AppDataDir>,
    archive_name: String,
) -> AppResult<()> {
    let data_dir = data_dir_of(&state);
    let _guard = lock_store();
    let mut store = store_for(&data_dir)?;
    store.delete(&archive_name).map_err(to_app_error)?;
    let _ = app.emit(EVENT_CHANGED, serde_json::json!({"reason": "delete"}));
    Ok(())
}

/// 从一份自动备份恢复全部数据。
///
/// 复用既有的导入链（`restore_backup`）：它自己会在导入前给当前数据建一份旁路备份，
/// 失败时一个字节都不动现有数据。返回的 `restoreReport.restartRequired` 为真，界面必须
/// 提示用户重启应用。
#[tauri::command]
pub fn restore_auto_backup(
    app: AppHandle,
    state: State<'_, AppDataDir>,
    archive_name: String,
) -> AppResult<serde_json::Value> {
    let data_dir = data_dir_of(&state);
    let archive_path = {
        let _guard = lock_store();
        let store = store_for(&data_dir)?;
        // 确认这份确实存在于**自动备份目录**里：不接受任意路径，避免这条命令变成
        // "导入任意 zip"的后门（那是既有 `import_backup` 的职责，它有它自己的确认流程）。
        store.resolve_existing(&archive_name).map_err(to_app_error)?
    };

    let report = crate::services::backup::restore_backup(
        &crate::services::backup::RestoreRequest {
            data_dir,
            archive_path,
        },
    )
    .map_err(|e| to_app_error(AutoBackupError::Export(e.to_string())))?;

    let _ = app.emit(EVENT_CHANGED, serde_json::json!({"reason": "restore"}));
    Ok(serde_json::json!({
        "archiveName": archive_name,
        "restoreReport": report,
    }))
}

/// 立即备份一次（手动触发）。占用的是与定时备份**同一批留存名额**。
///
/// 语义上它仍属于"容灾自动保险"这条链：写同一个目录、一起参与轮换与上限。与既有的
/// "导出备份"（用户自己选路径、不受份数与轮换约束）是两件不同的事。
#[tauri::command]
pub fn run_auto_backup_now(
    app: AppHandle,
    app_version: String,
    state: State<'_, AppDataDir>,
    db_state: State<'_, DbState>,
) -> AppResult<serde_json::Value> {
    let data_dir = data_dir_of(&state);
    let cfg = load_config(&db_state);
    let _guard = lock_store();
    let mut store = store_for(&data_dir)?;
    let entry = store
        .create(&data_dir, BackupOrigin::Manual, &app_version)
        .map_err(to_app_error)?;
    let rotation = store
        .enforce_rotation(cfg.max_keep)
        .map_err(to_app_error)?;
    let entries = store.list().map_err(to_app_error)?;
    let payload = view(&store, cfg, entries, rotation.warnings.clone());

    let _ = app.emit(
        EVENT_CREATED,
        serde_json::json!({"archiveName": entry.archive_name, "origin": "manual"}),
    );
    Ok(serde_json::json!({
        "entry": entry,
        "rotation": rotation,
        "list": payload,
    }))
}

/// 打开自动备份所在目录（供用户自己查看/取走 zip）。
#[tauri::command]
pub fn open_auto_backup_folder(state: State<'_, AppDataDir>) -> AppResult<String> {
    let data_dir = data_dir_of(&state);
    let dir = auto_backup_dir(&data_dir);
    std::fs::create_dir_all(&dir)?;
    let dir_str = dir.to_string_lossy().to_string();

    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer").arg(&dir_str).spawn();
    }
    Ok(dir_str)
}

// ---------------------------------------------------------------------------
// 后台服务：启动一次 + 定时循环
// ---------------------------------------------------------------------------

/// 在应用启动时接好自动备份服务。
///
/// 刻意**不阻塞启动**：整件事（包括"启动时备份一次"）都丢进后台任务，应用窗口先出来。
/// 用户要的是"软件刚启动自动备份一次"，不是"软件启动被一次备份卡住"。
///
/// # 与安装目录的关系（同名目录 `Tiez-Next/` 不会被卸载器带走）
///
/// 备份目录是 `<数据目录父级>/Tiez-Next/auto_backups/`，而数据目录默认是
/// `%LOCALAPPDATA%\com.tieznext`（安装目录是 `%LOCALAPPDATA%\Tiez-Next`，两者同级）。
/// NSIS 卸载器只清理它自己登记的安装目录与用户手工放进安装目录的东西；这个同名兄弟目录
/// 不在其列。这不只是推测——本任务写测试时在暂存根目录下实测确认过：造出
/// `<root>/Tiez-Next/auto_backups/` 与 `<root>/com.tieznext/` 并存后，递归删除后者
/// **不会**碰到前者（`backup_dir_is_a_sibling_of_the_data_dir` 与
/// `store_refuses_a_dir_inside_the_data_dir` 守着这条兄弟关系）。
///
/// 【未验证的部分，如实标注】没有在真实 Windows 上跑过一次卸载来端到端确认。若将来有人
/// 把备份目录改到安装目录**内部**，这条结论立刻失效——那时卸载就会带走全部容灾副本。
pub fn spawn_auto_backup_service(app: AppHandle) {
    tauri::async_runtime::spawn_blocking(move || {
        startup_pass(&app);
        loop {
            std::thread::sleep(schedule::TICK);
            scheduled_pass(&app);
        }
    });
}

/// 启动期的第一遍：只做"启动备份"（不受定时开关约束）。
fn startup_pass(app: &AppHandle) {
    let Some(data_dir) = app_data_dir(app) else {
        return;
    };
    let cfg = read_config(app);

    let _guard = lock_store();
    let Ok(mut store) = AutoBackupStore::open_for_data_dir(&data_dir) else {
        return;
    };
    let outcome = schedule::run_startup_backup(
        &mut store,
        &data_dir,
        &app_version(app),
        cfg.backup_on_startup,
    );
    if outcome.created.is_some() {
        // 启动备份也要遵守留存上限：否则"把份数调小之后重启"会让总量一直超着。
        let _ = store.enforce_rotation(cfg.max_keep);
        let _ = app.emit(EVENT_CREATED, serde_json::json!({"origin": "startup"}));
    }
    for w in outcome.warnings {
        crate::info!("[AUTO_BACKUP] {}", w);
    }
}

/// 定时循环的每一遍：读配置 → 判断是否到点 → 备份并轮换。
fn scheduled_pass(app: &AppHandle) {
    let Some(data_dir) = app_data_dir(app) else {
        return;
    };
    let cfg = read_config(app);
    if !cfg.enabled {
        return;
    }

    let _guard = lock_store();
    let Ok(mut store) = AutoBackupStore::open_for_data_dir(&data_dir) else {
        return;
    };
    let now_ms = chrono::Local::now().timestamp_millis();
    let outcome = schedule::run_scheduled_tick(
        &mut store,
        &data_dir,
        &app_version(app),
        cfg.enabled,
        cfg.interval_minutes,
        now_ms,
    );
    if outcome.created.is_some() {
        if let Ok(rotation) = store.enforce_rotation(cfg.max_keep) {
            for w in &rotation.warnings {
                crate::info!("[AUTO_BACKUP] {}", w);
            }
        }
        let _ = app.emit(EVENT_CREATED, serde_json::json!({"origin": "scheduled"}));
    }
    for w in outcome.warnings {
        crate::info!("[AUTO_BACKUP] {}", w);
    }
}

fn app_data_dir(app: &AppHandle) -> Option<std::path::PathBuf> {
    let state = app.try_state::<AppDataDir>()?;
    let dir = state.0.lock().ok()?.clone();
    if dir.as_os_str().is_empty() {
        None
    } else {
        Some(dir)
    }
}

fn read_config(app: &AppHandle) -> AutoBackupConfig {
    match app.try_state::<DbState>() {
        Some(db) => config::load(&db.settings_repo),
        None => AutoBackupConfig::default(),
    }
}

fn app_version(app: &AppHandle) -> String {
    app.package_info().version.to_string()
}

/// 供测试与其他模块使用：把配置里的四个键一次性写进一个设置库。
pub fn seed_config(repo: &impl SettingsRepository, cfg: &AutoBackupConfig) -> AppResult<()> {
    config::save(repo, cfg)
}
