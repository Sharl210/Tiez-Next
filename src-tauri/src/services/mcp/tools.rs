//! MCP 工具定义与调用实现。
//!
//! # 分层
//!
//! * [`catalog`]：工具清单（名字、说明、`inputSchema`、读写分类）。`tools/list`
//!   原样返回它，`tools/call` 用它做参数校验与权限判定。
//! * [`invoke`]：把一个已授权的调用映射到 [`McpStore`] 与
//!   [`crate::services::clipboard_mutation`]，返回结构化 JSON。
//!
//! 两个函数都不接触网络，因此协议正确性、权限与"不截断"都能用内存库直接断言。

use serde_json::{json, Map, Value};

use super::store::McpStore;
use crate::services::clipboard_mutation as mutation;
use crate::services::encryption_queue::{EncryptionAction, EncryptionJob};

/// 工具对数据的写意图。只读工具在只读模式下照常可用，写工具直接被拒绝。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// 只读：不改任何落盘状态。
    Read,
    /// 写入：改数据库或数据目录。
    Write,
}

/// 一个 MCP 工具的定义。
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub access: Access,
    /// 该工具是否需要 `confirm: true` 才执行（破坏性操作的最后一道闸）。
    pub destructive: bool,
    pub input_schema: Value,
}

/// 所有工具的定义。顺序即 `tools/list` 的返回顺序。
pub fn catalog() -> Vec<ToolSpec> {
    let obj = |props: Value, required: Vec<&str>| -> Value {
        json!({
            "type": "object",
            "properties": props,
            "required": required,
            "additionalProperties": false,
        })
    };
    let id_prop = json!({"type": "integer", "description": "剪贴板条目 id"});

    vec![
        ToolSpec {
            name: "list_entries",
            title: "列出剪贴板条目",
            description: "分页列出剪贴板条目，可按标签、内容类型过滤。返回的 content 为完整正文，不做任何截断。",
            access: Access::Read,
            destructive: false,
            input_schema: obj(
                json!({
                    "limit": {"type": "integer", "minimum": 1, "maximum": 500, "description": "本页条数，默认 50"},
                    "offset": {"type": "integer", "minimum": 0, "description": "偏移量，默认 0"},
                    "tag": {"type": "string", "description": "只返回带该标签的条目"},
                    "contentType": {"type": "string", "description": "只返回该内容类型（text/code/url/rich_text/image/file/video）"},
                    "includeContent": {"type": "boolean", "description": "是否返回完整正文，默认 true；置 false 时只返回元数据，适合大库概览"},
                }),
                vec![],
            ),
        },
        ToolSpec {
            name: "search_entries",
            title: "搜索剪贴板条目",
            description: "按关键词搜索剪贴板条目的正文、来源应用与标签。返回完整正文，不做截断。",
            access: Access::Read,
            destructive: false,
            input_schema: obj(
                json!({
                    "query": {"type": "string", "description": "搜索关键词"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 500, "description": "最多返回条数，默认 50"},
                    "offset": {"type": "integer", "minimum": 0, "description": "偏移量，默认 0"},
                    "tagOnly": {"type": "boolean", "description": "只在标签名中匹配，默认 false"},
                    "tag": {"type": "string", "description": "额外限定必须带该标签"},
                }),
                vec!["query"],
            ),
        },
        ToolSpec {
            name: "get_entry",
            title: "读取单条条目",
            description: "按 id 读取一条剪贴板条目，含完整正文、HTML、备注、标签与置顶状态。",
            access: Access::Read,
            destructive: false,
            input_schema: obj(json!({ "id": id_prop }), vec!["id"]),
        },
        ToolSpec {
            name: "get_entries",
            title: "批量读取条目",
            description: "按 id 列表批量读取条目，保留输入顺序，并报告不存在的 id。",
            access: Access::Read,
            destructive: false,
            input_schema: obj(
                json!({
                    "ids": {"type": "array", "items": {"type": "integer"}, "minItems": 1, "maxItems": 200, "description": "条目 id 列表"},
                }),
                vec!["ids"],
            ),
        },
        ToolSpec {
            name: "stats",
            title: "统计信息",
            description: "返回条目总数、标签数量、内容类型分布与当前服务的安全状态。",
            access: Access::Read,
            destructive: false,
            input_schema: obj(json!({}), vec![]),
        },
        ToolSpec {
            name: "list_tags",
            title: "列出标签",
            description: "列出全部标签及其条目数，并附带标签颜色。",
            access: Access::Read,
            destructive: false,
            input_schema: obj(json!({}), vec![]),
        },
        ToolSpec {
            name: "create_entry",
            title: "新建条目",
            description: "写入一条新的剪贴板条目，可同时指定标签与备注。",
            access: Access::Write,
            destructive: false,
            input_schema: obj(
                json!({
                    "content": {"type": "string", "description": "条目正文"},
                    "contentType": {"type": "string", "description": "内容类型，默认 text"},
                    "tags": {"type": "array", "items": {"type": "string"}, "description": "标签列表"},
                    "note": {"type": "string", "description": "备注"},
                    "sourceApp": {"type": "string", "description": "来源应用名，默认 MCP"},
                }),
                vec!["content"],
            ),
        },
        ToolSpec {
            name: "update_entry_content",
            title: "修改条目正文",
            description: "修改条目正文。image/file/video 类型存放的是文件路径，会被拒绝（这是有意设计：改写会让内容哈希与载荷不一致）。",
            access: Access::Write,
            destructive: false,
            input_schema: obj(
                json!({
                    "id": id_prop,
                    "content": {"type": "string", "description": "新的完整正文"},
                }),
                vec!["id", "content"],
            ),
        },
        ToolSpec {
            name: "update_entry_note",
            title: "修改条目备注",
            description: "设置或清空条目备注（传空串即清空）。对所有内容类型都可用。",
            access: Access::Write,
            destructive: false,
            input_schema: obj(
                json!({
                    "id": id_prop,
                    "note": {"type": "string", "description": "备注内容，空串表示清空"},
                }),
                vec!["id", "note"],
            ),
        },
        ToolSpec {
            name: "update_entry_tags",
            title: "修改条目标签",
            description: "整体替换条目的标签集合。若敏感标签（sensitive/密码）的判定发生变化，会与界面操作一样触发加解密。",
            access: Access::Write,
            destructive: false,
            input_schema: obj(
                json!({
                    "id": id_prop,
                    "tags": {"type": "array", "items": {"type": "string"}, "description": "新的标签集合（整体替换）"},
                }),
                vec!["id", "tags"],
            ),
        },
        ToolSpec {
            name: "set_entry_pinned",
            title: "设置条目置顶",
            description: "设置或取消条目置顶。与界面一样会更新置顶顺序并请求云同步。",
            access: Access::Write,
            destructive: false,
            input_schema: obj(
                json!({
                    "id": id_prop,
                    "pinned": {"type": "boolean", "description": "true 置顶，false 取消"},
                }),
                vec!["id", "pinned"],
            ),
        },
        ToolSpec {
            name: "delete_entry",
            title: "删除条目",
            description: "删除一条剪贴板条目（含其附件与云同步墓碑）。破坏性操作，需 confirm=true。",
            access: Access::Write,
            destructive: true,
            input_schema: obj(
                json!({
                    "id": id_prop,
                    "confirm": {"type": "boolean", "description": "必须显式传 true 才会真正删除"},
                }),
                vec!["id", "confirm"],
            ),
        },
        ToolSpec {
            name: "create_tag",
            title: "新建标签",
            description: "创建一个标签名。",
            access: Access::Write,
            destructive: false,
            input_schema: obj(
                json!({ "name": {"type": "string", "description": "标签名"} }),
                vec!["name"],
            ),
        },
        ToolSpec {
            name: "rename_tag",
            title: "重命名标签",
            description: "全库重命名一个标签（含标签颜色与所有条目的标签集合）。",
            access: Access::Write,
            destructive: false,
            input_schema: obj(
                json!({
                    "oldName": {"type": "string", "description": "原标签名"},
                    "newName": {"type": "string", "description": "新标签名"},
                }),
                vec!["oldName", "newName"],
            ),
        },
        ToolSpec {
            name: "delete_tag",
            title: "删除标签分组",
            description: "删除一个标签分组：只解除该标签与条目的关联，**不会删除任何条目**。破坏性操作，需 confirm=true。",
            access: Access::Write,
            destructive: true,
            input_schema: obj(
                json!({
                    "name": {"type": "string", "description": "标签名"},
                    "confirm": {"type": "boolean", "description": "必须显式传 true 才会真正删除"},
                }),
                vec!["name", "confirm"],
            ),
        },
        ToolSpec {
            name: "set_tag_color",
            title: "设置标签颜色",
            description: "设置标签颜色；color 传 null 表示清除颜色。",
            access: Access::Write,
            destructive: false,
            input_schema: obj(
                json!({
                    "name": {"type": "string", "description": "标签名"},
                    "color": {"type": ["string", "null"], "description": "颜色值（如 #ff0000），null 表示清除"},
                }),
                vec!["name", "color"],
            ),
        },
        ToolSpec {
            name: "export_backup",
            title: "导出备份",
            description: "把全部剪贴板历史、标签、附件与设置导出为一个 zip 包。不指定 outputPath 时由服务生成路径。",
            access: Access::Write,
            destructive: false,
            input_schema: obj(
                json!({
                    "outputPath": {"type": "string", "description": "输出 zip 的绝对路径；留空则自动生成到数据目录同级"},
                }),
                vec![],
            ),
        },
    ]
}

