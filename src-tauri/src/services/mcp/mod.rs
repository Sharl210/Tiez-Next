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
#[cfg(test)]
mod tools_extra;

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};
use tokio::task::JoinHandle;

use crate::app_state::{AppDataDir, EncryptionQueueState, PasteQueue, SessionHistory};
use crate::database::DbState;
use crate::infrastructure::repository::settings_repo::SettingsRepository;

pub use server::{listen_ip, AuthToken, RuntimeSettings, ServerState};
pub use store::{
    DEFAULT_ALLOW_LAN, DEFAULT_ALLOW_WRITE, DEFAULT_AUTOSTART, DEFAULT_ENABLED, DEFAULT_PORT,
    DEFAULT_REQUIRE_TOKEN, KEY_ALLOW_LAN, KEY_ALLOW_WRITE, KEY_AUTOSTART, KEY_ENABLED, KEY_PORT,
    KEY_REQUIRE_TOKEN, KEY_TOKEN,
};

/// 当前运行的 server 任务句柄。
static SERVER_HANDLE: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

/// 正在运行的服务共享状态（`ServerState` 只含 `Arc`，克隆是廉价的）。
///
/// 保留它是为了让"改设置"能**立刻**作用到已连上的客户端：写权限与令牌校验都是
/// 运行时原子开关，改设置时同步刷新这里即可，不必重启服务。停机时清空。
static ACTIVE_STATE: Mutex<Option<ServerState>> = Mutex::new(None);

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

    fn emit_tag_colors_updated(&self) {
        use tauri::Emitter;
        // 与 `TagManager.tsx` 用同一个事件名。界面自己改色时也发它，两条路径在这里
        // 汇合，`useTagColors.ts` 不必区分颜色是谁改的。
        let _ = self.app.emit("tag-colors-updated", ());
    }

    /// 云同步是否已配置。
    ///
    /// 判据与 `cloud_sync.rs` 的启动条件一致：本地开关 `cloud_sync_enabled` 为真，
    /// 或走"服务器下发配置"那条路（`cloud_sync_server_url` 非空）。两者都空时
    /// `request_cloud_sync` 是空操作，因此这里如实返回 `false`——让"AI 改了标签但
    /// 同步没配"这件事在工具返回值里就能区分出来。
    fn cloud_sync_enabled(&self) -> bool {
        let Some(db) = self.app.try_state::<DbState>() else {
            return false;
        };
        let get = |k: &str| {
            db.settings_repo
                .get(k)
                .ok()
                .flatten()
                .unwrap_or_default()
        };
        get("cloud_sync_enabled") == "true" || !get("cloud_sync_server_url").trim().is_empty()
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

    /// 读内存里的会话态条目。
    ///
    /// **这就是 P0 缺陷的修复点**：`SessionHistory` 是界面与 AI 之间那条唯一的
    /// 数据缝。界面命令一直在读它（`history_cmd.rs` 的列表与搜索都做合并），
    /// MCP 侧此前 `grep -c SessionHistory src/services/mcp/` 是 0——于是当
    /// `app.persistent` 为默认的 `'false'` 时，用户屏幕上的每一条都只存在于这块
    /// 内存里，AI 一条也看不到。
    ///
    /// `try_state` 而不是 `state`：这个实现也会在不装配 `SessionHistory` 的
    /// 场景（测试、早期启动阶段）被调用，缺状态应当是"没有会话态"而不是 panic。
    fn session_snapshot(&self) -> Option<Vec<crate::domain::models::ClipboardEntry>> {
        self.app.try_state::<SessionHistory>().map(|state| {
            state
                .0
                .lock()
                .map(|guard| guard.iter().cloned().collect())
                .unwrap_or_default()
        })
    }

    /// 就地改写内存里的会话态条目（删除、置顶、落库后换 id、标签重命名）。
    ///
    /// 与界面命令同源：`history_cmd::delete_clipboard_entry` 用 `retain` 剔除，
    /// `clipboard_cmd::toggle_clipboard_pin` 就地把负 id 改成库里的正 id，
    /// `rename_tag_globally`/`delete_tag_from_all` 遍历改写 `item.tags`。
    /// 少这一步，AI 的操作只落在库里，用户屏幕上那条会话态条目纹丝不动——
    /// 表现成"AI 说删了，界面上还在，一刷新又冒出来"。
    fn session_apply(
        &self,
        f: &(dyn Fn(&mut Vec<crate::domain::models::ClipboardEntry>) -> usize + Send + Sync),
    ) -> Option<usize> {
        let state = self.app.try_state::<SessionHistory>()?;
        let mut guard = state.0.lock().ok()?;
        let mut snapshot: Vec<crate::domain::models::ClipboardEntry> =
            guard.iter().cloned().collect();
        let hits = f(&mut snapshot);
        // 只有真的改动了才回写，避免无谓地重建整个双端队列。
        // 回写与读取都在这把锁内完成，因此不会把并发期间新加入的条目挤掉。
        if hits > 0 {
            guard.clear();
            guard.extend(snapshot);
        }
        Some(hits)
    }

    fn clipboard_write(&self, req: tools::ClipboardWrite) -> Result<Value, String> {
        let pasted = req.paste;
        if pasted {
            host_paste_entry(self, &req)?;
        } else {
            host_copy_to_clipboard(self, &req)?;
        }
        Ok(json!({
            "ok": true,
            "id": req.id,
            "contentType": req.content_type,
            "pasted": pasted,
            "deleteAfterUse": req.delete_after_use && pasted,
        }))
    }

    fn list_legacy_data_dirs(&self) -> Result<Value, String> {
        self.legacy_dirs_value()
    }

    fn migrate_from_data_dir(&self, path: &str) -> Result<Value, String> {
        self.migrate_from_dir_value(path)
    }

    fn paste_queue(&self) -> Result<Value, String> {
        self.paste_queue_value()
    }

    fn set_paste_queue(&self, item_ids: &[i64]) -> Result<Value, String> {
        self.set_paste_queue_value(item_ids)
    }

    fn save_emoji_favorite(&self, source_path: &str, data_url: &str) -> Result<Value, String> {
        self.save_emoji_value(source_path, data_url)
    }

    fn remove_emoji_favorite(&self, path: &str) -> Result<Value, String> {
        self.remove_emoji_value(path)
    }

    fn cloud_sync_status(&self) -> Result<Value, String> {
        self.cloud_status_value()
    }

    fn mqtt_status(&self) -> Result<Value, String> {
        self.mqtt_status_value()
    }
}

