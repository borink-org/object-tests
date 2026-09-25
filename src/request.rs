use crate::Result;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

/// Converts an HTTP request into the fields used by JSON assertions.
///
/// Decodes percent escapes once and preserves dot segments and literal query `+` signs.
///
/// # Errors
/// Returns an error for malformed request targets, percent escapes or decoded UTF-8.
pub fn normalize_http_request(
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<Value> {
    fn percent_decode_utf8(value: &str) -> Result<String> {
        let bytes = value.as_bytes();
        for (index, byte) in bytes.iter().enumerate() {
            if *byte == b'%'
                && (index + 2 >= bytes.len()
                    || !bytes[index + 1].is_ascii_hexdigit()
                    || !bytes[index + 2].is_ascii_hexdigit())
            {
                return Err("malformed percent encoding".into());
            }
        }
        Ok(percent_encoding::percent_decode_str(value)
            .decode_utf8()?
            .into_owned())
    }

    fn insert_preserving_duplicates(
        map: &mut serde_json::Map<String, Value>,
        name: String,
        value: String,
    ) {
        match map.get_mut(&name) {
            Some(Value::Array(values)) => values.push(json!(value)),
            Some(previous) => *previous = json!([previous.take(), value]),
            None => {
                map.insert(name, json!(value));
            }
        }
    }

    // HTTP/1.1 permits absolute-form targets too. Split without a URL library:
    // URL normalization would incorrectly collapse object-key dot segments.
    let target = if let Some(rest) = target
        .strip_prefix("http://")
        .or_else(|| target.strip_prefix("https://"))
    {
        let end = rest.find(['/', '?']).unwrap_or(rest.len());
        if end == 0 || rest[..end].contains(['@', '#']) {
            return Err("invalid absolute request authority".into());
        }
        &rest[end..]
    } else {
        target
    };
    if target.contains('#') {
        return Err("request target contains a fragment".into());
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let path = if path.is_empty() { "/" } else { path };
    if !path.starts_with('/') {
        return Err("expected origin-form or absolute-form request target".into());
    }
    let mut query_fields = serde_json::Map::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        insert_preserving_duplicates(
            &mut query_fields,
            percent_decode_utf8(name)?,
            percent_decode_utf8(value)?,
        );
    }

    let mut header_fields = serde_json::Map::new();
    for (name, value) in headers {
        insert_preserving_duplicates(&mut header_fields, name.to_ascii_lowercase(), value.clone());
    }

    let mut request = json!({
        "method": method,
        "raw_path": path,
        "path": percent_decode_utf8(path)?,
        "query": query_fields,
        "headers": header_fields,
        "body_base64": STANDARD.encode(body),
    });
    if let Ok(text) = std::str::from_utf8(body) {
        request["body_text"] = json!(text);
    }
    Ok(request)
}
