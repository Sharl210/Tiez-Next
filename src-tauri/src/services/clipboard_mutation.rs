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

/// 条目与标签之间一次"转移"的形态。
///
/// 条目与标签是**多对多**关系（`entry_tags` 的主键是 `(entry_id, tag)`，一个条目可以
/// 同时带多个标签），因此"从 A 移到 B"只能理解为"在集合里把 A 换成 B"，而不是"把这条
/// 从 A 组搬到 B 组"。若当成后者，一个带 `[工作, 待办]` 的条目移动 `工作 → 归档` 会
/// 顺手丢掉 `待办`——用户没有要求删掉的标签就此消失，且不可撤销。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagTransfer {
    /// 移动：源标签在集合内**原位替换**为目标标签，其余标签不动。
    Move,
    /// 复制：源标签保留，目标标签追加进集合（已存在则不重复）。
    Copy,
}

/// 计算一次标签转移之后的新标签集合。**纯函数**。
///
/// 规则：
/// * 先 trim；空串直接拒绝（`from` / `to` 任一为空都算调用错误，而不是"静默无效"）。
/// * 匹配大小写不敏感，与 `delete_globally` 的 `COLLATE NOCASE` 和
///   `has_sensitive_tag` 的大小写规则保持一致——库里同时存在 `Work` 与 `work` 时，
///   按 `work` 移动不能只搬走一半。**保留已有标签的原始拼写**。
/// * `Move` 原地替换（保持标签在界面条带中的位置），`Copy` 追加到末尾。
/// * 目标标签已存在时不产生重复项；`Move` 时 `from` 与 `to` 视为同一标签则集合不变。
///
/// 返回的集合已经去重，可直接交给 [`apply_entry_tags`]。
pub fn transferred_tags(
    current: &[String],
    from: &str,
    to: &str,
    kind: TagTransfer,
) -> Result<Vec<String>, String> {
    let from = from.trim();
    let to = to.trim();
    if from.is_empty() {
        return Err("源标签名不能为空".to_string());
    }
    if to.is_empty() {
        return Err("目标标签名不能为空".to_string());
    }

    let mut next: Vec<String> = current
        .iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();

    let same_needle = |t: &str, needle: &str| t.eq_ignore_ascii_case(needle);
    let already_has_to = next.iter().any(|t| same_needle(t, to));

    match kind {
        TagTransfer::Move => {
            if same_needle(from, to) {
                // 仅当源与目标忽略大小写相同、且集合里确实**只**记录了源那一份写法时，
                // 才算"移动到自身"。若集合里同时存在 `Work` 与 `work`，用户按任意一种
                // 写法做移动，意图都是"把这两份同义标签收成一份"——否则库里会永久留着
                // 两个界面看起来一模一样、分组却各自独立的标签。
                let spellings = next.iter().filter(|t| same_needle(t, from)).count();
                if spellings <= 1 {
                    return Ok(next);
                }
            }
            let mut replaced = false;
            for t in next.iter_mut() {
                if same_needle(t, from) {
                    *t = to.to_string();
                    replaced = true;
                    break;
                }
            }
            if !replaced {
                // 条目本来就不带源标签：语义仍是"结果里应有目标标签"，因此补上。
                if !already_has_to {
                    next.push(to.to_string());
                }
            }
            // 替换/追加之后可能和目标标签原有的另一份拼写撞车（`Work` + `work`）。
            dedupe_ignore_ascii_case(&mut next);
        }
        TagTransfer::Copy => {
            if !already_has_to {
                next.push(to.to_string());
            }
        }
    }

    Ok(next)
}

/// 大小写不敏感去重，保留**首次出现**的原始拼写。
fn dedupe_ignore_ascii_case(tags: &mut Vec<String>) {
    let mut seen: Vec<String> = Vec::with_capacity(tags.len());
    tags.retain(|t| {
        let key = t.to_ascii_lowercase();
        if seen.iter().any(|s| s.as_str() == key.as_str()) {
            false
        } else {
            seen.push(key);
            true
        }
    });
}

/// 读取一条条目当前的标签集合。
///
/// 从库里读而不是从调用方传入：界面状态、AI 的上下文与会话列表都可能是陈旧的，
/// 只有库里的值才是判定依据。条目不存在时明确报错，而不是返回空集合——空集合会被
/// 上层当成"这条没有标签"，于是一次针对错 id 的移动就静默成功了。
pub fn read_entry_tags(conn: &Arc<Mutex<Connection>>, id: i64) -> Result<Vec<String>, String> {
    let guard = conn.lock().map_err(|e| e.to_string())?;
    let row: Option<String> = guard
        .query_row(
            "SELECT tags FROM clipboard_history WHERE id = ?",
            [id],
            |row| row.get(0),
        )
        .ok();
    match row {
        None => Err(format!("条目 {} 不存在", id)),
        Some(tags_json) => Ok(serde_json::from_str::<Vec<String>>(&tags_json).unwrap_or_default()),
    }
}

