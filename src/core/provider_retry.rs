//! Retry policy for provider API calls.
//!
//! The agent loop owns retry state so it can keep task-level flags accurate and
//! avoid duplicating request orchestration inside each provider implementation.

use crate::cli::output::OutputEvent;
use crate::core::agent_types::TaskState;
use crate::core::context::context_window;
use crate::providers::{ApiStream, Provider, ProviderError, ProviderRequest, Providers};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::warn;

/// Log a retry status message (visible in logs and TUI output pane)
fn log_retry_status(
    retry_attempt: usize,
    max_retries: usize,
    delay: Duration,
    error: &ProviderError,
    output_writer: Option<&crate::cli::output::OutputWriterArc>,
) {
    let delay_secs = delay.as_secs_f64();
    let error_summary = match error {
        ProviderError::NetworkError(_) => "network error",
        ProviderError::AuthenticationError(_) => "authentication failed",
        ProviderError::RateLimitError { .. } => "rate limited",
        ProviderError::InvalidRequest(_) => "invalid request",
        ProviderError::ApiError(_) => "API error",
        ProviderError::UnexpectedError(_) => "unexpected error",
    };
    let msg = format!(
        "⚠️ {} — retrying (attempt {}/{}, delay: {:.1}s)",
        error_summary,
        retry_attempt + 1,
        max_retries,
        delay_secs
    );
    tracing::info!("{}", msg);
    if let Some(writer) = output_writer {
        writer.emit(OutputEvent::dim_yellow(&msg));
    }
}

/// Default max consecutive provider failures before pausing the agent loop.
pub const DEFAULT_MAX_CONSECUTIVE_PROVIDER_FAILURES: u32 = 3;

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
            max_retries: env_var_nonzero_usize("SNED_MAX_RETRIES", 3),
            base_delay_ms: env_var_nonzero("SNED_RETRY_BASE_DELAY_MS", 1_000),
            max_delay_ms: env_var_nonzero("SNED_RETRY_MAX_DELAY_MS", 10_000),
        }
    }
}

fn env_var_nonzero(name: &str, fallback: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(fallback)
}

fn env_var_nonzero_usize(name: &str, fallback: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(fallback)
}

