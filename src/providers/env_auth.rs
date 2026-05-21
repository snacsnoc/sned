//! Environment variable to provider detection for sned CLI.
//!
//! This module provides provider auto-detection based on available
//! environment variables. Secret and settings key mappings have been
//! moved to `storage/secrets.rs` to avoid duplication.

/// Get the best provider based on available environment variables.
///
/// Source: `dirac/src/shared/storage/env-config.ts` — `getProviderFromEnv()`
pub fn get_provider_from_env() -> Option<&'static str> {
    if std::env::var("ANTHROPIC_API_KEY").is_ok() {
        return Some("anthropic");
    }
    if std::env::var("OPENROUTER_API_KEY").is_ok() {
        return Some("openrouter");
    }
    if std::env::var("OPENAI_API_KEY").is_ok() {
        return Some("openai-native");
    }
    if std::env::var("GEMINI_API_KEY").is_ok() {
        return Some("gemini");
    }

    if std::env::var("GOOGLE_CLOUD_PROJECT").is_ok() || std::env::var("GCP_PROJECT").is_ok() {
        return Some("vertex");
    }
    if std::env::var("AWS_ACCESS_KEY_ID").is_ok() || std::env::var("AWS_BEDROCK_MODEL").is_ok() {
        return Some("bedrock");
    }
    if std::env::var("GROQ_API_KEY").is_ok() {
        return Some("groq");
    }
    if std::env::var("XAI_API_KEY").is_ok() {
        return Some("xai");
    }
    if std::env::var("MISTRAL_API_KEY").is_ok() {
        return Some("mistral");
    }
    if std::env::var("MOONSHOT_API_KEY").is_ok() {
        return Some("moonshot");
    }
    if std::env::var("HF_TOKEN").is_ok() {
        return Some("huggingface");
    }
    if std::env::var("ZAI_API_KEY").is_ok() {
        return Some("zai");
    }
    if std::env::var("MINIMAX_API_KEY").is_ok() || std::env::var("MINIMAX_CN_API_KEY").is_ok() {
        return Some("minimax");
    }
    if std::env::var("CEREBRAS_API_KEY").is_ok() {
        return Some("cerebras");
    }
    if std::env::var("AI_GATEWAY_API_KEY").is_ok() {
        return Some("vercel-ai-gateway");
    }
    // KIMI_API_KEY is Moonshot AI's API key (Kimi is their product)
    if std::env::var("OPENCODE_API_KEY").is_ok() {
        return Some("openai-native");
    }
    if std::env::var("KIMI_API_KEY").is_ok() {
        return Some("moonshot");
    }
    if std::env::var("DEEPSEEK_API_KEY").is_ok() {
        return Some("deepseek");
    }
    if std::env::var("QWEN_API_KEY").is_ok() {
        return Some("qwen");
    }
    if std::env::var("TOGETHER_API_KEY").is_ok() {
        return Some("together");
    }
    if std::env::var("FIREWORKS_API_KEY").is_ok() {
        return Some("fireworks");
    }
    if std::env::var("NEBIUS_API_KEY").is_ok() {
        return Some("nebius");
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
            "GEMINI_API_KEY",
            "GROQ_API_KEY",
            "XAI_API_KEY",
            "MISTRAL_API_KEY",
            "MOONSHOT_API_KEY",
            "HF_TOKEN",
            "ZAI_API_KEY",
            "MINIMAX_API_KEY",
            "MINIMAX_CN_API_KEY",
            "CEREBRAS_API_KEY",
            "AI_GATEWAY_API_KEY",
            "OPENCODE_API_KEY",
            "KIMI_API_KEY",
            "DEEPSEEK_API_KEY",
            "QWEN_API_KEY",
            "TOGETHER_API_KEY",
            "FIREWORKS_API_KEY",
            "NEBIUS_API_KEY",
            "OPENROUTER_API_KEY",
            "AWS_ACCESS_KEY_ID",
            "AWS_BEDROCK_MODEL",
            "GOOGLE_CLOUD_PROJECT",
            "GCP_PROJECT",
        ];
        for var in &vars {
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
        unsafe { env::set_var("ANTHROPIC_API_KEY", "sk-ant-test") };
        unsafe { env::set_var("OPENAI_API_KEY", "sk-openai-test") };
        assert_eq!(get_provider_from_env(), Some("anthropic"));

        clear_test_env_vars();

        // Test OPENAI_API_KEY alone
        unsafe { env::set_var("OPENAI_API_KEY", "sk-openai-test") };
        assert_eq!(get_provider_from_env(), Some("openai-native"));

        clear_test_env_vars();

        // Test AWS Bedrock detection
        unsafe { env::set_var("AWS_ACCESS_KEY_ID", "AKIA...") };
        assert_eq!(get_provider_from_env(), Some("bedrock"));

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
            ("GROQ_API_KEY", "groq"),
            ("XAI_API_KEY", "xai"),
            ("MISTRAL_API_KEY", "mistral"),
            ("MOONSHOT_API_KEY", "moonshot"),
            ("HF_TOKEN", "huggingface"),
            ("ZAI_API_KEY", "zai"),
            ("DEEPSEEK_API_KEY", "deepseek"),
            ("QWEN_API_KEY", "qwen"),
            ("TOGETHER_API_KEY", "together"),
            ("FIREWORKS_API_KEY", "fireworks"),
            ("NEBIUS_API_KEY", "nebius"),
        ];

        for (env_var, expected_provider) in test_cases {
            clear_test_env_vars();
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
        unsafe { env::set_var("MINIMAX_CN_API_KEY", "test-key") };
        assert_eq!(get_provider_from_env(), Some("minimax"));

        clear_test_env_vars();

        // KIMI_API_KEY maps to moonshot (Kimi is Moonshot AI's product)
        unsafe { env::set_var("KIMI_API_KEY", "test-key") };
        assert_eq!(get_provider_from_env(), Some("moonshot"));

        clear_test_env_vars();

        // GCP_PROJECT maps to vertex
        unsafe { env::set_var("GCP_PROJECT", "my-project") };
        assert_eq!(get_provider_from_env(), Some("vertex"));

        clear_test_env_vars();
    }
}