/// 按名字取工具定义。
pub fn find(name: &str) -> Option<ToolSpec> {
    catalog().into_iter().find(|t| t.name == name)
}

/// 一次工具调用的结果：要么是结构化数据，要么是执行错误。
pub struct ToolOutcome {
    pub value: Value,
    pub is_error: bool,
}

impl ToolOutcome {
    pub fn ok(value: Value) -> Self {
        Self {
            value,
            is_error: false,
        }
    }

    /// 工具**执行**阶段的错误（业务失败），按 MCP 规范应放进
    /// `result.content` 且 `isError: true`，而不是 JSON-RPC 的 `error` 字段。
    /// 参数不合法属于协议错误，由调用方走 JSON-RPC error（见 `server.rs`）。
    pub fn failed(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            value: json!({ "error": message }),
            is_error: true,
        }
    }
}

// ---------------------------------------------------------------------------
// 调用副作用：由宿主（Tauri）实现，MCP 核心只声明需要什么
// ---------------------------------------------------------------------------

/// 一次写操作之后需要宿主补齐的动作。
///
/// 这些动作（界面刷新、云同步、加解密）属于宿主能力，不能写进 MCP 核心——否则
/// 单元测试就必须起一个 Tauri 应用。测试实现空副作用并断言"需要哪些动作"，从而
/// 在不启动界面的前提下证明界面路径与 AI 路径拿到的是同一组副作用。
pub trait HostEffects: Send + Sync {
    /// 数据变了，请界面刷新。
    fn emit_changed(&self);
    /// 请按当前策略请求一次云同步。
    fn request_cloud_sync(&self);
    /// 把加解密任务交给既有队列。
    fn enqueue_encryption(&self, id: i64, encrypt: bool);
    /// 当前数据目录（导出与附件清理需要）。
    fn data_dir(&self) -> Option<std::path::PathBuf>;
    /// 当前应用版本（写进备份包 manifest）。
    fn app_version(&self) -> String;
}

