//! Typed helpers for the subset of MCP JSON-RPC messages that `npxc` acts on.
//!
//! The transport layer is untyped `serde_json::Value` — we parse each line as
//! a `Value`, then extract structure only for the messages we care about.

use serde_json::Value;

// ---------------------------------------------------------------------------
// Message kind detection
// ---------------------------------------------------------------------------

/// Classification of a JSON-RPC message derived from the fields present.
#[derive(Debug, Clone, PartialEq)]
pub enum MessageKind {
    /// A request: has both `"method"` and `"id"`.
    Request { method: String },
    /// A notification: has `"method"` but no `"id"`.
    Notification { method: String },
    /// A response: has `"result"` or `"error"` (but no `"method"`).
    Response,
    /// Does not match any of the above shapes.
    Unknown,
}

/// Classify a parsed JSON-RPC `Value`.
///
/// - Has `"method"` + `"id"`      → [`MessageKind::Request`]
/// - Has `"method"`, no `"id"`    → [`MessageKind::Notification`]
/// - Has `"result"` or `"error"`  → [`MessageKind::Response`]
/// - Otherwise                    → [`MessageKind::Unknown`]
#[must_use]
pub fn message_kind(value: &Value) -> MessageKind {
    if let Some(method) = value.get("method").and_then(|v| v.as_str()) {
        if value.get("id").is_some() {
            MessageKind::Request {
                method: method.to_owned(),
            }
        } else {
            MessageKind::Notification {
                method: method.to_owned(),
            }
        }
    } else if value.get("result").is_some() || value.get("error").is_some() {
        MessageKind::Response
    } else {
        MessageKind::Unknown
    }
}

// ---------------------------------------------------------------------------
// Tool schema cache entry
// ---------------------------------------------------------------------------

/// A tool schema entry parsed from a `tools/list` response.
#[derive(Debug, Clone)]
pub struct ToolSchema {
    /// The tool's registered name.
    pub name: String,
    /// The full `inputSchema` object from the `tools/list` response.
    pub input_schema: Value,
}

// ---------------------------------------------------------------------------
// Extraction helpers
// ---------------------------------------------------------------------------

/// Extract tool schemas from a `tools/list` response `Value`.
///
/// Returns an empty `Vec` if the value is not a well-formed `tools/list`
/// response (i.e., missing `result.tools` array or malformed entries).
///
/// Expected shape:
/// ```json
/// { "result": { "tools": [ { "name": "...", "inputSchema": { ... } } ] } }
/// ```
#[must_use]
pub fn extract_tool_schemas(value: &Value) -> Vec<ToolSchema> {
    let Some(tools) = value
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
    else {
        return Vec::new();
    };

    tools
        .iter()
        .filter_map(|tool| {
            let name = tool.get("name")?.as_str()?.to_owned();
            let input_schema = tool.get("inputSchema")?.clone();
            Some(ToolSchema { name, input_schema })
        })
        .collect()
}

/// For a `tools/call` request `Value`, return `(tool_name, arguments_object)`.
///
/// Returns `None` if the value does not have the expected shape:
/// ```json
/// { "params": { "name": "...", "arguments": { ... } } }
/// ```
#[must_use]
pub fn extract_tools_call(value: &Value) -> Option<(&str, &serde_json::Map<String, Value>)> {
    let params = value.get("params")?;
    let name = params.get("name")?.as_str()?;
    let arguments = params.get("arguments")?.as_object()?;
    Some((name, arguments))
}

/// For a `resources/read` request `Value`, return the URI string if present.
///
/// Expected shape:
/// ```json
/// { "params": { "uri": "file:///workspace/..." } }
/// ```
#[must_use]
pub fn extract_resources_read_uri(value: &Value) -> Option<&str> {
    value
        .get("params")
        .and_then(|p| p.get("uri"))
        .and_then(|u| u.as_str())
}

/// Return the `"id"` field of a JSON-RPC message.
///
/// The id may be a number, string, or `null` per the JSON-RPC 2.0 spec.
/// Returns a reference to a static `Value::Null` when no `"id"` field is
/// present (e.g., for notifications).
#[must_use]
pub fn message_id(value: &Value) -> &Value {
    static NULL: Value = Value::Null;
    value.get("id").unwrap_or(&NULL)
}

