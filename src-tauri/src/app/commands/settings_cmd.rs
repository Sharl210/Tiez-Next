use crate::app_state::SettingsState;
use crate::database::DbState;
use crate::error::{AppError, AppResult};
use crate::infrastructure::repository::settings_repo::SettingsRepository;
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Manager, State};

fn normalize_quick_paste_modifier(value: &str) -> &'static str {
    match value.trim().to_ascii_lowercase().as_str() {
        "disabled" => "disabled",
        "ctrl" => "ctrl",
        "alt" => "alt",
        "shift" => "shift",
        "win" => "win",
        _ => "disabled",
    }
}

#[tauri::command]
pub fn set_sequential_mode(
    app_handle: AppHandle,
    state: State<'_, crate::app_state::SettingsState>,
    enabled: bool,
) {
    state.sequential_mode.store(enabled, Ordering::Relaxed);
    let db_state = app_handle.state::<DbState>();
    let _ = db_state
        .settings_repo
        .set("app.sequential_mode", &enabled.to_string());
    let _ = crate::app::commands::hotkey_cmd::sync_registered_hotkeys(&app_handle);
}

#[tauri::command]
pub fn set_sequential_hotkey(
    app_handle: AppHandle,
    state: State<'_, SettingsState>,
    hotkey: String,
) -> AppResult<()> {
    if let Ok(mut guard) = state.sequential_paste_hotkey.lock() {
        *guard = hotkey.clone();
    }

    let db_state = app_handle.state::<DbState>();
    db_state
        .settings_repo
        .set("app.sequential_hotkey", &hotkey)
        .map_err(AppError::from)?;
    crate::app::commands::hotkey_cmd::sync_registered_hotkeys(&app_handle)
}

#[tauri::command]
pub fn set_rich_paste_hotkey(
    app_handle: AppHandle,
    state: State<'_, SettingsState>,
    hotkey: String,
) -> AppResult<()> {
    if let Ok(mut guard) = state.rich_paste_hotkey.lock() {
        *guard = hotkey.clone();
    }

    let db_state = app_handle.state::<DbState>();
    db_state
        .settings_repo
        .set("app.rich_paste_hotkey", &hotkey)
        .map_err(AppError::from)?;
    crate::app::commands::hotkey_cmd::sync_registered_hotkeys(&app_handle)
}

/// 数字快速粘贴的修饰键（`app.quick_paste_modifier`）。
///
/// 【为什么单独开一条命令，而不是让界面改调 `save_setting`】`save_setting` 本来就认识
/// 这个键（本文件里已有 `"app.quick_paste_modifier"` 分支：归一化后同时写内存态与设置库），
/// 而界面已经按 `set_quick_paste_modifier` 这个名字在调——命令名不在 `generate_handler!` 里，
/// 于是设置页选完下拉框什么都不发生。
///
/// 【为什么实现体是转调 `save_setting` 而不是再写一遍】归一化规则
/// （`normalize_quick_paste_modifier`）与"内存态 + 设置库同时写"这套动作只应有一处定义。
/// 这里直接复用，日后改规则不会漏改一边；键、存储表、写路径都与 `save_setting` 完全相同，
/// **没有第二套存储**。
#[tauri::command]
pub fn set_quick_paste_modifier(
    app_handle: AppHandle,
    state: State<'_, SettingsState>,
    modifier: String,
) -> AppResult<()> {
    let db_state = app_handle.state::<DbState>();
    save_setting(
        db_state,
        state,
        "app.quick_paste_modifier".to_string(),
        modifier,
    )
}

#[tauri::command]
pub fn set_search_hotkey(
    app_handle: AppHandle,
    state: State<'_, SettingsState>,
    hotkey: String,
) -> AppResult<()> {
    if let Ok(mut guard) = state.search_hotkey.lock() {
        *guard = hotkey.clone();
    }

    let db_state = app_handle.state::<DbState>();
    db_state
        .settings_repo
        .set("app.search_hotkey", &hotkey)
        .map_err(AppError::from)?;
    crate::app::commands::hotkey_cmd::sync_registered_hotkeys(&app_handle)
}

#[tauri::command]
pub fn set_deduplication(
    app_handle: AppHandle,
    state: State<'_, crate::app_state::SettingsState>,
    enabled: bool,
) {
    state.deduplicate.store(enabled, Ordering::Relaxed);
    let db_state = app_handle.state::<DbState>();
    let _ = db_state
        .settings_repo
        .set("app.deduplicate", &enabled.to_string());
}