/// 单元测试与"无宿主"场景使用的空实现：不做任何外部动作，且可记录被请求的动作。
#[derive(Default)]
pub struct NoopEffects {
    pub changed: std::sync::atomic::AtomicUsize,
    pub sync_requests: std::sync::atomic::AtomicUsize,
    pub encryptions: std::sync::Mutex<Vec<(i64, bool)>>,
}

impl HostEffects for NoopEffects {
    fn emit_changed(&self) {
        self.changed
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    fn request_cloud_sync(&self) {
        self.sync_requests
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    fn enqueue_encryption(&self, id: i64, encrypt: bool) {
        if let Ok(mut guard) = self.encryptions.lock() {
            guard.push((id, encrypt));
        }
    }

    fn data_dir(&self) -> Option<std::path::PathBuf> {
        None
    }

    fn app_version(&self) -> String {
        "test".to_string()
    }
}

/// 工具调用上下文。
pub struct Ctx<'a> {
    pub store: &'a McpStore,
    pub effects: &'a dyn HostEffects,
}

/// 参数访问辅助：缺键、类型不符都返回可读错误，而不是 panic。
fn str_arg(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("参数 `{}` 必须是字符串", key))
}

fn opt_str_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

fn i64_arg(args: &Value, key: &str) -> Result<i64, String> {
    args.get(key)
        .and_then(|v| v.as_i64())
        .ok_or_else(|| format!("参数 `{}` 必须是整数", key))
}

