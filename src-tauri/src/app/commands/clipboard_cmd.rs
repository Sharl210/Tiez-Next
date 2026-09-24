use crate::app_state::{AppDataDir, EncryptionQueueState, SessionHistory};
use crate::database::{self, DbState};
use crate::error::{AppError, AppResult};
use crate::infrastructure::repository::clipboard_repo::ClipboardRepository;
use crate::services::clipboard_mutation::{
    self, new_entry_preview, SensitiveTransition,
};
use crate::services::encryption_queue::{EncryptionAction, EncryptionJob};
use tauri::{AppHandle, Emitter, Manager, State};

#[tauri::command]
pub fn toggle_clipboard_pin(
    app_handle: AppHandle,
    state: State<'_, DbState>,
    session: State<'_, SessionHistory>,
    app_data_dir: State<'_, AppDataDir>,
    id: i64,
    is_pinned: bool,
) -> AppResult<i64> {
    let mut real_id = id;
    let mut entry_to_save = None;

    {
        let mut session_items = session.inner().0.lock().unwrap();
        if let Some(item) = session_items.iter_mut().find(|i| i.id == id) {
            item.is_pinned = is_pinned;
            if id < 0 && is_pinned {
                entry_to_save = Some(item.clone());
            }
        }
    }

    let conn = state.conn.lock().unwrap();

    if let Some(entry) = entry_to_save {
        let data_dir = app_data_dir.0.lock().unwrap().clone();
        if let Ok(new_id) = state.repo.save_with_conn(&conn, &entry, Some(&data_dir)) {
            real_id = new_id;
            if let Ok(deleted_ids) = state.repo.enforce_limit_with_conn(&conn, Some(&data_dir)) {
                for deleted_id in deleted_ids {
                    let _ = app_handle.emit("clipboard-removed", deleted_id);
                }
            }
            {
                let mut session_items = session.inner().0.lock().unwrap();
                if let Some(item) = session_items.iter_mut().find(|i| i.id == id) {
                    item.id = new_id;
                }
            }
        }
    }

    if real_id > 0 {
        state
            .repo
            .toggle_pin_with_conn(&conn, real_id, is_pinned)
            .map_err(AppError::from)?;
    }
    drop(conn);
    let _ = app_handle.emit("clipboard-changed", ());
    crate::services::cloud_sync::request_cloud_sync(app_handle);
    Ok(real_id)
}

#[tauri::command]
pub fn update_tags(
    app_handle: AppHandle,
    state: State<'_, DbState>,
    session: State<'_, SessionHistory>,
    app_data_dir: State<'_, AppDataDir>,
    id: i64,
    tags: Vec<String>,
) -> AppResult<i64> {
    if id < 0 {
        let mut session_items = session.inner().0.lock().unwrap();
        if let Some(index) = session_items.iter().position(|item| item.id == id) {
            let mut item = session_items[index].clone();
            item.tags = tags.clone();

            let data_dir = app_data_dir.0.lock().unwrap().clone();
            let new_id = state.repo.save(&item, Some(&data_dir))?;

            session_items[index].id = new_id;
            session_items[index].tags = tags;
            crate::services::cloud_sync::request_cloud_sync(app_handle);
            return Ok(new_id);
        }
        return Err(AppError::Validation("Item not found".to_string()));
    }

    let transition = clipboard_mutation::apply_entry_tags(&state.conn, &state.tag_repo, id, tags)?;
    if let Some(action) = encryption_action_for(transition) {
        let queue = app_handle.state::<EncryptionQueueState>();
        queue.0.enqueue(EncryptionJob { id, action });
    }
    crate::services::cloud_sync::request_cloud_sync(app_handle);
    Ok(id)
}

