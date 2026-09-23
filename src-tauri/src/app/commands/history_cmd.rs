use crate::app_state::{AppDataDir, EncryptionQueueState, SessionHistory};
use crate::database::DbState;
use crate::domain::models::ClipboardEntry;
use crate::error::{AppError, AppResult};
use crate::infrastructure::repository::clipboard_repo::ClipboardRepository;
use crate::infrastructure::repository::tag_repo::TagRepository;
use crate::services::clipboard::{
    build_entry_preview, derive_rich_text_content, truncate_html_for_preview,
};
use crate::services::clipboard_mutation::{self, TagTransfer};
use crate::services::encryption_queue::EncryptionJob;
use tauri::{AppHandle, Emitter, Manager, State};

fn normalize_rich_text_item_content(item: &mut ClipboardEntry) {
    if item.content_type != "rich_text" {
        return;
    }

    let normalized = derive_rich_text_content(&item.content, item.html_content.as_deref());
    if !normalized.trim().is_empty() {
        item.content = normalized;
    }
}

#[tauri::command]
pub fn get_clipboard_history(
    state: State<'_, DbState>,
    session: State<'_, SessionHistory>,
    limit: i32,
    offset: i32,
    content_type: Option<String>,
) -> AppResult<Vec<ClipboardEntry>> {
    // 1. Get history from repository
    let mut history = state
        .repo
        .get_history(limit, offset, content_type.as_deref())?;

    // 2. Add session history items (non-persisted) ONLY on the first page
    if offset == 0 {
        let session_items = session.inner().0.lock().unwrap();
        for item in session_items.iter().rev() {
            if let Some(ct) = content_type.as_deref() {
                if item.content_type != ct {
                    continue;
                }
            }
            // Avoid duplicates: if item is already in DB, it will have id > 0
            if !history.iter().any(|h| h.id == item.id && item.id != 0) {
                history.push(item.clone());
            }
        }
    }

    // 3. Apply stable sorting: Pinned -> Pinned Order -> Timestamp -> ID
    // This MUST match the repository's logic to maintain pagination stability
    history.sort_by(|a, b| {
        b.is_pinned
            .cmp(&a.is_pinned)
            .then_with(|| b.pinned_order.cmp(&a.pinned_order))
            .then_with(|| b.timestamp.cmp(&a.timestamp))
            .then_with(|| b.id.cmp(&a.id))
    });

    // 4. Truncate to limit
    if history.len() > limit as usize {
        history.truncate(limit as usize);
    }

    // 5. Truncate content for UI performance
    for item in &mut history {
        normalize_rich_text_item_content(item);

        if (item.content_type == "text"
            || item.content_type == "code"
            || item.content_type == "url"
            || item.content_type == "rich_text")
            && item.content.chars().count() > 2000
        {
            item.content = format!(
                "{}... [Truncated for speed]",
                item.content.chars().take(2000).collect::<String>()
            );
        }

        if let Some(ref html) = item.html_content {
            if html.chars().count() > 5000 {
                item.html_content = truncate_html_for_preview(html);
            }
        }

        if item.content_type == "text"
            || item.content_type == "code"
            || item.content_type == "url"
            || item.content_type == "rich_text"
        {
            item.preview = build_entry_preview(
                &item.content_type,
                &item.content,
                item.html_content.as_deref(),
            );
        }
    }

    Ok(history)
}