/// Recursively replace substrings in all JSON string *values* (leaves) of `value`.
///
/// For each `(from, to)` pair in `replacements`, every occurrence of `from`
/// in a string leaf is replaced with `to`. Object **keys** are not modified.
///
/// Traversal is a recursive DFS over arrays and objects. This is O(n·k) where
/// k = `replacements.len()`; k is expected to be small (bounded by the number
/// of published files per session).
///
/// Typical use: reverse-translate `/workspace/<uuid>/<basename>` paths back to
/// their host paths before forwarding a response to the client.
pub fn replace_in_strings(value: &mut Value, replacements: &[(String, String)]) {
    match value {
        Value::String(s) => {
            for (from, to) in replacements {
                if s.contains(from.as_str()) {
                    *s = s.replace(from.as_str(), to.as_str());
                }
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                replace_in_strings(item, replacements);
            }
        }
        Value::Object(obj) => {
            for v in obj.values_mut() {
                replace_in_strings(v, replacements);
            }
        }
        // Bool, Number, Null — nothing to replace.
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn message_kind_request() {
        let v = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {} });
        assert_eq!(
            message_kind(&v),
            MessageKind::Request {
                method: "tools/call".to_owned()
            }
        );
    }

    #[test]
    fn message_kind_notification() {
        let v = json!({ "jsonrpc": "2.0", "method": "notifications/progress", "params": {} });
        assert_eq!(
            message_kind(&v),
            MessageKind::Notification {
                method: "notifications/progress".to_owned()
            }
        );
    }

    #[test]
    fn message_kind_response_result() {
        let v = json!({ "jsonrpc": "2.0", "id": 1, "result": {} });
        assert_eq!(message_kind(&v), MessageKind::Response);
    }

    #[test]
    fn message_kind_response_error() {
        let v = json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32600, "message": "err" } });
        assert_eq!(message_kind(&v), MessageKind::Response);
    }

    #[test]
    fn message_kind_unknown() {
        let v = json!({ "jsonrpc": "2.0" });
        assert_eq!(message_kind(&v), MessageKind::Unknown);
    }

    #[test]
    fn extract_tool_schemas_happy() {
        let v = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {
                "tools": [
                    { "name": "read_file", "inputSchema": { "type": "object" } },
                    { "name": "write_file", "inputSchema": { "type": "object" } }
                ]
            }
        });
        let schemas = extract_tool_schemas(&v);
        assert_eq!(schemas.len(), 2);
        assert_eq!(schemas[0].name, "read_file");
        assert_eq!(schemas[1].name, "write_file");
    }

    #[test]
    fn extract_tool_schemas_missing_returns_empty() {
        let v = json!({ "result": {} });
        assert!(extract_tool_schemas(&v).is_empty());
    }

    #[test]
    fn extract_tools_call_happy() {
        let v = json!({
            "jsonrpc": "2.0", "id": 2,
            "method": "tools/call",
            "params": { "name": "read_file", "arguments": { "path": "/foo" } }
        });
        let (name, args) = extract_tools_call(&v).unwrap();
        assert_eq!(name, "read_file");
        assert_eq!(args["path"], "/foo");
    }

    #[test]
    fn extract_tools_call_missing_returns_none() {
        let v = json!({ "method": "tools/call" });
        assert!(extract_tools_call(&v).is_none());
    }

    #[test]
    fn extract_resources_read_uri_happy() {
        let v = json!({
            "jsonrpc": "2.0", "id": 3,
            "method": "resources/read",
            "params": { "uri": "file:///workspace/abc/foo.txt" }
        });
        assert_eq!(
            extract_resources_read_uri(&v),
            Some("file:///workspace/abc/foo.txt")
        );
    }

    #[test]
    fn message_id_present() {
        let v = json!({ "id": 42 });
        assert_eq!(message_id(&v), &json!(42));
    }

    #[test]
    fn message_id_absent_returns_null() {
        let v = json!({ "method": "notify" });
        assert_eq!(message_id(&v), &Value::Null);
    }

    #[test]
    fn replace_in_strings_recursive() {
        let mut v = json!({
            "text": "/workspace/uuid-1/file.txt",
            "nested": {
                "arr": ["/workspace/uuid-1/other.txt", 42, null]
            }
        });
        let replacements = vec![(
            "/workspace/uuid-1".to_owned(),
            "/home/user/project".to_owned(),
        )];
        replace_in_strings(&mut v, &replacements);
        assert_eq!(v["text"], "/home/user/project/file.txt");
        assert_eq!(v["nested"]["arr"][0], "/home/user/project/other.txt");
        // Non-string leaves are untouched.
        assert_eq!(v["nested"]["arr"][1], 42);
        assert_eq!(v["nested"]["arr"][2], Value::Null);
    }

    #[test]
    fn replace_in_strings_does_not_touch_keys() {
        let mut v = json!({ "/workspace/uuid-1": "value" });
        let replacements = vec![("/workspace/uuid-1".to_owned(), "/host".to_owned())];
        replace_in_strings(&mut v, &replacements);
        // Key is unchanged; value doesn't contain the pattern, also unchanged.
        assert!(v.get("/workspace/uuid-1").is_some());
        assert_eq!(v["/workspace/uuid-1"], "value");
    }
}
