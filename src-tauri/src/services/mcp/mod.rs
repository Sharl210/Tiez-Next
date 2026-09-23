//! 进程内 MCP 服务：让 AI 读写本机剪贴板历史与标签。
//!
//! # 模块结构
//!
//! * [`jsonrpc`]：JSON-RPC 2.0 消息模型（解析 / 响应组装）。
//! * [`store`]：数据访问面与配置键。
//! * [`tools`]：工具清单（`tools/list` 的内容）与调用实现。
//! * [`server`]：Streamable HTTP 端点、鉴权、Origin 校验、协议分发、端口绑定。
//! * 本文件：宿主生命周期（起停、开关从数据库读取、token 生成）与 Tauri 命令。
//!
//! # 为什么不做 stdio
//!
//! `main.rs` 装了 `tauri-plugin-single-instance`，stdio 会被单实例插件与宿主进程
//! 的父子关系搅在一起。因此只提供绑定 `127.0.0.1` 的进程内 HTTP。

pub mod jsonrpc;
#[cfg(test)]
mod selfcheck;
pub mod server;
pub mod store;
pub mod tools;

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tauri::{AppHandle, Manager};
use tokio::task::JoinHandle;

use crate::app_state::{AppDataDir, EncryptionQueueState};
use crate::database::DbState;
use crate::infrastructure::repository::settings_repo::SettingsRepository;

pub use server::{AuthToken, RuntimeSettings, ServerState};
pub use store::{DEFAULT_PORT, KEY_ALLOW_WRITE, KEY_AUTOSTART, KEY_ENABLED, KEY_PORT, KEY_TOKEN};

/// 当前运行的 server 任务句柄。
static SERVER_HANDLE: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

/// 实际监听端口（0 表示未运行）。
static ACTIVE_PORT: AtomicU16 = AtomicU16::new(0);

/// 由宿主导入的审计日志写入器。未安装时退化为"不记录"，绝不 panic。
static AUDIT_SINK: OnceLock<Arc<dyn server::AuditSink>> = OnceLock::new();

/// 由宿主导入的副作用实现（发事件、云同步、加解密入队）。
static HOST_EFFECTS: OnceLock<Arc<dyn tools::HostEffects>> = OnceLock::new();

/// 安装宿主的审计接收者。只在 `setup` 里调用一次。
pub fn install_audit_sink(sink: Arc<dyn server::AuditSink>) {
    let _ = AUDIT_SINK.set(sink);
}

/// 安装宿主的副作用实现。只在 `setup` 里调用一次。
pub fn install_host_effects(effects: Arc<dyn tools::HostEffects>) {
    let _ = HOST_EFFECTS.set(effects);
}

fn audit_sink() -> Arc<dyn server::AuditSink> {
    AUDIT_SINK
        .get()
        .cloned()
        .unwrap_or_else(|| Arc::new(server::NoopAudit))
}

// ---------------------------------------------------------------------------
// 写审计日志：走项目既有的 logger（`info!` -> tiez.log），并带固定前缀便于检索
// ---------------------------------------------------------------------------

/// 把一次 MCP 调用写进应用日志。
///
/// 内容刻意**不含**剪贴板正文，只含工具名、结果与参数规模：审计要回答"谁在什么
/// 时候改了什么"，而不是把用户数据再抄一份到日志里。
pub struct LoggerAudit;

impl server::AuditSink for LoggerAudit {
    fn record(&self, tool: Option<&str>, outcome: &str, detail: &str) {
        let stamp = chrono::Local::now().to_rfc3339();
        match tool {
            Some(name) => crate::info!("[MCP-AUDIT] {} tool={} outcome={} {}", stamp, name, outcome, detail),
            None => crate::info!("[MCP-AUDIT] {} tool=<protocol> outcome={} {}", stamp, outcome, detail),
        }
    }
}

// ---------------------------------------------------------------------------
// 宿主的副作用实现
// ---------------------------------------------------------------------------

/// 把 MCP 的写入动作接到 Tauri 宿主上：发事件、请求云同步、加解密入队。
pub struct TauriEffects {
    pub app: AppHandle,
}

impl tools::HostEffects for TauriEffects {
    fn emit_changed(&self) {
        use tauri::Emitter;
        let _ = self.app.emit("clipboard-changed", ());
    }

    fn request_cloud_sync(&self) {
        crate::services::cloud_sync::request_cloud_sync(self.app.clone());
    }

    fn enqueue_encryption(&self, id: i64, encrypt: bool) {
        let queue = self.app.state::<EncryptionQueueState>();
        queue
            .0
            .enqueue(tools::enqueue_from_bool(id, encrypt));
    }

    fn data_dir(&self) -> Option<std::path::PathBuf> {
        self.app
            .try_state::<AppDataDir>()
            .map(|state| state.0.lock().map(|g| g.clone()).unwrap_or_default())
            .filter(|p| !p.as_os_str().is_empty())
    }

    fn app_version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }
}

// ---------------------------------------------------------------------------
// 配置读写
// ---------------------------------------------------------------------------