/// 把共享的敏感性翻转结果翻译成加密队列动作。
///
/// 判定本身在 [`clipboard_mutation::apply_entry_tags`] 里，与 MCP 走的是同一份
/// 代码——这里只是把结论映射成队列项，因此界面路径与 AI 路径不可能判定不一致。
///
/// `pub(crate)`：`history_cmd` 的移动/复制命令也要用同一份映射。复制一遍映射表
/// 等于给"两条路径判定不一致"留了一个入口。
pub(crate) fn encryption_action_for(transition: SensitiveTransition) -> Option<EncryptionAction> {
    match transition {
        SensitiveTransition::Encrypt => Some(EncryptionAction::Encrypt),
        SensitiveTransition::Decrypt => Some(EncryptionAction::Decrypt),
        SensitiveTransition::None => None,
    }
}

#[tauri::command]
pub async fn add_manual_item(
    app_handle: AppHandle,
    state: State<'_, DbState>,
    content: String,
    content_type: String,
    tags: Vec<String>,
) -> AppResult<i64> {
    let preview = new_entry_preview(&content);

    let entry = database::ClipboardEntry {
        id: 0,
        content_type,
        content,
        html_content: None,
        source_app: "Manual".to_string(),
        source_app_path: None,
        timestamp: chrono::Utc::now().timestamp_millis(),
        preview,
        is_pinned: false,
        tags,
        use_count: 0,
        is_external: false,
        pinned_order: 0,
        note: String::new(),
        file_preview_exists: true,
    };

    let app_data_dir = app_handle.state::<AppDataDir>();
    let data_dir = app_data_dir.0.lock().unwrap().clone();
    let new_id = state.repo.save(&entry, Some(&data_dir))?;
    let _ = app_handle.emit("clipboard-changed", ());
    crate::services::cloud_sync::request_cloud_sync(app_handle);
    Ok(new_id)
}

/// R13：改条目正文，`html_content` 可同时带上富文本 HTML。
///
/// # 为什么这个命令需要 `html_content`
///
/// 修复之前，编辑富文本条目等于**降级**：仓储层把 `content_type` 改成 `text` 并把
/// `html_content` 置 NULL，界面只在弹窗里警告一句"格式会丢失"。用户要的是"编辑
/// 富文本不会坍缩成纯文本"，所以正文与 HTML 必须作为**一次写入**落到同一行 ——
/// 分两次调用会让中间态（正文已改、HTML 还是旧的）被并发读取到。
///
/// `html_content` 三种取值（与仓储层一致）：
/// * `None` —— 本次不含 HTML。富文本条目保留原 HTML，**不降级**。
/// * `Some("")` —— 显式清空 HTML。
/// * `Some(html)` —— 写入该 HTML，`content_type` 保持 `rich_text`。
///
/// 净化（白名单）由仓储层之前的调用方完成，命令层不透传未净化的 HTML。
#[tauri::command]
pub async fn update_item_content(
    app_handle: AppHandle,
    app_data_dir: State<'_, AppDataDir>,
    state: State<'_, DbState>,
    session: State<'_, SessionHistory>,
    id: i64,
    new_content: String,
    html_content: Option<String>,
) -> AppResult<()> {
    let base_preview = clipboard_mutation::body_preview(&new_content);

    // R13：把用户编辑的 HTML 过一遍白名单净化器，再落库。
    //
    // 这是**安全边界**而不是格式美化：允许用户写入任意 HTML 之后，`<img src="file:///...">`
    // 配合 `tauri.conf.json` 里 `assetProtocol.scope` 的 `$HOME/**` 会变成一条真实的
    // 本地文件读取通道，而 CSP 又显式允许内联脚本（`script-src 'unsafe-inline'`）。
    // 净化必须在写入路径上做，显示侧的弱净化只是第二道。
    let sanitized_html: Option<String> = html_content.as_deref().map(|raw| {
        let data_dir = app_data_dir.0.lock().unwrap().clone();
        let attachments = data_dir.join("attachments");
        crate::domain::rich_html::sanitize_rich_html(raw, Some(attachments.as_path()))
    });

    // R13：界面送来的是编辑器里的 `innerHTML`，**正文列必须是派生的纯文本**。
    //
    // 仓储层也会做同一件事（权威口径在那里），但会话态是用这里的值直接改内存的；
    // 若不同步派生，会出现"列表里显示 HTML 源码、库里是纯文本"的错位，而
    // `history_cmd` 恰好会把会话态条目合并进首页列表 —— 用户就先看到错的那一份。
    let content_type = state
        .repo
        .get_entry_by_id(id)
        .ok()
        .flatten()
        .map(|e| e.content_type)
        .unwrap_or_else(|| "text".to_string());
    let is_rich_edit = content_type == "rich_text" && sanitized_html.is_some();
    let plain_content: String = if is_rich_edit {
        crate::services::clipboard::derive_rich_text_content(
            &new_content,
            sanitized_html.as_deref(),
        )
    } else {
        new_content.clone()
    };
    let preview = if is_rich_edit {
        crate::services::clipboard::build_entry_preview(
            "rich_text",
            &plain_content,
            sanitized_html.as_deref(),
        )
    } else {
        base_preview
    };

    // Persist first, then mirror into the session list. The reverse order left the
    // in-memory copy updated even when the repository refused the edit (binary
    // content types are rejected there), so the session list disagreed with the
    // database until the next refresh.
    state
        .repo
        .update_entry_content(id, &plain_content, &preview, sanitized_html.as_deref())
        .map_err(AppError::from)?;

    {
        let mut session_items = session.inner().0.lock().unwrap();
        if let Some(item) = session_items.iter_mut().find(|i| i.id == id) {
            // R13：会话态必须与库内一致。这里曾无条件把 `html_content` 置 None 并把
            // `content_type` 改成 `text` —— 而 `history_cmd` 会把会话态条目合并进首页
            // 列表，于是用户先看到"没降级"，切窗口后才看到降级。
            //
            // 改用共享实现（`clipboard_mutation::mirror_body_edit_in_session`），
            // 那条不变量由它的单元测试守着，而不是靠这里的写法"看起来对"。
            clipboard_mutation::mirror_body_edit_in_session(
                item,
                &plain_content,
                &preview,
                sanitized_html.as_deref(),
            );
        }
    }

    let _ = app_handle.emit("clipboard-changed", ());
    crate::services::cloud_sync::request_cloud_sync(app_handle);
    Ok(())
}