/// `HostEffects` 的 Tauri 实现里那些"需要宿主"的方法。
///
/// 拆成独立函数而不是直接堆进 `impl` 块：那里一行行 `self.app.state::<...>()` 很容易
/// 掩盖"这一步到底做了什么"，而这些函数恰恰是安全攸关的（迁移、删除文件）。
impl TauriEffects {
    /// 宿主侧可用的数据目录（读不到就报错，绝不拿空路径去写文件）。
    fn require_data_dir(&self) -> Result<std::path::PathBuf, String> {
        tools::HostEffects::data_dir(self)
            .ok_or_else(|| "当前无法确定数据目录，该操作不可用".to_string())
    }

    fn legacy_dirs_value(&self) -> Result<Value, String> {
        let current = self.require_data_dir()?;
        // 原生应用数据目录（Tauri 由 identifier 推导）也要扫：用户若改过数据目录或用
        // 便携版，`%APPDATA%\com.tieznext` 里那份历史数据不在 current 的同级。
        let native = self.app.path().app_data_dir().ok();
        let extra: Vec<std::path::PathBuf> = native.into_iter().collect();
        let dirs = crate::migration_identifier::list_legacy_dirs(&current, &extra);
        let items: Vec<Value> = dirs
            .iter()
            .map(|i| {
                json!({
                    "path": i.path.to_string_lossy(),
                    "identifier": i.identifier,
                    "origin": i.origin.code(),
                    "bytes": i.bytes,
                    "files": i.files,
                    "hasDatabase": i.has_database,
                    "canDelete": i.can_delete,
                })
            })
            .collect();
        Ok(json!({
            "currentDataDir": current.to_string_lossy(),
            "count": items.len(),
            "dirs": items,
        }))
    }

    fn migrate_from_dir_value(&self, path: &str) -> Result<Value, String> {
        let current = self.require_data_dir()?;
        let trimmed = path.trim();
        if trimmed.is_empty() {
            return Err("未指定源数据目录".to_string());
        }
        let source = std::path::PathBuf::from(trimmed);

        // 只读预检：先把源会被迁移的文件逐个读一遍，作为"源可读"的证据。
        // 预检失败**不阻断**迁移（后续真实复制会给出更准确的错误），只记录。
        if let Err(e) = crate::migration_identifier::check_source_is_readable(&source) {
            // 仓库里只有 `info!` / `error!` 两个日志宏，没有 `warn!`；预检失败
            // 属于"要留痕但不阻断"的情况，用 `error!` 会把一次正常的可读性探测
            // 渲染成故障，因此这里按 `info!` 记录（细节完整，级别不高）。
            crate::info!(
                "[MCP] 迁移前只读预检未通过（未做任何写入）：源={:?} 原因={}",
                source,
                e
            );
        }

        // 安全闸：目标库必须是"从未被用过"的，否则拒绝接管。
        let takeover = target_db_is_pristine(&current);
        if !takeover {
            return Err(format!(
                "拒绝迁移：当前数据目录 {} 里已经有你自己的数据，接管会覆盖它。\
                 请先导出备份（export_backup）或改用另一个数据目录。",
                current.display()
            ));
        }
        // —— 两阶段迁移第一步：运行期只把源复制到暂存，**目标一个字节都不动** ——
        //
        // 【为什么 MCP 入口也必须走两阶段】迁移的动作里包含"给目标里那个
        // `clipboard.db` 改名让位"。MCP 服务跑在应用进程内，而应用启动时
        // `init_db` 已经打开过那个库、连接常驻 `DbState`——Windows **不允许**给已
        // 打开的文件改名（`os error 32`）。这与"谁触发的迁移"无关，因此两个入口
        // 面对的是同一个平台限制，必须共用同一条实现，否则行为会漂移。
        //
        // `true` = 允许接管：上面的安全闸已经确认过目标库从未被使用（读 SQLite 的
        // 判定只能在命令层做，本模块只依赖 `std`）。
        //
        // 不传进度回调：MCP 没有事件通道，调用方拿到的是**最终报告**而不是过程。
        let outcome = crate::migration_identifier::stage_takeover_with_progress(
            &source,
            &current,
            takeover,
            &mut |_, _, _, _| {},
        );
        let mut report = crate::app::apply_identifier_migration(&source, &current, outcome);
        finalize_mcp_deferred(&mut report, &self.app, &source, &current);
        Ok(serde_json::to_value(&report).unwrap_or_else(|_| json!({ "status": "unknown" })))
    }

