// Global state module
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize};

pub static GLOBAL_APP_HANDLE: std::sync::OnceLock<tauri::AppHandle> = std::sync::OnceLock::new();
pub static HOOK_HANDLE: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(null_mut());
pub static HOOK_MOUSE_HANDLE: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(null_mut());
pub static HOTKEY_STRING: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

#[derive(Clone, Debug)]
pub struct HookHotkey {
    pub vk: u32,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub win: bool,
}

pub static TARGET_HOTKEY: std::sync::Mutex<Option<HookHotkey>> = std::sync::Mutex::new(None);

// Win+ hotkeys are now handled via tauri-plugin-global-shortcut.

pub static IS_RECORDING: AtomicBool = AtomicBool::new(false);
pub static IGNORE_BLUR: AtomicBool = AtomicBool::new(false);
pub static WINDOW_PINNED: AtomicBool = AtomicBool::new(false);
pub static CLIPBOARD_MONITOR_PAUSED: AtomicBool = AtomicBool::new(false);
pub static LAST_ACTIVE_HWND: AtomicUsize = AtomicUsize::new(0);
pub static LAST_APP_SET_HASH: AtomicU64 = AtomicU64::new(0);
pub static LAST_APP_SET_HASH_ALT: AtomicU64 = AtomicU64::new(0);
pub static LAST_APP_SET_IMAGE_VISUAL_HASH: AtomicU64 = AtomicU64::new(0);
pub static LAST_APP_SET_TIMESTAMP: AtomicU64 = AtomicU64::new(0);
pub static LAST_TOGGLE_TIMESTAMP: AtomicU64 = AtomicU64::new(0);
pub static LAST_SHOW_TIMESTAMP: AtomicU64 = AtomicU64::new(0);
pub static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);
pub static TASKBAR_CREATED_MSG: AtomicU32 = AtomicU32::new(0);
pub static QUICK_PASTE_DIGIT_MASK: AtomicU32 = AtomicU32::new(0);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DockPosition {
    None,
    Top,
    Left,
    Right,
}

pub static CURRENT_DOCK: AtomicI32 = AtomicI32::new(0); // 0: None, 1: Top, 2: Left, 3: Right
pub static IS_HIDDEN: AtomicBool = AtomicBool::new(false);
pub static IS_MOUSE_BUTTON_DOWN: AtomicBool = AtomicBool::new(false);
pub static NAVIGATION_ENABLED: AtomicBool = AtomicBool::new(false);
pub static NAVIGATION_MODE_ACTIVE: AtomicBool = AtomicBool::new(false);
pub static IS_MAIN_WINDOW_FOCUSED: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// 多显示器几何（R1：窗口保留在唤起屏、异屏点击不隐藏）
//
// 这里的函数全部是纯几何计算，不调用任何平台 API，因此可以在 Linux 上直接
// 跑单元测试；真正取显示器列表/光标位置的平台胶水代码留在各调用点。
// ---------------------------------------------------------------------------

/// 一块显示器的物理像素矩形（左闭右开，与 Win32 / Tauri 的矩形语义一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl MonitorRect {
    pub fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn from_monitor(monitor: &tauri::Monitor) -> Self {
        let position = monitor.position();
        let size = monitor.size();
        Self::new(position.x, position.y, size.width as i32, size.height as i32)
    }

    pub fn left(&self) -> i32 {
        self.x
    }

    pub fn top(&self) -> i32 {
        self.y
    }

    pub fn right(&self) -> i32 {
        self.x + self.width
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.height
    }

    /// 点是否落在这块显示器上。右/下边界取开区间，避免相邻两块屏的交界点被同时命中。
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left() && x < self.right() && y >= self.top() && y < self.bottom()
    }

    /// 把窗口左上角夹进本显示器，四周保留 `margin` 像素。
    /// 窗口比显示器还大时退化为贴左/上边界，不会产生反向区间。
    pub fn clamp_window_position(&self, x: i32, y: i32, w: i32, h: i32, margin: i32) -> (i32, i32) {
        let min_x = self.left() + margin;
        let min_y = self.top() + margin;
        let max_x = (self.right() - w - margin).max(min_x);
        let max_y = (self.bottom() - h - margin).max(min_y);
        (x.clamp(min_x, max_x), y.clamp(min_y, max_y))
    }
}

/// 找出包含该点的显示器。点在所有显示器之外（多屏非对齐布局的缝隙）时返回 None。
pub fn monitor_rect_for_point(monitors: &[MonitorRect], x: i32, y: i32) -> Option<MonitorRect> {
    monitors.iter().copied().find(|m| m.contains(x, y))
}

