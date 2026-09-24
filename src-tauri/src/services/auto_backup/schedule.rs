//! 自动容灾备份的**调度**：启动时一次 + 之后按周期一次。
//!
//! # 两条独立的时间线
//!
//! 用户把这两件事说成两件：
//!
//! - **定时备份**：受总开关 [`AutoBackupConfig::enabled`] 与周期
//!   [`AutoBackupConfig::interval_minutes`] 控制。
//! - **启动时备份一次**：独立勾选项，**不受定时备份总开关约束**。
//!
//! 因此本模块**先算启动那一份**（只看 `backup_on_startup`），再进入定时循环（只看
//! `enabled`）。两者共用同一个目录——用户明确说了"他们的备份路径是一致的，因为都属于
//! 容灾的自动保险措施"。
//!
//! # 为什么"该不该备份"是一个纯函数
//!
//! 判据（[`due_for_scheduled_backup`]）只依赖"最近一次自动备份的时间"和"现在"，不碰
//! 文件系统也不碰时钟。于是"周期到了没有""启动那一份算不算进周期"这些最容易写错的地方
//! 可以被直接断言，而不是靠跑一分钟才看得出来。
//!
//! 一处刻意的取舍：**启动备份不重置定时周期**。启动时若已生成一份启动备份，定时循环会
//! 从这一份的时刻起算——否则"开机后 1 分钟又立刻再来一份定时备份"，用户会看到两份几乎
//! 一样的东西。这一点由 [`due_for_scheduled_backup`] 的"取最近一次**任意来源**的自动备份"
//! 保证（手动"立即备份"同样计入，理由相同）。

use super::store::{AutoBackupStore, BackupEntry, BackupOrigin};
use std::time::Duration;

/// 定时循环的**检查**间隔。
///
/// 它不是备份周期——备份周期由配置给出（默认 30 分钟）。检查间隔只是"多久醒来一次看看
/// 到点了没有"，取 1 分钟：足以让 30 分钟的周期有秒级精度，又不会空转吃 CPU。
pub const TICK: Duration = Duration::from_secs(60);

/// 定时备份是否到点。
///
/// `last_auto_ms` 是目录里最近一次**自动来源**（定时或启动）备份的时刻；没有任何备份时
/// 传 `None`（此时视为"到点"，由调用方决定要不要立刻补一份）。
pub fn due_for_scheduled_backup(
    last_auto_ms: Option<i64>,
    now_ms: i64,
    interval_minutes: u32,
) -> bool {
    let Some(last) = last_auto_ms else {
        return true;
    };
    // 周期下限兜底：配置层已经把 0 夹到 1，这里再防一次，避免"每毫秒都到点"。
    let interval_ms = (interval_minutes.max(1) as i64).saturating_mul(60_000);
    // 时钟被往回调（用户改系统时间、夏令时回拨）时 `now - last` 会是负数。此时**不备份**：
    // 否则每次改时间都会凭空多出一份，而用户并没有要求"改时间就备份"。
    now_ms.saturating_sub(last) >= interval_ms
}

/// 从备份列表里取最近一次自动备份（定时或启动）的毫秒时刻。
pub fn last_automatic_ms(entries: &[BackupEntry]) -> Option<i64> {
    entries
        .iter()
        .filter(|e| e.origin == "scheduled" || e.origin == "startup")
        .map(|e| e.created_at_ms)
        .max()
}

/// 启动时的那一次备份（只看 `backup_on_startup`，**不看**定时开关）。
pub struct StartupOutcome {
    pub created: Option<BackupEntry>,
    pub warnings: Vec<String>,
}

