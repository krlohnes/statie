//! Shared helper: turn an agent state (and optionally the latest
//! observation) into a plain-text BM25 query.

use serde::Serialize;
use serde_json::Value;

use statie_core::Observation;

/// All string leaves of `serde_json::to_value(state)` (walking objects and
/// arrays), joined by spaces, plus the last 800 chars of the observation
/// text if present.
pub fn query_text<W: Serialize>(state: &W, obs: Option<&Observation>) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Ok(value) = serde_json::to_value(state) {
        collect_string_leaves(&value, &mut parts);
    }

    if let Some(obs) = obs {
        let text = obs.as_str();
        let tail: String = tail_chars(text, 800);
        if !tail.is_empty() {
            parts.push(tail);
        }
    }

    parts.join(" ")
}

fn collect_string_leaves(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => {
            for item in items {
                collect_string_leaves(item, out);
            }
        }
        Value::Object(map) => {
            for v in map.values() {
                collect_string_leaves(v, out);
            }
        }
        _ => {}
    }
}

/// The last `n` chars of `s` (char-boundary safe).
fn tail_chars(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let start = chars.len().saturating_sub(n);
    chars[start..].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn walks_objects_and_arrays_joining_string_leaves() {
        let state = json!({"a": {"b": "hello"}, "c": ["x", "y"], "n": 3});
        assert_eq!(query_text(&state, None), "hello x y");
    }

    #[test]
    fn ignores_numbers_and_bools() {
        let state = json!({"a": 1, "b": true, "c": "keep"});
        assert_eq!(query_text(&state, None), "keep");
    }
}
