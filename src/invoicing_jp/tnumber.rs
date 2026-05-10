//! Japanese qualified-invoice registration number (T+13 digits) helpers.

use std::sync::OnceLock;

use regex::Regex;

fn full_match() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^T\d{13}$").expect("static regex compiles"))
}

#[allow(dead_code)] // Used by `extract`, which is wired in a later PR.
fn anywhere() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"T\d{13}").expect("static regex compiles"))
}

/// `true` if `s` is exactly `T` followed by 13 digits.
pub fn is_valid_format(s: &str) -> bool {
    full_match().is_match(s)
}

/// Extract every `T\d{13}` substring from `text`, deduplicated, in
/// first-occurrence order.
#[allow(dead_code)] // Used by `verify-tnumber` once the NTA client lands.
pub fn extract(text: &str) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for m in anywhere().find_iter(text) {
        let s = m.as_str().to_string();
        if seen.insert(s.clone()) {
            out.push(s);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_format_examples() {
        assert!(is_valid_format("T1234567890123"));
        assert!(!is_valid_format("T123"));
        assert!(!is_valid_format("1234567890123"));
        assert!(!is_valid_format("T12345678901234")); // 14 digits
        assert!(!is_valid_format(" T1234567890123")); // whitespace prefix
    }

    #[test]
    fn extract_deduplicates_in_order() {
        let text = "登録番号 T1111111111111 のとなり T2222222222222、再掲 T1111111111111 にて。";
        assert_eq!(
            extract(text),
            vec!["T1111111111111".to_string(), "T2222222222222".to_string()]
        );
    }

    #[test]
    fn extract_returns_empty_when_none() {
        assert!(extract("プレーンテキスト").is_empty());
    }
}
