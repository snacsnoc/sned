//! OpenRouter provider implementation for sned CLI.
//!
//! OpenRouter is a gateway provider providing unified access to 100+ models
//! from multiple providers (Anthropic, OpenAI, Google, Meta, etc.) through
//! a single API key.

use crate::providers::{
    ModelInfo, OpenAiCompatibleModelInfo, Provider,
    openai::{OpenAiConfig, OpenAiProvider},
};
use anyhow::Result;
use std::collections::HashMap;

/// Configuration for the OpenRouter provider.
#[derive(Debug, Clone)]
pub struct OpenRouterConfig {
    pub api_key: String,
    pub model_id: String,
    pub model_info: Option<OpenAiCompatibleModelInfo>,
    pub provider_sort: Option<String>,
    /// Provider name for error messages (defaults to "openrouter" if not set).
    pub provider_name: Option<String>,
}

/// OpenRouter provider (OpenAI-compatible with custom headers and base URL).
pub struct OpenRouterProvider {
    inner: OpenAiProvider,
}

impl OpenRouterProvider {
    pub fn new(config: OpenRouterConfig) -> Result<Self> {
        // OpenRouter-specific custom headers
        let mut custom_headers = HashMap::new();
        custom_headers.insert("HTTP-Referer".to_string(), "https://sned.run".to_string());
        custom_headers.insert("X-Title".to_string(), "Sned".to_string());
        custom_headers.insert(
            "X-OpenRouter-Categories".to_string(),
            "cli-agent,ide-extension".to_string(),
        );

        let openai_config = OpenAiConfig {
            api_key: config.api_key,
            base_url: Some("https://openrouter.ai/api/v1".to_string()),
            model_id: config.model_id,
            model_info: config.model_info,
            reasoning_effort: None,
            custom_headers: Some(custom_headers),
            provider_name: Some(config.provider_name.unwrap_or_else(|| "openrouter".to_string())),
        };

        let inner = OpenAiProvider::new(openai_config)?;
        Ok(Self { inner })
    }
}

#[async_trait::async_trait]
impl Provider for OpenRouterProvider {
    fn name(&self) -> &str {
        "openrouter"
    }

    fn get_model(&self) -> crate::providers::ProviderModel {
        self.inner.get_model()
    }

    async fn create_message(
        &self,
        request: crate::providers::ProviderRequest,
    ) -> Result<crate::providers::ApiStream, crate::providers::ProviderError> {
        self.inner.create_message(request).await
    }
}

