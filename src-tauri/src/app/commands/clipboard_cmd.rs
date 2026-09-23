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

#[tauri::command]
pub async fn update_item_content(
    app_handle: AppHandle,
    state: State<'_, DbState>,
    session: State<'_, SessionHistory>,
    id: i64,
    new_content: String,
) -> AppResult<()> {
    let preview = clipboard_mutation::body_preview(&new_content);

    // Persist first, then mirror into the session list. The reverse order left the
    // in-memory copy updated even when the repository refused the edit (binary
    // content types are rejected there), so the session list disagreed with the
    // database until the next refresh.
    state
        .repo
        .update_entry_content(id, &new_content, &preview)
        .map_err(AppError::from)?;

    {
        let mut session_items = session.inner().0.lock().unwrap();
        if let Some(item) = session_items.iter_mut().find(|i| i.id == id) {
            item.content = new_content.clone();
            item.preview = preview.clone();
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