    fn paste_queue_value(&self) -> Result<Value, String> {
        let state = self
            .app
            .try_state::<PasteQueue>()
            .ok_or_else(|| "粘贴队列未启用".to_string())?;
        let items: Vec<i64> = state
            .0
            .lock()
            .map(|q| q.items.iter().copied().collect())
            .map_err(|_| "粘贴队列状态不可读".to_string())?;
        Ok(json!({ "count": items.len(), "itemIds": items }))
    }

    fn set_paste_queue_value(&self, item_ids: &[i64]) -> Result<Value, String> {
        // 复用界面的同名命令，避免把"清空时还要重置最近粘贴指纹"这类细节抄漏。
        // 注意 `State` 的生命周期：它在**同一个作用域内**从 `app` 借出来并立刻用掉，
        // 因此不需要（也不应该）把生命周期"拉长"——那会引入不必要的 unsafe。
        let app = self.app.clone();
        let ids = item_ids.to_vec();
        crate::services::paste_queue::set_paste_queue(
            app.clone(),
            app.state::<PasteQueue>(),
            ids,
        )
        .map_err(|e| format!("设置粘贴队列失败：{}", e))?;
        self.paste_queue_value()
    }

    /// 把一张图片写入表情收藏目录。
    ///
    /// 【为什么清单同步由 `tools.rs` 负责而不是这里】界面（`EmojiPanel.tsx`）渲染的是
    /// **设置项 `app.emoji_favorites` 里的那个 JSON 路径数组**，`list_emoji_favorites`
    /// 只在切到收藏页时用它做一次合并。只把文件写进 `emoji_favorites/` 而不更新设置项，
    /// 用户切回界面**看不到**刚加的表情；只更新设置项而不写文件，用户看到的是一个打不开
    /// 的路径。两者必须一起做，而设置项这一半是纯数据操作，放在可单测的 `tools.rs` 里
    /// 比藏在这个需要 `AppHandle` 的文件里更安全。
    fn save_emoji_value(&self, source_path: &str, data_url: &str) -> Result<Value, String> {
        let data_dir = self.require_data_dir()?;
        let saved: String = if !source_path.trim().is_empty() {
            let src = source_path.trim();
            let ext = crate::app::commands::file_cmd::image_ext_from_filename(src).ok_or_else(
                || "不支持的图片格式（支持 png/jpg/jpeg/gif/webp/bmp/ico/svg）".to_string(),
            )?;
            let bytes = std::fs::read(src).map_err(|e| format!("读取源图片失败：{}", e))?;
            crate::app::commands::file_cmd::save_emoji_favorite_bytes_to_dir(
                &data_dir, &bytes, ext,
            )
            .map_err(|e| e.to_string())?
        } else if !data_url.trim().is_empty() {
            save_emoji_data_url(&data_dir, data_url.trim())?
        } else {
            return Err("必须提供 sourcePath 或 dataUrl".to_string());
        };
        Ok(json!({ "path": saved }))
    }

    fn remove_emoji_value(&self, path: &str) -> Result<Value, String> {
        let data_dir = self.require_data_dir()?;
        let favorites_dir = data_dir.join("emoji_favorites");
        let favorites_dir = favorites_dir.canonicalize().unwrap_or(favorites_dir);
        let target = std::path::PathBuf::from(path.trim());
        // 只允许删收藏目录里的文件：与界面命令一致，路径越界一律不动。
        let canonical = target.canonicalize().unwrap_or_else(|_| target.clone());
        let removed = if canonical.starts_with(&favorites_dir) && canonical.is_file() {
            std::fs::remove_file(&canonical).is_ok()
        } else {
            false
        };
        Ok(json!({ "path": path, "fileRemoved": removed }))
    }

    fn cloud_status_value(&self) -> Result<Value, String> {
        let status = crate::services::cloud_sync::get_cloud_sync_status();
        Ok(serde_json::to_value(&status).unwrap_or_else(|_| json!({})))
    }

    /// MQTT 状态。**刻意不返回密码**：这是一个一读就把值带出进程的接口，
    /// 而密码没有"MCP 需要知道"的场景。
    fn mqtt_status_value(&self) -> Result<Value, String> {
        let db = self.app.state::<DbState>();
        let get = |k: &str| db.settings_repo.get(k).ok().flatten();
        let get_or = |k: &str, d: &str| get(k).unwrap_or_else(|| d.to_string());
        Ok(json!({
            "connected": crate::services::mqtt_sub::get_mqtt_status(),
            "running": crate::services::mqtt_sub::get_mqtt_running(),
            "enabled": get_or("mqtt_enabled", "false") == "true",
            "protocol": get_or("mqtt_protocol", "mqtt://"),
            "server": get("mqtt_server"),
            "port": get("mqtt_port"),
            "topic": get_or("mqtt_topic", "tiez/next"),
            "clientId": get("mqtt_client_id"),
            "username": get("mqtt_username"),
            "ssl": get_or("mqtt_ssl", "false") == "true",
            "tlsInsecure": get_or("mqtt_tls_insecure", "false") == "true",
        }))
    }
}

