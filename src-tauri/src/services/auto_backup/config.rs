//! 自动容灾备份的**配置**：四个开关/数值项、默认值、取值范围与校验。
//!
//! # 为什么配置是独立的四个键
//!
//! 用户原话把这件事说成"定时备份总开关 + 周期 + 最大留存 + 启动备份（独立勾选项）"。
//! 四项各自独立持久化，于是"启动备份"确实**不随定时备份开关变化**——它读自己的键，
//! 定时开关只影响定时循环，不会顺手改动它。
//!
//! # 越界值为什么是"夹紧 + 回落"而不是"报错"
//!
//! [`load`] 面向的是**已经存在**的库（可能是旧版写的、被手工改过、或者被同步回来的）。
//! 在这里报错等于让整个备份功能不可用；而静默接受 `max_keep = 0` 又会让轮换把备份删空。
//! 因此读取消极防御：解析失败的回落默认值，超范围的夹进合法区间；**只有用户显式写入
//! 才做严格校验并报错**（[`validate_max_keep`] / [`validate_interval_minutes`]）。

use crate::error::AppResult;
use crate::infrastructure::repository::settings_repo::SettingsRepository;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// 定时备份总开关。默认**开**（用户明确要求）。
pub const KEY_ENABLED: &str = "auto_backup.enabled";
/// 定时备份周期（分钟）。
pub const KEY_INTERVAL_MINUTES: &str = "auto_backup.interval_minutes";
/// 最大留存份数。
pub const KEY_MAX_KEEP: &str = "auto_backup.max_keep";
/// 启动时备份一次（独立勾选项，**不受** [`KEY_ENABLED`] 约束）。
pub const KEY_BACKUP_ON_STARTUP: &str = "auto_backup.backup_on_startup";

/// 本功能全部设置键的前缀。
///
/// 云同步按这个前缀**整族排除**：这些值描述的是"这台机器怎么保管自己的容灾副本"，
/// 一台机器上的份数上限不该由另一台机器（或一个被篡改的远端快照）决定。
pub const KEY_PREFIX: &str = "auto_backup.";

pub const DEFAULT_ENABLED: bool = true;
pub const DEFAULT_INTERVAL_MINUTES: u32 = 30;
pub const DEFAULT_MAX_KEEP: u32 = 20;
/// 启动备份默认也开。
///
/// 【这是一处推断，标注在案】用户原话"软件刚启动默认自动备份一次也是可选项，不随定时
/// 备份开关约束，只是放在里面作为一个勾选项而已"只说明了它**是一个独立勾选项**，没有
/// 明说默认开还是关。取"默认开"的理由：它与定时备份同属容灾保险，定时备份默认开；
/// 若默认关，多数用户永远不会发现这个勾选项，容灾就少了一层。用户可自行取消。
pub const DEFAULT_BACKUP_ON_STARTUP: bool = true;

/// 最大留存份数的合法区间（用户原话：可调 1–200）。
pub const MAX_KEEP_MIN: u32 = 1;
pub const MAX_KEEP_MAX: u32 = 200;
/// 周期的合法区间。
///
/// 用户只指定了默认 30 分钟，没有给上下界。下界取 1 分钟（再密无意义且会挤满磁盘），
/// 上界取 24 小时（再长就不叫"定时备份"了）。
pub const INTERVAL_MINUTES_MIN: u32 = 1;
pub const INTERVAL_MINUTES_MAX: u32 = 1440;

/// 配置的四个字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoBackupConfig {
    /// 定时备份开关。
    pub enabled: bool,
    /// 定时备份周期（分钟）。
    pub interval_minutes: u32,
    /// 最大留存份数（1–200）。
    pub max_keep: u32,
    /// 启动时备份一次（独立于 [`Self::enabled`]）。
    pub backup_on_startup: bool,
}

impl Default for AutoBackupConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_ENABLED,
            interval_minutes: DEFAULT_INTERVAL_MINUTES,
            max_keep: DEFAULT_MAX_KEEP,
            backup_on_startup: DEFAULT_BACKUP_ON_STARTUP,
        }
    }
}

impl AutoBackupConfig {
    /// 允许被固定的**最大**条数 = 最大留存份数 − 1。
    ///
    /// 用户原话："固定的不能大于最大存的数量-1……保留一个位置由于轮换"。
    /// `max_keep = 1` 时结果是 0：一份都不许固定——因为那一份必须随时可被下一份顶掉，
    /// 否则新备份无处可放。
    pub fn max_pinned(&self) -> u32 {
        self.max_keep.saturating_sub(1)
    }
}

/// 配置的**局部更新**。
///
/// 用 `Option` 而不是整个结构：前端只发"用户改了哪一项"，未出现的字段保持原值。
/// 这样"改周期"不会顺手把"启动备份"重置回默认值——那正是用户特别强调不要发生的耦合。
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConfigPatch {
    pub enabled: Option<bool>,
    pub interval_minutes: Option<u32>,
    pub max_keep: Option<u32>,
    pub backup_on_startup: Option<bool>,
}