#[tauri::command]
pub fn search_clipboard_history(
    state: State<'_, DbState>,
    session: State<'_, SessionHistory>,
    search_term: String,
    limit: i32,
    tag_only: Option<bool>,
) -> AppResult<Vec<ClipboardEntry>> {
    let is_tag_only = tag_only.unwrap_or(false);
    let mut history = state.repo.search(&search_term, limit, is_tag_only)?;

    let term = search_term.to_lowercase();
    let session_items = session.inner().0.lock().unwrap();
    for item in session_items.iter().rev() {
        let matches = if is_tag_only {
            item.tags.iter().any(|t| t.to_lowercase().contains(&term))
        } else {
            item.content.to_lowercase().contains(&term)
                || item.source_app.to_lowercase().contains(&term)
                || item.tags.iter().any(|t| t.to_lowercase().contains(&term))
        };

        if matches {
            if !history.iter().any(|h| h.id == item.id && item.id != 0) {
                history.push(item.clone());
            }
        }
    }

    history.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then_with(|| b.id.cmp(&a.id)));
    if history.len() > limit as usize {
        history.truncate(limit as usize);
    }

    for item in &mut history {
        normalize_rich_text_item_content(item);

        if (item.content_type == "text"
            || item.content_type == "code"
            || item.content_type == "url"
            || item.content_type == "rich_text")
            && item.content.chars().count() > 2000
        {
            item.content = format!(
                "{}... [Truncated for speed]",
                item.content.chars().take(2000).collect::<String>()
            );
        }

        if let Some(ref html) = item.html_content {
            if html.chars().count() > 5000 {
                item.html_content = truncate_html_for_preview(html);
            }
        }

        if item.content_type == "text"
            || item.content_type == "code"
            || item.content_type == "url"
            || item.content_type == "rich_text"
        {
            item.preview = build_entry_preview(
                &item.content_type,
                &item.content,
                item.html_content.as_deref(),
            );
        }
    }

    Ok(history)
}

#[tauri::command]
pub fn delete_clipboard_entry(
    app_handle: AppHandle,
    state: State<'_, DbState>,
    session: State<'_, SessionHistory>,
    app_data: State<'_, AppDataDir>,
    id: i64,
) -> AppResult<()> {
    {
        let mut session_items = session.inner().0.lock().unwrap();
        session_items.retain(|item| item.id != id);
    }

    if id > 0 {
        let data_dir = app_data.0.lock().unwrap();
        state.repo.delete(id, Some(&data_dir))?;
    }
    let _ = app_handle.emit("clipboard-changed", ());
    crate::services::cloud_sync::request_cloud_sync(app_handle);
    Ok(())
}

#[tauri::command]
pub fn clear_clipboard_history(
    app_handle: AppHandle,
    state: State<'_, DbState>,
    session: State<'_, SessionHistory>,
    app_data: State<'_, AppDataDir>,
) -> AppResult<()> {
    {
        let mut session_items = session.inner().0.lock().unwrap();
        session_items.retain(|item| item.is_pinned || !item.tags.is_empty());
    }
    let data_dir = app_data.0.lock().unwrap();
    state.repo.clear(Some(&data_dir)).map_err(AppError::from)?;
    let _ = app_handle.emit("clipboard-changed", ());
    crate::services::cloud_sync::request_cloud_sync(app_handle);
    Ok(())
}

#[tauri::command]
pub fn get_tag_items(state: State<'_, DbState>, tag: String) -> AppResult<Vec<ClipboardEntry>> {
    let mut history = state
        .tag_repo
        .get_entries_by_tag(&tag)
        .map_err(AppError::from)?;

    for item in &mut history {
        normalize_rich_text_item_content(item);

        if (item.content_type == "text"
            || item.content_type == "code"
            || item.content_type == "url"
            || item.content_type == "rich_text")
            && item.content.chars().count() > 50000
        {
            item.content = format!(
                "{}... [Content Truncated]",
                item.content.chars().take(50000).collect::<String>()
            );
        }

        if item.content_type == "text"
            || item.content_type == "code"
            || item.content_type == "url"
            || item.content_type == "rich_text"
        {
            item.preview = build_entry_preview(
                &item.content_type,
                &item.content,
                item.html_content.as_deref(),
            );
        }
    }

    Ok(history)
}

#[tauri::command]
pub fn get_all_tags_info(
    state: State<'_, DbState>,
) -> AppResult<std::collections::HashMap<String, i32>> {
    state.tag_repo.get_all_with_counts().map_err(AppError::from)
}

