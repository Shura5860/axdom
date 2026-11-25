use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const JSONRPC_VERSION: &str = "2.0";
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

impl JsonRpcResponse {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

/// Standard MCP tool result block (`content: [{ type: "text", text }]`).
pub fn tool_text_result(text: impl Into<String>, is_error: bool) -> Value {
    serde_json::json!({
        "content": [
            {
                "type": "text",
                "text": text.into(),
            }
        ],
        "isError": is_error,
    })
}

/// MCP tool result containing both text and a base64-encoded image block (e.g. blind-spot fallback).
pub fn tool_text_and_image_result(
    text: impl Into<String>,
    mime_type: &str,
    base64_data: &str,
) -> Value {
    serde_json::json!({
        "content": [
            {
                "type": "text",
                "text": text.into(),
            },
            {
                "type": "image",
                "data": base64_data,
                "mimeType": mime_type,
            }
        ],
        "isError": false,
    })
}