#[tauri::command]
pub fn save_setting(
    db_state: State<'_, DbState>,
    settings_state: State<'_, crate::app_state::SettingsState>,
    key: String,
    mut value: String,
) -> AppResult<()> {
    match key.as_str() {
        "app.arrow_key_selection" => {
            settings_state
                .arrow_key_selection
                .store(value == "true", Ordering::Relaxed);
        }
        "app.sequential_mode" => {
            settings_state
                .sequential_mode
                .store(value == "true", Ordering::Relaxed);
        }
        "app.sound_enabled" => {
            settings_state
                .sound_enabled
                .store(value == "true", Ordering::Relaxed);
        }
        "app.sound_paste_enabled" => {
            settings_state
                .delete_after_paste
                .store(value != "false", Ordering::Relaxed);
        }
        "app.persistent" => {
            settings_state
                .persistent
                .store(value != "false", Ordering::Relaxed);
        }
        "app.capture_files" => {
            settings_state
                .capture_files
                .store(value != "false", Ordering::Relaxed);
        }
        "app.capture_rich_text" => {
            settings_state
                .capture_rich_text
                .store(value == "true", Ordering::Relaxed);
        }
        "app.silent_start" => {
            settings_state
                .silent_start
                .store(value != "false", Ordering::Relaxed);
        }
        "app.delete_after_paste" => {
            settings_state
                .delete_after_paste
                .store(value == "true", Ordering::Relaxed);
        }
        "app.privacy_protection" => {
            settings_state
                .privacy_protection
                .store(value == "true", Ordering::Relaxed);
        }
        "app.edge_docking" => {
            settings_state
                .edge_docking
                .store(value == "true", Ordering::Relaxed);
        }
        "app.follow_mouse" => {
            settings_state
                .follow_mouse
                .store(value != "false", Ordering::Relaxed);
        }
        "app.hide_tray_icon" => {
            settings_state
                .hide_tray_icon
                .store(value == "true", Ordering::Relaxed);
        }
        "app.quick_paste_modifier" => {
            value = normalize_quick_paste_modifier(&value).to_string();
            if let Ok(mut guard) = settings_state.quick_paste_modifier.lock() {
                *guard = value.clone();
            }
        }
        _ => {}
    }

    db_state
        .settings_repo
        .set(&key, &value)
        .map_err(AppError::from)
}

#[tauri::command]
pub fn set_ignore_blur(ignore: bool) {
    crate::IGNORE_BLUR.store(ignore, Ordering::Relaxed);
}

#[tauri::command]
pub fn set_window_pinned(app_handle: AppHandle, state: State<'_, DbState>, pinned: bool) {
    crate::WINDOW_PINNED.store(pinned, Ordering::Relaxed);
    if let Some(window) = app_handle.get_webview_window("main") {
        let _ = window.set_always_on_top(pinned);
        let _ = window.set_focusable(false);
        #[cfg(windows)]
        {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::WindowsAndMessaging::{
                GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE,
            };
            if let Ok(hwnd) = window.hwnd() {
                unsafe {
                    let ex_style = GetWindowLongPtrW(HWND(hwnd.0), GWL_EXSTYLE);
                    let _ = SetWindowLongPtrW(
                        HWND(hwnd.0),
                        GWL_EXSTYLE,
                        ex_style | WS_EX_NOACTIVATE.0 as isize,
                    );
                }
            }
        }
    }
    let _ = state
        .settings_repo
        .set("app.window_pinned", &pinned.to_string());
}

#[tauri::command]
pub fn get_settings(
    state: State<'_, DbState>,
) -> AppResult<std::collections::HashMap<String, String>> {
    state.settings_repo.get_all().map_err(AppError::from)
}

/// 粘贴方案是否真的会在当前权限下生效。
///
/// ## 为什么必须有这个命令（而不只是启动时改一次设置）
///
/// 「游戏模式」需要管理员权限。旧实现的做法是：启动时发现未提权就**把用户设置改回
/// 默认方案**——于是用户永远不知道自己的选择被改掉了，只看到"设置保存不住"。
///
/// 现在后端只回答事实（配置了什么、是否已提权、在当前权限下是否生效），
/// 由界面把"未生效"和一个一键提权重启入口摆在用户面前。**本命令是只读的**：
/// 它不会写 `app.paste_method`，也不会改任何其他设置。
#[tauri::command]
pub fn get_paste_method_status(state: State<'_, DbState>) -> AppResult<PasteMethodStatusPayload> {
    let method = state
        .settings_repo
        .get("app.paste_method")
        .unwrap_or(Some("shift_insert".to_string()))
        .unwrap_or_else(|| "shift_insert".to_string());
    let status = crate::app::setup::paste_method_status_from(
        &method,
        crate::app::commands::system_cmd::check_is_admin(),
    );
    Ok(PasteMethodStatusPayload {
        method: status.method,
        is_admin: status.is_admin,
        effective: status.effective,
        requires_admin: status.requires_admin,
    })
}

/// `get_paste_method_status` 的线上形状（camelCase，与前端 `invoke` 返回类型一致）。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PasteMethodStatusPayload {
    pub method: String,
    pub is_admin: bool,
    pub effective: bool,
    pub requires_admin: bool,
}

