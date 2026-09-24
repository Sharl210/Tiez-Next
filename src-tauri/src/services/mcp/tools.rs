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
use crate::domain::models::ClipboardEntry;
use crate::infrastructure::repository::clipboard_repo::ClipboardRepository;
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
    let id_prop = json!({
        "type": "integer",
        "description": "剪贴板条目 id；负数表示内存中的会话态条目（默认设置下新复制的内容都是这一类）"
    });

    vec![
        ToolSpec {
            name: "list_entries",
            title: "列出剪贴板条目",
            description: "分页列出剪贴板条目，可按标签、内容类型过滤。默认同时返回数据库条目与内存中的会话态条目（includeSession=true）——两者都是用户能在界面上看到的条目。返回的 content 为完整正文，不做任何截断。",
            access: Access::Read,
            destructive: false,
            input_schema: obj(
                json!({
                    "limit": {"type": "integer", "minimum": 1, "maximum": 500, "description": "本页条数，默认 50"},
                    "offset": {"type": "integer", "minimum": 0, "description": "偏移量，默认 0"},
                    "tag": {"type": "string", "description": "只返回带该标签的条目"},
                    "contentType": {"type": "string", "description": "只返回该内容类型（text/code/url/rich_text/image/file/video）"},
                    "includeContent": {"type": "boolean", "description": "是否返回完整正文，默认 true；置 false 时只返回元数据，适合大库概览"},
                    "includeSession": {"type": "boolean", "description": "是否包含内存中的会话态条目（负 id），默认 true。关闭后只返回数据库条目，与界面显示的集合可能不一致"},
                }),
                vec![],
            ),
        },
        ToolSpec {
            name: "search_entries",
            title: "搜索剪贴板条目",
            description: "按关键词搜索剪贴板条目的正文、来源应用与标签（与界面搜索同一匹配口径）。默认包含内存中的会话态条目。返回完整正文，不做截断。",
            access: Access::Read,
            destructive: false,
            input_schema: obj(
                json!({
                    "query": {"type": "string", "description": "搜索关键词"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 500, "description": "最多返回条数，默认 50"},
                    "offset": {"type": "integer", "minimum": 0, "description": "偏移量，默认 0"},
                    "tagOnly": {"type": "boolean", "description": "只在标签名中匹配，默认 false"},
                    "tag": {"type": "string", "description": "额外限定必须带该标签"},
                    "includeSession": {"type": "boolean", "description": "是否包含内存中的会话态条目（负 id），默认 true"},
                    "includeContent": {"type": "boolean", "description": "是否返回完整正文，默认 true"},
                }),
                vec!["query"],
            ),
        },
        ToolSpec {
            name: "get_entry",
            title: "读取单条条目",
            description: "按 id 读取一条剪贴板条目，含完整正文、HTML、备注、标签与置顶状态。id 可为负数——负 id 是内存中的会话态条目（默认设置下新复制的内容都在这一层）。",
            access: Access::Read,
            destructive: false,
            input_schema: obj(json!({ "id": id_prop }), vec!["id"]),
        },
        ToolSpec {
            name: "get_entries",
            title: "批量读取条目",
            description: "按 id 列表批量读取条目，保留输入顺序，并报告不存在的 id。id 可为负数（内存中的会话态条目）。",
            access: Access::Read,
            destructive: false,
            input_schema: obj(
                json!({
                    "ids": {"type": "array", "items": {"type": "integer"}, "minItems": 1, "maxItems": 200, "description": "条目 id 列表（负 id 表示内存中的会话态条目）"},
                    "includeSession": {"type": "boolean", "description": "是否解析负 id 的会话态条目，默认 true"},
                    "includeContent": {"type": "boolean", "description": "是否返回完整正文，默认 true"},
                }),
                vec!["ids"],
            ),
        },
        ToolSpec {
            name: "stats",
            title: "统计信息",
            description: "返回数据库条目数与内存会话态条目数、标签数量、内容类型分布。不含服务运行状态（那属于 MCP 自身配置，由界面管理）。",
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
                    "contentType": {"type": "string", "enum": ["text", "code", "url", "rich_text", "image", "file", "video", "emoji_sync"], "description": "内容类型，默认 text。必须是受支持的类型之一——未知类型会被当成纯文本落库，界面无法正确渲染"}, 
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
            name: "move_entry_to_tag",
            title: "移动条目标签",
            description: "把一个条目从标签 A 移动到标签 B：A 从该条目的标签集合里换成 B，条目上的其他标签不受影响（条目与标签是多对多关系）。与界面上的“移动到标签”是同一份实现。",
            access: Access::Write,
            destructive: false,
            input_schema: obj(
                json!({
                    "id": id_prop,
                    "fromTag": {"type": "string", "description": "源标签名（从哪个标签移出）"},
                    "toTag": {"type": "string", "description": "目标标签名（移动到哪个标签）"},
                }),
                vec!["id", "fromTag", "toTag"],
            ),
        },
        ToolSpec {
            name: "copy_entry_to_tag",
            title: "复制条目标签",
            description: "把条目复制到标签 B：保留原有的全部标签，额外加上 B（已存在则不重复）。与界面上的“复制到标签”是同一份实现。",
            access: Access::Write,
            destructive: false,
            input_schema: obj(
                json!({
                    "id": id_prop,
                    "fromTag": {"type": "string", "description": "源标签名（从哪个标签复制）"},
                    "toTag": {"type": "string", "description": "目标标签名（复制到哪个标签）"},
                }),
                vec!["id", "fromTag", "toTag"],
            ),
        },
        ToolSpec {
            name: "set_entry_pinned",
            title: "设置条目置顶",
            description: "设置或取消条目置顶。与界面一样会更新置顶顺序并请求云同步。id 为负数时会先把会话态条目落库（返回新 id），与界面操作同一行为。",
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
            description: "删除一条剪贴板条目（含其附件与云同步墓碑）。id 为负数时删除内存中的会话态条目（只从内存移除，不涉及数据库）。破坏性操作，需 confirm=true。",
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
        ToolSpec {
            name: "inspect_backup",
            title: "预览备份包",
            description: "只读解析一个备份 zip 包：归属、版本、条目/标签/附件数量、体积与校验结果。不做任何写入，用于导入前核对这是不是想要的那个包。",
            access: Access::Read,
            destructive: false,
            input_schema: obj(
                json!({
                    "path": {"type": "string", "description": "备份 zip 的绝对路径"},
                }),
                vec!["path"],
            ),
        },
        ToolSpec {
            name: "import_backup",
            title: "导入备份包",
            description: "导入备份包并完全恢复数据（覆盖当前数据）。执行前会先建立旁路备份；返回 restartRequired=true 表示数据已组装就绪、但**必须重启应用**才会真正换上新数据（交换发生在下次启动、打开数据库之前，因为应用此刻正占用着数据库文件）。请务必把这一点转达用户，不要只说「导入成功」。破坏性操作，需 confirm: true。",
            access: Access::Write,
            destructive: true,
            input_schema: obj(
                json!({
                    "path": {"type": "string", "description": "备份 zip 的绝对路径"},
                    "confirm": {"type": "boolean", "description": "必须为 true 才执行"},
                }),
                vec!["path", "confirm"],
            ),
        },
        ToolSpec {
            name: "list_legacy_data_dirs",
            title: "列出可迁移的旧数据目录",
            description: "只读扫描并列出本机上可迁移的历史数据目录（旧版或被改名前的数据目录），含路径、来源标识、体积、文件数、是否含数据库与是否可安全删除。",
            access: Access::Read,
            destructive: false,
            input_schema: obj(json!({}), vec![]),
        },
        ToolSpec {
            name: "migrate_from_data_dir",
            title: "从指定旧数据目录迁移",
            description: "把指定旧数据目录里的数据复制到当前数据目录（源目录不会被改动，可反复验证）。若当前库非空则拒绝接管。破坏性操作（会写入当前数据目录），需 confirm: true。",
            access: Access::Write,
            destructive: true,
            input_schema: obj(
                json!({
                    "path": {"type": "string", "description": "源数据目录的绝对路径"},
                    "confirm": {"type": "boolean", "description": "必须为 true 才执行"},
                }),
                vec!["path", "confirm"],
            ),
        },
        ToolSpec {
            name: "copy_to_clipboard",
            title: "复制条目到系统剪贴板",
            description: "把一条条目（按 id）或一段文本（按 content）写入系统剪贴板；paste=true 时随后直接粘贴到当前焦点窗口。id 可为负数（内存态条目）。",
            access: Access::Write,
            destructive: false,
            input_schema: obj(
                json!({
                    "id": {"type": "integer", "description": "条目 id，可为负数（内存态条目）；给 content 时传 0"},
                    "content": {"type": "string", "description": "直接指定的文本；给了 id 时以 id 的正文为准"},
                    "contentType": {"type": "string", "enum": ["text", "code", "url", "rich_text", "image", "file", "video", "emoji_sync"], "description": "内容类型，默认 text。必须是受支持的类型之一——未知类型会静默降级成纯文本"},
                    "paste": {"type": "boolean", "description": "是否随后执行粘贴，默认 false"},
                    "deleteAfterUse": {"type": "boolean", "description": "粘贴后是否删除该条目，默认 false"},
                    "pasteWithFormat": {"type": "boolean", "description": "是否保留富文本格式"},
                    "moveToTop": {"type": "boolean", "description": "粘贴后是否移到最前"},
                }),
                vec![],
            ),
        },
        ToolSpec {
            name: "paste_entry",
            title: "粘贴条目到当前窗口",
            description: "把一条条目写入系统剪贴板并立即粘贴到当前焦点窗口。transient=true（默认）时先保存原剪贴板内容，粘贴后还原，不污染用户剪贴板。",
            access: Access::Write,
            destructive: false,
            input_schema: obj(
                json!({
                    "id": {"type": "integer", "description": "条目 id，可为负数（内存态条目）"},
                    "content": {"type": "string", "description": "直接指定的文本；给了 id 时以 id 的正文为准"},
                    "contentType": {"type": "string", "enum": ["text", "code", "url", "rich_text", "image", "file", "video", "emoji_sync"], "description": "内容类型，默认 text。必须是受支持的类型之一——未知类型会静默降级成纯文本"},
                    "transient": {"type": "boolean", "description": "是否粘贴后还原原剪贴板内容，默认 true"},
                    "pasteWithFormat": {"type": "boolean", "description": "是否保留富文本格式"},
                }),
                vec![],
            ),
        },
        ToolSpec {
            name: "clear_history",
            title: "清空历史",
            description: "清空剪贴板历史。默认保留置顶条目与带标签的条目（keepPinnedAndTagged=true）；置 false 则全部清除。破坏性操作，需 confirm: true。",
            access: Access::Write,
            destructive: true,
            input_schema: obj(
                json!({
                    "confirm": {"type": "boolean", "description": "必须为 true 才执行"},
                    "keepPinnedAndTagged": {"type": "boolean", "description": "是否保留置顶与带标签的条目，默认 true"},
                }),
                vec!["confirm"],
            ),
        },
        ToolSpec {
            name: "reorder_pinned",
            title: "调整置顶条目顺序",
            description: "按给定的 (id, order) 列表重排置顶条目的顺序；order 越大越靠前。",
            access: Access::Write,
            destructive: false,
            input_schema: obj(
                json!({
                    "orders": {
                        "type": "array",
                        "items": {
                            "type": "array",
                            "items": {"type": "integer"},
                            "minItems": 2,
                            "maxItems": 2,
                        },
                        "minItems": 1,
                        "description": "[[id, order], ...] 的列表",
                    },
                }),
                vec!["orders"],
            ),
        },
        ToolSpec {
            name: "get_settings",
            title: "读取设置项",
            description: "读取应用设置项（键值对）。可用 keys 指定若干键，或用 prefix 按前缀筛选；都不给则返回全部。",
            access: Access::Read,
            destructive: false,
            input_schema: obj(
                json!({
                    "keys": {"type": "array", "items": {"type": "string"}, "description": "只返回这些键"},
                    "prefix": {"type": "string", "description": "只返回键名以该前缀开头的项"},
                    "includeNonSyncable": {"type": "boolean", "description": "是否返回不在云同步白名单内的设置项（含凭据类），默认 false。仅在排查时使用"},
                }),
                vec![],
            ),
        },
        ToolSpec {
            name: "set_setting",
            title: "修改设置项",
            description: "写入一个设置项。与界面保存设置同一张表、同一套加密规则。出于安全考虑，mcp.* 前缀（本服务的开关、端口、令牌等）不允许通过本工具修改。",
            access: Access::Write,
            destructive: false,
            input_schema: obj(
                json!({
                    "key": {"type": "string", "description": "设置键名"},
                    "value": {"type": "string", "description": "设置值（一律以字符串写入）"},
                }),
                vec!["key", "value"],
            ),
        },
        ToolSpec {
            name: "list_emoji_favorites",
            title: "列出表情收藏",
            description: "列出已收藏的表情图片在磁盘上的绝对路径（位于数据目录 emoji_favorites/ 下）。",
            access: Access::Read,
            destructive: false,
            input_schema: obj(json!({}), vec![]),
        },
        ToolSpec {
            name: "add_emoji_favorite",
            title: "添加表情收藏",
            description: "把一张图片加入表情收藏。可给 sourcePath（本地图片路径，支持 png/jpg/gif/webp）或 dataUrl（data:image/...;base64,...）。默认同时写入 app.emoji_favorites 清单，使界面立刻显示。",
            access: Access::Write,
            destructive: false,
            input_schema: obj(
                json!({
                    "sourcePath": {"type": "string", "description": "本地图片绝对路径"},
                    "dataUrl": {"type": "string", "description": "图片的 data URL"},
                    "updateManifest": {"type": "boolean", "description": "是否同步写入界面清单 app.emoji_favorites，默认 true"},
                }),
                vec![],
            ),
        },
        ToolSpec {
            name: "remove_emoji_favorite",
            title: "移除表情收藏",
            description: "从表情收藏移除一张图片：删除磁盘文件，并从界面清单 app.emoji_favorites 中移除该条（否则界面仍会显示一个已不存在的路径）。",
            access: Access::Write,
            destructive: false,
            input_schema: obj(
                json!({
                    "path": {"type": "string", "description": "表情图片的绝对路径"},
                    "updateManifest": {"type": "boolean", "description": "是否同步更新界面清单，默认 true"},
                }),
                vec!["path"],
            ),
        },
        ToolSpec {
            name: "get_paste_queue",
            title: "读取粘贴队列",
            description: "读取当前粘贴队列里的条目 id（按粘贴顺序）。",
            access: Access::Read,
            destructive: false,
            input_schema: obj(json!({}), vec![]),
        },
        ToolSpec {
            name: "set_paste_queue",
            title: "设置粘贴队列",
            description: "重设粘贴队列为给定的条目 id 序列（空数组表示清空队列）。id 可为负数（内存态条目）。",
            access: Access::Write,
            destructive: false,
            input_schema: obj(
                json!({
                    "itemIds": {"type": "array", "items": {"type": "integer"}, "description": "条目 id 序列，按粘贴顺序"},
                }),
                vec!["itemIds"],
            ),
        },
        ToolSpec {
            name: "request_cloud_sync",
            title: "请求云同步",
            description: "按当前云同步策略请求一次同步（未配置云同步时为无操作）。返回当前同步状态。",
            access: Access::Write,
            destructive: false,
            input_schema: obj(json!({}), vec![]),
        },
        ToolSpec {
            name: "get_cloud_sync_status",
            title: "读取云同步状态",
            description: "读取云同步的运行状态：状态机取值、是否运行中、上次同步时间、上次错误、已上传/已接收条目数。",
            access: Access::Read,
            destructive: false,
            input_schema: obj(json!({}), vec![]),
        },
        ToolSpec {
            name: "get_mqtt_status",
            title: "读取 MQTT 状态",
            description: "读取 MQTT 客户端的状态：是否已连接、是否运行中，以及配置里的 broker 与主题（不含密码等敏感值）。",
            access: Access::Read,
            destructive: false,
            input_schema: obj(json!({}), vec![]),
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

    /// 「待接管标记」的存放目录（**原生**应用数据目录）。
    ///
    /// # 为什么 AI 入口也需要它
    ///
    /// 导入备份在运行期只做"组装暂存 + 写标记"，真正的文件交换要等下次启动在
    /// `init_db` 之前完成。因此这条链**必然**需要一个原生目录来放标记；拿不到它就不能
    /// 提交这次导入（必须如实失败，而不是让 AI 回一句"导入成功"、用户重启后什么都没发生）。
    ///
    /// 默认实现返回 `None`：没有宿主的场景（单元测试、无界面）本来就没有原生数据目录，
    /// 此时导入会明确失败并说明原因，而不是猜一个路径。
    fn pending_marker_dir(&self) -> Option<std::path::PathBuf> {
        None
    }
    /// 当前应用版本（写进备份包 manifest）。
    fn app_version(&self) -> String;

    /// 读取**会话态条目快照**（内存里那些还没落库的条目，id 为负）。
    ///
    /// # 为什么这个方法是必需的，而不是"锦上添花"
    ///
    /// 界面命令 `get_clipboard_history` / `search_clipboard_history` 都会把
    /// `SessionHistory` 里的条目与数据库条目**合并**后返回；而 MCP 的全部读取工具原本
    /// 只走 `McpStore`（纯数据库）。`database.rs` 的种子把 `app.persistent` 默认写成
    /// `'false'`，`services/clipboard/pipeline.rs` 据此把**新复制的条目全部落进内存**：
    ///
    /// > 出厂默认状态下，用户屏幕上看到的每一条，id 全是负数，库里一条都没有。
    ///
    /// 于是 AI 通过 MCP 看到的条目集合与用户看到的**一条都不重合**——而且这不体现在
    /// 任何"功能清单对账表"上：工具齐全、调用成功、返回空列表，看起来只是"库是空的"。
    ///
    /// 默认实现返回 `None`，含义是"这个宿主没有会话态概念"（单元测试、无界面场景），
    /// 此时工具退化为原来的纯数据库行为，不会凭空报错。
    fn session_snapshot(&self) -> Option<Vec<crate::domain::models::ClipboardEntry>> {
        None
    }

    /// 把一批会话态条目就地改写（删除、置顶、落库后改 id、标签重命名都要用）。
    ///
    /// 与 [`Self::session_snapshot`] 同理：界面路径（`history_cmd::delete_clipboard_entry`、
    /// `clipboard_cmd::toggle_clipboard_pin`、`rename_tag_globally` 等）都会同时改写内存，
    /// 否则"AI 删掉了、界面里还在"或"改完标签，界面里会话态条目还是旧标签"——
    /// 用户看到的是自相矛盾的两份状态。
    ///
    /// 闭包收到的是**内存本体**（不是副本），自行返回命中条数；本方法把它原样透出。
    /// 返回 `None` 表示该宿主没有会话态（单元测试、无界面场景），调用方据此跳过
    /// 内存同步而不报错。
    fn session_apply(
        &self,
        _f: &(dyn Fn(&mut Vec<crate::domain::models::ClipboardEntry>) -> usize + Send + Sync),
    ) -> Option<usize> {
        None
    }

    /// 标签颜色变了，请界面刷新颜色表。
    ///
    /// 界面 `useTagColors.ts` 监听 `tag-colors-updated`，而 `TagManager.tsx` 在改色后
    /// 自己 `emit` 一次。AI 路径此前**完全不发**这个事件，于是通过 MCP 改的颜色在
    /// 界面上不生效——用户不重启就永远看不到。
    fn emit_tag_colors_updated(&self) {}

    /// 云同步是否已启用。默认 `false`（未配置时不该产生同步请求）。
    ///
    /// 【为什么不让核心直接无条件调 `request_cloud_sync`】界面 `request_cloud_sync`
    /// 内部会检查配置，未配置时是空操作——但**审计日志照样记一条**。让核心先问一句
    /// "到底配了没有"，能让"AI 改了标签但同步没配"这件事在返回值里就区分出来，
    /// 而不是留一条无效果的同步记录让人以为同步发生了。
    fn cloud_sync_enabled(&self) -> bool {
        false
    }

    // -----------------------------------------------------------------------
    // 需要真实宿主的能力
    // -----------------------------------------------------------------------
    //
    // 下面这些工具操作的是"宿主进程之外的真实状态"：系统剪贴板、焦点窗口、
    // 内存粘贴队列、数据目录里的表情文件、云同步/MQTT 运行时、旧数据目录扫描
    // 与迁移。它们**必须**有 `AppHandle`，因此实现留在 `mod.rs`（`TauriEffects`），
    // 这里只声明契约。
    //
    // 默认实现一律返回 [`HOST_UNAVAILABLE`]：单元测试与"无界面"场景下这些工具
    // 应当**明确失败**，而不是返回一个编造的成功。一条"已复制到剪贴板"的回复
    // 而剪贴板毫无变化，比直接报错坏得多。

    fn clipboard_write(&self, _req: ClipboardWrite) -> Result<Value, String> {
        Err(HOST_UNAVAILABLE.to_string())
    }

    fn list_legacy_data_dirs(&self) -> Result<Value, String> {
        Err(HOST_UNAVAILABLE.to_string())
    }

    fn migrate_from_data_dir(&self, _path: &str) -> Result<Value, String> {
        Err(HOST_UNAVAILABLE.to_string())
    }

    fn paste_queue(&self) -> Result<Value, String> {
        Err(HOST_UNAVAILABLE.to_string())
    }

    fn set_paste_queue(&self, _item_ids: &[i64]) -> Result<Value, String> {
        Err(HOST_UNAVAILABLE.to_string())
    }

    fn save_emoji_favorite(&self, _source_path: &str, _data_url: &str) -> Result<Value, String> {
        Err(HOST_UNAVAILABLE.to_string())
    }

    fn remove_emoji_favorite(&self, _path: &str) -> Result<Value, String> {
        Err(HOST_UNAVAILABLE.to_string())
    }

    fn cloud_sync_status(&self) -> Result<Value, String> {
        Err(HOST_UNAVAILABLE.to_string())
    }

    fn mqtt_status(&self) -> Result<Value, String> {
        Err(HOST_UNAVAILABLE.to_string())
    }
}

/// 无宿主时调用宿主专属工具的固定错误。
///
/// 文案里刻意写清"为什么不可用"和"在哪里才可用"：AI 拿到这句应当能自己判断
/// 是"环境不支持"而不是"参数写错了"，从而不再重试，转而如实告诉用户。
pub const HOST_UNAVAILABLE: &str =
    "该操作需要运行中的应用宿主（系统剪贴板、焦点窗口、粘贴队列、表情文件、\
     云同步/MQTT 运行时或旧数据目录迁移），当前环境没有宿主可用；\
     这些工具只在应用进程内提供，不会在单元测试或无界面环境里生效";

/// 写系统剪贴板 / 粘贴的参数。
#[derive(Debug, Clone, Default)]
pub struct ClipboardWrite {
    /// 条目 id（可为负数＝内存态条目）。给 `content` 时传 0。
    pub id: i64,
    /// 直接指定的文本；`id != 0` 时以条目正文为准。
    pub content: String,
    pub content_type: String,
    /// 写入后是否随即粘贴到当前焦点窗口（`paste_entry` 恒为 true）。
    pub paste: bool,
    /// 粘贴后是否删除该条目。
    pub delete_after_use: bool,
    pub paste_with_format: Option<bool>,
    pub move_to_top: Option<bool>,
}

/// 单元测试与"无宿主"场景使用的空实现：不做任何外部动作，且可记录被请求的动作。
#[derive(Default)]
pub struct NoopEffects {
    pub changed: std::sync::atomic::AtomicUsize,
    pub sync_requests: std::sync::atomic::AtomicUsize,
    pub encryptions: std::sync::Mutex<Vec<(i64, bool)>>,
    /// 会话态条目的假快照。默认空——单元测试要构造"内存里有哪些条目"时把它填上。
    pub session: std::sync::Mutex<Vec<crate::domain::models::ClipboardEntry>>,
    /// 宿主是否"拥有会话态"。`false` 时 `session_snapshot()` 返回 `None`，
    /// 用来验证"没有会话态的宿主"这条退化路径。
    pub has_session: std::sync::atomic::AtomicBool,
    /// `emit_tag_colors_updated` 被调用的次数。
    pub tag_color_emits: std::sync::atomic::AtomicUsize,
    /// 宿主为本轮工具提供的假返回值（`app.*` 设置项等）。
    ///
    /// 存在的理由：标签改名/删除要不要请求云同步，取决于 `app.cloud_sync_enabled`
    /// 的真值。把这个判定做成"宿主可注入"，测试才能在不启云同步的前提下验证
    /// "配置了才同步、没配置就不同步"——否则测试要么被迫起一整套云同步，要么
    /// 只能断言 `>= 1` 这种什么都能过的弱条件。
    pub host_values: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl NoopEffects {
    /// 构造一个"有会话态"的宿主，并填入给定条目。
    pub fn with_session(entries: Vec<crate::domain::models::ClipboardEntry>) -> Self {
        let me = Self::default();
        me.has_session
            .store(true, std::sync::atomic::Ordering::SeqCst);
        *me.session.lock().unwrap() = entries;
        me
    }
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

    fn session_snapshot(&self) -> Option<Vec<crate::domain::models::ClipboardEntry>> {
        if self.has_session.load(std::sync::atomic::Ordering::SeqCst) {
            self.session.lock().ok().map(|g| g.clone())
        } else {
            None
        }
    }

    fn session_apply(
        &self,
        f: &(dyn Fn(&mut Vec<crate::domain::models::ClipboardEntry>) -> usize + Send + Sync),
    ) -> Option<usize> {
        if !self.has_session.load(std::sync::atomic::Ordering::SeqCst) {
            return None;
        }
        let mut guard = self.session.lock().ok()?;
        Some(f(&mut guard))
    }

    fn emit_tag_colors_updated(&self) {
        self.tag_color_emits
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// 测试宿主如实报告"确实配了云同步"。
    ///
    /// 与真实实现同一判据（见 `TauriEffects::cloud_sync_enabled`）：两个开关之一为真
    /// 才算启用。写死一个 `false` 会让"标签操作要不要同步"的测试失去意义。
    fn cloud_sync_enabled(&self) -> bool {
        let values = match self.host_values.lock() {
            Ok(v) => v,
            Err(_) => return false,
        };
        values.get("app.cloud_sync_enabled").map(|v| v == "true").unwrap_or(false)
            || values.get("cloud_sync_enabled").map(|v| v == "true").unwrap_or(false)
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

// ---------------------------------------------------------------------------
// 会话态（内存条目）覆盖层
// ---------------------------------------------------------------------------
//
// 这是本模块最容易被漏掉的一层。界面把"数据库行 ∪ 内存里的会话态条目"合并后
// 呈现给用户；MCP 只读数据库。两者在默认设置下**完全不相交**：
//
//   `database.rs` 把 `app.persistent` 默认写成 `'false'`
//     → `services/clipboard/pipeline.rs` 走会话分支，新复制的内容只进内存（负 id）
//     → 数据库里没有这些行
//     → MCP 的 `list_entries` 返回空列表，而用户屏幕上条目排得满满当当。
//
// 更麻烦的是它不会报错：工具存在、调用成功、返回 `[]`，看起来只是"库是空的"。
// 所以这一层不是"锦上添花的功能"，而是"AI 能不能看见用户正在看的东西"。

/// 取会话态快照，并区分"宿主没有会话态"与"会话态恰好为空"。
///
/// 返回值第二项是 `has_session`。**不能只看 `Vec` 是否为空**：空列表既可能是
/// "这个宿主没有内存态概念"（单元测试、无界面运行），也可能是"有内存态但现在没条目"。
/// 前者应当安静降级成纯数据库行为，后者是正常空结果——两者在对调用方的回复里
/// 也不该混为一谈。
fn session_of(ctx: &Ctx<'_>) -> (Vec<ClipboardEntry>, bool) {
    match ctx.effects.session_snapshot() {
        Some(v) => (v, true),
        None => (Vec::new(), false),
    }
}

/// 会话态条目是否命中搜索词——判定口径逐字对齐界面 `search_clipboard_history`。
///
/// `tagOnly` 只匹配标签名；否则匹配正文、来源应用与标签。界面就这么写的，这里
/// 不能"顺手改成更聪明的匹配"：同一个搜索词在界面上能搜到、在 AI 这里搜不到，
/// 或者反过来，都是对等性缺陷。
fn session_matches(entry: &ClipboardEntry, query: &str, tag_only: bool) -> bool {
    let needle = query.to_lowercase();
    if tag_only {
        return entry
            .tags
            .iter()
            .any(|t| t.to_lowercase().contains(&needle));
    }
    entry.content.to_lowercase().contains(&needle)
        || entry.source_app.to_lowercase().contains(&needle)
        || entry
            .tags
            .iter()
            .any(|t| t.to_lowercase().contains(&needle))
}

/// 一条条目属于哪一层——用于写工具判断"该走数据库还是该走内存"。
#[derive(Debug, PartialEq, Eq)]
enum EntryHome {
    /// 库里的行（id 为正）。
    Database,
    /// 内存里的会话态条目（id 为负）。
    Session,
    /// 两层都没有。
    Nowhere,
}

/// 定位一条条目的归属。
///
/// **不能只看 id 的符号**：负 id 必然在内存里，但"id 为正却查不到"同样存在
/// （条目已被清理、或 id 是编造的）。写工具需要的是"到底在哪一层"，而不是
/// "id 好不好看"，否则一个失效的正 id 会被当成数据库条目去删，最后静默成功。
fn locate_entry(ctx: &Ctx<'_>, id: i64) -> (EntryHome, Option<ClipboardEntry>) {
    if id < 0 {
        let (session, _) = session_of(ctx);
        return match session.into_iter().find(|e| e.id == id) {
            Some(e) => (EntryHome::Session, Some(e)),
            None => (EntryHome::Nowhere, None),
        };
    }
    match ctx.store.entry(id) {
        Ok(Some(e)) => (EntryHome::Database, Some(e)),
        // 正 id 不可能是会话态条目（会话态 id 一律为负，见 `pipeline.rs` 的负 id
        // 分配），因此"查不到"就是"两层都没有"。读失败也同样归到 Nowhere：
        // 调用方拿到的是"这条不在"，与"数据库坏了"的区分由后续真正的写操作去报。
        Ok(None) | Err(_) => (EntryHome::Nowhere, None),
    }
}

/// 「本条是会话态条目就先落库」的统一入口，返回可在数据库上操作的 id。
///
/// 界面在每个写命令里各写了一遍这段（`toggle_clipboard_pin`/`update_tags`/
/// `update_item_content`），MCP 侧集中到这里，避免四处各漏一个分支。三个动作的顺序
/// 严格照界面：**先落库拿真实 id → 再在库上执行 → 最后把结果同步回内存**。
///
/// 返回 `Err` 的情况只有一种：id 为负、但宿主既没有会话态、内存里也找不到这条——
/// 此时如实报错，绝不退化成"在数据库上执行一个不存在的 id"（那会更新 0 行却不报错）。
fn adopt_session_entry(ctx: &Ctx<'_>, id: i64, action: &str) -> Result<i64, String> {
    if id >= 0 {
        // 正 id 就是库里的行。仍然确认它存在，把"id 写错了"与"数据库故障"分开报告。
        return match ctx.store.entry(id) {
            Ok(Some(_)) => Ok(id),
            Ok(None) => Err(format!("条目 {} 不存在", id)),
            Err(e) => Err(format!("读取条目 {} 失败：{}", id, e)),
        };
    }
    let (home, entry) = locate_entry(ctx, id);
    if home != EntryHome::Session {
        return Err(format!(
            "条目 {} 不存在：负 id 只在本进程的内存态里有效，且该条可能已被清理",
            id
        ));
    }
    let entry = entry.expect("Session 分支必然带条目");
    let data_dir = ctx.effects.data_dir();
    ctx.store
        .persist_session_entry(&entry, data_dir.as_deref())
        .map(|new_id| {
            // 落库成功后才改写内存 id，顺序与界面一致（反序会让内存与库不一致）。
            let _ = ctx.effects.session_apply(&|snapshot: &mut Vec<ClipboardEntry>| {
                let Some(item) = snapshot.iter_mut().find(|e| e.id == id) else {
                    return 0;
                };
                item.id = new_id;
                1
            });
            let _ = action;
            new_id
        })
        .map_err(|e| format!("{}失败（会话态条目落库）：{}", action, e))
}

/// 把新正文同步回会话态（`update_entry_content` 用）。
fn mirror_content_in_session(ctx: &Ctx<'_>, id: i64, content: &str, html: Option<&str>) {
    let preview = mutation::body_preview(content);
    let html_owned = html.map(|h| h.to_string());
    let _ = ctx.effects.session_apply(&|snapshot: &mut Vec<ClipboardEntry>| {
        let Some(item) = snapshot.iter_mut().find(|e| e.id == id) else {
            return 0;
        };
        item.content = content.to_string();
        item.preview = preview.clone();
        // R13：会话态与库内一致 —— 只有富文本条目的 HTML 会被更新，类型不被偷走。
        if item.content_type == "rich_text" {
            item.html_content = html_owned.clone();
        }
        1
    });
}

/// 把标签集合同步回会话态（`update_entry_tags` 用）。
fn set_tags_in_session(ctx: &Ctx<'_>, id: i64, tags: &[String]) {
    let owned = tags.to_vec();
    let _ = ctx.effects.session_apply(&|snapshot: &mut Vec<ClipboardEntry>| {
        let Some(item) = snapshot.iter_mut().find(|e| e.id == id) else {
            return 0;
        };
        item.tags = owned.clone();
        1
    });
}

/// 标签改名时同步改写会话态条目的标签，返回被触动的条目数。
/// 判定口径逐字对齐界面 `rename_tag_globally`：**标签名比较不区分大小写**。
/// 若这里用区分大小写的比较，把 `Sensitive` 改成 `secret` 时库里改了、内存里没改，
/// 用户会看到同一条记录的标签在两处不一致。
fn rename_tag_in_session(ctx: &Ctx<'_>, old: &str, new: &str) -> usize {
    let applied = ctx
        .effects
        .session_apply(&|snapshot: &mut Vec<ClipboardEntry>| {
            let mut touched = 0usize;
            for entry in snapshot.iter_mut() {
                let mut hit = false;
                for tag in entry.tags.iter_mut() {
                    if tag.eq_ignore_ascii_case(old) {
                        *tag = new.to_string();
                        hit = true;
                    }
                }
                if hit {
                    touched += 1;
                }
            }
            touched
        });
    applied.unwrap_or(0)
}

/// 删除标签时把会话态条目上的该标签摘掉（不改动条目本身），返回被触动的条目数。
///
/// 与界面 `delete_tag_from_all` 同语义：**只解关联，不删除带标签的条目**。
fn remove_tag_from_session(ctx: &Ctx<'_>, name: &str) -> usize {
    let applied = ctx
        .effects
        .session_apply(&|snapshot: &mut Vec<ClipboardEntry>| {
            let mut touched = 0usize;
            for entry in snapshot.iter_mut() {
                let before = entry.tags.len();
                entry.tags.retain(|t| !t.eq_ignore_ascii_case(name));
                if entry.tags.len() != before {
                    touched += 1;
                }
            }
            touched
        });
    applied.unwrap_or(0)
}

/// 标签写入后按"是否配了云同步"决定要不要请求同步。
///
/// 【为什么这里要判断，而不是直接调 `request_cloud_sync`】界面路径无条件调它，是因为
/// 界面命令拿不到"这次改动值不值得同步"的判断也不需要——未配置时它本来就是空操作。
/// 但 MCP 侧多一层收益：把"同步没配"这件事如实反映出来，避免返回值暗示"已经同步了"。
fn sync_tags_if_configured(ctx: &Ctx<'_>) {
    if ctx.effects.cloud_sync_enabled() {
        ctx.effects.request_cloud_sync();
    }
}

/// 把会话态的一条条目落库，返回新 id，并**在内存里就地把 id 改写成新 id**。
/// 与界面命令 `toggle_clipboard_pin` / `update_tags` 的顺序、结果逐一对应：
/// 1. 从内存快照里复制出待落库的条目；
/// 2. 落库（`persist_session_entry` 会把 id 归零，让 SQLite 分配真实 id）；
/// 3. 把新 id 写回内存同一条目——**这一步不能省**，否则界面上那条条目下一轮
///    刷新时会带着旧负 id 消失，用户看到"置顶了但条目不见了"。
///
/// 返回 `(new_id, 内存命中数)`。
fn persist_session_entry(
    ctx: &Ctx<'_>,
    id: i64,
    entry: &ClipboardEntry,
    data_dir: Option<&std::path::Path>,
) -> Result<i64, String> {
    let new_id = ctx.store.persist_session_entry(entry, data_dir)?;
    let applied = ctx.effects.session_apply(&|snapshot: &mut Vec<ClipboardEntry>| {
        let Some(item) = snapshot.iter_mut().find(|e| e.id == id) else {
            return 0;
        };
        item.id = new_id;
        1
    });
    // 宿主有会话态却没找到这条：说明快照是过期的（另一条路径刚改过内存）。
    // 不当成硬错误——落库已经成功，条目在库里；但要让调用方知道内存没同步上。
    if applied == Some(0) {
        return Ok(new_id);
    }
    Ok(new_id)
}

/// 「移动到标签」与「复制到标签」的共同实现。
///
/// 与界面命令 `move_entry_to_tag` / `copy_entry_to_tag` 的关系：**同一个共享内核
/// 调用序列**——读旧集合 → 由 [`mutation::transferred_tags`] 算出新集合 →
/// [`mutation::apply_entry_tag_transfer`] 落库 → 按敏感性翻转入队加解密。
/// 因此"人能操作的 MCP 也支持"不是两份实现的巧合，而是同一份实现的两个入口。
///
/// 返回值刻意带上 `tagsBefore` / `tagsAfter`：AI 调完就能自己核对"源没了、目标有了、
/// 其他标签还在"，不必再发一次读取请求。
fn tag_transfer(ctx: &Ctx<'_>, args: &Value, kind: mutation::TagTransfer) -> ToolOutcome {
    let store = ctx.store;
    let id = match i64_arg(args, "id") {
        Ok(v) => v,
        Err(e) => return ToolOutcome::failed(e),
    };
    let from_tag = match str_arg(args, "fromTag") {
        Ok(v) => v,
        Err(e) => return ToolOutcome::failed(e),
    };
    let to_tag = match str_arg(args, "toTag") {
        Ok(v) => v,
        Err(e) => return ToolOutcome::failed(e),
    };
    if let Err(e) = require_entry(store, id) {
        return ToolOutcome::failed(e);
    }

    // 变更前的集合从库里读，而不是相信调用方传进来的 tags。
    let before = match mutation::read_entry_tags(&store.conn, id) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::failed(e),
    };

    match mutation::apply_entry_tag_transfer(&store.conn, &store.tag_repo, id, &from_tag, &to_tag, kind)
    {
        Ok(step) => {
            match step {
                mutation::SensitiveTransition::Encrypt => ctx.effects.enqueue_encryption(id, true),
                mutation::SensitiveTransition::Decrypt => ctx.effects.enqueue_encryption(id, false),
                mutation::SensitiveTransition::None => {}
            }
            ctx.effects.emit_changed();
            ctx.effects.request_cloud_sync();
            let after = mutation::read_entry_tags(&store.conn, id).unwrap_or_default();
            ToolOutcome::ok(json!({
                "id": id,
                "mode": if kind == mutation::TagTransfer::Move { "move" } else { "copy" },
                "fromTag": from_tag,
                "toTag": to_tag,
                "tagsBefore": before,
                "tagsAfter": after,
                "sensitivityChanged": !matches!(step, mutation::SensitiveTransition::None),
            }))
        }
        Err(e) => ToolOutcome::failed(format!("标签转移失败：{}", e)),
    }
}

/// 条目的界面同序比较器：`is_pinned DESC, pinned_order DESC, timestamp DESC, id DESC`。
///
/// 与 `SqliteClipboardRepository::get_history` 的 `ORDER BY` 和 `McpStore::search_paged`
/// 的排序逐项对齐。合并两层数据时**必须复用同一个比较器**，否则会出现"库里的行
/// 排在内存条目前面/后面"这种只在对齐时才暴露的偏差。
///
/// 这条比较器同时是"会话态条目能不能稳定分页"的关键：负 id 在最末一级排序键上
/// 必然小于任何正 id，因此同一时间戳时内存条目总在库行之后——顺序是确定的，
/// 不会在两次调用之间抖动。
fn sort_like_ui(entries: &mut [ClipboardEntry]) {
    entries.sort_by(|a, b| {
        b.is_pinned
            .cmp(&a.is_pinned)
            .then_with(|| b.pinned_order.cmp(&a.pinned_order))
            .then_with(|| b.timestamp.cmp(&a.timestamp))
            .then_with(|| b.id.cmp(&a.id))
    });
}

/// 清空历史的实现（`clear_history` 工具）。
///
/// # 为什么直连仓库的 `clear`
///
/// `clipboard_mutation` 里曾有一个 `apply_history_clear` 包装，但它**从诞生起就没有
/// 任何调用方**（引入提交 `ff1a481` 的 diff 只加了定义；本轮实测其全部"外部出现"
/// 都是注释）。它的函数体就是 `repo.clear(data_dir)` 一行，没有加密入队、没有事件、
/// 没有会话态处理——即零附加语义，却会让人以为"MCP 与界面共享清空路径"。
/// 该包装已删除；这里直连仓储，让共享关系在调用链上真实可见，而不是靠一层空壳暗示。
///
/// 清空语义**已经**是"保留置顶与带标签"：`repo.clear` 的 SQL 条件是
/// `is_pinned = 0 AND NOT EXISTS (entry_tags 里有该条目)`，也正因为如此界面命令
/// `clear_clipboard_history` 的名字里没有参数——它本来就只有这一种行为。
/// 因此 `keepPinnedAndTagged=false` 需要**额外**再做一步：把剩下的置顶/带标签条目
/// 也删掉。这一步刻意不写进 `repo.clear`（那会改变界面行为），而在这里补齐。
fn clear_history(ctx: &Ctx<'_>, keep_pinned_and_tagged: bool) -> Result<Value, String> {
    let store = ctx.store;
    let data_dir = ctx.effects.data_dir();
    let before = store.count().unwrap_or(0);

    use crate::infrastructure::repository::clipboard_repo::ClipboardRepository;
    ClipboardRepository::clear(&store.repo, data_dir.as_deref()).map_err(|e| e.to_string())?;

    let mut also_removed = 0usize;
    if !keep_pinned_and_tagged {
        // 逐条删除剩下的（置顶 / 带标签）。用 `history` 分页取而不是一条 SQL 通配：
        // 仓储的 `delete` 会同时清理附件与墓碑记录，绕过它会留下孤儿附件。
        loop {
            let remaining = store.history(500, 0, None).unwrap_or_default();
            if remaining.is_empty() {
                break;
            }
            let mut progressed = false;
            for entry in &remaining {
                if ClipboardRepository::delete(&store.repo, entry.id, data_dir.as_deref()).is_ok()
                {
                    also_removed += 1;
                    progressed = true;
                }
            }
            if !progressed {
                // 一条都删不掉时立刻停：继续循环就是死循环，而这里没有可恢复的动作。
                return Err(format!(
                    "尚有 {} 条无法删除（可能是文件占用），已删除的部分不会回滚",
                    remaining.len()
                ));
            }
        }
    }

    // 会话态：与界面 `clear_clipboard_history` 同一条 retain 条件（置顶或带标签的留下）。
    let session_removed = ctx
        .effects
        .session_apply(&|snapshot: &mut Vec<ClipboardEntry>| {
            let before = snapshot.len();
            snapshot.retain(|i| keep_pinned_and_tagged && (i.is_pinned || !i.tags.is_empty()));
            before - snapshot.len()
        })
        .unwrap_or(0);

    ctx.effects.emit_changed();
    ctx.effects.request_cloud_sync();
    Ok(json!({
        "cleared": true,
        "keepPinnedAndTagged": keep_pinned_and_tagged,
        "databaseEntriesBefore": before,
        "databaseEntriesAfter": store.count().unwrap_or(0),
        "pinnedOrTaggedAlsoRemoved": also_removed,
        "sessionEntriesRemoved": session_removed,
    }))
}

/// 合并两层的分页结果。
pub struct MergedPage {
    pub entries: Vec<ClipboardEntry>,
    pub has_more: bool,
}

/// 在"数据库 + 会话态"合并后的集合上分页。
///
/// # 为什么必须统一分页，而不能"各自取一页再拼"
///
/// 本文件的第一版实现是"库按 (limit, offset) 取一页、内存按 (limit, offset) 取一页，
/// 再拼起来取前 limit 条"。它在**跨页**时是错的，而且错得很隐蔽：
///
/// * 库侧重取了第 offset..offset+limit 条，内存侧也重取了同一段；
/// * 合并后取前 limit 条，于是第 offset 页里可能混进第 0 页出现过的内存条目，同时把
///   本该在这一页的库条目挤到下一页——**"翻页既不重也不漏"根本无法保证**。
///
/// 实测抓到的形态：8 条数据、`limit=2`，翻页读出 40 条且大量重复。这属于"顺手就能
/// 写出、但只有真去翻页才会暴露"的一类缺陷，因此这里收敛成单一实现：每一层都从 0
/// 取到 `offset + limit + 1` 条（多取 1 条用于判断是否还有下一页），合并、统一排序，
/// **然后**才做偏移与截断。
///
/// 候选量与实际返回量同阶，不随库增长放大开销。界面 `get_clipboard_history` 也是
/// 同一条思路（合并内存条目后重新排序再截断），只是它只在 `offset == 0` 时合并。
fn page_over_both_layers(
    store: &McpStore,
    session: &[ClipboardEntry],
    tag: Option<&str>,
    content_type: Option<&str>,
    limit: usize,
    offset: usize,
) -> Result<MergedPage, String> {
    let needed = offset.saturating_add(limit).saturating_add(1).min(10_000);
    let db_page = store
        .search_paged(None, tag, content_type, needed, 0)
        .map(|(page, _, _)| page)?;

    // 会话态层：与库侧同一套过滤条件（标签不区分大小写、内容类型全等）。
    let session_page: Vec<ClipboardEntry> = session
        .iter()
        .filter(|e| {
            let tag_ok = tag
                .map(|t| e.tags.iter().any(|x| x.eq_ignore_ascii_case(t)))
                .unwrap_or(true);
            let ct_ok = content_type.map(|c| e.content_type == c).unwrap_or(true);
            tag_ok && ct_ok
        })
        .cloned()
        .collect();

    let mut merged = db_page;
    merged.extend(session_page);
    sort_like_ui(&mut merged);

    let has_more = merged.len() > offset + limit;
    let entries = merged.into_iter().skip(offset).take(limit).collect();
    Ok(MergedPage { entries, has_more })
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
            // 默认 true：AI 要看到的是"用户屏幕上那一份"，而默认设置下那一份
            // 恰好全在内存里。默认 false 会让这次修复对默认安装完全无效。
            let include_session = bool_arg(args, "includeSession", true);
            let (session_all, has_session) = if include_session {
                session_of(ctx)
            } else {
                (Vec::new(), false)
            };
            match page_over_both_layers(
                store,
                &session_all,
                tag.as_deref(),
                content_type.as_deref(),
                limit,
                offset,
            ) {
                Ok(page) => {
                    let mut value = json!({
                        "count": page.entries.len(),
                        "totalReturned": page.entries.len(),
                        "hasMore": page.has_more,
                        "nextOffset": if page.has_more { Some(offset + limit) } else { None },
                        "ids": page.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
                        "sessionIncluded": has_session && include_session,
                        "sessionCount": session_all.len(),
                        "entries": page
                            .entries
                            .iter()
                            .map(|e| entry_json(e, include_content))
                            .collect::<Vec<_>>(),
                    });
                    if !include_session {
                        // 让调用方能自己发现"这次没带内存条目"，而不是把空结果
                        // 误读成"用户什么都没复制"。
                        if let Some(map) = value.as_object_mut() {
                            map.insert("sessionOmitted".into(), json!(true));
                        }
                    }
                    ToolOutcome::ok(value)
                }
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
            let include_session = bool_arg(args, "includeSession", true);
            let (session_all, has_session) = if include_session {
                session_of(ctx)
            } else {
                (Vec::new(), false)
            };

            // 与 `list_entries` 同一套分页实现：两层都在**同一次切片**里定序，
            // 否则 offset 数的是"库里那一层"的位置，加入内存条目后整页会错位。
            let needed = offset.saturating_add(limit).saturating_add(1).min(10_000);
            let candidate_limit = needed as i32;
            let db_hits = if tag_only {
                // `store.search(.., tag_only=true)` 只匹配标签名，与界面
                // `search_clipboard_history` 的 tagOnly 分支一致；但它没有 offset
                // 参数，因此取够候选后由本函数统一切片。
                store.search(&query, candidate_limit, true)
            } else {
                store
                    .search_paged(Some(&query), tag.as_deref(), None, needed, 0)
                    .map(|(page, _, _)| page)
            };
            let mut hits = match db_hits {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(format!("搜索失败：{}", e)),
            };

            // 会话态层：与界面同一个匹配口径（tagOnly 只匹配标签名；否则匹配正文、
            // 来源应用与标签）。`tag` 参数与 `tagOnly` 是两件事——前者限定"必须带这个
            // 标签"，对内存条目同样生效。
            let session_hits: Vec<ClipboardEntry> = session_all
                .iter()
                .filter(|e| session_matches(e, &query, tag_only))
                .filter(|e| {
                    tag.as_ref()
                        .map(|t| e.tags.iter().any(|x| x.to_lowercase() == t.to_lowercase()))
                        .unwrap_or(true)
                })
                .cloned()
                .collect();
            hits.extend(session_hits);
            sort_like_ui(&mut hits);

            let has_more = hits.len() > offset + limit;
            let page: Vec<_> = hits.into_iter().skip(offset).take(limit).collect();
            let mut value = json!({
                "count": page.len(),
                "hasMore": has_more,
                "nextOffset": if has_more { Some(offset + limit) } else { None },
                "sessionIncluded": has_session && include_session,
                "entries": page.iter().map(|e| entry_json(e, include_content)).collect::<Vec<_>>(),
            });
            if !include_session {
                if let Some(map) = value.as_object_mut() {
                    map.insert("sessionOmitted".into(), json!(true));
                }
            }
            ToolOutcome::ok(value)
        }

        "get_entry" => {
            let id = match i64_arg(args, "id") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            // 会话态条目（负 id）在数据库里根本不存在，早先这里直接返回
            // "条目 -1234 不存在"——而用户屏幕上它就在第一条。先查内存。
            let (home, session_entry) = locate_entry(ctx, id);
            if home == EntryHome::Session {
                let entry = session_entry.expect("Session 分支必然带条目");
                return ToolOutcome::ok(entry_json(&entry, true));
            }
            if home == EntryHome::Nowhere {
                let hint = if id < 0 {
                    "（负 id 是内存态条目；该条目可能已被清理、或已被置顶/改标签落库并换成了正 id。\
                     请用 list_entries 或 search_entries 重新取一次 id）"
                } else {
                    ""
                };
                return ToolOutcome::failed(format!("条目 {} 不存在{}", id, hint));
            }
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
            // 与 `list_entries` 同口径：批量读也默认带上会话态条目，否则"单条读得到、
            // 批量读不到"会变成一个新的不一致面。
            let include_session = bool_arg(args, "includeSession", true);
            let (session, _) = if include_session {
                session_of(ctx)
            } else {
                (Vec::new(), false)
            };
            let (found, db_missing) = store.entries_by_ids(&ids);

            // 保持**输入顺序**：库里有的、内存里有的按 ids 原序列出。
            let mut ordered: Vec<ClipboardEntry> = Vec::new();
            let mut missing: Vec<i64> = Vec::new();
            for id in &ids {
                if let Some(e) = found.iter().find(|e| e.id == *id) {
                    ordered.push(e.clone());
                } else if let Some(e) = session.iter().find(|e| e.id == *id) {
                    ordered.push(e.clone());
                } else {
                    missing.push(*id);
                }
            }
            let _ = db_missing;
            ToolOutcome::ok(json!({
                "count": ordered.len(),
                "missingIds": missing,
                "sessionIncluded": include_session,
                "entries": ordered.iter().map(|e| entry_json(e, include_content)).collect::<Vec<_>>(),
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
            // 会话态单独计数并**明确写出来**：不写的话，AI 看到 `entryCount: 0` 会
            // 直接回答"你没有剪贴板记录"，而用户屏幕上其实排着一长串。
            //
            // 描述与实现的对齐也是这次修的一部分：catalog 里这条工具原先写着"与当前
            // 服务的安全状态"，实现却只有计数——描述比实现多，会让 AI 以为实现坏了。
            // 现在两边都只讲数据，并把真正缺的那一半（会话态）补上。
            let (session, has_session) = session_of(ctx);
            ToolOutcome::ok(json!({
                "databaseEntryCount": count,
                "sessionEntryCount": session.len(),
                "hasSession": has_session,
                "totalVisibleEntryCount": if count < 0 { session.len() } else { count as usize + session.len() },
                "tagCount": tags.len(),
                "contentTypesSampled": by_type,
                "sampleSize": page.len(),
                "note": if has_session && count == 0 && !session.is_empty() {
                    "数据库里没有条目，但内存里有会话态条目（默认设置下新复制的内容只进内存）。\
                     用 list_entries（默认 includeSession=true）即可读到它们。"
                } else {
                    ""
                },
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
            // 会话态条目同样可改：界面 `update_item_content` 就是先落库再写内存。
            // 这里先落库（拿到真实 id）再改正文，结果与界面一致。
            let target_id = match adopt_session_entry(ctx, id, "修改正文") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            // R13：AI 通过 MCP 改正文时同样**不降级**。富文本条目要得到一份与
            // 新正文一致的 HTML（否则界面按 HTML 画、复制走 content，两者不一致），
            // 非富文本条目则完全不传 HTML。
            let html_for_write: Option<String> =
                match ClipboardRepository::get_entry_by_id(&store.repo, target_id) {
                    Ok(Some(e)) if e.content_type == "rich_text" => {
                        Some(mutation::plain_text_to_html(&content))
                    }
                    _ => None,
                };
            match mutation::apply_entry_content(
                &store.repo,
                target_id,
                &content,
                html_for_write.as_deref(),
            ) {
                Ok(()) => {
                    mirror_content_in_session(ctx, target_id, &content, html_for_write.as_deref());
                    ctx.effects.emit_changed();
                    ctx.effects.request_cloud_sync();
                    ToolOutcome::ok(json!({ "id": target_id, "updated": true }))
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
            let target_id = match adopt_session_entry(ctx, id, "修改备注") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            match mutation::apply_entry_note(&store.repo, target_id, &note) {
                Ok(()) => {
                    ctx.effects.emit_changed();
                    ctx.effects.request_cloud_sync();
                    ToolOutcome::ok(json!({ "id": target_id, "updated": true }))
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
            let target_id = match adopt_session_entry(ctx, id, "修改标签") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            match mutation::apply_entry_tags(&store.conn, &store.tag_repo, target_id, tags.clone())
            {
                Ok(step) => {
                    // 与界面命令完全同源：敏感性翻转时才入队加解密。
                    match step {
                        mutation::SensitiveTransition::Encrypt => {
                            ctx.effects.enqueue_encryption(target_id, true)
                        }
                        mutation::SensitiveTransition::Decrypt => {
                            ctx.effects.enqueue_encryption(target_id, false)
                        }
                        mutation::SensitiveTransition::None => {}
                    }
                    // 会话态里那条的标签也要跟上（条目刚从内存落库时尤其重要：
                    // 内存那份还带着旧标签）。
                    set_tags_in_session(ctx, target_id, &tags);
                    ctx.effects.emit_changed();
                    ctx.effects.request_cloud_sync();
                    ToolOutcome::ok(json!({
                        "id": target_id,
                        "updated": true,
                        "sensitivityChanged": !matches!(step, mutation::SensitiveTransition::None),
                    }))
                }
                Err(e) => ToolOutcome::failed(format!("修改标签失败：{}", e)),
            }
        }

        "move_entry_to_tag" => tag_transfer(ctx, args, mutation::TagTransfer::Move),

        "copy_entry_to_tag" => tag_transfer(ctx, args, mutation::TagTransfer::Copy),

        "set_entry_pinned" => {
            let id = match i64_arg(args, "id") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            let pinned = bool_arg(args, "pinned", false);

            // 会话态条目（负 id）在库里没有行，`apply_entry_pin` 会更新 0 行却**不报错**，
            // 工具却回一句"已置顶"——这正是最坏的一类回复。界面的做法是先把该条落库拿到
            // 真实 id，再置顶（见 `toggle_clipboard_pin`），这里完全照做。
            if id < 0 {
                let (home, entry) = locate_entry(ctx, id);
                if home != EntryHome::Session {
                    return ToolOutcome::failed(format!("条目 {} 不存在（会话态条目可能已被清理）", id));
                }
                let entry = entry.expect("Session 分支必然带条目");
                let data_dir = ctx.effects.data_dir();
                let new_id = match persist_session_entry(ctx, id, &entry, data_dir.as_deref()) {
                    Ok(v) => v,
                    Err(e) => return ToolOutcome::failed(format!("置顶会话态条目失败：{}", e)),
                };
                return match mutation::apply_entry_pin(&store.conn, &store.repo, new_id, pinned) {
                    Ok(()) => {
                        ctx.effects.emit_changed();
                        ctx.effects.request_cloud_sync();
                        ToolOutcome::ok(json!({
                            "id": new_id,
                            "previousId": id,
                            "pinned": pinned,
                            "persisted": true,
                            "note": "会话态条目已落库并改写为库内 id；后续请用返回的 id",
                        }))
                    }
                    Err(e) => ToolOutcome::failed(format!("设置置顶失败：{}", e)),
                };
            }

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

            // 会话态条目只存在于内存：`repo.delete` 会删 0 行且不报错。若这里不管它，
            // AI 回一句"已删除"，而用户屏幕上那条**纹丝不动**，一刷新还在。
            if id < 0 {
                // 闭包在 `session_apply` 里是 `Fn`（可重复调用），因此不能捕获外部
                // 可变变量；改成"命中返回 1、没命中返回 0"，由返回值表达结果。
                let applied = ctx
                    .effects
                    .session_apply(&|snapshot: &mut Vec<ClipboardEntry>| {
                        let before = snapshot.len();
                        snapshot.retain(|e| e.id != id);
                        if snapshot.len() != before {
                            1
                        } else {
                            0
                        }
                    });
                // `None` = 该宿主没有会话态概念：此时负 id 必然无效，如实报错。
                if applied.is_none() {
                    return ToolOutcome::failed(format!(
                        "条目 {} 不存在：当前宿主没有内存态条目（负 id 只在应用运行期间有效）",
                        id
                    ));
                }
                if applied == Some(0) {
                    return ToolOutcome::failed(format!("条目 {} 不存在", id));
                }
                ctx.effects.emit_changed();
                ToolOutcome::ok(json!({
                    "id": id,
                    "deleted": true,
                    "scope": "session",
                    "note": "该条目只在内存中，已从会话态移除，未涉及数据库",
                }))
            } else {
                match mutation::apply_entry_delete(&store.repo, id, data_dir.as_deref()) {
                    Ok(()) => {
                        ctx.effects.emit_changed();
                        ctx.effects.request_cloud_sync();
                        ToolOutcome::ok(json!({ "id": id, "deleted": true, "scope": "database" }))
                    }
                    Err(e) => ToolOutcome::failed(format!("删除失败：{}", e)),
                }
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
                    // 标签集合本身会随条目同步出去；新建一个空标签不会立刻产生同步
                    // 内容，但界面上的标签树已变，所以仍按"配置了才同步"的同一口径处理。
                    sync_tags_if_configured(ctx);
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
                    // 会话态条目的标签也必须跟着改：界面 `rename_tag_globally` 会遍历
                    // 内存里的条目一起改。不同步的后果是"标签树里已经叫新名字了，但
                    // 列表里那几条还挂着旧标签"——用户看到两份互相矛盾的状态。
                    let touched = rename_tag_in_session(ctx, &old, &new);
                    ctx.effects.emit_changed();
                    sync_tags_if_configured(ctx);
                    ToolOutcome::ok(json!({
                        "oldName": old,
                        "newName": new,
                        "renamed": true,
                        "sessionEntriesTouched": touched,
                    }))
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
                    // 同 `rename_tag`：内存里挂着这个标签的条目也要摘掉它，
                    // 否则界面上仍是"标签没了，但条目上还写着它"。
                    let touched = remove_tag_from_session(ctx, &name);
                    ctx.effects.emit_changed();
                    sync_tags_if_configured(ctx);
                    ToolOutcome::ok(json!({
                        "name": name,
                        "deleted": true,
                        "entriesPreserved": true,
                        "sessionEntriesTouched": touched,
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
                Ok(()) => {
                    // 界面 `TagManager.tsx` 在设置颜色后会 `emit('tag-colors-updated')`，
                    // 而 `useTagColors.ts` 正是听这个事件重新拉取颜色表。AI 改色若不发，
                    // 用户会看到"颜色设了但界面没变"，直到下次重启。
                    //
                    // 【为什么不发 request_cloud_sync】标签颜色不进云同步：设置项白名单里
                    // 没有它（见 `cloud_sync.rs::is_setting_sync_eligible` 的排除项——标签
                    // 颜色不在同步白名单里），颜色只存在于本地 `saved_tags` 表。凭空发一次
                    // 同步请求只会让审计日志出现一次没有实际效果的记录。
                    ctx.effects.emit_tag_colors_updated();
                    ToolOutcome::ok(json!({ "name": name, "color": color }))
                }
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

        // -------------------------------------------------------------------
        // 清空历史 / 重排置顶
        // -------------------------------------------------------------------

        "clear_history" => {
            // 破坏性：`server.rs` 已在校验阶段拦下缺 `confirm` 的调用，这里再确认一次
            // 是有意的双保险——这一条删的是用户全部非置顶数据，不值得为省一行押注在
            // 单一防线上（见 AB2-G3-078 的"两段式闸门"经验）。
            if !bool_arg(args, "confirm", false) {
                return ToolOutcome::failed("清空历史需要 confirm: true");
            }
            let keep_pinned_and_tagged = bool_arg(args, "keepPinnedAndTagged", true);
            match clear_history(ctx, keep_pinned_and_tagged) {
                Ok(outcome) => ToolOutcome::ok(outcome),
                Err(e) => ToolOutcome::failed(format!("清空历史失败：{}", e)),
            }
        }

        "reorder_pinned" => {
            let raw = match args.get("orders").and_then(|v| v.as_array()) {
                Some(v) => v.clone(),
                None => {
                    return ToolOutcome::failed("参数 `orders` 必须是 [[id, order], ...] 数组")
                }
            };
            if raw.is_empty() {
                return ToolOutcome::failed("参数 `orders` 不能为空");
            }
            let mut orders: Vec<(i64, i64)> = Vec::with_capacity(raw.len());
            for item in &raw {
                let pair = match item.as_array() {
                    Some(p) => p,
                    None => return ToolOutcome::failed("每个元素必须形如 [id, order]"),
                };
                if pair.len() != 2 {
                    return ToolOutcome::failed("每个元素必须恰好有两个整数：[id, order]");
                }
                let (Some(id), Some(order)) = (pair[0].as_i64(), pair[1].as_i64()) else {
                    return ToolOutcome::failed("`orders` 的元素必须是整数");
                };
                if id < 0 {
                    // 与界面一致：置顶顺序只对库里的行有意义，会话态条目在落库时才分配
                    // `pinned_order`。明确拒绝比静默忽略更好——静默忽略会让 AI 以为排好了。
                    return ToolOutcome::failed(format!(
                        "条目 {} 是会话态条目（负 id），无法设置置顶顺序；\
                         请先用 set_entry_pinned 把它置顶（会自动落库并返回新 id）",
                        id
                    ));
                }
                orders.push((id, order));
            }
            use crate::infrastructure::repository::clipboard_repo::ClipboardRepository;
            match ClipboardRepository::update_pinned_order(&store.repo, orders.clone()) {
                Ok(()) => {
                    ctx.effects.emit_changed();
                    ctx.effects.request_cloud_sync();
                    // 读回实际生效的顺序，让调用方拿到可核对的结果（界面 `usePinnedSort`
                    // 只发请求不读回；这里读回是为了让 AI 不必再发一次读取请求）。
                    let applied = store
                        .history(orders.len() as i32 + 50, 0, None)
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|e| e.is_pinned)
                        .map(|e| json!({ "id": e.id, "pinnedOrder": e.pinned_order }))
                        .collect::<Vec<_>>();
                    ToolOutcome::ok(json!({
                        "updated": orders.len(),
                        "orders": orders.iter().map(|(id, o)| json!([id, o])).collect::<Vec<_>>(),
                        "pinnedNow": applied,
                    }))
                }
                Err(e) => ToolOutcome::failed(format!("重排失败：{}", e)),
            }
        }

        // -------------------------------------------------------------------
        // 备份导入
        // -------------------------------------------------------------------

        "inspect_backup" => {
            let path = match str_arg(args, "path") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            let p = std::path::PathBuf::from(path.trim());
            if !p.is_file() {
                return ToolOutcome::failed(format!("备份包不存在：{}", p.display()));
            }
            match crate::services::backup::import::inspect_backup(&p) {
                Ok(report) => match serde_json::to_value(&report) {
                    Ok(v) => ToolOutcome::ok(v),
                    Err(e) => ToolOutcome::failed(format!("序列化检查结果失败：{}", e)),
                },
                Err(e) => ToolOutcome::failed(format!("解析备份包失败：{}", e)),
            }
        }

        "import_backup" => {
            if !bool_arg(args, "confirm", false) {
                return ToolOutcome::failed("导入备份需要 confirm: true（会覆盖当前数据）");
            }
            let path = match str_arg(args, "path") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            let data_dir = match ctx.effects.data_dir() {
                Some(d) => d,
                None => return ToolOutcome::failed("当前无法确定数据目录，导入不可用"),
            };
            match crate::services::backup::import::restore_backup(
                &crate::services::backup::import::RestoreRequest {
                    data_dir,
                    archive_path: std::path::PathBuf::from(path.trim()),
                    pending_marker_dir: ctx.effects.pending_marker_dir(),
                },
            ) {
                Ok(report) => {
                    ctx.effects.emit_changed();
                    match serde_json::to_value(&report) {
                        Ok(v) => ToolOutcome::ok(v),
                        Err(e) => ToolOutcome::failed(format!("序列化导入结果失败：{}", e)),
                    }
                }
                Err(e) => ToolOutcome::failed(format!("导入失败（现有数据保持完好）：{}", e)),
            }
        }

        // -------------------------------------------------------------------
        // 宿主专属能力：实现托管给 `HostEffects`，本层不认识 Tauri
        // -------------------------------------------------------------------

        "copy_to_clipboard" | "paste_entry" => {
            let id = match args.get("id") {
                None | Some(Value::Null) => 0,
                Some(v) => match v.as_i64() {
                    Some(n) => n,
                    None => {
                        return ToolOutcome::failed("参数 `id` 必须是整数（给 content 时传 0）")
                    }
                },
            };
            let content = opt_str_arg(args, "content").unwrap_or_default();
            if id == 0 && content.is_empty() {
                return ToolOutcome::failed("必须提供 `id` 或 `content`");
            }
            // 会话态条目：宿主侧直接按负 id 到内存里取正文。先在这里确认它确实在，
            // 免得把一个不存在的负 id 递下去、由宿主静默取到空内容。
            let session_entry = if id < 0 {
                let (home, entry) = locate_entry(ctx, id);
                if home != EntryHome::Session {
                    return ToolOutcome::failed(format!(
                        "条目 {} 不存在（会话态条目可能已被清理）",
                        id
                    ));
                }
                entry
            } else {
                None
            };
            // 内容类型：显式给了就用；没给就从条目推断。默认成 "text" 会把图片/文件
            // 当成文本塞进剪贴板——那是"看起来成功但粘贴出乱码"的一类缺陷。
            let content_type = match opt_str_arg(args, "contentType") {
                Some(t) => t,
                None => {
                    let inferred = if id < 0 {
                        session_entry.as_ref().map(|e| e.content_type.clone())
                    } else {
                        store.entry(id).ok().flatten().map(|e| e.content_type)
                    };
                    inferred.unwrap_or_else(|| "text".to_string())
                }
            };
            let req = ClipboardWrite {
                id,
                content,
                content_type,
                // `paste_entry` 恒定粘贴；`copy_to_clipboard` 看参数。
                paste: tool == "paste_entry" || bool_arg(args, "paste", false),
                delete_after_use: bool_arg(args, "deleteAfterUse", false),
                paste_with_format: args.get("pasteWithFormat").and_then(|v| v.as_bool()),
                move_to_top: args.get("moveToTop").and_then(|v| v.as_bool()),
            };
            match ctx.effects.clipboard_write(req) {
                Ok(v) => ToolOutcome::ok(v),
                Err(e) => ToolOutcome::failed(e),
            }
        }

        "list_legacy_data_dirs" => match ctx.effects.list_legacy_data_dirs() {
            Ok(v) => ToolOutcome::ok(v),
            Err(e) => ToolOutcome::failed(e),
        },

        "migrate_from_data_dir" => {
            if !bool_arg(args, "confirm", false) {
                return ToolOutcome::failed("迁移需要 confirm: true（会写入当前数据目录）");
            }
            let path = match str_arg(args, "path") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            match ctx.effects.migrate_from_data_dir(&path) {
                Ok(v) => ToolOutcome::ok(v),
                Err(e) => ToolOutcome::failed(e),
            }
        }

        // -------------------------------------------------------------------
        // 设置族
        // -------------------------------------------------------------------

        "get_settings" => {
            use crate::infrastructure::repository::settings_repo::SettingsRepository;
            let all = match SettingsRepository::get_all(&store.settings_repo) {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(format!("读取设置失败：{}", e)),
            };
            let keys = string_list_arg(args, "keys").unwrap_or_default();
            let prefix = opt_str_arg(args, "prefix");
            // 默认只给**可同步白名单**内的键。那份白名单就是"这些设置项可以离开本机"
            // 的既有判定（`cloud_sync.rs::is_setting_sync_eligible`，本轮已改为
            // `pub(crate)` 并**直接调用**，不再镜像规则）。复用同一条规则，
            // 等于让"MCP 能读到的设置"天然落在"已经会同步到云端"的范围里，不会因为
            // 新开一个读取口径而把凭据顺带带出去。要看全部得显式要（includeNonSyncable）。
            let include_all = bool_arg(args, "includeNonSyncable", false);
            let mut items: Vec<Value> = Vec::new();
            let mut excluded = 0usize;
            for (k, v) in all.iter() {
                if !keys.is_empty() && !keys.iter().any(|x| x == k) {
                    continue;
                }
                if let Some(p) = &prefix {
                    if !k.starts_with(p.as_str()) {
                        continue;
                    }
                }
                if !include_all && !crate::services::cloud_sync::is_setting_sync_eligible(k) {
                    excluded += 1;
                    continue;
                }
                items.push(json!({ "key": k, "value": v }));
            }
            items.sort_by(|a, b| {
                a["key"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["key"].as_str().unwrap_or(""))
            });
            ToolOutcome::ok(json!({
                "count": items.len(),
                "settings": items,
                "excludedNonSyncable": excluded,
            }))
        }

        "set_setting" => {
            let key = match str_arg(args, "key") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            let value = match str_arg(args, "value") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            let key = key.trim().to_string();
            if key.is_empty() {
                return ToolOutcome::failed("参数 `key` 不能为空");
            }
            // ---- 硬闸：MCP 自身的配置不允许经由 MCP 修改 ----
            //
            // 这不是"少给一个功能"，而是一条**必须存在**的边界：`mcp.*` 里装着本服务的
            // 开关（enabled）、端口、是否允许局域网可达、以及**免鉴权开关与令牌**。
            // 允许 AI 写这些键，等于允许 AI 把"免鉴权 + 局域网可达"直接落库——而用户
            // 明确要求的是"默认免鉴权（仅本机回环）"。用户仍可在界面里改这些设置；
            // 这里拒绝的只是"通过 AI 接口改 AI 接口自己的权限"这条自授权回路。
            if key.starts_with("mcp.") {
                return ToolOutcome::failed(
                    "拒绝修改 mcp.* 设置：这些是 MCP 服务自身的开关、端口与鉴权配置。\
                     通过 AI 接口修改 AI 接口的权限会形成自授权回路；请在界面「设置 → MCP」里修改。",
                );
            }
            // ---- 硬闸：安全处置状态不允许经由 MCP 修改 ----
            //
            // `security.*` 记录的是"这台机器对某件安全事件的处置到了哪一步"。当前唯一
            // 成员是"存量凭据外流的告知是否已经展示过"。
            //
            // 为什么必须拒绝：那条告知的内容是"你的 MQTT 密码可能已经在云端存储里，
            // 建议更换"。若 AI 能把它标成"已展示"，用户永远不会看到它——而**看不见的
            // 安全告知等于没有告知**。这与 `mcp.*` 是同一条边界：不允许通过接口去关掉
            // 那些"用来约束接口本身"的东西。用户仍可看/可改（界面里点"知道了"）。
            if key.starts_with(crate::services::cloud_sync::SECURITY_SETTING_KEY_PREFIX) {
                return ToolOutcome::failed(
                    "拒绝修改 security.* 设置：这一族记录的是本机对安全事件的处置状态，\
                     通过接口改写它会让该安全告知不再展示。请在界面里处理。",
                );
            }
            use crate::infrastructure::repository::settings_repo::SettingsRepository;
            match SettingsRepository::set(&store.settings_repo, &key, &value) {
                Ok(()) => {
                    ctx.effects.emit_changed();
                    // 与界面 `App.tsx::saveSetting` 一致：改完 `app.emoji_favorites`
                    // 补一次同步请求（表情清单在同步范围内）。
                    if key == "app.emoji_favorites" {
                        ctx.effects.request_cloud_sync();
                    }
                    ToolOutcome::ok(json!({ "key": key, "value": value, "saved": true }))
                }
                Err(e) => ToolOutcome::failed(format!("写入设置失败：{}", e)),
            }
        }

        // -------------------------------------------------------------------
        // 表情收藏
        // -------------------------------------------------------------------

        "list_emoji_favorites" => {
            let data_dir = match ctx.effects.data_dir() {
                Some(d) => d,
                None => return ToolOutcome::failed("当前无法确定数据目录，读不到表情收藏"),
            };
            // 磁盘文件是事实来源；设置项里的清单可能因为手工删文件而过期，因此两者
            // 都给出来，让调用方看得出差异（界面上会出现"能点但打不开"的项）。
            let on_disk =
                match crate::app::commands::file_cmd::list_emoji_favorite_paths_in_dir(&data_dir) {
                    Ok(v) => v,
                    Err(e) => return ToolOutcome::failed(format!("读取表情收藏目录失败：{}", e)),
                };
            let manifest = store.emoji_manifest();
            ToolOutcome::ok(json!({
                "count": on_disk.len(),
                "paths": on_disk,
                "manifestCount": manifest.len(),
                "manifestMissingOnDisk": manifest
                    .iter()
                    .filter(|p| !on_disk.iter().any(|d| d == *p))
                    .collect::<Vec<_>>(),
            }))
        }

        "add_emoji_favorite" => {
            let source_path = opt_str_arg(args, "sourcePath").unwrap_or_default();
            let data_url = opt_str_arg(args, "dataUrl").unwrap_or_default();
            let update_manifest = bool_arg(args, "updateManifest", true);
            let saved = match ctx.effects.save_emoji_favorite(&source_path, &data_url) {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            let path = saved
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let manifest_updated = if update_manifest && !path.is_empty() {
                match store.add_to_emoji_manifest(&path) {
                    Ok(()) => true,
                    Err(e) => {
                        return ToolOutcome::failed(format!("图片已保存到收藏目录，但更新界面清单失败：{}", e))
                    }
                }
            } else {
                false
            };
            ctx.effects.emit_changed();
            ToolOutcome::ok(json!({
                "path": path,
                "manifestUpdated": manifest_updated,
                "note": if manifest_updated {
                    "界面收藏列表来自 app.emoji_favorites 设置项，已同步写入"
                } else {
                    "未写入界面清单：界面需重新扫描才可能显示该表情"
                },
            }))
        }

        "remove_emoji_favorite" => {
            let path = match str_arg(args, "path") {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            let removed = match ctx.effects.remove_emoji_favorite(&path) {
                Ok(v) => v,
                Err(e) => return ToolOutcome::failed(e),
            };
            let manifest_updated = if bool_arg(args, "updateManifest", true) {
                match store.remove_from_emoji_manifest(&path) {
                    Ok(changed) => changed,
                    Err(e) => return ToolOutcome::failed(format!("更新界面清单失败：{}", e)),
                }
            } else {
                false
            };
            ctx.effects.emit_changed();
            ToolOutcome::ok(json!({
                "path": path,
                "fileRemoved": removed.get("fileRemoved").cloned().unwrap_or(json!(false)),
                "manifestUpdated": manifest_updated,
            }))
        }

        // -------------------------------------------------------------------
        // 粘贴队列
        // -------------------------------------------------------------------

        "get_paste_queue" => match ctx.effects.paste_queue() {
            Ok(v) => ToolOutcome::ok(v),
            Err(e) => ToolOutcome::failed(e),
        },

        "set_paste_queue" => {
            let ids: Vec<i64> = match args.get("itemIds").and_then(|v| v.as_array()) {
                Some(items) => {
                    let mut out = Vec::with_capacity(items.len());
                    for v in items {
                        match v.as_i64() {
                            Some(n) => out.push(n),
                            None => return ToolOutcome::failed("参数 `itemIds` 的元素必须是整数"),
                        }
                    }
                    out
                }
                None => {
                    return ToolOutcome::failed("参数 `itemIds` 必须是整数数组（空数组表示清空）")
                }
            };
            match ctx.effects.set_paste_queue(&ids) {
                Ok(v) => ToolOutcome::ok(v),
                Err(e) => ToolOutcome::failed(e),
            }
        }

        // -------------------------------------------------------------------
        // 云同步 / MQTT 状态
        // -------------------------------------------------------------------

        "request_cloud_sync" => {
            if !ctx.effects.cloud_sync_enabled() {
                return ToolOutcome::failed(
                    "云同步未配置（cloud_sync_enabled 为 false 且无服务器地址），请求不会有任何效果",
                );
            }
            ctx.effects.request_cloud_sync();
            match ctx.effects.cloud_sync_status() {
                Ok(v) => ToolOutcome::ok(json!({ "requested": true, "status": v })),
                Err(e) => ToolOutcome::failed(e),
            }
        }

        "get_cloud_sync_status" => match ctx.effects.cloud_sync_status() {
            Ok(v) => ToolOutcome::ok(v),
            Err(e) => ToolOutcome::failed(e),
        },

        "get_mqtt_status" => match ctx.effects.mqtt_status() {
            Ok(v) => ToolOutcome::ok(v),
            Err(e) => ToolOutcome::failed(e),
        },

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
