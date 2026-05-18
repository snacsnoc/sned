//! xAI (Grok) provider implementation for sned CLI.
//!
//! xAI provides an OpenAI-compatible API with Grok models.
//! Models: grok-3, grok-3-mini, grok-2, etc.

use crate::providers::{
    ModelInfo, OpenAiCompatibleModelInfo, Provider,
    openai::{OpenAiConfig, OpenAiProvider},
};
use anyhow::Result;

/// Configuration for the xAI provider.
#[derive(Debug, Clone)]
pub struct XaiConfig {
    pub api_key: String,
    pub model_id: String,
    pub model_info: Option<OpenAiCompatibleModelInfo>,
}

/// xAI (Grok) provider (OpenAI-compatible with custom base URL).
pub struct XaiProvider {
    inner: OpenAiProvider,
}

impl XaiProvider {
    pub fn new(config: XaiConfig) -> Result<Self> {
        let openai_config = OpenAiConfig {
            api_key: config.api_key,
            base_url: Some("https://api.x.ai/v1".to_string()),
            model_id: config.model_id,
            model_info: config.model_info,
            reasoning_effort: None,
            custom_headers: None,
        };

        let inner = OpenAiProvider::new(openai_config)?;
        Ok(Self { inner })
    }
}

#[async_trait::async_trait]
impl Provider for XaiProvider {
    fn name(&self) -> &str {
        "xai"
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

/// Get model info for known xAI (Grok) models.
pub fn get_xai_model_info(model_id: &str) -> OpenAiCompatibleModelInfo {
    // Default matching TS xaiModelInfo
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

    // Model-specific overrides based on xAI pricing
    // https://x.ai/api for latest pricing
    if model_id == "grok-3" || model_id == "grok-3-latest" {
        info.max_tokens = Some(8192);
        info.context_window = Some(128_000);
        info.supports_images = Some(true);
        info.supports_reasoning = Some(false);
        info.input_price = Some(3.0); // $3 / 1M tokens
        info.output_price = Some(15.0); // $15 / 1M tokens
        info.temperature = Some(0.7);
    } else if model_id == "grok-3-mini" || model_id == "grok-3-mini-latest" {
        info.max_tokens = Some(8192);
        info.context_window = Some(128_000);
        info.supports_images = Some(true);
        info.supports_reasoning = Some(false);
        info.input_price = Some(0.3); // $0.3 / 1M tokens
        info.output_price = Some(0.5); // $0.5 / 1M tokens
        info.temperature = Some(0.7);
    } else if model_id == "grok-2" || model_id == "grok-2-latest" {
        info.max_tokens = Some(8192);
        info.context_window = Some(128_000);
        info.supports_images = Some(true);
        info.supports_reasoning = Some(false);
        info.input_price = Some(2.0); // $2 / 1M tokens
        info.output_price = Some(10.0); // $10 / 1M tokens
        info.temperature = Some(0.7);
    } else if model_id == "grok-2-mini" || model_id == "grok-2-mini-latest" {
        info.max_tokens = Some(8192);
        info.context_window = Some(128_000);
        info.supports_images = Some(true);
        info.supports_reasoning = Some(false);
        info.input_price = Some(0.2); // $0.2 / 1M tokens
        info.output_price = Some(0.3); // $0.3 / 1M tokens
        info.temperature = Some(0.7);
    } else if model_id == "grok-beta" {
        info.max_tokens = Some(8192);
        info.context_window = Some(128_000);
        info.supports_images = Some(true);
        info.supports_reasoning = Some(false);
        info.input_price = Some(5.0); // $5 / 1M tokens
        info.output_price = Some(15.0); // $15 / 1M tokens
        info.temperature = Some(0.7);
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

/// Create an xAI provider from environment variables.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_xai_config() {
        let config = XaiConfig {
            api_key: "test-key".to_string(),
            model_id: "grok-3".to_string(),
            model_info: None,
        };
        let provider = XaiProvider::new(config).unwrap();
        assert_eq!(provider.name(), "xai");
    }

    #[test]
    fn test_xai_model_info_grok_3() {
        let info = get_xai_model_info("grok-3");
        assert_eq!(info.base.max_tokens, Some(8192));
        assert_eq!(info.base.context_window, Some(128_000));
        assert_eq!(info.base.supports_images, Some(true));
        assert_eq!(info.base.supports_reasoning, Some(false));
        assert_eq!(info.base.input_price, Some(3.0));
        assert_eq!(info.base.output_price, Some(15.0));
        assert_eq!(info.base.temperature, Some(0.7));
    }

    #[test]
    fn test_xai_model_info_grok_3_mini() {
        let info = get_xai_model_info("grok-3-mini");
        assert_eq!(info.base.max_tokens, Some(8192));
        assert_eq!(info.base.context_window, Some(128_000));
        assert_eq!(info.base.supports_images, Some(true));
        assert_eq!(info.base.input_price, Some(0.3));
        assert_eq!(info.base.output_price, Some(0.5));
    }

    #[test]
    fn test_xai_model_info_grok_2() {
        let info = get_xai_model_info("grok-2");
        assert_eq!(info.base.max_tokens, Some(8192));
        assert_eq!(info.base.context_window, Some(128_000));
        assert_eq!(info.base.supports_images, Some(true));
        assert_eq!(info.base.input_price, Some(2.0));
        assert_eq!(info.base.output_price, Some(10.0));
    }

    #[test]
    fn test_xai_model_info_unknown() {
        let info = get_xai_model_info("unknown-model");
        assert_eq!(info.base.max_tokens, Some(8192));
        assert_eq!(info.base.context_window, Some(32_768));
        assert_eq!(info.base.supports_images, Some(false));
    }
}
