//! Shared helpers for the stress-gate test binaries. Each `tests/*.rs`
//! compiles as its own crate — include with `mod common;`.

/// Parse a numeric JSON field robustly: `"key":123` and `"key": 123`
/// both work. Substring matching (`contains("\"reaped\":32")`) can
/// false-pass on a value-prefix collision (`"reaped":32` is contained in
/// `"reaped":320`) — an overcount is exactly the anomaly class these
/// gates hunt, so they must read the actual number.
pub fn json_num(content: &str, key: &str) -> u64 {
    let needle = format!("\"{key}\":");
    let start = content
        .find(&needle)
        .unwrap_or_else(|| panic!("missing {key}: {content}"))
        + needle.len();
    let rest = content[start..].trim_start();
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits
        .parse()
        .unwrap_or_else(|_| panic!("{key} is not a number: {content}"))
}