/// 标签及其排序用统计量（最近使用时间、总字节数等）。
///
/// 与 `get_all_tags_info` 并存：后者返回的 `name -> count` 形状仍被多处使用。
#[tauri::command]
pub fn get_tag_stats(
    state: State<'_, DbState>,
) -> AppResult<Vec<crate::infrastructure::repository::tag_repo::TagStats>> {
    state.tag_repo.get_all_with_stats().map_err(AppError::from)
}

#[tauri::command]
pub fn rename_tag_globally(
    state: State<'_, DbState>,
    session: State<'_, SessionHistory>,
    old_name: String,
    new_name: String,
) -> AppResult<()> {
    {
        let mut session_items = session.inner().0.lock().unwrap();
        for item in session_items.iter_mut() {
            for tag in item.tags.iter_mut() {
                if *tag == old_name {
                    *tag = new_name.clone();
                }
            }
            item.tags.sort();
            item.tags.dedup();
        }
    }

    state
        .tag_repo
        .rename(&old_name, &new_name)
        .map_err(AppError::from)
}

#[tauri::command]
pub fn delete_tag_from_all(
    state: State<'_, DbState>,
    session: State<'_, SessionHistory>,
    app_data: State<'_, AppDataDir>,
    tag_name: String,
) -> AppResult<()> {
    // R3: unlinking the tag from in-session entries, not dropping those entries.
    //
    // This used to be `session_items.retain(|item| !item.tags.contains(&tag_name))`,
    // which discarded every not-yet-persisted entry that happened to carry the tag —
    // the same "delete a group, lose your data" defect the repository path had, only
    // harder to notice because session entries are the most recently copied ones.
    // Matching is case-insensitive to mirror the repository's `COLLATE NOCASE`.
    {
        let mut session_items = session.inner().0.lock().unwrap();
        for item in session_items.iter_mut() {
            if item.tags.iter().any(|t| t.eq_ignore_ascii_case(&tag_name)) {
                item.tags.retain(|t| !t.eq_ignore_ascii_case(&tag_name));
            }
        }
    }

    let data_dir = app_data.0.lock().unwrap();
    state
        .tag_repo
        .delete_globally(&tag_name, Some(&data_dir))
        .map_err(AppError::from)
}

#[tauri::command]
pub fn create_new_tag(state: State<'_, DbState>, tag_name: String) -> AppResult<()> {
    state.tag_repo.create(&tag_name).map_err(AppError::from)
}

/// 把条目从标签 `from_tag` **移动**到标签 `to_tag`。
///
/// 语义（与 MCP 的 `move_entry_to_tag` 逐字一致，因为判定与写入都在
/// `clipboard_mutation::apply_entry_tag_transfer`）：`entry_tags` 是
/// `(entry_id, tag)` 的多对多表，所以"移动"是**在集合里把源标签换成目标标签**，
/// 条目上的其他标签一个都不动。`[工作, 待办]` 执行 `工作 → 归档` 之后是
/// `[归档, 待办]`，不是 `[归档]`。
///
/// 副作用与 `update_tags` 同源：先在共享内核里算敏感性翻转，再决定是否入队加解密。
/// 这里额外发一次 `clipboard-changed`（`update_tags` 没有发），让标签计数等派生视图
/// 立刻跟上——不改这一点也不会出错，但会让界面在标签页里显示陈旧计数。
#[tauri::command]
pub fn move_entry_to_tag(
    app_handle: AppHandle,
    state: State<'_, DbState>,
    session: State<'_, SessionHistory>,
    app_data: State<'_, AppDataDir>,
    id: i64,
    from_tag: String,
    to_tag: String,
) -> AppResult<i64> {
    transfer_entry_tag(
        app_handle,
        state,
        session,
        app_data,
        id,
        &from_tag,
        &to_tag,
        TagTransfer::Move,
    )
}

/// 把条目**复制**到标签 `to_tag`：源标签保留，目标标签追加（已存在则不重复）。
#[tauri::command]
pub fn copy_entry_to_tag(
    app_handle: AppHandle,
    state: State<'_, DbState>,
    session: State<'_, SessionHistory>,
    app_data: State<'_, AppDataDir>,
    id: i64,
    from_tag: String,
    to_tag: String,
) -> AppResult<i64> {
    transfer_entry_tag(
        app_handle,
        state,
        session,
        app_data,
        id,
        &from_tag,
        &to_tag,
        TagTransfer::Copy,
    )
}