#[tauri::command]
pub fn set_file_server_auto_close(
    state: State<'_, crate::app_state::SettingsState>,
    db_state: State<'_, DbState>,
    enabled: bool,
) -> AppResult<()> {
    state
        .file_server_auto_close
        .store(enabled, Ordering::Relaxed);
    db_state
        .settings_repo
        .set("file_transfer_auto_close", &enabled.to_string())
        .map_err(AppError::from)
}

#[tauri::command]
pub fn set_file_transfer_auto_open(db_state: State<'_, DbState>, enabled: bool) -> AppResult<()> {
    db_state
        .settings_repo
        .set("file_transfer_auto_open", &enabled.to_string())
        .map_err(AppError::from)
}

#[tauri::command]
pub fn set_arrow_key_selection(
    state: State<'_, crate::app_state::SettingsState>,
    enabled: bool,
) -> AppResult<()> {
    state.arrow_key_selection.store(enabled, Ordering::Relaxed);
    Ok(())
}

#[tauri::command]
pub fn set_persistence(
    state: State<'_, crate::app_state::SettingsState>,
    db_state: State<'_, DbState>,
    enabled: bool,
) -> AppResult<()> {
    state.persistent.store(enabled, Ordering::Relaxed);
    db_state
        .settings_repo
        .set("app.persistent", &enabled.to_string())
        .map_err(AppError::from)
}

#[tauri::command]
pub fn set_capture_files(
    state: State<'_, crate::app_state::SettingsState>,
    db_state: State<'_, DbState>,
    enabled: bool,
) -> AppResult<()> {
    state.capture_files.store(enabled, Ordering::Relaxed);
    db_state
        .settings_repo
        .set("app.capture_files", &enabled.to_string())
        .map_err(AppError::from)
}

#[tauri::command]
pub fn set_capture_rich_text(
    state: State<'_, crate::app_state::SettingsState>,
    db_state: State<'_, DbState>,
    enabled: bool,
) -> AppResult<()> {
    state.capture_rich_text.store(enabled, Ordering::Relaxed);
    db_state
        .settings_repo
        .set("app.capture_rich_text", &enabled.to_string())
        .map_err(AppError::from)
}

#[tauri::command]
pub fn set_auto_copy_file(
    state: State<'_, crate::app_state::SettingsState>,
    db_state: State<'_, DbState>,
    enabled: bool,
) -> AppResult<()> {
    state.auto_copy_file.store(enabled, Ordering::Relaxed);
    db_state
        .settings_repo
        .set(
            "file_transfer_auto_copy",
            if enabled { "true" } else { "false" },
        )
        .map_err(AppError::from)
}

#[tauri::command]
pub fn set_silent_start(
    state: State<'_, crate::app_state::SettingsState>,
    db_state: State<'_, DbState>,
    enabled: bool,
) -> AppResult<()> {
    state.silent_start.store(enabled, Ordering::Relaxed);
    db_state
        .settings_repo
        .set("app.silent_start", &enabled.to_string())
        .map_err(AppError::from)
}

#[tauri::command]
pub fn set_delete_after_paste(
    state: State<'_, crate::app_state::SettingsState>,
    db_state: State<'_, DbState>,
    enabled: bool,
) -> AppResult<()> {
    state.delete_after_paste.store(enabled, Ordering::Relaxed);
    db_state
        .settings_repo
        .set("app.delete_after_paste", &enabled.to_string())
        .map_err(AppError::from)
}

#[tauri::command]
pub fn set_privacy_protection(
    state: State<'_, crate::app_state::SettingsState>,
    db_state: State<'_, DbState>,
    enabled: bool,
) -> AppResult<()> {
    state.privacy_protection.store(enabled, Ordering::Relaxed);
    db_state
        .settings_repo
        .set("app.privacy_protection", &enabled.to_string())
        .map_err(AppError::from)
}

#[tauri::command]
pub fn set_privacy_protection_kinds(
    state: State<'_, crate::app_state::SettingsState>,
    db_state: State<'_, DbState>,
    kinds: Vec<String>,
) -> AppResult<()> {
    let mut guard = state.privacy_protection_kinds.lock().unwrap();
    *guard = kinds.clone();
    let serialized = kinds.join(",");
    db_state
        .settings_repo
        .set("app.privacy_protection_kinds", &serialized)
        .map_err(AppError::from)
}

#[tauri::command]
pub fn set_privacy_protection_custom_rules(
    state: State<'_, crate::app_state::SettingsState>,
    db_state: State<'_, DbState>,
    rules: String,
) -> AppResult<()> {
    let list = rules
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    let mut guard = state.privacy_protection_custom_rules.lock().unwrap();
    *guard = list;
    db_state
        .settings_repo
        .set("app.privacy_protection_custom_rules", &rules)
        .map_err(AppError::from)
}

