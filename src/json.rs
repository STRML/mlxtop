// SPDX-License-Identifier: MIT
//! Provider JSON extraction helpers that preserve missing measurements.
use serde_json::Value;
pub(crate) fn json_u64_paths_or_keys(
    value: &Value,
    paths: &[&[&str]],
    keys: &[&str],
) -> Option<u64> {
    paths
        .iter()
        .find_map(|path| json_u64(value, path))
        .or_else(|| keys.iter().find_map(|key| json_u64_key(value, key)))
}

pub(crate) fn json_string_paths_or_keys(
    value: &Value,
    paths: &[&[&str]],
    keys: &[&str],
) -> Option<String> {
    paths
        .iter()
        .find_map(|path| json_string(value, path))
        .or_else(|| keys.iter().find_map(|key| json_string_key(value, key)))
}

pub(crate) fn json_u64_key(value: &Value, key: &str) -> Option<u64> {
    match value {
        Value::Object(object) => object
            .get(key)
            .and_then(value_as_u64)
            .or_else(|| object.values().find_map(|child| json_u64_key(child, key))),
        Value::Array(values) => values.iter().find_map(|child| json_u64_key(child, key)),
        _ => None,
    }
}

pub(crate) fn json_string_key(value: &Value, key: &str) -> Option<String> {
    match value {
        Value::Object(object) => object
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                object
                    .values()
                    .find_map(|child| json_string_key(child, key))
            }),
        Value::Array(values) => values.iter().find_map(|child| json_string_key(child, key)),
        _ => None,
    }
}

pub(crate) fn json_f64_key(value: &Value, key: &str) -> Option<f64> {
    match value {
        Value::Object(object) => object
            .get(key)
            .and_then(value_as_f64)
            .or_else(|| object.values().find_map(|child| json_f64_key(child, key))),
        Value::Array(values) => values.iter().find_map(|child| json_f64_key(child, key)),
        _ => None,
    }
}

pub(crate) fn value_as_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_u64().map(|value| value as f64))
}

pub(crate) fn gib_to_bytes(value: f64) -> Option<u64> {
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let bytes = value * 1024_f64.powi(3);
    (bytes.is_finite() && bytes <= u64::MAX as f64).then_some(bytes.round() as u64)
}

pub(crate) fn value_as_u64(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|value| value.is_finite() && *value >= 0.0)
            .map(|value| value as u64)
    })
}

pub(crate) fn json_string(value: &Value, path: &[&str]) -> Option<String> {
    json_value(value, path)?.as_str().map(str::to_owned)
}

pub(crate) fn json_u64(value: &Value, path: &[&str]) -> Option<u64> {
    let value = json_value(value, path)?;
    value_as_u64(value)
}

pub(crate) fn json_f64(value: &Value, path: &[&str]) -> Option<f64> {
    let value = json_value(value, path)?;
    value
        .as_f64()
        .or_else(|| value.as_u64().map(|value| value as f64))
}

pub(crate) fn request_rate(request: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|key| json_f64(request, &[*key]))
        .filter(|value| value.is_finite() && *value >= 0.0)
}

pub(crate) fn json_value<'a>(mut value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    for key in path {
        value = value.get(*key)?;
    }
    Some(value)
}
