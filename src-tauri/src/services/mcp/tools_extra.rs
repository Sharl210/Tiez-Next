//! MCP 工具的补充测试：会话态可见性、负 id 读写、分页边界、设置族边界。
//!
//! # 为什么单独一个文件
//!
//! `selfcheck.rs`（1055 行）承载的是"工具清单与界面能力的对等性"那一组断言；
//! 本文件承载的是**本轮修复**的定向守卫：每一条都对应一个已确认的缺陷，并且都能
//! 用"退回旧行为必须变红"来证明自己在承重（见 AB2-G3-078 的"反向对照要精确到断言"）。
//!
//! 放在独立文件而不是塞进 `selfcheck.rs`，是为了让"这些测试是新增的、守的是什么"
//! 一眼可见——混进千行文件里，下一位读者会以为它们本来就在。

use serde_json::{json, Value};

use super::store::McpStore;
use super::tools::{self, HostEffects, NoopEffects};
use crate::domain::models::ClipboardEntry;

/// 造一条会话态条目（负 id）。
///
/// 【为什么负 id 必须由测试显式给】生产侧由 `pipeline.rs` 用 `-(now_micros/1000)`
/// 分配。测试若依赖真实时钟，两条连续构造的条目 id 可能相同（同毫秒），断言会随机
/// 失败；这里让调用方指定，测试因此是可重复的。
fn session_entry(id: i64, content: &str, tags: &[&str]) -> ClipboardEntry {
    ClipboardEntry {
        id,
        content_type: "text".to_string(),
        content: content.to_string(),
        html_content: None,
        source_app: "test".to_string(),
        source_app_path: None,
        timestamp: 1_700_000_000_000 + id.abs() % 1000,
        preview: content.to_string(),
        is_pinned: false,
        tags: tags.iter().map(|s| s.to_string()).collect(),
        use_count: 0,
        is_external: false,
        pinned_order: 0,
        note: String::new(),
        file_preview_exists: true,
    }
}

fn seed_db_entry(store: &McpStore, content: &str) -> i64 {
    store
        .create_entry(
            content.to_string(),
            "text".to_string(),
            Vec::new(),
            String::new(),
            "test".to_string(),
            None,
        )
        .expect("造库内条目应成功")
}

/// 调一次工具。
fn call(store: &McpStore, effects: &dyn HostEffects, tool: &str, args: Value) -> Value {
    let ctx = tools::Ctx { store, effects };
    let outcome = tools::invoke(&ctx, tool, &args);
    assert!(
        !outcome.is_error,
        "{} 不应失败：{}",
        tool,
        outcome.value
    );
    outcome.value
}

