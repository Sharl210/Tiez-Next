//! 需要用户**手动操作**的系统级设置清单。
//!
//! # 这一层为什么必须存在
//!
//! 应用能改的东西分三类：
//!
//! | 级别 | 例子 | 应用能不能自己做 |
//! |---|---|---|
//! | 应用自己的设置 | 主题、端口、数据库路径 | ✅ 能，且必须自己做 |
//! | 当前用户可写的系统项 | `HKCU\...\Run` 开机自启动 | ✅ 能（**不需要管理员权限**），但要"写后回读"确认 |
//! | **系统强制介入项** | 防火墙放行、UAC 提权确认、任务管理器禁用启动项 | ❌ **做不到**，只能用户手动 |
//!
//! 第三类只有三项。它们不是"应用偷懒"—— 防火墙规则需要管理员令牌、UAC 是系统弹窗、
//! 任务管理器里的"已禁用"状态由 `StartupApproved` 覆盖 `Run` 键的意图，三者都**没有**
//! 让应用静默代做的接口。
//!
//! # 为什么**不能**用"已读标记"
//!
//! 同类的"凭据暴露告知"用的是"这条已展示并处理过"的一次性标记。**本清单不能照抄**：
//! 凭据告知是**一次性事件**（用户看过就完了），而系统级设置的状态是**客观可变的** ——
//! 用户今天放行了防火墙，明天可能又去任务管理器把启动项禁掉。
//!
//! 用标记掩盖的后果是：**"上次点了知道了，这次真坏了却不提示"**。
//!
//! ⇒ 因此本模块的每一条都要求**每轮实时探测**；只有用户明确说"不想再被提醒"时，
//! 才在 `security.system_setting_<id>_ack` 落一个标记（见 [`ACK_KEY_PREFIX`]）。

use serde::Serialize;

/// 用户说"不想再被提醒"时落下的标记键前缀。**默认不写任何标记。**
pub const ACK_KEY_PREFIX: &str = "security.system_setting_";

/// 清单里的一项。
///
/// `state` 是**探测结果**，不是缓存的记忆值。`satisfied` 为 `true` 表示"这一项已经不需要
/// 用户处理了"，界面据此把它收起来或标成已完成。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SystemChecklistItem {
    /// 稳定标识，用于 ack 键名与前端 i18n 键（`system_check_<id>_title` 等）。
    pub id: String,
    /// 当前是否已满足。**由探测得出，不由标记得出。**
    pub satisfied: bool,
    /// 探测本身是否可信。
    ///
    /// 【为什么需要它】探测失败时若回 `satisfied: false`，界面会把它显示成"待处理"，
    /// 而实际是"不知道" —— 用户会去做一件本来不需要做的事，或者以为功能坏了。
    /// 探测失败必须是**第三种状态**，不能与"未满足"混为一谈。
    pub probe_ok: bool,
    /// 供界面展示的**探测细节**（例如实际读到的防火墙规则名、注册表值）。
    /// 这是"真的查过了"的唯一可信证据，不给用户看原文就只能让他信一个布尔值。
    pub detail: Option<String>,
    /// 用户是否已要求不再提醒（读自 ack 键）。
    pub ack: bool,
}

/// 由三项探测结果组装清单（**纯函数**，可在任意平台单测）。
///
/// 把组装从平台探测里拆出来，是为了让"探测失败必须与未满足区分开""ack 不影响探测结果"
/// 这两条关键判据能在没有 Windows 的机器上被真正断言 —— 否则它们只能靠真机人工验证。
///
/// `probes` 的每一项是 `(id, 探测结果)`，`探测结果` 为 `Result<Option<探测细节>, 错误>`：
/// - `Ok(Some(detail))` —— 探测成功且已满足，`detail` 是证据原文
/// - `Ok(None)` —— 探测成功但未满足
/// - `Err(e)` —— 探测本身失败（权限、命令不存在等）
pub fn build_checklist(probes: &[(String, Result<Option<String>, String>)], acks: &[String]) -> Vec<SystemChecklistItem> {
    probes
        .iter()
        .map(|(id, probe)| {
            let (satisfied, probe_ok, detail) = match probe {
                Ok(Some(d)) => (true, true, Some(d.clone())),
                Ok(None) => (false, true, None),
                Err(e) => (false, false, Some(e.clone())),
            };
            SystemChecklistItem {
                id: id.clone(),
                satisfied,
                probe_ok,
                detail,
                ack: acks.iter().any(|a| a == id),
            }
        })
        .collect()
}

