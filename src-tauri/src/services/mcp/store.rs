//! MCP 服务的数据访问面：一份 schema、一份句柄、若干只读/写入原语。
//!
//! # 为什么要单独一层
//!
//! MCP 的每个工具最终都落到"读一条 / 搜一批 / 改一条"，这些原语如果散在工具
//! 分发里，测试就只能靠真起一个 Tauri 应用来覆盖。把这些原语收敛到这里，测试
//! 可以直接用内存库驱动**同一份代码**，证据强度与线上一致。
//!
//! # 与既有模块的关系
//!
//! * 读取一律走仓储层（`ClipboardRepository` / `TagRepository`），因此拿到的是
//!   **解密后的完整正文**，不是界面命令那种为性能截断过的预览。
//! * 写入一律走 [`crate::services::clipboard_mutation`]，与界面命令共用同一份判定。

use crate::database::DbState;
use crate::infrastructure::repository::clipboard_repo::{
    ClipboardRepository, SqliteClipboardRepository,
};
use crate::infrastructure::repository::settings_repo::{SettingsRepository, SqliteSettingsRepository};
use crate::infrastructure::repository::tag_repo::{SqliteTagRepository, TagRepository};
use crate::domain::models::ClipboardEntry;
use rusqlite::Connection;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// MCP 自用的最小 schema。
///
/// 刻意**不**包含附件表、表情表、迁移表等与本任务无关的对象：MCP 只碰剪贴板条目
/// 与标签，窄 schema 让测试与线上共用同一段建表代码，且不会被无关迁移拖住。
/// 字段集合与生产库中本模块实际读写的列一一对应，包含删除路径会写到的
/// `cloud_sync_tombstones`（少了它，删除在测试里会静默丢墓碑而在线上写成功）。
pub const MCP_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS clipboard_history (
    id INTEGER PRIMARY KEY,
    content_type TEXT NOT NULL,
    content TEXT NOT NULL,
    html_content TEXT,
    source_app TEXT NOT NULL,
    source_app_path TEXT,
    timestamp INTEGER NOT NULL,
    preview TEXT NOT NULL,
    is_pinned INTEGER NOT NULL DEFAULT 0,
    content_hash INTEGER NOT NULL DEFAULT 0,
    tags TEXT NOT NULL DEFAULT '[]',
    use_count INTEGER NOT NULL DEFAULT 0,
    is_external INTEGER NOT NULL DEFAULT 0,
    pinned_order INTEGER NOT NULL DEFAULT 0,
    note TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS entry_tags (
    entry_id INTEGER NOT NULL,
    tag TEXT NOT NULL,
    PRIMARY KEY (entry_id, tag)
);
CREATE TABLE IF NOT EXISTS saved_tags (
    name TEXT PRIMARY KEY,
    color TEXT
);
CREATE TABLE IF NOT EXISTS cloud_sync_tombstones (
    content_type TEXT NOT NULL,
    content_hash INTEGER NOT NULL,
    deleted_at INTEGER NOT NULL,
    PRIMARY KEY (content_type, content_hash)
);
CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

/// MCP 运行时的数据访问句柄。
///
/// `conn` 与仓储共享同一个连接句柄（`Arc<Mutex<Connection>>`），这样一次工具调用
/// 内的"读—改—再读"看到的是同一份数据，不需要引入事务或快照。
pub struct McpStore {
    pub conn: Arc<Mutex<Connection>>,
    pub repo: SqliteClipboardRepository,
    pub tag_repo: SqliteTagRepository,
    pub settings_repo: SqliteSettingsRepository,
}

/// 手工实现 `Clone`：三个仓储本身不实现 `Clone`，但它们都只是 `Arc<Mutex<Connection>>`
/// 的薄包装，重建一个等价实例是零成本且语义相同的（共享同一连接）。
impl Clone for McpStore {
    fn clone(&self) -> Self {
        Self::new(self.conn.clone())
    }
}

impl McpStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self {
            repo: SqliteClipboardRepository::new(conn.clone()),
            tag_repo: SqliteTagRepository::new(conn.clone()),
            settings_repo: SqliteSettingsRepository::new(conn.clone()),
            conn,
        }
    }

    /// 从正在运行的 Tauri 应用状态里取句柄。
    pub fn from_state(state: &DbState) -> Self {
        Self {
            conn: state.conn.clone(),
            repo: SqliteClipboardRepository::new(state.conn.clone()),
            tag_repo: SqliteTagRepository::new(state.conn.clone()),
            settings_repo: SqliteSettingsRepository::new(state.conn.clone()),
        }
    }

    /// 测试用：内存库 + [`MCP_SCHEMA`]。
    pub fn in_memory() -> Self {
        let conn = Connection::open_in_memory().expect("内存数据库应可创建");
        conn.execute_batch(MCP_SCHEMA)
            .expect("MCP schema 应可建表");
        Self::new(Arc::new(Mutex::new(conn)))
    }

    /// 把外部连接接进来，并按 [`MCP_SCHEMA`] 补齐缺的表。
    ///
    /// 生产库由迁移链建表，不会因此改变；测试用它把真实文件库接进同一套代码。
    pub fn attach(conn: Arc<Mutex<Connection>>) -> Self {
        if let Ok(guard) = conn.lock() {
            let _ = guard.execute_batch(MCP_SCHEMA);
        }
        Self::new(conn)
    }

    pub fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64
    }

    /// 一个会话态条目落库：新条目、置顶、改标签都要先走这一步。
    ///
    /// 会话态条目的 id 是负数（内存里的临时编号），库里不可能有这一行；`repo.save`
    /// 在 `id == 0` 时才会分配新 id，因此这里**必须先把 id 归零**再落库，否则
    /// SQLite 会把负 id 当成显式主键写进去——那一行的 id 会一直是负数，看起来像
    /// "归档失败"，而且下一次负 id 分配很容易和它撞号。
    ///
    /// 返回库里真正分配的 id；调用方负责把这个新 id 回写到会话态（与界面
    /// `toggle_clipboard_pin` / `update_tags` 的"id 就地改写"同语义）。
    pub fn persist_session_entry(
        &self,
        entry: &ClipboardEntry,
        data_dir: Option<&std::path::Path>,
    ) -> Result<i64, String> {
        let mut owned = entry.clone();
        owned.id = 0;
        self.repo.save(&owned, data_dir)
    }

    /// 把一个**会话态**条目的状态写回内存（`mutate` 返回值表示是否找到该 id）。
    ///
    /// 与 [`Self::persist_session_entry`] 配对使用：先落库拿真实 id，再改写会话态，
    /// 顺序与界面命令一致（界面注释里明确写了"反序会导致内存与数据库不一致"）。
    /// 本函数只做内存改写，落库由调用方先完成。
    ///
    /// 返回改写后的会话态条目（供调用方回读校验），找不到该 id 时返回 `None`。
    pub fn rewrite_session_entry<F>(
        &self,
        snapshot: &mut [ClipboardEntry],
        id: i64,
        mutate: F,
    ) -> Option<ClipboardEntry>
    where
        F: FnOnce(&mut ClipboardEntry),
    {
        let item = snapshot.iter_mut().find(|e| e.id == id)?;
        mutate(item);
        Some(item.clone())
    }

    /// 在条目集合里按 id 找一条（`None` 表示不在这一层）。
    pub fn find_in(entries: &[ClipboardEntry], id: i64) -> Option<ClipboardEntry> {
        entries.iter().find(|e| e.id == id).cloned()
    }

    // -----------------------------------------------------------------------
    // 表情收藏清单（设置项 `app.emoji_favorites`）
    // -----------------------------------------------------------------------
    //
    // 界面渲染的收藏列表来自这个设置项里的一个 **JSON 字符串数组**（见
    // `EmojiPanel.tsx`：`favorites` 就是解析它得来的）。因此"把图片放进
    // `emoji_favorites/` 目录"只是完成了一半——不更新这个设置项，用户切回界面
    // 看不到刚加的表情；只更新设置项不写文件，用户看到的是一个点不开的路径。

    /// 读取表情清单里的路径（解析失败按空处理，不报错）。
    pub fn emoji_manifest(&self) -> Vec<String> {
        self.settings_repo
            .get("app.emoji_favorites")
            .ok()
            .flatten()
            .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
            .unwrap_or_default()
    }

    fn write_emoji_manifest(&self, paths: &[String]) -> Result<(), String> {
        let raw = serde_json::to_string(paths).map_err(|e| e.to_string())?;
        self.settings_repo
            .set("app.emoji_favorites", &raw)
            .map_err(|e| e.to_string())
    }

    /// 把一个路径加入表情清单（已存在则不重复添加）。
    pub fn add_to_emoji_manifest(&self, path: &str) -> Result<(), String> {
        let mut paths = self.emoji_manifest();
        if paths.iter().any(|p| p == path) {
            return Ok(());
        }
        paths.push(path.to_string());
        self.write_emoji_manifest(&paths)
    }

    /// 从表情清单移除一个路径，返回是否真的改动过。
    ///
    /// 比较**不区分大小写**：Windows 路径大小写不敏感，界面上传进来的字符串与设置项
    /// 里存的可能是同一个路径的大小写变体；只按全等比较会留下一条删不掉的幽灵项。
    pub fn remove_from_emoji_manifest(&self, path: &str) -> Result<bool, String> {
        let mut paths = self.emoji_manifest();
        let before = paths.len();
        paths.retain(|p| !p.eq_ignore_ascii_case(path));
        if paths.len() == before {
            return Ok(false);
        }
        self.write_emoji_manifest(&paths)?;
        Ok(true)
    }

    /// 读一条完整条目（含解密后的正文、HTML、备注、标签）。
    pub fn entry(&self, id: i64) -> Result<Option<ClipboardEntry>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        self.repo.get_entry_by_id_with_conn(&conn, id)
    }

    /// 读一条**完整正文**（不截断）。
    ///
    /// 与界面命令 `get_clipboard_history` 的区别就在这里：那条路径为了 UI 性能把
    /// 正文截到 2000 字符，AI 拿它当数据源会静默丢内容。
    pub fn full_content(&self, id: i64) -> Result<Option<(String, String, Option<String>)>, String> {
        self.repo.get_entry_content_with_html(id)
    }

    /// 分页读历史（正文同样不截断）。
    pub fn history(
        &self,
        limit: i32,
        offset: i32,
        content_type: Option<&str>,
    ) -> Result<Vec<ClipboardEntry>, String> {
        self.repo.get_history(limit, offset, content_type)
    }

    /// 搜索（正文与 `search_clipboard_history` 命令同源，但这里不再二次截断）。
    pub fn search(&self, query: &str, limit: i32, tag_only: bool) -> Result<Vec<ClipboardEntry>, String> {
        self.repo.search(query, limit, tag_only)
    }

    pub fn by_tag(&self, tag: &str) -> Result<Vec<ClipboardEntry>, String> {
        self.tag_repo.get_entries_by_tag(tag)
    }

    pub fn tags(&self) -> Result<std::collections::HashMap<String, i32>, String> {
        self.tag_repo.get_all_with_counts()
    }

    pub fn tag_colors(&self) -> Result<std::collections::HashMap<String, String>, String> {
        self.tag_repo.get_colors()
    }

    pub fn count(&self) -> Result<i64, String> {
        self.repo.get_count()
    }

    /// 新建条目并返回新 id。
    pub fn create_entry(
        &self,
        content: String,
        content_type: String,
        tags: Vec<String>,
        note: String,
        source_app: String,
        data_dir: Option<&std::path::Path>,
    ) -> Result<i64, String> {
        let entry = ClipboardEntry {
            id: 0,
            content_type,
            content,
            html_content: None,
            source_app,
            source_app_path: None,
            timestamp: self.now_ms(),
            preview: crate::services::clipboard_mutation::new_entry_preview(""),
            is_pinned: false,
            tags,
            use_count: 0,
            is_external: false,
            pinned_order: 0,
            note,
            file_preview_exists: true,
        };
        let mut entry = entry;
        entry.preview = crate::services::clipboard_mutation::new_entry_preview(&entry.content);
        self.repo.save(&entry, data_dir)
    }

    /// 按 id 列表批量取条目；保持**输入顺序**，缺失的 id 会被记录在第二个返回值里。
    ///
    /// 显式按 id 取是"完整内容"的关键：不需要靠 SQL 通配去猜某条是否命中，也就
    /// 不会出现"搜索命中了但正文被截断"的情况。
    pub fn entries_by_ids(&self, ids: &[i64]) -> (Vec<ClipboardEntry>, Vec<i64>) {
        let mut found = Vec::new();
        let mut missing = Vec::new();
        for id in ids {
            match self.entry(*id) {
                Ok(Some(entry)) => found.push(entry),
                _ => missing.push(*id),
            }
        }
        (found, missing)
    }

    /// 分页与筛选用的"无副作用"搜索：先按条件拿 id，再按 id 取完整条目。
    ///
    /// 为什么两步走：仓储层的 `search` 会把整个库（含加密条目）扫一遍，返回条数由
    /// `limit` 控制。MCP 需要的是"完整正文的、可翻页的、可组合筛选的结果"，两步
    /// 走既复用了既有解密逻辑，又能保证返回的正文不受任何展示层限幅影响。
    pub fn search_paged(
        &self,
        query: Option<&str>,
        tag: Option<&str>,
        content_type: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<ClipboardEntry>, bool, Vec<i64>), String> {
        // 候选集：不同筛选条件走不同的既有查询，然后统一按 id 去重保序。
        let mut candidates: Vec<ClipboardEntry> = match (query, tag) {
            (Some(q), _) if !q.trim().is_empty() => {
                // 取足够多的候选，再在下面按 offset/limit 切片。
                let raw = self.search(q, (limit + offset + 1).min(10_000) as i32, false)?;
                raw
            }
            (_, Some(t)) => self.by_tag(t)?,
            _ => self.history((limit + offset + 1).min(10_000) as i32, 0, content_type)?,
        };

        if let Some(ct) = content_type {
            candidates.retain(|e| e.content_type == ct);
        }
        // 标签过滤与关键词过滤叠加时，`search` 已覆盖标签名匹配；显式按 tag 参数
        // 再筛一遍，保证 `tag: "x"` 的语义是"确实带这个标签"。
        if let Some(t) = tag {
            if query.map(|q| !q.trim().is_empty()).unwrap_or(false) {
                let needle = t.to_lowercase();
                candidates.retain(|e| e.tags.iter().any(|x| x.to_lowercase() == needle));
            }
        }

        // 稳定排序，保证翻页不重不漏。
        candidates.sort_by(|a, b| {
            b.is_pinned
                .cmp(&a.is_pinned)
                .then_with(|| b.pinned_order.cmp(&a.pinned_order))
                .then_with(|| b.timestamp.cmp(&a.timestamp))
                .then_with(|| b.id.cmp(&a.id))
        });

        let total_before_slice = candidates.len();
        let has_more = total_before_slice > offset + limit;
        let page: Vec<ClipboardEntry> = candidates.into_iter().skip(offset).take(limit).collect();
        let ids: Vec<i64> = page.iter().map(|e| e.id).collect();
        Ok((page, has_more, ids))
    }
}

