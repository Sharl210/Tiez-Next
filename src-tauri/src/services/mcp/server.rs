//! MCP Streamable HTTP 端点。
//!
//! # 传输选择
//!
//! MCP 规范提供 stdio 与 Streamable HTTP 两种传输。本项目**只能用进程内 HTTP**：
//! `main.rs` 装了 `tauri-plugin-single-instance`，stdio 会被单实例插件与宿主进程
//! 的父子关系搅在一起，客户端拿不到稳定通道。因此这里实现 Streamable HTTP。
//!
//! 监听地址由 `mcp.allow_lan` 决定：默认 `false` 只绑 `127.0.0.1`；显式打开后
//! 绑 `0.0.0.0`，同网段其它机器即可访问（见 [`bind_listener`]）。
//!
//! # 规范落点
//!
//! 依据 MCP 规范 2025-06-18（`basic/lifecycle`、`basic/transports`、`server/tools`）：
//!
//! * 一个端点上同时支持 POST 与 GET；
//! * POST 必须是**单条** JSON-RPC 消息；请求回 `application/json`，通知回
//!   `202 Accepted` 且无 body；
//! * GET 不接受 SSE 流时**必须**回 `405 Method Not Allowed`（而不是空 200）；
//! * 服务端**必须**校验 `Origin` 头（DNS rebinding 防护）；
//! * `protocolVersion` 协商：客户端版本受支持就回同一个，否则回服务端最新支持版本；
//! * 工具结果放 `result.content`，执行错误放 `result.isError = true`，协议错误才走
//!   JSON-RPC `error`。
//!
//! # 明确不支持（并在 `initialize` 中如实反映）
//!
//! * **SSE 流**：不声明 `resources`/`prompts`/`logging`，也不实现 GET 流。通知
//!   （如 `notifications/initialized`）已被无状态接受，因此不需要流也能工作。
//! * **`Mcp-Session-Id`**：服务端可以选择不分配会话 id（规范中为 MAY）。本服务
//!   不在响应里返回该头，客户端也就不会带上它；若客户端仍然发来该头，会被忽略
//!   （规范要求返回 404 的情形只适用于"服务端已终止该会话"）。
//! * **SSE 断线重放 / `Last-Event-ID`**：因为没有 SSE 流。

use std::net::Ipv4Addr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use serde_json::{json, Value};

use super::jsonrpc::{
    self, RpcError, Response as RpcResponse, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND,
};
use super::store::McpStore;
use super::tools::{self, Access, Ctx, ToolOutcome};

/// 服务端实现的 MCP 协议版本。
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// 服务端版本号，会出现在 `serverInfo.version` 里。
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 服务名，会出现在 `serverInfo.name` 里。
pub const SERVER_NAME: &str = "tiez-next";

/// 静态鉴权令牌，附带一个**运行时**的"是否强制校验"开关。
///
/// `require_token = false`（出厂默认）表示免鉴权模式：任何请求都通过。
///
/// # 为什么免鉴权是默认值，而不是把令牌功能删掉
///
/// 出厂默认是"只监听回环 + 免鉴权"，也就是本机上任何进程都能连——这是用户明确
/// 要求的开箱体验。把令牌保留成可选项（而不是删代码）的理由是：用户一旦打开
/// `mcp.allow_lan`，就**必须**有办法立刻把校验加回来，否则局域网暴露没有任何
/// 补救手段。因此令牌的生成、展示、重生成能力全部保留。
///
/// 开关用原子量而不是普通字段：用户打开强制校验时应当**立刻**生效，不必先把
/// 服务停掉再启动——正在被外部机器连着的服务恰恰是最需要马上收紧的时刻。
#[derive(Clone)]
pub struct AuthToken {
    secret: Arc<String>,
    require_token: Arc<std::sync::atomic::AtomicBool>,
}

impl AuthToken {
    /// 按开关构造。`require_token = false` 得到免鉴权实例。
    pub fn new(secret: Arc<String>, require_token: bool) -> Self {
        Self {
            secret,
            require_token: Arc::new(std::sync::atomic::AtomicBool::new(require_token)),
        }
    }

    /// 构造一个"必须校验"的令牌（最严格的用法，测试与内部工具使用）。
    pub fn required(secret: Arc<String>) -> Self {
        Self::new(secret, true)
    }

    /// 免鉴权实例（默认姿态与测试使用）。
    pub fn disabled() -> Self {
        Self::new(Arc::new(String::new()), false)
    }

    /// 当前是否强制校验。
    pub fn requires_token(&self) -> bool {
        self.require_token.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 运行时切换强制校验。
    pub fn set_require_token(&self, value: bool) {
        self.require_token
            .store(value, std::sync::atomic::Ordering::SeqCst);
    }

    /// 令牌明文。免鉴权模式下它依然存在，用户随时可以打开校验而不必重新生成。
    pub fn secret(&self) -> &str {
        self.secret.as_str()
    }

    /// 校验请求携带的令牌。
    ///
    /// 校验关闭时直接放行；开启时三种失败分开回报，便于用户一眼看出是"没配"
    /// 还是"配错"：未配置、缺令牌、令牌不匹配。
    pub fn check(&self, provided: Option<&str>) -> Result<(), String> {
        if !self.requires_token() {
            return Ok(());
        }
        if self.secret.is_empty() {
            return Err("服务端未配置访问令牌，服务不可用".to_string());
        }
        match provided {
            None => Err("缺少访问令牌".to_string()),
            Some(value) if !constant_time_eq(value.as_bytes(), self.secret.as_bytes()) => {
                Err("访问令牌不正确".to_string())
            }
            Some(_) => Ok(()),
        }
    }
}

/// 定长比较，避免按字符提前返回带来的时序差。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 运行时开关。
pub struct RuntimeSettings {
    /// 写操作是否被允许。
    pub allow_write: std::sync::atomic::AtomicBool,
}

impl RuntimeSettings {
    pub fn new(allow_write: bool) -> Self {
        Self {
            allow_write: std::sync::atomic::AtomicBool::new(allow_write),
        }
    }