/// 把一个条目从标签 `from` 移动/复制到标签 `to`，并报告需要执行的加解密动作。
///
/// **只改数据库**，与 [`apply_entry_tags`] 一样由调用方补齐宿主动作。实现上先在内存里
/// 算出目标集合（[`transferred_tags`]），再整份交给同一个写入内核，因此条目与标签的
/// 关联表、`clipboard_history.tags` 冗余列以及敏感性翻转判定**只有一份代码**：
/// 界面路径、MCP 路径不可能出现"一个维护了关联表、另一个只改了 JSON 列"的分叉。
pub fn apply_entry_tag_transfer(
    conn: &Arc<Mutex<Connection>>,
    tag_repo: &impl TagRepository,
    id: i64,
    from: &str,
    to: &str,
    kind: TagTransfer,
) -> Result<SensitiveTransition, String> {
    let current = read_entry_tags(conn, id)?;
    let next = transferred_tags(&current, from, to, kind)?;
    apply_entry_tags(conn, tag_repo, id, next)
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

    fn owned(tags: &[&str]) -> Vec<String> {
        tags.iter().map(|s| s.to_string()).collect()
    }

    // ------------------------------------------------------------------
    // 标签转移（移动 / 复制）的纯逻辑
    // ------------------------------------------------------------------

    #[test]
    fn move_replaces_only_the_source_tag() {
        // 这是移动语义的核心断言：C 是"其他标签"，必须原样留下。
        let next = transferred_tags(&owned(&["A", "C"]), "A", "B", TagTransfer::Move).unwrap();
        assert_eq!(next, owned(&["B", "C"]));
    }

    #[test]
    fn move_keeps_the_position_of_the_replaced_tag() {
        // 原位替换（而不是"删掉再追加"）：界面上标签条带的顺序不会跳。
        let next = transferred_tags(
            &owned(&["A", "C", "D"]),
            "C",
            "B",
            TagTransfer::Move,
        )
        .unwrap();
        assert_eq!(next, owned(&["A", "B", "D"]));
    }

    #[test]
    fn copy_keeps_the_source_and_appends_the_target() {
        let next = transferred_tags(&owned(&["A"]), "A", "B", TagTransfer::Copy).unwrap();
        assert_eq!(next, owned(&["A", "B"]));
    }

    #[test]
    fn copy_of_an_already_present_target_does_not_duplicate() {
        let next = transferred_tags(&owned(&["A", "B"]), "A", "B", TagTransfer::Copy).unwrap();
        assert_eq!(next, owned(&["A", "B"]));
    }

    #[test]
    fn transfer_matches_case_insensitively_but_preserves_spelling() {
        // 库里存的是 `Work`，用户从 `work` 移出：源必须被换掉，且目标按用户输入的写法落库。
        let moved = transferred_tags(&owned(&["Work", "x"]), "work", "Archive", TagTransfer::Move)
            .unwrap();
        assert_eq!(moved, owned(&["Archive", "x"]));

        // 复制时 `WORK` 已经在集合里，就不应再追加一份 `work`（否则界面出现两个同义标签）。
        let copied = transferred_tags(&owned(&["WORK"]), "WORK", "work", TagTransfer::Copy).unwrap();
        assert_eq!(copied, owned(&["WORK"]));
    }

    #[test]
    fn moving_a_missing_source_still_lands_on_the_target() {
        // "把这条移到 B" 的语义是"结果里它属于 B"。条目本来没带 A 不该让操作变成空转，
        // 否则用户以为移动成功了，界面却毫无变化。
        let next = transferred_tags(&owned(&["x"]), "A", "B", TagTransfer::Move).unwrap();
        assert_eq!(next, owned(&["x", "B"]));
    }

    #[test]
    fn moving_a_tag_onto_itself_is_a_no_op() {
        let next = transferred_tags(&owned(&["A", "C"]), "A", "A", TagTransfer::Move).unwrap();
        assert_eq!(next, owned(&["A", "C"]));
    }

    #[test]
    fn move_to_self_collapses_two_spellings_of_the_same_tag() {
        // `Work` 与 `work` 同时存在时，用户按任意一种写法做"移动到自身"，意图都是
        // 把这两份同义标签收成一份——否则库里永久留着两个界面看着一样、分组却各自
        // 独立的标签。这一条与上一条的区别只在"集合里是否真的有两份写法"。
        let next = transferred_tags(&owned(&["Work", "work"]), "Work", "work", TagTransfer::Move)
            .unwrap();
        assert_eq!(next.len(), 1, "同义标签必须收敛：{:?}", next);
        assert!(next[0].eq_ignore_ascii_case("work"));
    }

    #[test]
    fn blank_source_or_target_is_refused_not_silently_ignored() {
        assert!(transferred_tags(&owned(&["A"]), "   ", "B", TagTransfer::Move).is_err());
        assert!(transferred_tags(&owned(&["A"]), "A", "  ", TagTransfer::Copy).is_err());
    }

    #[test]
    fn transfer_trims_and_drops_blank_entries_from_the_existing_set() {
        let next = transferred_tags(
            &owned(&[" A ", "", "  "]),
            "A",
            " B ",
            TagTransfer::Move,
        )
        .unwrap();
        assert_eq!(next, owned(&["B"]));
    }
}