fn call_expect_failure(
    store: &McpStore,
    effects: &dyn HostEffects,
    tool: &str,
    args: Value,
) -> String {
    let ctx = tools::Ctx { store, effects };
    let outcome = tools::invoke(&ctx, tool, &args);
    assert!(outcome.is_error, "{} 本应失败，却成功了：{}", tool, outcome.value);
    outcome
        .value
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

// ===========================================================================
// P0-1：会话态条目必须对 MCP 可见
// ===========================================================================

/// **本轮最核心的一条**：默认设置下，用户屏幕上的条目全在内存里，MCP 必须读得到。
///
/// 【缺陷原状】`list_entries` 只查数据库。而 `database.rs` 把 `app.persistent` 种成
/// `'false'`，`pipeline.rs` 据此把所有新条目放进 `SessionHistory`（负 id、不落库）。
/// 于是 AI 看到的条目集合与用户看到的**完全不相交**，且工具不报错——返回一个空列表，
/// 看起来只是"库里没东西"。
///
/// 【反向对照】把 `list_entries` 里的 `session_of(ctx)` 合并逻辑删掉，本测试立刻变红
/// （实测见本轮报告的"反向对照"一节）。
#[test]
fn list_entries_sees_session_state_by_default() {
    let store = McpStore::in_memory();
    let effects = NoopEffects::with_session(vec![
        session_entry(-1001, "内存里的第一条", &[]),
        session_entry(-1002, "内存里的第二条", &[]),
    ]);
    // 数据库刻意保持为空——这正是默认设置下的真实状态。
    assert_eq!(store.count().unwrap(), 0, "前提：数据库必须是空的");

    let value = call(&store, &effects, "list_entries", json!({}));
    let entries = value["entries"].as_array().expect("entries 应为数组");
    assert_eq!(
        entries.len(),
        2,
        "数据库为空时，仍必须返回内存里的 2 条；实际返回 {}",
        entries.len()
    );
    assert!(
        entries.iter().any(|e| e["id"] == json!(-1001)),
        "必须包含负 id 的会话态条目：{}",
        value
    );
    assert_eq!(value["sessionIncluded"], json!(true));
    assert_eq!(value["sessionCount"], json!(2));
}

/// `includeSession=false` 时必须**如实说明**自己省略了什么。
///
/// 【为什么这条不能省】只返回空列表、不做任何标注，调用方无法区分"用户没有条目"
/// 与"这次查询被限定在数据库层"。这个区别决定了 AI 是回答"你没有记录"（错的）
/// 还是"这次没查内存层"（对的）。
#[test]
fn list_entries_can_opt_out_and_says_so() {
    let store = McpStore::in_memory();
    let effects = NoopEffects::with_session(vec![session_entry(-2001, "只在内存里", &[])]);

    let value = call(&store, &effects, "list_entries", json!({"includeSession": false}));
    assert_eq!(value["entries"].as_array().unwrap().len(), 0);
    assert_eq!(value["sessionIncluded"], json!(false));
    assert_eq!(
        value["sessionOmitted"],
        json!(true),
        "省略了内存层就必须标注，否则调用方会把空结果误读成'用户没有记录'"
    );
}

/// 没有会话态概念的宿主（单元测试、无界面场景）必须安静降级，而不是报错。
///
/// 【为什么单列一条】`session_snapshot()` 返回 `None`（无宿主）与 `Some(vec![])`
/// （有宿主但当前为空）是两件不同的事。若把前者也当成"合并了一份空列表"，行为上没错，
/// 但 `sessionIncluded` 会谎报 `true`——调用方据此以为自己在看完整视图。
#[test]
fn host_without_session_degrades_quietly() {
    let store = McpStore::in_memory();
    let effects = NoopEffects::default(); // has_session = false
    seed_db_entry(&store, "库里的条目");

    let value = call(&store, &effects, "list_entries", json!({}));
    assert_eq!(value["entries"].as_array().unwrap().len(), 1);
    assert_eq!(
        value["sessionIncluded"],
        json!(false),
        "没有会话态的宿主不该声称自己包含了内存层"
    );
    // 也不该标注"我漏了东西"——那会让人以为这是配置问题。
    assert!(value.get("sessionOmitted").is_none());
}

/// 会话态条目必须能按 id 单条读到（此前是"条目 -1001 不存在"）。
#[test]
fn get_entry_reads_a_negative_id() {
    let store = McpStore::in_memory();
    let effects = NoopEffects::with_session(vec![session_entry(-3001, "内存正文", &["标签A"])]);

    let value = call(&store, &effects, "get_entry", json!({"id": -3001}));
    assert_eq!(value["id"], json!(-3001));
    assert_eq!(value["content"], json!("内存正文"));
    assert_eq!(value["tags"], json!(["标签A"]));
}

/// 批量读要同时覆盖两层，且保持输入顺序。
#[test]
fn get_entries_merges_both_layers_in_input_order() {
    let store = McpStore::in_memory();
    let db_id = seed_db_entry(&store, "库里的");
    let effects = NoopEffects::with_session(vec![session_entry(-4001, "内存里的", &[])]);

    let value = call(
        &store,
        &effects,
        "get_entries",
        json!({"ids": [-4001, db_id, 999_999]}),
    );
    let entries = value["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "两层各一条，缺 id 不应出现：{}", value);
    assert_eq!(entries[0]["id"], json!(-4001), "必须保持输入顺序");
    assert_eq!(entries[1]["id"], json!(db_id));
    assert_eq!(value["missingIds"], json!([999_999]));
}

/// 搜索会话态条目要用**与界面相同**的匹配口径（正文 / 来源应用 / 标签）。
#[test]
fn search_entries_matches_session_with_the_ui_semantics() {
    let store = McpStore::in_memory();
    let effects = NoopEffects::with_session(vec![
        session_entry(-5001, "包含关键词 alpha 的正文", &[]),
        session_entry(-5002, "无关正文", &["alpha标签"]),
        session_entry(-5003, "也无关", &[]),
    ]);

    let by_content = call(&store, &effects, "search_entries", json!({"query": "alpha"}));
    assert_eq!(
        by_content["entries"].as_array().unwrap().len(),
        2,
        "正文命中与标签命中都应算（界面就是这么匹配的）"
    );

    let tag_only = call(
        &store,
        &effects,
        "search_entries",
        json!({"query": "alpha", "tagOnly": true}),
    );
    let tag_entries = tag_only["entries"].as_array().unwrap();
    assert_eq!(tag_entries.len(), 1, "tagOnly 只匹配标签名");
    assert_eq!(tag_entries[0]["id"], json!(-5002));
}

/// 删会话态条目必须真的从内存里移除，且不影响数据库。
#[test]
fn delete_entry_removes_a_session_entry() {
    let store = McpStore::in_memory();
    let effects = NoopEffects::with_session(vec![
        session_entry(-6001, "要删的", &[]),
        session_entry(-6002, "留着的", &[]),
    ]);
    let db_id = seed_db_entry(&store, "库里的");

    let value = call(&store, &effects, "delete_entry", json!({"id": -6001, "confirm": true}));
    assert_eq!(value["deleted"], json!(true));
    assert_eq!(value["scope"], json!("session"));

    let remaining = effects.session.lock().unwrap();
    assert_eq!(remaining.len(), 1, "内存里应只剩一条");
    assert_eq!(remaining[0].id, -6002);
    drop(remaining);
    assert_eq!(store.count().unwrap(), 1, "数据库条目不能被误删");
    assert!(store.entry(db_id).unwrap().is_some());
}

/// 删一个不存在的负 id 必须报错，而不是回一句"已删除"。
///
/// 【为什么这条重要】"更新 0 行却不报错"是这条链路上最容易出现的假成功。
/// 用户/AI 拿到"已删除"后不会再去核对，而那条记录其实还在屏幕上。
#[test]
fn delete_entry_refuses_a_bogus_negative_id() {
    let store = McpStore::in_memory();
    let effects = NoopEffects::with_session(vec![session_entry(-7001, "存在的一条", &[])]);

    let msg = call_expect_failure(
        &store,
        &effects,
        "delete_entry",
        json!({"id": -9999, "confirm": true}),
    );
    assert!(msg.contains("不存在"), "错误信息应说明不存在：{}", msg);
}

/// 置顶会话态条目要走"先落库、再把内存里的 id 换成库里 id"这条路。
///
/// 与界面 `toggle_clipboard_pin` 完全同序。若只落库不改内存 id，界面上那条会带着旧
/// 负 id 消失——用户看到"置顶了但条目不见了"。
#[test]
fn set_entry_pinned_persists_a_session_entry_and_rewrites_its_id() {
    let store = McpStore::in_memory();
    let effects = NoopEffects::with_session(vec![session_entry(-8001, "要置顶的", &[])]);

    let value = call(
        &store,
        &effects,
        "set_entry_pinned",
        json!({"id": -8001, "pinned": true}),
    );
    let new_id = value["id"].as_i64().expect("应返回新 id");
    assert!(new_id > 0, "落库后应是正 id，实际 {}", new_id);
    assert_eq!(value["previousId"], json!(-8001));
    assert_eq!(value["persisted"], json!(true));

    let entry = store.entry(new_id).unwrap().expect("库里应有这条");
    assert!(entry.is_pinned, "必须真的置顶了");

    let session = effects.session.lock().unwrap();
    assert_eq!(
        session[0].id, new_id,
        "内存里的 id 必须就地改写成库内 id，否则界面会以为条目消失了"
    );
}

/// 改标签同样要落库并改写 id，且把新标签写回内存。
#[test]
fn update_entry_tags_persists_session_entry_and_mirrors_tags() {
    let store = McpStore::in_memory();
    let effects = NoopEffects::with_session(vec![session_entry(-9001, "要打标签的", &[])]);

    let value = call(
        &store,
        &effects,
        "update_entry_tags",
        json!({"id": -9001, "tags": ["新标签", "另一个"]}),
    );
    let new_id = value["id"].as_i64().unwrap();
    assert!(new_id > 0);

    let session = effects.session.lock().unwrap();
    assert_eq!(session[0].id, new_id);
    assert_eq!(
        session[0].tags,
        vec!["新标签".to_string(), "另一个".to_string()],
        "内存里的标签必须跟上，否则界面上显示的还是旧标签"
    );
}

/// 改正文/备注同样支持负 id。
#[test]
fn content_and_note_edits_work_on_session_entries() {
    let store = McpStore::in_memory();
    let effects = NoopEffects::with_session(vec![session_entry(-9101, "旧正文", &[])]);

    let value = call(
        &store,
        &effects,
        "update_entry_content",
        json!({"id": -9101, "content": "新正文"}),
    );
    let new_id = value["id"].as_i64().unwrap();
    assert_eq!(
        store.entry(new_id).unwrap().unwrap().content,
        "新正文"
    );
    {
        let session = effects.session.lock().unwrap();
        assert_eq!(session[0].content, "新正文", "内存正文必须同步");
        assert_eq!(session[0].id, new_id);
    }

    let value = call(
        &store,
        &effects,
        "update_entry_note",
        json!({"id": new_id, "note": "一条备注"}),
    );
    assert_eq!(value["id"], json!(new_id));
    assert_eq!(store.entry(new_id).unwrap().unwrap().note, "一条备注");
}

// ===========================================================================
// P2-3：分页边界
// ===========================================================================

/// 会话态与数据库混合时，分页必须不重不漏。
///
/// 【缺陷原状】`hasMore` 在切片**之前**计算，且合并发生在切片之后，于是翻页时
/// 两次调用都可能返回同一批（或漏掉跨层的那几条）。
#[test]
fn pagination_over_both_layers_neither_duplicates_nor_drops() {
    let store = McpStore::in_memory();
    for i in 0..5 {
        seed_db_entry(&store, &format!("库里的 {}", i));
    }
    let effects = NoopEffects::with_session(vec![
        session_entry(-1101, "内存 1", &[]),
        session_entry(-1102, "内存 2", &[]),
        session_entry(-1103, "内存 3", &[]),
    ]);

    let mut seen: Vec<i64> = Vec::new();
    let mut offset = 0usize;
    let mut has_more = true;
    let mut rounds = 0;
    while has_more && rounds < 20 {
        rounds += 1;
        let value = call(
            &store,
            &effects,
            "list_entries",
            json!({"limit": 2, "offset": offset}),
        );
        for e in value["entries"].as_array().unwrap() {
            seen.push(e["id"].as_i64().unwrap());
        }
        has_more = value["hasMore"].as_bool().unwrap();
        offset += 2;
    }

    assert_eq!(seen.len(), 8, "8 条必须一条不多一条不少：{:?}", seen);
    let mut unique = seen.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), 8, "翻页出现了重复：{:?}", seen);
    assert!(
        seen.iter().any(|id| *id < 0),
        "翻页必须能翻到内存层的条目"
    );
    assert!(seen.iter().any(|id| *id > 0), "也必须能翻到库里的条目");
}

