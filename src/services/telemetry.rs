//! Telemetry service for sending usage and error events to the Sned analytics endpoint.
//!
//! Enforces the user-visible opt-out gate: when `telemetry_setting == "disabled"` or
//! `open_telemetry_enabled == false`, all telemetry sends are suppressed.
//!
//! Collects:
//! - Task completion/start events
//! - Tool use events (non-sensitive)
//! - Error/crash events
//! - Provider/agent loop telemetry (OpenTelemetry OTLP)

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use url::Url;

// ---------------------------------------------------------------------------
// Event types
// ---------------------------------------------------------------------------

/// The kind of telemetry event being recorded.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TelemetryEventKind {
    /// A new task was started.
    TaskStart,
    /// A task completed (success or failure).
    TaskComplete { success: bool },
    /// A tool was used.
    ToolUse { tool_name: String, success: bool },
    /// An error occurred.
    Error { error_type: String, message: String },
    /// A panic/crash occurred.
    Crash { panic_message: String, backtrace: String },
    /// A provider request was made.
    ProviderRequest { provider: String, success: bool, duration_ms: u64 },
    /// A context compaction occurred.
    ContextCompaction,
    /// A hook was triggered.
    HookTrigger { hook_name: String, success: bool },
}

/// A telemetry event envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryEvent {
    /// Unique event ID (ULID).
    pub event_id: String,
    /// Machine ID.
    pub machine_id: String,
    /// Event kind.
    pub kind: TelemetryEventKind,
    /// Timestamp (Unix epoch seconds).
    pub timestamp: u64,
    /// Task ID (when applicable).
    pub task_id: Option<String>,
    /// Provider/model info.
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Additional free-form metadata.
    pub metadata: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Telemetry service
// ---------------------------------------------------------------------------

/// Telemetry service that collects and batches events for sending.
pub struct TelemetryService {
    /// Whether telemetry is enabled (opt-out gate).
    enabled: bool,
    /// Whether OpenTelemetry is enabled.
    otel_enabled: bool,
    /// OTLP endpoint URL.
    otlp_endpoint: Option<String>,
    /// OTLP protocol.
    otlp_protocol: Option<String>,
    /// Machine ID.
    machine_id: String,
    /// Version string.
    version: String,
    /// Batching queue (in-memory).
    queue: Arc<Mutex<Vec<TelemetryEvent>>>,
    /// Send interval.
    send_interval: Duration,
    /// Max batch size before forcing send.
    max_batch_size: usize,
}

