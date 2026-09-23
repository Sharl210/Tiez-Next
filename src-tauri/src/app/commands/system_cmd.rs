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

/// 判定某个数据目录里的数据库是不是**从未使用过的空库**（0 条剪贴板记录）。
///
/// 放在命令层而不是 `migration_identifier` 里，是因为它需要读 SQLite（rusqlite），
/// 而后者按设计**只依赖 `std`**，以便脱离 Tauri 与平台专用代码独立编译验证。判据本身
/// 很简单：能打开、有 `clipboard_history` 表、且条数为 0。
///
/// 实现上**先把库连同 WAL 侧车复制到临时目录再查**，而不是直接打开目标库：实测
/// （`migration_identifier` 的回归测试抓出）即便以只读方式打开，SQLite 也会改写目标
/// 的 `clipboard.db-shm`。那个副作用会干扰正在运行的应用，也让"这次检查没动过任何
/// 东西"不再成立。
///
/// 保守性：库存在但复制失败、打不开或没有该表时一律返回 `false`（按"有数据"处理）。
/// 宁可少迁，不可覆盖。
fn target_db_is_pristine(target: &std::path::Path) -> bool {
    let db = target.join("clipboard.db");
    if !db.is_file() {
        return true; // 连库都没有 = 完全没用过
    }

    let probe_dir = std::env::temp_dir().join(format!(
        "tiez-db-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    if std::fs::create_dir_all(&probe_dir).is_err() {
        return false;
    }
    for suffix in ["", "-wal", "-shm"] {
        let name = format!("clipboard.db{}", suffix);
        let from = target.join(&name);
        if from.is_file() && std::fs::copy(&from, probe_dir.join(&name)).is_err() {
            let _ = std::fs::remove_dir_all(&probe_dir);
            return false;
        }
    }

    let count: Option<i64> = rusqlite::Connection::open(probe_dir.join("clipboard.db"))
        .ok()
        .and_then(|conn| {
            conn.query_row("SELECT COUNT(*) FROM clipboard_history", [], |r| r.get(0))
                .ok()
        });
    let _ = std::fs::remove_dir_all(&probe_dir); // 探针目录始终清理；目标目录从未被写过

    matches!(count, Some(0))
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
