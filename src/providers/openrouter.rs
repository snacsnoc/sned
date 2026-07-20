//! OpenRouter provider implementation for sned CLI.
//!
//! OpenRouter is a gateway provider providing unified access to 100+ models
//! from multiple providers (Anthropic, OpenAI, Google, Meta, etc.) through
//! a single API key.

use crate::providers::{
    ModelInfo, OpenAiCompatibleModelInfo, Provider, apply_qwen_model_profile,
    openai::{OpenAiConfig, OpenAiEndpointKind, OpenAiProvider},
};
use anyhow::Result;
use std::collections::HashMap;

/// Configuration for the OpenRouter provider.
#[derive(Clone)]
pub struct OpenRouterConfig {
    pub api_key: String,
    pub model_id: String,
    pub model_info: Option<OpenAiCompatibleModelInfo>,
    pub provider_sort: Option<String>,
    pub reasoning_effort: Option<String>,
    /// Provider name for error messages (defaults to "openrouter" if not set).
    pub provider_name: Option<String>,
}

impl std::fmt::Debug for OpenRouterConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenRouterConfig")
            .field(
                "api_key",
                &format!("***REDACTED ({} chars)***", self.api_key.len()),
            )
            .field("model_id", &self.model_id)
            .field("model_info", &self.model_info)
            .field("provider_sort", &self.provider_sort)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("provider_name", &self.provider_name)
            .finish()
    }
}

/// OpenRouter provider (OpenAI-compatible with custom headers and base URL).
#[derive(Debug)]
pub struct OpenRouterProvider {
    inner: OpenAiProvider,
}

impl OpenRouterProvider {
    pub fn new(config: OpenRouterConfig) -> Result<Self> {
        let provider_sort = config.provider_sort;
        let mut custom_headers = HashMap::with_capacity(4);
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
            reasoning_effort: config.reasoning_effort,
            custom_headers: Some(custom_headers),
            endpoint_kind: OpenAiEndpointKind::Compatible,
            provider_name: Some(
                config
                    .provider_name
                    .unwrap_or_else(|| "openrouter".to_string()),
            ),
        };

        let inner = OpenAiProvider::new(openai_config)?
            .with_provider_sort(provider_sort);
        Ok(Self { inner })
    }

    #[cfg(test)]
    pub(crate) fn build_request_body_for_test(
        &self,
        request: &crate::providers::ProviderRequest,
    ) -> Result<serde_json::Value> {
        self.inner.build_request_body(request)
    }
}