/// 清单的稳定 ID 列表。**只有三项** —— 不要凭想象扩大。
pub const CHECKLIST_IDS: [&str; 3] = ["firewall", "uac", "startup_approved"];

/// 组装 ack 键名。
pub fn ack_key(id: &str) -> String {
    format!("{ACK_KEY_PREFIX}{id}_ack")
}

/// 探测：Windows 防火墙是否已放行本程序。
///
/// 【本机无法验证】`netsh advfirewall` 调用与防火墙规则的查询语义只在真实 Windows 上
/// 成立。此处的实现按 `netsh advfirewall firewall show rule name=all` 的输出解析，
/// **未在真机上跑过**。
#[cfg(target_os = "windows")]
pub fn probe_firewall(exe_name: &str) -> Result<Option<String>, String> {
    use std::process::Command;
    let out = Command::new("netsh")
        .args(["advfirewall", "firewall", "show", "rule", "name=all"])
        .output()
        .map_err(|e| format!("无法执行 netsh：{e}"))?;
    if !out.status.success() {
        return Err(format!("netsh 返回非零：{}", out.status));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // 找规则名里含本程序名的块，确认其 Action 是 Allow。
    let mut in_block = false;
    let mut action_allow = false;
    let mut matched = None;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("Rule Name:") {
            if in_block && action_allow {
                return Ok(Some(matched.unwrap_or_default()));
            }
            in_block = t.to_ascii_lowercase().contains(&exe_name.to_ascii_lowercase());
            action_allow = false;
        } else if in_block && t.starts_with("Action:") {
            action_allow = t.to_ascii_lowercase().contains("allow");
            matched = Some(t.to_string());
        }
    }
    if in_block && action_allow {
        return Ok(Some(matched.unwrap_or_default()));
    }
    Ok(None)
}

/// 探测：当前进程是否已提权（UAC）。
///
/// 复用既有的 `check_is_admin()`，不另写一套令牌检查 —— 判据只允许存在一处。
#[cfg(target_os = "windows")]
pub fn probe_uac() -> Result<Option<String>, String> {
    if crate::app::commands::system_cmd::check_is_admin() {
        Ok(Some("已以管理员身份运行".to_string()))
    } else {
        Ok(None)
    }
}

/// 探测：任务管理器是否把本程序的启动项禁用了。
///
/// 【为什么这条必须存在】`StartupApproved` 里的二值状态会**覆盖** `Run` 键的开启意图 ——
/// 应用把 `Run` 写对了、回读也一致，用户仍可能因为任务管理器里点过"禁用"而开机不启动。
/// 这是一个**应用看不到**的失败面，只能探测后告知用户。
#[cfg(target_os = "windows")]
pub fn probe_startup_approved(app_name: &str) -> Result<Option<String>, String> {
    use winreg::enums::*;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu
        .open_subkey_with_flags(
            "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run",
            KEY_READ,
        )
        .map_err(|e| format!("无法打开 StartupApproved：{e}"))?;
    let raw: Result<Vec<u8>, _> = key.get_raw_value(app_name).map(|v| v.bytes);
    match raw {
        Ok(bytes) => {
            // 该值是 12 字节二进制；首字节 0x02/0x06 表示"已启用"，0x03 表示"已禁用"
            // （具体取值随 Windows 版本有差异，故只把原始字节回传给用户看，不硬判）。
            let enabled = bytes.first().map(|b| *b != 0x03).unwrap_or(true);
            if enabled {
                Ok(Some(format!("StartupApproved 首字节 = 0x{:02x}", bytes.first().unwrap_or(&0))))
            } else {
                Ok(None)
            }
        }
        Err(_) => Ok(None), // 值不存在 = 未被禁用过
    }
}

