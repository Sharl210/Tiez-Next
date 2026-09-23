//! 把"数据库里记着的绝对路径"翻译成"数据目录内的相对路径"，并生成导入侧的还原计划。
//!
//! # 为什么需要这一层
//!
//! 备份包里存的是**数据目录的相对结构**（`attachments/a.png`），但数据库里存的是
//! **导出机器上的绝对路径**（`C:\Users\x\AppData\Roaming\com.tieznext\attachments\a.png`）。
//! 导入到另一台机器（或另一个数据目录）后，这些绝对路径全部失效。
//!
//! 因此导出时扫描两类引用、记录"原绝对路径 → 数据目录内相对路径"的映射：
//!
//! - `clipboard_history.content`（图片条目）与 `html_content`（富文本内嵌图片）
//! - `settings.app.custom_background`、`settings.app.emoji_favorites`
//!
//! 导入时按同一张表把路径改写成**当前**数据目录下的绝对路径。这张表是 `mappings.json`
//! ——与 `background/` 一样属于"新增附加条目"，旧版读取端跳过它，不会因此报错。

use super::format::{safe_relative_path, sha256_bytes};
use crate::error::AppResult;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn default_map_version() -> u32 {
    1
}

/// 路径映射表的固定 schema 版本，独立于主 `format_version` 演进。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathMappings {
    #[serde(default = "default_map_version")]
    pub map_version: u32,
    /// 键=数据目录内的相对路径（与 zip 条目同名）；值=导出时的绝对路径。
    ///
    /// 选择"相对路径为键"是因为它是跨机器稳定的那一侧：导入端知道自己的数据目录，
    /// 拿相对路径拼出新的绝对路径即可。
    #[serde(default)]
    pub items: BTreeMap<String, String>,
}

impl Default for PathMappings {
    fn default() -> Self {
        Self {
            map_version: 1,
            items: BTreeMap::new(),
        }
    }
}

/// 一个字符串是否是"整条就是一个文件路径值"（而非包含路径的普通文本）。
///
/// 与 `looks_like_absolute_file_path` 的区别：这里**不允许**内部换行/标签，
/// 且去掉引号后仍须是绝对路径。调用方据此决定"能不能整条替换剪贴板正文"——
/// 这是防止把用户复制的脚本/JSON/日志当成路径改写掉的关键判据。
pub fn looks_like_path_value(v: &str) -> bool {
    let t = v.trim().trim_matches('"');
    if t.is_empty() || t.len() > 4096 {
        return false;
    }
    if t.contains('<') || t.contains('\n') || t.contains('\r') {
        return false;
    }
    let bytes = t.as_bytes();
    let win = bytes.len() > 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    win || t.starts_with('/') || t.starts_with("\\\\")
}

/// 判断一个字符串像不像"数据目录里的文件路径"。
///
/// 只做**保守**的判定：不含 `data:` 前缀（那是内联 data URL）、不含换行/HTML 标签，
/// 且看上去是一个绝对路径。宁可漏改写（用户看到图片丢失，可重新设置），也不误改
/// 普通文本内容（那会静默损坏剪贴板历史）。
fn looks_like_absolute_file_path(value: &str) -> bool {
    let v = value.trim();
    if v.is_empty() || v.len() > 4096 {
        return false;
    }
    // 内联图片不落盘，无需改写。
    if v.starts_with("data:") {
        return false;
    }
    // HTML / 多行文本不是路径。
    if v.contains('<') || v.contains('\n') || v.contains('\r') {
        return false;
    }
    // Windows 盘符路径，或 POSIX 绝对路径。
    let bytes = v.as_bytes();
    let windows_drive = bytes.len() > 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    windows_drive || v.starts_with('/') || v.starts_with("\\\\")
}

/// 把绝对路径折算成"数据目录内的相对路径"。
///
/// 只接受**确实位于数据目录之下**的路径；数据目录之外的引用（例如用户把背景图放在
/// 桌面）返回 `None`——这类引用由导出层用 [`extra_file_plan`] 单独打包处理。
pub fn relative_within(base: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(base).ok()?;
    let s = rel.to_string_lossy().replace('\\', "/");
    safe_relative_path(&s)
}

/// 一个"必须随包带走"的背景图文件。
///
/// 它覆盖三种来源，统一都落到包内 `background/` 前缀下——单一落点是"往返闭合"的前提：
/// 导出端与导入端对"背景图在包里长什么样"只有一种约定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtraFile {
    /// 源文件绝对路径（用于读取字节；也用于导入时展示原位置）。
    pub original: PathBuf,
    /// 包内条目名，形如 `background/<名字>`。
    pub entry: String,
    /// 还原时使用的文件名。
    pub file_name: String,
    /// 该条目是否**已经**由 `background/` 目录递归写入过。
    ///
    /// 为真时只登记进 `background_map.json`、不重复写 zip 条目（避免同一份字节进包两次）。
    pub already_packed: bool,
}

