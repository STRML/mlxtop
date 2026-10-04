// SPDX-License-Identifier: MIT
//! Numeric and unit parsing for host counters and completion summaries.
pub(crate) fn find_number(text: &str, needle: &str) -> Option<u64> {
    let start = text.find(needle)? + needle.len();
    let start = text[start..].find('=')? + start + 1;
    let digits: String = text[start..]
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

pub(crate) fn find_named_number(text: &str, key: &str) -> Option<u64> {
    find_named_numbers(text, key).into_iter().next()
}

pub(crate) fn find_named_numbers(text: &str, key: &str) -> Vec<u64> {
    let needle = format!("\"{key}\"");
    let mut values = Vec::new();
    let mut offset = 0;
    while let Some(relative) = text[offset..].find(&needle) {
        let start = offset + relative + needle.len();
        let rest = text[start..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            offset = start;
            continue;
        };
        let digits: String = rest
            .chars()
            .skip_while(|character| character.is_whitespace())
            .take_while(|character| character.is_ascii_digit())
            .collect();
        if let Ok(value) = digits.parse() {
            values.push(value);
        }
        offset = start;
    }
    values
}

pub(crate) fn find_named_string(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = text.find(&needle)? + needle.len();
    let rest = text[start..].trim_start().strip_prefix('=')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    let value = rest[..end].trim();
    (!value.is_empty()).then(|| value.to_owned())
}

pub(crate) fn parse_unit(value: &str) -> u64 {
    let trimmed = value.trim_matches(',');
    let split = trimmed
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(trimmed.len());
    let number = trimmed[..split].parse::<f64>().unwrap_or(0.0);
    let unit = trimmed[split..].to_ascii_uppercase();
    let multiplier = match unit.as_str() {
        "K" | "KB" => 1024.0,
        "M" | "MB" => 1024.0_f64.powi(2),
        "G" | "GB" => 1024.0_f64.powi(3),
        "T" | "TB" => 1024.0_f64.powi(4),
        _ => 1.0,
    };
    (number * multiplier) as u64
}
