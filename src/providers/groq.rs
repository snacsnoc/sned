//! Groq provider implementation for sned CLI.
//!
//! Groq provides an OpenAI-compatible API with ultra-low-latency inference.
//! Models: llama-3.3-70b-versatile, llama-3.1-8b-instant.

use crate::providers::{
    ModelInfo, OpenAiCompatibleModelInfo, Provider,
    openai::{OpenAiConfig, OpenAiProvider},
};
use anyhow::Result;

/// Configuration for the Groq provider.
#[derive(Clone)]
pub struct GroqConfig {
    pub api_key: String,
    pub model_id: String,
    pub model_info: Option<OpenAiCompatibleModelInfo>,
}

impl std::fmt::Debug for GroqConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroqConfig")
            .field("api_key", &format!("***REDACTED ({} chars)***", self.api_key.len()))
            .field("model_id", &self.model_id)
            .field("model_info", &self.model_info)
            .finish()
    }
}

/// Groq provider (OpenAI-compatible with custom base URL).
pub struct GroqProvider {
    inner: OpenAiProvider,
}

impl GroqProvider {
    pub fn new(config: GroqConfig) -> Result<Self> {
        let openai_config = OpenAiConfig {
            api_key: config.api_key,
            base_url: Some("https://api.groq.com/openai/v1".to_string()),
            model_id: config.model_id,
            model_info: config.model_info,
            reasoning_effort: None,
            custom_headers: None,
            provider_name: Some("groq".to_string()),
        };

        let inner = OpenAiProvider::new(openai_config)?;
        Ok(Self { inner })
    }
}

#[async_trait::async_trait]
impl Provider for GroqProvider {
    fn name(&self) -> &str {
        "groq"
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

/// Get model info for known Groq models.
pub fn get_groq_model_info(model_id: &str) -> OpenAiCompatibleModelInfo {
    // Default matching TS groqModelInfo
    // All current Groq models have 131K context window
    let mut info = ModelInfo {
        name: Some(model_id.to_string()),
        max_tokens: Some(8192),
        context_window: Some(131_072),
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
    // Note: mixtral-8x7b-32768 and gemma2-9b-it were removed from Groq API (dead models)
    if model_id == "llama-3.3-70b-versatile" {
        info.max_tokens = Some(8192);
        info.context_window = Some(128_000);
        info.supports_images = Some(false);
        info.supports_reasoning = Some(false);
        info.input_price = Some(0.59); // $0.59 / 1M tokens
        info.output_price = Some(0.79); // $0.79 / 1M tokens
        info.temperature = Some(0.7);
    } else if model_id == "llama-3.1-8b-instant" {
        info.max_tokens = Some(8192);
        info.context_window = Some(128_000);
        info.supports_images = Some(false);
        info.supports_reasoning = Some(false);
        info.input_price = Some(0.05); // $0.05 / 1M tokens
        info.output_price = Some(0.08); // $0.08 / 1M tokens
        info.temperature = Some(0.7);
    // GPT-OSS reasoning models (openai/ prefix on Groq)
    } else if model_id == "openai/gpt-oss-120b" {
        info.max_tokens = Some(65_536);
        info.context_window = Some(131_072);
        info.supports_images = Some(false);
        info.supports_reasoning = Some(true);
        info.input_price = Some(0.15); // $0.15 / 1M tokens
        info.output_price = Some(0.60); // $0.60 / 1M tokens
        info.temperature = Some(0.7);
    } else if model_id == "openai/gpt-oss-20b" {
        info.max_tokens = Some(65_536);
        info.context_window = Some(131_072);
        info.supports_images = Some(false);
        info.supports_reasoning = Some(true);
        info.input_price = Some(0.075); // $0.075 / 1M tokens
        info.output_price = Some(0.30); // $0.30 / 1M tokens
        info.temperature = Some(0.7);
    } else if model_id == "openai/gpt-oss-safeguard-20b" {
        info.max_tokens = Some(65_536);
        info.context_window = Some(131_072);
        info.supports_images = Some(false);
        info.supports_reasoning = Some(true);
        info.input_price = Some(0.075); // $0.075 / 1M tokens
        info.output_price = Some(0.30); // $0.30 / 1M tokens
        info.temperature = Some(0.7);
    // Qwen reasoning model
    } else if model_id == "qwen/qwen3-32b" {
        info.max_tokens = Some(40_960);
        info.context_window = Some(131_072);
        info.supports_images = Some(false);
        info.supports_reasoning = Some(true);
        info.input_price = Some(0.29); // $0.29 / 1M tokens
        info.output_price = Some(0.59); // $0.59 / 1M tokens
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

/// Create a Groq provider from environment variables.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_groq_config() {
        let config = GroqConfig {
            api_key: "test-key".to_string(),
            model_id: "llama-3.3-70b-versatile".to_string(),
            model_info: None,
        };
        let provider = GroqProvider::new(config).unwrap();
        assert_eq!(provider.name(), "groq");
    }

    #[test]
    fn test_groq_model_info_llama_3_3() {
        let info = get_groq_model_info("llama-3.3-70b-versatile");
        assert_eq!(info.base.max_tokens, Some(8192));
        assert_eq!(info.base.context_window, Some(128_000));
        assert_eq!(info.base.supports_images, Some(false));
        assert_eq!(info.base.supports_reasoning, Some(false));
        assert_eq!(info.base.input_price, Some(0.59));
        assert_eq!(info.base.output_price, Some(0.79));
        assert_eq!(info.base.temperature, Some(0.7));
    }

    #[test]
    fn test_groq_model_info_llama_3_1_8b() {
        let info = get_groq_model_info("llama-3.1-8b-instant");
        assert_eq!(info.base.max_tokens, Some(8192));
        assert_eq!(info.base.context_window, Some(128_000));
        assert_eq!(info.base.supports_images, Some(false));
        assert_eq!(info.base.input_price, Some(0.05));
        assert_eq!(info.base.output_price, Some(0.08));
    }

    #[test]
    fn test_groq_model_info_unknown() {
        let info = get_groq_model_info("unknown-model");
        assert_eq!(info.base.max_tokens, Some(8192));
        assert_eq!(info.base.context_window, Some(131_072));
        assert_eq!(info.base.supports_images, Some(false));
    }

    #[test]
    fn test_groq_model_info_gpt_oss_120b() {
        let info = get_groq_model_info("openai/gpt-oss-120b");
        assert_eq!(info.base.max_tokens, Some(65_536));
        assert_eq!(info.base.context_window, Some(131_072));
        assert_eq!(info.base.supports_reasoning, Some(true));
        assert_eq!(info.base.input_price, Some(0.15));
        assert_eq!(info.base.output_price, Some(0.60));
    }

    #[test]
    fn test_groq_model_info_gpt_oss_20b() {
        let info = get_groq_model_info("openai/gpt-oss-20b");
        assert_eq!(info.base.max_tokens, Some(65_536));
        assert_eq!(info.base.context_window, Some(131_072));
        assert_eq!(info.base.supports_reasoning, Some(true));
        assert_eq!(info.base.input_price, Some(0.075));
        assert_eq!(info.base.output_price, Some(0.30));
    }

    #[test]
    fn test_groq_model_info_qwen3_32b() {
        let info = get_groq_model_info("qwen/qwen3-32b");
        assert_eq!(info.base.max_tokens, Some(40_960));
        assert_eq!(info.base.context_window, Some(131_072));
        assert_eq!(info.base.supports_reasoning, Some(true));
        assert_eq!(info.base.input_price, Some(0.29));
        assert_eq!(info.base.output_price, Some(0.59));
    }
}
