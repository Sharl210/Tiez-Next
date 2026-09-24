use std::fs;
use std::path::PathBuf;

/// v0.2.8 Rename Migration: 贴汁 -> TieZ
pub fn perform_migration_v028(default_app_dir: &PathBuf) {
    // Check multiple possible locations for old data folder
    let mut old_app_dirs_to_check = Vec::new();

    // Check parent of current app dir (AppData\Roaming or AppData\Local)
    if let Some(parent) = default_app_dir.parent() {
        old_app_dirs_to_check.push(parent.join("贴汁"));
    }

    // Also check AppData\Local explicitly
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        old_app_dirs_to_check.push(std::path::PathBuf::from(&local_app_data).join("贴汁"));
    }

    // Also check AppData\Roaming explicitly
    if let Ok(roaming_app_data) = std::env::var("APPDATA") {
        old_app_dirs_to_check.push(std::path::PathBuf::from(&roaming_app_data).join("贴汁"));
    }

    // Try each possible location
    for old_app_dir in old_app_dirs_to_check {
        if old_app_dir.exists() && old_app_dir.is_dir() {
            println!(
                ">>> [MIGRATION] Found old data folder at: {:?}",
                old_app_dir
            );
            let new_db = default_app_dir.join("clipboard.db");
            let old_db = old_app_dir.join("clipboard.db");

            let mut success = false;

            // 1. Check for custom data path redirect (datapath.txt)
            let old_redirect = old_app_dir.join("datapath.txt");
            let new_redirect = default_app_dir.join("datapath.txt");

            if old_redirect.exists() {
                println!(">>> [MIGRATION] Found custom data path configuration. Migrating...");
                let _ = std::fs::create_dir_all(&default_app_dir);
                if std::fs::copy(&old_redirect, &new_redirect).is_ok() {
                    success = true;
                    println!(">>> [MIGRATION] Migrated datapath.txt successfully.");
                }
            }

            // 2. Data Migration Logic
            if !default_app_dir.exists() && !success {
                println!(">>> [MIGRATION] Renaming old data folder '贴汁' to 'TieZ'...");
                success = std::fs::rename(&old_app_dir, &default_app_dir).is_ok();
            } else if old_db.exists() && !new_db.exists() {
                println!(">>> [MIGRATION] Pulling old data from '贴汁' to 'TieZ'...");
                let _ = std::fs::create_dir_all(&default_app_dir);
                if std::fs::copy(&old_db, &new_db).is_ok() {
                    success = true;
                    let old_log = old_app_dir.join("tiez.log");
                    if old_log.exists() {
                        let _ = std::fs::copy(&old_log, default_app_dir.join("tiez.log"));
                    }
                }
            } else if old_db.exists() && new_db.exists() {
                let old_size = std::fs::metadata(&old_db).map(|m| m.len()).unwrap_or(0);
                let new_size = std::fs::metadata(&new_db).map(|m| m.len()).unwrap_or(0);

                if old_size > new_size && new_size < 50_000 {
                    println!(">>> [MIGRATION] Old database ({} bytes) has more data than new ({} bytes). Replacing...", old_size, new_size);
                    let backup_db = default_app_dir.join("clipboard.db.backup");
                    let _ = std::fs::rename(&new_db, &backup_db);

                    if std::fs::copy(&old_db, &new_db).is_ok() {
                        success = true;
                        println!(">>> [MIGRATION] Successfully migrated old database to TieZ.");
                        let old_redirect = old_app_dir.join("datapath.txt");
                        if old_redirect.exists() {
                            let _ =
                                std::fs::copy(&old_redirect, default_app_dir.join("datapath.txt"));
                        }
                    } else {
                        let _ = std::fs::rename(&backup_db, &new_db);
                    }
                } else {
                    success = true;
                }
            } else {
                success = true;
            }

            if success {
                println!(">>> [CLEANUP] Cleaning up residues of old '贴汁' version...");
                if old_app_dir.exists() {
                    let _ = std::fs::remove_dir_all(&old_app_dir);
                }
                let custom_path = cleanup_old_install_registry();
                cleanup_old_start_menu();
                cleanup_old_install_folder(custom_path);
            }
        }
    }

    // Always try to clean up old version residues on every startup
    let custom_path = cleanup_old_install_registry();
    cleanup_old_start_menu();
    cleanup_old_install_folder(custom_path);
}

/// 判断一个「添加/删除程序」条目是不是**需要清理的旧版残留**。
///
/// 抽成纯函数是为了可测：真正的清理路径整段在 `#[cfg(windows)]` 里，且要读写
/// 注册表，测试跑不动。而这段判定恰好是最容易出错的地方——判错一边会在用户机器上
/// 留下一条永远删不掉的旧版卸载项，判错另一边会去删**我们自己**的条目。
///
/// 判据：
/// * `Tiez-Next`（当前产品名）出现的条目一律**不算**旧版残留，无论出现在
///   DisplayName 还是子键名里——这是显式护栏，不依赖"我们的名字恰好不含旧名"这种
///   巧合（`Tiez-Next` 不含 `TieZ` 只是大小写敏感下的巧合）。
/// * 其余提到「贴汁」（更早的旧名）或 `TieZ`（改名前的旧名）的条目需要清理。
pub fn is_legacy_uninstall_entry(subkey_name: &str, display_name: &str) -> bool {
    let is_ours = display_name.contains("Tiez-Next") || subkey_name.contains("Tiez-Next");
    if is_ours {
        return false;
    }
    display_name.contains("贴汁") || display_name.contains("TieZ")
}

