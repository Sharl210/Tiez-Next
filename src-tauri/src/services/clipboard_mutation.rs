//! 剪贴板条目与标签的**共享变更内核**。
//!
//! # 这一层解决什么问题
//!
//! 同一个变更现在有两条入口：用户在界面里点（Tauri 命令），和 AI 通过 MCP 调。
//! 两条路径必须产生**逐字节相同**的库内结果，否则会出现最难排查的一类状态不一致
//! ——例如 AI 给某条打了 `sensitive` 标签却没触发加解密，界面里看起来打了标签，
//! 数据却是明文躺在库里。
//!
//! 因此凡是"改数据 + 有副作用"的操作，真正的实现只写一遍，放在这里：
//!
//! * Tauri 命令负责补齐宿主侧动作（发 UI 事件、请求云同步）；
//! * MCP 运行时负责补齐同样的宿主侧动作；
//! * 两边调用的都是本模块的函数，函数**只做数据库那一步**，并把"接下来该做什么
//!   副作用"作为返回值交出去，由调用方执行。
//!
//! 这样一来，"两条路径一致"不是靠约定，而是靠构造：副作用判定只有一份代码。

use rusqlite::Connection;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::database::has_sensitive_tag;
use crate::infrastructure::repository::clipboard_repo::{normalize_note, ClipboardRepository};
use crate::infrastructure::repository::tag_repo::TagRepository;

/// 正文预览的字符上限（与界面列表保持一致）。
const BODY_PREVIEW_CHARS: usize = 500;
/// 新建条目时预览的字符上限。
const NEW_ENTRY_PREVIEW_CHARS: usize = 200;

/// 截断为预览文本，超出部分加后缀。按**字符**而非字节计数，避免把中文截成半个字。
pub fn truncate_chars_with_suffix(text: &str, max_chars: usize, suffix: &str) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let cut = text
        .char_indices()
        .nth(max_chars)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len());
    let mut out = String::with_capacity(cut + suffix.len());
    out.push_str(&text[..cut]);
    out.push_str(suffix);
    out
}

/// 编辑正文时使用的预览。
pub fn body_preview(content: &str) -> String {
    truncate_chars_with_suffix(content, BODY_PREVIEW_CHARS, "...")
}

/// 新建条目时使用的预览。
pub fn new_entry_preview(content: &str) -> String {
    truncate_chars_with_suffix(content, NEW_ENTRY_PREVIEW_CHARS, "...")
}

/// 一次标签变更之后需要执行的加密动作。
///
/// `None` 表示敏感与否的判定没有翻转，不需要动密文；这正是"状态一致"的关键：
/// 反复给同一条打/去同一个非敏感标签不会反复入队加解密。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensitiveTransition {
    /// 敏感性未变化，无需加解密。
    None,
    /// 从非敏感变为敏感，需要加密。
    Encrypt,
    /// 从敏感变为非敏感，需要解密。
    Decrypt,
}

/// 写入某条条目的标签，并报告需要执行的加解密动作。
///
/// **只改数据库**：不含发事件、云同步与加解密执行，这些由调用方按各自场景补齐。
/// 旧敏感性从库里读（而不是从调用方传入），这样重入、并发或界面状态陈旧时判定
/// 依据始终是当前真实值。
pub fn apply_entry_tags(
    conn: &Arc<Mutex<Connection>>,
    tag_repo: &impl TagRepository,
    id: i64,
    tags: Vec<String>,
) -> Result<SensitiveTransition, String> {
    let old_sensitive = {
        let guard = conn.lock().map_err(|e| e.to_string())?;
        let tags_json: Option<String> = guard
            .query_row(
                "SELECT tags FROM clipboard_history WHERE id = ?",
                [id],
                |row| row.get(0),
            )
            .ok();
        let prev_tags: Vec<String> = tags_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
            .unwrap_or_default();
        has_sensitive_tag(&prev_tags)
    };

    let new_sensitive = has_sensitive_tag(&tags);
    tag_repo.update_entry_tags(id, tags)?;

    Ok(match (old_sensitive, new_sensitive) {
        (false, true) => SensitiveTransition::Encrypt,
        (true, false) => SensitiveTransition::Decrypt,
        _ => SensitiveTransition::None,
    })
}

