//! Clipboard request validation shared by the current Android and legacy v2 wire formats.
use super::profile::{hash_text, MAX_SOURCE_BYTES};
use super::{MAX_JSON_BYTES, MAX_TEXT_BYTES};
use serde_json::Value;

#[derive(Debug)]
pub(crate) struct ClipboardRequestError {
    pub status: u16,
    pub reason: &'static str,
}

fn invalid(reason: &'static str) -> ClipboardRequestError {
    ClipboardRequestError {
        status: 400,
        reason,
    }
}

fn too_large(reason: &'static str) -> ClipboardRequestError {
    ClipboardRequestError {
        status: 413,
        reason,
    }
}

/// Validate without rewriting the sender's fields or interpreting its clock.
pub(crate) fn parse_clipboard_body(body: &[u8]) -> Result<Value, ClipboardRequestError> {
    if body.len() > MAX_JSON_BYTES {
        return Err(too_large("clipboard JSON exceeds maximum size"));
    }
    let value: Value =
        serde_json::from_slice(body).map_err(|_| invalid("invalid clipboard JSON"))?;
    if !value.is_object() {
        return Err(invalid("clipboard JSON must be an object"));
    }
    if value.get("v").is_some_and(|v| v.as_u64() != Some(2)) {
        return Err(invalid("unsupported clipboard version"));
    }
    if value.get("type").and_then(Value::as_str) != Some("text") {
        return Err(invalid("clipboard type must be text"));
    }
    let text = value
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("clipboard text must be a string"))?;
    if text.len() > MAX_TEXT_BYTES {
        return Err(too_large("clipboard text exceeds maximum size"));
    }
    if text.contains('\0') {
        return Err(invalid("clipboard text contains NUL"));
    }
    if value.get("size").and_then(Value::as_u64) != Some(text.len() as u64) {
        return Err(invalid("clipboard size does not match UTF-8 bytes"));
    }
    if value.get("hash").and_then(Value::as_str) != Some(hash_text(text.as_bytes()).as_str()) {
        return Err(invalid("clipboard hash does not match text"));
    }
    // Old peers may omit these optional metadata fields; present values must describe text only.
    if value
        .get("has_data")
        .is_some_and(|v| v.as_bool() != Some(false))
    {
        return Err(invalid("clipboard has_data must be false"));
    }
    if value.get("data_name").is_some_and(|v| !v.is_null()) {
        return Err(invalid("clipboard data_name must be null"));
    }
    if let Some(source) = value.get("source") {
        let source = source
            .as_str()
            .ok_or_else(|| invalid("clipboard source must be a string"))?;
        if source.len() > MAX_SOURCE_BYTES {
            return Err(too_large("clipboard source exceeds maximum size"));
        }
    }
    let stamp = value
        .get("stamp")
        .ok_or_else(|| invalid("clipboard stamp is required"))?;
    let numeric_stamp = stamp.as_i64().is_some_and(|n| n >= 0);
    let legacy_stamp = stamp.is_object()
        && stamp.get("device_id").and_then(Value::as_str).is_some()
        && stamp.get("counter").and_then(Value::as_u64).is_some();
    if !numeric_stamp && !legacy_stamp {
        return Err(invalid("invalid clipboard stamp"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn android(text: &str) -> Value {
        json!({"type":"text", "text":text, "hash":hash_text(text.as_bytes()),
            "size":text.len(), "has_data":false, "data_name":null,
            "source":"CyIME-Android", "stamp":1776000000000i64})
    }

    fn parse(value: &Value) -> Result<Value, ClipboardRequestError> {
        parse_clipboard_body(&serde_json::to_vec(value).unwrap())
    }

    fn rejects(value: &Value, status: u16) {
        let error = parse(value).unwrap_err();
        assert_eq!(error.status, status, "{}", error.reason);
        assert!(!error.reason.is_empty());
    }

    #[test]
    fn android_unicode_preserves_original_json_and_clock() {
        let mut value = android("你好\r\n🌍");
        value["extension"] = json!({"retain":true});
        assert_eq!(parse(&value).unwrap(), value);
        assert!(parse(&value).unwrap().get("v").is_none());
        for stamp in [0, i64::MAX] {
            value["stamp"] = json!(stamp);
            assert_eq!(parse(&value).unwrap(), value);
        }
    }

    #[test]
    fn legacy_v2_preserves_object_stamp() {
        let mut value = android("legacy");
        value["v"] = json!(2);
        value["stamp"] = json!({"device_id":"pc", "counter":u64::MAX});
        assert_eq!(parse(&value).unwrap(), value);
    }

    #[test]
    fn rejects_bad_version_and_stamp() {
        for version in [json!(1), json!(3), json!("2"), Value::Null, json!(2.5)] {
            let mut value = android("a");
            value["v"] = version;
            rejects(&value, 400);
        }
        for stamp in [
            json!(-1),
            json!(u64::MAX),
            json!(1.5),
            json!("1"),
            Value::Null,
            json!({}),
            json!({"counter":1}),
            json!({"device_id":"pc", "counter":-1}),
            json!({"device_id":1, "counter":1}),
        ] {
            let mut value = android("a");
            value["stamp"] = stamp;
            rejects(&value, 400);
        }
        let mut value = android("a");
        value.as_object_mut().unwrap().remove("stamp");
        rejects(&value, 400);
    }

    #[test]
    fn rejects_invalid_text_hash_and_byte_size() {
        let mut value = android("你好🌍");
        value["size"] = json!(3);
        rejects(&value, 400);
        value = android("a");
        value["hash"] = json!("0".repeat(64));
        rejects(&value, 400);
        rejects(&android("a\0b"), 400);
        for (field, bad) in [
            ("type", json!("image")),
            ("text", Value::Null),
            ("size", json!(-1)),
            ("has_data", json!(true)),
            ("data_name", json!("file")),
            ("source", json!(1)),
        ] {
            value = android("a");
            value[field] = bad;
            rejects(&value, 400);
        }
    }

    #[test]
    fn optional_metadata_may_be_absent() {
        let mut value = android("a");
        for field in ["has_data", "data_name", "source"] {
            value.as_object_mut().unwrap().remove(field);
        }
        assert_eq!(parse(&value).unwrap(), value);
    }

    #[test]
    fn rejects_malformed_json_without_panicking() {
        for body in [b"{".as_slice(), b"[]", b"null", b"\xff"] {
            assert_eq!(parse_clipboard_body(body).unwrap_err().status, 400);
        }
    }

    #[test]
    fn enforces_limits_and_accepts_exact_boundaries() {
        assert!(parse(&android(&"a".repeat(MAX_TEXT_BYTES))).is_ok());
        rejects(&android(&"a".repeat(MAX_TEXT_BYTES + 1)), 413);
        let mut value = android("a");
        value["source"] = json!("a".repeat(MAX_SOURCE_BYTES));
        assert!(parse(&value).is_ok());
        value["source"] = json!("a".repeat(MAX_SOURCE_BYTES + 1));
        rejects(&value, 413);
        let mut body = serde_json::to_vec(&android("a")).unwrap();
        body.resize(MAX_JSON_BYTES, b' ');
        assert!(parse_clipboard_body(&body).is_ok());
        body.push(b' ');
        assert_eq!(parse_clipboard_body(&body).unwrap_err().status, 413);
    }
}
