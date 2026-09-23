use crate::app_state::AppDataDir;
use crate::database::ENCRYPT_PREFIX;
use crate::error::{AppError, AppResult};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json;
use tauri::{AppHandle, Manager, State};

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

/// 供"迁移中心"展示的一条历史数据目录信息（前端友好格式）。
#[derive(serde::Serialize)]
pub struct LegacyDirView {
    pub path: String,
    pub identifier: String,
    pub bytes: u64,
    pub files: u64,
    pub has_database: bool,
}

/// 迁移中心：列出旧标识符遗留的数据目录及其占用。
///
/// 只读操作，不修改任何数据。用户据此决定是否清理。
#[tauri::command]
pub fn list_legacy_data_dirs(state: State<'_, AppDataDir>) -> AppResult<Vec<LegacyDirView>> {
    let current = state.0.lock().unwrap().clone();
    Ok(crate::migration_identifier::list_legacy_dirs(&current)
        .into_iter()
        .map(|i| LegacyDirView {
            path: i.path.to_string_lossy().to_string(),
            identifier: i.identifier,
            bytes: i.bytes,
            files: i.files,
            has_database: i.has_database,
        })
        .collect())
}

/// 迁移中心：备份后删除一个遗留数据目录。
///
/// 安全边界：仅允许删除白名单内的历史标识符目录；先完整备份并校验，通过后才删除源目录；
/// 备份失败则不删除任何数据。返回备份目录路径供界面告知用户。
#[tauri::command]
pub fn remove_legacy_data_dir(state: State<'_, AppDataDir>, path: String) -> AppResult<String> {
    let current = state.0.lock().unwrap().clone();
    let target = std::path::PathBuf::from(&path);

    // 额外守卫（只对**非白名单**路径生效）：新版那边还没有任何数据时，不允许删掉
    // 用户手选的旧目录——否则用户等于把剪贴板历史从应用会读的位置彻底抹掉。
    // 白名单内的历史标识符目录保持既有行为不变，避免影响原本就存在的清理流程。
    let is_whitelisted = crate::migration_identifier::legacy_dirs_for(&current)
        .iter()
        .any(|p| p == &target);
    if !is_whitelisted {
        can_remove_source_safely(&current).map_err(AppError::Validation)?;
    }

    crate::migration_identifier::backup_and_remove_legacy_dir(&current, &target)
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

    // 3) settings 必须与"刚装好的新版"完全一致。
    let baseline = build_seed_settings_baseline(&probe_dir.join("baseline.db"));
    let actual = read_settings(&probe_db);
    let pristine = match (baseline, actual) {
        (Some(base), Some(act)) => act == base,
        _ => false, // 无法判定 → 按"在用"处理
    };

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

    let outcome =
        crate::migration_identifier::migrate_from_source_dir(&source, &current, takeover);
    let report = crate::app::apply_identifier_migration(&source, &current, outcome);
    match report.status.as_str() {
        "migrated" => crate::info!(
            ">>> [MIGRATION] 手动迁移完成：源 {:?} 已复制 {} 项 / {} 字节到 {:?}；源目录未被改动，可重复验证。",
            report.source,
            report.files,
            report.bytes,
            report.target
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

#[tauri::command]
pub fn toggle_autostart(enabled: bool) -> AppResult<()> {
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
        key.set_value("Tiez-Next", &cmd)
            .map_err(|e| AppError::Internal(e.to_string()))?;
    } else {
        let _ = key.delete_value("Tiez-Next");
        let _ = key.delete_value("TieZ");
        let _ = key.delete_value("tie-z");
    }
    Ok(())
}

#[tauri::command]
pub fn is_autostart_enabled() -> AppResult<bool> {
    use winreg::enums::*;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu
        .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(key.get_value::<String, _>("Tiez-Next").is_ok()
        || key.get_value::<String, _>("TieZ").is_ok()
        || key.get_value::<String, _>("tie-z").is_ok())
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

#[tauri::command]
pub fn restart_explorer() -> AppResult<()> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    let _ = Command::new("cmd")
        .args(["/C", "taskkill /F /IM explorer.exe & start explorer.exe"])
        .creation_flags(0x08000000)
        .spawn();
    Ok(())
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
    ///
    /// 用"与全新种子库逐键逐值比对"实现，因此这里改一个已存在的 key 即可触发。
    #[test]
    fn customized_setting_makes_target_non_pristine() {
        let root = tmp_root("setting");
        let dir = seeded_dir(&root);
        assert!(target_db_is_pristine(&dir), "前提：先确认基线是空库");

        let conn = rusqlite::Connection::open(dir.join("clipboard.db")).unwrap();
        conn.execute(
            "UPDATE settings SET value = 'definitely-not-the-default' WHERE key = 'app.theme'",
            [],
        )
        .unwrap();
        drop(conn);

        assert!(
            !target_db_is_pristine(&dir),
            "用户改过设置时绝不能判定为空库"
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