/// R4: `image` / `file` / `video` store a path or a `data:` URL in `content`, so
/// their body is not editable text. Editing one would rewrite `content` while
/// leaving `content_hash` on the old payload — a row whose hash and content
/// disagree, which dedup and cloud sync then mis-handle.
///
/// The authoritative guard lives in the repository
/// (`SqliteClipboardRepository::update_entry_content_with_conn`), so it also covers
/// the AI-rewrite and `open_content` callers; the tag manager additionally hides the
/// body field for these types. Nothing in this module needs the predicate directly,
/// only the reasoning, which is recorded here because this is where the command is
/// exposed to the UI.


/// R6: set or clear the user note of one entry.
///
/// Naming is fixed to `update_entry_note` to match the beta branch so the two can
/// be merged without a rename. The note is stored independently of `content`, so
/// this is valid for every content type, including the binary ones that
/// [`is_binary_content_type`] protects from body edits. An empty or
/// whitespace-only `note` clears it.
#[tauri::command]
pub async fn update_entry_note(
    app_handle: AppHandle,
    state: State<'_, DbState>,
    session: State<'_, SessionHistory>,
    id: i64,
    note: String,
) -> AppResult<()> {
    use crate::infrastructure::repository::clipboard_repo::normalize_note;

    let normalized = normalize_note(&note);

    // Session (not-yet-persisted) entries: mirror the `update_item_content`
    // approach. A session row with a negative id has no database counterpart yet,
    // so updating the in-memory copy is the only meaningful write; it will be
    // persisted later by `save`, which already round-trips the note column.
    {
        let mut session_items = session.inner().0.lock().unwrap();
        if let Some(item) = session_items.iter_mut().find(|i| i.id == id) {
            item.note = normalized.clone();
        }
    }

    if id > 0 {
        state
            .repo
            .update_entry_note(id, &normalized)
            .map_err(AppError::from)?;
    }

    let _ = app_handle.emit("clipboard-changed", ());
    crate::services::cloud_sync::request_cloud_sync(app_handle);
    Ok(())
}