// ===========================================================================
// P1：清空历史 / 重排置顶
// ===========================================================================

/// 清空历史默认保留置顶与带标签的条目（与界面同语义），并且只清内存层该清的。
#[test]
fn clear_history_keeps_pinned_and_tagged_by_default() {
    let store = McpStore::in_memory();
    let pinned = seed_db_entry(&store, "置顶的");
    let tagged = seed_db_entry(&store, "带标签的");
    seed_db_entry(&store, "普通的 A");
    seed_db_entry(&store, "普通的 B");
    {
        use crate::infrastructure::repository::clipboard_repo::ClipboardRepository;
        ClipboardRepository::toggle_pin(&store.repo, pinned, true).unwrap();
        // 【不能先 `store.conn.lock()` 再调 `apply_entry_tags`】后者内部也会去锁同一
        // 把 `Arc<Mutex<Connection>>`，而 std 的 Mutex 不可重入——那样写会在这条测试
        // 里**永久挂住**（实测：整轮测试卡死到超时，看起来像"测试太慢"）。直接调用，
        // 让内核自己取锁。
        crate::services::clipboard_mutation::apply_entry_tags(
            &store.conn,
            &store.tag_repo,
            tagged,
            vec!["保留标签".to_string()],
        )
        .unwrap();
    }
    let mut pinned_session = session_entry(-1201, "内存里置顶的", &[]);
    pinned_session.is_pinned = true;
    let effects = NoopEffects::with_session(vec![
        pinned_session,
        session_entry(-1202, "内存里普通的", &[]),
        session_entry(-1203, "内存里带标签的", &["x"]),
    ]);

    let value = call(&store, &effects, "clear_history", json!({"confirm": true}));
    assert_eq!(value["cleared"], json!(true));
    assert_eq!(value["keepPinnedAndTagged"], json!(true));
    assert_eq!(
        value["sessionEntriesRemoved"],
        json!(1),
        "内存里只有那条普通的该被清掉"
    );

    let ids: Vec<i64> = store
        .history(100, 0, None)
        .unwrap()
        .iter()
        .map(|e| e.id)
        .collect();
    assert!(ids.contains(&pinned), "置顶的必须保留");
    assert!(ids.contains(&tagged), "带标签的必须保留");
    assert_eq!(ids.len(), 2, "两条普通的应被清掉，实际剩 {:?}", ids);
}