fn read_bool(repo: &impl SettingsRepository, key: &str, default: bool) -> bool {
    match repo.get(key) {
        Ok(Some(v)) => v == "true",
        _ => default,
    }
}

/// 读取允许写入开关。**读不到就是 false**：默认只读是硬约束，任何异常都不能
/// 让它变成"允许写"。
pub fn allow_write(state: &DbState) -> bool {
    read_bool(&state.settings_repo, KEY_ALLOW_WRITE, false)
}

/// 读取服务开关。默认关闭。
pub fn enabled(state: &DbState) -> bool {
    read_bool(&state.settings_repo, KEY_ENABLED, false)
}

/// 读取监听端口，非法值回落到默认端口。
pub fn configured_port(state: &DbState) -> u16 {
    state
        .settings_repo
        .get(KEY_PORT)
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<u16>().ok())
        .filter(|p| *p >= 1024)
        .unwrap_or(DEFAULT_PORT)
}

/// 取访问令牌；不存在时生成一个并落库。
///
/// 生成用 `uuid::Uuid::new_v4()` 两次拼接，得到 64 个十六进制字符：这是项目里
/// 已有的随机源（`file_transfer` 也用同一个），不引入新的密码学依赖。**它不是
/// 密码学级不可预测的密钥**，因此报告里如实标注；对"防止本机其它进程误连"这个
/// 目标足够，对"防御本机上的恶意程序读取数据库"不够——后者需要独立的安全设计。
pub fn token_or_create(state: &DbState) -> String {
    if let Ok(Some(existing)) = state.settings_repo.get(KEY_TOKEN) {
        if !existing.trim().is_empty() {
            return existing;
        }
    }
    let generated = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let _ = state.settings_repo.set(KEY_TOKEN, &generated);
    generated
}

/// 当前实际监听端口（0 = 未运行）。
pub fn active_port() -> u16 {
    ACTIVE_PORT.load(Ordering::SeqCst)
}

/// 服务是否在运行。
pub fn is_running() -> bool {
    match SERVER_HANDLE.lock() {
        Ok(guard) => guard.is_some(),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// 启停
// ---------------------------------------------------------------------------

/// 启动服务。返回实际监听端口。
///
/// 与 `file_transfer::toggle_file_server` 同构：先查句柄避免重复启动，绑定端口，
/// 在 tokio 上跑 axum，最后把实际端口与启用状态写回设置表，让界面刷新后仍然一致。
pub async fn start(app: AppHandle, requested_port: Option<u16>) -> Result<u16, String> {
    if is_running() {
        return Ok(active_port());
    }

    let (token, allow_write_now, port_setting) = {
        let db_state = app.state::<DbState>();
        (
            token_or_create(&db_state),
            allow_write(&db_state),
            requested_port.unwrap_or_else(|| configured_port(&db_state)),
        )
    };

    let (listener, actual_port) = server::bind_local_listener(port_setting)
        .await
        .map_err(|e| format!("绑定本地端口失败：{}", e))?;

    let store = {
        let db_state = app.state::<DbState>();
        store::McpStore::from_state(&db_state)
    };

    let state = ServerState {
        store,
        token: AuthToken(Arc::new(token)),
        settings: Arc::new(RuntimeSettings::new(allow_write_now)),
        audit: audit_sink(),
        effects: HOST_EFFECTS
            .get()
            .cloned()
            .unwrap_or_else(|| Arc::new(tools::NoopEffects::default())),
    };

    // 与 `file_transfer::toggle_file_server` 用同一个 tokio 运行时形态：
    // `tokio::spawn` 返回 `tokio::task::JoinHandle`，可以 `abort()` 掉。
    let handle = tokio::spawn(async move {
        server::serve(listener, state).await;
        ACTIVE_PORT.store(0, Ordering::SeqCst);
    });

    {
        let mut guard = SERVER_HANDLE.lock().map_err(|e| e.to_string())?;
        *guard = Some(handle);
    }
    ACTIVE_PORT.store(actual_port, Ordering::SeqCst);

    // 端口可能因占用而后移，回写真实值，避免界面显示的与实际不一致。
    {
        let db_state = app.state::<DbState>();
        let _ = db_state.settings_repo.set(KEY_PORT, &actual_port.to_string());
        let _ = db_state.settings_repo.set(KEY_ENABLED, "true");
    }

    crate::info!(
        ">>> [MCP] 服务已启动：http://127.0.0.1:{}/mcp（写操作 {}）",
        actual_port,
        if allow_write_now { "已允许" } else { "未允许（只读）" }
    );
    Ok(actual_port)
}

/// 停止服务。未运行时是幂等的空操作。
pub fn stop(app: &AppHandle) -> Result<(), String> {
    let handle = {
        let mut guard = SERVER_HANDLE.lock().map_err(|e| e.to_string())?;
        guard.take()
    };
    if let Some(handle) = handle {
        handle.abort();
        ACTIVE_PORT.store(0, Ordering::SeqCst);
        let db_state = app.state::<DbState>();
        let _ = db_state.settings_repo.set(KEY_ENABLED, "false");
        crate::info!(">>> [MCP] 服务已停止");
    }
    Ok(())
}

/// 应用启动时按配置自动拉起服务（默认关闭）。
pub fn autostart_if_configured(app: &AppHandle) {
    let (enabled_now, autostart, port) = {
        let db_state = app.state::<DbState>();
        (
            enabled(&db_state),
            read_bool(&db_state.settings_repo, KEY_AUTOSTART, false),
            configured_port(&db_state),
        )
    };
    if !enabled_now || !autostart {
        return;
    }
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = start(handle, Some(port)).await {
            crate::error!(">>> [MCP] 自动启动失败：{}", e);
        }
    });
}