fn usize_arg(args: &Value, key: &str, default: usize, min: usize, max: usize) -> Result<usize, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => {
            let n = v
                .as_u64()
                .ok_or_else(|| format!("参数 `{}` 必须是非负整数", key))? as usize;
            if n < min || n > max {
                return Err(format!("参数 `{}` 必须在 {} 到 {} 之间", key, min, max));
            }
            Ok(n)
        }
    }
}

fn bool_arg(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
}

fn string_list_arg(args: &Value, key: &str) -> Result<Vec<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(|s| s.to_string())
                    .ok_or_else(|| format!("参数 `{}` 的元素必须是字符串", key))
            })
            .collect(),
        Some(_) => Err(format!("参数 `{}` 必须是字符串数组", key)),
    }
}

/// 条目 → JSON。`include_content=false` 时用 `preview` 代替 `content`，
/// 便于在大库上做概览而不必搬运全部正文。
fn entry_json(entry: &crate::domain::models::ClipboardEntry, include_content: bool) -> Value {
    let mut map = Map::new();
    map.insert("id".into(), json!(entry.id));
    map.insert("contentType".into(), json!(entry.content_type));
    map.insert(
        "contentChars".into(),
        json!(entry.content.chars().count()),
    );
    if include_content {
        map.insert("content".into(), json!(entry.content));
    } else {
        map.insert("contentPreview".into(), json!(entry.preview));
    }
    map.insert(
        "htmlContent".into(),
        match (&entry.html_content, include_content) {
            (Some(html), true) => json!(html),
            (Some(_), false) => json!(null),
            (None, _) => json!(null),
        },
    );
    map.insert("note".into(), json!(entry.note));
    map.insert("tags".into(), json!(entry.tags));
    map.insert("sourceApp".into(), json!(entry.source_app));
    map.insert("timestamp".into(), json!(entry.timestamp));
    map.insert("isPinned".into(), json!(entry.is_pinned));
    map.insert("pinnedOrder".into(), json!(entry.pinned_order));
    map.insert("useCount".into(), json!(entry.use_count));
    Value::Object(map)
}

fn now_iso() -> String {
    chrono::Local::now().to_rfc3339()
}

/// 确认条目存在，并把"读失败"与"不存在"分开报告。
///
/// 分开的理由：前者是数据库故障（可能是损坏或锁冲突），后者是用户/AI 给了错 id。
/// 混成一句"不存在"会把故障排查引到错误方向。
fn require_entry(store: &McpStore, id: i64) -> Result<(), String> {
    match store.entry(id) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(format!("条目 {} 不存在", id)),
        Err(e) => Err(format!("读取条目 {} 失败：{}", id, e)),
    }
}