    pub fn allow_write(&self) -> bool {
        self.allow_write.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn set_allow_write(&self, value: bool) {
        self.allow_write
            .store(value, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 审计日志接收者。
///
/// MCP 让 AI 改用户数据，这属于"必须留痕"的操作。日志交给宿主（Tauri）实现，
/// 因为宿主才知道项目的 logger 落在哪个文件；MCP 核心只声明"我要记一条"。
pub trait AuditSink: Send + Sync {
    /// `tool` 为 `None` 表示这条记录的是协议层事件（初始化 / 列出工具）。
    fn record(&self, tool: Option<&str>, outcome: &str, detail: &str);
}

/// 什么都不做的审计实现，供测试使用。
pub struct NoopAudit;

impl AuditSink for NoopAudit {
    fn record(&self, _tool: Option<&str>, _outcome: &str, _detail: &str) {}
}

/// 服务共享状态。
#[derive(Clone)]
pub struct ServerState {
    pub store: McpStore,
    pub token: AuthToken,
    pub settings: Arc<RuntimeSettings>,
    pub audit: Arc<dyn AuditSink>,
    pub effects: Arc<dyn tools::HostEffects>,
}

/// 解析 `Origin` 头。
///
/// 规范要求**必须**校验 `Origin`：没有这一步，任意网页都能通过 DNS rebinding
/// 访问本机服务。允许集收得很紧——只有 MCP 客户端常见的本地/无浏览器来源：
///
/// * 没有 `Origin`：原生客户端（`mcp-remote`、IDE 插件等）通常不发该头，放行；
/// * `http://127.0.0.1[:port]`、`http://localhost[:port]`、`http://[::1][:port]`：放行；
/// * 其它一切（尤其 `https://*` 公网来源、`null`）：拒绝。
pub fn origin_allowed(origin: Option<&str>) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    let lower = origin.trim().to_ascii_lowercase();
    // `null` 来自 sandbox iframe / data: URL，是最典型的滥用来源。
    if lower == "null" || lower.is_empty() {
        return false;
    }
    for scheme in ["http://", "https://"] {
        for host in ["127.0.0.1", "localhost", "[::1]"] {
            let prefix = format!("{}{}", scheme, host);
            if lower == prefix {
                return true;
            }
            if let Some(rest) = lower.strip_prefix(&prefix) {
                // 只接受 ":port"，不接受 ".evil.com" 这种后缀伪造。
                if rest.starts_with(':') && rest[1..].chars().all(|c| c.is_ascii_digit()) {
                    return true;
                }
            }
        }
    }
    false
}

fn json_response(status: StatusCode, body: Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

fn rpc_response(status: StatusCode, response: RpcResponse) -> Response {
    json_response(status, serde_json::to_value(response).unwrap_or(Value::Null))
}

/// 组装路由。
pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/mcp", any(mcp_endpoint))
        // 同一个 handler 挂在两个路径上，方便用户按习惯配置。
        .route("/", any(mcp_endpoint))
        .with_state(state)
}

async fn mcp_endpoint(
    State(state): State<ServerState>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match method {
        Method::POST => handle_post(state, headers, body).await,
        Method::GET => handle_get(&state, &headers),
        Method::DELETE => handle_delete(&state, &headers),
        // 规范只定义 POST/GET/(DELETE) 三种；其余一律 405。
        _ => (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, "GET, POST, DELETE")],
        )
            .into_response(),
    }
}

/// 交给 POST 处理前的公共校验：Origin、鉴权。
///
/// 校验失败时：
/// * Origin 不合法 → `403 Forbidden`（规范强调这是防 DNS rebinding 的硬要求）；
/// * 鉴权失败 → `401 Unauthorized`，并在 body 里给出可读原因。
fn preflight(state: &ServerState, headers: &HeaderMap) -> Result<(), Response> {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    if !origin_allowed(origin) {
        state
            .audit
            .record(None, "rejected", &format!("Origin 不被允许：{:?}", origin));
        return Err((
            StatusCode::FORBIDDEN,
            "Origin not allowed: MCP server only accepts local clients",
        )
            .into_response());
    }

    let provided = headers.get("x-mcp-token").and_then(|v| v.to_str().ok());
    if let Err(reason) = state.token.check(provided) {
        state
            .audit
            .record(None, "rejected", &format!("鉴权失败：{}", reason));
        return Err(json_response(StatusCode::UNAUTHORIZED, json!({ "error": reason })));
    }
    Ok(())
}

async fn handle_post(state: ServerState, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(rejection) = preflight(&state, &headers) {
        return rejection;
    }
    if body.is_empty() {
        return rpc_response(
            StatusCode::OK,
            RpcResponse::failure(Value::Null, RpcError::new(INVALID_REQUEST, "请求体为空")),
        );
    }

    let request = match jsonrpc::parse_request(&body) {
        Ok(req) => req,
        Err(err) => {
            // 解析失败时拿不到 id，按规范用 `id: null`。
            return rpc_response(StatusCode::OK, RpcResponse::failure(Value::Null, err));
        }
    };

    // 通知不得有响应：规范要求 `202 Accepted` + 空体。这里对未知方法同样只记审计，
    // 因为"回一条 405/JSON-RPC 错误"反而违反通知语义。
    if request.is_notification() {
        state.audit.record(
            None,
            "notification",
            &format!("收到通知 {}", request.method),
        );
        return StatusCode::ACCEPTED.into_response();
    }

    let id = request.id.clone().unwrap_or(Value::Null);
    let response = dispatch(&state, &request.method, request.params.as_ref(), id.clone());

    if let Some(error) = &response.error {
        state.audit.record(
            if request.method == "tools/call" {
                request
                    .params
                    .as_ref()
                    .and_then(|p| p.get("name"))
                    .and_then(|v| v.as_str())
            } else {
                None
            },
            "protocol_error",
            &format!("{} -> {} ({})", request.method, error.message, error.code),
        );
        let _ = id;
    }

    rpc_response(StatusCode::OK, response)
}

/// GET：本服务不提供 SSE 流，按规范回 `405 Method Not Allowed`。
fn handle_get(state: &ServerState, headers: &HeaderMap) -> Response {
    if let Err(rejection) = preflight(state, headers) {
        return rejection;
    }
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "POST, DELETE")],
    )
        .into_response()
}

/// DELETE：未启用会话管理，因此没有可终止的会话。
///
/// 规范允许在"服务器不允许客户端终止会话"时回 `405`，这里就按这个分支处理，
/// 而不是假装成功——假装成功会让客户端误以为会话已结束。
fn handle_delete(state: &ServerState, headers: &HeaderMap) -> Response {
    if let Err(rejection) = preflight(state, headers) {
        return rejection;
    }
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "POST")],
    )
        .into_response()
}