/// 为一张背景图规划包内条目。
///
/// 条目名用**内容哈希**而不是原文件名：既避免路径里的非法字符，也让同一张图重复
/// 导出时自然去重。扩展名保留，方便用户从包里直接辨认。
///
/// 若该文件恰好位于 `data_dir/background/` 之内，说明它会被目录递归原样写进
/// `background/<相对路径>`——此时返回 `already_packed = true`，只登记映射、不重复写，
/// 条目名与递归产生的那一条保持一致（这是"往返闭合"的关键：导入端与导出端对
/// 同一个文件必须给出同一个条目名，否则会以为背景丢了）。
pub fn extra_file_plan(original: &Path, bytes: &[u8], data_dir: Option<&Path>) -> ExtraFile {
    let file_name = original
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "background.img".to_string());

    // 已在 data_dir/background/ 内 -> 递归会原样写入，条目名用相对路径。
    if let Some(data_dir) = data_dir {
        if let Some(rel) = relative_within(&data_dir.join("background"), original) {
            return ExtraFile {
                original: original.to_path_buf(),
                entry: format!("background/{}", rel),
                file_name,
                already_packed: true,
            };
        }
    }

    let ext = original
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty() && e.len() <= 8 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("bin")
        .to_ascii_lowercase();
    let digest = sha256_bytes(bytes);
    let hex = digest.trim_start_matches("sha256:");
    ExtraFile {
        original: original.to_path_buf(),
        entry: format!("background/{}.{}", hex, ext),
        file_name,
        already_packed: false,
    }
}

/// 收集数据库里所有指向数据目录内的绝对路径引用（附件 / 表情收藏 / 自定义背景）。
///
/// 导出时用它生成 [`PathMappings`]；同一个函数也被导入侧的回归测试复用来核对
/// "改写后是否还剩旧机器路径"。
pub fn collect_local_references(
    conn: &Connection,
    data_dir: &Path,
) -> AppResult<PathMappings> {
    let mut mappings = PathMappings::default();

    // ---- 附件：clipboard_history.content 与 html_content ----
    {
        let mut stmt = conn.prepare(
            "SELECT content, html_content FROM clipboard_history \
             WHERE content_type IN ('image', 'file', 'video') OR html_content IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |row| {
            let content: String = row.get(0)?;
            let html: Option<String> = row.get(1)?;
            Ok((content, html))
        })?;
        for row in rows {
            let (content, html) = row?;
            record_if_inside(&mut mappings, data_dir, &content);
            if let Some(html) = html {
                for candidate in extract_path_like_tokens(&html) {
                    record_if_inside(&mut mappings, data_dir, &candidate);
                }
            }
        }
    }

    // ---- 表情收藏（设置项那份，存的是绝对路径 JSON 数组）----
    if let Some(raw) = read_setting(conn, "app.emoji_favorites")? {
        if let Ok(paths) = serde_json::from_str::<Vec<String>>(&raw) {
            for p in paths {
                record_if_inside(&mut mappings, data_dir, &p);
            }
        }
    }

    // ---- 自定义背景 ----
    if let Some(raw) = read_setting(conn, "app.custom_background")? {
        record_if_inside(&mut mappings, data_dir, raw.trim());
    }

    Ok(mappings)
}

fn read_setting(conn: &Connection, key: &str) -> AppResult<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
            r.get::<_, String>(0)
        })
        .optional()?)
}

fn record_if_inside(mappings: &mut PathMappings, data_dir: &Path, value: &str) {
    if !looks_like_absolute_file_path(value) {
        return;
    }
    let path = PathBuf::from(value.trim());
    if let Some(rel) = relative_within(data_dir, &path) {
        mappings.items.insert(rel, value.trim().to_string());
    }
}

/// 从 HTML 里抽出"看起来像数据目录内文件路径"的候选串。
///
/// 抽的是 `src="..."` / `href="..."` 与裸路径。**不做** HTML 解析（不引入新依赖），
/// 只按引号切分；候选串最终还要通过 [`looks_like_absolute_file_path`] 与
/// [`relative_within`] 两道过滤，因此误抽的代价很低。
fn extract_path_like_tokens(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    for quote in ['"', '\''] {
        for part in html.split(quote) {
            if looks_like_absolute_file_path(part) {
                out.push(part.trim().to_string());
            }
        }
    }
    out
}

