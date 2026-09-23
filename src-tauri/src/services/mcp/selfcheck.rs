//! 任务级自证：把"需求要证明的六件事"逐条写成可执行断言。
//!
//! # 为什么单独一个文件
//!
//! 这些测试关心的不是某个函数的行为，而是**用户提出的六条验收**：
//!
//! 1. 协议正确（`initialize` / `tools/list` / `tools/call`）；
//! 2. 鉴权：无 token / 错 token 被拒，正确 token 通过；
//! 3. 只读守卫：写开关关闭时写工具被拒（而不是静默失败）；
//! 4. **不截断**：>2000 字符的正文必须完整返回；
//! 5. **副作用一致**：通过 MCP 打敏感标签与走界面命令的结果一致；
//! 6. 写后可读：MCP 创建后能从 MCP 读回。
//!
//! 放在一处便于复核者一次跑完，也便于将来需求变化时集中修改。

#![cfg(test)]

use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::jsonrpc::Response as RpcResponse;
use super::server::{self, AuthToken, RuntimeSettings, ServerState};
use super::store::McpStore;
use super::tools::{self, HostEffects, NoopEffects};
use crate::infrastructure::repository::clipboard_repo::ClipboardRepository;
use crate::infrastructure::repository::tag_repo::TagRepository;

// ---------------------------------------------------------------------------
// 测试脚手架
// ---------------------------------------------------------------------------

/// 记录所有审计行，用于断言"拒绝也必须留痕"。
#[derive(Default)]
struct RecordingAudit(std::sync::Mutex<Vec<String>>);

impl server::AuditSink for RecordingAudit {
    fn record(&self, tool: Option<&str>, outcome: &str, detail: &str) {
        if let Ok(mut guard) = self.0.lock() {
            guard.push(format!("{:?}|{}|{}", tool, outcome, detail));
        }
    }
}

struct Harness {
    state: ServerState,
    audit: Arc<RecordingAudit>,
    effects: Arc<NoopEffects>,
}

impl Harness {
    /// 默认脚手架：**强制校验令牌**。
    ///
    /// selfcheck 关心的六条验收里有一条就是"无 token / 错 token 必须被拒"，因此
    /// 这里默认走严格档；免鉴权档由 `new_open` 显式构造。
    fn new(allow_write: bool, token: &str) -> Self {
        Self::new_with(allow_write, token, true)
    }

    /// 免鉴权档（出厂默认姿态）。
    #[allow(dead_code)]
    fn new_open(allow_write: bool, token: &str) -> Self {
        Self::new_with(allow_write, token, false)
    }

    fn new_with(allow_write: bool, token: &str, require_token: bool) -> Self {
        let audit = Arc::new(RecordingAudit::default());
        let effects = Arc::new(NoopEffects::default());
        Self {
            state: ServerState {
                store: McpStore::in_memory(),
                token: AuthToken::new(Arc::new(token.to_string()), require_token),
                settings: Arc::new(RuntimeSettings::new(allow_write)),
                audit: audit.clone(),
                effects: effects.clone(),
            },
            audit,
            effects,
        }
    }

    fn request(&self, method: &str, params: Value, id: i64) -> RpcResponse {
        server::dispatch(&self.state, method, Some(&params), json!(id))
    }

    fn call(&self, tool: &str, args: Value) -> RpcResponse {
        self.request(
            "tools/call",
            json!({ "name": tool, "arguments": args }),
            1,
        )
    }

    /// 调用一个**应当成功**的工具，返回 `structuredContent`。
    fn ok(&self, tool: &str, args: Value) -> Value {
        let response = self.call(tool, args);
        assert!(
            response.error.is_none(),
            "{} 不应产生协议错误：{:?}",
            tool,
            response.error
        );
        let result = response.result.expect("应有 result");
        assert_eq!(result["isError"], false, "{} 不应是执行错误：{}", tool, result);
        result["structuredContent"].clone()
    }

    fn audit_log(&self) -> String {
        self.audit.0.lock().map(|g| g.join("\n")).unwrap_or_default()
    }
}

/// 直接向内存库塞一条正文，绕过仓储的保存路径。
///
/// 目的是构造"库里的正文恰好是超长文本"这一状态，而不受保存路径的哈希/去重逻辑
/// 干扰。"不截断"要证明的是**读取端**不截断，因此构造数据的方式越直接越好。
fn seed_raw(store: &McpStore, content: &str, content_type: &str) -> i64 {
    let conn = store.conn.lock().unwrap();
    conn.execute(
        "INSERT INTO clipboard_history
            (content_type, content, html_content, source_app, timestamp, preview, is_pinned, content_hash, tags, note)
         VALUES (?1, ?2, NULL, 'seed', 1000, ?3, 0, 0, '[]', '')",
        rusqlite::params![content_type, content, content],
    )
    .unwrap();
    conn.last_insert_rowid()
}

/// 把库里与剪贴板条目相关的原始列取出来，用于**逐字节**比较两条路径的落库结果。
fn raw_entry_state(store: &McpStore, id: i64) -> (String, String, String, String, i64) {
    let conn = store.conn.lock().unwrap();
    conn.query_row(
        "SELECT content, preview, tags, content_type, content_hash FROM clipboard_history WHERE id = ?",
        [id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        },
    )
    .unwrap()
}

