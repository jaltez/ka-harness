//! Repairing JSON parser for streamed tool-call arguments. Providers emit
//! fragments that can end mid-string, mid-number, or with unbalanced
//! brackets; we close what's open and re-parse before giving up.

use serde_json::Value;

/// Best-effort parse of a possibly-truncated JSON object. Returns `{}` for
/// empty/garbage input rather than failing — an unparsable argument set is
/// a model bug, not a reason to crash the turn.
pub fn repair_json(raw: &str) -> Value {
    let s = raw.trim();
    if s.is_empty() {
        return Value::Object(Default::default());
    }
    if let Ok(v) = serde_json::from_str::<Value>(s) {
        return v;
    }
    let repaired = close_open(s);
    if let Ok(v) = serde_json::from_str::<Value>(&repaired) {
        return v;
    }
    // Truncated mid-key (`{"a": 1, "b"`): close_open leaves the dangling
    // key unparsed. Complete the pair and re-close before salvage.
    if s.ends_with('"') && !s.ends_with("\\\"") {
        let completed = close_open(&format!("{s}: null"));
        if let Ok(v) = serde_json::from_str::<Value>(&completed) {
            return v;
        }
    }
    // Last resort: salvage `key: "value"` pairs by wrapping and closing.
    // A prefix that already opens an object must be closed, not re-wrapped
    // (double-`{` never parses).
    let stripped = strip_trailing_garbage(s);
    let salvaged = if stripped.starts_with('{') {
        close_open(&stripped)
    } else {
        format!("{{{stripped}}}")
    };
    serde_json::from_str::<Value>(&salvaged).unwrap_or(Value::Object(Default::default()))
}

fn close_open(s: &str) -> String {
    let mut out = s.to_string();
    // Drop a trailing fragment that cannot be part of a complete value:
    // e.g. `"key": "partial` or `"key": 12.` or a trailing `,`.
    loop {
        let trimmed = out.trim_end();
        if trimmed.ends_with(',') {
            out.truncate(trimmed.len() - 1);
            continue;
        }
        if trimmed.ends_with(':') {
            // dangling key — drop the key entirely is hard; neutralize value
            out.truncate(trimmed.len());
            out.push_str(" null");
            break;
        }
        if trimmed.ends_with('.') {
            out.truncate(trimmed.len() - 1);
            continue;
        }
        break;
    }
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escape = false;
    for ch in out.chars() {
        if in_string {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else {
            match ch {
                '"' => in_string = true,
                '{' | '[' => stack.push(ch),
                '}' | ']' => {
                    stack.pop();
                }
                _ => {}
            }
        }
    }
    if in_string {
        out.push('"');
    }
    while let Some(open) = stack.pop() {
        out.push(match open {
            '{' => '}',
            _ => ']',
        });
    }
    out
}

fn strip_trailing_garbage(s: &str) -> String {
    // Cut at the last complete `"..."` pair boundary we can find.
    match s.rfind('"') {
        Some(pos) if pos > 0 && !s[..pos].ends_with('\\') => s[..=pos].to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn valid_json_passes_through() {
        let v = repair_json("{\"a\": 1}");
        assert_eq!(v.get("a").and_then(Value::as_i64), Some(1));
    }

    #[test]
    fn empty_becomes_empty_object() {
        assert!(repair_json("").as_object().unwrap().is_empty());
        assert!(repair_json("   ").as_object().unwrap().is_empty());
    }

    #[test]
    fn closes_unbalanced_braces_and_trailing_comma() {
        let v = repair_json("{\"path\": \"src/main.rs\",");
        assert_eq!(v.get("path").and_then(Value::as_str), Some("src/main.rs"));
    }

    #[test]
    fn closes_open_string() {
        let v = repair_json("{\"note\": \"half written");
        assert_eq!(v.get("note").and_then(Value::as_str), Some("half written"));
    }

    #[test]
    fn nested_containers_closed() {
        let v = repair_json("{\"outer\": {\"inner\": [1, 2");
        let inner = v
            .get("outer")
            .unwrap()
            .get("inner")
            .cloned()
            .unwrap_or_default();
        assert!(inner.is_array());
    }

    #[test]
    fn garbage_becomes_empty_object() {
        assert!(
            repair_json("total nonsense !!")
                .as_object()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn dangling_key_neutralized() {
        let v = repair_json("{\"a\": 1, \"b\":");
        assert_eq!(v.get("a").and_then(Value::as_i64), Some(1));
    }

    #[test]
    fn truncated_mid_key_preserves_earlier_pairs() {
        // stream cut after the key but before the colon
        let v = repair_json("{\"a\": 1, \"b\"");
        assert_eq!(v.get("a").and_then(Value::as_i64), Some(1));
    }

    #[test]
    fn salvage_does_not_double_open() {
        // close_open cannot fix the juxtaposed garbage; salvage must close
        // the existing `{`, not wrap a second one around it
        let v = repair_json("{\"cmd\": \"run\" trailing junk");
        assert_eq!(v.get("cmd").and_then(Value::as_str), Some("run"));
    }
}