/// 移动与复制的共同实现。两个命令的差别只有 [`TagTransfer`] 这一个枚举值，
/// 其余（会话态条目落库、敏感性判定、加解密入队、云同步）逐字相同。
#[allow(clippy::too_many_arguments)]
fn transfer_entry_tag(
    app_handle: AppHandle,
    state: State<'_, DbState>,
    session: State<'_, SessionHistory>,
    app_data: State<'_, AppDataDir>,
    id: i64,
    from_tag: &str,
    to_tag: &str,
    kind: TagTransfer,
) -> AppResult<i64> {
    // 尚未落库的会话条目（id < 0）只存在于内存里，`entry_tags` 里没有它。
    // 因此在这里就地算出结果集合，再照 `update_tags` 的老路落库。
    if id < 0 {
        let mut session_items = session.inner().0.lock().unwrap();
        if let Some(index) = session_items.iter().position(|item| item.id == id) {
            let mut item = session_items[index].clone();
            let next = clipboard_mutation::transferred_tags(&item.tags, from_tag, to_tag, kind)?;
            item.tags = next.clone();

            let data_dir = app_data.0.lock().unwrap().clone();
            let new_id = state.repo.save(&item, Some(&data_dir))?;

            session_items[index].id = new_id;
            session_items[index].tags = next;
            drop(session_items);

            let _ = app_handle.emit("clipboard-changed", ());
            crate::services::cloud_sync::request_cloud_sync(app_handle);
            return Ok(new_id);
        }
        return Err(AppError::Validation("Item not found".to_string()));
    }

    let transition = clipboard_mutation::apply_entry_tag_transfer(
        &state.conn,
        &state.tag_repo,
        id,
        from_tag,
        to_tag,
        kind,
    )?;
    if let Some(action) = super::clipboard_cmd::encryption_action_for(transition) {
        let queue = app_handle.state::<EncryptionQueueState>();
        queue.0.enqueue(EncryptionJob { id, action });
    }
    let _ = app_handle.emit("clipboard-changed", ());
    crate::services::cloud_sync::request_cloud_sync(app_handle);
    Ok(id)
}

#[tauri::command]
pub fn get_clipboard_content(
    state: State<'_, DbState>,
    session: State<'_, SessionHistory>,
    id: i64,
) -> AppResult<String> {
    {
        let session_items = session.inner().0.lock().unwrap();
        if let Some(item) = session_items.iter().find(|i| i.id == id) {
            if item.content_type == "rich_text" {
                let normalized =
                    derive_rich_text_content(&item.content, item.html_content.as_deref());
                if !normalized.trim().is_empty() {
                    return Ok(normalized);
                }
            }
            return Ok(item.content.clone());
        }
    }

    if let Some((content, content_type, html_content)) = state
        .repo
        .get_entry_content_with_html(id)
        .map_err(AppError::from)?
    {
        if content_type == "rich_text" {
            let normalized = derive_rich_text_content(&content, html_content.as_deref());
            if !normalized.trim().is_empty() {
                return Ok(normalized);
            }
        }
        return Ok(content);
    }

    Err(AppError::Validation("Entry not found".to_string()))
}

#[tauri::command]
pub fn update_pinned_order(
    app_handle: AppHandle,
    state: State<'_, DbState>,
    orders: Vec<(i64, i64)>,
) -> AppResult<()> {
    state
        .repo
        .update_pinned_order(orders)
        .map_err(AppError::from)?;
    let _ = app_handle.emit("clipboard-changed", ());
    crate::services::cloud_sync::request_cloud_sync(app_handle);
    Ok(())
}

#[tauri::command]
pub fn get_db_count(state: State<'_, DbState>) -> AppResult<i64> {
    state.repo.get_count().map_err(AppError::from)
}
