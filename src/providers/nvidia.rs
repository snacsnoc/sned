//! NVIDIA provider implementation for sned CLI.
//!
//! NVIDIA NIM exposes an OpenAI-compatible API. Model IDs are namespaced
//! (for example `meta/llama-3.1-8b-instruct`) and availability is
//! account/license dependent.

use crate::providers::{
    ModelInfo, OpenAiCompatibleModelInfo, Provider,
    openai::{OpenAiConfig, OpenAiEndpointKind, OpenAiProvider},
};
use anyhow::Result;

/// Default NVIDIA NIM endpoint for cloud-hosted inference.
pub const NVIDIA_DEFAULT_BASE_URL: &str = "https://integrate.api.nvidia.com/v1";

/// Configuration for the NVIDIA provider.
#[derive(Clone)]
pub struct NvidiaConfig {
    pub api_key: String,
    pub model_id: String,
    pub model_info: Option<OpenAiCompatibleModelInfo>,
    pub extra_body: Option<serde_json::Map<String, serde_json::Value>>,
    /// Overrides the default endpoint (self-hosted NIM, proxies).
    pub base_url: Option<String>,
}

impl std::fmt::Debug for NvidiaConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NvidiaConfig")
            .field(
                "api_key",
                &format!("***REDACTED ({} chars)***", self.api_key.len()),
            )
            .field("model_id", &self.model_id)
            .field("model_info", &self.model_info)
            .field("extra_body", &self.extra_body)
            .field("base_url", &self.base_url)
            .finish()
    }
}

/// NVIDIA provider (OpenAI-compatible with custom base URL).
#[derive(Debug)]
pub struct NvidiaProvider {
    inner: OpenAiProvider,
}

impl NvidiaProvider {
    pub fn new(config: NvidiaConfig) -> Result<Self> {
        let openai_config = OpenAiConfig {
            api_key: config.api_key,
            base_url: Some(
                config
                    .base_url
                    .unwrap_or_else(|| NVIDIA_DEFAULT_BASE_URL.to_string()),
            ),
            model_id: config.model_id,
            model_info: config.model_info,
            reasoning_effort: None,
            extra_body: config.extra_body,
            custom_headers: None,
            endpoint_kind: OpenAiEndpointKind::Compatible,
            stream: true,
            provider_name: Some("nvidia".to_string()),
        };

        let inner = OpenAiProvider::new(openai_config)?;
        Ok(Self { inner })
    }
}

impl Provider for NvidiaProvider {
    fn name(&self) -> &'static str {
        "nvidia"
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

/// Get model info for NVIDIA NIM models.
#[must_use]
pub fn get_nvidia_model_info(model_id: &str) -> OpenAiCompatibleModelInfo {
    // Namespaced IDs are account dependent; defaults stay permissive and
    // per-model overrides only pin what NIM documents.
    let mut info = ModelInfo {
        name: Some(model_id.to_string()),
        max_tokens: Some(4096),
        context_window: Some(128_000),
        supports_images: Some(false),
        supports_prompt_cache: false,
        supports_reasoning: Some(false),
        input_price: None,
        output_price: None,
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

    if model_id == "mistralai/mistral-7b-instruct-v0.3" {
        info.context_window = Some(32_000);
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

    fn test_config() -> NvidiaConfig {
        NvidiaConfig {
            api_key: "test-key".to_string(),
            model_id: "meta/llama-3.1-8b-instruct".to_string(),
            model_info: None,
            extra_body: None,
            base_url: None,
        }
    }

    #[test]
    fn test_nvidia_provider_name() {
        let provider = NvidiaProvider::new(test_config()).unwrap();
        assert_eq!(provider.name(), "nvidia");
    }

    #[test]
    fn test_nvidia_default_base_url() {
        let provider = NvidiaProvider::new(test_config()).unwrap();
        assert_eq!(provider.inner.base_url(), NVIDIA_DEFAULT_BASE_URL);
    }

    #[test]
    fn test_nvidia_base_url_override() {
        let mut config = test_config();
        config.base_url = Some("http://localhost:8000/v1".to_string());
        let provider = NvidiaProvider::new(config).unwrap();
        assert_eq!(provider.inner.base_url(), "http://localhost:8000/v1");
    }

    #[test]
    fn test_nvidia_model_info_defaults() {
        let info = get_nvidia_model_info("meta/llama-3.1-8b-instruct");
        assert_eq!(info.base.max_tokens, Some(4096));
        assert_eq!(info.base.context_window, Some(128_000));
        assert_eq!(info.base.supports_tools, Some(true));
        assert_eq!(info.base.temperature, Some(0.7));
    }

    #[test]
    fn test_nvidia_model_info_mistral_window() {
        let info = get_nvidia_model_info("mistralai/mistral-7b-instruct-v0.3");
        assert_eq!(info.base.context_window, Some(32_000));
    }

    #[test]
    fn test_nvidia_model_info_unknown_id() {
        let info = get_nvidia_model_info("some-org/future-model");
        assert_eq!(info.base.context_window, Some(128_000));
        assert_eq!(info.base.name.as_deref(), Some("some-org/future-model"));
    }

    #[test]
    fn test_nvidia_model_info_emits_no_prices() {
        let info = get_nvidia_model_info("meta/llama-3.1-8b-instruct");
        assert_eq!(info.base.input_price, None);
        assert_eq!(info.base.output_price, None);
    }
}