/// 执行"启动备份一次"。
///
/// 返回 `None` 表示用户没开这个勾选项（不是失败）。
pub fn run_startup_backup(
    store: &mut AutoBackupStore,
    data_dir: &std::path::Path,
    app_version: &str,
    backup_on_startup: bool,
) -> StartupOutcome {
    if !backup_on_startup {
        return StartupOutcome {
            created: None,
            warnings: Vec::new(),
        };
    }
    match store.create(data_dir, BackupOrigin::Startup, app_version) {
        Ok(entry) => StartupOutcome {
            created: Some(entry),
            warnings: Vec::new(),
        },
        Err(e) => StartupOutcome {
            created: None,
            warnings: vec![format!("启动自动备份失败：{}", e)],
        },
    }
}

/// 执行"定时到点就备份一次"。
///
/// 由调用方在每次 [`TICK`] 时调用一次；本函数自己判断是否到点。
pub struct TickOutcome {
    pub created: Option<BackupEntry>,
    pub warnings: Vec<String>,
}

/// 一次定时检查。`enabled=false` 或未到点时不做事。
pub fn run_scheduled_tick(
    store: &mut AutoBackupStore,
    data_dir: &std::path::Path,
    app_version: &str,
    enabled: bool,
    interval_minutes: u32,
    now_ms: i64,
) -> TickOutcome {
    if !enabled {
        return TickOutcome {
            created: None,
            warnings: Vec::new(),
        };
    }
    let entries = match store.list() {
        Ok(v) => v,
        Err(e) => {
            return TickOutcome {
                created: None,
                warnings: vec![format!("读取自动备份列表失败：{}", e)],
            }
        }
    };
    if !due_for_scheduled_backup(last_automatic_ms(&entries), now_ms, interval_minutes) {
        return TickOutcome {
            created: None,
            warnings: Vec::new(),
        };
    }
    match store.create(data_dir, BackupOrigin::Scheduled, app_version) {
        Ok(entry) => TickOutcome {
            created: Some(entry),
            warnings: Vec::new(),
        },
        Err(e) => TickOutcome {
            created: None,
            warnings: vec![format!("定时自动备份失败：{}", e)],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(origin: &str, ms: i64) -> BackupEntry {
        BackupEntry {
            archive_name: format!("x-{}.zip", ms),
            path: String::new(),
            origin: origin.to_string(),
            created_at: String::new(),
            created_at_ms: ms,
            created_at_local: String::new(),
            size_bytes: 0,
            pinned: false,
            seq: 1,
        }
    }

    /// 从未备份过 → 立刻该备份（否则用户开箱后要等满一个周期才有第一份）。
    #[test]
    fn no_backup_yet_means_due_immediately() {
        assert!(due_for_scheduled_backup(None, 1_000, 30));
    }

    /// 周期边界：差 1 毫秒不算到点，差 0 毫秒算到点。
    #[test]
    fn interval_boundary_is_exact() {
        let interval_ms = 30 * 60_000;
        assert!(!due_for_scheduled_backup(
            Some(0),
            interval_ms - 1,
            30
        ));
        assert!(due_for_scheduled_backup(Some(0), interval_ms, 30));
    }

    /// 手动与启动备份都计入周期：不会出现"刚启动备份过一分钟又定时备份一份"。
    #[test]
    fn startup_backup_counts_toward_the_scheduled_interval() {
        let entries = vec![entry("startup", 500_000), entry("manual", 900_000)];
        assert_eq!(last_automatic_ms(&entries), Some(500_000));
        assert!(
            !due_for_scheduled_backup(Some(500_000), 500_000 + 60_000, 30),
            "启动备份之后一分钟不该再到点"
        );
    }

    /// 时钟往回调时不备份——否则每次改系统时间都凭空多一份。
    #[test]
    fn moving_the_clock_backwards_does_not_create_backups() {
        assert!(!due_for_scheduled_backup(Some(10_000_000), 5_000_000, 30));
    }

    /// 配置层把周期夹在 1 以上，这里再防一次：`0` 不能变成"每毫秒都到点"。
    #[test]
    fn zero_interval_cannot_become_a_hot_loop() {
        assert!(!due_for_scheduled_backup(Some(0), 1, 0));
        assert!(due_for_scheduled_backup(Some(0), 60_000, 0));
    }
}
