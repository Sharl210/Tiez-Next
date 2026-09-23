//! JSON-RPC 2.0 消息模型。
//!
//! MCP 的传输层就是 JSON-RPC 2.0（见 MCP 规范 `basic/index`），因此这一层只做
//! "把一个 JSON 文本解析成请求 / 把结果组装成响应"，不掺任何业务判断，也不认识
//! Tauri。这样协议正确性可以脱离真实 socket 与真实应用单独测试。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC 协议版本字段的固定值。
pub const JSONRPC_VERSION: &str = "2.0";

// 标准 JSON-RPC 2.0 错误码。
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// 一条入站请求。
///
/// `id` 用 `Option<Value>` 而不是整数：JSON-RPC 允许字符串 id，而**没有 `id` 的
/// 消息是通知（notification），按规范必须不产生响应**。把这两种情况分开是协议
/// 正确性的关键，因此这里不做"缺 id 就当 0"的兜底。
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

impl Request {
    /// 是否是通知：规范规定通知不得带 `id`，且不得被回复。
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

/// 一条出站响应。成功与失败互斥，序列化时丢掉 `None`，避免出现
/// `{"result":null,"error":null}` 这种不合规形状。
#[derive(Debug, Clone, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn failure(id: Value, error: RpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// 解析一条入站消息。
///
/// 返回 `Err` 时调用方应回复 `-32700 Parse error`（带 `id: null`，这是规范允许的
/// 形状——请求本身无法解析，自然拿不到它的 id）。JSON 合法但结构不符合请求形状
/// （缺 `method`、`jsonrpc` 不是 "2.0"）时返回 `-32600 Invalid Request`。
pub fn parse_request(raw: &[u8]) -> Result<Request, RpcError> {
    let text = std::str::from_utf8(raw)
        .map_err(|e| RpcError::new(PARSE_ERROR, format!("请求体不是合法 UTF-8：{}", e)))?;
    let parsed: Request = serde_json::from_str(text)
        .map_err(|e| RpcError::new(PARSE_ERROR, format!("请求体不是合法 JSON-RPC：{}", e)))?;

    if parsed.jsonrpc != JSONRPC_VERSION {
        return Err(RpcError::new(
            INVALID_REQUEST,
            format!(
                "jsonrpc 字段必须是 \"{}\"，收到 \"{}\"",
                JSONRPC_VERSION, parsed.jsonrpc
            ),
        ));
    }
    if parsed.method.trim().is_empty() {
        return Err(RpcError::new(INVALID_REQUEST, "method 不能为空"));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_request() {
        let req = parse_request(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).unwrap();
        assert_eq!(req.method, "tools/list");
        assert_eq!(req.id, Some(serde_json::json!(1)));
        assert!(!req.is_notification());
    }

    #[test]
    fn a_request_without_id_is_a_notification() {
        let req =
            parse_request(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap();
        assert!(req.is_notification());
    }

    #[test]
    fn string_ids_are_preserved_verbatim() {
        let req = parse_request(br#"{"jsonrpc":"2.0","id":"abc-1","method":"ping"}"#).unwrap();
        assert_eq!(req.id, Some(serde_json::json!("abc-1")));
    }

    #[test]
    fn null_id_counts_as_a_notification() {
        // 规范里通知"不得包含 id"；`"id": null` 是显式写了 id 但不带值，仍按通知处理，
        // 因为无法为一个 null id 返回有意义的响应。
        let req = parse_request(br#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#).unwrap();
        assert!(req.is_notification());
    }

    #[test]
    fn broken_json_is_a_parse_error() {
        let err = parse_request(b"{not json").unwrap_err();
        assert_eq!(err.code, PARSE_ERROR);
    }

    #[test]
    fn wrong_jsonrpc_version_is_an_invalid_request() {
        let err = parse_request(br#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#).unwrap_err();
        assert_eq!(err.code, INVALID_REQUEST);
    }

    #[test]
    fn empty_method_is_an_invalid_request() {
        let err = parse_request(br#"{"jsonrpc":"2.0","id":1,"method":"  "}"#).unwrap_err();
        assert_eq!(err.code, INVALID_REQUEST);
    }

    #[test]
    fn success_response_omits_the_error_field() {
        let json = serde_json::to_value(Response::success(
            serde_json::json!(7),
            serde_json::json!({"ok": true}),
        ))
        .unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 7);
        assert!(json.get("error").is_none());
    }

    #[test]
    fn failure_response_omits_the_result_field() {
        let json = serde_json::to_value(Response::failure(
            serde_json::json!(7),
            RpcError::new(METHOD_NOT_FOUND, "nope"),
        ))
        .unwrap();
        assert!(json.get("result").is_none());
        assert_eq!(json["error"]["code"], METHOD_NOT_FOUND);
    }
}