#[tauri::command]
pub fn set_cleanup_rules(
    state: State<'_, crate::app_state::SettingsState>,
    db_state: State<'_, DbState>,
    rules: String,
) -> AppResult<()> {
    let mut guard = state.cleanup_rules.lock().unwrap();
    *guard = rules.clone();
    db_state
        .settings_repo
        .set("app.cleanup_rules", &rules)
        .map_err(AppError::from)
}

#[tauri::command]
pub fn set_app_cleanup_policies(
    state: State<'_, crate::app_state::SettingsState>,
    db_state: State<'_, DbState>,
    policies: String,
) -> AppResult<()> {
    let mut guard = state.app_cleanup_policies.lock().unwrap();
    *guard = policies.clone();
    db_state
        .settings_repo
        .set("app.app_cleanup_policies", &policies)
        .map_err(AppError::from)
}

#[tauri::command]
pub fn set_sound_enabled(
    state: State<'_, crate::app_state::SettingsState>,
    db_state: State<'_, DbState>,
    enabled: bool,
) -> AppResult<()> {
    state.sound_enabled.store(enabled, Ordering::Relaxed);
    db_state
        .settings_repo
        .set("app.sound_enabled", &enabled.to_string())
        .map_err(AppError::from)
}

#[tauri::command]
pub fn get_mqtt_status() -> bool {
    crate::services::mqtt_sub::get_mqtt_status()
}

#[tauri::command]
pub fn get_mqtt_running() -> bool {
    crate::services::mqtt_sub::get_mqtt_running()
}

#[tauri::command]
pub fn restart_mqtt_client(app_handle: AppHandle) {
    crate::services::mqtt_sub::restart_mqtt_client(app_handle)
}

#[tauri::command]
pub fn get_cloud_sync_status() -> crate::services::cloud_sync::CloudSyncStatus {
    crate::services::cloud_sync::get_cloud_sync_status()
}

#[tauri::command]
pub fn restart_cloud_sync_client(app_handle: AppHandle) {
    crate::services::cloud_sync::restart_cloud_sync_client(app_handle);
}

#[tauri::command]
pub fn request_cloud_sync(app_handle: AppHandle) {
    crate::services::cloud_sync::request_cloud_sync(app_handle);
}

/// 停止云同步后台循环。
///
/// 【为什么必须是命令而不是只写设置】用户关掉「云同步」开关时，正在跑的那轮同步不会
/// 因为库里 `cloud_sync_enabled=false` 而中断——它已经读了配置、正在上传。界面因此先
/// 调本命令取消当前轮，再落设置（见 `src/shared/hooks/useAppActions.ts`）。
#[tauri::command]
pub fn stop_cloud_sync_client(app_handle: AppHandle) {
    crate::services::cloud_sync::stop_cloud_sync_client(app_handle);
}

#[tauri::command]
pub async fn cloud_sync_now(
    app_handle: AppHandle,
) -> AppResult<crate::services::cloud_sync::CloudSyncStatus> {
    crate::services::cloud_sync::cloud_sync_now(app_handle).await
}

/// 查询"本机是否曾经可能把凭据随云同步快照上传过"，即**要不要向用户提示**。
///
/// # 为什么是只读查询 + 单独一个确认命令，而不是一个"查完顺便标记"的命令
///
/// 提示的送达与否不该由"前端调过一次只读查询"决定：渲染失败、用户没看见就被关掉、
/// 窗口崩了，都会让标记白白落下，而**看不见的安全告知等于没有告知**。因此拆成两步：
/// 本命令只读；真正落标记只在用户点了"知道了"（或主动跳去改设置）时发生。
///
/// 代价是前端必须真的把标记写下去（`mark_credential_exposure_notice_seen`）。宁可在
/// 极端情况下重复提示一次，也不要静默丢掉一次。
#[tauri::command]
pub fn get_credential_exposure_notice(
    state: State<'_, DbState>,
) -> AppResult<crate::services::cloud_sync::CredentialExposureNotice> {
    crate::services::cloud_sync::assess_credential_exposure_from_repo(&state.settings_repo)
}

/// 记下"这条告知已经展示给用户了"，此后不再提示。
#[tauri::command]
pub fn mark_credential_exposure_notice_seen(
    state: State<'_, DbState>,
) -> AppResult<()> {
    crate::services::cloud_sync::mark_credential_exposure_acknowledged(&state.settings_repo)
}

