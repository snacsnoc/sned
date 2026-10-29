//! Simple secret redaction for exports and logs.

use regex::Regex;
use std::borrow::Cow;
use std::sync::LazyLock;

/// Redact API keys and secrets from text.
pub fn redact_secrets(text: &str) -> Cow<'_, str> {
    let tokens = API_KEY_PATTERN.replace_all(text, "[REDACTED]");
    match tokens {
        Cow::Borrowed(original) => SECRET_ASSIGNMENT_PATTERN.replace_all(original, "[REDACTED]"),
        Cow::Owned(redacted) => Cow::Owned(
            SECRET_ASSIGNMENT_PATTERN
                .replace_all(&redacted, "[REDACTED]")
                .into_owned(),
        ),
    }
}

static API_KEY_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:sk-[a-z0-9_-]{20,}|key-[a-z0-9_-]{20,}|nvapi-[a-z0-9_-]{20,}|AIza[a-z0-9_-]{20,}|bearer\s+[a-z0-9._~+/_-]{10,}=*|x-api-key\s*[:=]\s*[a-z0-9_-]{10,})").unwrap()
});

static SECRET_ASSIGNMENT_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b(?:[a-z0-9]+_)?(?:api_?key|access_?token|refresh_?token|client_?secret|password)["']?\s*[:=]\s*(?:"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|bearer\s+[^"'\s,;}]+|[^"'\s,;}]+)"#).unwrap()
});

/// Redact recognizable credentials and explicitly named secret fields.
pub fn redact_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => *text = redact_secrets(text).into_owned(),
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_json),
        serde_json::Value::Object(fields) => {
            let original = std::mem::take(fields);
            for (key, mut value) in original {
                let normalized = key.to_ascii_lowercase().replace('-', "_");
                if matches!(
                    normalized.as_str(),
                    "api_key"
                        | "apikey"
                        | "x_api_key"
                        | "authorization"
                        | "access_token"
                        | "refresh_token"
                        | "password"
                        | "secret"
                        | "client_secret"
                ) {
                    value = serde_json::Value::String("[REDACTED]".into());
                } else {
                    redact_json(&mut value);
                }
                let redacted_key = redact_secrets(&key).into_owned();
                let mut unique_key = redacted_key.clone();
                let mut suffix = 2usize;
                while fields.contains_key(&unique_key) {
                    unique_key = format!("{redacted_key} ({suffix})");
                    suffix += 1;
                }
                fields.insert(unique_key, value);
            }
        }
        _ => {}
    }
}

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

    #[test]
    fn redacts_provider_credentials_and_structured_fields() {
        let mut value = serde_json::json!({"message": "sk-ant-abcdefghijklmnopqrstuvwxyz123 nvapi-abcdefghijklmnopqrstuvwxyz123 AIzaabcdefghijklmnopqrstuvwxyz123", "nested": {"api_key": "short-secret", "Authorization": "Basic abc"}, "ordinary": "unchanged"});
        redact_json(&mut value);
        let text = value["message"].as_str().unwrap();
        assert_eq!(text.matches("[REDACTED]").count(), 3);
        assert_eq!(value["nested"]["api_key"], "[REDACTED]");
        assert_eq!(value["nested"]["Authorization"], "[REDACTED]");
        assert_eq!(value["ordinary"], "unchanged");
    }

    #[test]
    fn redacts_named_credentials_in_text() {
        let text = r#"MINIMAX_API_KEY=arbitraryvalue and {"api_key":"shortvalue"}"#;
        let redacted = redact_secrets(text);
        assert!(!redacted.contains("arbitraryvalue"));
        assert!(!redacted.contains("shortvalue"));
    }

    #[test]
    fn redacts_bearer_tokens_with_underscores() {
        for text in ["Bearer abc_defghijklmnop", "Bearer abcdefghijklmnop_secret"] {
            assert_eq!(redact_secrets(text), "[REDACTED]");
        }
    }

    #[test]
    fn redacts_complete_quoted_values_with_spaces_and_escapes() {
        for text in [
            r#"password="my secret phrase" rest"#,
            r#"password="my \"secret\" phrase" rest"#,
            "password='my secret phrase' rest",
        ] {
            assert_eq!(redact_secrets(text), "[REDACTED] rest");
        }
    }

    #[test]
    fn bearer_assignment_never_leaves_token_suffix() {
        for text in [
            "access_token: Bearer abcdefghijklmnop",
            "access_token=Bearer abc_defghijklmnop",
            "password=Bearer abcdefghijklmnop",
            "access_token: Bearer short",
        ] {
            let redacted = redact_secrets(text);
            assert!(!redacted.contains("abcdefghijklmnop"), "{redacted}");
            assert!(!redacted.contains("abc_defghijklmnop"), "{redacted}");
            assert!(!redacted.contains("short"), "{redacted}");
        }
    }

    #[test]
    fn redacts_object_keys_without_dropping_colliding_values() {
        let mut value = serde_json::json!({"[REDACTED]": "existing", "sk-abcdefghijklmnopqrstuvwxyz": "first", "sk-zyxwvutsrqponmlkjihgfedcba": "second"});
        redact_json(&mut value);
        let object = value.as_object().unwrap();
        assert_eq!(object.len(), 3);
        assert!(
            object
                .keys()
                .all(|key| !key.contains("abcdefghijklmnopqrstuvwxyz")
                    && !key.contains("zyxwvutsrqponmlkjihgfedcba"))
        );
        assert!(object.values().any(|value| value == "existing"));
        assert!(object.values().any(|value| value == "first"));
        assert!(object.values().any(|value| value == "second"));
    }
}