/// 重命名一个标签（全库范围 + 关联表）。
pub fn apply_tag_rename(
    tag_repo: &impl TagRepository,
    old_name: &str,
    new_name: &str,
) -> Result<(), String> {
    tag_repo.rename(old_name, new_name)
}

/// 删除一个标签分组：**只解关联，不删除带该标签的条目**。
pub fn apply_tag_delete(
    tag_repo: &impl TagRepository,
    name: &str,
    data_dir: Option<&Path>,
) -> Result<(), String> {
    tag_repo.delete_globally(name, data_dir)
}

/// 新建一个标签名。
pub fn apply_tag_create(tag_repo: &impl TagRepository, name: &str) -> Result<(), String> {
    tag_repo.create(name)
}

/// 设置一个标签的颜色（`None` 表示清除颜色）。
pub fn apply_tag_color(
    tag_repo: &impl TagRepository,
    name: &str,
    color: Option<String>,
) -> Result<(), String> {
    tag_repo.set_color(name, color)
}

/// 设置条目正文。
///
/// 仓储层会拒绝二进制类型（`image`/`file`/`video` 的 `content` 是路径或 data URL），
/// 这是有意为之：改写这类行的正文会让 `content_hash` 与实际载荷不一致。调用方
/// 必须把这个错误如实返回给用户 / AI，而不是绕过。
pub fn apply_entry_content(
    repo: &impl ClipboardRepository,
    id: i64,
    content: &str,
) -> Result<(), String> {
    let preview = body_preview(content);
    repo.update_entry_content(id, content, preview.as_str())
}


/// 设置条目备注。对所有内容类型都可用（备注不参与内容哈希）。
pub fn apply_entry_note(
    repo: &impl ClipboardRepository,
    id: i64,
    note: &str,
) -> Result<(), String> {
    let normalized = normalize_note(note);
    repo.update_entry_note(id, normalized.as_str())
}

/// 设置条目置顶状态。
pub fn apply_entry_pin(
    conn: &Arc<Mutex<Connection>>,
    repo: &crate::infrastructure::repository::clipboard_repo::SqliteClipboardRepository,
    id: i64,
    is_pinned: bool,
) -> Result<(), String> {
    let guard = conn.lock().map_err(|e| e.to_string())?;
    repo.toggle_pin_with_conn(&guard, id, is_pinned)
}

/// 删除一个条目（含附件清理）。
pub fn apply_entry_delete(
    repo: &impl ClipboardRepository,
    id: i64,
    data_dir: Option<&Path>,
) -> Result<(), String> {
    repo.delete(id, data_dir)
}

/// 清空历史。
pub fn apply_history_clear(
    repo: &impl ClipboardRepository,
    data_dir: Option<&Path>,
) -> Result<(), String> {
    repo.clear(data_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_below_the_limit_is_untouched() {
        assert_eq!(body_preview("short"), "short");
        assert_eq!(new_entry_preview("short"), "short");
    }

    #[test]
    fn preview_truncates_on_char_boundaries_for_cjk() {
        let text = "中".repeat(600);
        let preview = body_preview(&text);
        // 500 个汉字 + "..."，而不是 500 字节。
        assert_eq!(preview.chars().count(), 503);
        assert!(preview.ends_with("..."));
    }

    #[test]
    fn new_entry_preview_uses_the_shorter_budget() {
        let text = "a".repeat(300);
        assert_eq!(new_entry_preview(&text).chars().count(), 203);
        assert_eq!(body_preview(&text), text);
    }

    #[test]
    fn transition_none_when_sensitivity_is_unchanged() {
        // 纯函数层面的判定：`None` 必须覆盖"都没敏感标签"和"都有敏感标签"两种情形。
        // 真实落库路径由 mcp 模块的等价性测试覆盖。
        assert_eq!(sensitivity_step(false, false), SensitiveTransition::None);
        assert_eq!(sensitivity_step(true, true), SensitiveTransition::None);
        assert_eq!(sensitivity_step(false, true), SensitiveTransition::Encrypt);
        assert_eq!(sensitivity_step(true, false), SensitiveTransition::Decrypt);
    }

    /// 与 `apply_entry_tags` 内的同一个判定，抽出来做纯逻辑断言。
    fn sensitivity_step(old: bool, new: bool) -> SensitiveTransition {
        match (old, new) {
            (false, true) => SensitiveTransition::Encrypt,
            (true, false) => SensitiveTransition::Decrypt,
            _ => SensitiveTransition::None,
        }
    }
}