fn raw_entry_tags(store: &McpStore, id: i64) -> Vec<String> {
    let conn = store.conn.lock().unwrap();
    let mut stmt = conn
        .prepare("SELECT tag FROM entry_tags WHERE entry_id = ? ORDER BY tag")
        .unwrap();
    let rows = stmt.query_map([id], |row| row.get::<_, String>(0)).unwrap();
    rows.filter_map(|r| r.ok()).collect()
}

// ---------------------------------------------------------------------------
// 1. 协议正确性
// ---------------------------------------------------------------------------

#[test]
fn selfcheck_1_protocol_initialize_list_and_call() {
    let h = Harness::new(true, "token-1");

    // initialize：版本协商、能力、服务信息。
    let init = h.request(
        "initialize",
        json!({
            "protocolVersion": server::PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "self-check", "version": "1.0.0" }
        }),
        1,
    );
    assert!(init.error.is_none());
    let init_result = init.result.unwrap();
    assert_eq!(init_result["protocolVersion"], server::PROTOCOL_VERSION);
    assert!(init_result["capabilities"]["tools"].is_object());
    assert_eq!(init_result["serverInfo"]["name"], "tiez-next");

    // tools/list：每个工具都有 name/description/inputSchema。
    let listed = h.request("tools/list", json!({}), 2);
    let listed = listed.result.unwrap();
    let names: Vec<String> = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    for required in [
        "list_entries",
        "get_entry",
        "create_entry",
        "update_entry_content",
        "update_entry_note",
        "update_entry_tags",
        "set_entry_pinned",
        "delete_entry",
        "list_tags",
        "create_tag",
        "rename_tag",
        "delete_tag",
        "export_backup",
        "search_entries",
    ] {
        assert!(names.contains(&required.to_string()), "缺少工具 {}", required);
    }

    // tools/call：读的 JSON-RPC 形状。
    let called = h.call("list_entries", json!({}));
    assert!(called.error.is_none());
    let result = called.result.unwrap();
    assert_eq!(result["jsonrpc"], Value::Null, "result 内不应出现 jsonrpc");
    assert_eq!(result["isError"], false);
    assert!(result["content"].is_array());
    assert!(result["structuredContent"].is_object());
}

