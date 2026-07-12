//! DeepSeek provider implementation for sned CLI.
//!
//! DeepSeek provides an OpenAI-compatible API with custom base URL.
//! Models: deepseek-chat, deepseek-reasoner

use crate::providers::{
    ModelInfo, OpenAiCompatibleModelInfo, Provider,
    openai::{OpenAiConfig, OpenAiEndpointKind, OpenAiProvider},
};
use anyhow::Result;

/// Configuration for the DeepSeek provider.
#[derive(Clone)]
pub struct DeepSeekConfig {
    pub api_key: String,
    pub model_id: String,
    pub model_info: Option<OpenAiCompatibleModelInfo>,
}

impl std::fmt::Debug for DeepSeekConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeepSeekConfig")
            .field(
                "api_key",
                &format!("***REDACTED ({} chars)***", self.api_key.len()),
            )
            .field("model_id", &self.model_id)
            .field("model_info", &self.model_info)
            .finish()
    }
}

/// DeepSeek provider (OpenAI-compatible with custom base URL).
#[derive(Debug)]
pub struct DeepSeekProvider {
    inner: OpenAiProvider,
}

impl DeepSeekProvider {
    pub fn new(config: DeepSeekConfig) -> Result<Self> {
        let openai_config = OpenAiConfig {
            api_key: config.api_key,
            base_url: Some("https://api.deepseek.com".to_string()),
            model_id: config.model_id,
            model_info: config.model_info,
            reasoning_effort: None,
            custom_headers: None,
            endpoint_kind: OpenAiEndpointKind::Compatible,
            provider_name: Some("deepseek".to_string()),
        };

        let inner = OpenAiProvider::new(openai_config)?;
        Ok(Self { inner })
    }
}

impl Provider for DeepSeekProvider {
    fn name(&self) -> &'static str {
        "deepseek"
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

/// Get model info for known DeepSeek models.
#[must_use]
pub fn get_deepseek_model_info(model_id: &str) -> OpenAiCompatibleModelInfo {
    // Default matching TS deepseekModelInfo
    let mut info = ModelInfo {
        name: Some(model_id.to_string()),
        max_tokens: Some(4096),
        context_window: Some(64_000),
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

    // Model-specific overrides
    if model_id == "deepseek-chat" {
        info.max_tokens = Some(4096);
        info.context_window = Some(64_000);
        info.supports_images = Some(false);
        info.supports_reasoning = Some(false);
        info.input_price = Some(0.27); // $0.27 / 1M tokens
        info.output_price = Some(1.10); // $1.10 / 1M tokens
        info.temperature = Some(0.7);
    } else if model_id == "deepseek-reasoner" {
        info.max_tokens = Some(8192);
        info.context_window = Some(64_000);
        info.supports_images = Some(false);
        info.supports_reasoning = Some(true);
        info.input_price = Some(0.55); // $0.55 / 1M tokens
        info.output_price = Some(2.19); // $2.19 / 1M tokens
        info.temperature = None; // Reasoning model, no temperature
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

/// Create a DeepSeek provider from environment variables.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deepseek_config() {
        let config = DeepSeekConfig {
            api_key: "test-key".to_string(),
            model_id: "deepseek-chat".to_string(),
            model_info: None,
        };
        let provider = DeepSeekProvider::new(config).unwrap();
        assert_eq!(provider.name(), "deepseek");
    }

    #[test]
    fn test_deepseek_model_info_deepseek_chat() {
        let info = get_deepseek_model_info("deepseek-chat");
        assert_eq!(info.base.max_tokens, Some(4096));
        assert_eq!(info.base.context_window, Some(64_000));
        assert_eq!(info.base.supports_images, Some(false));
        assert_eq!(info.base.supports_reasoning, Some(false));
        assert_eq!(info.base.input_price, Some(0.27));
        assert_eq!(info.base.output_price, Some(1.10));
        assert_eq!(info.base.temperature, Some(0.7));
    }

    #[test]
    fn test_deepseek_model_info_deepseek_reasoner() {
        let info = get_deepseek_model_info("deepseek-reasoner");
        assert_eq!(info.base.max_tokens, Some(8192));
        assert_eq!(info.base.context_window, Some(64_000));
        assert_eq!(info.base.supports_images, Some(false));
        assert_eq!(info.base.supports_reasoning, Some(true));
        assert_eq!(info.base.input_price, Some(0.55));
        assert_eq!(info.base.output_price, Some(2.19));
        assert_eq!(info.base.temperature, None);
    }

    #[test]
    fn test_deepseek_model_info_unknown() {
        let info = get_deepseek_model_info("unknown-model");
        assert_eq!(info.base.max_tokens, Some(4096));
        assert_eq!(info.base.context_window, Some(64_000));
        assert_eq!(info.base.supports_images, Some(false));
    }

    #[test]
    fn test_deepseek_provider_name() {
        let config = DeepSeekConfig {
            api_key: "test-key".to_string(),
            model_id: "deepseek-chat".to_string(),
            model_info: None,
        };
        let provider = DeepSeekProvider::new(config).unwrap();
        assert_eq!(provider.name(), "deepseek");
    }

    #[test]
    fn test_deepseek_base_url() {
        // Verify DeepSeek base_url is exactly https://api.deepseek.com (no /v1 suffix)
        // This is correct per official DeepSeek API docs:
        // https://api.deepseek.com/chat/completions
        let config = DeepSeekConfig {
            api_key: "test-key".to_string(),
            model_id: "deepseek-chat".to_string(),
            model_info: None,
        };
        let provider = DeepSeekProvider::new(config).unwrap();
        // Access inner provider's base_url for verification
        assert_eq!(provider.inner.base_url(), "https://api.deepseek.com");
    }
}