/// 点是否落在「窗口当前所在显示器」之外的另一块显示器上。
///
/// 只有确实命中另一块屏时才为 true；点落在所有显示器之外（缝隙）时为 false，
/// 以便异屏判定保持保守——不确定时沿用既有隐藏语义。
pub fn is_point_on_other_monitor(
    window_monitor: Option<MonitorRect>,
    monitors: &[MonitorRect],
    x: i32,
    y: i32,
) -> bool {
    let Some(window_monitor) = window_monitor else {
        return false;
    };
    match monitor_rect_for_point(monitors, x, y) {
        Some(hit) => hit != window_monitor,
        None => false,
    }
}

/// 窗口最近一次被本程序落位的显示器，即需求里的「唤起屏」。
/// 窗口显示期间不因鼠标/前台窗口跨屏而改写它，也就不再跨屏搬窗。
pub static RECALL_MONITOR: std::sync::Mutex<Option<MonitorRect>> = std::sync::Mutex::new(None);

pub fn set_recall_monitor(monitor: MonitorRect) {
    if let Ok(mut guard) = RECALL_MONITOR.lock() {
        *guard = Some(monitor);
    }
}

pub fn recall_monitor() -> Option<MonitorRect> {
    RECALL_MONITOR.lock().ok().and_then(|guard| *guard)
}

/// 窗口当前的落位是否仍然有效。
///
/// 有效 = 已经记录过唤起屏，且窗口中心仍落在某块显示器上。
/// 此时唤起动作不得做任何跨屏搬窗，窗口留在原处（用户手动拖到别的屏也受此保护）。
/// 无效 = 首次显示，或窗口跑到所有显示器之外，需要按唤起位置重新落位。
pub fn window_placement_is_valid(
    recall: Option<MonitorRect>,
    window_center: Option<(i32, i32)>,
    monitors: &[MonitorRect],
) -> bool {
    let Some(center) = window_center else {
        return false;
    };
    if recall.is_none() {
        return false;
    }
    monitors.iter().any(|m| m.contains(center.0, center.1))
}

/// 唤起屏记录只有在对应显示器仍然连着时才可用。
///
/// 显示器被拔掉、分辨率/排布变更后，旧记录指向一块不存在的屏幕，
/// 直接拿它选屏会把窗口摆到屏幕之外，因此必须先按当前显示器列表过滤。
pub fn connected_recall_monitor(
    recall: Option<MonitorRect>,
    monitors: &[MonitorRect],
) -> Option<MonitorRect> {
    recall.filter(|r| monitors.iter().any(|m| m == r))
}

/// 显示器查询：让 `tauri::Window` 与 `tauri::WebviewWindow` 共用同一套取屏逻辑。
///
/// 两者都暴露 `available_monitors()` / `current_monitor()`，但类型不同，
/// 静态分发可以避免在两个调用点各写一份转换代码。
pub trait MonitorQuery {
    fn monitor_rects(&self) -> Vec<MonitorRect>;
    fn current_monitor_rect(&self) -> Option<MonitorRect>;
}

macro_rules! impl_monitor_query {
    ($ty:ty) => {
        impl<R: tauri::Runtime> MonitorQuery for $ty {
            fn monitor_rects(&self) -> Vec<MonitorRect> {
                self.available_monitors()
                    .map(|list| list.iter().map(MonitorRect::from_monitor).collect())
                    .unwrap_or_default()
            }

            fn current_monitor_rect(&self) -> Option<MonitorRect> {
                match self.current_monitor() {
                    Ok(Some(monitor)) => Some(MonitorRect::from_monitor(&monitor)),
                    _ => None,
                }
            }
        }
    };
}

impl_monitor_query!(tauri::Window<R>);
impl_monitor_query!(tauri::WebviewWindow<R>);

#[cfg(test)]
mod monitor_rect_tests {
    use super::*;

    fn dual_screen() -> Vec<MonitorRect> {
        // 主屏在左（1920x1080），副屏在右（2560x1440，顶部对齐）
        vec![
            MonitorRect::new(0, 0, 1920, 1080),
            MonitorRect::new(1920, 0, 2560, 1440),
        ]
    }

    #[test]
    fn contains_uses_half_open_bounds() {
        let m = MonitorRect::new(0, 0, 1920, 1080);
        assert!(m.contains(0, 0));
        assert!(m.contains(1919, 1079));
        assert!(!m.contains(1920, 1079), "右边界必须属于下一块屏");
        assert!(!m.contains(1919, 1080));
        assert!(!m.contains(-1, 0));
    }

    #[test]
    fn monitor_rect_for_point_picks_the_owning_screen() {
        let monitors = dual_screen();
        assert_eq!(
            monitor_rect_for_point(&monitors, 100, 100),
            Some(MonitorRect::new(0, 0, 1920, 1080))
        );
        assert_eq!(
            monitor_rect_for_point(&monitors, 2000, 100),
            Some(MonitorRect::new(1920, 0, 2560, 1440))
        );
        assert_eq!(monitor_rect_for_point(&monitors, -500, 100), None);
    }

