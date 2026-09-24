#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

pub mod app;
pub mod app_state;
pub mod database;
pub mod domain;
pub mod error;
pub mod global_state;
pub mod infrastructure;
pub mod logger;
pub mod migration;
pub mod migration_identifier;
pub mod migration_pending;
pub mod services;

use crate::app::setup;
use crate::global_state::*;
use std::sync::atomic::Ordering;

fn main() {
    // 显式安装 rustls 的 crypto provider，防止 rumqttc 因缺少 provider 而 panic
    let _ = rustls::crypto::ring::default_provider().install_default();

    let _ = dotenvy::dotenv();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_sql::Builder::default().build())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state() == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                        setup::handle_global_shortcut(app, shortcut);
                    }
                })
                .build(),
        )
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .plugin(tauri_plugin_single_instance::init(|_app, _args, _cwd| {}))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        .plugin(tauri_plugin_http::init())
        .setup(|app| {
            setup::init(app)?;
            // 自动容灾备份服务（启动时一次 + 之后按周期）。
            //
            // 【为什么挂在 `main.rs` 而不是 `app/setup.rs`】`setup::init` 已经完成了
            // `AppDataDir` 与 `DbState` 的注册，因此这里能拿到数据目录与设置库；把接线放在
            // 调用方一侧，`setup.rs` 里那串"数据目录解析 → 日志 → 数据库 → 状态"的启动顺序
            // 保持原样不动。
            //
            // 它**不阻塞启动**：整件事（含"软件刚启动自动备份一次"）都在后台线程，窗口先出来。
            services::auto_backup::spawn_auto_backup_service(app.handle().clone());

            // macOS 的 Dock 图标：把用户上次的选择复原到系统。
            //
            // 【为什么必须在这里补一句】`set_dock_visible` 只负责"改的时候立刻生效 + 记住"，
            // 而 Dock 图标的状态不落盘在系统里——重启后应用会带着图标回来，用户会以为
            // 设置没保存。这正是 v0.5.3 那批"界面显示已生效、实际没生效"的同型缺陷。
            //
            // 放在 `setup::init` 之后：那时 `DbState` 已注册，能读到设置库。
            #[cfg(target_os = "macos")]
            {
                use crate::infrastructure::repository::settings_repo::SettingsRepository;
                use tauri::Manager;
                if let Some(db) = app.try_state::<database::DbState>() {
                    let hidden = db
                        .settings_repo
                        .get("app.hide_dock_icon")
                        .ok()
                        .flatten()
                        .map(|v| v == "true")
                        .unwrap_or(false);
                    if hidden {
                        let _ = app.handle().set_dock_visibility(false);
                    }
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app::window_manager::toggle_window_cmd,
            app::window_manager::hide_window_cmd,
            app::window_manager::activate_window_focus,
            app::window_manager::focus_clipboard_window,
            app::window_manager::set_navigation_enabled,
            app::window_manager::set_navigation_mode,
            // 「把焦点还给上一个前台窗口」：粘贴后回焦用（`src/shared/lib/focus.ts`）。
            // 命令本身一直存在且已加 `#[tauri::command]`，但从未登记到这里，
            // 于是搜索框每次失焦都只是往控制台丢一条 not found。
            app::window_manager::restore_last_focus,
            app::hooks::set_recording_mode,
            services::content_handler::open_content,
            services::clipboard_ops::copy_to_clipboard,
            services::clipboard_ops::paste_latest_rich,
            app::commands::get_clipboard_history,
            app::commands::search_clipboard_history,
            app::commands::delete_clipboard_entry,
            app::commands::clear_clipboard_history,
            app::commands::get_tag_items,
            app::commands::get_all_tags_info,
            app::commands::get_tag_stats,
            app::commands::rename_tag_globally,
            app::commands::delete_tag_from_all,
            app::commands::create_new_tag,
            app::commands::move_entry_to_tag,
            app::commands::copy_entry_to_tag,
            app::commands::update_pinned_order,
            app::commands::get_db_count,
            app::commands::get_clipboard_content,
            app::commands::set_sequential_mode,
            app::commands::set_sequential_hotkey,
            app::commands::set_rich_paste_hotkey,
            app::commands::set_search_hotkey,
            // 数字快速粘贴的修饰键。设置项、内存态与 Windows 钩子都早就实现了，
            // 唯独这条写入命令从没存在过，于是设置页选了下拉框不会有任何效果。
            app::commands::set_quick_paste_modifier,
            app::commands::set_deduplication,
            app::commands::save_setting,
            app::commands::set_ignore_blur,
            app::commands::set_window_pinned,
            app::commands::get_settings,
            app::commands::set_file_server_auto_close,
            app::commands::set_persistence,
            app::commands::set_capture_files,
            app::commands::set_capture_rich_text,
            app::commands::set_auto_copy_file,
            app::commands::set_silent_start,
            app::commands::set_delete_after_paste,
            app::commands::set_privacy_protection,
            app::commands::set_privacy_protection_kinds,
            app::commands::set_privacy_protection_custom_rules,
            app::commands::set_cleanup_rules,
            app::commands::set_app_cleanup_policies,
            app::commands::reset_settings,
            app::commands::get_mqtt_status,
            app::commands::get_mqtt_running,
            app::commands::restart_mqtt_client,
            app::commands::get_cloud_sync_status,
            app::commands::restart_cloud_sync_client,
            app::commands::request_cloud_sync,
            // 关闭云同步时先让后台循环退出：只写 `cloud_sync_enabled=false` 不会通知
            // 正在跑的循环。这个包装一直缺 `#[tauri::command]`，前端调它会直接报
            // "command not found"（见 `src/shared/hooks/useAppActions.ts`）。
            app::commands::stop_cloud_sync_client,
            app::commands::cloud_sync_now,
            // 存量凭据外流的升级告知：只读查询 + 用户确认后落一次性标记。
            // 拆成两条命令是刻意的——标记只在用户真的看到并确认后才写，
            // 避免"渲染失败但标记已落"导致告知静默丢失。
            app::commands::get_credential_exposure_notice,
            app::commands::mark_credential_exposure_notice_seen,
            app::commands::set_sound_enabled,
            app::commands::set_file_transfer_auto_open,
            app::commands::set_arrow_key_selection,
            app::commands::set_tray_visible,
            // macOS 专属：隐藏 Dock 图标。非 macOS 上只落设置、不做系统动作，
            // 界面也只在 macOS 渲染这个开关。
            app::commands::set_dock_visible,
            app::commands::set_edge_docking,
            app::commands::set_follow_mouse,
            app::commands::get_data_path,
            app::commands::open_folder,
            app::commands::open_data_folder,
            app::commands::list_legacy_data_dirs,
            app::commands::remove_legacy_data_dir,
            app::commands::migrate_from_data_dir,
            // 迁移进度的当前快照：前端先 `listen` 再 `invoke` 这个命令拉初值，
            // 否则"事件先于监听"的那一段会永久丢失，进度条卡在第一帧。
            app::commands::get_migration_progress,
            app::commands::backup_preflight,
            app::commands::suggest_backup_path,
            app::commands::reveal_path,
            app::commands::export_backup,
            app::commands::inspect_backup_package,
            app::commands::import_backup,
            // 自动容灾备份（定时 + 启动）：存储/轮换/固定层。
            // 与上面的手动导出/导入是两条不同的链：手动备份由用户选路径、不受份数与轮换
            // 约束；这里是应用自己保管的容灾副本，同目录、受上限与轮换约束。
            services::auto_backup::get_auto_backup_config,
            services::auto_backup::set_auto_backup_config,
            services::auto_backup::list_auto_backups,
            services::auto_backup::set_auto_backup_pinned,
            services::auto_backup::delete_auto_backup,
            services::auto_backup::restore_auto_backup,
            services::auto_backup::run_auto_backup_now,
            services::auto_backup::open_auto_backup_folder,
            app::commands::open_file_with_default_app,
            app::commands::open_file_location,
            app::commands::set_data_path,
            app::commands::toggle_autostart,
            app::commands::system_checklist::get_system_checklist,
            app::commands::system_checklist::ack_system_checklist_item,
            app::commands::is_autostart_enabled,
            // 「粘贴方案是否真的生效」的只读查询：未提权时如实报告，不再静默改用户设置。
            app::commands::get_paste_method_status,
            app::commands::set_windows_clipboard_history,
            app::commands::get_windows_clipboard_history,
            app::commands::set_win_clipboard_disabled,
            app::commands::trigger_registry_win_v_optimization,
            app::commands::is_registry_win_v_optimized,
            app::commands::restart_explorer,
            app::commands::restart_as_admin,
            app::commands::check_is_admin,
            app::commands::quit,
            app::commands::relaunch,
            app::commands::set_theme,
            app::commands::get_platform_info,
            app::commands::send_system_notification,
            app::commands::register_hotkey,
            app::commands::test_hotkey_available,
            app::commands::toggle_clipboard_pin,
            app::commands::update_tags,
            app::commands::add_manual_item,
            app::commands::update_item_content,
            app::commands::update_entry_note,
            app::commands::save_emoji_favorite,
            app::commands::remove_emoji_favorite,
            app::commands::list_emoji_favorites,
            app::commands::save_emoji_favorite_data_url,
            app::commands::save_emoji_favorite_url,
            app::commands::get_file_size,
            app::commands::save_file_copy,
            services::clipboard_ops::paste_text_directly,
            services::clipboard_ops::paste_content_transiently,
            services::file_transfer::send_chat_message,
            services::file_transfer::get_chat_history,
            services::file_transfer::send_file_to_client,
            services::file_transfer::get_app_logo,
            services::file_transfer::get_local_ip_addr,
            services::file_transfer::get_available_ips,
            services::file_transfer::get_file_server_status,
            services::file_transfer::toggle_file_server,
            services::file_transfer::get_active_file_transfer_path,
            // 传输页用到的三条：把粘贴板里的图片落成临时文件、按路径换一个可下载的
            // token URL、以及改「展示给对方的 IP」。三者都早有 `#[tauri::command]`，
            // 只是从来没登记，属于"界面点了没反应"的一类。
            services::file_transfer::save_temp_image,
            services::file_transfer::get_download_url,
            services::file_transfer::set_display_ip,
            services::paste_queue::get_paste_queue,
            services::paste_queue::set_paste_queue,
            services::paste_queue::paste_next_step,
            app::commands::get_tag_colors,
            app::commands::set_tag_color,
            app::commands::call_ai,
            app::commands::check_ai_connectivity,
            infrastructure::windows_api::apps::get_system_default_app,
            infrastructure::windows_api::apps::get_executable_icon,
            infrastructure::windows_api::apps::get_file_icon,
            infrastructure::windows_api::apps::scan_installed_apps,
            infrastructure::windows_api::apps::get_associated_apps,
            services::mcp::get_mcp_status,
            services::mcp::set_mcp_server_enabled,
            services::mcp::set_mcp_allow_write,
            services::mcp::set_mcp_require_token,
            services::mcp::set_mcp_allow_lan,
            services::mcp::set_mcp_port,
            services::mcp::set_mcp_autostart,
            services::mcp::regenerate_mcp_token
        ])
        .on_window_event(|window, event| {
            setup::handle_window_event(window, event);
        })
        .build(tauri::generate_context!());

    match app {
        Ok(app) => {
            info!(">>> [STARTUP] Tauri app built successfully.");
            app.run(|_app_handle, _event| {});
        }
        Err(e) => {
            error!(">>> [STARTUP] Failed to build tauri app: {}", e);
        }
    }

    // Cleanup Hooks on exit
    unsafe {
        let h_hook = HOOK_HANDLE.swap(std::ptr::null_mut(), Ordering::SeqCst);
        if !h_hook.is_null() {
            let _ = windows::Win32::UI::WindowsAndMessaging::UnhookWindowsHookEx(
                windows::Win32::UI::WindowsAndMessaging::HHOOK(h_hook as _),
            );
        }
        let h_mouse = HOOK_MOUSE_HANDLE.swap(std::ptr::null_mut(), Ordering::SeqCst);
        if !h_mouse.is_null() {
            let _ = windows::Win32::UI::WindowsAndMessaging::UnhookWindowsHookEx(
                windows::Win32::UI::WindowsAndMessaging::HHOOK(h_mouse as _),
            );
        }
    }
}