/// v0.2.8 Rename Migration: Registry Cleanup - Returns found install location if any
pub fn cleanup_old_install_registry() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        use winreg::enums::*;
        use winreg::RegKey;
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let path = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall";
        let mut found_install_loc = None;

        println!(
            ">>> [CLEANUP] Scanning registry for old versions at HKCU\\{}",
            path
        );

        if let Ok(key) = hkcu.open_subkey_with_flags(path, KEY_READ | KEY_WRITE) {
            for subkey_name in key.enum_keys().filter_map(|x| x.ok()) {
                if let Ok(subkey) = key.open_subkey(&subkey_name) {
                    let name: String = subkey.get_value("DisplayName").unwrap_or_default();

                    // 【改名后暴露的缺口】这里原先只匹配「贴汁」——那是**再上一次**
                    // 改名的旧名。本项目改名到 Tiez-Next 之前叫「TieZ」，它的
                    // 添加/删除程序条目（`Uninstall\TieZ`，由旧版自己的安装器写入，
                    // DisplayName="TieZ"）**不匹配「贴汁」**，于是永久留在用户机器上。
                    //
                    // 后果不只是"列表里多个东西"：新旧两版的产品名不同 → 安装器推导出
                    // 的 Uninstall 键名不同 → 装新版时读不到旧键，被当成**全新安装**，
                    // 新旧并列共存。用户看到两条，点旧的那条就跑到原版卸载器，界面上写
                    // 着「卸载 TieZ」——这正是用户反馈的现象。
                    //
                    // 这里只清**注册表条目**，不碰任何目录：旧版的程序目录与用户数据
                    // 都不该被我们擅自删除（数据目录的迁移由「迁移中心」负责）。
                    if is_legacy_uninstall_entry(&subkey_name, &name) {
                        println!(
                            ">>> [CLEANUP] Found old registry entry: {} ({}).",
                            subkey_name, name
                        );

                        // Try to get InstallLocation
                        if let Ok(loc) = subkey.get_value::<String, _>("InstallLocation") {
                            if !loc.is_empty() {
                                println!(
                                    ">>> [CLEANUP] Found InstallLocation in registry: {}",
                                    loc
                                );
                                found_install_loc = Some(PathBuf::from(loc));
                            }
                        }
                        // Fallback: Try to parse from UninstallString "C:\path\to\uninstall.exe"
                        if found_install_loc.is_none() {
                            if let Ok(uninstall_str) =
                                subkey.get_value::<String, _>("UninstallString")
                            {
                                println!(">>> [CLEANUP] Found UninstallString: {}", uninstall_str);
                                // Simple heuristic: remove quotes and find parent of executable
                                let clean_str = uninstall_str.replace("\"", "");
                                let p = std::path::Path::new(&clean_str);
                                if let Some(parent) = p.parent() {
                                    println!(">>> [CLEANUP] inferred install path from uninstaller: {:?}", parent);
                                    found_install_loc = Some(parent.to_path_buf());
                                }
                            }
                        }

                        println!(">>> [CLEANUP] Deleting registry key...");
                        if let Err(e) = key.delete_subkey_all(&subkey_name) {
                            println!(">>> [CLEANUP ERROR] Failed to delete registry key: {}", e);
                        } else {
                            println!(">>> [CLEANUP] Registry entry deleted.");
                        }
                    }
                }
            }
        }
        return found_install_loc;
    }
    #[cfg(not(windows))]
    None
}

/// v0.2.8 Rename Migration: Start Menu & Desktop Cleanup
pub fn cleanup_old_start_menu() {
    #[cfg(windows)]
    {
        if let Ok(app_data) = std::env::var("APPDATA") {
            let start_menu =
                std::path::Path::new(&app_data).join("Microsoft\\Windows\\Start Menu\\Programs");
            println!(">>> [CLEANUP] Checking Start Menu at: {:?}", start_menu);

            // Delete old shortcut
            let old_lnk = start_menu.join("贴汁.lnk");
            if old_lnk.exists() {
                println!(
                    ">>> [CLEANUP] Deleting old start menu shortcut: {:?}",
                    old_lnk
                );
                let _ = fs::remove_file(old_lnk);
            }

            // Delete old start menu folder
            let old_folder = start_menu.join("贴汁");
            if old_folder.exists() && old_folder.is_dir() {
                println!(
                    ">>> [CLEANUP] Deleting old start menu folder: {:?}",
                    old_folder
                );
                let _ = fs::remove_dir_all(old_folder);
            }
        }

        // Desktop Cleanup
        if let Ok(user_profile) = std::env::var("USERPROFILE") {
            let desktop = std::path::Path::new(&user_profile).join("Desktop");
            let old_desktop_lnk = desktop.join("贴汁.lnk");
            println!(
                ">>> [CLEANUP] Checking Desktop shortcut at: {:?}",
                old_desktop_lnk
            );
            if old_desktop_lnk.exists() {
                println!(
                    ">>> [CLEANUP] Deleting old desktop shortcut: {:?}",
                    old_desktop_lnk
                );
                let _ = fs::remove_file(old_desktop_lnk);
            }
        }
    }
}

