//! Environment variable to provider detection for sned CLI.
//!
//! This module provides provider auto-detection based on available
//! environment variables. Secret and settings key mappings have been
//! moved to `storage/secrets.rs` to avoid duplication.

/// Get the best provider based on available environment variables.
///
/// Source: `dirac/src/shared/storage/env-config.ts` — `getProviderFromEnv()`
#[must_use]
pub fn get_provider_from_env() -> Option<&'static str> {
    if std::env::var("ANTHROPIC_API_KEY").is_ok() {
        return Some("anthropic");
    }
    if std::env::var("OPENROUTER_API_KEY").is_ok() {
        return Some("openrouter");
    }
    if std::env::var("OPENAI_API_BASE").is_ok() && std::env::var("OPENAI_API_KEY").is_ok() {
        return Some("openai");
    }
    if std::env::var("OPENAI_API_KEY").is_ok() {
        return Some("openai-native");
    }
    if std::env::var("GEMINI_API_KEY").is_ok() {
        return Some("gemini");
    }
    if std::env::var("MINIMAX_API_KEY").is_ok() || std::env::var("MINIMAX_CN_API_KEY").is_ok() {
        return Some("minimax");
    }
    if std::env::var("DEEPSEEK_API_KEY").is_ok() {
        return Some("deepseek");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn clear_test_env_vars() {
        let vars = [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "OPENAI_API_BASE",
            "GEMINI_API_KEY",
            "MINIMAX_API_KEY",
            "MINIMAX_CN_API_KEY",
            "DEEPSEEK_API_KEY",
            "OPENROUTER_API_KEY",
        ];
        for var in &vars {
            // SAFETY: single-threaded test; sequential env mutation
            unsafe {
                env::remove_var(var);
            }
        }
    }

    #[test]
    fn test_provider_detection() {
        // Single sequential test to avoid parallel env var interference

        // Test ANTHROPIC_API_KEY has highest priority
        clear_test_env_vars();
        // SAFETY: single-threaded test; sequential env mutation
        unsafe { env::set_var("ANTHROPIC_API_KEY", "sk-ant-test") };
        // SAFETY: single-threaded test; sequential env mutation
        unsafe { env::set_var("OPENAI_API_KEY", "sk-openai-test") };
        assert_eq!(get_provider_from_env(), Some("anthropic"));

        clear_test_env_vars();

        // Test OPENAI_API_KEY alone
        // SAFETY: single-threaded test; sequential env mutation
        unsafe { env::set_var("OPENAI_API_KEY", "sk-openai-test") };
        assert_eq!(get_provider_from_env(), Some("openai-native"));

        clear_test_env_vars();

        // Test no provider
        assert_eq!(get_provider_from_env(), None);

        clear_test_env_vars();

        // Test all providers in priority order
        let test_cases = vec![
            ("ANTHROPIC_API_KEY", "anthropic"),
            ("OPENROUTER_API_KEY", "openrouter"),
            ("OPENAI_API_KEY", "openai-native"),
            ("GEMINI_API_KEY", "gemini"),
            ("MINIMAX_API_KEY", "minimax"),
            ("DEEPSEEK_API_KEY", "deepseek"),
        ];

        for (env_var, expected_provider) in test_cases {
            clear_test_env_vars();
            // SAFETY: single-threaded test; sequential env mutation
            unsafe { env::set_var(env_var, "test-key") };
            assert_eq!(
                get_provider_from_env(),
                Some(expected_provider),
                "Failed for env var: {}",
                env_var
            );
        }

        clear_test_env_vars();

        // Test special mappings
        // MINIMAX_CN_API_KEY maps to minimax
        // SAFETY: single-threaded test; sequential env mutation
        unsafe { env::set_var("MINIMAX_CN_API_KEY", "test-key") };
        assert_eq!(get_provider_from_env(), Some("minimax"));

        clear_test_env_vars();

        // Test OPENAI_API_BASE alone (should return None without OPENAI_API_KEY)
        // SAFETY: single-threaded test; sequential env mutation
        unsafe { env::set_var("OPENAI_API_BASE", "https://custom.example.com/v1") };
        assert_eq!(get_provider_from_env(), None);

        clear_test_env_vars();

        // Test OPENAI_API_BASE + OPENAI_API_KEY together
        // SAFETY: single-threaded test; sequential env mutation
        unsafe { env::set_var("OPENAI_API_BASE", "https://custom.example.com/v1") };
        unsafe { env::set_var("OPENAI_API_KEY", "sk-test") };
        assert_eq!(get_provider_from_env(), Some("openai"));

        clear_test_env_vars();
    }
}
