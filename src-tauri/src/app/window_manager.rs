use crate::app_state::SettingsState;
use crate::global_state::*;
#[cfg(target_os = "windows")]
use crate::infrastructure::windows_ext::WindowExt;
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Emitter, Manager};

#[cfg(windows)]
use windows::Win32::Foundation::{HWND, POINT};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE,
};

/// 程序自动摆位时与屏幕边缘保持的最小距离（物理像素）。
///
/// 这个值必须显著大于边缘停靠的判定阈值 `threshold`（`setup.rs` 中为 5px）。
/// 原先是 5px，与停靠阈值完全重合，于是「程序按鼠标位置把窗口摆到屏幕边缘/主副屏交界」
/// 会被停靠判定误读成「用户把窗口拖到了边缘」，进而触发自动置顶与隐藏（R1 现象 b、c）。
/// 改成 40px 让「程序摆位」与「贴边判定」彻底解耦。
pub(crate) const AUTO_PLACEMENT_EDGE_MARGIN: i32 = 40;

/// 依「唤起屏」把窗口映射到目标显示器。
///
/// 取窗口在源显示器内的相对位置，等比映射到目标显示器，再夹紧到目标显示器内，
/// 因此结果一定完整可见。源显示器与目标显示器相同时结果为原位置（等比映射的不动点），
/// 所以调用方可以用「结果不等于原位置」来判断是否真的需要搬窗。
fn remap_fixed_window_position(
    window_pos: (i32, i32),
    window_size: (i32, i32),
    source_monitor: MonitorRect,
    target_monitor: MonitorRect,
) -> (i32, i32) {
    let (window_width, window_height) = window_size;
    let source_span_x = (source_monitor.width - window_width).max(0);
    let source_span_y = (source_monitor.height - window_height).max(0);
    let target_span_x = (target_monitor.width - window_width).max(0);
    let target_span_y = (target_monitor.height - window_height).max(0);

    let source_offset_x = (window_pos.0 - source_monitor.x).clamp(0, source_span_x);
    let source_offset_y = (window_pos.1 - source_monitor.y).clamp(0, source_span_y);

    let ratio_x = if source_span_x == 0 {
        0.0
    } else {
        source_offset_x as f64 / source_span_x as f64
    };
    let ratio_y = if source_span_y == 0 {
        0.0
    } else {
        source_offset_y as f64 / source_span_y as f64
    };

    let mapped_x = target_monitor.x + (ratio_x * target_span_x as f64).round() as i32;
    let mapped_y = target_monitor.y + (ratio_y * target_span_y as f64).round() as i32;

    (
        mapped_x.clamp(target_monitor.x, target_monitor.x + target_span_x),
        mapped_y.clamp(target_monitor.y, target_monitor.y + target_span_y),
    )
}

/// 读取当前显示器列表（物理像素矩形）。定位决策只在此处取一次屏幕列表。
#[cfg(target_os = "windows")]
fn available_monitor_rects(window: &tauri::WebviewWindow) -> Vec<MonitorRect> {
    window.monitor_rects()
}

/// 按窗口当前实际位置刷新「唤起屏」记录。
///
/// 在启动落位、显示落位之后调用，让记录的永远是窗口真实所在的显示器。
pub fn refresh_recall_monitor(window: &tauri::WebviewWindow) {
    let (Ok(size), Ok(pos)) = (window.outer_size(), window.outer_position()) else {
        return;
    };
    let window_center = (pos.x + size.width as i32 / 2, pos.y + size.height as i32 / 2);

    let monitors = window.monitor_rects();
    if let Some(monitor) = monitor_rect_for_point(&monitors, window_center.0, window_center.1) {
        set_recall_monitor(monitor);
    }
}

