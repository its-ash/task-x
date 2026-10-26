//! JSON-RPC 2.0 message shapes. Framing is one JSON object per line (see docs/protocol.md).

use serde::Deserialize;
use serde_json::{Value, json};

pub const PROTOCOL_VERSION: u32 = 1;

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
pub const PROVIDER_ERROR: i64 = -32001;
pub const PROTOCOL_MISMATCH: i64 = -32002;
pub const NOT_INITIALIZED: i64 = -32003;
pub const NO_WORKSPACE: i64 = -32004;
pub const TOOL_ERROR: i64 = -32005;
pub const REQUEST_CANCELLED: i64 = -32800;

#[derive(Debug, Deserialize)]
pub struct Incoming {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

pub fn response(id: &Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

pub fn error(id: &Value, e: &RpcError) -> String {
    let mut err = json!({ "code": e.code, "message": e.message });
    if let Some(d) = &e.data {
        err["data"] = d.clone();
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": err }).to_string()
}

pub fn request(id: &str, method: &str, params: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string()
}

pub fn notification(method: &str, params: Value) -> String {
    json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string()
}

/// Ids must be strings or integers; used as the cancellation key.
pub fn id_key(id: &Value) -> Option<String> {
    match id {
        Value::String(s) => Some(format!("s:{s}")),
        Value::Number(n) if n.is_i64() || n.is_u64() => Some(format!("n:{n}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_keys_distinguish_string_and_number() {
        assert_ne!(id_key(&json!(1)), id_key(&json!("1")));
        assert_eq!(id_key(&json!(1.5)), None);
        assert_eq!(id_key(&Value::Null), None);
    }

    #[test]
    fn error_serialization_includes_optional_data() {
        let s = error(&json!(7), &RpcError::new(PROVIDER_ERROR, "boom").with_data(json!({"kind": "auth"})));
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error"]["data"]["kind"], "auth");
        assert_eq!(v["id"], 7);
        assert!(!s.contains('\n'));
    }
}