/// 把 JSON-RPC 方法分发到具体实现。
///
/// 这里刻意保持"纯函数"形态：不碰 socket，测试可以直接调用它断言协议行为。
pub fn dispatch(
    state: &ServerState,
    method: &str,
    params: Option<&Value>,
    id: Value,
) -> RpcResponse {
    match method {
        "initialize" => RpcResponse::success(id, initialize_result(params)),
        "ping" => RpcResponse::success(id, json!({})),
        "tools/list" => RpcResponse::success(id, tools_list_result(state)),
        "tools/call" => match call_tool(state, params) {
            Ok(outcome) => RpcResponse::success(id, tool_result_value(outcome)),
            Err(err) => RpcResponse::failure(id, err),
        },
        other => RpcResponse::failure(
            id,
            RpcError::new(METHOD_NOT_FOUND, format!("未实现的方法：{}", other)),
        ),
    }
}

/// `initialize` 的结果。
///
/// `protocolVersion` 协商按规范执行：客户端给的版本是本服务支持的就原样回，
/// 否则回本服务支持的最新版本（由客户端决定是否继续）。
fn initialize_result(params: Option<&Value>) -> Value {
    let requested = params
        .and_then(|p| p.get("protocolVersion"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    const SUPPORTED: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
    let negotiated = if SUPPORTED.contains(&requested) {
        requested
    } else {
        PROTOCOL_VERSION
    };

    json!({
        "protocolVersion": negotiated,
        "capabilities": {
            // 只声明 tools：本服务不提供 prompts / resources / logging，
            // 声明了却不实现会让客户端发出永远得不到响应的请求。
            "tools": { "listChanged": false }
        },
        "serverInfo": {
            "name": SERVER_NAME,
            "title": "Tiez-Next 剪贴板",
            "version": SERVER_VERSION,
        },
        "instructions": "读写本机 Tiez-Next 剪贴板的条目与标签。写操作需在应用设置里显式开启；条目正文与标签的完整内容均不做截断。"
    })
}

fn tools_list_result(state: &ServerState) -> Value {
    let allow_write = state.settings.allow_write();
    let list: Vec<Value> = tools::catalog()
        .into_iter()
        .map(|spec| {
            let mut value = json!({
                "name": spec.name,
                "title": spec.title,
                "description": spec.description,
                "inputSchema": spec.input_schema,
                "annotations": {
                    "title": spec.title,
                    "readOnlyHint": spec.access == Access::Read,
                    "destructiveHint": spec.destructive,
                    // 写工具在只读模式下的"幂等性"无从谈起：直接被拒。
                    "idempotentHint": spec.access == Access::Read,
                    "openWorldHint": false,
                },
            });
            if spec.access == Access::Write && !allow_write {
                // 让 AI 在调用前就知道写工具当前不可用，而不是调一次吃一次错。
                if let Some(map) = value.as_object_mut() {
                    map.insert(
                        "description".into(),
                        json!(format!(
                            "{}[当前只读模式，写操作被拒绝：请在 Tiez-Next 设置中开启“允许 AI 修改”]",
                            spec.description
                        )),
                    );
                }
            }
            value
        })
        .collect();
    json!({ "tools": list })
}

/// 把工具执行结果包成 MCP 的 `CallToolResult`。
///
/// 同时给 `structuredContent` 与序列化后的 `content[0].text`：规范建议结构化结果
/// 附带一份文本副本以兼容旧客户端，这里照做。
fn tool_result_value(outcome: ToolOutcome) -> Value {
    let text = serde_json::to_string_pretty(&outcome.value)
        .unwrap_or_else(|_| outcome.value.to_string());
    let mut result = json!({
        "content": [{ "type": "text", "text": text }],
        "isError": outcome.is_error,
    });
    if !outcome.is_error {
        if let Some(map) = result.as_object_mut() {
            map.insert("structuredContent".into(), outcome.value);
        }
    }
    result
}

/// 处理 `tools/call`。返回 `Err` 表示**协议级**错误（未知工具、参数不合法、
/// 写权限不足），它们按规范走 JSON-RPC error；业务失败由 `ToolOutcome.is_error`
/// 在 `result` 里表达。
fn call_tool(state: &ServerState, params: Option<&Value>) -> Result<ToolOutcome, RpcError> {
    let params = params.ok_or_else(|| RpcError::new(INVALID_PARAMS, "tools/call 缺少 params"))?;
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| RpcError::new(INVALID_PARAMS, "tools/call 缺少 name"))?;

    let spec = tools::find(name)
        .ok_or_else(|| RpcError::new(INVALID_PARAMS, format!("未知工具：{}", name)))?;

    let empty = Value::Object(serde_json::Map::new());
    let args = params.get("arguments").unwrap_or(&empty);
    if !args.is_object() {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "arguments 必须是 JSON 对象".to_string(),
        ));
    }

    // 权限校验必须在参数校验之前。
    //
    // 理由：只读模式下写工具**整体不可用**，此时报"缺少必填参数 `content`"会把
    // 调用方引向修参数，而真正要解决的是去设置里打开写权限。角色判定先于细节
    // 判定，错误信息才指向正确的下一步。
    if spec.access == Access::Write && !state.settings.allow_write() {
        let detail = format!("工具 {} 被拒绝：服务处于只读模式", name);
        state.audit.record(Some(name), "denied_readonly", &detail);
        return Err(RpcError::new(
            INVALID_PARAMS,
            format!(
                "写操作当前被禁用：请在 Tiez-Next 设置中开启“允许 AI 修改剪贴板数据”，再重试 `{}`。",
                name
            ),
        ));
    }

    if spec.destructive {
        let confirmed = args.get("confirm").and_then(|v| v.as_bool()).unwrap_or(false);
        if !confirmed {
            let detail = format!("工具 {} 被拒绝：缺少 confirm=true", name);
            state.audit.record(Some(name), "denied_unconfirmed", &detail);
            return Err(RpcError::new(
                INVALID_PARAMS,
                format!("`{}` 是破坏性操作，必须显式传 confirm=true", name),
            ));
        }
    }

    validate_args(&spec, args).map_err(|e| RpcError::new(INVALID_PARAMS, e))?;

    let ctx = Ctx {
        store: &state.store,
        effects: state.effects.as_ref(),
    };
    let outcome = tools::invoke(&ctx, name, args);

    state.audit.record(
        Some(name),
        if outcome.is_error { "error" } else { "ok" },
        &audit_detail(name, args, &outcome),
    );

    Ok(outcome)
}

/// 审计明细：写操作记下"改了什么"，读操作只记参数规模，避免把用户剪贴板正文
/// 抄进日志。
fn audit_detail(tool: &str, args: &Value, outcome: &ToolOutcome) -> String {
    if let Some(spec) = tools::find(tool) {
        if spec.access == Access::Read {
            return format!("params={}", summarize_args(args));
        }
    }
    if outcome.is_error {
        return format!(
            "params={} error={}",
            summarize_args(args),
            outcome
                .value
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
        );
    }
    format!("params={} result={}", summarize_args(args), summarize_args(&outcome.value))
}