impl McpStore {
    /// 读取一个配置项。
    pub fn setting(&self, key: &str) -> Option<String> {
        self.settings_repo.get(key).ok().flatten()
    }

    /// 写入一个配置项。
    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), String> {
        self.settings_repo
            .set(key, value)
            .map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------
// MCP 服务自身使用的配置键（与其它 settings 项同一张表，前缀区分归属）
// ---------------------------------------------------------------------------

/// 服务开关，默认 [`DEFAULT_ENABLED`]。
pub const KEY_ENABLED: &str = "mcp.enabled";
/// 写操作开关，默认 [`DEFAULT_ALLOW_WRITE`]。
pub const KEY_ALLOW_WRITE: &str = "mcp.allow_write";
/// 服务监听端口，默认 [`DEFAULT_PORT`]。
pub const KEY_PORT: &str = "mcp.port";
/// 启动时自动拉起服务，默认 [`DEFAULT_AUTOSTART`]。
pub const KEY_AUTOSTART: &str = "mcp.autostart";
/// 鉴权 token。免鉴权模式下它依然存在，用户随时可以打开校验而不必重新生成。
pub const KEY_TOKEN: &str = "mcp.token";
/// 是否**强制**校验 token，默认 [`DEFAULT_REQUIRE_TOKEN`]。
pub const KEY_REQUIRE_TOKEN: &str = "mcp.require_token";
/// 是否允许局域网访问，默认 [`DEFAULT_ALLOW_LAN`]。
pub const KEY_ALLOW_LAN: &str = "mcp.allow_lan";

// ---------------------------------------------------------------------------
// 默认值：出厂姿态的唯一定义处
// ---------------------------------------------------------------------------
//
// 默认姿态 = 开箱即用（服务开、可写、免鉴权、固定端口），但**只监听回环**。
// 免鉴权只有在"外部机器根本连不上"时才成立，所以 `DEFAULT_ALLOW_LAN = false`
// 是这一组默认值里唯一不能松的一项：用户要暴露到局域网，必须自己显式打开。
//
// 这些常量同时被 `mod.rs` 的读取函数与测试引用——改一个数字，对应的断言就会红。

/// 服务默认开启。
pub const DEFAULT_ENABLED: bool = true;
/// 默认允许 AI 修改（全权限）。
pub const DEFAULT_ALLOW_WRITE: bool = true;
/// 默认随应用一起启动，否则"默认打开"只是设置里的一行字，服务并不会真的跑起来。
pub const DEFAULT_AUTOSTART: bool = true;
/// 默认不校验令牌（免鉴权）。
pub const DEFAULT_REQUIRE_TOKEN: bool = false;
/// 默认仅本机访问（绑回环）。
pub const DEFAULT_ALLOW_LAN: bool = false;

/// 默认端口。选固定端口是为了让用户能把 `http://127.0.0.1:23123/mcp` 写进
/// MCP 客户端配置并长期有效；被占用时会自动向后试探（见 `bind_listener`）。
pub const DEFAULT_PORT: u16 = 23123;

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(store: &McpStore, content: &str, content_type: &str, tags: &[&str]) -> i64 {
        let id = store
            .create_entry(
                content.to_string(),
                content_type.to_string(),
                tags.iter().map(|s| s.to_string()).collect(),
                String::new(),
                "test".to_string(),
                None,
            )
            .unwrap();
        store
            .tag_repo
            .update_entry_tags(id, tags.iter().map(|s| s.to_string()).collect())
            .unwrap();
        id
    }

    #[test]
    fn in_memory_store_round_trips_an_entry() {
        let store = McpStore::in_memory();
        let id = seed(&store, "hello", "text", &["work"]);
        let entry = store.entry(id).unwrap().unwrap();
        assert_eq!(entry.content, "hello");
        assert_eq!(entry.tags, vec!["work".to_string()]);
    }

    #[test]
    fn schema_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MCP_SCHEMA).unwrap();
        conn.execute_batch(MCP_SCHEMA).unwrap();
    }

    #[test]
    fn search_paged_orders_pinned_first() {
        let store = McpStore::in_memory();
        let a = seed(&store, "alpha", "text", &[]);
        let b = seed(&store, "beta", "text", &[]);
        {
            let conn = store.conn.lock().unwrap();
            store.repo.toggle_pin_with_conn(&conn, b, true).unwrap();
        }
        let (page, _, ids) = store
            .search_paged(None, None, None, 10, 0)
            .expect("分页查询应成功");
        assert_eq!(ids.first(), Some(&b));
        assert_eq!(page.len(), 2);
        assert!(page.iter().any(|e| e.id == a));
    }

    #[test]
    fn search_paged_filters_by_content_type() {
        let store = McpStore::in_memory();
        seed(&store, "text one", "text", &[]);
        seed(&store, "code one", "code", &[]);
        let (page, _, _) = store
            .search_paged(None, None, Some("code"), 10, 0)
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].content_type, "code");
    }

    #[test]
    fn entries_by_ids_reports_missing_ids() {
        let store = McpStore::in_memory();
        let id = seed(&store, "present", "text", &[]);
        let (found, missing) = store.entries_by_ids(&[id, 999_999]);
        assert_eq!(found.len(), 1);
        assert_eq!(missing, vec![999_999]);
    }

    #[test]
    fn settings_default_to_none_before_being_written() {
        let store = McpStore::in_memory();
        // 库里没写过这一项时读回 `None`：调用方据此套用默认值，而不是把"未设置"
        // 当成"已设为 false"。
        assert!(store.setting(KEY_ENABLED).is_none());
        store.set_setting(KEY_ENABLED, "false").unwrap();
        assert_eq!(store.setting(KEY_ENABLED).as_deref(), Some("false"));
    }

    // -----------------------------------------------------------------------
    // 出厂默认姿态：逐项断言
    // -----------------------------------------------------------------------
    //
    // 存在的唯一目的是**防回退**：这组数字是用户明确要求的开箱姿态，任何人把手
    // 伸回"默认关 / 默认只读 / 默认强制令牌"的旧行为，这里立刻会红。
    // 与之配对的是 `mod.rs` 里的 `defaults_survive_a_fresh_database`，那条走真
    // 读函数（常量对但读取函数写死错值的情形也会被它抓住）。
    #[test]
    fn shipping_defaults_match_the_required_posture() {
        assert!(DEFAULT_ENABLED, "默认必须开启服务");
        assert!(DEFAULT_ALLOW_WRITE, "默认必须允许 AI 修改（全权限）");
        assert!(DEFAULT_AUTOSTART, "默认必须随应用自动启动，否则“默认打开”落不了地");
        assert!(!DEFAULT_REQUIRE_TOKEN, "默认必须免鉴权");
        assert!(
            !DEFAULT_ALLOW_LAN,
            "默认必须只监听本机：免鉴权 + 局域网暴露是危险的组合"
        );
        assert_eq!(DEFAULT_PORT, 23123, "默认端口是用户指定的 23123");
    }

    #[test]
    fn setting_keys_are_stable_strings() {
        // 键名是前端与用户既有数据库之间的契约，改字符串等于丢弃用户已存的设置。
        assert_eq!(KEY_ENABLED, "mcp.enabled");
        assert_eq!(KEY_ALLOW_WRITE, "mcp.allow_write");
        assert_eq!(KEY_PORT, "mcp.port");
        assert_eq!(KEY_AUTOSTART, "mcp.autostart");
        assert_eq!(KEY_TOKEN, "mcp.token");
        assert_eq!(KEY_REQUIRE_TOKEN, "mcp.require_token");
        assert_eq!(KEY_ALLOW_LAN, "mcp.allow_lan");
    }
}