/// 非 Windows 平台：三项都返回"探测不可用"。
///
/// **不能返回 `Ok(None)`** —— 那会被界面显示成"未满足"，让用户以为需要处理。
/// 必须是 `Err`，即"探测不可用"这一独立状态。
#[cfg(not(target_os = "windows"))]
pub fn probe_firewall(_exe_name: &str) -> Result<Option<String>, String> {
    Err("防火墙探测仅支持 Windows".to_string())
}

#[cfg(not(target_os = "windows"))]
pub fn probe_uac() -> Result<Option<String>, String> {
    Err("UAC 探测仅支持 Windows".to_string())
}

#[cfg(not(target_os = "windows"))]
pub fn probe_startup_approved(_app_name: &str) -> Result<Option<String>, String> {
    Err("启动项状态探测仅支持 Windows".to_string())
}

// ---------------------------------------------------------------------------
// 命令层
// ---------------------------------------------------------------------------

use crate::app_state::AppDataDir;
use crate::error::{AppError, AppResult};
use crate::infrastructure::repository::settings_repo::SettingsRepository;
use tauri::{AppHandle, Manager};

/// 实时探测三项系统级设置（**只读**，不写任何设置）。
///
/// 返回的每一项都带探测细节与"探测是否可信"，界面据此展示。
pub fn probe_system_checklist(
    repo: &impl SettingsRepository,
    exe_name: &str,
    app_name: &str,
) -> Vec<SystemChecklistItem> {
    let probes = vec![
        ("firewall".to_string(), probe_firewall(exe_name)),
        ("uac".to_string(), probe_uac()),
        ("startup_approved".to_string(), probe_startup_approved(app_name)),
    ];
    let acks: Vec<String> = CHECKLIST_IDS
        .iter()
        .filter(|id| {
            repo.get(&ack_key(id))
                .ok()
                .flatten()
                .map(|v| v == "true")
                .unwrap_or(false)
        })
        .map(|s| s.to_string())
        .collect();
    build_checklist(&probes, &acks)
}

#[tauri::command]
pub fn get_system_checklist(
    app: AppHandle,
    repo: tauri::State<'_, std::sync::Arc<crate::infrastructure::repository::settings_repo::SqliteSettingsRepository>>,
) -> AppResult<Vec<SystemChecklistItem>> {
    let _ = &app;
    let exe_name = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "tiez-next.exe".to_string());
    let app_name = exe_name.trim_end_matches(".exe").to_string();
    let _: Option<&AppDataDir> = app.try_state::<AppDataDir>().as_deref();
    Ok(probe_system_checklist(repo.inner().as_ref(), &exe_name, &app_name))
}

