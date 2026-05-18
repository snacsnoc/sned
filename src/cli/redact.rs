//! Simple secret redaction for exports and logs.

use once_cell::sync::Lazy;
use regex::Regex;
use std::borrow::Cow;

/// Redact API keys and secrets from text.
pub fn redact_secrets(text: &str) -> Cow<'_, str> {
    API_KEY_PATTERN.replace_all(text, "[REDACTED]")
}

static API_KEY_PATTERN: Lazy<Regex> = Lazy::new(|| {
    // Matches sk-..., key-..., bearer tokens, and common API key patterns
    Regex::new(r"(sk-[a-zA-Z0-9]{20,}|key-[a-zA-Z0-9]{20,}|Bearer [a-zA-Z0-9\-_\.]{20,}|x-api-key:\s*[a-zA-Z0-9\-_]{20,})").unwrap()
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_redact_sk_key() {
        let input = "My API key is sk-abcdefghijklmnopqrstuvwxyz1234567890";
        let redacted = redact_secrets(input);
        assert!(redacted.contains("[REDACTED]"));
        assert!(!redacted.contains("sk-abcdef"));
    }

    #[test]
    fn test_no_false_positives() {
        let input = "This is a normal sentence with no secrets.";
        let redacted = redact_secrets(input);
        assert_eq!(redacted, input);
    }
}