/// `copy_to_clipboard` 的宿主实现（只写剪贴板，可选随后粘贴）。
///
/// 直接复用界面同名命令，而不是在这里重写"写系统剪贴板 + 粘贴"：界面命令已经处理了
/// 会话态取正文、富文本归一化、焦点切换、粘贴后置顶/删除、音效等一串细节，任何一份
/// 复制都会立刻与界面分叉。
///
/// # 为什么 `State` 是在 async 块**内部**取的
///
/// `AppHandle::state::<T>()` 返回的 `State<'_, T>` 借的是 `&AppHandle`。若在块外取、
/// 再搬进 `async move`，编译器会要求把那笔借用延长到 `'static`（那正是需要 unsafe
/// 的地方）。把取值放进块内、并让 `app_handle` 以**克隆**的形式传给被调函数，借用就
/// 自然活在块内——`clipboard_ops::paste_history_item_by_index` 用的也是这个写法。
fn host_copy_to_clipboard(effects: &TauriEffects, req: &tools::ClipboardWrite) -> Result<(), String> {
    let app_handle = effects.app.clone();
    let (content, content_type, id) = (req.content.clone(), req.content_type.clone(), req.id);
    let (paste, delete_after_use) = (req.paste, req.delete_after_use);
    let (paste_with_format, move_to_top) = (req.paste_with_format, req.move_to_top);
    run_blocking("复制到剪贴板", async move {
        let db_state = app_handle.state::<DbState>();
        let session_state = app_handle.state::<SessionHistory>();
        crate::services::clipboard_ops::copy_to_clipboard(
            app_handle.clone(),
            db_state,
            session_state,
            content,
            content_type,
            paste,
            id,
            delete_after_use,
            paste_with_format,
            move_to_top,
        )
        .await
    })
}

/// `paste_entry` 的宿主实现（临时粘贴：先存原剪贴板，粘贴后还原）。
fn host_paste_entry(effects: &TauriEffects, req: &tools::ClipboardWrite) -> Result<(), String> {
    if req.delete_after_use {
        // 界面没有"临时粘贴 + 粘贴后删除"这个组合（`paste_content_transiently` 恒不删），
        // 与其在这里悄悄忽略，不如明确拒绝：静默少做一件用户明确要求的事，比报错更难排查。
        return Err(
            "临时粘贴不支持 deleteAfterUse；要粘贴后删除请用 copy_to_clipboard 并设 paste=true"
                .to_string(),
        );
    }
    let app_handle = effects.app.clone();
    let (content, content_type, id) = (req.content.clone(), req.content_type.clone(), req.id);
    let paste_with_format = req.paste_with_format;
    run_blocking("粘贴条目", async move {
        let db_state = app_handle.state::<DbState>();
        let session_state = app_handle.state::<SessionHistory>();
        crate::services::clipboard_ops::paste_content_transiently(
            app_handle.clone(),
            db_state,
            session_state,
            content,
            content_type,
            id,
            paste_with_format,
        )
        .await
    })
}

/// 在一个独立线程上等待异步宿主操作完成，并把错误翻成人话。
///
/// # 为什么不是 `tauri::async_runtime::block_on`
///
/// MCP 的调用链是
///   axum（跑在 `tauri::async_runtime` 上）→ `handle_post`(async) → `dispatch`(同步) → `invoke`(同步)
/// 也就是说这些函数**是在 tokio 工作线程里执行的**。在 async 上下文里调
/// `Runtime::block_on` 会直接 panic（"Cannot start a runtime from within a runtime"），
/// `JoinHandle::blocking_recv` 同样会 panic（"Cannot block the current thread from
/// within a runtime"）。
///
/// 所以这里把未来交给 `spawn_blocking`——它跑在**专用阻塞线程池**上，那里没有 async
/// 上下文，`block_on` 是合法的——再用一条标准库通道把结果传回来。
///
/// # 代价与取舍
///
/// 调用方所在的工作线程会在此处阻塞到操作完成（几十到几百毫秒；粘贴路径含焦点切换，
/// 约 300ms）。这是刻意的：MCP 工具调用本来就是同步请求-响应语义，"先返回成功、稍后
/// 再说"会让 AI 收到一条它无法验证的成功。代价是多占一个 tokio 工作线程，对一个本机
/// 桌面服务可以接受。
fn run_blocking<F>(what: &str, fut: F) -> Result<(), String>
where
    F: std::future::Future<Output = crate::error::AppResult<()>> + Send + 'static,
{
    let label = what.to_string();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    tauri::async_runtime::spawn_blocking(move || {
        let outcome = tauri::async_runtime::block_on(fut);
        let _ = tx.send(outcome);
    });
    match rx.recv() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!("{}失败：{}", label, e)),
        Err(_) => Err(format!("{}未完成：宿主任务未回传结果", label)),
    }
}