#[test]
fn selfcheck_1b_notifications_and_unknown_methods_are_handled_per_spec() {
    // 解析层：通知（无 id）必须被识别出来，调用方才能回 202 而不是回响应。
    let notification =
        super::jsonrpc::parse_request(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .unwrap();
    assert!(notification.is_notification());

    // 分发层：未实现的方法必须是 -32601，而不是"静默成功"。
    let h = Harness::new(false, "token-1");
    let unknown = h.request("resources/list", json!({}), 3);
    assert_eq!(
        unknown.error.unwrap().code,
        super::jsonrpc::METHOD_NOT_FOUND
    );
}

// ---------------------------------------------------------------------------
// 2. 鉴权
// ---------------------------------------------------------------------------

#[test]
fn selfcheck_2_token_gate() {
    let h = Harness::new(false, "right-token");

    // 正确 token。
    assert!(h.state.token.check(Some("right-token")).is_ok());

    // 错 token 与缺 token 都必须被拒，且**原因可区分**。
    let wrong = h.state.token.check(Some("wrong-token")).unwrap_err();
    assert!(wrong.contains("不正确"), "{}", wrong);
    let missing = h.state.token.check(None).unwrap_err();
    assert!(missing.contains("缺少"), "{}", missing);

    // 长度不同的 token 也不能通过（定长比较不早退）。
    assert!(h.state.token.check(Some("right-token-plus")).is_err());

    // 未配置 token 时不得放行。
    let unconfigured = AuthToken::required(Arc::new(String::new()));
    assert!(unconfigured.check(Some("")).is_err());
    assert!(unconfigured.check(None).is_err());
}

#[test]
fn selfcheck_2c_default_posture_is_open_but_the_gate_still_exists() {
    // 出厂默认是免鉴权：无令牌请求必须通过。这是用户明确要求的开箱行为。
    let open = Harness::new_open(false, "unused");
    assert!(open.state.token.check(None).is_ok(), "默认姿态下无令牌必须通过");
    assert!(open.state.token.check(Some("whatever")).is_ok());

    // 但令牌能力没有被删掉：同一个 harness 打开开关就立刻收紧。
    open.state.token.set_require_token(true);
    assert!(open.state.token.check(None).is_err(), "开启后无令牌必须被拒");
    assert!(open.state.token.check(Some("wrong")).is_err());
    assert!(open.state.token.check(Some("unused")).is_ok());
}

#[test]
fn selfcheck_2d_origin_is_checked_even_without_a_token() {
    // 免鉴权只关掉令牌这一道门，不关 Origin 校验：DNS rebinding 防护是独立要求。
    assert!(server::origin_allowed(None));
    assert!(server::origin_allowed(Some("http://127.0.0.1:23123")));
    assert!(!server::origin_allowed(Some("http://evil.example.com")));
}

#[test]
fn selfcheck_2b_origin_is_checked() {
    // 规范要求必须校验 Origin 以防 DNS rebinding。
    assert!(server::origin_allowed(None));
    assert!(server::origin_allowed(Some("http://127.0.0.1:39217")));
    assert!(!server::origin_allowed(Some("http://evil.example.com")));
    assert!(!server::origin_allowed(Some("https://127.0.0.1.evil.com")));
    assert!(!server::origin_allowed(Some("null")));
}

// ---------------------------------------------------------------------------
// 3. 只读守卫
// ---------------------------------------------------------------------------

#[test]
fn selfcheck_3_readonly_guard() {
    let h = Harness::new(false, "t");

    // 读工具正常。
    assert!(h.call("list_entries", json!({})).error.is_none());
    assert!(h.call("get_entry", json!({ "id": 1 })).error.is_none());
    assert!(h.call("list_tags", json!({})).error.is_none());

    // 每一个写工具都必须被拒——不是静默失败（`isError` 而非无响应），
    // 也不是"执行了但返回失败"。
    for spec in tools::catalog()
        .into_iter()
        .filter(|t| t.access == tools::Access::Write)
    {
        let response = h.call(spec.name, json!({}));
        let error = response.error.unwrap_or_else(|| {
            panic!("只读模式下 {} 必须被拒绝，而不是产生 result", spec.name)
        });
        assert_eq!(error.code, super::jsonrpc::INVALID_PARAMS);
        assert!(
            error.message.contains("禁用") || error.message.contains("只读"),
            "{} 的拒绝信息应指向“去设置里开启”：{}",
            spec.name,
            error.message
        );
    }

    // 拒绝必须留痕。
    assert!(h.audit_log().contains("denied_readonly"));

    // 打开开关后同一个调用应当成功。
    h.state.settings.set_allow_write(true);
    let created = h.ok("create_entry", json!({ "content": "开写之后" }));
    assert!(created["id"].as_i64().unwrap() > 0);
}

#[test]
fn selfcheck_3b_destructive_tools_need_confirmation() {
    let h = Harness::new(true, "t");
    let id = seed_raw(&h.state.store, "待删除", "text");

    let denied = h.call("delete_entry", json!({ "id": id }));
    let error = denied.error.expect("缺 confirm 必须被拒");
    assert!(error.message.contains("confirm"));

    let allowed = h.ok("delete_entry", json!({ "id": id, "confirm": true }));
    assert_eq!(allowed["deleted"], true);
    assert!(h.state.store.entry(id).unwrap().is_none());
}

// ---------------------------------------------------------------------------
// 4. 不截断（最容易错的一点）
// ---------------------------------------------------------------------------

/// 3500 个字符，稳稳越过界面命令的 2000 字符与标签命令的 50000 字符阈值的对比点。
fn long_body() -> String {
    let mut s = String::new();
    for i in 0..3500usize {
        s.push(char::from(b'a' + (i % 26) as u8));
    }
    s
}

#[test]
fn selfcheck_4_content_is_never_truncated() {
    let h = Harness::new(true, "t");
    let body = long_body();
    assert!(body.chars().count() > 2000, "样本必须超过 2000 字符");

    let store = &h.state.store;
    let seeded = seed_raw(store, &body, "text");

    // (a) get_entry 返回完整正文。
    let fetched = h.ok("get_entry", json!({ "id": seeded }));
    let returned = fetched["content"].as_str().unwrap();
    assert_eq!(
        returned.chars().count(),
        3500,
        "get_entry 不应截断：实际 {} 字符",
        returned.chars().count()
    );
    assert_eq!(returned, &body);
    assert!(
        !returned.contains("Truncated") && !returned.contains("Content Truncated"),
        "不得出现 UI 层的截断后缀"
    );

    // (b) list_entries 返回的正文同样完整。
    let listed = h.ok("list_entries", json!({}));
    let first = &listed["entries"][0];
    assert_eq!(first["content"].as_str().unwrap(), &body);
    assert_eq!(first["contentChars"], json!(3500));

    // (c) search_entries 命中同一串时不截断。
    let searched = h.ok("search_entries", json!({ "query": "abc" }));
    let hit = searched["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == json!(seeded))
        .expect("搜索应命中该条");
    assert_eq!(hit["content"].as_str().unwrap(), &body);

    // (d) 批量读也不截断。
    let batch = h.ok("get_entries", json!({ "ids": [seeded] }));
    assert_eq!(batch["entries"][0]["content"].as_str().unwrap(), &body);

}

#[test]
fn selfcheck_4b_write_then_read_back_is_not_truncated() {
    // 这条覆盖"写入路径也不能顺手截断正文"：若 create_entry 存的是预览而非全文，
    // 读回来就会变短。
    let h = Harness::new(true, "t");
    let body = long_body();
    let created = h.ok("create_entry", json!({ "content": body }));
    let id = created["id"].as_i64().unwrap();

    let read_back = h.ok("get_entry", json!({ "id": id }));
    assert_eq!(read_back["content"].as_str().unwrap(), &body);
    assert_eq!(read_back["contentChars"], json!(3500));
}

// ---------------------------------------------------------------------------
// 5. 副作用一致：MCP 打敏感标签 == 界面命令打敏感标签
// ---------------------------------------------------------------------------

/// 界面命令路径的最小复现：与 `clipboard_cmd::update_tags` 相同的关键步骤顺序。
///
/// 这里刻意**重新写一遍调用序列**（而不是复用 MCP 的那段代码），否则就变成
/// "自己和自己比"。序列为：读旧标签 → 写标签 → 判定敏感性翻转 → 入队加解密。
fn ui_command_path(
    store: &McpStore,
    effects: &NoopEffects,
    id: i64,
    tags: Vec<String>,
) -> Result<(), String> {
    use crate::services::clipboard_mutation::{apply_entry_tags, SensitiveTransition};
    use crate::services::encryption_queue::EncryptionAction;

    let transition = apply_entry_tags(&store.conn, &store.tag_repo, id, tags)?;
    let action = match transition {
        SensitiveTransition::Encrypt => Some(EncryptionAction::Encrypt),
        SensitiveTransition::Decrypt => Some(EncryptionAction::Decrypt),
        SensitiveTransition::None => None,
    };
    if let Some(action) = action {
        effects.enqueue_encryption(id, matches!(action, EncryptionAction::Encrypt));
    }
    effects.emit_changed();
    Ok(())
}

#[test]
fn selfcheck_5_sensitive_tag_side_effect_matches_the_ui_command() {
    let body = "SuperSecret-payload";

    // --- 路径 A：MCP ---
    let mcp = Harness::new(true, "t");
    let mcp_id = seed_raw(&mcp.state.store, body, "text");
    mcp.ok(
        "update_entry_tags",
        json!({ "id": mcp_id, "tags": ["sensitive"] }),
    );

    // --- 路径 B：界面命令 ---
    let ui_effects = Arc::new(NoopEffects::default());
    let ui_store = McpStore::in_memory();
    let ui_id = seed_raw(&ui_store, body, "text");
    ui_command_path(
        &ui_store,
        ui_effects.as_ref(),
        ui_id,
        vec!["sensitive".to_string()],
    )
    .expect("界面路径应成功");

    // 两条路径的**原始库状态**必须逐字节一致。
    assert_eq!(
        raw_entry_state(&mcp.state.store, mcp_id),
        raw_entry_state(&ui_store, ui_id),
        "MCP 与界面命令落库结果必须一致"
    );
    // 关联表也必须一致（这正是"直接 UPDATE clipboard_history.tags" 会漏掉的部分）。
    assert_eq!(
        raw_entry_tags(&mcp.state.store, mcp_id),
        raw_entry_tags(&ui_store, ui_id)
    );
    assert_eq!(
        raw_entry_tags(&mcp.state.store, mcp_id),
        vec!["sensitive".to_string()]
    );

    // 两条路径都必须请求一次"加密"（true = Encrypt）。
    let mcp_jobs = mcp.effects.encryptions.lock().unwrap().clone();
    let ui_jobs = ui_effects.encryptions.lock().unwrap().clone();
    assert_eq!(mcp_jobs.len(), 1, "应恰好入队一次加解密");
    assert_eq!(mcp_jobs, ui_jobs, "两条路径的加解密请求必须相同");
    assert!(mcp_jobs[0].1, "方向上必须是加密（true）");
    assert_eq!(mcp.effects.changed.load(Ordering::SeqCst), 1);
    assert_eq!(ui_effects.changed.load(Ordering::SeqCst), 1);
}

#[test]
fn selfcheck_5b_sensitivity_flip_is_symmetric() {
    // 打上敏感标签 → 加密；去掉 → 解密；重复打同一个非敏感标签 → 不动密文。
    let h = Harness::new(true, "t");
    let id = seed_raw(&h.state.store, "payload", "text");

    h.ok("update_entry_tags", json!({ "id": id, "tags": ["work"] }));
    assert!(
        h.effects.encryptions.lock().unwrap().is_empty(),
        "非敏感标签不应触发加解密"
    );

    let encrypted = h.ok(
        "update_entry_tags",
        json!({ "id": id, "tags": ["work", "sensitive"] }),
    );
    assert_eq!(encrypted["sensitivityChanged"], true);
    assert_eq!(
        h.effects.encryptions.lock().unwrap().clone(),
        vec![(id, true)]
    );

    let decrypted = h.ok(
        "update_entry_tags",
        json!({ "id": id, "tags": ["work"] }),
    );
    assert_eq!(decrypted["sensitivityChanged"], true);
    assert_eq!(
        h.effects.encryptions.lock().unwrap().clone(),
        vec![(id, true), (id, false)]
    );

    // 再打一次"work"：敏感性没变，不应再入队。
    let again = h.ok("update_entry_tags", json!({ "id": id, "tags": ["work"] }));
    assert_eq!(again["sensitivityChanged"], false);
    assert_eq!(h.effects.encryptions.lock().unwrap().len(), 2);
}

#[test]
fn selfcheck_5c_pin_requests_cloud_sync_like_the_ui_command() {
    let h = Harness::new(true, "t");
    let id = seed_raw(&h.state.store, "pin me", "text");
    let before = h.effects.sync_requests.load(Ordering::SeqCst);
    h.ok("set_entry_pinned", json!({ "id": id, "pinned": true }));
    assert_eq!(
        h.effects.sync_requests.load(Ordering::SeqCst),
        before + 1,
        "置顶应与界面命令一样请求一次云同步"
    );
    assert!(h.state.store.entry(id).unwrap().unwrap().is_pinned);
}

#[test]
fn selfcheck_5d_hand_written_update_is_not_what_we_do() {
    // 反面对照：如果像早期设想那样直接 `UPDATE clipboard_history SET tags=...`，
    // 关联表就漏了。这条断言存在的意义是让"必须复用命令逻辑"这条约束可回归。
    let h = Harness::new(true, "t");
    let id = seed_raw(&h.state.store, "x", "text");
    {
        let conn = h.state.store.conn.lock().unwrap();
        conn.execute(
            "UPDATE clipboard_history SET tags = '[\"sensitive\"]' WHERE id = ?",
            [id],
        )
        .unwrap();
    }
    assert!(
        raw_entry_tags(&h.state.store, id).is_empty(),
        "手写 UPDATE 不会维护 entry_tags，这正是必须复用共享内核的原因"
    );

    // 走正式路径之后，关联表才被正确维护。
    h.ok("update_entry_tags", json!({ "id": id, "tags": ["sensitive"] }));
    assert_eq!(
        raw_entry_tags(&h.state.store, id),
        vec!["sensitive".to_string()]
    );
}

// ---------------------------------------------------------------------------
// 6. 写后可读
// ---------------------------------------------------------------------------

#[test]
fn selfcheck_6_create_then_read_back_through_mcp() {
    let h = Harness::new(true, "t");

    let created = h.ok(
        "create_entry",
        json!({
            "content": "MCP 写入的条目",
            "contentType": "text",
            "tags": ["mcp", "自证"],
            "note": "这是备注",
            "sourceApp": "self-check"
        }),
    );
    let id = created["id"].as_i64().unwrap();
    assert!(id > 0);

    // 通过 MCP 读回。
    let read = h.ok("get_entry", json!({ "id": id }));
    assert_eq!(read["content"], "MCP 写入的条目");
    assert_eq!(read["note"], "这是备注");
    assert_eq!(read["sourceApp"], "self-check");
    assert_eq!(read["contentType"], "text");
    let mut tags: Vec<String> = read["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    tags.sort();
    assert_eq!(tags, vec!["mcp".to_string(), "自证".to_string()]);

    // 列表里也看得到。
    let listed = h.ok("list_entries", json!({}));
    assert!(listed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["id"] == json!(id)));

    // 标签清单里也看得到（说明标签关联被正确建立）。
    let tag_list = h.ok("list_tags", json!({}));
    let names: Vec<String> = tag_list["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"mcp".to_string()), "{:?}", names);
}

#[test]
fn selfcheck_6b_edit_operations_round_trip() {
    let h = Harness::new(true, "t");
    let id = h.ok("create_entry", json!({ "content": "原始" }))["id"]
        .as_i64()
        .unwrap();

    h.ok("update_entry_content", json!({ "id": id, "content": "改过" }));
    h.ok("update_entry_note", json!({ "id": id, "note": "备注改过" }));
    h.ok("set_entry_pinned", json!({ "id": id, "pinned": true }));

    let read = h.ok("get_entry", json!({ "id": id }));
    assert_eq!(read["content"], "改过");
    assert_eq!(read["note"], "备注改过");
    assert_eq!(read["isPinned"], true);

    // 清空备注。
    h.ok("update_entry_note", json!({ "id": id, "note": "   " }));
    assert_eq!(h.ok("get_entry", json!({ "id": id }))["note"], "");

    // 取消置顶。
    h.ok("set_entry_pinned", json!({ "id": id, "pinned": false }));
    assert_eq!(h.ok("get_entry", json!({ "id": id }))["isPinned"], false);
}

#[test]
fn selfcheck_6c_tag_management_round_trip() {
    let h = Harness::new(true, "t");
    let id = h.ok(
        "create_entry",
        json!({ "content": "带标签", "tags": ["旧名"] }),
    )["id"]
        .as_i64()
        .unwrap();

    h.ok("create_tag", json!({ "name": "新名" }));
    h.ok("rename_tag", json!({ "oldName": "旧名", "newName": "新名" }));

    let read = h.ok("get_entry", json!({ "id": id }));
    assert_eq!(read["tags"], json!(["新名"]));

    // 删除标签分组：条目必须**保留**，只解除关联（R3 语义）。
    h.ok("delete_tag", json!({ "name": "新名", "confirm": true }));
    let after = h.ok("get_entry", json!({ "id": id }));
    assert_eq!(after["tags"], json!([]));
    assert_eq!(after["content"], "带标签", "删除标签不得删除条目");

    // 颜色设置与清除。
    //
    // 注意 `list_tags` 的条目数只统计"被条目引用的关联表"，因此这里先新建一个
    // 带颜色的标签并给它挂一个条目，才能从清单里读到颜色。
    h.ok("create_tag", json!({ "name": "带色" }));
    h.ok("set_tag_color", json!({ "name": "带色", "color": "#ff8800" }));
    h.ok(
        "update_entry_tags",
        json!({ "id": id, "tags": ["带色"] }),
    );

    let color_of = |h: &Harness| -> Value {
        h.ok("list_tags", json!({}))["tags"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "带色")
            .map(|t| t["color"].clone())
            .unwrap_or(Value::Null)
    };
    assert_eq!(color_of(&h), json!("#ff8800"));

    h.ok("set_tag_color", json!({ "name": "带色", "color": null }));
    assert!(color_of(&h).is_null(), "清除颜色后应为 null");
}

#[test]
fn selfcheck_6d_binary_content_edit_is_refused_not_silently_accepted() {
    // image/file/video 的 content 是路径，改写会让 content_hash 与载荷不一致，
    // 仓储层会拒绝。MCP 必须如实上报这个拒绝，而不是假装成功。
    let h = Harness::new(true, "t");
    let id = seed_raw(&h.state.store, "/tmp/pic.png", "image");
    let response = h.call(
        "update_entry_content",
        json!({ "id": id, "content": "试图把图片改成文本" }),
    );
    let result = response.result.expect("应有 result");
    assert_eq!(result["isError"], true, "必须如实报错");
    assert!(result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("cannot be edited as text"));

    // 但备注仍然可以改——这正是 R4/R6 的语义。
    let note = h.call("update_entry_note", json!({ "id": id, "note": "图片备注" }));
    assert_eq!(note.result.unwrap()["isError"], false);
}

#[test]
fn selfcheck_6e_missing_entry_is_reported_not_faked() {
    let h = Harness::new(true, "t");
    for (tool, args) in [
        ("update_entry_content", json!({ "id": 999, "content": "x" })),
        ("update_entry_note", json!({ "id": 999, "note": "x" })),
        ("update_entry_tags", json!({ "id": 999, "tags": ["a"] })),
    ] {
        let response = h.call(tool, args);
        let result = response.result.unwrap_or_else(|| panic!("{} 应产生 result", tool));
        assert_eq!(result["isError"], true, "{} 对不存在条目必须报错", tool);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("不存在"));
    }
}

#[test]
fn selfcheck_extra_catalog_covers_the_full_tag_management_surface() {
    // 需求里"所有操作"指的是"凡标签管理能做的都应可用"。
    // 界面能做的四类：建标签、改名、删除分组、设置颜色；条目侧：打标签。
    // 这里逐项断言它们都在清单里，避免将来被误删。
    let names: Vec<&str> = tools::catalog().iter().map(|t| t.name).collect();
    for name in [
        "create_tag",
        "rename_tag",
        "delete_tag",
        "set_tag_color",
        "update_entry_tags",
        "list_tags",
    ] {
        assert!(names.contains(&name), "标签管理能力缺少 {}", name);
    }
}

#[test]
fn selfcheck_extra_helpers_are_not_dead_code() {
    // `enqueue_from_bool` 是宿主实现副作用时的唯一入口，这里保证它的方向映射正确。
    use crate::services::encryption_queue::EncryptionAction;
    assert!(matches!(
        tools::enqueue_from_bool(1, true).action,
        EncryptionAction::Encrypt
    ));
    assert!(matches!(
        tools::enqueue_from_bool(1, false).action,
        EncryptionAction::Decrypt
    ));
    // 仓储层 trait 在本模块中用于构造/校验测试数据，确认导入不是死代码。
    let store = McpStore::in_memory();
    assert_eq!(store.repo.get_count().unwrap(), 0);
    assert_eq!(store.tag_repo.get_all_with_counts().unwrap().len(), 0);
}

// ---------------------------------------------------------------------------
// 7. 条目标签的移动与复制（v0.5 需求⑧⑨：人能操作的 MCP 也要支持）
// ---------------------------------------------------------------------------

/// 用原始 SQL 给一条条目挂上标签，绕过写入内核。
///
/// 目的是**独立于被测路径**构造初始状态。若用 `update_entry_tags` 来铺数据，
/// "移动"的断言就变成拿共享内核去验证共享内核，测试会跟着同一个 bug 一起绿。
fn seed_tags(store: &McpStore, id: i64, tags: &[&str]) {
    {
        let conn = store.conn.lock().unwrap();
        conn.execute("DELETE FROM entry_tags WHERE entry_id = ?", [id])
            .unwrap();
        for tag in tags {
            conn.execute(
                "INSERT OR IGNORE INTO entry_tags (entry_id, tag) VALUES (?1, ?2)",
                rusqlite::params![id, tag],
            )
            .unwrap();
        }
        let json = serde_json::to_string(tags).unwrap();
        conn.execute(
            "UPDATE clipboard_history SET tags = ? WHERE id = ?",
            rusqlite::params![json, id],
        )
        .unwrap();
    }
}

/// 界面命令侧的移动/复制路径：与 `history_cmd::move_entry_to_tag` 的调用序列相同。
///
/// 刻意**重写一遍调用序列**而不是复用 MCP 的那段代码——否则就成了"自己和自己比"，
/// 两条路径即使一起错也会判绿。序列：读旧集合 → 算新集合 → 共享内核落库 →
/// 按敏感性翻转入队加解密 → 发变更事件。
fn ui_transfer_path(
    store: &McpStore,
    effects: &NoopEffects,
    id: i64,
    from_tag: &str,
    to_tag: &str,
    kind: crate::services::clipboard_mutation::TagTransfer,
) -> Result<(), String> {
    use crate::services::clipboard_mutation::{apply_entry_tag_transfer, SensitiveTransition};
    use crate::services::encryption_queue::EncryptionAction;

    let transition = apply_entry_tag_transfer(&store.conn, &store.tag_repo, id, from_tag, to_tag, kind)?;
    let action = match transition {
        SensitiveTransition::Encrypt => Some(EncryptionAction::Encrypt),
        SensitiveTransition::Decrypt => Some(EncryptionAction::Decrypt),
        SensitiveTransition::None => None,
    };
    if let Some(action) = action {
        effects.enqueue_encryption(id, matches!(action, EncryptionAction::Encrypt));
    }
    effects.emit_changed();
    Ok(())
}

#[test]
fn selfcheck_7_move_replaces_only_the_source_tag_and_leaves_the_others() {
    let h = Harness::new(true, "t");
    let id = seed_raw(&h.state.store, "要整理的条目", "text");
    seed_tags(&h.state.store, id, &["工作", "待办", "重要"]);

    let moved = h.ok(
        "move_entry_to_tag",
        json!({ "id": id, "fromTag": "工作", "toTag": "归档" }),
    );

    // 返回体自带前后集合，AI 不必再读一次。
    assert_eq!(moved["mode"], "move");
    assert_eq!(moved["tagsBefore"], json!(["工作", "待办", "重要"]));
    let mut after: Vec<String> = moved["tagsAfter"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    after.sort();
    assert_eq!(
        after,
        // 码点序：归 < 待 < 重（Rust 的 `String` 排序是码点序，不是拼音序）。
        vec!["归档".to_string(), "待办".to_string(), "重要".to_string()],
        "只应替换源标签，其他标签必须原样留下"
    );

    // 库里的关联表与上面一致（`ORDER BY tag`）。
    let mut raw = raw_entry_tags(&h.state.store, id);
    raw.sort();
    assert_eq!(
        raw,
        vec!["归档".to_string(), "待办".to_string(), "重要".to_string()]
    );

    // 冗余 JSON 列也必须同步，否则列表里的标签条带会和分组内容对不上。
    let entry = h.state.store.entry(id).unwrap().unwrap();
    let mut json_tags = entry.tags.clone();
    json_tags.sort();
    assert_eq!(json_tags, raw);
}

#[test]
fn selfcheck_7b_copy_keeps_the_source_and_adds_the_target() {
    let h = Harness::new(true, "t");
    let id = seed_raw(&h.state.store, "要复制的条目", "text");
    seed_tags(&h.state.store, id, &["工作"]);

    let copied = h.ok(
        "copy_entry_to_tag",
        json!({ "id": id, "fromTag": "工作", "toTag": "归档" }),
    );
    assert_eq!(copied["mode"], "copy");
    assert_eq!(copied["tagsBefore"], json!(["工作"]));

    let mut after: Vec<String> = copied["tagsAfter"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    after.sort();
    assert_eq!(after, vec!["工作".to_string(), "归档".to_string()]);

    // 再复制一次不得产生重复行。
    h.ok(
        "copy_entry_to_tag",
        json!({ "id": id, "fromTag": "工作", "toTag": "归档" }),
    );
    let raw = raw_entry_tags(&h.state.store, id);
    assert_eq!(raw.len(), 2, "重复复制不应堆积重复标签：{:?}", raw);
}

#[test]
fn selfcheck_7c_move_and_copy_are_the_same_two_paths() {
    // MCP 与界面命令各做一次同样的移动，落库结果必须逐字节一致。
    let mcp = Harness::new(true, "t");
    let mcp_id = seed_raw(&mcp.state.store, "same payload", "text");
    seed_tags(&mcp.state.store, mcp_id, &["A", "C"]);

    let ui_effects = Arc::new(NoopEffects::default());
    let ui_store = McpStore::in_memory();
    let ui_id = seed_raw(&ui_store, "same payload", "text");
    seed_tags(&ui_store, ui_id, &["A", "C"]);

    mcp.ok(
        "move_entry_to_tag",
        json!({ "id": mcp_id, "fromTag": "A", "toTag": "B" }),
    );
    ui_transfer_path(
        &ui_store,
        ui_effects.as_ref(),
        ui_id,
        "A",
        "B",
        crate::services::clipboard_mutation::TagTransfer::Move,
    )
    .expect("界面路径应成功");

    assert_eq!(
        raw_entry_state(&mcp.state.store, mcp_id),
        raw_entry_state(&ui_store, ui_id),
        "MCP 与界面命令的落库结果必须一致"
    );
    assert_eq!(
        raw_entry_tags(&mcp.state.store, mcp_id),
        raw_entry_tags(&ui_store, ui_id),
        "关联表也必须一致——这正是绕过共享内核会漏掉的部分"
    );
    assert_eq!(
        raw_entry_tags(&mcp.state.store, mcp_id),
        vec!["B".to_string(), "C".to_string()]
    );

    // 两条路径都必须发一次变更事件。
    assert_eq!(mcp.effects.changed.load(Ordering::SeqCst), 1);
    assert_eq!(ui_effects.changed.load(Ordering::SeqCst), 1);
}

#[test]
fn selfcheck_7d_moving_onto_a_sensitive_tag_encrypts_like_the_ui_command() {
    // 副作用一致性的关键一条：把一条明文条目"移动"到敏感标签上，必须触发加密，
    // 且两条路径的加解密请求完全相同。否则会出现"标着敏感、数据却是明文"。
    let body = "moving-into-sensitive";

    let mcp = Harness::new(true, "t");
    let mcp_id = seed_raw(&mcp.state.store, body, "text");
    seed_tags(&mcp.state.store, mcp_id, &["普通"]);

    let ui_effects = Arc::new(NoopEffects::default());
    let ui_store = McpStore::in_memory();
    let ui_id = seed_raw(&ui_store, body, "text");
    seed_tags(&ui_store, ui_id, &["普通"]);

    let moved = mcp.ok(
        "move_entry_to_tag",
        json!({ "id": mcp_id, "fromTag": "普通", "toTag": "sensitive" }),
    );
    assert_eq!(moved["sensitivityChanged"], true);
    ui_transfer_path(
        &ui_store,
        ui_effects.as_ref(),
        ui_id,
        "普通",
        "sensitive",
        crate::services::clipboard_mutation::TagTransfer::Move,
    )
    .unwrap();

    let mcp_jobs = mcp.effects.encryptions.lock().unwrap().clone();
    let ui_jobs = ui_effects.encryptions.lock().unwrap().clone();
    assert_eq!(mcp_jobs, vec![(mcp_id, true)], "移动到敏感标签必须请求加密");
    assert_eq!(
        mcp_jobs.iter().map(|(_, e)| *e).collect::<Vec<_>>(),
        ui_jobs.iter().map(|(_, e)| *e).collect::<Vec<_>>(),
        "两条路径的加解密方向必须一致"
    );

    // 反向：把敏感标签移走，必须请求解密。
    let away = mcp.ok(
        "move_entry_to_tag",
        json!({ "id": mcp_id, "fromTag": "sensitive", "toTag": "普通" }),
    );
    assert_eq!(away["sensitivityChanged"], true);
    assert_eq!(
        mcp.effects.encryptions.lock().unwrap().clone(),
        vec![(mcp_id, true), (mcp_id, false)],
        "移出敏感标签必须请求解密"
    );

    // 复制（而非移动）到敏感标签同样要加密——否则"复制到敏感标签"就成了绕过加密的后门。
    let h2 = Harness::new(true, "t");
    let id2 = seed_raw(&h2.state.store, "copy-into-sensitive", "text");
    seed_tags(&h2.state.store, id2, &["普通"]);
    let copied = h2.ok(
        "copy_entry_to_tag",
        json!({ "id": id2, "fromTag": "普通", "toTag": "密码" }),
    );
    assert_eq!(copied["sensitivityChanged"], true);
    assert_eq!(h2.effects.encryptions.lock().unwrap().clone(), vec![(id2, true)]);
}

#[test]
fn selfcheck_7e_missing_entry_is_reported_by_transfer_tools() {
    let h = Harness::new(true, "t");
    for tool in ["move_entry_to_tag", "copy_entry_to_tag"] {
        let response = h.call(tool, json!({ "id": 999, "fromTag": "a", "toTag": "b" }));
        let result = response.result.expect("应产生 result");
        assert_eq!(result["isError"], true, "{} 对不存在条目必须报错", tool);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("不存在"));
    }
}

#[test]
fn selfcheck_7f_transfer_tools_are_declared_write_and_readonly_is_enforced() {
    // 工具必须是 Write 分类（否则只读模式下会被放行），并且只读模式下确实被拒。
    for name in ["move_entry_to_tag", "copy_entry_to_tag"] {
        let spec = tools::find(name).unwrap_or_else(|| panic!("清单里缺少 {}", name));
        assert_eq!(spec.access, tools::Access::Write, "{} 必须是写工具", name);
        assert!(!spec.destructive, "{} 不是破坏性操作，不该要求 confirm", name);
    }

    let h = Harness::new(false, "t");
    for name in ["move_entry_to_tag", "copy_entry_to_tag"] {
        let response = h.call(name, json!({ "id": 1, "fromTag": "a", "toTag": "b" }));
        let error = response.error.expect("只读模式必须拒绝");
        assert!(error.message.contains("禁用") || error.message.contains("只读"));
    }
}