/// 决定并落实「本次显示时窗口应该落在哪里」。
///
/// 规则（对应 R1 需求 (a)：窗口不跨屏跳变）：
/// 1. `follow_mouse` 开启时按光标所在显示器落位——这是用户显式开启的能力，保留；
/// 2. 否则，只要窗口已经显示过、且仍停留在某块显示器上（`placement_is_valid`），
///    **一律保留窗口当前位置**。鼠标或前台窗口换到另一块屏不构成搬窗理由；
///    用户自己把窗口拖到别的屏也受此保护，不会被拽回来；
/// 3. 只有首次显示、或窗口已经跑到所有显示器之外时，才按「唤起位置」
///    （仍在前台的前台窗口中心 → 仍然连接着的唤起屏 → 光标 → 第一块显示器）重新落位。
///
/// 注意「唤起屏记录」只在对应显示器仍连着时才参与选屏：显示器被拔掉或排布变更后，
/// 旧记录指向一块不存在的屏幕，直接用它会算到屏幕之外。
#[cfg(target_os = "windows")]
fn apply_show_position(
    window: &tauri::WebviewWindow,
    wants_follow_mouse: bool,
    was_docked: bool,
    current_dock_val: i32,
    active_center: Option<(i32, i32)>,
) {
    let Ok(size) = window.outer_size() else {
        return;
    };
    let w = size.width as i32;
    let h = size.height as i32;

    let monitors = available_monitor_rects(window);
    if monitors.is_empty() {
        return;
    }

    let current_pos = window.outer_position().ok().map(|p| (p.x, p.y));
    let window_center = current_pos.map(|(x, y)| (x + w / 2, y + h / 2));
    let recall = recall_monitor();
    let placement_is_valid = window_placement_is_valid(recall, window_center, &monitors);

    let mut cursor = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut cursor);
    }

    // 唤起位置优先级：用户刚刚操作的前台窗口中心 > 光标所在显示器 > 第一块显示器。
    // 「上次记录的唤起屏」只在其对应显示器仍然连着时才参与选屏——显示器被拔掉/换布局后
    // 这条记录就是块不存在的屏幕，用它选屏会把窗口摆到屏幕之外。
    let focus_point = active_center.unwrap_or((cursor.x, cursor.y));
    let focus_monitor = monitor_rect_for_point(&monitors, focus_point.0, focus_point.1);
    let cursor_monitor = monitor_rect_for_point(&monitors, cursor.x, cursor.y);
    let connected_recall = connected_recall_monitor(recall, &monitors);
    let fallback_monitor = monitors.first().copied();

    if wants_follow_mouse {
        // 用户显式开启「跟随鼠标」：窗口跟随光标所在显示器落位（既有能力，保持不变）
        let target = cursor_monitor
            .or(focus_monitor)
            .or(connected_recall)
            .or(fallback_monitor);
        if let Some(monitor) = target {
            let below_y = cursor.y + 12;
            let y = if below_y + h > monitor.bottom() {
                // 下方放不下就翻到光标上方；上方也放不下则贴住下边界留出摆位留白
                let above_y = cursor.y - h - 12;
                if above_y >= monitor.top() {
                    above_y
                } else {
                    monitor.bottom() - h - AUTO_PLACEMENT_EDGE_MARGIN
                }
            } else {
                below_y
            };
            // 最后统一夹紧：即使走了「翻到上方」分支，也不会落到屏幕外
            let (x, y) = monitor.clamp_window_position(
                cursor.x - (w / 2),
                y,
                w,
                h,
                AUTO_PLACEMENT_EDGE_MARGIN,
            );
            let _ = window.set_position(tauri::Position::Physical(tauri::PhysicalPosition {
                x,
                y,
            }));
        }
    } else if placement_is_valid {
        // 关键分支：窗口已经在屏上，保留它当前所在位置与所在屏幕，不做任何搬窗。
        // 这里刻意不读取鼠标位置，也不比较 `current_monitor()` 与「唤起屏」是否相同。
    } else if was_docked {
        // 停靠态回显：以「它停靠的那块屏」为基准展开，而不是按当前鼠标/前台窗口重新选屏
        let target = connected_recall
            .or(focus_monitor)
            .or(cursor_monitor)
            .or(fallback_monitor);
        if let Some(monitor) = target {
            let mw = monitor.width;
            let (x, y) = match current_dock_val {
                1 => (monitor.x + (mw / 2 - w / 2), monitor.y + 10),
                2 => (monitor.x + 10, monitor.y + 10),
                3 => (monitor.x + mw - w - 10, monitor.y + 10),
                _ => (
                    monitor.x + (mw / 2) - (w / 2),
                    monitor.y + (monitor.height / 2) - (h / 2),
                ),
            };
            let _ = window.set_position(tauri::Position::Physical(tauri::PhysicalPosition {
                x,
                y,
            }));
        }
    } else {
        // 首次显示，或窗口已不在任何显示器上：按唤起位置重新落位
        let target = focus_monitor
            .or(connected_recall)
            .or(cursor_monitor)
            .or(fallback_monitor);
        if let Some(monitor) = target {
            let mapped = match (
                window_center.and_then(|(cx, cy)| monitor_rect_for_point(&monitors, cx, cy)),
                current_pos,
            ) {
                (Some(source), Some(pos)) => {
                    Some(remap_fixed_window_position(pos, (w, h), source, monitor))
                }
                _ => None,
            };
            let (x, y) = mapped.unwrap_or_else(|| {
                (
                    monitor.x + (monitor.width - w) / 2,
                    monitor.y + (monitor.height - h) / 2,
                )
            });

            // 目标位置与当前位置一致时不做多余的 set_position，避免无谓的窗口事件
            if current_pos != Some((x, y)) {
                let _ = window.set_position(tauri::Position::Physical(tauri::PhysicalPosition {
                    x,
                    y,
                }));
            }
        }
    }

    // 记录窗口实际落位所在显示器，作为后续判定的「唤起屏」。
    // 放在位置决策之后，保证记录的是真实落位而不是意图。
    if let Ok(pos) = window.outer_position() {
        if let Some(monitor) =
            monitor_rect_for_point(&monitors, pos.x + w / 2, pos.y + h / 2)
        {
            set_recall_monitor(monitor);
        }
    }
}