/// 把一段文本/HTML 里的旧绝对路径前缀替换成新的数据目录前缀。
///
/// 返回 `Some(new)` 表示确实发生了改写；`None` 表示无需改动。
pub fn rewrite_prefix(value: &str, old_base: &Path, new_base: &Path) -> Option<String> {
    let old_fwd = old_base.to_string_lossy().replace('\\', "/");
    let new_fwd = new_base.to_string_lossy().replace('\\', "/");
    let old_native = old_base.to_string_lossy().to_string();
    let new_native = new_base.to_string_lossy().to_string();

    let mut next = value.to_string();
    if old_fwd != new_fwd {
        next = next.replace(&old_fwd, &new_fwd);
    }
    if old_native != new_native {
        next = next.replace(&old_native, &new_native);
    }
    if next == value {
        None
    } else {
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tiez-pathmap-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn only_paths_inside_data_dir_are_mapped() {
        let base = Path::new("/data/app");
        assert_eq!(
            relative_within(base, &PathBuf::from("/data/app/attachments/a.png")).as_deref(),
            Some("attachments/a.png")
        );
        // 数据目录之外 -> 不映射（交给 background/ 附加条目处理）
        assert_eq!(relative_within(base, &PathBuf::from("/home/u/Desktop/bg.png")), None);
        // 数据目录本身 -> 空，不接受
        assert_eq!(relative_within(base, base), None);
    }

    #[test]
    fn path_like_detection_is_conservative() {
        assert!(looks_like_absolute_file_path("C:\\Users\\x\\a.png"));
        assert!(looks_like_absolute_file_path("/home/u/a.png"));
        assert!(!looks_like_absolute_file_path("data:image/png;base64,AAAA"));
        assert!(!looks_like_absolute_file_path("hello world"));
        assert!(!looks_like_absolute_file_path("<img src=\"x\">"));
        assert!(!looks_like_absolute_file_path("line1\nline2"));
        assert!(!looks_like_absolute_file_path("   "));
    }

    #[test]
    fn collects_references_and_rewrites_prefix() {
        let root = tmp_dir("collect");
        let data = root.join("com.tieznext");
        std::fs::create_dir_all(data.join("attachments")).unwrap();
        let db = root.join("t.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE clipboard_history (
                id INTEGER PRIMARY KEY, content_type TEXT NOT NULL, content TEXT NOT NULL,
                html_content TEXT, is_external INTEGER DEFAULT 0);",
        )
        .unwrap();

        let img = data.join("attachments").join("a.png");
        let bg = root.join("pictures").join("bg.jpg");
        std::fs::create_dir_all(bg.parent().unwrap()).unwrap();

        conn.execute(
            "INSERT INTO clipboard_history (content_type, content) VALUES ('image', ?1)",
            [img.to_string_lossy().to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES ('app.custom_background', ?1)",
            [bg.to_string_lossy().to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES ('app.emoji_favorites', ?1)",
            [serde_json::to_string(&vec![img.to_string_lossy().to_string()]).unwrap()],
        )
        .unwrap();

        let m = collect_local_references(&conn, &data).unwrap();
        assert!(m.items.contains_key("attachments/a.png"));
        // 数据目录外的背景图**不**进映射表
        assert!(!m.items.values().any(|v| v.ends_with("bg.jpg")));

        // 前缀改写：把整个数据目录前缀换成新目录后，旧前缀必须一个不剩。
        let new_data = root.join("newdata");
        let rewritten = rewrite_prefix(
            &format!("{} and {}", img.to_string_lossy(), bg.to_string_lossy()),
            &data,
            &new_data,
        )
        .unwrap();
        assert!(
            rewritten.contains(&new_data.join("attachments").join("a.png").to_string_lossy().to_string()),
            "改写后的文本必须包含新目录下的同一相对路径，实际={}",
            rewritten
        );
        assert!(
            !rewritten.contains(&data.to_string_lossy().to_string()),
            "旧数据目录前缀必须被完全替换掉，实际={}",
            rewritten
        );
        // 数据目录之外的文件不属于改写范围，应原样保留
        assert!(rewritten.contains(&bg.to_string_lossy().to_string()));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rewrite_prefix_returns_none_when_nothing_changes() {
        let base = Path::new("/data/app");
        assert!(rewrite_prefix("no paths here", base, Path::new("/other")).is_none());
        assert!(rewrite_prefix("x", base, base).is_none());
    }

    #[test]
    fn extra_file_entry_is_content_addressed() {
        let a = extra_file_plan(Path::new("/home/u/Desktop/My BG.JPEG"), b"abc", None);
        let b = extra_file_plan(Path::new("/tmp/other/Other.JPEG"), b"abc", None);
        // 相同内容 + 相同扩展名 -> 相同条目（路径不同也能自然去重）
        assert_eq!(a.entry, b.entry);
        assert!(a.entry.starts_with("background/"));
        // 扩展名统一小写，避免 `A.PNG` / `a.png` 被当成两个条目
        assert!(a.entry.ends_with(".jpeg"));
        // 原始文件名按用户看到的样子保留，便于从包里辨认
        assert_eq!(a.file_name, "My BG.JPEG");
        // 不同内容 -> 不同条目
        assert_ne!(a.entry, extra_file_plan(Path::new("/t/x.jpg"), b"zzz", None).entry);
        // 无扩展名时退化为 .bin，且不 panic
        let noext = extra_file_plan(Path::new("/t/noext"), b"abc", None);
        assert!(noext.entry.ends_with(".bin"), "实际={}", noext.entry);
    }
}
