//! Capacity signals are error semantics, not arbitrary diagnostic field names.

use serde_json::Value;

pub(super) fn in_error(value: &Value) -> bool {
    let mut remaining = 64;
    inspect(value, 0, &mut remaining)
}

fn inspect(value: &Value, depth: usize, remaining: &mut usize) -> bool {
    if depth > 4 || *remaining == 0 {
        return false;
    }
    *remaining -= 1;
    match value {
        Value::String(text) => has_marker(text),
        Value::Array(values) => values
            .iter()
            .take(8)
            .any(|value| inspect(value, depth + 1, remaining)),
        Value::Object(fields) => {
            // Inspect primary fields independently; a large details/debug field
            // must not consume the budget before message or code is examined.
            if [
                "message",
                "detail",
                "description",
                "code",
                "type",
                "error_type",
                "reason",
            ]
            .iter()
            .filter_map(|key| fields.get(*key).and_then(Value::as_str))
            .any(has_marker)
                || fields
                    .iter()
                    .any(|(key, value)| is_flag_name(key) && affirmative(value))
            {
                return true;
            }
            // Only error containers are traversed. Echoed input, arbitrary
            // metadata and unrelated field names cannot enable retries.
            ["error", "errors", "cause", "details"]
                .iter()
                .filter_map(|key| fields.get(*key))
                .any(|value| inspect(value, depth + 1, remaining))
        }
        _ => false,
    }
}

fn affirmative(value: &Value) -> bool {
    value.as_bool() == Some(true)
        || value.as_u64() == Some(1)
        || value
            .as_str()
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("true") || value.trim() == "1")
}

fn is_flag_name(key: &str) -> bool {
    let mut words = key
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty());
    matches!((words.next(), words.next(), words.next()), (Some(first), Some(second), None)
        if capacity_pair(first, second))
}

fn has_marker(text: &str) -> bool {
    // Borrow tokens instead of copying/truncating the error into one shared
    // buffer. Input size is already bounded by the provider response reader.
    let mut previous = "";
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .any(|word| {
            let matched = capacity_pair(previous, word);
            previous = word;
            matched
        })
}

fn capacity_pair(first: &str, second: &str) -> bool {
    (second.eq_ignore_ascii_case("capacity")
        && (first.eq_ignore_ascii_case("at") || first.eq_ignore_ascii_case("insufficient")))
        || (first.eq_ignore_ascii_case("high") && second.eq_ignore_ascii_case("demand"))
}