/// 迁移前的"目标库是否从未被用过"判定。
///
/// 与界面 `system_cmd::target_db_is_pristine` **同一条判据**（剪贴板 0 条 且 无用户
/// 自建标签）。那条函数是私有的，而 `system_cmd.rs` 不在本代理的可改范围里，所以这里
/// 照抄判据并保留同样的保守性：**读不到就按"在用"处理**。
///
/// 【为什么要判而不是直接迁】不判这个，AI 就能在用户已有一库数据的情况下发起迁移，
/// 由 `migrate_from_source_dir` 的 `allow_takeover=true` 接管目标库——那会替换掉用户
/// 自己的数据。宁可少迁，不可覆盖。
fn target_db_is_pristine(target: &std::path::Path) -> bool {
    let db = target.join("clipboard.db");
    if !db.is_file() {
        return true; // 连库都没有 = 完全没用过
    }
    // 在副本上判定：绝不因为"看一眼"而去动目标库（含 WAL 侧车）。
    let stamp = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let probe_dir = std::env::temp_dir().join(format!("tiez-mcp-db-probe-{}", stamp));
    if std::fs::create_dir_all(&probe_dir).is_err() {
        return false;
    }
    for suffix in ["", "-wal", "-shm"] {
        let from = target.join(format!("clipboard.db{}", suffix));
        if from.is_file()
            && std::fs::copy(&from, probe_dir.join(format!("clipboard.db{}", suffix))).is_err()
        {
            let _ = std::fs::remove_dir_all(&probe_dir);
            return false;
        }
    }
    let probe_db = probe_dir.join("clipboard.db");
    let conn = match rusqlite::Connection::open(&probe_db) {
        Ok(c) => c,
        Err(_) => {
            let _ = std::fs::remove_dir_all(&probe_dir);
            return false;
        }
    };
    let clips: Option<i64> = conn
        .query_row("SELECT COUNT(*) FROM clipboard_history", [], |r| r.get(0))
        .ok();
    let tags: Option<i64> = conn
        .query_row(
            "SELECT COUNT(*) FROM saved_tags WHERE name NOT IN ('sensitive', ?1)",
            rusqlite::params!["密码"],
            |r| r.get(0),
        )
        .ok();
    drop(conn);
    let _ = std::fs::remove_dir_all(&probe_dir);
    clips == Some(0) && tags == Some(0)
}

/// 供 MCP 入口使用的"待接管标记"落盘。
///
/// 【为什么 MCP 与界面各有一份】两者的宿主耦合点不同：界面命令拿得到 `AppHandle`
/// 与 `State<AppDataDir>`，MCP 入口只有一个 `AppHandle`。但**契约必须一致**——
/// 两边都要在拿到 `Deferred` 时写同一个标记、返回同一个 `status = "deferred"`。
/// 标记文件的路径与格式由 `migration_pending` 单点持有，这里只负责"取原生目录 + 写"。
///
/// 写失败时**如实改为失败**：标记是下次启动唯一能知道"有活要干"的凭据，若对调用方
/// 说"重启后自动完成"而标记没写成功，重启后什么都不会发生。
fn finalize_mcp_deferred(
    report: &mut crate::app::IdentifierMigrationReport,
    app: &AppHandle,
    source: &std::path::Path,
    target: &std::path::Path,
) {
    if report.status != "deferred" {
        return;
    }
    let Some(native) = app.path().app_data_dir().ok() else {
        report.status = "failed".to_string();
        report.pending_until_restart = false;
        report.error = Some(
            "取不到应用的原生数据目录，无法记录「待接管」状态；已复制到暂存的数据仍保留，请重试本次迁移。"
                .to_string(),
        );
        return;
    };

    // 必须记归一化之后的源目录（理由见界面入口同处的说明：便携版用户常停在外层，
    // 库里记录的却是内层 `data/` 的绝对路径）。
    let pending = crate::migration_pending::PendingMigration::new(
        crate::migration_identifier::resolve_source_dir(source),
        crate::migration_identifier::takeover_staging_dir(target),
        target.to_path_buf(),
        env!("CARGO_PKG_VERSION"),
    );
    match crate::migration_pending::write(&native, &pending) {
        Ok(path) => {
            crate::info!(">>> [MCP] 已写入待接管标记：{:?}", path);
            report.pending_until_restart = true;
        }
        Err(e) => {
            crate::error!("[MCP] 待接管标记写入失败：{}", e);
            report.status = "failed".to_string();
            report.pending_until_restart = false;
            report.error = Some(format!(
                "数据已复制到暂存目录，但「待接管」状态未能记下（{}）；请重试本次迁移。源目录未被改动。",
                e
            ));
        }
    }
}

/// 表情 dataUrl 落盘：**与界面命令 `save_emoji_favorite_data_url` 同一套判定顺序**
/// （MIME → 魔数 → png 兜底）。
///
/// 那段逻辑在界面命令里是内联的、没有可复用的函数；这里重写时必须把顺序与兜底逐条
/// 对齐，绝不写得更宽松——否则同一个 dataUrl 经界面加能进、经 AI 加报错。
fn save_emoji_data_url(data_dir: &std::path::Path, data_url: &str) -> Result<String, String> {
    use base64::Engine as _;
    let (mime, payload) = if data_url.starts_with("data:") {
        let mut parts = data_url.splitn(2, ',');
        let header = parts.next().unwrap_or("");
        let body = parts.next().unwrap_or("");
        let mime = header
            .trim_start_matches("data:")
            .split(';')
            .next()
            .unwrap_or("")
            .to_string();
        (mime, body.to_string())
    } else {
        (String::new(), data_url.to_string())
    };

    if payload.is_empty() {
        return Err("dataUrl 为空".to_string());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|e| format!("Base64 解码失败：{}", e))?;

    let ext = crate::app::commands::file_cmd::image_ext_from_mime(&mime)
        .or_else(|| crate::app::commands::file_cmd::image_ext_from_bytes(&bytes))
        .unwrap_or("png");

    crate::app::commands::file_cmd::save_emoji_favorite_bytes_to_dir(data_dir, &bytes, ext)
        .map_err(|e| e.to_string())
}

fn read_bool(repo: &impl SettingsRepository, key: &str, default: bool) -> bool {
    match repo.get(key) {
        Ok(Some(v)) => v == "true",
        _ => default,
    }
}

/// 读取允许写入开关。默认 [`store::DEFAULT_ALLOW_WRITE`]（允许写）。
///
/// 默认值来自 `store.rs` 的常量而不是写死在这里：出厂姿态只应有一处定义，测试断言
/// 的也是那一处。
pub fn allow_write(state: &DbState) -> bool {
    read_bool(
        &state.settings_repo,
        KEY_ALLOW_WRITE,
        store::DEFAULT_ALLOW_WRITE,
    )
}

