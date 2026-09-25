//! Windows 端口占用查询与进程停止。
//!
//! 这组命令只服务于 MCP 设置页的排障工具：先按端口列出占用进程，
//! 用户逐项点击停止。不会自动杀进程，也不会因为端口冲突擅自改用其它端口。

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortProcess {
    pub pid: u32,
    pub process_name: String,
    pub executable_path: Option<String>,
    pub local_address: String,
    pub state: String,
    pub can_terminate: bool,
    pub is_current_process: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StopProcessResult {
    pub stopped: bool,
    pub launched_elevated: bool,
    pub requires_admin: bool,
    pub message: String,
}

fn port_from_local_address(address: &str) -> Option<u16> {
    address.rsplit_once(':')?.1.parse().ok()
}

fn parse_netstat_line(line: &str, port: u16) -> Option<(String, String, u32)> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 5 || !fields[0].eq_ignore_ascii_case("TCP") {
        return None;
    }
    if port_from_local_address(fields[1])? != port {
        return None;
    }
    let pid = fields[4].parse().ok()?;
    Some((fields[1].to_string(), fields[3].to_string(), pid))
}

#[cfg(windows)]
fn process_identity(pid: u32) -> (String, Option<String>, bool) {
    use std::path::Path;
    use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED};
    use windows::Win32::System::ProcessStatus::GetModuleFileNameExW;
    use windows::Win32::System::Threading::{
        GetCurrentProcessId, OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    };

    let current = unsafe { GetCurrentProcessId() } == pid;
    let handle = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid) };
    let Ok(handle) = handle else {
        return ("未知进程".to_string(), None, !current);
    };
    let mut path_buf = [0u16; 2048];
    let length = unsafe { GetModuleFileNameExW(Some(handle), None, &mut path_buf) };
    let path = if length > 0 {
        Some(String::from_utf16_lossy(&path_buf[..length as usize]))
    } else {
        None
    };
    unsafe { CloseHandle(handle).ok(); }
    let name = path
        .as_deref()
        .and_then(|value| Path::new(value).file_name())
        .and_then(|value| value.to_str())
        .unwrap_or("未知进程")
        .to_string();
    (name, path, !current)
}

#[cfg(not(windows))]
fn process_identity(_pid: u32) -> (String, Option<String>, bool) {
    ("仅 Windows 可用".to_string(), None, false)
}

/// 查询指定 TCP 端口的占用进程。
#[tauri::command]
pub fn inspect_mcp_port_occupancy(port: u16) -> Result<Vec<PortProcess>, String> {
    if !(1..=65535).contains(&port) {
        return Err("端口需在 1-65535 之间".to_string());
    }
    #[cfg(not(windows))]
    {
        let _ = port;
        return Err("端口占用查询仅支持 Windows".to_string());
    }
    #[cfg(windows)]
    {
        let output = std::process::Command::new("netstat.exe")
            .args(["-ano", "-p", "tcp"])
            .output()
            .map_err(|e| format!("无法执行 Windows netstat：{}", e))?;
        if !output.status.success() {
            return Err(format!("netstat 执行失败：{}", String::from_utf8_lossy(&output.stderr)));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut result = Vec::new();
        for line in text.lines() {
            let Some((local_address, state, pid)) = parse_netstat_line(line, port) else {
                continue;
            };
            let (process_name, executable_path, can_terminate) = process_identity(pid);
            result.push(PortProcess {
                pid,
                process_name,
                executable_path,
                local_address,
                state,
                can_terminate,
                is_current_process: pid == std::process::id(),
            });
        }
        result.sort_by_key(|item| (item.pid, item.local_address.clone()));
        result.dedup_by(|a, b| a.pid == b.pid && a.local_address == b.local_address);
        Ok(result)
    }
}

/// 尝试停止一个占用端口的进程。权限不足时不静默提权，返回 requiresAdmin=true。
#[tauri::command]
pub fn stop_mcp_port_process(pid: u32) -> Result<StopProcessResult, String> {
    if pid == 0 || pid == std::process::id() {
        return Ok(StopProcessResult {
            stopped: false,
            launched_elevated: false,
            requires_admin: false,
            message: "不能停止当前 Tiez-Next 进程或无效进程".to_string(),
        });
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        return Err("停止端口占用进程仅支持 Windows".to_string());
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED};
        use windows::Win32::System::Threading::{
            OpenProcess, TerminateProcess, PROCESS_TERMINATE,
        };
        let handle = unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) };
        let Ok(handle) = handle else {
            let access_denied = unsafe { GetLastError() } == ERROR_ACCESS_DENIED;
            return Ok(StopProcessResult {
                stopped: false,
                launched_elevated: false,
                requires_admin: access_denied,
                message: if access_denied { "权限不足，需要管理员权限" } else { "进程不存在或无法打开" }.to_string(),
            });
        };
        let result = unsafe { TerminateProcess(handle, 1) };
        unsafe { CloseHandle(handle).ok(); }
        if result.is_ok() {
            Ok(StopProcessResult {
                stopped: true,
                launched_elevated: false,
                requires_admin: false,
                message: "已请求停止进程".to_string(),
            })
        } else {
            Ok(StopProcessResult {
                stopped: false,
                launched_elevated: false,
                requires_admin: true,
                message: "停止失败，可能需要管理员权限".to_string(),
            })
        }
    }
}

/// 以管理员权限启动 taskkill。这里只负责发起 UAC，不假装已经停止；前端随后刷新列表。
#[tauri::command]
pub fn stop_mcp_port_process_as_admin(pid: u32) -> Result<StopProcessResult, String> {
    if pid == 0 || pid == std::process::id() {
        return Err("不能停止当前 Tiez-Next 进程或无效进程".to_string());
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        return Err("管理员停止仅支持 Windows".to_string());
    }
    #[cfg(windows)]
    {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
        let exe = OsStr::new("taskkill.exe").encode_wide().chain(std::iter::once(0)).collect::<Vec<_>>();
        let verb = OsStr::new("runas").encode_wide().chain(std::iter::once(0)).collect::<Vec<_>>();
        let args = format!("/PID {} /F", pid);
        let args_w = OsStr::new(&args).encode_wide().chain(std::iter::once(0)).collect::<Vec<_>>();
        let result = unsafe {
            ShellExecuteW(None, PCWSTR::from_raw(verb.as_ptr()), PCWSTR::from_raw(exe.as_ptr()), PCWSTR::from_raw(args_w.as_ptr()), PCWSTR::null(), SW_HIDE)
        };
        if result.0 as usize <= 32 {
            return Err("管理员权限请求被取消或无法启动 taskkill".to_string());
        }
        Ok(StopProcessResult {
            stopped: false,
            launched_elevated: true,
            requires_admin: false,
            message: "已发起管理员停止请求，请稍候刷新".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_netstat_line, port_from_local_address};

    #[test]
    fn parses_ipv4_and_ipv6_netstat_rows() {
        assert_eq!(port_from_local_address("0.0.0.0:23123"), Some(23123));
        assert_eq!(port_from_local_address("[::]:23123"), Some(23123));
        assert_eq!(parse_netstat_line("TCP    0.0.0.0:23123    0.0.0.0:0    LISTENING    1234", 23123).unwrap().2, 1234);
    }

    #[test]
    fn ignores_other_ports_and_non_tcp_rows() {
        assert!(parse_netstat_line("TCP    0.0.0.0:23124    0.0.0.0:0    LISTENING    1234", 23123).is_none());
        assert!(parse_netstat_line("UDP    0.0.0.0:23123    *:*    1234", 23123).is_none());
    }
}