/// 只保留结构信息（键与长度），不复制正文。
fn summarize_args(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| match v {
                    Value::String(s) => format!("{}:<{} chars>", k, s.chars().count()),
                    other => format!("{}:{}", k, other),
                })
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        other => other.to_string(),
    }
}

/// 按 `inputSchema` 做最小必要校验。
///
/// 只校验 `required` 与顶层类型：完整 JSON Schema 校验需要引入额外依赖，而这些
/// 工具的 schema 都很窄，"必填 + 类型"已经能挡掉绝大多数误用。**未校验的部分
/// 在报告里如实标注**，不假装做了全量校验。
fn validate_args(spec: &tools::ToolSpec, args: &Value) -> Result<(), String> {
    let required = spec
        .input_schema
        .get("required")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let props = spec.input_schema.get("properties").and_then(|v| v.as_object());

    for key in required {
        let Some(key) = key.as_str() else { continue };
        let schema = props.and_then(|p| p.get(key));
        // schema 明确写了 `"null"` 时，显式传 `null` 是**合法取值**而不是缺参数。
        // 例如 `set_tag_color` 的 `color`：`null` 的语义是"清除颜色"，把它当成
        // "没传"会让那条路径永远不可达。
        let null_is_allowed = schema.map(type_allows_null).unwrap_or(false);

        match args.get(key) {
            None => return Err(format!("缺少必填参数 `{}`", key)),
            Some(Value::Null) if !null_is_allowed => {
                return Err(format!("缺少必填参数 `{}`", key))
            }
            Some(_) => {}
        }
    }

    // 已声明的参数若给了值，检查顶层 JSON 类型是否与 schema 一致。
    if let Some(props) = props {
        for (key, schema) in props {
            let Some(given) = args.get(key) else { continue };
            if given.is_null() {
                // `null` 只在 schema 允许时视为类型合法；否则交给下面的类型检查
                // 报错，避免"传了 null 却被当作没传"这种静默语义。
                if type_allows_null(schema) {
                    continue;
                }
            }
            let expected = schema.get("type");
            let ok = match expected {
                Some(Value::String(t)) => type_matches(t, given),
                Some(Value::Array(types)) => types
                    .iter()
                    .filter_map(|t| t.as_str())
                    .any(|t| type_matches(t, given)),
                _ => true,
            };
            if !ok {
                return Err(format!("参数 `{}` 类型不符", key));
            }
        }
    }
    Ok(())
}

/// schema 的 `type` 是否允许 `null`（可能是 `"null"`，也可能是 `["string","null"]`）。
fn type_allows_null(schema: &Value) -> bool {
    match schema.get("type") {
        Some(Value::String(t)) => t == "null",
        Some(Value::Array(types)) => types.iter().any(|t| t.as_str() == Some("null")),
        _ => false,
    }
}

fn type_matches(expected: &str, given: &Value) -> bool {
    match expected {
        "string" => given.is_string(),
        "integer" => given.is_i64() || given.is_u64(),
        "boolean" => given.is_boolean(),
        "array" => given.is_array(),
        "object" => given.is_object(),
        "number" => given.is_number(),
        "null" => given.is_null(),
        _ => true,
    }
}

/// 按 `mcp.allow_lan` 决定监听地址。
///
/// * `false`（出厂默认）→ `127.0.0.1`：只有本机能连。
/// * `true` → `0.0.0.0`：**同网段任何机器都能连**，界面必须把这一点讲清楚。
pub fn listen_ip(allow_lan: bool) -> Ipv4Addr {
    if allow_lan {
        Ipv4Addr::UNSPECIFIED
    } else {
        Ipv4Addr::LOCALHOST
    }
}

/// 绑定监听器。
///
/// 端口策略：**固定端口 + 顺序回退**——"从首选端口向后找到第一个可用端口，全都
/// 被占则回落到系统分配的临时端口（port 0）"。绑定地址由 `allow_lan` 决定，
/// 不硬编码：默认只绑回环，用户显式打开局域网后才绑 `0.0.0.0`。
///
/// 回退只在**同一地址族**内进行：`0.0.0.0` 段被占用与"本机端口被占用"是两件
/// 事，混着试探会让用户看到与设置里填的地址不符的结果。
pub async fn bind_listener(
    preferred: u16,
    allow_lan: bool,
) -> std::io::Result<(tokio::net::TcpListener, u16)> {
    let ip = listen_ip(allow_lan);
    let mut port = if preferred == 0 {
        super::store::DEFAULT_PORT
    } else {
        preferred
    };
    loop {
        let addr = std::net::SocketAddr::from((ip, port));
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => return Ok((listener, port)),
            Err(_) if port < 65535 => port += 1,
            Err(_) => {
                let listener = tokio::net::TcpListener::bind((ip, 0u16)).await?;
                let actual = listener.local_addr()?.port();
                return Ok((listener, actual));
            }
        }
    }
}

/// 只绑本机的绑定（`allow_lan = false` 的简写）。
///
/// 保留这个名字是因为"绑本机"是一个独立、常见且安全含义明确的动作；调用方读它
/// 时不必先在心里把 `false` 翻译成"回环"。
pub async fn bind_local_listener(preferred: u16) -> std::io::Result<(tokio::net::TcpListener, u16)> {
    bind_listener(preferred, false).await
}