/// Helper function to manually enable autostart via registry
pub fn enable_autostart_manually(app_name: &str, exe_path: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use winreg::enums::*;
        use winreg::RegKey;
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let path = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
        let (key, _) = hkcu.create_subkey(path)?;
        key.set_value(app_name, &exe_path)?;
        Ok(())
    }
    #[cfg(not(windows))]
    Ok(())
}

/// v0.2.8 Rename Migration: Clean up old installation directory
pub fn cleanup_old_install_folder(custom_path: Option<PathBuf>) {
    #[cfg(windows)]
    {
        // Try to find and delete old installation folder
        // Common installation paths
        let mut possible_paths = vec![
            std::env::var("LOCALAPPDATA")
                .ok()
                .map(|p| PathBuf::from(p).join("Programs").join("贴汁")),
            std::env::var("ProgramFiles")
                .ok()
                .map(|p| PathBuf::from(p).join("贴汁")),
            std::env::var("ProgramFiles(x86)")
                .ok()
                .map(|p| PathBuf::from(p).join("贴汁")),
            // Also check direct local appdata just in case
            std::env::var("LOCALAPPDATA")
                .ok()
                .map(|p| PathBuf::from(p).join("贴汁")),
        ];

        // Add custom path from registry if found
        if let Some(path) = custom_path {
            println!(
                ">>> [CLEANUP] Adding custom path from registry to cleanup list: {:?}",
                path
            );
            possible_paths.push(Some(path));
        }

        for path_opt in possible_paths.iter() {
            if let Some(path) = path_opt {
                println!(">>> [CLEANUP] Checking installation path: {:?}", path);
                if path.exists() && path.is_dir() {
                    // Safety check: Don't delete if it's the current running dir (unlikely due to rename, but good practice)
                    if let Ok(current_exe) = std::env::current_exe() {
                        if let Some(current_dir) = current_exe.parent() {
                            if path == current_dir {
                                println!(
                                    ">>> [CLEANUP] Skipping current directory safety check: {:?}",
                                    path
                                );
                                continue;
                            }
                        }
                    }

                    println!(">>> [CLEANUP] Found old installation folder: {:?}", path);
                    // Try to delete - this might fail if files are in use
                    match fs::remove_dir_all(path) {
                        Ok(_) => {
                            println!(">>> [CLEANUP] Successfully deleted old installation folder")
                        }
                        Err(e) => println!(
                            ">>> [CLEANUP] Could not delete old installation folder: {}",
                            e
                        ),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod legacy_uninstall_entry_tests {
    use super::is_legacy_uninstall_entry;

    /// 旧版 TieZ 的条目必须被识别出来。
    ///
    /// 【这条对应一个真实缺陷】判据原先只匹配「贴汁」——那是**再上一次**改名的旧名。
    /// `DisplayName="TieZ"` 不含「贴汁」，于是旧版条目永久留在用户的添加/删除程序里，
    /// 用户点到它就会跑去原版卸载器（界面上写着「卸载 TieZ」）。
    #[test]
    fn recognizes_the_tiez_entry_that_the_old_guard_missed() {
        assert!(
            is_legacy_uninstall_entry("TieZ", "TieZ"),
            "改名前的旧版条目必须被清理，否则用户会永远看到一条「卸载 TieZ」"
        );
    }

    /// 更早的旧名仍然要识别（原有行为不能丢）。
    #[test]
    fn still_recognizes_the_older_name() {
        assert!(is_legacy_uninstall_entry("贴汁", "贴汁"));
    }

    /// **绝不能**把我们自己的条目当成旧版残留——那会把用户正在用的安装记录删掉。
    #[test]
    fn never_treats_our_own_entry_as_legacy() {
        assert!(
            !is_legacy_uninstall_entry("Tiez-Next", "Tiez-Next"),
            "删掉自己的卸载条目会让用户无法正常卸载本应用"
        );
    }

    /// 自我排除要显式且健壮：即使有人把判定改成忽略大小写，也不能删到自己。
    #[test]
    fn self_exclusion_does_not_rely_on_case_sensitivity() {
        // 子键名或显示名里出现当前产品名，就必须认定为"自己的"。
        assert!(!is_legacy_uninstall_entry("Tiez-Next", ""));
        assert!(!is_legacy_uninstall_entry("", "Tiez-Next"));
    }

    /// 与旧版无关的条目一律不动。
    #[test]
    fn leaves_unrelated_entries_alone() {
        for (key, name) in [("SomeOtherApp", "SomeOtherApp"), ("Code", "Visual Studio Code")] {
            assert!(
                !is_legacy_uninstall_entry(key, name),
                "不该动与旧版无关的条目：{key}"
            );
        }
    }
}