pub fn toggle_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        #[cfg(windows)]
        let mut active_center: Option<(i32, i32)> = None;
        let is_visible = window.is_visible().unwrap_or(false);
        let is_hidden_by_edge = IS_HIDDEN.load(Ordering::Relaxed);

        if is_visible && !is_hidden_by_edge {
            #[cfg(target_os = "windows")]
            WindowExt::release_win_keys();
            let _ = window.set_focusable(false);
            let _ = window.hide();

            let _ = restore_last_focus(app.clone());

            IS_HIDDEN.store(false, Ordering::Relaxed);
            NAVIGATION_ENABLED.store(false, Ordering::SeqCst);
            NAVIGATION_MODE_ACTIVE.store(false, Ordering::SeqCst);
            return;
        }

        IS_HIDDEN.store(false, Ordering::Relaxed);
        NAVIGATION_ENABLED.store(true, Ordering::SeqCst);
        let was_docked = is_hidden_by_edge;
        let current_dock_val = CURRENT_DOCK.load(Ordering::Relaxed);
        CURRENT_DOCK.store(0, Ordering::Relaxed);

        #[cfg(windows)]
        {
            let hwnd = WindowExt::get_foreground_window();
            let current_hwnd_val = hwnd.0 as isize;
            if current_hwnd_val != 0 {
                let mut main_hwnd_val = 0isize;
                if let Ok(h) = window.hwnd() {
                    main_hwnd_val = h.0 as isize;
                }
                if current_hwnd_val != main_hwnd_val {
                    LAST_ACTIVE_HWND.store(current_hwnd_val as usize, Ordering::Relaxed);
                    if let Some(rect) = WindowExt::get_window_rect(hwnd) {
                        let cx = (rect.left + rect.right) / 2;
                        let cy = (rect.top + rect.bottom) / 2;
                        active_center = Some((cx, cy));
                    }
                }
            }
        }

        #[cfg(target_os = "windows")]
        {
            let wants_follow_mouse = app
                .state::<SettingsState>()
                .follow_mouse
                .load(Ordering::Relaxed);
            apply_show_position(
                &window,
                wants_follow_mouse,
                was_docked,
                current_dock_val,
                active_center,
            );
        }

        // 变量在非 Windows 目标上不参与定位，显式消费以保持编译无警告
        let _ = (was_docked, current_dock_val);

        #[cfg(target_os = "windows")]
        WindowExt::release_win_keys();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        LAST_SHOW_TIMESTAMP.store(now, Ordering::Relaxed);

        let pinned = WINDOW_PINNED.load(Ordering::Relaxed);
        let _ = window.set_always_on_top(pinned);
        let _ = window.set_focusable(false);
        let _ = app.emit("window-pinned-changed", pinned);

        #[cfg(target_os = "windows")]
        {
            if let Ok(hwnd_raw) = window.hwnd() {
                unsafe {
                    let ex_style = GetWindowLongPtrW(HWND(hwnd_raw.0), GWL_EXSTYLE);
                    let _ = SetWindowLongPtrW(
                        HWND(hwnd_raw.0),
                        GWL_EXSTYLE,
                        ex_style | WS_EX_NOACTIVATE.0 as isize,
                    );
                }
                let _ = window.show();
                if pinned {
                    WindowExt::show_window_no_activate(HWND(hwnd_raw.0));
                } else {
                    WindowExt::show_window_no_activate_normal(HWND(hwnd_raw.0));
                }
            } else {
                let _ = window.show();
            }
        }

        #[cfg(not(windows))]
        {
            let _ = window.show();
        }
    }
}