impl Provider for OpenRouterProvider {
    fn name(&self) -> &'static str {
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
#[must_use]
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
        top_p: None,
        top_k: None,
        supports_tools: Some(true),
        api_format: None,
    };

    if apply_qwen_model_profile(model_id, &mut info) {
        return OpenAiCompatibleModelInfo {
            base: info,
            is_r1_format_required: None,
            system_role: None,
            supports_reasoning_effort: Some(false),
            supports_streaming: Some(true),
        };
    }

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
            info.supports_reasoning = Some(true);
            info.description = Some(
                "Claude Sonnet 4.5 - superior intelligence across coding workflows".to_string(),
            );
        } else if model_id.contains("claude-sonnet-4") || model_id.contains("claude-4-sonnet") {
            info.max_tokens = Some(64_000);
            info.context_window = Some(200_000);
            info.input_price = Some(3.0);
            info.output_price = Some(15.0);
            info.cache_writes_price = Some(3.75);
            info.cache_reads_price = Some(0.3);
            info.supports_reasoning = Some(true);
        } else if model_id.contains("claude-opus-4") || model_id.contains("claude-4-opus") {
            info.max_tokens = Some(64_000);
            info.context_window = Some(200_000);
            info.input_price = Some(15.0);
            info.output_price = Some(75.0);
            info.cache_writes_price = Some(18.75);
            info.cache_reads_price = Some(1.5);
            info.supports_reasoning = Some(true);
        } else if model_id.contains("claude-3.7-sonnet") || model_id.contains("claude-3-7-sonnet") {
            info.max_tokens = Some(64_000);
            info.context_window = Some(200_000);
            info.input_price = Some(3.0);
            info.output_price = Some(15.0);
            info.cache_writes_price = Some(3.75);
            info.cache_reads_price = Some(0.3);
            info.supports_reasoning = Some(true);
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
        // OpenAI reasoning models (o-series)
        } else if model_id.contains("o1") || model_id.contains("o3") || model_id.contains("o4-mini")
        {
            info.max_tokens = Some(100_000);
            info.context_window = Some(200_000);
            info.input_price = Some(5.0);
            info.output_price = Some(15.0);
            info.supports_reasoning = Some(true);
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
        is_r1_format_required: None,
        system_role: None,
        supports_reasoning_effort: None,
        supports_streaming: Some(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_request() -> crate::providers::ProviderRequest {
        crate::providers::ProviderRequest {
            system_prompt: "You are helpful.".to_string(),
            messages: Vec::new(),
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        }
    }

    #[test]
    fn test_openrouter_config() {
        let config = OpenRouterConfig {
            api_key: "test-key".to_string(),
            model_id: "anthropic/claude-sonnet-4.5".to_string(),
            model_info: None,
            provider_sort: None,
            reasoning_effort: None,
            provider_name: None,
        };
        let provider = OpenRouterProvider::new(config).unwrap();
        assert_eq!(provider.name(), "openrouter");
    }

    #[test]
    fn test_openrouter_request_body_applies_sort_and_reasoning_effort() {
        for reasoning_effort in ["high", "none"] {
            let provider = OpenRouterProvider::new(OpenRouterConfig {
                api_key: "test-key".to_string(),
                model_id: "openai/gpt-5.4".to_string(),
                model_info: None,
                provider_sort: Some("throughput".to_string()),
                reasoning_effort: Some(reasoning_effort.to_string()),
                provider_name: None,
            })
            .unwrap();

            let body = provider
                .build_request_body_for_test(&test_request())
                .unwrap();

            assert_eq!(body["provider"]["sort"], "throughput");
            assert_eq!(body["reasoning_effort"], reasoning_effort);
        }
    }

    #[test]
    fn test_openrouter_provider_name() {
        let config = OpenRouterConfig {
            api_key: "test-key".to_string(),
            model_id: "anthropic/claude-sonnet-4.5".to_string(),
            model_info: None,
            provider_sort: None,
            reasoning_effort: None,
            provider_name: None,
        };
        let provider = OpenRouterProvider::new(config).unwrap();
        assert_eq!(provider.name(), "openrouter");
    }

    #[test]
    fn test_openrouter_name_ignores_custom_provider_name() {
        let config = OpenRouterConfig {
            api_key: "test-key".to_string(),
            model_id: "anthropic/claude-sonnet-4.5".to_string(),
            model_info: None,
            provider_sort: None,
            reasoning_effort: None,
            provider_name: Some("custom-openrouter".to_string()),
        };
        let provider = OpenRouterProvider::new(config).unwrap();
        assert_eq!(provider.name(), "openrouter");
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
    fn test_openrouter_model_info_qwen_family() {
        for model_id in [
            "qwen3.6-35b-a3b",
            "qwen/qwen3.6-35b-a3b",
            "qwen/qwen3.5-27b",
        ] {
            let info = get_openrouter_model_info(model_id);
            assert_eq!(info.base.context_window, Some(262_144));
            assert_eq!(info.base.max_tokens, Some(65_536));
            assert_eq!(info.base.supports_tools, Some(true));
            assert_eq!(info.base.supports_images, Some(false));
            assert!(!info.base.supports_prompt_cache);
            assert_eq!(info.base.supports_reasoning, Some(true));
            assert_eq!(info.supports_reasoning_effort, Some(false));
        }
    }

    #[test]
    fn test_openrouter_model_info_unknown() {
        let info = get_openrouter_model_info("unknown/model");
        assert_eq!(info.base.max_tokens, Some(8192));
        assert_eq!(info.base.context_window, Some(32_768));
        assert_eq!(info.base.supports_images, Some(false));
    }
}