impl TelemetryService {
    /// Create a new telemetry service.
    ///
    /// If `enabled` is `false` (user disabled telemetry), all events are silently
    /// dropped and no HTTP requests are made.
    pub fn new(
        enabled: bool,
        otel_enabled: bool,
        machine_id: String,
        version: String,
        otlp_endpoint: Option<String>,
        otlp_protocol: Option<String>,
        send_interval: Duration,
        max_batch_size: usize,
    ) -> Self {
        Self {
            enabled,
            otel_enabled,
            machine_id,
            version,
            otlp_endpoint,
            otlp_protocol,
            send_interval,
            max_batch_size,
            queue: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Check if telemetry is enabled (opt-out gate).
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Record a telemetry event. If telemetry is disabled, the event is silently dropped.
    pub async fn record(&self, kind: TelemetryEventKind, task_id: Option<String>) {
        if !self.enabled {
            return;
        }

        let event_id = ulid::Ulid::new().to_string();
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let event = TelemetryEvent {
            event_id,
            machine_id: self.machine_id.clone(),
            kind,
            timestamp,
            task_id,
            provider: None,
            model: None,
            metadata: None,
        };

        let mut queue = self.queue.lock().await;
        queue.push(event);

        // If queue exceeds max_batch_size, trigger immediate send
        if queue.len() >= self.max_batch_size {
            let batch = queue.drain(..).collect::<Vec<_>>();
            let _ = self.send_batch(batch).await;
        }
    }

    /// Record an error event.
    pub async fn record_error(&self, kind: TelemetryEventKind, task_id: Option<String>) {
        self.record(kind, task_id).await;
    }

    /// Flush the queue immediately (for shutdown).
    pub async fn flush(&self) {
        if !self.enabled {
            return;
        }

        let mut queue = self.queue.lock().await;
        if queue.is_empty() {
            return;
        }

        let batch = queue.drain(..).collect::<Vec<_>>();
        drop(queue);

        let _ = self.send_batch(batch).await;
    }

    /// Send a batch of events to the OTLP endpoint (or mock endpoint for testing).
    async fn send_batch(&self, batch: Vec<TelemetryEvent>) -> Result<(), TelemetryError> {
        if !self.enabled || batch.is_empty() {
            return Err(TelemetryError::Disabled);
        }

        let endpoint = match &self.otlp_endpoint {
            Some(url) => url.clone(),
            None => return Ok(()), // No endpoint configured, silently drop
        };

        let payload = serde_json::to_value(&batch).map_err(TelemetryError::Serialization)?;

        // Use reqwest to send the batch
        let client = reqwest::Client::new();
        let response = client
            .post(&endpoint)
            .header("Content-Type", "application/x-ndjson")
            .body(payload.to_string())
            .timeout(Duration::from_secs(5))
            .send()
            .await;

        match response {
            Ok(resp) => {
                if resp.status().is_success() {
                    Ok(())
                } else {
                    tracing::warn!(
                        "Telemetry send failed with status {}: {}",
                        resp.status(),
                        resp.status().as_u16()
                    );
                    Err(TelemetryError::HttpError(resp.status().as_u16()))
                }
            }
            Err(e) => {
                tracing::warn!("Telemetry send failed: {}", e);
                Err(TelemetryError::NetworkError(e.to_string()))
            }
        }
    }

    /// Start a background task that periodically flushes the queue.
    /// Returns a handle that can be used to stop the task.
    pub fn spawn_flush_task(
        &self,
        state_handle: Arc<tokio::sync::Mutex<Option<Arc<tokio::sync::Mutex<crate::core::agent_types::TaskState>>>>>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if !self.enabled {
            return None;
        }

        let service = self.clone();
        let interval = self.send_interval;

        Some(tokio::spawn(async move {
            let mut timer = tokio::time::interval(interval);
            loop {
                timer.tick().await;
                service.flush().await;
            }
        }))
    }
}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Errors that can occur during telemetry collection/sending.
#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    #[error("telemetry is disabled")]
    Disabled,
    #[error("serialization failed: {0}")]
    Serialization(serde_json::Error),
    #[error("http error: {0}")]
    HttpError(u16),
    #[error("network error: {0}")]
    NetworkError(String),
}

// ---------------------------------------------------------------------------
// Crash reporting integration
// ---------------------------------------------------------------------------

/// Install a panic hook that collects crash information for telemetry.
///
/// The panic hook restores terminal state (as before) and optionally records
/// crash events for telemetry if enabled.
pub fn install_panic_hook(service: Option<&TelemetryService>) {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Restore terminal state first
        let _ = crate::terminal::input::disable_raw_mode();
        let _ = std::io::stderr().write_all(b"\x1b[?1049l");
        let _ = std::io::stderr().write_all(b"\x1b[?25h");
        let _ = std::io::stderr().write_all(b"\x1b[0m");
        let _ = std::io::stderr().flush();

        // Collect crash info for telemetry if service is enabled
        if let Some(service) = service {
            let message = if let Some(s) = info.payload().downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_string()
            };

            let backtrace = format!("{:?}", std::backtrace::Backtrace::force_capture());

            let _ = tokio::runtime::Handle::current().block_on(async {
                service
                    .record(
                        TelemetryEventKind::Crash {
                            panic_message: message,
                            backtrace,
                        },
                        None,
                    )
                    .await;
            });
        }

        // Call original hook
        original(info);
    }));
}

// ---------------------------------------------------------------------------
// Telemetry configuration from GlobalState
// ---------------------------------------------------------------------------

/// Check if telemetry should be enabled based on GlobalState.
pub fn is_telemetry_enabled(
    telemetry_setting: &str,
    open_telemetry_enabled: bool,
) -> bool {
    // If user explicitly disabled telemetry, suppress all sends
    if telemetry_setting == "disabled" {
        return false;
    }

    // If OpenTelemetry is explicitly disabled, suppress all sends
    if !open_telemetry_enabled {
        return false;
    }

    // If telemetry_setting is "unset" or "enabled", allow sends
    true
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disable_telemetry_when_setting_is_disabled() {
        assert!(!is_telemetry_enabled("disabled", true));
    }

    #[test]
    fn test_disable_telemetry_when_otel_disabled() {
        assert!(!is_telemetry_enabled("enabled", false));
    }

    #[test]
    fn test_enable_telemetry_when_unset_and_otel_enabled() {
        assert!(is_telemetry_enabled("unset", true));
    }

    #[test]
    fn test_enable_telemetry_when_enabled_and_otel_enabled() {
        assert!(is_telemetry_enabled("enabled", true));
    }

    #[test]
    fn test_create_telemetry_service_with_disabled() {
        let service = TelemetryService::new(
            false, // disabled
            true,
            "test-machine-id".to_string(),
            "0.1.0".to_string(),
            None, // no endpoint
            None,
            Duration::from_secs(60),
            100,
        );
        assert!(!service.is_enabled());
    }

    #[test]
    fn test_create_telemetry_service_with_enabled() {
        let service = TelemetryService::new(
            true, // enabled
            true,
            "test-machine-id".to_string(),
            "0.1.0".to_string(),
            Some("http://localhost:4318".to_string()),
            Some("http/json".to_string()),
            Duration::from_secs(60),
            100,
        );
        assert!(service.is_enabled());
    }
}
