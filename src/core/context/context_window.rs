//! Context window utilities.
//!

use crate::providers::{Provider, ProviderRequest};

const HARD_LIMIT: u64 = 1_000_000;

/// Estimate token count for a provider request.
/// Uses rough heuristic: ~4 chars per token for English text.
pub fn estimate_request_tokens(request: &ProviderRequest) -> u64 {
    let system_tokens = request.system_prompt.len() as f64 / 4.0;

    let message_tokens: f64 = request
        .messages
        .iter()
        .map(|msg| match &msg.content {
            crate::providers::MessageContent::Text(text) => text.len() as f64 / 4.0,
            crate::providers::MessageContent::UserBlocks(blocks) => blocks
                .iter()
                .map(|block| match block {
                    crate::providers::UserContentBlock::Text(t) => t.text.len() as f64 / 4.0,
                    crate::providers::UserContentBlock::ToolResult(tr) => match &tr.content {
                        crate::providers::ToolResultContent::Text(text) => text.len() as f64 / 4.0,
                        crate::providers::ToolResultContent::Blocks(blocks) => blocks
                            .iter()
                            .map(|b| match b {
                                crate::providers::ToolResultContentBlock::Text { text } => {
                                    text.len() as f64 / 4.0
                                }
                                crate::providers::ToolResultContentBlock::Image { source } => {
                                    match source {
                                        crate::providers::ImageSource::Base64 { data, .. } => {
                                            (data.len() as f64 / 4.0).max(1000.0)
                                        }
                                        crate::providers::ImageSource::Url { .. } => 1000.0,
                                    }
                                }
                            })
                            .sum(),
                    },
                    crate::providers::UserContentBlock::Image(img) => match &img.source {
                        crate::providers::ImageSource::Base64 { data, .. } => {
                            (data.len() as f64 / 3.0).clamp(85.0, 1700.0)
                        }
                        crate::providers::ImageSource::Url { .. } => 1000.0,
                    },
                    crate::providers::UserContentBlock::Document(doc) => match &doc.source {
                        crate::providers::DocumentSource::Text { text } => text.len() as f64 / 4.0,
                        crate::providers::DocumentSource::Base64 { data, .. } => {
                            (data.len() as f64 / 4.0).max(500.0)
                        }
                        crate::providers::DocumentSource::Url { .. } => 1000.0,
                    },
                })
                .sum(),
            crate::providers::MessageContent::AssistantBlocks(blocks) => blocks
                .iter()
                .map(|block| match block {
                    crate::providers::AssistantContentBlock::Text(t) => t.text.len() as f64 / 4.0,
                    crate::providers::AssistantContentBlock::ToolUse(tu) => {
                        serde_json::to_string(&tu.input)
                            .map(|s| s.len() as f64 / 4.0)
                            .unwrap_or(10.0)
                    }
                    crate::providers::AssistantContentBlock::Thinking(th) => {
                        th.thinking.len() as f64 / 4.0
                    }
                    crate::providers::AssistantContentBlock::RedactedThinking(rt) => {
                        rt.data.len() as f64 / 4.0
                    }
                    crate::providers::AssistantContentBlock::Image(img) => match &img.source {
                        crate::providers::ImageSource::Base64 { data, .. } => {
                            (data.len() as f64 / 4.0).max(1000.0)
                        }
                        crate::providers::ImageSource::Url { .. } => 1000.0,
                    },
                    crate::providers::AssistantContentBlock::Document(doc) => match &doc.source {
                        crate::providers::DocumentSource::Base64 { data, .. } => {
                            (data.len() as f64 / 4.0).max(500.0)
                        }
                        crate::providers::DocumentSource::Text { text } => text.len() as f64 / 4.0,
                        crate::providers::DocumentSource::Url { .. } => 500.0,
                    },
                })
                .sum(),
        })
        .sum();

    let tool_def_tokens: f64 = request
        .tools
        .as_ref()
        .map(|tools| {
            tools
                .iter()
                .map(|t| {
                    t.function.name.len() as f64 / 4.0
                        + t.function.description.len() as f64 / 4.0
                        + serde_json::to_string(&t.function.parameters)
                            .map(|s| s.len() as f64 / 4.0)
                            .unwrap_or(10.0)
                })
                .sum()
        })
        .unwrap_or(0.0);

    (system_tokens + message_tokens + tool_def_tokens) as u64
}

/// Validate that a request fits within the provider's context window.
/// Returns Ok(()) if valid, or Err with a descriptive message if it exceeds limits.
pub fn validate_context_window(
    request: &ProviderRequest,
    provider: &dyn Provider,
) -> Result<(), String> {
    let model_info = provider.get_model().info;
    let context_window = model_info.context_window.unwrap_or(256_000);
    let max_allowed = get_context_window_info(provider).max_allowed_size;

    let estimated_tokens = estimate_request_tokens(request);

    if estimated_tokens > max_allowed {
        return Err(format!(
            "Request size ({}) exceeds provider context limit ({} / {} max_allowed)",
            estimated_tokens, context_window, max_allowed
        ));
    }

    Ok(())
}

/// Information about the context window for a given provider.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContextWindowInfo {
    /// The raw context window size reported by the model.
    pub context_window: u64,
    /// The effective maximum allowed size (with safety margin).
    pub max_allowed_size: u64,
}