/// 执行一次工具调用。
///
/// 调用方（`server.rs`）已完成：JSON-RPC 形状校验、工具存在性、写权限、破坏性
/// `confirm` 检查与审计记录。这里只负责把参数变成数据操作。
pub fn invoke(ctx: &Ctx<'_>, tool: &str, args: &Value) -> ToolOutcome {
    if !args.is_object() {
        return ToolOutcome::failed("arguments 必须是 JSON 对象");
    }
    let store = ctx.store;

    match tool {
        "list_entries" => {
            let limit = match usize_arg(args, "limit", 50, 1, 500) {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            let offset = match usize_arg(args, "offset", 0, 0, 1_000_000) {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            let tag = opt_str_arg(args, "tag");
            let content_type = opt_str_arg(args, "contentType");
            let include_content = bool_arg(args, "includeContent", true);
            match store.search_paged(None, tag.as_deref(), content_type.as_deref(), limit, offset) {
                Ok((page, has_more, ids)) => ToolOutcome::ok(json!({
                    "count": page.len(),
                    "totalReturned": page.len(),
                    "hasMore": has_more,
                    "nextOffset": if has_more { Some(offset + limit) } else { None },
                    "ids": ids,
                    "entries": page.iter().map(|e| entry_json(e, include_content)).collect::<Vec<_>>(),
                })),
                Err(e) => ToolOutcome::failed(format!("读取失败：{}", e)),
            }
        }

        "search_entries" => {
            let query = match str_arg(args, "query") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            if query.trim().is_empty() {
                return ToolOutcome::failed("参数 `query` 不能为空");
            }
            let limit = match usize_arg(args, "limit", 50, 1, 500) {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            let offset = match usize_arg(args, "offset", 0, 0, 1_000_000) {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            let tag = opt_str_arg(args, "tag");
            let tag_only = bool_arg(args, "tagOnly", false);
            let include_content = bool_arg(args, "includeContent", true);

            // 两条路径都必须先取到"候选全集"再切片：`search` 的 `limit` 已经是
            // 最终条数，若把它同时当作候选上限，`offset` 一变大就会翻不到东西。
            let candidate_limit = (limit + offset).min(10_000) as i32;
            let result = if tag_only {
                store.search(&query, candidate_limit, true)
            } else {
                store
                    .search_paged(Some(&query), tag.as_deref(), None, limit, offset)
                    .map(|(page, _, _)| page)
            };
            match result {
                Ok(mut hits) => {
                    let has_more = hits.len() > offset + limit;
                    let page: Vec<_> = hits.drain(..).skip(offset).take(limit).collect();
                    ToolOutcome::ok(json!({
                        "count": page.len(),
                        "hasMore": has_more,
                        "entries": page.iter().map(|e| entry_json(e, include_content)).collect::<Vec<_>>(),
                    }))
                }
                Err(e) => ToolOutcome::failed(format!("搜索失败：{}", e)),
            }
        }

        "get_entry" => {
            let id = match i64_arg(args, "id") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            match store.entry(id) {
                Ok(Some(entry)) => {
                    // 用仓储的"完整内容"通道再读一次并断言与条目读出的正文一致，
                    // 确保即使将来有人在这条路径上加限幅，也能立刻被发现。
                    match store.full_content(id) {
                        Ok(Some((content, content_type, html))) => {
                            let mut value = entry_json(&entry, true);
                            if let Some(map) = value.as_object_mut() {
                                map.insert("content".into(), json!(content));
                                map.insert("contentType".into(), json!(content_type));
                                map.insert("htmlContent".into(), json!(html));
                            }
                            ToolOutcome::ok(value)
                        }
                        Ok(None) => ToolOutcome::failed(format!("条目 {} 不存在", id)),
                        Err(e) => ToolOutcome::failed(format!("读取正文失败：{}", e)),
                    }
                }
                Ok(None) => ToolOutcome::failed(format!("条目 {} 不存在", id)),
                Err(e) => ToolOutcome::failed(format!("读取失败：{}", e)),
            }
        }

        "get_entries" => {
            let ids: Vec<i64> = match args.get("ids").and_then(|v| v.as_array()) {
                Some(items) => {
                    let mut out = Vec::with_capacity(items.len());
                    for v in items {
                        match v.as_i64() {
                            Some(n) => out.push(n),
                            None => return ToolOutcome::failed("参数 `ids` 的元素必须是整数"),
                        }
                    }
                    out
                }
                None => return ToolOutcome::failed("参数 `ids` 必须是整数数组"),
            };
            let include_content = bool_arg(args, "includeContent", true);
            let (found, missing) = store.entries_by_ids(&ids);
            ToolOutcome::ok(json!({
                "count": found.len(),
                "missingIds": missing,
                "entries": found.iter().map(|e| entry_json(e, include_content)).collect::<Vec<_>>(),
            }))
        }

        "stats" => {
            let count = store.count().unwrap_or(-1);
            let tags = store.tags().unwrap_or_default();
            let (page, _, _) = store
                .search_paged(None, None, None, 500, 0)
                .unwrap_or((Vec::new(), false, Vec::new()));
            let mut by_type: std::collections::BTreeMap<String, usize> =
                std::collections::BTreeMap::new();
            for e in &page {
                *by_type.entry(e.content_type.clone()).or_insert(0) += 1;
            }
            ToolOutcome::ok(json!({
                "entryCount": count,
                "tagCount": tags.len(),
                "contentTypesSampled": by_type,
                "sampleSize": page.len(),
            }))
        }

        "list_tags" => {
            let counts = match store.tags() {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(format!("读取标签失败：{}", e)),
            };
            let colors = store.tag_colors().unwrap_or_default();
            let mut items: Vec<Value> = counts
                .iter()
                .map(|(name, count)| {
                    json!({
                        "name": name,
                        "entryCount": count,
                        "color": colors.get(name),
                    })
                })
                .collect();
            items.sort_by(|a, b| {
                b["entryCount"]
                    .as_i64()
                    .unwrap_or(0)
                    .cmp(&a["entryCount"].as_i64().unwrap_or(0))
            });
            ToolOutcome::ok(json!({ "count": items.len(), "tags": items }))
        }

        "create_entry" => {
            let content = match str_arg(args, "content") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            let content_type = opt_str_arg(args, "contentType").unwrap_or_else(|| "text".to_string());
            let tags = match string_list_arg(args, "tags") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            let note = opt_str_arg(args, "note").unwrap_or_default();
            let source_app = opt_str_arg(args, "sourceApp").unwrap_or_else(|| "MCP".to_string());
            let data_dir = ctx.effects.data_dir();

            match store.create_entry(
                content,
                content_type,
                tags,
                note,
                source_app,
                data_dir.as_deref(),
            ) {
                Ok(id) => {
                    ctx.effects.emit_changed();
                    ctx.effects.request_cloud_sync();
                    ToolOutcome::ok(json!({ "id": id, "created": true }))
                }
                Err(e) => ToolOutcome::failed(format!("新建失败：{}", e)),
            }
        }

        "update_entry_content" => {
            let id = match i64_arg(args, "id") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            let content = match str_arg(args, "content") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            if let Err(e) = require_entry(store, id) {
                return ToolOutcome::failed(e);
            }
            match mutation::apply_entry_content(&store.repo, id, &content) {
                Ok(()) => {
                    ctx.effects.emit_changed();
                    ctx.effects.request_cloud_sync();
                    ToolOutcome::ok(json!({ "id": id, "updated": true }))
                }
                Err(e) => ToolOutcome::failed(format!("修改正文失败：{}", e)),
            }
        }

        "update_entry_note" => {
            let id = match i64_arg(args, "id") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            let note = match str_arg(args, "note") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            if let Err(e) = require_entry(store, id) {
                return ToolOutcome::failed(e);
            }
            match mutation::apply_entry_note(&store.repo, id, &note) {
                Ok(()) => {
                    ctx.effects.emit_changed();
                    ctx.effects.request_cloud_sync();
                    ToolOutcome::ok(json!({ "id": id, "updated": true }))
                }
                Err(e) => ToolOutcome::failed(format!("修改备注失败：{}", e)),
            }
        }

        "update_entry_tags" => {
            let id = match i64_arg(args, "id") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            let tags = match string_list_arg(args, "tags") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            if let Err(e) = require_entry(store, id) {
                return ToolOutcome::failed(e);
            }
            match mutation::apply_entry_tags(&store.conn, &store.tag_repo, id, tags) {
                Ok(step) => {
                    // 与界面命令完全同源：敏感性翻转时才入队加解密。
                    match step {
                        mutation::SensitiveTransition::Encrypt => {
                            ctx.effects.enqueue_encryption(id, true)
                        }
                        mutation::SensitiveTransition::Decrypt => {
                            ctx.effects.enqueue_encryption(id, false)
                        }
                        mutation::SensitiveTransition::None => {}
                    }
                    ctx.effects.emit_changed();
                    ctx.effects.request_cloud_sync();
                    ToolOutcome::ok(json!({
                        "id": id,
                        "updated": true,
                        "sensitivityChanged": !matches!(step, mutation::SensitiveTransition::None),
                    }))
                }
                Err(e) => ToolOutcome::failed(format!("修改标签失败：{}", e)),
            }
        }

        "set_entry_pinned" => {
            let id = match i64_arg(args, "id") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            let pinned = bool_arg(args, "pinned", false);
            match mutation::apply_entry_pin(&store.conn, &store.repo, id, pinned) {
                Ok(()) => {
                    ctx.effects.emit_changed();
                    ctx.effects.request_cloud_sync();
                    ToolOutcome::ok(json!({ "id": id, "pinned": pinned }))
                }
                Err(e) => ToolOutcome::failed(format!("设置置顶失败：{}", e)),
            }
        }

        "delete_entry" => {
            let id = match i64_arg(args, "id") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            let data_dir = ctx.effects.data_dir();
            match mutation::apply_entry_delete(&store.repo, id, data_dir.as_deref()) {
                Ok(()) => {
                    ctx.effects.emit_changed();
                    ctx.effects.request_cloud_sync();
                    ToolOutcome::ok(json!({ "id": id, "deleted": true }))
                }
                Err(e) => ToolOutcome::failed(format!("删除失败：{}", e)),
            }
        }

        "create_tag" => {
            let name = match str_arg(args, "name") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            match mutation::apply_tag_create(&store.tag_repo, &name) {
                Ok(()) => {
                    ctx.effects.emit_changed();
                    ToolOutcome::ok(json!({ "name": name, "created": true }))
                }
                Err(e) => ToolOutcome::failed(format!("新建标签失败：{}", e)),
            }
        }

        "rename_tag" => {
            let old = match str_arg(args, "oldName") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            let new = match str_arg(args, "newName") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            match mutation::apply_tag_rename(&store.tag_repo, &old, &new) {
                Ok(()) => {
                    ctx.effects.emit_changed();
                    ToolOutcome::ok(json!({ "oldName": old, "newName": new, "renamed": true }))
                }
                Err(e) => ToolOutcome::failed(format!("重命名失败：{}", e)),
            }
        }

        "delete_tag" => {
            let name = match str_arg(args, "name") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            let data_dir = ctx.effects.data_dir();
            match mutation::apply_tag_delete(&store.tag_repo, &name, data_dir.as_deref()) {
                Ok(()) => {
                    ctx.effects.emit_changed();
                    ToolOutcome::ok(json!({
                        "name": name,
                        "deleted": true,
                        "entriesPreserved": true,
                    }))
                }
                Err(e) => ToolOutcome::failed(format!("删除标签失败：{}", e)),
            }
        }

        "set_tag_color" => {
            let name = match str_arg(args, "name") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            let color = match args.get("color") {
                Some(Value::Null) | None => None,
                Some(Value::String(s)) => Some(s.clone()),
                Some(_) => return ToolOutcome::failed("参数 `color` 必须是字符串或 null"),
            };
            match mutation::apply_tag_color(&store.tag_repo, &name, color.clone()) {
                Ok(()) => ToolOutcome::ok(json!({ "name": name, "color": color })),
                Err(e) => ToolOutcome::failed(format!("设置颜色失败：{}", e)),
            }
        }

        "export_backup" => {
            let data_dir = match ctx.effects.data_dir() {
                Some(d) => d,
                None => return ToolOutcome::failed("当前无法确定数据目录，导出不可用"),
            };
            let output = match opt_str_arg(args, "outputPath") {
                Some(p) if !p.trim().is_empty() => std::path::PathBuf::from(p.trim()),
                _ => {
                    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
                    data_dir
                        .parent()
                        .unwrap_or(&data_dir)
                        .join(format!("Tiez-Next-mcp-export-{}.zip", stamp))
                }
            };
            match crate::services::backup::create_backup(&crate::services::backup::BackupRequest {
                data_dir,
                output_path: output,
                app_version: ctx.effects.app_version(),
            }) {
                Ok(report) => ToolOutcome::ok(json!({
                    "outputPath": report.output_path,
                    "entriesWritten": report.entries_written,
                    "bytesWritten": report.bytes_written,
                    "counts": report.counts,
                    "sha256": report.sha256,
                    "skipped": report.skipped,
                    "notes": report.notes,
                    "exportedAt": now_iso(),
                })),
                Err(e) => ToolOutcome::failed(format!("导出失败：{}", e)),
            }
        }

        other => ToolOutcome::failed(format!("未知工具：{}", other)),
    }
}

/// 把布尔翻译回队列动作，供宿主实现 `HostEffects::enqueue_encryption` 时复用。
pub fn enqueue_from_bool(id: i64, encrypt: bool) -> EncryptionJob {
    EncryptionJob {
        id,
        action: if encrypt {
            EncryptionAction::Encrypt
        } else {
            EncryptionAction::Decrypt
        },
    }
}