/// `keepPinnedAndTagged=false` 时连置顶的也清掉。
#[test]
fn clear_history_can_remove_everything() {
    let store = McpStore::in_memory();
    let pinned = seed_db_entry(&store, "置顶的");
    seed_db_entry(&store, "普通的");
    {
        use crate::infrastructure::repository::clipboard_repo::ClipboardRepository;
        ClipboardRepository::toggle_pin(&store.repo, pinned, true).unwrap();
    }
    let mut pinned_session = session_entry(-1301, "内存置顶", &[]);
    pinned_session.is_pinned = true;
    let effects = NoopEffects::with_session(vec![pinned_session]);

    let value = call(
        &store,
        &effects,
        "clear_history",
        json!({"confirm": true, "keepPinnedAndTagged": false}),
    );
    assert_eq!(value["databaseEntriesAfter"], json!(0));
    assert_eq!(value["pinnedOrTaggedAlsoRemoved"], json!(1));
    assert_eq!(store.count().unwrap(), 0);
    assert!(effects.session.lock().unwrap().is_empty());
}

/// 缺 `confirm` 时清空历史必须被拦下。
///
/// `server.rs` 的破坏性闸门也会拦（按工具清单的 `destructive` 标记），这条守的是
/// **实现内部**的第二道闸：即便将来有人把 `destructive` 标记改错，这一层仍然拦得住。
#[test]
fn clear_history_requires_confirm_inside_the_implementation() {
    let store = McpStore::in_memory();
    seed_db_entry(&store, "不该被删");
    let effects = NoopEffects::default();

    let msg = call_expect_failure(&store, &effects, "clear_history", json!({}));
    assert!(msg.contains("confirm"), "错误信息应提到 confirm：{}", msg);
    assert_eq!(store.count().unwrap(), 1, "被拦下时不许删任何东西");
}