// ---------------------------------------------------------------------------
// Tauri 命令：供设置界面使用
// ---------------------------------------------------------------------------

/// 服务状态快照，供设置界面渲染。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpStatus {
    pub running: bool,
    pub port: u16,
    pub enabled: bool,
    pub allow_write: bool,
    pub autostart: bool,
    pub token: String,
    pub endpoint: String,
}

#[tauri::command]
pub fn get_mcp_status(app: AppHandle) -> McpStatus {
    let db_state = app.state::<DbState>();
    let running = is_running();
    let port = if running { active_port() } else { 0 };
    McpStatus {
        running,
        port,
        enabled: enabled(&db_state),
        allow_write: allow_write(&db_state),
        autostart: read_bool(&db_state.settings_repo, KEY_AUTOSTART, false),
        token: token_or_create(&db_state),
        endpoint: if running {
            format!("http://127.0.0.1:{}/mcp", port)
        } else {
            String::new()
        },
    }
}

/// 起停服务。
#[tauri::command]
pub async fn set_mcp_server_enabled(app: AppHandle, enabled: bool) -> Result<u16, String> {
    if enabled {
        start(app, None).await
    } else {
        stop(&app)?;
        Ok(0)
    }
}

/// 切换写权限。只影响后续请求，不重启服务。
///
/// 运行时开关与数据库同时更新：前者让已连上的客户端立刻受限，后者保证重启后
/// 状态不丢。
#[tauri::command]
pub fn set_mcp_allow_write(app: AppHandle, allow: bool) -> Result<(), String> {
    let db_state = app.state::<DbState>();
    db_state
        .settings_repo
        .set(KEY_ALLOW_WRITE, if allow { "true" } else { "false" })
        .map_err(|e| e.to_string())?;
    crate::info!(
        "[MCP-AUDIT] {} 写权限被设置为 {}",
        chrono::Local::now().to_rfc3339(),
        allow
    );
    Ok(())
}

/// 设置端口。运行中则重启服务以生效。
#[tauri::command]
pub async fn set_mcp_port(app: AppHandle, port: u16) -> Result<u16, String> {
    if port < 1024 {
        return Err("端口需在 1024-65535 之间".to_string());
    }
    {
        let db_state = app.state::<DbState>();
        db_state
            .settings_repo
            .set(KEY_PORT, &port.to_string())
            .map_err(|e| e.to_string())?;
    }
    if is_running() {
        stop(&app)?;
        return start(app, Some(port)).await;
    }
    Ok(port)
}

/// 设置"随应用自动启动"。
#[tauri::command]
pub fn set_mcp_autostart(app: AppHandle, autostart: bool) -> Result<(), String> {
    let db_state = app.state::<DbState>();
    db_state
        .settings_repo
        .set(KEY_AUTOSTART, if autostart { "true" } else { "false" })
        .map_err(|e| e.to_string())
}

/// 重新生成访问令牌。旧令牌立即失效。
#[tauri::command]
pub fn regenerate_mcp_token(app: AppHandle) -> Result<String, String> {
    let db_state = app.state::<DbState>();
    let generated = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    db_state
        .settings_repo
        .set(KEY_TOKEN, &generated)
        .map_err(|e| e.to_string())?;
    // 令牌在服务启动时被放进 `AuthToken`，要生效必须重启。
    if is_running() {
        stop(&app)?;
        let handle = app.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = start(handle, None).await {
                crate::error!(">>> [MCP] 令牌更新后重启失败：{}", e);
            }
        });
    }
    Ok(generated)
}

/// 供 `setup` 一次性接线：安装审计与副作用实现。
pub fn install_host(app: &AppHandle) {
    install_audit_sink(Arc::new(LoggerAudit));
    install_host_effects(Arc::new(TauriEffects { app: app.clone() }));
}

/// 确保 `DbState` 里存在令牌（首次读取即生成），返回它。
///
/// 拆出来是为了让设置界面第一次打开就能看到令牌，而不是等到启用服务之后。
pub fn ensure_token(state: &DbState) -> String {
    token_or_create(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_store_via_module_path_works() {
        // 说明模块对外导出可用（`store` 对 `server`/`tools` 是共享依赖）。
        let store = store::McpStore::in_memory();
        assert_eq!(store.count().unwrap(), 0);
    }

    #[test]
    fn tool_catalog_is_exposed_through_the_module() {
        assert!(tools::find("list_entries").is_some());
        assert!(tools::find("nope").is_none());
        assert!(tools::catalog().len() >= 15);
    }
}