/// 重置设置时**必须原样保留**的设置键前缀。
///
/// # 为什么 `mcp.*` 不能被重置抹掉
///
/// `reset_settings` 的动作是"清空 settings 表 → 重新种入出厂默认值"，而
/// [`crate::database::seed_defaults`] 里**一个 `mcp.*` 键都没有**。于是重置会：
///
/// 1. 删掉 `mcp.token`（用户已经配进 MCP 客户端的唯一凭据）；
/// 2. 删掉 `mcp.allow_write` / `mcp.require_token` / `mcp.allow_lan` / `mcp.port` /
///    `mcp.autostart`，让这些键回落到 `services::mcp::store` 的出厂默认值；
/// 3. **不通知正在运行的服务实例**：`push_allow_write_to_running_service` /
///    `push_require_token_to_running_service` 都不在这里被调用，令牌也不会重新生成。
///
/// 第 3 条是最危险的一条：重置之后**数据库里是一套姿态，正在跑的服务是另一套**。
/// 用户点完"重置设置"当场看不到任何变化（服务仍按旧姿态服务），直到下次重启应用才
/// 突然发现 MCP 的端口/写权限/鉴权全变了——这是"惊喜式"的姿态翻转。
///
/// # 为什么选择"保留键"而不是"重置后强制推送运行态 + 重新生成令牌"
///
/// 两条路都能解决"库与运行态不一致"，但代价不同：
///
/// * **重新生成令牌会静默打断用户已配置的每一个 MCP 客户端**。用户按设置界面的提示
///   把入口与令牌写进了 IDE/客户端配置，令牌一换，那些客户端只会开始连不上，而
///   "重置设置"这个按钮的语义里**没有任何一处**暗示它会动 MCP。
/// * 强制推送运行态还需要重启服务（端口与监听地址在 bind 时就定了），而"重置设置"是
///   同步命令，做不了这件事——那就会退回"重置后要重启才一致"，正是要消除的东西。
/// * 保留键让数据库与运行态**根本不需要对齐**：`mcp.*` 一行没动，运行中的服务读到的
///   仍是同一套值，不存在需要推送的差异。
///
/// 因此这里选"保留"。代价是重置不再把 MCP 恢复出厂——但出厂姿态本身是**用户已经做过
/// 的选择**，不该被另一个按钮顺手推翻。
const PRESERVED_PREFIXES_ON_RESET: &[&str] = &["mcp."];

/// 快照所有需要跨过 `clear()` 保留下来的设置键值。
///
/// 抽成独立函数是为了让它可被单元测试直接驱动：命令函数需要 `AppHandle`，单测里构造
/// 不出来，而"保留逻辑是否真的生效"正是这条修复的全部内容。
fn snapshot_preserved_settings(
    repo: &impl SettingsRepository,
) -> AppResult<Vec<(String, String)>> {
    let all = repo.get_all().map_err(AppError::from)?;
    let mut kept: Vec<(String, String)> = all
        .into_iter()
        .filter(|(k, _)| {
            PRESERVED_PREFIXES_ON_RESET
                .iter()
                .any(|p| k.starts_with(p))
        })
        .collect();
    // `HashMap` 的迭代顺序不稳定；排序让"到底保留了哪几条"可复现、可断言。
    kept.sort();
    Ok(kept)
}

/// 把快照写回设置表。与 [`snapshot_preserved_settings`] 配对，同样抽出来以便单测。
fn restore_preserved_settings(
    repo: &impl SettingsRepository,
    kept: &[(String, String)],
) -> AppResult<()> {
    for (key, value) in kept {
        repo.set(key, value).map_err(AppError::from)?;
    }
    Ok(())
}

#[tauri::command]
pub fn reset_settings(
    app: AppHandle,
    state: State<'_, DbState>,
    settings_state: State<'_, crate::app_state::SettingsState>,
) -> AppResult<()> {
    use crate::database::seed_defaults;

    // 先取快照再清库：`clear()` 会把 `mcp.token` 一起删掉，而它是用户已经配进
    // MCP 客户端的凭据——重新生成一个等价的随机串**不可能**与原值相同。
    let preserved = snapshot_preserved_settings(&state.settings_repo)?;

    state.settings_repo.clear().map_err(AppError::from)?;
    {
        let conn = state.conn.lock().unwrap();
        seed_defaults(&conn).map_err(AppError::from)?;
    }

    // 放在 `seed_defaults` 之后回填：那个函数用的是 `INSERT OR IGNORE`，两者不会互相
    // 覆盖，但先种默认再回填让"保留值优先"这件事只依赖一处顺序。
    restore_preserved_settings(&state.settings_repo, &preserved)?;

    let machine_id = crate::app::system::get_machine_id();
    let new_id = format!("{}-0000-0000-0000-000000000000", machine_id);
    state
        .settings_repo
        .set("app.anon_id", &new_id)
        .map_err(AppError::from)?;

    let main_hotkey = state
        .settings_repo
        .get("app.hotkey")
        .unwrap_or(Some("Alt+C".to_string()))
        .unwrap_or("Alt+C".to_string());
    let sequential_mode = state
        .settings_repo
        .get("app.sequential_mode")
        .unwrap_or(Some("false".to_string()))
        .map(|v| v == "true")
        .unwrap_or(false);
    let seq_hotkey = state
        .settings_repo
        .get("app.sequential_hotkey")
        .unwrap_or(Some("Alt+V".to_string()))
        .unwrap_or("Alt+V".to_string());
    let rich_hotkey = state
        .settings_repo
        .get("app.rich_paste_hotkey")
        .unwrap_or(Some("Ctrl+Shift+Z".to_string()))
        .unwrap_or("Ctrl+Shift+Z".to_string());
    let search_hotkey = state
        .settings_repo
        .get("app.search_hotkey")
        .unwrap_or(Some("Alt+F".to_string()))
        .unwrap_or("Alt+F".to_string());
    let quick_paste_modifier = state
        .settings_repo
        .get("app.quick_paste_modifier")
        .unwrap_or(Some("disabled".to_string()))
        .unwrap_or("disabled".to_string());

    settings_state
        .sequential_mode
        .store(sequential_mode, Ordering::Relaxed);
    {
        let mut guard = settings_state.main_hotkey.lock().unwrap();
        *guard = main_hotkey.clone();
    }
    {
        let mut guard = settings_state.sequential_paste_hotkey.lock().unwrap();
        *guard = seq_hotkey.clone();
    }
    {
        let mut guard = settings_state.rich_paste_hotkey.lock().unwrap();
        *guard = rich_hotkey.clone();
    }
    {
        let mut guard = settings_state.search_hotkey.lock().unwrap();
        *guard = search_hotkey.clone();
    }
    {
        let mut guard = settings_state.quick_paste_modifier.lock().unwrap();
        *guard = normalize_quick_paste_modifier(&quick_paste_modifier).to_string();
    }
    {
        let mut guard = crate::global_state::HOTKEY_STRING.lock().unwrap();
        *guard = main_hotkey.clone();
    }

    crate::app::commands::hotkey_cmd::sync_registered_hotkeys(&app)
}