/// 读取服务开关。默认 [`store::DEFAULT_ENABLED`]（开启）。
pub fn enabled(state: &DbState) -> bool {
    read_bool(&state.settings_repo, KEY_ENABLED, store::DEFAULT_ENABLED)
}

/// 读取"是否强制校验令牌"。默认 [`store::DEFAULT_REQUIRE_TOKEN`]（不校验）。
pub fn require_token(state: &DbState) -> bool {
    read_bool(
        &state.settings_repo,
        KEY_REQUIRE_TOKEN,
        store::DEFAULT_REQUIRE_TOKEN,
    )
}

/// 读取"是否允许局域网访问"。默认 [`store::DEFAULT_ALLOW_LAN`]（仅本机）。
pub fn allow_lan(state: &DbState) -> bool {
    read_bool(&state.settings_repo, KEY_ALLOW_LAN, store::DEFAULT_ALLOW_LAN)
}

/// 读取"是否随应用自动启动"。默认 [`store::DEFAULT_AUTOSTART`]。
pub fn autostart(state: &DbState) -> bool {
    read_bool(&state.settings_repo, KEY_AUTOSTART, store::DEFAULT_AUTOSTART)
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

    let (token, allow_write_now, require_token_now, allow_lan_now, port_setting) = {
        let db_state = app.state::<DbState>();
        (
            token_or_create(&db_state),
            allow_write(&db_state),
            require_token(&db_state),
            allow_lan(&db_state),
            requested_port.unwrap_or_else(|| configured_port(&db_state)),
        )
    };

    let (listener, actual_port) = server::bind_listener(port_setting, allow_lan_now)
        .await
        .map_err(|e| format!("绑定监听端口失败：{}", e))?;

    let store = {
        let db_state = app.state::<DbState>();
        store::McpStore::from_state(&db_state)
    };

    let state = ServerState {
        store,
        token: AuthToken::new(Arc::new(token), require_token_now),
        settings: Arc::new(RuntimeSettings::new(allow_write_now)),
        audit: audit_sink(),
        effects: HOST_EFFECTS
            .get()
            .cloned()
            .unwrap_or_else(|| Arc::new(tools::NoopEffects::default())),
    };

    // 与 `file_transfer::toggle_file_server` 用同一个 tokio 运行时形态：
    // `tokio::spawn` 返回 `tokio::task::JoinHandle`，可以 `abort()` 掉。
    let shared = state.clone();
    let handle = tokio::spawn(async move {
        server::serve(listener, state).await;
        ACTIVE_PORT.store(0, Ordering::SeqCst);
    });

    {
        let mut guard = SERVER_HANDLE.lock().map_err(|e| e.to_string())?;
        *guard = Some(handle);
    }
    {
        let mut guard = ACTIVE_STATE.lock().map_err(|e| e.to_string())?;
        *guard = Some(shared);
    }
    ACTIVE_PORT.store(actual_port, Ordering::SeqCst);

    // 端口可能因占用而后移，回写真实值，避免界面显示的与实际不一致。
    {
        let db_state = app.state::<DbState>();
        let _ = db_state.settings_repo.set(KEY_PORT, &actual_port.to_string());
        let _ = db_state.settings_repo.set(KEY_ENABLED, "true");
    }

    // 监听地址与鉴权状态都写进日志：用户排查"为什么局域网连不上"时，日志要能
    // 直接回答"当时到底绑在哪个地址、有没有要令牌"。
    let host = if allow_lan_now {
        format!("0.0.0.0（局域网可访问，本机入口 http://127.0.0.1:{}/mcp）", actual_port)
    } else {
        format!("127.0.0.1（仅本机，http://127.0.0.1:{}/mcp）", actual_port)
    };
    crate::info!(
        ">>> [MCP] 服务已启动：绑定 {}（写操作 {}，令牌校验 {}）",
        host,
        if allow_write_now { "已允许" } else { "未允许（只读）" },
        if require_token_now { "已开启" } else { "未开启（免鉴权）" }
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
        if let Ok(mut guard) = ACTIVE_STATE.lock() {
            *guard = None;
        }
        let db_state = app.state::<DbState>();
        let _ = db_state.settings_repo.set(KEY_ENABLED, "false");
        crate::info!(">>> [MCP] 服务已停止");
    }
    Ok(())
}