impl ConfigPatch {
    /// 把补丁套用到现有配置上。
    pub fn apply(self, base: AutoBackupConfig) -> AutoBackupConfig {
        AutoBackupConfig {
            enabled: self.enabled.unwrap_or(base.enabled),
            interval_minutes: self.interval_minutes.unwrap_or(base.interval_minutes),
            max_keep: self.max_keep.unwrap_or(base.max_keep),
            backup_on_startup: self.backup_on_startup.unwrap_or(base.backup_on_startup),
        }
    }
}

/// 校验失败的原因。每个变体有稳定的机器可读码，前端据此映射成当前语言的人话。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    /// 最大留存份数超出 1–200。
    MaxKeepOutOfRange { value: u32 },
    /// 周期超出 1–1440 分钟。
    IntervalOutOfRange { value: u32 },
}

impl ConfigError {
    pub fn code(&self) -> &'static str {
        match self {
            ConfigError::MaxKeepOutOfRange { .. } => "auto_backup_max_keep_out_of_range",
            ConfigError::IntervalOutOfRange { .. } => "auto_backup_interval_out_of_range",
        }
    }

    /// 结构化载荷：`code` + 越界的实际值 + 合法区间。
    ///
    /// 前端无需解析文案即可拼出"1–200"这类提示，也不必在前后端各写一份边界。
    pub fn payload(&self) -> serde_json::Value {
        match self {
            ConfigError::MaxKeepOutOfRange { value } => json!({
                "code": self.code(),
                "value": value,
                "min": MAX_KEEP_MIN,
                "max": MAX_KEEP_MAX,
            }),
            ConfigError::IntervalOutOfRange { value } => json!({
                "code": self.code(),
                "value": value,
                "min": INTERVAL_MINUTES_MIN,
                "max": INTERVAL_MINUTES_MAX,
            }),
        }
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::MaxKeepOutOfRange { value } => write!(
                f,
                "最大留存备份份数必须在 {}–{} 之间（当前为 {}）。",
                MAX_KEEP_MIN, MAX_KEEP_MAX, value
            ),
            ConfigError::IntervalOutOfRange { value } => write!(
                f,
                "定时备份周期必须在 {}–{} 分钟之间（当前为 {}）。",
                INTERVAL_MINUTES_MIN, INTERVAL_MINUTES_MAX, value
            ),
        }
    }
}

/// 校验用户显式写入的最大留存份数。
pub fn validate_max_keep(value: u32) -> Result<(), ConfigError> {
    if !(MAX_KEEP_MIN..=MAX_KEEP_MAX).contains(&value) {
        return Err(ConfigError::MaxKeepOutOfRange { value });
    }
    Ok(())
}

/// 校验用户显式写入的周期。
pub fn validate_interval_minutes(value: u32) -> Result<(), ConfigError> {
    if !(INTERVAL_MINUTES_MIN..=INTERVAL_MINUTES_MAX).contains(&value) {
        return Err(ConfigError::IntervalOutOfRange { value });
    }
    Ok(())
}

/// 校验整份配置（写入前调用）。
pub fn validate(cfg: &AutoBackupConfig) -> Result<(), ConfigError> {
    validate_max_keep(cfg.max_keep)?;
    validate_interval_minutes(cfg.interval_minutes)
}