#[tauri::command]
pub fn set_tray_visible(
    app_handle: AppHandle,
    state: State<'_, crate::app_state::SettingsState>,
    visible: bool,
) -> AppResult<()> {
    state.hide_tray_icon.store(!visible, Ordering::Relaxed);
    if let Some(tray) = app_handle.tray_by_id("main_tray") {
        let _ = tray.set_visible(visible);
    }
    let db_state = app_handle.state::<DbState>();
    db_state
        .settings_repo
        .set("app.hide_tray_icon", &(!visible).to_string())
        .map_err(AppError::from)
}

/// 隐藏 / 显示 macOS 的 Dock 图标，并把选择存进设置库。
///
/// 【为什么存的是 `app.hide_dock_icon` 而参数叫 `visible`】用户看到的是"隐藏 Dock 图标"
/// 这个开关，`visible` 是命令参数的方向；库里沿用既有键（默认 `false`），启动时用同一个
/// 值复原，不需要第二套存储。
///
/// 【非 macOS 平台】Dock 概念不存在，这里只落设置、不动系统。命令本身全平台注册，
/// 免得前端在非 macOS 上拿到 "command not found"。
#[tauri::command]
pub fn set_dock_visible(app_handle: AppHandle, visible: bool) -> AppResult<()> {
    #[cfg(target_os = "macos")]
    {
        app_handle
            .set_dock_visibility(visible)
            .map_err(AppError::from)?;
    }

    let db_state = app_handle.state::<DbState>();
    db_state
        .settings_repo
        .set("app.hide_dock_icon", &(!visible).to_string())
        .map_err(AppError::from)
}

#[tauri::command]
pub fn set_edge_docking(
    app_handle: AppHandle,
    state: State<'_, crate::app_state::SettingsState>,
    enabled: bool,
) -> AppResult<()> {
    state.edge_docking.store(enabled, Ordering::Relaxed);
    let db_state = app_handle.state::<DbState>();
    db_state
        .settings_repo
        .set("app.edge_docking", &enabled.to_string())
        .map_err(AppError::from)
}