/// 重排置顶顺序要真的落到 `pinned_order` 上，并读回可核对的结果。
#[test]
fn reorder_pinned_applies_the_requested_order() {
    let store = McpStore::in_memory();
    let a = seed_db_entry(&store, "A");
    let b = seed_db_entry(&store, "B");
    let c = seed_db_entry(&store, "C");
    {
        use crate::infrastructure::repository::clipboard_repo::ClipboardRepository;
        ClipboardRepository::toggle_pin(&store.repo, a, true).unwrap();
        ClipboardRepository::toggle_pin(&store.repo, b, true).unwrap();
        ClipboardRepository::toggle_pin(&store.repo, c, true).unwrap();
    }

    let value = call(
        &store,
        &effects_noop(),
        "reorder_pinned",
        json!({"orders": [[a, 30], [b, 20], [c, 10]]}),
    );
    assert_eq!(value["updated"], json!(3));

    let order: Vec<(i64, i64)> = store
        .history(10, 0, None)
        .unwrap()
        .into_iter()
        .map(|e| (e.id, e.pinned_order))
        .collect();
    // 界面按 `pinned_order DESC` 排，因此 A 应当排在最前。
    assert_eq!(
        order.first().map(|(id, _)| *id),
        Some(a),
        "pinned_order 最大的应排最前，实际顺序 {:?}",
        order
    );
    assert_eq!(
        order.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![a, b, c]
    );
}