/// 用户明确要求"这一项不再提醒"。
///
/// **只有显式调用才会写标记** —— 探测本身绝不写任何东西。标记的语义是"用户可以接受
/// 这一项不被处理"，不是"这一项已满足"。界面仍会显示探测到的真实状态。
#[tauri::command]
pub fn ack_system_checklist_item(
    repo: tauri::State<'_, std::sync::Arc<crate::infrastructure::repository::settings_repo::SqliteSettingsRepository>>,
    id: String,
) -> AppResult<()> {
    if !CHECKLIST_IDS.contains(&id.as_str()) {
        return Err(AppError::Validation(format!("未知的清单项：{id}")));
    }
    repo.set(&ack_key(&id), "true")
        .map_err(|e| AppError::Internal(e.to_string()))?;
    // 写后回读：与自启动同一约定 —— 界面显示的状态必须来自真实存储，不能靠乐观置位。
    let readback = repo.get(&ack_key(&id)).ok().flatten();
    if readback.as_deref() != Some("true") {
        return Err(AppError::Internal(format!(
            "标记写入后回读不一致（读到 {readback:?}）"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// **探测失败必须与"未满足"区分开。**
    ///
    /// 两者若都回 `satisfied: false`，界面会把"探测不可用"显示成"待处理" ——
    /// 用户会去做一件本来不需要做的事。这条在非 Windows 上尤其重要：
    /// 三项探测**全部**会失败，若混同，界面会显示三个"待处理"。
    #[test]
    fn probe_failure_is_distinct_from_unsatisfied() {
        let probes = vec![
            ("firewall".to_string(), Err("探测不可用".to_string())),
            ("uac".to_string(), Ok(None)),
        ];
        let items = build_checklist(&probes, &[]);

        assert!(!items[0].satisfied && !items[0].probe_ok, "探测失败：satisfied=false 且 probe_ok=false");
        assert!(!items[1].satisfied && items[1].probe_ok, "未满足：satisfied=false 但 probe_ok=true");
        assert_ne!(
            (items[0].satisfied, items[0].probe_ok),
            (items[1].satisfied, items[1].probe_ok),
            "两者必须能被区分 —— 否则界面无法把「不知道」与「待处理」分开"
        );
    }

    /// **已满足时探测细节必须回传** —— 它是"真的查过了"的唯一证据。
    #[test]
    fn satisfied_item_carries_probe_detail() {
        let probes = vec![("firewall".to_string(), Ok(Some("Action: Allow".to_string())))];
        let items = build_checklist(&probes, &[]);
        assert!(items[0].satisfied);
        assert_eq!(items[0].detail.as_deref(), Some("Action: Allow"));
    }

    /// **ack 标记不影响探测结果。**
    ///
    /// 标记的语义是"用户可以接受这一项不被处理"，**不是**"这一项已满足"。
    /// 若 ack 把 `satisfied` 也带上去，界面就会停止显示真实状态 —— 正是要避免的
    /// "上次点了知道了，这次真坏了却不提示"。
    #[test]
    fn ack_does_not_change_satisfaction() {
        let probes = vec![("firewall".to_string(), Ok(None))];
        let without = build_checklist(&probes, &[]);
        let with = build_checklist(&probes, &["firewall".to_string()]);

        assert!(!without[0].ack && with[0].ack, "ack 应当被读出");
        assert_eq!(with[0].satisfied, without[0].satisfied, "ack 不得改变 satisfied");
        assert_eq!(with[0].probe_ok, without[0].probe_ok, "ack 不得改变 probe_ok");
        assert!(!with[0].satisfied, "ack 之后真实状态仍是「未满足」");
    }

    /// `ack` 键名唯一且带前缀，便于审计与清理。
    #[test]
    fn ack_key_is_namespaced() {
        assert_eq!(ack_key("firewall"), "security.system_setting_firewall_ack");
    }

    /// 清单**只有三项** —— 这条钉住"不要凭想象扩大范围"。
    #[test]
    fn checklist_has_exactly_three_items() {
        assert_eq!(CHECKLIST_IDS.len(), 3);
        assert_eq!(CHECKLIST_IDS, ["firewall", "uac", "startup_approved"]);
    }

    /// **非 Windows 上三项探测都必须返回 Err（探测不可用），不得返回 Ok(None)。**
    ///
    /// 返回 `Ok(None)` 会被界面显示成"未满足"= 让用户去处理一件做不到的事。
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn non_windows_probes_report_unavailable_not_unsatisfied() {
        assert!(probe_firewall("tiez-next.exe").is_err(), "非 Windows 上防火墙探测必须报「不可用」");
        assert!(probe_uac().is_err(), "非 Windows 上 UAC 探测必须报「不可用」");
        assert!(probe_startup_approved("tiez-next").is_err(), "非 Windows 上启动项探测必须报「不可用」");
    }

    /// 真实探测路径在非 Windows 上产生的清单：三项都应为 `probe_ok: false`。
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn real_probes_on_non_windows_yield_unavailable() {
        let probes = vec![
            ("firewall".to_string(), probe_firewall("x.exe")),
            ("uac".to_string(), probe_uac()),
            ("startup_approved".to_string(), probe_startup_approved("x")),
        ];
        let items = build_checklist(&probes, &[]);
        assert!(items.iter().all(|i| !i.probe_ok), "三项都应标记为探测不可用");
        assert!(items.iter().all(|i| !i.satisfied), "都不应被当成「已满足」");
    }
}