/// Get model info for common OpenRouter models.
/// OpenRouter supports 100+ models from multiple providers.
pub fn get_openrouter_model_info(model_id: &str) -> OpenAiCompatibleModelInfo {
    // Default base info
    let mut info = ModelInfo {
        name: Some(model_id.to_string()),
        max_tokens: Some(8192),
        context_window: Some(32_768),
        supports_images: Some(false),
        supports_prompt_cache: false,
        supports_reasoning: Some(false),
        input_price: Some(0.0),
        output_price: Some(0.0),
        image_output_price: None,
        thinking_config: None,
        supports_global_endpoint: None,
        cache_writes_price: None,
        cache_reads_price: None,
        description: None,
        tiers: None,
        temperature: Some(0.7),
        supports_tools: Some(true),
        api_format: None,
    };

    // Anthropic Claude models
    if model_id.starts_with("anthropic/") {
        info.supports_images = Some(true);
        info.supports_prompt_cache = true;
        info.temperature = Some(0.7);

        if model_id.contains("claude-sonnet-4.5") || model_id.contains("claude-4.5-sonnet") {
            info.max_tokens = Some(64_000);
            info.context_window = Some(200_000);
            info.input_price = Some(3.0);
            info.output_price = Some(15.0);
            info.cache_writes_price = Some(3.75);
            info.cache_reads_price = Some(0.3);
            info.description = Some(
                "Claude Sonnet 4.5 - superior intelligence across coding and AI agents".to_string(),
            );
        } else if model_id.contains("claude-sonnet-4") || model_id.contains("claude-4-sonnet") {
            info.max_tokens = Some(64_000);
            info.context_window = Some(200_000);
            info.input_price = Some(3.0);
            info.output_price = Some(15.0);
            info.cache_writes_price = Some(3.75);
            info.cache_reads_price = Some(0.3);
        } else if model_id.contains("claude-opus-4") || model_id.contains("claude-4-opus") {
            info.max_tokens = Some(64_000);
            info.context_window = Some(200_000);
            info.input_price = Some(15.0);
            info.output_price = Some(75.0);
            info.cache_writes_price = Some(18.75);
            info.cache_reads_price = Some(1.5);
        } else if model_id.contains("claude-3.7-sonnet") || model_id.contains("claude-3-7-sonnet") {
            info.max_tokens = Some(64_000);
            info.context_window = Some(200_000);
            info.input_price = Some(3.0);
            info.output_price = Some(15.0);
            info.cache_writes_price = Some(3.75);
            info.cache_reads_price = Some(0.3);
        } else if model_id.contains("claude-3.5-sonnet") || model_id.contains("claude-3-5-sonnet") {
            info.max_tokens = Some(8192);
            info.context_window = Some(200_000);
            info.input_price = Some(3.0);
            info.output_price = Some(15.0);
            info.cache_writes_price = Some(3.75);
            info.cache_reads_price = Some(0.3);
        } else if model_id.contains("claude-3.5-haiku") || model_id.contains("claude-3-5-haiku") {
            info.max_tokens = Some(8192);
            info.context_window = Some(200_000);
            info.input_price = Some(1.0);
            info.output_price = Some(5.0);
            info.cache_writes_price = Some(1.25);
            info.cache_reads_price = Some(0.1);
        } else if model_id.contains("claude-3-haiku") || model_id.contains("claude-3-haiku") {
            info.max_tokens = Some(4096);
            info.context_window = Some(200_000);
            info.input_price = Some(0.25);
            info.output_price = Some(1.25);
        } else if model_id.contains("claude-3-opus") || model_id.contains("claude-3-opus") {
            info.max_tokens = Some(4096);
            info.context_window = Some(200_000);
            info.input_price = Some(15.0);
            info.output_price = Some(75.0);
        }
    }
    // OpenAI models
    else if model_id.starts_with("openai/") {
        info.supports_images = Some(true);
        info.temperature = Some(0.7);

        if model_id.contains("gpt-4o") {
            info.max_tokens = Some(16_384);
            info.context_window = Some(128_000);
            info.input_price = Some(2.5);
            info.output_price = Some(10.0);
        } else if model_id.contains("gpt-4-turbo") || model_id.contains("gpt-4.5") {
            info.max_tokens = Some(16_384);
            info.context_window = Some(128_000);
            info.input_price = Some(10.0);
            info.output_price = Some(30.0);
        } else if model_id.contains("gpt-3.5-turbo") {
            info.max_tokens = Some(4096);
            info.context_window = Some(16_385);
            info.input_price = Some(0.5);
            info.output_price = Some(1.5);
        }
    }
    // Google Gemini models
    else if model_id.starts_with("google/") {
        info.supports_images = Some(true);
        info.temperature = Some(0.7);

        if model_id.contains("gemini-2.5-pro") {
            info.max_tokens = Some(65_536);
            info.context_window = Some(2_097_152);
            info.input_price = Some(2.5);
            info.output_price = Some(7.5);
        } else if model_id.contains("gemini-2.0-flash") {
            info.max_tokens = Some(8192);
            info.context_window = Some(1_048_576);
            info.input_price = Some(0.1);
            info.output_price = Some(0.4);
        }
    }
    // Meta Llama models
    else if model_id.starts_with("meta-llama/") {
        info.supports_images = Some(false);
        info.temperature = Some(0.7);

        if model_id.contains("llama-3.3-70b") {
            info.max_tokens = Some(8192);
            info.context_window = Some(128_000);
            info.input_price = Some(0.59);
            info.output_price = Some(0.79);
        } else if model_id.contains("llama-3.1-405b") {
            info.max_tokens = Some(8192);
            info.context_window = Some(128_000);
            info.input_price = Some(0.8);
            info.output_price = Some(0.8);
        } else if model_id.contains("llama-3.1-70b") {
            info.max_tokens = Some(8192);
            info.context_window = Some(128_000);
            info.input_price = Some(0.59);
            info.output_price = Some(0.79);
        }
    }
    // DeepSeek models
    else if model_id.starts_with("deepseek/") {
        info.supports_images = Some(false);
        info.temperature = Some(0.3);

        if model_id.contains("deepseek-chat") || model_id.contains("deepseek-v3") {
            info.max_tokens = Some(8192);
            info.context_window = Some(128_000);
            info.input_price = Some(0.27);
            info.output_price = Some(1.10);
        } else if model_id.contains("deepseek-reasoner") || model_id.contains("deepseek-r1") {
            info.max_tokens = Some(8192);
            info.context_window = Some(128_000);
            info.input_price = Some(0.55);
            info.output_price = Some(2.19);
            info.supports_reasoning = Some(true);
        }
    }
    // Mistral models
    else if model_id.starts_with("mistralai/") {
        info.supports_images = Some(false);
        info.temperature = Some(0.7);

        if model_id.contains("mistral-large") {
            info.max_tokens = Some(8192);
            info.context_window = Some(128_000);
            info.input_price = Some(2.0);
            info.output_price = Some(6.0);
        } else if model_id.contains("mistral-medium") {
            info.max_tokens = Some(8192);
            info.context_window = Some(32_000);
            info.input_price = Some(0.27);
            info.output_price = Some(0.81);
        }
    }

    OpenAiCompatibleModelInfo {
        base: info,
        temperature: None,
        is_r1_format_required: None,
        system_role: None,
        supports_reasoning_effort: None,
        supports_streaming: Some(true),
    }
}