/// 重排置顶必须拒绝负 id（会话态条目的 `pinned_order` 只在落库时才有意义）。
#[test]
fn reorder_pinned_rejects_session_entries() {
    let store = McpStore::in_memory();
    let effects = NoopEffects::with_session(vec![session_entry(-1401, "内存里的", &[])]);

    let msg = call_expect_failure(
        &store,
        &effects,
        "reorder_pinned",
        json!({"orders": [[-1401, 5]]}),
    );
    assert!(
        msg.contains("会话态"),
        "应明确说明这是会话态条目并给出下一步：{}",
        msg
    );
}

fn effects_noop() -> NoopEffects {
    NoopEffects::default()
}

// ===========================================================================
// P1：设置族边界
// ===========================================================================

/// `set_setting` 必须拒绝 `mcp.*`——这是"AI 不能改自己的权限"这条边界。
///
/// 【为什么必须有】`mcp.*` 里同时有 `enabled`、`allow_lan`、`require_token` 与
/// `token`。允许写入，等于允许 AI 落库一个"免鉴权 + 局域网可达"的组合。
#[test]
fn set_setting_refuses_every_mcp_key() {
    let store = McpStore::in_memory();
    let effects = NoopEffects::default();

    for key in [
        "mcp.token",
        "mcp.require_token",
        "mcp.allow_lan",
        "mcp.allow_write",
        "mcp.enabled",
        "mcp.port",
        "mcp.autostart",
    ] {
        let msg = call_expect_failure(
            &store,
            &effects,
            "set_setting",
            json!({"key": key, "value": "true"}),
        );
        assert!(
            msg.contains("mcp."),
            "{} 必须被拒绝且说明原因，实际：{}",
            key,
            msg
        );
        assert!(
            store.setting(key).is_none(),
            "{} 被拒绝后绝不允许留下写入痕迹",
            key
        );
    }
}