/// Create a provider stream with retry semantics.
///
/// Retries are intentionally handled here instead of inside the provider
/// implementations so the agent loop can keep `TaskState` in sync with whether
/// a retry actually occurred.
pub async fn create_message_with_retry(
    provider: Arc<Providers>,
    request: ProviderRequest,
    task_state: Arc<Mutex<TaskState>>,
    retry_config: RetryConfig,
    json_output: bool,
    output_writer: Option<crate::cli::output::OutputWriterArc>,
    cancelled: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> Result<ApiStream, ProviderError> {
    // Validate context window before sending request
    if let Err(msg) = context_window::validate_context_window(&request, provider.as_ref()) {
        return Err(ProviderError::InvalidRequest(msg));
    }

    let mut retry_attempt = 0usize;

    loop {
        match provider.create_message(request.clone()).await {
            Ok(stream) => {
                let mut state = task_state.lock().await;
                state.consecutive_provider_failures = 0;
                drop(state);
                return Ok(stream);
            }
            Err(error) => {
                let Some(delay) = retry_delay_for_error(&error, retry_attempt, retry_config) else {
                    return Err(error);
                };

                if retry_attempt >= retry_config.max_retries {
                    let mut state = task_state.lock().await;
                    state.consecutive_provider_failures =
                        state.consecutive_provider_failures.saturating_add(1);
                    let consecutive_failures = state.consecutive_provider_failures;
                    let max_retries = retry_config.max_retries;
                    drop(state);
                    let msg = if consecutive_failures >= DEFAULT_MAX_CONSECUTIVE_PROVIDER_FAILURES {
                        format!(
                            "⚠ Provider failed after {max_retries} retries. {consecutive_failures} consecutive provider requests have failed. Use /model <provider/model> to switch or retry after the provider recovers."
                        )
                    } else {
                        format!(
                            "⚠ Provider failed after {max_retries} retries. Request {consecutive_failures}/{DEFAULT_MAX_CONSECUTIVE_PROVIDER_FAILURES} failed consecutively. Retry or use /model <provider/model> to switch."
                        )
                    };
                    if let Some(writer) = output_writer {
                        writer.emit(OutputEvent::plain(msg));
                    }
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
                if !json_output {
                    log_retry_status(
                        retry_attempt,
                        retry_config.max_retries,
                        delay,
                        &error,
                        output_writer.as_ref(),
                    );
                }

                // Sleep with cancellation check — poll in small intervals to remain responsive to Ctrl+C
                if let Some(cancelled) = &cancelled {
                    let remaining = delay;
                    let poll_interval = Duration::from_millis(100);
                    let mut elapsed = Duration::ZERO;
                    while elapsed < remaining {
                        if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                            return Err(ProviderError::NetworkError(
                                "cancelled by user during retry delay".to_string(),
                            ));
                        }
                        tokio::time::sleep(poll_interval.min(remaining.checked_sub(elapsed).unwrap())).await;
                        elapsed += poll_interval;
                    }
                } else {
                    sleep(delay).await;
                }

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
    use crate::cli::output::{OutputEvent, OutputWriter};
    use crate::providers::{
        ApiStreamChunk, Providers, RetryTestProvider,
    };
    use futures::StreamExt;
    use std::sync::{
        Arc,
        atomic::AtomicUsize,
    };
    use std::sync::{LazyLock, Mutex as StdMutex};

    static ENV_LOCK: LazyLock<StdMutex<()>> = LazyLock::new(|| StdMutex::new(()));

    struct CaptureWriter {
        events: StdMutex<Vec<OutputEvent>>,
    }

    impl CaptureWriter {
        fn new() -> Self {
            Self {
                events: StdMutex::new(Vec::new()),
            }
        }
    }

    impl OutputWriter for CaptureWriter {
        fn emit(&self, event: OutputEvent) {
            self.events.lock().unwrap().push(event);
        }

        fn flush(&self) {}
    }

    struct EnvVarGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(ref original) = self.original {
                    std::env::set_var(self.key, original);
                } else {
                    std::env::remove_var(self.key);
                }
            }
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
    fn retry_status_uses_configured_max_retries() {
        let writer = Arc::new(CaptureWriter::new());
        let writer_dyn: Arc<dyn OutputWriter> = writer.clone();
        let error = ProviderError::RateLimitError {
            message: "rate limited".to_string(),
            retry_delay_ms: None,
        };

        log_retry_status(0, 3, Duration::from_secs(2), &error, Some(&writer_dyn));

        let events = writer.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        let rendered = match &events[0] {
            OutputEvent::Line(line) => format!("{}", line),
            other => panic!("expected line event, got {other:?}"),
        };
        assert!(rendered.contains("attempt 1/3"));
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
        let provider = Arc::new(Providers::RetryTest(RetryTestProvider {
            attempts: Arc::new(AtomicUsize::new(0)),
            fail_until: 2,
        }));
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
            None,
            None,
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

    #[tokio::test]
    async fn consecutive_failures_reset_on_success() {
        let provider = Arc::new(Providers::RetryTest(RetryTestProvider {
            attempts: Arc::new(AtomicUsize::new(0)),
            fail_until: 1,
        }));
        let state = Arc::new(Mutex::new(TaskState::default()));
        let request = ProviderRequest {
            system_prompt: "system".to_string(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        // First call succeeds, should set consecutive failures to 0
        let stream = create_message_with_retry(
            provider.clone(),
            request.clone(),
            state.clone(),
            RetryConfig::default(),
            false,
            None,
            None,
        )
        .await
        .unwrap();
        drop(stream);
        assert_eq!(state.lock().await.consecutive_provider_failures, 0);

        // Second call also succeeds
        let stream = create_message_with_retry(
            provider.clone(),
            request.clone(),
            state.clone(),
            RetryConfig::default(),
            false,
            None,
            None,
        )
        .await
        .unwrap();
        drop(stream);
        assert_eq!(state.lock().await.consecutive_provider_failures, 0);
    }

    #[tokio::test]
    async fn consecutive_failures_increment_on_retry_exhaustion() {
        let provider = Arc::new(Providers::RetryTest(RetryTestProvider {
            attempts: Arc::new(AtomicUsize::new(0)),
            fail_until: usize::MAX,
        }));
        let state = Arc::new(Mutex::new(TaskState::default()));
        let request = ProviderRequest {
            system_prompt: "system".to_string(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };
        let retry_config = RetryConfig {
            max_retries: 0,
            base_delay_ms: 1,
            max_delay_ms: 1,
        };

        let result = create_message_with_retry(
            provider,
            request,
            state.clone(),
            retry_config,
            false,
            None,
            None,
        )
        .await;

        let err = match result {
            Ok(_) => panic!("retry exhaustion should return the provider error"),
            Err(err) => err,
        };

        assert!(matches!(err, ProviderError::RateLimitError { .. }));
        assert_eq!(state.lock().await.consecutive_provider_failures, 1);
    }

    #[test]
    fn retry_config_reads_sned_max_retries_from_env() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvVarGuard::set("SNED_MAX_RETRIES", "10");

        let config = RetryConfig::default();

        assert_eq!(config.max_retries, 10);
    }

    #[test]
    fn env_var_sned_retry_base_delay_overrides_default() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvVarGuard::set("SNED_RETRY_BASE_DELAY_MS", "5000");

        let config = RetryConfig::default();

        assert_eq!(config.base_delay_ms, 5_000);
    }

    #[test]
    fn retry_config_zero_max_retries_uses_fallback() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvVarGuard::set("SNED_MAX_RETRIES", "0");

        let config = RetryConfig::default();

        assert_eq!(config.max_retries, 3);
    }

    #[test]
    fn retry_config_overflow_max_retries_uses_fallback() {
        let _lock = ENV_LOCK.lock().unwrap();
        let overflow = (usize::MAX as u128) + 1;
        let _guard = EnvVarGuard::set("SNED_MAX_RETRIES", &overflow.to_string());

        let config = RetryConfig::default();

        assert_eq!(config.max_retries, 3);
    }
}