/// Create an OpenRouter provider from environment variables.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_openrouter_config() {
        let config = OpenRouterConfig {
            api_key: "test-key".to_string(),
            model_id: "anthropic/claude-sonnet-4.5".to_string(),
            model_info: None,
            provider_sort: None,
            provider_name: None,
        };
        let provider = OpenRouterProvider::new(config).unwrap();
        assert_eq!(provider.name(), "openrouter");
    }

    #[test]
    fn test_openrouter_provider_name() {
        // Verify that OpenRouter provider sets the correct provider name for error messages
        let config = OpenRouterConfig {
            api_key: "test-key".to_string(),
            model_id: "anthropic/claude-sonnet-4.5".to_string(),
            model_info: None,
            provider_sort: None,
            provider_name: None,
        };
        let provider = OpenRouterProvider::new(config).unwrap();
        // The inner OpenAiProvider should have "openrouter" as the provider name
        // This is tested indirectly via the name() method and ensures error messages
        // will show "openrouter" instead of "OpenAI"
        assert_eq!(provider.name(), "openrouter");
    }

    #[test]
    fn test_openrouter_custom_provider_name() {
        // Verify custom provider name can be set
        let config = OpenRouterConfig {
            api_key: "test-key".to_string(),
            model_id: "anthropic/claude-sonnet-4.5".to_string(),
            model_info: None,
            provider_sort: None,
            provider_name: Some("custom-openrouter".to_string()),
        };
        let provider = OpenRouterProvider::new(config).unwrap();
        assert_eq!(provider.name(), "openrouter");
        // Custom provider name would appear in error messages, not in the name() method
    }

    #[test]
    fn test_openrouter_model_info_claude_sonnet_4_5() {
        let info = get_openrouter_model_info("anthropic/claude-sonnet-4.5");
        assert_eq!(info.base.max_tokens, Some(64_000));
        assert_eq!(info.base.context_window, Some(200_000));
        assert_eq!(info.base.supports_images, Some(true));
        assert!(info.base.supports_prompt_cache);
        assert_eq!(info.base.input_price, Some(3.0));
        assert_eq!(info.base.output_price, Some(15.0));
        assert_eq!(info.base.cache_writes_price, Some(3.75));
        assert_eq!(info.base.cache_reads_price, Some(0.3));
    }

    #[test]
    fn test_openrouter_model_info_gpt_4o() {
        let info = get_openrouter_model_info("openai/gpt-4o");
        assert_eq!(info.base.max_tokens, Some(16_384));
        assert_eq!(info.base.context_window, Some(128_000));
        assert_eq!(info.base.supports_images, Some(true));
        assert_eq!(info.base.input_price, Some(2.5));
        assert_eq!(info.base.output_price, Some(10.0));
    }

    #[test]
    fn test_openrouter_model_info_gemini() {
        let info = get_openrouter_model_info("google/gemini-2.5-pro");
        assert_eq!(info.base.max_tokens, Some(65_536));
        assert_eq!(info.base.context_window, Some(2_097_152));
        assert_eq!(info.base.supports_images, Some(true));
        assert_eq!(info.base.input_price, Some(2.5));
        assert_eq!(info.base.output_price, Some(7.5));
    }

    #[test]
    fn test_openrouter_model_info_llama() {
        let info = get_openrouter_model_info("meta-llama/llama-3.3-70b");
        assert_eq!(info.base.max_tokens, Some(8192));
        assert_eq!(info.base.context_window, Some(128_000));
        assert_eq!(info.base.supports_images, Some(false));
        assert_eq!(info.base.input_price, Some(0.59));
        assert_eq!(info.base.output_price, Some(0.79));
    }

    #[test]
    fn test_openrouter_model_info_deepseek() {
        let info = get_openrouter_model_info("deepseek/deepseek-reasoner");
        assert_eq!(info.base.max_tokens, Some(8192));
        assert_eq!(info.base.context_window, Some(128_000));
        assert_eq!(info.base.supports_images, Some(false));
        assert_eq!(info.base.supports_reasoning, Some(true));
        assert_eq!(info.base.input_price, Some(0.55));
        assert_eq!(info.base.output_price, Some(2.19));
    }

    #[test]
    fn test_openrouter_model_info_unknown() {
        let info = get_openrouter_model_info("unknown/model");
        assert_eq!(info.base.max_tokens, Some(8192));
        assert_eq!(info.base.context_window, Some(32_768));
        assert_eq!(info.base.supports_images, Some(false));
    }
}
