//! Privacy boundary for automatic names copied into list/search metadata.
use std::sync::LazyLock;

use regex::Regex;

static PRIVATE_TITLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(?:password|passwd|pwd|secret|token|api[_ -]?key|парол[ьья]|токен)(?:\s*["']?\s*[:=]|\s+\S+)|\bbearer\s+\S+|-----BEGIN [\w ]*PRIVATE KEY-----|[\w.+-]+@[\w.-]+\.[a-z]{2,}|https?://[^\s]*[?@#]|\b(?:sk-|ghp_|github_pat_)[a-z0-9_-]{12,}"#,
    )
    .expect("static automatic-title privacy expression is valid")
});

/// Prefer a harmless placeholder to copying credential-bearing prompts into
/// sidebar, notification, and search metadata. Explicit user renames are owned
/// by the user and do not pass through this automatic-title policy.
pub(super) fn safe_derived_title(title: &str) -> String {
    if title.len() > 4096 || PRIVATE_TITLE.is_match(title) {
        return "New Chat".to_owned();
    }
    title
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(120)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::safe_derived_title;

    #[test]
    fn private_automatic_names_are_not_truncated_secrets() {
        for title in [
            "Inspect fixture password: example-secret",
            "Inspect fixture\n\"password\": \"example secret\"",
            "Проверь стенд, пароль: example-secret",
            "Authorization: Bearer example-secret",
            "Inspect person@example.test",
            "Inspect https://example.test/callback?code=example-secret",
            "Inspect https://person:example-secret@example.test",
            "Use ghp_abcdefghijklmnop",
            "Fixture password example-secret",
            "Стенд пароль example-secret",
            "Fix password reset",
        ] {
            assert_eq!(safe_derived_title(title), "New Chat");
        }
    }

    #[test]
    fn ordinary_titles_remain_useful_and_unicode_safe() {
        assert_eq!(safe_derived_title("Fix login flow"), "Fix login flow");
        assert_eq!(safe_derived_title("  QA fixture\nDetails"), "QA fixture");
        assert_eq!(safe_derived_title(&"я".repeat(130)).chars().count(), 120);
        assert_eq!(safe_derived_title(""), "");
    }
}