/// `security.*` 同样不允许经由 MCP 改写。
///
/// 【为什么单列一条】当前唯一成员是"存量凭据外流的告知是否已经展示过"的标记。
/// 若 AI 能把它写成 `true`，用户**永远**看不到那条"建议更换 MQTT 密码"的告知——
/// 而看不到的安全告知等于没有告知。这比"AI 改自己的端口"更隐蔽：它不留任何界面痕迹。
#[test]
fn set_setting_refuses_security_keys() {
    let store = McpStore::in_memory();
    let effects = NoopEffects::default();

    for key in [
        "security.credential_exposure_2026_notice_ack",
        "security.some_future_flag",
    ] {
        let msg = call_expect_failure(
            &store,
            &effects,
            "set_setting",
            json!({"key": key, "value": "true"}),
        );
        assert!(
            msg.contains("security."),
            "{} 必须被拒绝且说明原因，实际：{}",
            key,
            msg
        );
        assert!(
            store.setting(key).is_none(),
            "{} 被拒绝后绝不允许留下写入痕迹",
            key
        );
    }
}

/// 普通设置项可读写，并且真的落到同一张设置表里。
#[test]
fn set_setting_writes_ordinary_keys() {
    let store = McpStore::in_memory();
    let effects = NoopEffects::default();

    let value = call(
        &store,
        &effects,
        "set_setting",
        json!({"key": "app.theme", "value": "dark"}),
    );
    assert_eq!(value["saved"], json!(true));
    assert_eq!(store.setting("app.theme").as_deref(), Some("dark"));
}

/// `get_settings` 默认不返回不可同步/敏感项，除非显式要。
///
/// 复用云同步的白名单，等于"MCP 能读到的设置"天然落在"本来就会同步出去"的范围里；
/// 这样新开一个读取口径不会顺带把凭据带出去。
#[test]
fn get_settings_hides_non_syncable_keys_by_default() {
    use crate::infrastructure::repository::settings_repo::SettingsRepository;
    let store = McpStore::in_memory();
    SettingsRepository::set(&store.settings_repo, "app.theme", "dark").unwrap();
    SettingsRepository::set(&store.settings_repo, "cloud_sync_api_key", "s3cret").unwrap();
    SettingsRepository::set(&store.settings_repo, "mcp.token", "tok").unwrap();
    SettingsRepository::set(&store.settings_repo, "mqtt_password", "pw").unwrap();
    let effects = NoopEffects::default();

    let value = call(&store, &effects, "get_settings", json!({}));
    let keys: Vec<String> = value["settings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["key"].as_str().unwrap().to_string())
        .collect();
    assert!(keys.contains(&"app.theme".to_string()), "普通项应当可见");
    assert!(!keys.contains(&"cloud_sync_api_key".to_string()), "凭据不可见");
    assert!(!keys.contains(&"mcp.token".to_string()), "本服务令牌不可见");
    assert!(!keys.contains(&"mqtt_password".to_string()), "密码不可见");
    assert!(value["excludedNonSyncable"].as_u64().unwrap() >= 3, "应如实报告被排除了几项");

    // 显式要求时才给全部（用于排查），但这条路径不该被当成默认。
    let all = call(&store, &effects, "get_settings", json!({"includeNonSyncable": true}));
    assert!(all["settings"].as_array().unwrap().len() >= 4);
}

/// 前缀筛选要真的生效。
#[test]
fn get_settings_filters_by_prefix_and_keys() {
    use crate::infrastructure::repository::settings_repo::SettingsRepository;
    let store = McpStore::in_memory();
    SettingsRepository::set(&store.settings_repo, "app.theme", "dark").unwrap();
    SettingsRepository::set(&store.settings_repo, "app.persistent", "true").unwrap();
    SettingsRepository::set(&store.settings_repo, "file_transfer_port", "1234").unwrap();
    let effects = NoopEffects::default();

    let by_prefix = call(&store, &effects, "get_settings", json!({"prefix": "file_transfer"}));
    assert_eq!(by_prefix["count"], json!(1));

    let by_keys = call(&store, &effects, "get_settings", json!({"keys": ["app.theme"]}));
    assert_eq!(by_keys["count"], json!(1));
    assert_eq!(by_keys["settings"][0]["key"], json!("app.theme"));
}