/// 启动 axum 服务。
pub async fn serve(listener: tokio::net::TcpListener, state: ServerState) {
    let app = router(state);
    if let Err(e) = axum::serve(listener, app).await {
        crate::error!(">>> [MCP] 服务退出：{}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::mcp::tools::NoopEffects;

    struct CollectingAudit(std::sync::Mutex<Vec<String>>);
    impl AuditSink for CollectingAudit {
        fn record(&self, tool: Option<&str>, outcome: &str, detail: &str) {
            if let Ok(mut guard) = self.0.lock() {
                guard.push(format!("{:?}|{}|{}", tool, outcome, detail));
            }
        }
    }

    /// 默认测试状态：**强制校验令牌**。
    ///
    /// 刻意让绝大多数既有用例继续跑在"必须带令牌"的姿态下：这样鉴权一旦被人
    /// 顺手关掉，受影响的用例会集中变红，而不是静默放行。
    fn test_state(allow_write: bool, token: &str) -> (ServerState, Arc<CollectingAudit>) {
        test_state_with(allow_write, token, true)
    }

    /// 显式指定"是否强制校验令牌"的构造。
    fn test_state_with(
        allow_write: bool,
        token: &str,
        require_token: bool,
    ) -> (ServerState, Arc<CollectingAudit>) {
        let audit = Arc::new(CollectingAudit(std::sync::Mutex::new(Vec::new())));
        let state = ServerState {
            store: McpStore::in_memory(),
            token: AuthToken::new(Arc::new(token.to_string()), require_token),
            settings: Arc::new(RuntimeSettings::new(allow_write)),
            audit: audit.clone(),
            effects: Arc::new(NoopEffects::default()),
        };
        (state, audit)
    }

    /// 调用 `dispatch` 的薄封装：把 params 的所有权留在函数内，调用点更短。
    fn call(
        state: &ServerState,
        method: &str,
        params: Value,
        id: Value,
    ) -> RpcResponse {
        dispatch(state, method, Some(&params), id)
    }

    // ---------------- Origin 校验 ----------------

    #[test]
    fn origin_header_rules() {
        assert!(origin_allowed(None), "原生客户端不发 Origin，应放行");
        assert!(origin_allowed(Some("http://127.0.0.1:5173")));
        assert!(origin_allowed(Some("http://localhost")));
        assert!(origin_allowed(Some("http://[::1]:8080")));
        assert!(!origin_allowed(Some("https://evil.example.com")));
        assert!(!origin_allowed(Some("http://127.0.0.1.evil.com")));
        assert!(!origin_allowed(Some("null")));
        assert!(!origin_allowed(Some("")));
    }

    // ---------------- 鉴权 ----------------

    #[test]
    fn token_gate_reports_each_failure_separately() {
        let token = AuthToken::required(Arc::new("secret".to_string()));
        assert!(token.check(Some("secret")).is_ok());
        assert!(token.check(None).is_err());
        assert!(token.check(Some("wrong")).is_err());
        let empty = AuthToken::required(Arc::new(String::new()));
        assert!(
            empty.check(Some("")).is_err(),
            "未配置令牌时必须拒绝而不是放行"
        );
    }

    // ---------------- 免鉴权（出厂默认姿态） ----------------

    #[test]
    fn token_gate_is_off_when_not_required() {
        // 免鉴权 = 校验整体关闭：无令牌、错令牌、空令牌都必须通过。这不是"漏了
        // 一个分支"，而是用户明确要求的默认行为。
        let open = AuthToken::new(Arc::new("secret".to_string()), false);
        assert!(!open.requires_token());
        assert!(open.check(None).is_ok(), "免鉴权时无令牌必须通过");
        assert!(open.check(Some("wrong")).is_ok(), "免鉴权时不比对令牌");
        assert!(open.check(Some("")).is_ok());

        // 出厂默认就是这一档。
        assert!(!AuthToken::disabled().requires_token());
    }

    #[test]
    fn token_gate_can_be_turned_on_at_runtime() {
        // 打开的瞬间必须立刻生效：正在被外部机器连着的服务，收紧权限不能等重启。
        let token = AuthToken::new(Arc::new("secret".to_string()), false);
        assert!(token.check(None).is_ok());

        token.set_require_token(true);
        assert!(token.requires_token());
        assert!(token.check(None).is_err(), "开启后无令牌必须被拒");
        assert!(token.check(Some("wrong")).is_err());
        assert!(token.check(Some("secret")).is_ok(), "正确令牌仍应通过");

        // 关回去也要立刻生效。
        token.set_require_token(false);
        assert!(token.check(None).is_ok());
    }

    #[test]
    fn secret_survives_the_switch() {
        // 免鉴权不等于把令牌删掉：用户打开校验时应当拿到原本那把钥匙，
        // 而不是面对一个空串（那会变成"谁都不许连"）。
        let token = AuthToken::new(Arc::new("keep-me".to_string()), false);
        assert_eq!(token.secret(), "keep-me");
    }

    #[test]
    fn constant_time_eq_handles_length_and_content() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    // ---------------- initialize ----------------

    #[test]
    fn initialize_echoes_a_supported_version() {
        let (state, _) = test_state(false, "t");
        let response = call(
            &state,
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name":"x","version":"1"}}),
            json!(1),
        );
        assert!(response.error.is_none());
        let result = response.result.unwrap();
        assert_eq!(result["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(result["serverInfo"]["name"], SERVER_NAME);
        // 只声明 tools，未实现的能力不得声明。
        assert!(result["capabilities"]["tools"].is_object());
        assert!(result["capabilities"].get("resources").is_none());
        assert!(result["capabilities"].get("prompts").is_none());
    }

    #[test]
    fn initialize_falls_back_for_an_unknown_version() {
        let (state, _) = test_state(false, "t");
        let response = call(
            &state,
            "initialize",
            json!({"protocolVersion": "1999-01-01"}),
            json!(1),
        );
        assert_eq!(response.result.unwrap()["protocolVersion"], PROTOCOL_VERSION);
    }

    #[test]
    fn initialize_accepts_the_older_supported_versions() {
        let (state, _) = test_state(false, "t");
        for version in ["2025-03-26", "2024-11-05"] {
            let response = call(&state, "initialize", json!({ "protocolVersion": version }), json!(1));
            assert_eq!(response.result.unwrap()["protocolVersion"], version);
        }
    }

    // ---------------- tools/list ----------------

    #[test]
    fn tools_list_returns_every_catalog_entry_with_a_schema() {
        let (state, _) = test_state(false, "t");
        let response = call(&state, "tools/list", json!({}), json!(2));
        let tools = response.result.unwrap()["tools"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(tools.len(), tools::catalog().len());
        for tool in &tools {
            assert!(tool["name"].is_string());
            assert!(tool["inputSchema"]["type"] == "object");
            assert!(tool["annotations"]["readOnlyHint"].is_boolean());
        }
    }

    #[test]
    fn tools_list_marks_write_tools_when_readonly() {
        let (readonly, _) = test_state(false, "t");
        let listed = call(&readonly, "tools/list", json!({}), json!(1));
        let tools = listed.result.unwrap()["tools"].as_array().cloned().unwrap();
        let delete = tools
            .iter()
            .find(|t| t["name"] == "delete_entry")
            .expect("delete_entry 应在清单里");
        assert_eq!(delete["annotations"]["readOnlyHint"], false);
        assert!(
            delete["description"].as_str().unwrap().contains("只读模式"),
            "只读模式下的写工具应在描述里显式提示"
        );

        let (writable, _) = test_state(true, "t");
        let listed = call(&writable, "tools/list", json!({}), json!(1));
        let tools = listed.result.unwrap()["tools"].as_array().cloned().unwrap();
        let delete = tools.iter().find(|t| t["name"] == "delete_entry").unwrap();
        assert!(!delete["description"].as_str().unwrap().contains("只读模式"));
    }

    // ---------------- tools/call 协议错误 ----------------

    #[test]
    fn unknown_tool_is_an_invalid_params_error() {
        let (state, _) = test_state(true, "t");
        let response = call(
            &state,
            "tools/call",
            json!({"name": "no_such_tool", "arguments": {}}),
            json!(3),
        );
        let error = response.error.expect("未知工具应产生协议错误");
        assert_eq!(error.code, INVALID_PARAMS);
        assert!(error.message.contains("no_such_tool"));
    }

    #[test]
    fn missing_params_is_an_invalid_params_error() {
        let (state, _) = test_state(true, "t");
        let response = dispatch(&state, "tools/call", None, json!(3));
        assert_eq!(response.error.unwrap().code, INVALID_PARAMS);
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let (state, _) = test_state(true, "t");
        let response = call(&state, "resources/list", json!({}), json!(3));
        assert_eq!(response.error.unwrap().code, METHOD_NOT_FOUND);
    }

    #[test]
    fn missing_required_argument_is_rejected_before_execution() {
        let (state, _) = test_state(true, "t");
        let response = call(
            &state,
            "tools/call",
            json!({"name": "get_entry", "arguments": {}}),
            json!(4),
        );
        let error = response.error.expect("缺必填参数应产生协议错误");
        assert!(error.message.contains("id"));
    }

    #[test]
    fn explicit_null_is_accepted_when_the_schema_allows_it() {
        // `set_tag_color.color` 的 schema 是 `["string","null"]`，`null` 的语义是
        // "清除颜色"。若把显式 `null` 当成"缺参数"，这条路径就永远不可达——
        // 这是本人实测发现并修正的一处缺陷，因此留一条回归。
        let (state, _) = test_state(true, "t");
        let response = call(
            &state,
            "tools/call",
            json!({"name": "set_tag_color", "arguments": {"name": "x", "color": null}}),
            json!(1),
        );
        assert!(
            response.error.is_none(),
            "schema 允许 null 时，显式 null 不应被当成缺参数：{:?}",
            response.error
        );
        assert_eq!(response.result.unwrap()["isError"], false);
    }

    #[test]
    fn explicit_null_is_still_missing_when_the_schema_forbids_it() {
        // 反向对照：schema 未声明 null 的必填参数，显式传 null 仍必须被拒。
        let (state, _) = test_state(true, "t");
        let response = call(
            &state,
            "tools/call",
            json!({"name": "get_entry", "arguments": {"id": null}}),
            json!(1),
        );
        let error = response.error.expect("id=null 应被当成缺参数");
        assert!(error.message.contains("id"), "{}", error.message);
    }

    #[test]
    fn wrong_argument_type_is_rejected() {
        let (state, _) = test_state(true, "t");
        let response = call(
            &state,
            "tools/call",
            json!({"name": "get_entry", "arguments": {"id": "not-a-number"}}),
            json!(4),
        );
        assert!(response.error.is_some());
    }

    // ---------------- 只读守卫 ----------------

    #[test]
    fn readonly_mode_denies_write_tools_with_a_protocol_error() {
        let (state, audit) = test_state(false, "t");
        let response = call(
            &state,
            "tools/call",
            json!({"name": "create_entry", "arguments": {"content": "x"}}),
            json!(5),
        );
        let error = response.error.expect("只读模式必须拒绝写工具");
        assert!(error.message.contains("只读") || error.message.contains("禁用"));
        // 读工具仍然可用。
        let read = call(
            &state,
            "tools/call",
            json!({"name": "list_entries", "arguments": {}}),
            json!(6),
        );
        assert!(read.error.is_none());
        let log = audit.0.lock().unwrap().join("\n");
        assert!(log.contains("denied_readonly"), "拒绝必须留痕：{}", log);
    }

    #[test]
    fn readonly_mode_denies_every_write_tool() {
        let (state, _) = test_state(false, "t");
        for spec in tools::catalog().into_iter().filter(|t| t.access == Access::Write) {
            let response = call(
                &state,
                "tools/call",
                json!({"name": spec.name, "arguments": {}}),
                json!(1),
            );
            let error = response
                .error
                .unwrap_or_else(|| panic!("只读模式应拒绝 {}", spec.name));
            assert!(
                error.message.contains("禁用") || error.message.contains("只读"),
                "{} 的拒绝信息不可读：{}",
                spec.name,
                error.message
            );
        }
    }

    #[test]
    fn destructive_tools_require_explicit_confirmation() {
        let (state, audit) = test_state(true, "t");
        let response = call(
            &state,
            "tools/call",
            json!({"name": "delete_entry", "arguments": {"id": 1}}),
            json!(7),
        );
        let error = response.error.expect("缺 confirm 应被拒绝");
        assert!(error.message.contains("confirm"));
        let log = audit.0.lock().unwrap().join("\n");
        assert!(log.contains("denied_unconfirmed"));
    }

    // ---------------- 成功路径与协议形状 ----------------

    #[test]
    fn create_then_read_through_the_protocol() {
        let (state, _) = test_state(true, "t");
        let created = call(
            &state,
            "tools/call",
            json!({"name": "create_entry", "arguments": {"content": "通过 MCP 写入"}}),
            json!(8),
        );
        let result = created.result.expect("创建应成功");
        assert_eq!(result["isError"], false);
        let id = result["structuredContent"]["id"].as_i64().unwrap();

        let read = call(
            &state,
            "tools/call",
            json!({"name": "get_entry", "arguments": {"id": id}}),
            json!(9),
        );
        let result = read.result.unwrap();
        assert_eq!(result["structuredContent"]["content"], "通过 MCP 写入");
        // 规范建议同时给出文本副本以便旧客户端使用。
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("通过 MCP 写入"));
    }

    #[test]
    fn tool_execution_errors_stay_inside_the_result() {
        let (state, _) = test_state(true, "t");
        let response = call(
            &state,
            "tools/call",
            json!({"name": "get_entry", "arguments": {"id": 424242}}),
            json!(10),
        );
        assert!(response.error.is_none(), "业务失败不应变成 JSON-RPC 错误");
        let result = response.result.unwrap();
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("不存在"));
    }

    // ---------------- 端口绑定与监听地址 ----------------

    #[tokio::test]
    async fn binds_to_loopback_only_with_fallback() {
        let (first, port_a) = bind_local_listener(0).await.unwrap();
        assert_eq!(port_a, crate::services::mcp::store::DEFAULT_PORT);
        let addr = first.local_addr().unwrap();
        assert!(addr.ip().is_loopback(), "只能绑回环地址，实际 {}", addr.ip());

        // 首选端口被占用时应回退到下一个可用端口，而不是失败。
        let (second, port_b) = bind_local_listener(port_a).await.unwrap();
        assert_ne!(port_a, port_b);
        assert!(second.local_addr().unwrap().ip().is_loopback());
    }

    /// 真绑定一个 socket，断言不同设置下**实际**落在哪个地址上。
    ///
    /// 只看 `listen_ip()` 的返回值不够：绑定函数完全可能拿到正确的 IP 又把它丢掉。
    /// 这里读的是 `local_addr()`，也就是内核实际接受的地址。
    ///
    /// 端口刻意用显式的高位端口，**不碰 `DEFAULT_PORT`**：`bind_listener(0, ..)`
    /// 的语义是"占用默认端口"，本测试若把 23123 拿在手里不放，就会把
    /// `binds_to_loopback_only_with_fallback` 挤到 23124 上去（测试并行运行，
    /// 顺序不可控）。占用默认端口是那条测试的职责，不该由这里代劳。
    #[tokio::test]
    async fn binding_address_follows_the_lan_switch() {
        // 默认：仅本机。
        let (loopback, _port_local) = bind_listener(45_231, false).await.expect("应能绑定");
        assert_eq!(
            loopback.local_addr().unwrap().ip(),
            std::net::Ipv4Addr::LOCALHOST,
            "allow_lan=false 必须绑 127.0.0.1"
        );
        assert!(loopback.local_addr().unwrap().ip().is_loopback());

        // 打开局域网：绑 0.0.0.0，同网段其它机器可连。
        let (any, port_any) = bind_listener(45_233, true).await.expect("应能绑定 0.0.0.0");
        assert_eq!(
            any.local_addr().unwrap().ip(),
            std::net::Ipv4Addr::UNSPECIFIED,
            "allow_lan=true 必须绑 0.0.0.0"
        );
        assert!(
            !any.local_addr().unwrap().ip().is_loopback(),
            "0.0.0.0 不是回环地址"
        );

        // 回退逻辑在两种地址下都要工作：占用刚拿到的端口，再要一个应换到别处，
        // 且**回退不得改变监听地址**。
        let (fallback, port_fallback) = bind_listener(port_any, true).await.expect("应能回退");
        assert_ne!(port_any, port_fallback, "端口被占用时应回退到下一个");
        assert_eq!(
            fallback.local_addr().unwrap().ip(),
            std::net::Ipv4Addr::UNSPECIFIED,
            "回退不得改变监听地址"
        );
    }

    #[tokio::test]
    async fn lan_binding_is_reachable_on_the_external_interface() {
        // 这条是"局域网真的能连"的证据：连的是本机在外部网卡上的地址，而不是
        // 回环别名。若实现绑成 127.0.0.1，这里的连接会失败——那正是用户报
        // "局域网连不上"时最容易忽略的一种"看起来设置开了其实没绑对"。
        let Ok(external_ip) = local_ip_address::local_ip() else {
            // 无外网卡的环境（例如纯容器）跳过：这是环境限制，不是实现缺陷。
            return;
        };
        if external_ip.is_loopback() {
            return;
        }

        // 同样不占用 `DEFAULT_PORT`，理由见 `binding_address_follows_the_lan_switch`。
        let (listener, port) = bind_listener(45_235, true).await.expect("应能绑定 0.0.0.0");
        let (state, _) = test_state_with(false, "unused", false);
        let handle = tokio::spawn(serve(listener, state));

        let url = format!("http://{}:{}/mcp", external_ip, port);
        let response = reqwest::Client::new()
            .post(&url)
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
            .send()
            .await;
        handle.abort();

        let response = response.expect("经由外部网卡地址应能连上服务");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
    }

    #[test]
    fn set_tag_color_schema_declares_a_nullable_color() {
        // 形状自检：`color` 必须同时允许 string 与 null，且出现在 required 里。
        // 若这条变红，说明 schema 被改坏了，`null`（清除颜色）会随之不可达。
        let spec = tools::find("set_tag_color").expect("set_tag_color 应在清单里");
        let color = &spec.input_schema["properties"]["color"]["type"];
        assert_eq!(color, &json!(["string", "null"]), "color 应允许 string 与 null");
        let required = spec.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "color"));
        // 直接驱动纯函数，确认判定本身正确。
        assert!(type_allows_null(&spec.input_schema["properties"]["color"]));
        assert!(!type_allows_null(&spec.input_schema["properties"]["name"]));
        // 再直接把校验函数本身跑一遍：把"schema 对但校验逻辑错"这种情形分开。
        assert!(
            validate_args(&spec, &json!({"name": "x", "color": null})).is_ok(),
            "validate_args 应接受显式 null 的 color：{:?}",
            validate_args(&spec, &json!({"name": "x", "color": null}))
        );
    }

    // ---------------- 真实 HTTP 往返（socket 层证据） ----------------

    /// 真起一个 socket、真发 HTTP 请求：单测直接调 `dispatch` 证明不了传输层，
    /// 这一条补上"axum 路由 + 头校验 + JSON 编解码"整条链路的证据。
    ///
    /// 端口用系统分配的临时端口，测试结束即 abort 服务任务，不留监听。
    #[tokio::test]
    async fn real_http_round_trip_over_loopback() {
        let (state, _) = test_state(false, "http-token");
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0u16))
            .await
            .expect("应能绑定回环临时端口");
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(serve(listener, state));

        let base = format!("http://127.0.0.1:{}/mcp", port);
        let client = reqwest::Client::new();

        // (1) 无令牌 → 401。
        let anonymous = client
            .post(&base)
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
            .send()
            .await
            .expect("请求应到达服务");
        assert_eq!(anonymous.status(), reqwest::StatusCode::UNAUTHORIZED);

        // (2) 错 Origin → 403。
        let bad_origin = client
            .post(&base)
            .header("Origin", "https://evil.example.com")
            .header("x-mcp-token", "http-token")
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
            .send()
            .await
            .expect("请求应到达服务");
        assert_eq!(bad_origin.status(), reqwest::StatusCode::FORBIDDEN);

        // (3) 正确令牌 → 200 且是合法 initialize 结果。
        let initialized: Value = client
            .post(&base)
            .header("Origin", "http://127.0.0.1")
            .header("x-mcp-token", "http-token")
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "http-probe", "version": "1.0.0"}
                }
            }))
            .send()
            .await
            .expect("请求应到达服务")
            .json()
            .await
            .expect("响应应是 JSON");
        assert_eq!(initialized["jsonrpc"], "2.0");
        assert_eq!(initialized["id"], 1);
        assert_eq!(initialized["result"]["protocolVersion"], PROTOCOL_VERSION);

        // (4) 通知（无 id）→ 202 且无 body。
        let notified = client
            .post(&base)
            .header("x-mcp-token", "http-token")
            .json(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .send()
            .await
            .expect("通知应被接受");
        assert_eq!(notified.status(), reqwest::StatusCode::ACCEPTED);
        assert!(notified.text().await.unwrap_or_default().is_empty());

        // (5) GET → 405（本服务不提供 SSE 流，规范要求如此）。
        let got = client
            .get(&base)
            .header("x-mcp-token", "http-token")
            .send()
            .await
            .expect("GET 应被受理并拒绝");
        assert_eq!(got.status(), reqwest::StatusCode::METHOD_NOT_ALLOWED);

        handle.abort();
    }

    /// 免鉴权模式下的真实 HTTP 往返。
    ///
    /// 这是"默认无鉴权"这件事在**传输层**的证据：完全没有 `x-mcp-token` 头的
    /// 请求必须拿到 200 与合法结果。只断言 `AuthToken::check` 是不够的——门后的
    /// handler 或中间件完全可能另有一道校验，而用户感知到的恰恰是 socket 层的结果。
    #[tokio::test]
    async fn anonymous_http_request_is_accepted_when_token_is_not_required() {
        let (state, _) = test_state_with(true, "generated-but-unused", false);
        // 用高位端口，不占用 `DEFAULT_PORT`（理由见 `binding_address_follows_the_lan_switch`）。
        let (listener, port) = bind_listener(45_237, false).await.expect("应能绑定本机端口");
        let handle = tokio::spawn(serve(listener, state));
        let base = format!("http://127.0.0.1:{}/mcp", port);
        let client = reqwest::Client::new();

        // (1) 一个头都不带的 initialize → 200 且是合法结果。
        let response = client
            .post(&base)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "anonymous-probe", "version": "1.0.0"}
                }
            }))
            .send()
            .await
            .expect("免鉴权时请求应到达服务");
        assert_eq!(
            response.status(),
            reqwest::StatusCode::OK,
            "免鉴权时无令牌请求必须被接受"
        );
        let body: Value = response.json().await.expect("响应应是 JSON");
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], 1);
        assert_eq!(body["result"]["protocolVersion"], PROTOCOL_VERSION);

        // (2) 带上一个**错误的**令牌同样放行：免鉴权是不比对，不是"比对了但宽松"。
        let with_wrong_token = client
            .post(&base)
            .header("x-mcp-token", "definitely-not-the-token")
            .json(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
            .send()
            .await
            .expect("请求应到达服务");
        assert_eq!(with_wrong_token.status(), reqwest::StatusCode::OK);

        // (3) 关掉鉴权不代表关掉 Origin 校验：DNS rebinding 防护与令牌是两件事。
        let bad_origin = client
            .post(&base)
            .header("Origin", "https://evil.example.com")
            .json(&json!({"jsonrpc": "2.0", "id": 3, "method": "initialize"}))
            .send()
            .await
            .expect("请求应到达服务");
        assert_eq!(
            bad_origin.status(),
            reqwest::StatusCode::FORBIDDEN,
            "免鉴权不得连带放开 Origin 校验"
        );

        handle.abort();
    }

    /// 打开强制校验后，同一条无令牌请求必须被拒。
    ///
    /// 与上一条配对：证明"免鉴权"不是把校验代码删了，而是确实还存在、且能立刻
    /// 生效。两扇门用同一个端口启动，差别只在 `require_token` 这一个布尔。
    #[tokio::test]
    async fn anonymous_http_request_is_rejected_when_token_is_required() {
        let (state, _) = test_state_with(true, "real-token", true);
        // 用高位端口，不占用 `DEFAULT_PORT`（理由见 `binding_address_follows_the_lan_switch`）。
        let (listener, port) = bind_listener(45_239, false).await.expect("应能绑定本机端口");
        let handle = tokio::spawn(serve(listener, state));
        let base = format!("http://127.0.0.1:{}/mcp", port);
        let client = reqwest::Client::new();
        let payload = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"});

        // (1) 无令牌 → 401。
        let anonymous = client
            .post(&base)
            .json(&payload)
            .send()
            .await
            .expect("请求应到达服务");
        assert_eq!(
            anonymous.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "开启校验后无令牌请求必须被拒"
        );

        // (2) 错令牌 → 401。
        let wrong = client
            .post(&base)
            .header("x-mcp-token", "wrong")
            .json(&payload)
            .send()
            .await
            .expect("请求应到达服务");
        assert_eq!(wrong.status(), reqwest::StatusCode::UNAUTHORIZED);

        // (3) 正确令牌 → 200。
        let correct = client
            .post(&base)
            .header("x-mcp-token", "real-token")
            .json(&payload)
            .send()
            .await
            .expect("请求应到达服务");
        assert_eq!(correct.status(), reqwest::StatusCode::OK);

        handle.abort();
    }

    /// 供外部 `curl` 验证的监听窗口。
    ///
    /// 默认不跑（`#[ignore]`）；显式执行时在 127.0.0.1:23123 上服务约 60 秒，
    /// 让另一个进程用真实 HTTP 客户端打进来，随后自行结束——**不留常驻监听**。
    #[tokio::test]
    #[ignore = "开一个 60 秒的本地监听窗口供外部 curl 验证，默认不跑"]
    async fn loopback_listener_window_for_external_curl() {
        let (state, _) = test_state(true, "curl-token");
        let (listener, port) = bind_local_listener(0).await.expect("应能绑定默认端口");
        crate::info!("MCP curl window listening on 127.0.0.1:{}", port);
        let handle = tokio::spawn(serve(listener, state));
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        handle.abort();
    }

    #[test]
    fn every_tool_has_a_unique_name() {
        let mut names: Vec<&str> = tools::catalog().iter().map(|t| t.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "工具名必须唯一");
    }

    #[test]
    fn required_arguments_appear_in_the_schema_properties() {
        for spec in tools::catalog() {
            let props = spec.input_schema["properties"].as_object().unwrap();
            let required = spec.input_schema["required"].as_array().unwrap();
            for key in required {
                let key = key.as_str().unwrap();
                assert!(
                    props.contains_key(key),
                    "工具 {} 的必填参数 {} 未在 properties 中声明",
                    spec.name,
                    key
                );
            }
        }
    }
}
