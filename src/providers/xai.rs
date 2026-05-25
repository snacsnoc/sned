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
            provider_name: Some("xai".to_string()),
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
    // All xAI models support prompt caching at $0.20/1M tokens per xAI pricing docs
    let mut info = ModelInfo {
        name: Some(model_id.to_string()),
        max_tokens: Some(8192),
        context_window: Some(1_000_000),
        supports_images: Some(true),
        supports_prompt_cache: true,
        supports_reasoning: Some(false),
        input_price: Some(1.25),  // $1.25 / 1M tokens
        output_price: Some(2.50), // $2.50 / 1M tokens
        image_output_price: None,
        thinking_config: None,
        supports_global_endpoint: None,
        cache_writes_price: None,
        cache_reads_price: Some(0.20), // $0.20 / 1M cached tokens
        description: None,
        tiers: None,
        temperature: Some(0.7),
        supports_tools: Some(true),
        api_format: None,
    };

    // Model-specific overrides based on xAI pricing (https://docs.x.ai/developers/pricing)
    // grok-4.3 is the latest and recommended model - supports reasoning
    if model_id == "grok-4.3" || model_id == "grok-4.3-latest" {
        info.max_tokens = Some(8192);
        info.context_window = Some(1_000_000);
        info.supports_images = Some(true);
        info.supports_reasoning = Some(true);
        info.input_price = Some(1.25);
        info.output_price = Some(2.50);
    // grok-4.20 variants (reasoning and non-reasoning)
    } else if model_id == "grok-4.20-0309-reasoning" {
        info.max_tokens = Some(8192);
        info.context_window = Some(1_000_000);
        info.supports_images = Some(true);
        info.supports_reasoning = Some(true);
        info.input_price = Some(1.25);
        info.output_price = Some(2.50);
        info.cache_reads_price = Some(0.20);
    } else if model_id == "grok-4.20-0309-non-reasoning" {
        info.max_tokens = Some(8192);
        info.context_window = Some(1_000_000);
        info.supports_images = Some(true);
        info.supports_reasoning = Some(false);
        info.input_price = Some(1.25);
        info.output_price = Some(2.50);
        info.cache_reads_price = Some(0.20);
    // grok-4.20-multi-agent with 1M context (not 2M)
    } else if model_id == "grok-4.20-multi-agent-0309" {
        info.max_tokens = Some(8192);
        info.context_window = Some(1_000_000);
        info.supports_images = Some(true);
        info.supports_reasoning = Some(false);
        info.input_price = Some(1.25);
        info.output_price = Some(2.50);
        info.cache_reads_price = Some(0.20);
    // grok-build-0.1 with 256k context - supports reasoning
    } else if model_id == "grok-build-0.1" {
        info.max_tokens = Some(8192);
        info.context_window = Some(256_000);
        info.supports_images = Some(true);
        info.supports_reasoning = Some(true);
        info.input_price = Some(1.00);
        info.output_price = Some(2.00);
        info.cache_reads_price = Some(0.20);
    // Legacy model aliases - map to grok-4.3
    } else if model_id == "grok-3" || model_id == "grok-3-latest" {
        // grok-3 is aliased to grok-4.3
        info.name = Some("grok-4.3".to_string());
        info.max_tokens = Some(8192);
        info.context_window = Some(1_000_000);
        info.supports_images = Some(true);
        info.supports_reasoning = Some(true);
        info.input_price = Some(1.25);
        info.output_price = Some(2.50);
        info.cache_reads_price = Some(0.20);
    } else if model_id == "grok-3-mini" || model_id == "grok-3-mini-latest" {
        // grok-3-mini is aliased to grok-4.3
        info.name = Some("grok-4.3".to_string());
        info.max_tokens = Some(8192);
        info.context_window = Some(1_000_000);
        info.supports_images = Some(true);
        info.supports_reasoning = Some(true);
        info.input_price = Some(1.25);
        info.output_price = Some(2.50);
        info.cache_reads_price = Some(0.20);
    // Deprecated models (grok-2, grok-beta) - use grok-4.3 pricing/context
    } else if model_id == "grok-2" || model_id == "grok-2-latest" {
        info.max_tokens = Some(8192);
        info.context_window = Some(1_000_000);
        info.supports_images = Some(true);
        info.supports_reasoning = Some(false);
        info.input_price = Some(1.25);
        info.output_price = Some(2.50);
        info.cache_reads_price = Some(0.20);
    } else if model_id == "grok-2-mini" || model_id == "grok-2-mini-latest" {
        info.max_tokens = Some(8192);
        info.context_window = Some(1_000_000);
        info.supports_images = Some(true);
        info.supports_reasoning = Some(false);
        info.input_price = Some(1.25);
        info.output_price = Some(2.50);
        info.cache_reads_price = Some(0.20);
    } else if model_id == "grok-beta" {
        info.max_tokens = Some(8192);
        info.context_window = Some(1_000_000);
        info.supports_images = Some(true);
        info.supports_reasoning = Some(false);
        info.input_price = Some(1.25);
        info.output_price = Some(2.50);
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
        // grok-3 is aliased to grok-4.3 per xAI docs (grok-4.3 supports reasoning)
        let info = get_xai_model_info("grok-3");
        assert_eq!(info.base.max_tokens, Some(8192));
        assert_eq!(info.base.context_window, Some(1_000_000));
        assert_eq!(info.base.supports_images, Some(true));
        assert_eq!(info.base.supports_reasoning, Some(true));
        assert_eq!(info.base.input_price, Some(1.25));
        assert_eq!(info.base.output_price, Some(2.50));
        assert_eq!(info.base.temperature, Some(0.7));
        assert_eq!(info.base.cache_reads_price, Some(0.20));
    }

    #[test]
    fn test_xai_model_info_grok_3_mini() {
        // grok-3-mini is aliased to grok-4.3 per xAI docs
        let info = get_xai_model_info("grok-3-mini");
        assert_eq!(info.base.max_tokens, Some(8192));
        assert_eq!(info.base.context_window, Some(1_000_000));
        assert_eq!(info.base.supports_images, Some(true));
        assert_eq!(info.base.input_price, Some(1.25));
        assert_eq!(info.base.output_price, Some(2.50));
    }

    #[test]
    fn test_xai_model_info_grok_2() {
        // grok-2 uses grok-4.3 pricing per xAI docs
        let info = get_xai_model_info("grok-2");
        assert_eq!(info.base.max_tokens, Some(8192));
        assert_eq!(info.base.context_window, Some(1_000_000));
        assert_eq!(info.base.supports_images, Some(true));
        assert_eq!(info.base.input_price, Some(1.25));
        assert_eq!(info.base.output_price, Some(2.50));
    }

    #[test]
    fn test_xai_model_info_unknown() {
        // Default values based on xAI grok-4.3
        let info = get_xai_model_info("unknown-model");
        assert_eq!(info.base.max_tokens, Some(8192));
        assert_eq!(info.base.context_window, Some(1_000_000));
        assert_eq!(info.base.supports_images, Some(true));
    }
}