/// 应用启动时按配置自动拉起服务。
///
/// 出厂默认是"开 + 自动启动"，因此这一支在默认姿态下会真的把服务跑起来；用户
/// 关掉任何一个开关都会在这里被挡住。
pub fn autostart_if_configured(app: &AppHandle) {
    let (enabled_now, autostart_now, port) = {
        let db_state = app.state::<DbState>();
        (enabled(&db_state), autostart(&db_state), configured_port(&db_state))
    };
    if !enabled_now || !autostart_now {
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
    /// 是否强制校验令牌。`false` = 免鉴权（出厂默认）。
    pub require_token: bool,
    /// 是否允许局域网访问。`false` = 仅本机（出厂默认）。
    pub allow_lan: bool,
    pub token: String,
    /// 本机入口。无论是否开放局域网，这个地址始终可用。
    pub endpoint: String,
    /// 局域网入口；只在"运行中 + 已开放局域网"时有值，否则为空串。
    pub lan_endpoint: String,
    /// 当前默认值，供界面在"用户从未改过"时显示"默认"标记。
    pub default_port: u16,
    pub default_enabled: bool,
    pub default_allow_write: bool,
    pub default_require_token: bool,
    pub default_allow_lan: bool,
}

#[tauri::command]
pub fn get_mcp_status(app: AppHandle) -> McpStatus {
    let db_state = app.state::<DbState>();
    let running = is_running();
    let port = if running { active_port() } else { 0 };
    let allow_lan_now = allow_lan(&db_state);
    McpStatus {
        running,
        port,
        enabled: enabled(&db_state),
        allow_write: allow_write(&db_state),
        autostart: autostart(&db_state),
        require_token: require_token(&db_state),
        allow_lan: allow_lan_now,
        token: token_or_create(&db_state),
        endpoint: if running {
            format!("http://127.0.0.1:{}/mcp", port)
        } else {
            String::new()
        },
        lan_endpoint: if running && allow_lan_now {
            local_lan_endpoint(port)
        } else {
            String::new()
        },
        default_port: DEFAULT_PORT,
        default_enabled: DEFAULT_ENABLED,
        default_allow_write: DEFAULT_ALLOW_WRITE,
        default_require_token: DEFAULT_REQUIRE_TOKEN,
        default_allow_lan: DEFAULT_ALLOW_LAN,
    }
}

/// 拼出局域网入口。取不到本机地址时回退到 `0.0.0.0`，让用户至少看到"要自己
/// 换成实际 IP"这个事实，而不是一个空白。
fn local_lan_endpoint(port: u16) -> String {
    let ip = local_ip_address::local_ip()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|_| "0.0.0.0".to_string());
    format!("http://{}:{}/mcp", ip, port)
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

/// 把"写权限"的新值推给**正在运行**的服务实例。
///
/// 单独抽出来是为了可测：命令函数要 `AppHandle`，单测里构造不出来，于是这段真正
/// 决定"改动是否立刻生效"的逻辑一直没有保护——把它退回旧行为（只写库、不到达运行
/// 中的服务）时全套测试依然全绿。这个函数只依赖全局状态，可以直接断言。
fn push_allow_write_to_running_service(allow: bool) {
    if let Ok(guard) = ACTIVE_STATE.lock() {
        if let Some(state) = guard.as_ref() {
            state.settings.set_allow_write(allow);
        }
    }
}

/// 把"是否强制校验令牌"的新值推给正在运行的服务实例。理由同上。
fn push_require_token_to_running_service(require: bool) {
    if let Ok(guard) = ACTIVE_STATE.lock() {
        if let Some(state) = guard.as_ref() {
            state.token.set_require_token(require);
        }
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
    // 默认全权限之后，"立刻收回写权限"必须是真能生效的动作，而不是等下次重启。
    push_allow_write_to_running_service(allow);
    crate::info!(
        "[MCP-AUDIT] {} 写权限被设置为 {}",
        chrono::Local::now().to_rfc3339(),
        allow
    );
    Ok(())
}

/// 切换"是否强制校验令牌"。只影响后续请求，不重启服务。
///
/// 与 [`set_mcp_allow_write`] 同构：数据库与运行时开关一起更新。用户打开局域网
/// 访问之后，这一步就是马上把校验加回来的手段。
#[tauri::command]
pub fn set_mcp_require_token(app: AppHandle, require: bool) -> Result<(), String> {
    let db_state = app.state::<DbState>();
    db_state
        .settings_repo
        .set(KEY_REQUIRE_TOKEN, if require { "true" } else { "false" })
        .map_err(|e| e.to_string())?;
    push_require_token_to_running_service(require);
    crate::info!(
        "[MCP-AUDIT] {} 令牌校验被设置为 {}",
        chrono::Local::now().to_rfc3339(),
        require
    );
    Ok(())
}

/// 切换"是否允许局域网访问"。**必须重启服务**：监听地址是在 bind 时定下的，
/// 运行中无法改；这里按端口设置的同款做法停掉再拉起。
///
/// 返回重启后的实际端口（未运行时返回 0）。
#[tauri::command]
pub async fn set_mcp_allow_lan(app: AppHandle, allow: bool) -> Result<u16, String> {
    {
        let db_state = app.state::<DbState>();
        db_state
            .settings_repo
            .set(KEY_ALLOW_LAN, if allow { "true" } else { "false" })
            .map_err(|e| e.to_string())?;
    }
    crate::info!(
        "[MCP-AUDIT] {} 局域网访问被设置为 {}",
        chrono::Local::now().to_rfc3339(),
        allow
    );
    if !is_running() {
        return Ok(0);
    }
    let port = active_port();
    stop(&app)?;
    start(app, Some(port)).await
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

    // -----------------------------------------------------------------------
    // 出厂默认姿态（端到端：真 DbState + 真读取函数）
    // -----------------------------------------------------------------------
    //
    // 与 `store.rs` 的 `shipping_defaults_match_the_required_posture` 配对：
    // 那条盯常量，这条盯"常量真的被读取函数用上了"。只写常量却忘了改读取函数的
    // 默认值参数，是这类改动最常见的漏法，只有这条能抓住。

    /// 造一个"刚装好、用户什么都没设置过"的空库。
    fn fresh_db_state() -> DbState {
        let conn = Arc::new(Mutex::new(
            rusqlite::Connection::open_in_memory().expect("内存库应可创建"),
        ));
        conn.lock()
            .expect("连接锁可用")
            .execute_batch(store::MCP_SCHEMA)
            .expect("schema 应可建表");
        DbState {
            repo: crate::infrastructure::repository::clipboard_repo::SqliteClipboardRepository::new(
                conn.clone(),
            ),
            tag_repo: crate::infrastructure::repository::tag_repo::SqliteTagRepository::new(
                conn.clone(),
            ),
            settings_repo:
                crate::infrastructure::repository::settings_repo::SqliteSettingsRepository::new(
                    conn.clone(),
                ),
            conn,
        }
    }

    /// 造一个可放进 `ACTIVE_STATE` 的运行中服务状态，用于验证"热更新会到达运行中的
    /// 服务"。只关心开关字段，其余依赖用最小实现。
    fn running_state_for_hot_reload(allow_write_seed: bool, require_token_seed: bool) -> ServerState {
        let db = fresh_db_state();
        let store = store::McpStore::new(db.conn.clone());
        ServerState {
            store,
            token: server::AuthToken::new(Arc::new("test-secret".to_string()), require_token_seed),
            settings: Arc::new(server::RuntimeSettings::new(allow_write_seed)),
            audit: Arc::new(server::NoopAudit),
            effects: Arc::new(tools::NoopEffects::default()),
        }
    }

    /// 安装一个运行中状态，并在测试结束时清空，避免污染同进程的其它测试。
    struct ActiveStateGuard;
    impl ActiveStateGuard {
        fn install(state: ServerState) -> Self {
            *ACTIVE_STATE.lock().expect("全局状态锁可用") = Some(state);
            Self
        }
    }
    impl Drop for ActiveStateGuard {
        fn drop(&mut self) {
            *ACTIVE_STATE.lock().expect("全局状态锁可用") = None;
        }
    }

    /// 关闭写权限必须**立刻**到达运行中的服务。
    ///
    /// 这条曾经无人保护：把热更新退回旧行为（只写库、不到达运行中的服务）时，全套
    /// 测试依然全绿。默认全权限之后，"一键收回写权限"是用户唯一能立刻止血的动作，
    /// 它悄悄失效是不能接受的，所以这里直接断言运行中实例的开关真的被翻过来了。
    #[test]
    fn disabling_write_reaches_the_running_service_immediately() {
        let state = running_state_for_hot_reload(true, false);
        let live = state.settings.clone();
        let _guard = ActiveStateGuard::install(state);

        assert!(live.allow_write(), "前置：运行中的服务初始允许写入");

        push_allow_write_to_running_service(false);

        assert!(
            !live.allow_write(),
            "关闭写权限后，运行中的服务实例必须立刻变为禁止写入，而不是等下次重启"
        );
    }

    /// 打开令牌校验必须**立刻**到达运行中的服务。理由同上。
    #[test]
    fn enabling_token_check_reaches_the_running_service_immediately() {
        let state = running_state_for_hot_reload(true, false);
        let live = state.token.clone();
        let _guard = ActiveStateGuard::install(state);

        assert!(!live.requires_token(), "前置：运行中的服务初始免鉴权");

        push_require_token_to_running_service(true);

        assert!(
            live.requires_token(),
            "打开令牌校验后，运行中的服务实例必须立刻要求令牌"
        );
    }

    /// 服务未运行时，热更新不应 panic，也不应留下任何状态。
    #[test]
    fn hot_reload_is_a_no_op_when_no_service_is_running() {
        *ACTIVE_STATE.lock().expect("全局状态锁可用") = None;
        // 不 panic 即通过；这两条路径在服务停止时会被用户正常触发。
        push_allow_write_to_running_service(false);
        push_require_token_to_running_service(true);
        assert!(ACTIVE_STATE.lock().expect("锁可用").is_none());
    }

    #[test]
    fn defaults_survive_a_fresh_database() {
        let state = fresh_db_state();

        assert!(enabled(&state), "空库应默认开启 MCP 服务");
        assert!(allow_write(&state), "空库应默认允许 AI 修改（全权限）");
        assert!(autostart(&state), "空库应默认随应用自动启动");
        assert!(!require_token(&state), "空库应默认免鉴权");
        assert!(!allow_lan(&state), "空库应默认只监听本机");
        assert_eq!(configured_port(&state), 23123, "空库应默认使用 23123 端口");
    }

    #[test]
    fn user_written_values_override_the_defaults() {
        // 默认值是"没设置时"的行为，不是"不许设置"。用户显式写入必须生效。
        let state = fresh_db_state();
        state.settings_repo.set(KEY_ENABLED, "false").unwrap();
        state.settings_repo.set(KEY_ALLOW_WRITE, "false").unwrap();
        state.settings_repo.set(KEY_AUTOSTART, "false").unwrap();
        state.settings_repo.set(KEY_REQUIRE_TOKEN, "true").unwrap();
        state.settings_repo.set(KEY_ALLOW_LAN, "true").unwrap();
        state.settings_repo.set(KEY_PORT, "34567").unwrap();

        assert!(!enabled(&state));
        assert!(!allow_write(&state));
        assert!(!autostart(&state));
        assert!(require_token(&state));
        assert!(allow_lan(&state));
        assert_eq!(configured_port(&state), 34567);
    }

    #[test]
    fn listen_ip_follows_the_lan_switch() {
        // 绑定地址必须是开关的函数，不能被硬编码回回环。
        assert_eq!(listen_ip(false), std::net::Ipv4Addr::LOCALHOST);
        assert_eq!(listen_ip(true), std::net::Ipv4Addr::UNSPECIFIED);
        assert!(listen_ip(false).is_loopback());
        assert!(!listen_ip(true).is_loopback());
    }
}