/// Gets context window information for the given provider.
///
/// Calculates the max allowed size as `min(HARD_LIMIT, max(context_window - 40_000, context_window * 0.8))`.
pub fn get_context_window_info(provider: &dyn Provider) -> ContextWindowInfo {
    let context_window = provider.get_model().info.context_window.unwrap_or(256_000);

    let max_allowed_size = (HARD_LIMIT as f64)
        .min((context_window as f64 - 40_000.0).max(context_window as f64 * 0.8))
        as u64;

    ContextWindowInfo {
        context_window,
        max_allowed_size,
    }
}

/// Calculates context usage percentage based on total tokens used.
///
/// For OpenAI-compatible providers (OpenAI, MiniMax, DeepSeek, Groq, xAI, OpenRouter),
/// `tokens_in` already includes cache tokens, so cache_writes/cache_reads are not added separately.
/// For Gemini, `input_tokens` has cache reads already subtracted by the provider.
/// For Anthropic, cache tokens are reported separately and are added.
///
/// Returns percentage (0.0-100.0) of context window consumed.
pub fn calculate_context_usage_percentage(
    tokens_in: u32,
    tokens_out: u32,
    cache_writes: Option<u32>,
    cache_reads: Option<u32>,
    context_window: u64,
    _provider_name: &str,
) -> f64 {
    // All providers report input_tokens without cache tokens:
    // - OpenAI-compatible: input_tokens = prompt_tokens - cached_tokens
    // - Anthropic: input_tokens excludes cache separately
    // - Gemini: input_tokens = prompt_tokens - cached_tokens
    let cache_tokens =
        cache_writes.unwrap_or(0) as u64 + cache_reads.unwrap_or(0) as u64;

    let total_tokens = tokens_in as u64 + tokens_out as u64 + cache_tokens;

    if context_window == 0 {
        return 0.0;
    }

    (total_tokens as f64 / context_window as f64) * 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ModelInfo, ProviderError, ProviderModel};
    use async_trait::async_trait;

    struct MockProvider {
        context_window: Option<u64>,
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn create_message(
            &self,
            _request: crate::providers::ProviderRequest,
        ) -> Result<crate::providers::ApiStream, ProviderError> {
            panic!("MockProvider::create_message should not be called in this test")
        }

        fn get_model(&self) -> ProviderModel {
            ProviderModel {
                id: "test-model".to_string(),
                info: ModelInfo {
                    context_window: self.context_window,
                    ..Default::default()
                },
            }
        }

        fn name(&self) -> &str {
            "mock"
        }
    }

    #[test]
    fn test_default_context_window() {
        let provider = MockProvider {
            context_window: None,
        };
        let info = get_context_window_info(&provider);
        assert_eq!(info.context_window, 256_000);
        let expected = f64::max(256_000.0 - 40_000.0, 256_000.0 * 0.8) as u64;
        assert_eq!(info.max_allowed_size, expected);
    }

    #[test]
    fn test_large_context_window() {
        let provider = MockProvider {
            context_window: Some(2_000_000),
        };
        let info = get_context_window_info(&provider);
        assert_eq!(info.context_window, 2_000_000);
        assert_eq!(info.max_allowed_size, HARD_LIMIT);
    }

    #[test]
    fn test_small_context_window() {
        let provider = MockProvider {
            context_window: Some(64_000),
        };
        let info = get_context_window_info(&provider);
        assert_eq!(info.context_window, 64_000);
        let expected = f64::max(64_000.0 - 40_000.0, 64_000.0 * 0.8) as u64;
        assert_eq!(info.max_allowed_size, expected);
    }

    #[test]
    fn test_calculate_context_usage_percentage() {
        assert_eq!(
            calculate_context_usage_percentage(1000, 500, None, None, 100_000, "anthropic"),
            1.5
        );
        assert_eq!(
            calculate_context_usage_percentage(50_000, 50_000, None, None, 200_000, "anthropic"),
            50.0
        );
        assert_eq!(
            calculate_context_usage_percentage(
                100_000,
                100_000,
                Some(10_000),
                Some(5_000),
                200_000,
                "anthropic"
            ),
            107.5
        );
        assert_eq!(
            calculate_context_usage_percentage(0, 0, None, None, 100_000, "anthropic"),
            0.0
        );
        assert_eq!(
            calculate_context_usage_percentage(1000, 500, None, None, 0, "anthropic"),
            0.0
        );
    }

    #[test]
    fn test_cache_counted_for_openai() {
        let with_cache = calculate_context_usage_percentage(
            50_000,
            10_000,
            Some(20_000),
            Some(15_000),
            200_000,
            "openai",
        );
        let without_cache =
            calculate_context_usage_percentage(50_000, 10_000, None, None, 200_000, "openai");
        assert!(
            with_cache > without_cache,
            "for OpenAI, cache tokens should be counted (input_tokens excludes cache)"
        );
    }

    #[test]
    fn test_cache_counted_for_anthropic() {
        let with_cache = calculate_context_usage_percentage(
            50_000,
            10_000,
            Some(20_000),
            Some(15_000),
            200_000,
            "anthropic",
        );
        let without_cache =
            calculate_context_usage_percentage(50_000, 10_000, None, None, 200_000, "anthropic");
        assert!(
            with_cache > without_cache,
            "for Anthropic, cache tokens should be counted separately"
        );
    }
}