    #[test]
    fn click_on_other_monitor_is_detected() {
        let monitors = dual_screen();
        let window_monitor = MonitorRect::new(0, 0, 1920, 1080);

        // 同一块屏上的点击（含窗口外）-> 沿用既有隐藏语义
        assert!(!is_point_on_other_monitor(
            Some(window_monitor),
            &monitors,
            1900,
            900
        ));
        // 另一块屏上的点击 -> 不隐藏
        assert!(is_point_on_other_monitor(
            Some(window_monitor),
            &monitors,
            2500,
            900
        ));
        // 落在所有显示器之外的缝隙 -> 保守沿用既有语义
        assert!(!is_point_on_other_monitor(
            Some(window_monitor),
            &monitors,
            -10,
            500
        ));
        // 拿不到窗口所在显示器 -> 不做异屏豁免
        assert!(!is_point_on_other_monitor(None, &monitors, 2500, 900));
    }

    #[test]
    fn click_on_other_monitor_is_detected_for_left_placed_negative_coords() {
        // 副屏在主屏左侧时坐标为负
        let monitors = vec![
            MonitorRect::new(0, 0, 1920, 1080),
            MonitorRect::new(-1600, 0, 1600, 900),
        ];
        let window_monitor = MonitorRect::new(0, 0, 1920, 1080);
        assert!(is_point_on_other_monitor(
            Some(window_monitor),
            &monitors,
            -800,
            400
        ));
        assert!(!is_point_on_other_monitor(
            Some(window_monitor),
            &monitors,
            800,
            400
        ));
    }

    #[test]
    fn placement_stays_valid_once_recall_monitor_is_known() {
        let monitors = dual_screen();
        let recall = Some(MonitorRect::new(0, 0, 1920, 1080));

        // 已记录唤起屏 + 窗口仍在某块屏上 -> 保留原位置，不跨屏搬窗
        assert!(window_placement_is_valid(recall, Some((900, 500)), &monitors));
        // 鼠标跨到副屏不改变判定：窗口没动，落位依旧有效
        assert!(window_placement_is_valid(recall, Some((900, 500)), &monitors));
        // 用户把窗口拖到副屏 -> 仍然有效（尊重用户手动摆放）
        assert!(window_placement_is_valid(recall, Some((2500, 500)), &monitors));
    }

    #[test]
    fn placement_needs_recompute_on_first_show_or_when_offscreen() {
        let monitors = dual_screen();
        let recall = Some(MonitorRect::new(0, 0, 1920, 1080));

        // 首次显示（尚无唤起屏记录）-> 需要按唤起位置计算
        assert!(!window_placement_is_valid(None, Some((900, 500)), &monitors));
        // 窗口跑到所有显示器之外 -> 需要重新落位
        assert!(!window_placement_is_valid(recall, Some((-500, 500)), &monitors));
        // 取不到窗口位置 -> 需要重新落位
        assert!(!window_placement_is_valid(recall, None, &monitors));
    }

    #[test]
    fn clamp_window_position_keeps_window_inside_screen_with_margin() {
        let m = MonitorRect::new(1920, 0, 2560, 1440);

        // 越界到左边 -> 贴到左边界 + margin
        assert_eq!(m.clamp_window_position(1000, 500, 300, 400, 40), (1960, 500));
        // 越界到右下 -> 贴到右下边界 - margin
        assert_eq!(
            m.clamp_window_position(9000, 9000, 300, 400, 40),
            (1920 + 2560 - 300 - 40, 1440 - 400 - 40)
        );
        // 屏内位置保持不变
        assert_eq!(m.clamp_window_position(2500, 300, 300, 400, 40), (2500, 300));
    }

    #[test]
    fn clamp_window_position_degrades_gracefully_for_oversized_window() {
        let m = MonitorRect::new(0, 0, 800, 600);
        // 窗口比屏幕还大时贴左上，不产生反向区间或 panic
        assert_eq!(m.clamp_window_position(500, 500, 1200, 900, 40), (40, 40));
    }

    #[test]
    fn stale_recall_monitor_is_dropped_when_screen_is_unplugged() {
        let monitors = dual_screen();
        // 记录的唤起屏仍在 -> 可用
        assert_eq!(
            connected_recall_monitor(Some(MonitorRect::new(0, 0, 1920, 1080)), &monitors),
            Some(MonitorRect::new(0, 0, 1920, 1080))
        );
        // 显示器被拔掉后旧记录作废，否则会把窗口摆到不存在的屏幕上
        assert_eq!(
            connected_recall_monitor(Some(MonitorRect::new(3840, 0, 1920, 1080)), &monitors),
            None
        );
        assert_eq!(connected_recall_monitor(None, &monitors), None);
    }
}