#[tauri::command]
pub fn set_navigation_enabled(enabled: bool) -> Result<(), String> {
    NAVIGATION_ENABLED.store(enabled, Ordering::SeqCst);
    if !enabled {
        NAVIGATION_MODE_ACTIVE.store(false, Ordering::SeqCst);
    }
    Ok(())
}

#[tauri::command]
pub fn set_navigation_mode(active: bool) -> Result<(), String> {
    NAVIGATION_MODE_ACTIVE.store(active, Ordering::SeqCst);
    Ok(())
}

#[tauri::command]
pub fn activate_window_focus(app_handle: AppHandle) -> Result<(), String> {
    if let Some(window) = app_handle.get_webview_window("main") {
        let _ = window.set_focusable(true);

        #[cfg(windows)]
        {
            if let Ok(hwnd_raw) = window.hwnd() {
                unsafe {
                    let ex_style = GetWindowLongPtrW(HWND(hwnd_raw.0), GWL_EXSTYLE);
                    let next = ex_style & !(WS_EX_NOACTIVATE.0 as isize);
                    let _ = SetWindowLongPtrW(HWND(hwnd_raw.0), GWL_EXSTYLE, next);
                }
                let _ = window.set_focus();
                WindowExt::force_focus_window(HWND(hwnd_raw.0));
                return Ok(());
            }
        }
        let _ = window.set_focus();
    }
    Ok(())
}

#[tauri::command]
pub fn hide_window_cmd(app_handle: AppHandle) -> Result<(), String> {
    if let Some(window) = app_handle.get_webview_window("main") {
        #[cfg(target_os = "windows")]
        WindowExt::release_win_keys();
        let _ = window.set_focusable(false);
        let _ = window.hide();
        NAVIGATION_ENABLED.store(false, Ordering::SeqCst);
        NAVIGATION_MODE_ACTIVE.store(false, Ordering::SeqCst);
        let _ = restore_last_focus(app_handle.clone());
    }
    Ok(())
}

#[tauri::command]
pub fn toggle_window_cmd(app_handle: AppHandle) -> Result<(), String> {
    toggle_window(&app_handle);
    Ok(())
}

#[tauri::command]
pub fn focus_clipboard_window(app_handle: AppHandle) -> Result<(), String> {
    if let Some(window) = app_handle.get_webview_window("main") {
        let _ = window.set_focusable(true);
        let _ = window.show();

        #[cfg(windows)]
        {
            if let Ok(hwnd_raw) = window.hwnd() {
                unsafe {
                    let ex_style = GetWindowLongPtrW(HWND(hwnd_raw.0), GWL_EXSTYLE);
                    let next = ex_style & !(WS_EX_NOACTIVATE.0 as isize);
                    let _ = SetWindowLongPtrW(HWND(hwnd_raw.0), GWL_EXSTYLE, next);
                }
                let _ = window.set_focus();
                WindowExt::force_focus_window(HWND(hwnd_raw.0));
                return Ok(());
            }
        }
        let _ = window.set_focus();
        Ok(())
    } else {
        Err("Main window not found".to_string())
    }
}

