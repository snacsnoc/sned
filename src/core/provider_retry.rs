//! Retry policy for provider API calls.
//!
//! The agent loop owns retry state so it can keep task-level flags accurate and
//! avoid duplicating request orchestration inside each provider implementation.

use crate::core::agent_types::TaskState;
use crate::core::context::context_window;
use crate::providers::{ApiStream, Provider, ProviderError, ProviderRequest};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::warn;

/// Log a retry status message (visible in logs, not terminal during TUI)
fn log_retry_status(retry_attempt: usize, delay: Duration, error: &ProviderError) {
    let delay_secs = delay.as_secs_f64();
    let error_summary = match error {
        ProviderError::NetworkError(_) => "network error",
        ProviderError::AuthenticationError(_) => "authentication failed",
        ProviderError::RateLimitError { .. } => "rate limited",
        ProviderError::InvalidRequest(_) => "invalid request",
        ProviderError::ApiError(_) => "API error",
        ProviderError::UnexpectedError(_) => "unexpected error",
    };
    tracing::info!(
        attempt = retry_attempt + 1,
        delay_secs = delay_secs,
        "⚠️ {} — retrying",
        error_summary
    );
}

/// Retry policy for provider API requests.
#[derive(Debug, Clone, Copy)]
pub struct RetryConfig {
    pub max_retries: usize,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay_ms: 1_000,
            max_delay_ms: 10_000,
        }
    }
}

/// Create a provider stream with retry semantics.
///
/// Retries are intentionally handled here instead of inside the provider
/// implementations so the agent loop can keep `TaskState` in sync with whether
/// a retry actually occurred.
pub async fn create_message_with_retry(
    provider: Arc<dyn Provider>,
    request: ProviderRequest,
    task_state: Arc<Mutex<TaskState>>,
    retry_config: RetryConfig,
    _json_output: bool,
) -> Result<ApiStream, ProviderError> {
    // Validate context window before sending request
    if let Err(msg) = context_window::validate_context_window(&request, provider.as_ref()) {
        return Err(ProviderError::InvalidRequest(msg));
    }

    let mut retry_attempt = 0usize;

    loop {
        match provider.create_message(request.clone()).await {
            Ok(stream) => return Ok(stream),
            Err(error) => {
                let Some(delay) = retry_delay_for_error(&error, retry_attempt, retry_config) else {
                    return Err(error);
                };

                if retry_attempt >= retry_config.max_retries {
                    return Err(error);
                }

                if retry_attempt == 0 {
                    let mut state = task_state.lock().await;
                    state.did_automatically_retry_failed_api_request = true;
                }

                warn!(
                    attempt = retry_attempt + 1,
                    delay_ms = delay.as_millis() as u64,
                    error = %error,
                    "provider request failed; retrying"
                );
                log_retry_status(retry_attempt, delay, &error);
                sleep(delay).await;
                retry_attempt += 1;
            }
        }
    }
}

fn retry_delay_for_error(
    error: &ProviderError,
    retry_attempt: usize,
    retry_config: RetryConfig,
) -> Option<Duration> {
    match error {
        ProviderError::NetworkError(_) => {
            Some(exponential_backoff_delay(retry_attempt, retry_config))
        }
        ProviderError::RateLimitError {
            retry_delay_ms: Some(ms),
            ..
        } => Some(Duration::from_millis(*ms).min(Duration::from_millis(retry_config.max_delay_ms))),
        ProviderError::RateLimitError {
            retry_delay_ms: None,
            ..
        } => Some(exponential_backoff_delay(retry_attempt, retry_config)),
        ProviderError::ApiError(msg) => {
            if let Some(pos) = msg.find("failed: ") {
                let remainder = &msg[pos..];
                if let Some(status_str) = remainder.split_whitespace().nth(1)
                    && let Ok(status) = status_str.parse::<u16>()
                    && is_retryable_status_code(status)
                {
                    return Some(exponential_backoff_delay(retry_attempt, retry_config));
                }
            }
            None
        }
        _ => None,
    }
}

fn is_retryable_status_code(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
}