/// 从设置库读出配置（**消极防御**：坏值回落默认，越界值夹进合法区间）。
pub fn load(repo: &impl SettingsRepository) -> AutoBackupConfig {
    let raw = repo.get_all().unwrap_or_default();
    let d = AutoBackupConfig::default();

    let enabled = raw
        .get(KEY_ENABLED)
        .map(|v| v.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(d.enabled);
    let backup_on_startup = raw
        .get(KEY_BACKUP_ON_STARTUP)
        .map(|v| v.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(d.backup_on_startup);

    let interval_minutes = raw
        .get(KEY_INTERVAL_MINUTES)
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(d.interval_minutes)
        // 夹紧而不是回落默认：`0` 会被当成"每隔 0 分钟"从而疯狂备份，
        // 这个方向必须堵死；而 99999 分钟同样是无效配置，夹到上界即可。
        .clamp(INTERVAL_MINUTES_MIN, INTERVAL_MINUTES_MAX);

    let max_keep = raw
        .get(KEY_MAX_KEEP)
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(d.max_keep)
        // 同上：`0` 会让轮换把全部备份（含刚生成的）判为超限，必须夹到 1。
        .clamp(MAX_KEEP_MIN, MAX_KEEP_MAX);

    AutoBackupConfig {
        enabled,
        interval_minutes,
        max_keep,
        backup_on_startup,
    }
}

/// 把配置写回设置库。调用方需自行保证已通过 [`validate`]。
pub fn save(repo: &impl SettingsRepository, cfg: &AutoBackupConfig) -> AppResult<()> {
    repo.set(KEY_ENABLED, if cfg.enabled { "true" } else { "false" })
        .map_err(|e| crate::error::AppError::Database(e.to_string()))?;
    repo.set(KEY_INTERVAL_MINUTES, &cfg.interval_minutes.to_string())
        .map_err(|e| crate::error::AppError::Database(e.to_string()))?;
    repo.set(KEY_MAX_KEEP, &cfg.max_keep.to_string())
        .map_err(|e| crate::error::AppError::Database(e.to_string()))?;
    repo.set(
        KEY_BACKUP_ON_STARTUP,
        if cfg.backup_on_startup { "true" } else { "false" },
    )
    .map_err(|e| crate::error::AppError::Database(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(max_keep: u32) -> AutoBackupConfig {
        AutoBackupConfig {
            max_keep,
            ..AutoBackupConfig::default()
        }
    }

    /// 默认值就是用户当初定的那三个数：开、30 分钟、20 份。
    #[test]
    fn defaults_match_the_users_numbers() {
        let d = AutoBackupConfig::default();
        assert!(d.enabled, "定时备份默认必须是开的");
        assert_eq!(d.interval_minutes, 30);
        assert_eq!(d.max_keep, 20);
    }

    /// 固定上限的算术：`max_keep - 1`，且 `max_keep = 1` 时是 0（一份都不许固定）。
    #[test]
    fn max_pinned_is_one_less_than_max_keep() {
        assert_eq!(cfg(50).max_pinned(), 49, "用户原话的 50 → 49");
        assert_eq!(cfg(200).max_pinned(), 199);
        assert_eq!(cfg(2).max_pinned(), 1);
        assert_eq!(cfg(1).max_pinned(), 0, "只剩一个位置时必须留给轮换");
    }

    /// 用户明确要求：1–200 之外的份数必须被拒绝，而不是静默夹紧。
    #[test]
    fn out_of_range_max_keep_is_rejected() {
        for bad in [0u32, 201, 1000, u32::MAX] {
            let err = validate_max_keep(bad).expect_err("越界值必须报错");
            assert_eq!(err.code(), "auto_backup_max_keep_out_of_range");
            assert_eq!(err.payload()["max"], MAX_KEEP_MAX);
        }
        assert!(validate_max_keep(1).is_ok(), "下界 1 合法");
        assert!(validate_max_keep(200).is_ok(), "上界 200 合法");
    }

    /// 局部更新只改出现的字段：改周期不能把"启动备份"重置掉。
    ///
    /// 这正是用户强调的那条约束（启动备份不随定时开关变化）在配置层的落点。
    #[test]
    fn patch_only_touches_the_fields_it_carries() {
        let base = AutoBackupConfig {
            enabled: false,
            interval_minutes: 45,
            max_keep: 7,
            backup_on_startup: false,
        };
        let patched = ConfigPatch {
            enabled: Some(true),
            ..ConfigPatch::default()
        }
        .apply(base);
        assert!(patched.enabled);
        assert_eq!(patched.interval_minutes, 45, "未提到的字段必须保持原值");
        assert_eq!(patched.max_keep, 7);
        assert!(!patched.backup_on_startup, "改定时开关不得联动启动备份");
    }

    /// 坏值不炸整个功能：解析失败回落默认，`0` 被夹到合法下界。
    #[test]
    fn corrupt_stored_values_never_disable_the_feature_or_delete_everything() {
        use crate::infrastructure::repository::settings_repo::SqliteSettingsRepository;
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .unwrap();
        let repo = SqliteSettingsRepository::new(std::sync::Arc::new(std::sync::Mutex::new(conn)));

        // 全空 → 默认
        let loaded = load(&repo);
        assert_eq!(loaded, AutoBackupConfig::default());

        // 手工塞入垃圾与 0
        repo.set(KEY_MAX_KEEP, "0").unwrap();
        repo.set(KEY_INTERVAL_MINUTES, "not-a-number").unwrap();
        repo.set(KEY_ENABLED, "yes-please").unwrap();
        let loaded = load(&repo);
        assert_eq!(loaded.max_keep, 1, "0 必须被夹到 1，不能让它把备份删空");
        assert_eq!(
            loaded.interval_minutes, DEFAULT_INTERVAL_MINUTES,
            "垃圾周期回落默认"
        );
        assert!(!loaded.enabled, "非 true 一律视为关，避免误解为开启");
    }

    /// 写—读往返：四个字段都要原样回来。
    #[test]
    fn config_round_trips_through_the_settings_table() {
        use crate::infrastructure::repository::settings_repo::SqliteSettingsRepository;
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .unwrap();
        let repo = SqliteSettingsRepository::new(std::sync::Arc::new(std::sync::Mutex::new(conn)));

        let written = AutoBackupConfig {
            enabled: false,
            interval_minutes: 90,
            max_keep: 200,
            backup_on_startup: false,
        };
        save(&repo, &written).unwrap();
        assert_eq!(load(&repo), written);
    }
}