#[tauri::command]
pub fn restore_last_focus(_app_handle: AppHandle) -> Result<(), String> {
    #[cfg(windows)]
    {
        let last_hwnd_val = LAST_ACTIVE_HWND.load(Ordering::Relaxed);
        if last_hwnd_val == 0 {
            return Ok(());
        }
        WindowExt::force_focus_window(HWND(last_hwnd_val as _));
        std::thread::sleep(std::time::Duration::from_millis(60));
    }
    Ok(())
}

pub fn release_win_keys() {
    #[cfg(target_os = "windows")]
    WindowExt::release_win_keys();
}

pub fn is_main_window_focused() -> bool {
    IS_MAIN_WINDOW_FOCUSED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::{remap_fixed_window_position, AUTO_PLACEMENT_EDGE_MARGIN};
    use crate::global_state::MonitorRect;

    fn primary_1080p() -> MonitorRect {
        MonitorRect::new(0, 0, 1920, 1080)
    }

    #[test]
    fn keeps_bottom_right_anchor_when_switching_monitors() {
        let source = primary_1080p();
        let target = MonitorRect::new(1920, 0, 2560, 1440);

        let mapped = remap_fixed_window_position((1610, 670), (300, 400), source, target);

        // 期望值由当前实现逐项算出，可复算：
        //   span_src  = (1920-300, 1080-400) = (1620, 680)
        //   offset    = (clamp(1610,0,1620), clamp(670,0,680)) = (1610, 670)
        //   ratio     = (1610/1620, 670/680) = (0.993827.., 0.985294..)
        //   span_dst  = (2560-300, 1440-400) = (2260, 1040)
        //   mapped    = (1920 + round(0.993827*2260), 0 + round(0.985294*1040))
        //             = (1920 + 2246, 1025) = (4166, 1025)
        // 旧期望 (4180, 1040) 描述的是「贴到新屏右下角」的另一套算法，与实现不符。
        assert_eq!(mapped, (4166, 1025));
    }

    #[test]
    fn preserves_center_ratio_for_mid_screen_window() {
        let source = primary_1080p();
        let target = MonitorRect::new(-1600, 0, 1600, 900);

        let mapped = remap_fixed_window_position((810, 340), (300, 400), source, target);

        //   span_src = (1620, 680)；offset = (810, 340)；ratio = (0.5, 0.5)
        //   span_dst = (1600-300, 900-400) = (1300, 500)
        //   mapped   = (-1600 + 650, 0 + 250) = (-950, 250)
        // 旧期望 (-800, 250) 把窗口中心当成屏幕中心（忽略窗口自身宽度），与实现不符。
        assert_eq!(mapped, (-950, 250));
    }

    #[test]
    fn clamps_positions_that_started_partly_outside_source_monitor() {
        let source = primary_1080p();
        let target = MonitorRect::new(1920, 0, 1280, 1024);

        let mapped = remap_fixed_window_position((2000, 900), (500, 500), source, target);

        assert_eq!(mapped, (2700, 524));
    }

    #[test]
    fn remap_is_identity_within_the_same_monitor() {
        // 源显示器与目标显示器相同时，等比映射是恒等变换，
        // 因此 `apply_show_position` 可以用「结果 != 原位置」判断是否需要搬窗。
        let monitor = MonitorRect::new(1920, 0, 2560, 1440);
        assert_eq!(
            remap_fixed_window_position((2500, 300), (300, 400), monitor, monitor),
            (2500, 300)
        );
    }

    #[test]
    fn auto_placement_margin_is_decoupled_from_dock_threshold() {
        // setup.rs 的贴边判定 threshold 为 5px；程序摆位留白必须与之不同且更大，
        // 否则「程序摆位」会被误判成「用户拖到边缘」而触发自动置顶（R1 现象 b）。
        const DOCK_THRESHOLD_IN_SETUP_RS: i32 = 5;
        assert_ne!(AUTO_PLACEMENT_EDGE_MARGIN, DOCK_THRESHOLD_IN_SETUP_RS);
        assert!(AUTO_PLACEMENT_EDGE_MARGIN > DOCK_THRESHOLD_IN_SETUP_RS);
    }
}