fn exponential_backoff_delay(retry_attempt: usize, retry_config: RetryConfig) -> Duration {
    let factor = 1u64
        .checked_shl(retry_attempt.min(10) as u32)
        .unwrap_or(u64::MAX);
    let mut delay_ms = retry_config
        .base_delay_ms
        .saturating_mul(factor)
        .min(retry_config.max_delay_ms);
    if delay_ms >= 4 {
        let jitter_ms = fastrand::u64(0..=(delay_ms / 4));
        delay_ms = delay_ms
            .saturating_add(jitter_ms)
            .min(retry_config.max_delay_ms);
    }
    Duration::from_millis(delay_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ApiStreamChunk, ProviderModel};
    use async_trait::async_trait;
    use futures::StreamExt;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct RetryTestProvider {
        attempts: Arc<AtomicUsize>,
        fail_until: usize,
    }

    #[async_trait]
    impl Provider for RetryTestProvider {
        async fn create_message(
            &self,
            _request: ProviderRequest,
        ) -> Result<ApiStream, ProviderError> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if attempt < self.fail_until {
                return Err(ProviderError::RateLimitError {
                    message: "rate limited".to_string(),
                    retry_delay_ms: None,
                });
            }

            Ok(Box::pin(tokio_stream::iter(vec![ApiStreamChunk::Usage(
                crate::providers::ApiStreamUsageChunk {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_write_tokens: None,
                    cache_read_tokens: None,
                    reasoning_tokens: None,
                    thoughts_token_count: None,
                    total_cost: None,
                    stop_reason: None,
                    id: None,
                },
            )])))
        }

        fn get_model(&self) -> ProviderModel {
            ProviderModel {
                id: "test".to_string(),
                info: Default::default(),
            }
        }

        fn name(&self) -> &str {
            "test"
        }
    }

    #[test]
    fn retry_delay_for_rate_limit_error() {
        let error = ProviderError::RateLimitError {
            message: "rate limited".to_string(),
            retry_delay_ms: None,
        };
        let delay = retry_delay_for_error(&error, 0, RetryConfig::default());
        assert!(delay.is_some());
    }

    #[test]
    fn retry_delay_uses_retry_delay_ms_when_provided() {
        let error = ProviderError::RateLimitError {
            message: "rate limited".to_string(),
            retry_delay_ms: Some(5000),
        };
        let delay = retry_delay_for_error(&error, 0, RetryConfig::default());
        assert_eq!(delay.unwrap().as_millis(), 5000);
    }

    #[test]
    fn retry_delay_caps_at_max_delay_ms() {
        let error = ProviderError::RateLimitError {
            message: "rate limited".to_string(),
            retry_delay_ms: Some(50000),
        };
        let config = RetryConfig {
            max_retries: 3,
            base_delay_ms: 1000,
            max_delay_ms: 10000,
        };
        let delay = retry_delay_for_error(&error, 0, config);
        assert_eq!(delay.unwrap().as_millis(), 10000);
    }

    #[test]
    fn retry_delay_for_network_error() {
        let error = ProviderError::NetworkError("connection reset".to_string());
        let delay = retry_delay_for_error(&error, 0, RetryConfig::default());
        assert!(delay.is_some());
    }

    #[test]
    fn no_retry_for_auth_error() {
        let error = ProviderError::AuthenticationError("invalid key".to_string());
        let delay = retry_delay_for_error(&error, 0, RetryConfig::default());
        assert!(delay.is_none());
    }

    #[test]
    fn retry_api_error_with_503_service_unavailable() {
        let error = ProviderError::ApiError(
            "OpenAI POST https://api.example.com/v1/chat/completions failed: 503 Service Unavailable - {\"error\":{\"message\":\"No healthy backends\"}}".to_string(),
        );
        let delay = retry_delay_for_error(&error, 0, RetryConfig::default());
        assert!(delay.is_some(), "503 should be retryable; delay was None");
    }

    #[test]
    fn retry_api_error_with_500_internal_server_error() {
        let error = ProviderError::ApiError(
            "Anthropic POST https://api.anthropic.com/v1/messages failed: 500 Internal Server Error - {}".to_string(),
        );
        let delay = retry_delay_for_error(&error, 0, RetryConfig::default());
        assert!(delay.is_some(), "500 should be retryable");
    }

    #[test]
    fn retry_api_error_with_502_bad_gateway() {
        let error = ProviderError::ApiError(
            "MiniMax POST https://api.minimax.chat/anthropic/v1/messages failed: 502 Bad Gateway - upstream error".to_string(),
        );
        let delay = retry_delay_for_error(&error, 0, RetryConfig::default());
        assert!(delay.is_some(), "502 should be retryable");
    }

    #[test]
    fn retry_api_error_with_504_gateway_timeout() {
        let error = ProviderError::ApiError(
            "MiniMax POST https://api.minimax.chat/anthropic/v1/messages failed: 504 Gateway Timeout - upstream timeout".to_string(),
        );
        let delay = retry_delay_for_error(&error, 0, RetryConfig::default());
        assert!(delay.is_some(), "504 should be retryable");
    }

    #[test]
    fn retry_api_error_with_408_request_timeout() {
        let error = ProviderError::ApiError(
            "OpenAI POST https://api.example.com failed: 408 request_timeout - Request timed out"
                .to_string(),
        );
        let delay = retry_delay_for_error(&error, 0, RetryConfig::default());
        assert!(delay.is_some(), "408 should be retryable");
    }

    #[test]
    fn retry_api_error_with_429_rate_limit() {
        let error = ProviderError::ApiError(
            "Anthropic POST https://api.anthropic.com/v1/messages failed: 429 Too Many Requests - Rate limit exceeded".to_string(),
        );
        let delay = retry_delay_for_error(&error, 0, RetryConfig::default());
        assert!(delay.is_some(), "429 should be retryable");
    }

    #[test]
    fn no_retry_api_error_with_400_bad_request() {
        let error = ProviderError::ApiError(
            "OpenAI POST https://api.example.com failed: 400 Bad Request - invalid parameter"
                .to_string(),
        );
        let delay = retry_delay_for_error(&error, 0, RetryConfig::default());
        assert!(delay.is_none(), "400 should NOT be retryable");
    }

    #[test]
    fn no_retry_api_error_with_422_unprocessable() {
        let error = ProviderError::ApiError(
            "MiniMax POST https://api.minimax.chat/anthropic/v1/messages failed: 422 Unprocessable Entity - validation error".to_string(),
        );
        let delay = retry_delay_for_error(&error, 0, RetryConfig::default());
        assert!(delay.is_none(), "422 should NOT be retryable");
    }

    #[tokio::test]
    async fn retries_retryable_errors_and_sets_state_flag() {
        let provider = Arc::new(RetryTestProvider {
            attempts: Arc::new(AtomicUsize::new(0)),
            fail_until: 2,
        });
        let state = Arc::new(Mutex::new(TaskState::default()));
        let request = ProviderRequest {
            system_prompt: "system".to_string(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let stream = create_message_with_retry(
            provider,
            request,
            state.clone(),
            RetryConfig::default(),
            false,
        )
        .await
        .unwrap();
        let items: Vec<ApiStreamChunk> = stream.collect().await;

        assert_eq!(items.len(), 1);
        assert!(
            state
                .lock()
                .await
                .did_automatically_retry_failed_api_request
        );
    }
}