#[tauri::command]
pub fn set_follow_mouse(
    app_handle: AppHandle,
    state: State<'_, crate::app_state::SettingsState>,
    enabled: bool,
) -> AppResult<()> {
    state.follow_mouse.store(enabled, Ordering::Relaxed);
    let db_state = app_handle.state::<DbState>();
    db_state
        .settings_repo
        .set("app.follow_mouse", &enabled.to_string())
        .map_err(AppError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::repository::settings_repo::SqliteSettingsRepository;
    use std::sync::{Arc, Mutex};

    /// 造一个只有 `settings` 表的内存库，接上真实的 `SqliteSettingsRepository`。
    ///
    /// 用真仓储而不是假实现：这条修复要证明的正是"真读写路径下 `mcp.*` 会活下来"，
    /// 换成一个记录调用的假仓储就把被测对象换掉了。
    fn repo() -> (Arc<Mutex<rusqlite::Connection>>, SqliteSettingsRepository) {
        let conn = Arc::new(Mutex::new(
            rusqlite::Connection::open_in_memory().expect("内存库应可创建"),
        ));
        conn.lock()
            .expect("连接锁可用")
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
            )
            .expect("settings 表应可创建");
        let repo = SqliteSettingsRepository::new(conn.clone());
        (conn, repo)
    }

    /// 命令层走的是**真实读取路径**：真实 `SqliteSettingsRepository` → `get_all()` →
    /// 判定。`cloud_sync.rs` 里的测试喂的是手搓 HashMap，证明不了"从库里真读对了键"。
    ///
    /// 这一组同时覆盖用户要求的两条主断言：**确实可能受影响必须提示**、
    /// **肯定没受影响不得提示**，以及**一次性**（落标记后不再提示）。
    #[test]
    fn credential_exposure_notice_reads_the_real_settings_table() {
        use crate::services::cloud_sync::{
            assess_credential_exposure_from_repo, mark_credential_exposure_acknowledged,
            ExposureEvidence, CREDENTIAL_EXPOSURE_ACK_KEY,
        };

        let (_conn, repo) = repo();

        // ① 空表（等价于"从未配置过云同步"）⇒ 不得提示。
        let notice = assess_credential_exposure_from_repo(&repo).expect("读取设置应成功");
        assert!(
            !notice.should_notify,
            "从未配置过云同步的机器不得看到这条安全警告"
        );
        assert_eq!(notice.evidence, ExposureEvidence::NotConfigured);

        // ② 模拟"配好并成功推送过设置快照"⇒ 必须提示，且依据是最强的那一档。
        repo.set("cloud_sync_enabled", "true").unwrap();
        repo.set("cloud_sync_webdav_last_snapshot_push_at", "1760000000000")
            .unwrap();
        // 三个被外流的键里放一个，验证"文案材料"这条路真的通到了命令层。
        repo.set("mqtt_username", "mqtt-user-a").unwrap();

        let notice = assess_credential_exposure_from_repo(&repo).expect("读取设置应成功");
        assert!(
            notice.should_notify,
            "推送过设置快照的机器必须被告知：那 3 项凭据已经在它自己的云端存储里了"
        );
        assert_eq!(notice.evidence, ExposureEvidence::ConfirmedSyncHistory);
        assert_eq!(notice.stored_credential_keys, vec!["mqtt_username".to_string()]);

        // ③ 用户点过"知道了"⇒ 此后不再提示，但判定依据不被改写。
        mark_credential_exposure_acknowledged(&repo).expect("标记应可写入");
        assert_eq!(
            repo.get(CREDENTIAL_EXPOSURE_ACK_KEY).unwrap().as_deref(),
            Some("true"),
            "标记必须真的落到设置表（否则下一次启动会重复提示）"
        );

        let notice = assess_credential_exposure_from_repo(&repo).expect("读取设置应成功");
        assert!(!notice.should_notify, "提示过一次后不得再提示");
        assert!(notice.acknowledged);
        assert_eq!(
            notice.evidence,
            ExposureEvidence::ConfirmedSyncHistory,
            "标记只影响是否提示，不得改变判定依据"
        );
    }

    /// 复现 `reset_settings` 的动作序列（快照 → 清空 → 种默认 → 回填）。
    ///
    /// 不直接调用命令函数：它需要 `AppHandle`（还要求一个活的 Tauri 环境）。这里把
    /// 命令体内**真正决定结果的四步**照原样执行一遍，因此"退回旧行为会变红"这件事
    /// 是可证的——旧的命令体里就没有第 4 步。
    fn reset_with_preservation(
        repo: &impl SettingsRepository,
        conn: &Arc<Mutex<rusqlite::Connection>>,
    ) -> AppResult<Vec<(String, String)>> {
        let preserved = snapshot_preserved_settings(repo)?;
        repo.clear().map_err(AppError::from)?;
        {
            let guard = conn.lock().unwrap();
            crate::database::seed_defaults(&guard).map_err(AppError::from)?;
        }
        restore_preserved_settings(repo, &preserved)?;
        Ok(preserved)
    }

    /// 反向对照用的**旧行为**：清空 + 种默认，不做任何保留。
    fn reset_without_preservation(
        repo: &impl SettingsRepository,
        conn: &Arc<Mutex<rusqlite::Connection>>,
    ) {
        repo.clear().unwrap();
        let guard = conn.lock().unwrap();
        crate::database::seed_defaults(&guard).unwrap();
    }

    /// 用户已经配好 MCP 的常见状态：令牌 + 一串非默认姿态。
    fn seed_configured_mcp(repo: &impl SettingsRepository) {
        repo.set("mcp.token", "configured-token-abc").unwrap();
        repo.set("mcp.allow_write", "false").unwrap();
        repo.set("mcp.require_token", "true").unwrap();
        repo.set("mcp.allow_lan", "true").unwrap();
        repo.set("mcp.port", "34567").unwrap();
        repo.set("mcp.autostart", "false").unwrap();
        repo.set("mcp.enabled", "true").unwrap();
        // 一个普通设置，用来证明重置**确实**把非 mcp 的设置清了（保留下沉为"全保留"）。
        repo.set("app.theme", "dark").unwrap();
        repo.set("app.persistent", "true").unwrap();
    }

    /// 重置设置必须原样保留 `mcp.*`（含令牌与安全姿态）。
    ///
    /// 【这条为什么重要】`seed_defaults` 里一个 `mcp.*` 键都没有，因此清库会让这些键
    /// 整体回落到 `store.rs` 的出厂默认值：`mcp.token` 直接消失（用户已配进 MCP 客户端
    /// 的凭据没了）、`mcp.require_token` 回落到 `false`（免鉴权）、`mcp.allow_lan` 回落
    /// 到 `false`。用户点一下"重置设置"就把 MCP 的安全姿态换了一套，而按钮上没有任何
    /// 提示。
    #[test]
    fn reset_settings_preserves_every_mcp_key() {
        let (conn, repo) = repo();
        seed_configured_mcp(&repo);
        let before: Vec<(String, String)> = {
            let mut v: Vec<(String, String)> = repo
                .get_all()
                .unwrap()
                .into_iter()
                .filter(|(k, _)| k.starts_with("mcp."))
                .collect();
            v.sort();
            v
        };
        assert_eq!(before.len(), 7, "前置：应有 7 个 mcp.* 键");

        reset_with_preservation(&repo, &conn).expect("重置应成功");

        let after: Vec<(String, String)> = {
            let mut v: Vec<(String, String)> = repo
                .get_all()
                .unwrap()
                .into_iter()
                .filter(|(k, _)| k.starts_with("mcp."))
                .collect();
            v.sort();
            v
        };
        assert_eq!(
            after, before,
            "mcp.* 必须在重置前后**逐键逐值**相同（含令牌）"
        );
        assert_eq!(
            repo.get("mcp.token").unwrap().as_deref(),
            Some("configured-token-abc"),
            "令牌若被换掉，用户已配置的每个 MCP 客户端都会静默失联"
        );
        assert_eq!(
            repo.get("mcp.require_token").unwrap().as_deref(),
            Some("true"),
            "鉴权姿态不该被重置悄悄关掉"
        );

        // 对照：普通设置确实被重置了（保留下沉为"什么都不清"是另一种缺陷）。
        assert_eq!(
            repo.get("app.theme").unwrap().as_deref(),
            Some("mica"),
            "非 mcp 设置应回到出厂默认值"
        );
    }

    /// **反向对照**：退回旧行为（不保留）时，上一条断言必须变红。
    ///
    /// 这条测试存在的意义是证明上一条真的在守着这条修复，而不是在守一个恒真命题——
    /// 如果 `seed_defaults` 将来加了 `mcp.*` 默认值，这里会红，提醒复核者换一种造数据
    /// 的方式，而不是让两条测试一起变成空转。
    #[test]
    fn without_preservation_the_mcp_keys_are_wiped_and_the_guard_would_fail() {
        let (conn, repo) = repo();
        seed_configured_mcp(&repo);

        reset_without_preservation(&repo, &conn);

        assert!(
            repo.get("mcp.token").unwrap().is_none(),
            "旧行为下令牌必然消失——这正是要修的缺陷；它仍然消失说明测试夹具没能复现缺陷"
        );
        assert_eq!(
            repo.get("mcp.allow_lan").unwrap(),
            None,
            "旧行为下局域网开关回落默认（仅本机）"
        );
        assert_eq!(
            repo.get("mcp.require_token").unwrap(),
            None,
            "旧行为下令牌校验回落默认（免鉴权）"
        );
    }

    /// 保留的是**前缀**而不是逐键列举：将来新增 `mcp.*` 键会自动被覆盖。
    ///
    /// 逐键列举有一个安静的失效模式——新增键漏登记不会有编译错误或测试失败，而
    /// `mcp.*` 正是一个还在增长的族（`allow_lan`、`autostart` 都是后来加的）。
    #[test]
    fn preservation_covers_unknown_future_mcp_keys() {
        let (conn, repo) = repo();
        repo.set("mcp.some_future_switch", "on").unwrap();

        reset_with_preservation(&repo, &conn).expect("重置应成功");

        assert_eq!(
            repo.get("mcp.some_future_switch").unwrap().as_deref(),
            Some("on"),
            "前缀保留必须覆盖将来新增的 mcp.* 键"
        );
    }

    /// 快照函数本身：只挑 `mcp.*`，且顺序稳定。
    #[test]
    fn snapshot_only_keeps_the_protected_prefixes() {
        let (_conn, repo) = repo();
        seed_configured_mcp(&repo);

        let kept = snapshot_preserved_settings(&repo).unwrap();

        assert!(kept.iter().all(|(k, _)| k.starts_with("mcp.")));
        assert_eq!(kept.len(), 7);
        let mut sorted = kept.clone();
        sorted.sort();
        assert_eq!(kept, sorted, "顺序必须稳定可复现");
    }
}
