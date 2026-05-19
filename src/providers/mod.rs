//! Provider trait and shared types for sned CLI.
//!
//! Defines the core abstractions that all LLM providers must implement,
//! along with message/content types compatible with the TypeScript source schema.

pub mod anthropic;
pub mod deepseek;
pub mod env_auth;
pub mod gemini;
pub mod gemini_format;
pub mod groq;
pub mod minimax;
pub mod mock;
pub mod openai;
pub mod openrouter;
pub mod xai;

use reqwest::StatusCode;

/// Maximum size for tool call arguments (128KB).
/// Modern LLMs send 10-100KB tool calls, so this limit prevents memory exhaustion
/// while allowing legitimate large tool arguments. Enforced during streaming assembly.
pub const MAX_TOOL_ARGUMENT_SIZE: usize = 131072;
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use std::pin::Pin;

// ============================================================================
// Content Block Types (ported from dirac/src/shared/messages/content.ts)
// ============================================================================

/// A reasoning detail parameter, used by OpenRouter and Sned providers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReasoningDetailParam {
    #[serde(rename = "type")]
    pub detail_type: String,
    pub text: String,
    pub signature: String,
    pub format: String,
    pub index: i32,
}

/// Shared fields across all content blocks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SharedContentFields {
    /// The call ID associated with this content block.
    pub call_id: Option<String>,
    /// Thought signature (used by Gemini).
    pub signature: Option<String>,
}

/// A text content block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TextContentBlock {
    pub text: String,
    #[serde(flatten)]
    pub shared: SharedContentFields,
    /// Reasoning details (only for providers that support them).
    pub reasoning_details: Option<Vec<ReasoningDetailParam>>,
}

/// Source of an image.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ImageSource {
    #[serde(rename = "base64")]
    Base64 {
        #[serde(rename = "media_type")]
        media_type: String,
        data: String,
    },
    #[serde(rename = "url")]
    Url { url: String },
}

/// Source of a document.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum DocumentSource {
    #[serde(rename = "base64")]
    Base64 {
        #[serde(rename = "media_type")]
        media_type: String,
        data: String,
    },
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "url")]
    Url { url: String },
}

/// Content of a tool result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ToolResultContentBlock>),
}

/// Individual block inside a tool result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ToolResultContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { source: ImageSource },
}

/// An image content block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImageContentBlock {
    pub source: ImageSource,
    #[serde(flatten)]
    pub shared: SharedContentFields,
}

/// A tool use block (assistant-side).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolUseBlock {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
    #[serde(flatten)]
    pub shared: SharedContentFields,
    pub reasoning_details: Option<Vec<ReasoningDetailParam>>,
}

/// A tool result block (user-side).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResultBlock {
    #[serde(rename = "tool_use_id")]
    pub tool_use_id: String,
    pub content: ToolResultContent,
    #[serde(flatten)]
    pub shared: SharedContentFields,
}

/// A thinking block (assistant-side).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThinkingBlock {
    pub thinking: String,
    pub signature: String,
    #[serde(flatten)]
    pub shared: SharedContentFields,
    pub summary: Option<Vec<ReasoningDetailParam>>,
}

/// A redacted thinking block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RedactedThinkingBlock {
    pub data: String,
    #[serde(flatten)]
    pub shared: SharedContentFields,
}

/// A document content block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocumentContentBlock {
    pub source: DocumentSource,
    #[serde(flatten)]
    pub shared: SharedContentFields,
}

/// Union of all user content blocks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum UserContentBlock {
    #[serde(rename = "text")]
    Text(TextContentBlock),
    #[serde(rename = "image")]
    Image(ImageContentBlock),
    #[serde(rename = "document")]
    Document(DocumentContentBlock),
    #[serde(rename = "tool_result")]
    ToolResult(ToolResultBlock),
}

/// Union of all assistant content blocks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum AssistantContentBlock {
    #[serde(rename = "text")]
    Text(TextContentBlock),
    #[serde(rename = "image")]
    Image(ImageContentBlock),
    #[serde(rename = "document")]
    Document(DocumentContentBlock),
    #[serde(rename = "tool_use")]
    ToolUse(ToolUseBlock),
    #[serde(rename = "thinking")]
    Thinking(ThinkingBlock),
    #[serde(rename = "redacted_thinking")]
    RedactedThinking(RedactedThinkingBlock),
}

// ============================================================================
// Message Types
// ============================================================================

/// Role of a message participant.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
}

/// Metrics attached to a message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MessageMetrics {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub cache_write_tokens: Option<u32>,
    pub cache_read_tokens: Option<u32>,
    pub cost_usd: Option<f64>,
}

/// Model info attached to a message (internal use only, stripped before sending to providers).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MessageModelInfo {
    pub provider: String,
    pub model_id: String,
}

/// A storage message, compatible with the TypeScript SnedStorageMessage schema.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StorageMessage {
    pub id: Option<String>,
    pub role: MessageRole,
    pub content: MessageContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_info: Option<MessageModelInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<MessageMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<u64>,
}

/// Content of a storage message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    UserBlocks(Vec<UserContentBlock>),
    AssistantBlocks(Vec<AssistantContentBlock>),
}

// ============================================================================
// Stream Chunk Types (ported from transform/stream.ts)
// ============================================================================

/// A chunk in the API stream.
#[derive(Debug, Clone, PartialEq)]
pub enum ApiStreamChunk {
    Text(ApiStreamTextChunk),
    Reasoning(ApiStreamReasoningChunk),
    ToolCalls(ApiStreamToolCallsChunk),
    Usage(ApiStreamUsageChunk),
    Error(String),
}

/// Text chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiStreamTextChunk {
    pub text: String,
    pub id: Option<String>,
    pub signature: Option<String>,
}

/// Reasoning/thinking chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiStreamReasoningChunk {
    pub reasoning: String,
    pub details: Option<serde_json::Value>,
    pub signature: Option<String>,
    pub redacted_data: Option<String>,
    pub id: Option<String>,
}

/// Tool call chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiStreamToolCallsChunk {
    pub tool_call: ApiStreamToolCall,
    pub id: Option<String>,
    pub signature: Option<String>,
}

/// Individual tool call in a chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiStreamToolCall {
    pub call_id: Option<String>,
    pub function: ApiStreamToolCallFunction,
    pub signature: Option<String>,
}

/// Function details of a tool call.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiStreamToolCallFunction {
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments: Option<String>,
}

/// Usage chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiStreamUsageChunk {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_write_tokens: Option<u32>,
    pub cache_read_tokens: Option<u32>,
    pub reasoning_tokens: Option<u32>,
    pub thoughts_token_count: Option<u32>,
    pub total_cost: Option<f64>,
    pub stop_reason: Option<String>,
    pub id: Option<String>,
}

// ============================================================================
// Model Info Types (ported from dirac/src/shared/api.ts)
// ============================================================================

/// Price tier for models with tiered pricing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PriceTier {
    pub token_limit: u64,
    pub price: f64,
}

/// Thinking configuration for a model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThinkingConfig {
    pub max_budget: Option<u32>,
    pub output_price: Option<f64>,
    pub output_price_tiers: Option<Vec<PriceTier>>,
    pub gemini_thinking_level: Option<String>,
    pub supports_thinking_level: Option<bool>,
}

/// Tiered pricing configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelTier {
    pub context_window: u64,
    pub input_price: Option<f64>,
    pub output_price: Option<f64>,
    pub cache_writes_price: Option<f64>,
    pub cache_reads_price: Option<f64>,
}

/// Model capability information.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ModelInfo {
    pub name: Option<String>,
    pub max_tokens: Option<u32>,
    pub context_window: Option<u64>,
    pub supports_images: Option<bool>,
    pub supports_prompt_cache: bool,
    pub supports_reasoning: Option<bool>,
    pub input_price: Option<f64>,
    pub output_price: Option<f64>,
    pub image_output_price: Option<f64>,
    pub thinking_config: Option<ThinkingConfig>,
    pub supports_global_endpoint: Option<bool>,
    pub cache_writes_price: Option<f64>,
    pub cache_reads_price: Option<f64>,
    pub description: Option<String>,
    pub tiers: Option<Vec<ModelTier>>,
    pub temperature: Option<f64>,
    pub supports_tools: Option<bool>,
    pub api_format: Option<String>,
}

/// OpenAI-compatible model info extension.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenAiCompatibleModelInfo {
    #[serde(flatten)]
    pub base: ModelInfo,
    pub temperature: Option<f64>,
    pub is_r1_format_required: Option<bool>,
    pub system_role: Option<String>,
    pub supports_reasoning_effort: Option<bool>,
    pub supports_streaming: Option<bool>,
}

// ============================================================================
// Provider Trait (ported from dirac/src/core/api/index.ts ApiHandler)
// ============================================================================

/// Tool choice options for provider requests.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// Model decides whether to call tools (default)
    Auto,
    /// Model MUST call at least one tool
    Required,
    /// Model cannot call tools
    None,
    /// Force a specific tool by name
    Named(String),
}

/// Configuration for a provider request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderRequest {
    pub system_prompt: String,
    pub messages: Vec<StorageMessage>,
    pub tools: Option<Vec<ToolDefinition>>,
    pub tool_choice: Option<ToolChoice>,
    pub use_response_api: Option<bool>,
    pub max_tokens: Option<u32>,
}

/// HTTP error returned by a provider after the remote API responded with a non-success status.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{provider} POST {url} failed: {status} - {body}")]
pub struct ProviderHttpError {
    pub provider: &'static str,
    pub url: String,
    pub status: StatusCode,
    pub body: String,
    pub headers: HeaderMap,
    pub retry_delay_ms: Option<u64>,
}

impl ProviderHttpError {
    pub fn new(
        provider: &'static str,
        url: String,
        status: StatusCode,
        body: String,
        headers: HeaderMap,
    ) -> Self {
        let retry_delay_ms = if status == StatusCode::TOO_MANY_REQUESTS {
            Self::extract_retry_delay(&headers, &body)
        } else {
            None
        };

        let body_display = if body.len() > 1024 {
            let end = body.floor_char_boundary(1024);
            format!(
                "{}... [truncated, total {} bytes]",
                &body[..end],
                body.len()
            )
        } else {
            body
        };

        Self {
            provider,
            url,
            status,
            body: body_display,
            headers,
            retry_delay_ms,
        }
    }

    fn extract_retry_delay(headers: &HeaderMap, body: &str) -> Option<u64> {
        if let Some(retry_delay) = Self::parse_retry_delay_from_body(body) {
            return Some(retry_delay);
        }

        if let Some(retry_delay) = Self::parse_retry_delay_from_headers(headers) {
            return Some(retry_delay);
        }

        None
    }

    fn parse_retry_delay_from_body(body: &str) -> Option<u64> {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(body)
            && let Some(details) = json.get("error")?.get("details")?.as_array()
        {
            for detail in details {
                if detail.get("@type")?.as_str()? == "type.googleapis.com/google.rpc.RetryInfo"
                    && let Some(retry_delay) = detail.get("retryDelay")?.as_str()
                {
                    let delay_str = retry_delay.trim();
                    if delay_str.ends_with('h') {
                        let hrs = delay_str.trim_end_matches('h').parse::<f64>().ok()?;
                        return Some((hrs * 3600.0 * 1000.0) as u64);
                    }
                    if delay_str.ends_with('m') {
                        let mins = delay_str.trim_end_matches('m').parse::<f64>().ok()?;
                        return Some((mins * 60.0 * 1000.0) as u64);
                    }
                    if delay_str.ends_with('s') {
                        let secs = delay_str.trim_end_matches('s').parse::<f64>().ok()?;
                        return Some((secs * 1000.0) as u64);
                    }
                }
            }
        }
        None
    }

    fn parse_retry_delay_from_headers(headers: &HeaderMap) -> Option<u64> {
        if let Some(retry_after) = headers.get("retry-after").and_then(|v| v.to_str().ok())
            && let Ok(secs) = retry_after.parse::<u64>()
        {
            return Some(secs * 1000);
        }

        if let Some(reset) = headers
            .get("x-ratelimit-reset")
            .and_then(|v| v.to_str().ok())
            .or_else(|| headers.get("ratelimit-reset").and_then(|v| v.to_str().ok()))
            && let Ok(epoch_secs) = reset.parse::<u64>()
        {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?;
            let delay_secs = epoch_secs.saturating_sub(now.as_secs());
            return Some(delay_secs * 1000);
        }

        None
    }
}

/// A tool definition for provider-native tool calling.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionDefinition,
}

/// Function definition within a tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Usage information from a provider.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_write_tokens: Option<u32>,
    pub cache_read_tokens: Option<u32>,
    pub reasoning_tokens: Option<u32>,
    pub total_cost: Option<f64>,
}

/// Information about the model being used.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderModel {
    pub id: String,
    pub info: ModelInfo,
}

/// The API stream type — an async stream of chunks.
pub type ApiStream = Pin<Box<dyn tokio_stream::Stream<Item = ApiStreamChunk> + Send>>;

/// Buffers SSE bytes until complete newline-delimited lines are available.
#[derive(Debug)]
pub struct SseLineBuffer {
    pending: Vec<u8>,
    max_line_length: usize,
    limit_exceeded: bool,
    /// Set by `take_error()` when the overflow error was consumed, so the next
    /// `push_chunk` call can skip the trailing newline of the overflowed line.
    skip_next_newline: bool,
}

impl SseLineBuffer {
    const DEFAULT_MAX_LINE_LENGTH: usize = 1024 * 1024; // 1MB

    #[cfg(test)]
    pub(crate) fn with_max_line_length(max: usize) -> Self {
        Self {
            pending: Vec::new(),
            max_line_length: max,
            limit_exceeded: false,
            skip_next_newline: false,
        }
    }

    /// Push bytes into the buffer and return complete lines.
    /// Made public for benchmarking.
    pub fn push_chunk(&mut self, chunk: &[u8]) -> Vec<String> {
        if self.skip_next_newline {
            // Skip leading newline from overflow recovery, then process rest
            if let Some(newline_idx) = chunk.iter().position(|&b| b == b'\n') {
                self.skip_next_newline = false;
                if newline_idx + 1 < chunk.len() {
                    return self.push_chunk(&chunk[newline_idx + 1..]);
                }
                return Vec::new();
            }
            // Still waiting for the terminating newline of the oversized line.
            // Discard this chunk — it's part of the overflow line, not new data.
            return Vec::new();
        }

        if self.limit_exceeded {
            // Find next newline to recover from overflow
            if let Some(newline_idx) = chunk.iter().position(|&b| b == b'\n') {
                self.limit_exceeded = false;
                if newline_idx + 1 < chunk.len() {
                    return self.push_chunk(&chunk[newline_idx + 1..]);
                }
                return Vec::new();
            }
            return Vec::new();
        }

        self.pending.extend_from_slice(chunk);

        if self.pending.len() >= self.max_line_length {
            if let Some(last_newline) = self.pending.iter().rposition(|&b| b == b'\n') {
                let complete_lines = self.pending.drain(..=last_newline).collect::<Vec<u8>>();
                let mut lines = Vec::new();
                for line_bytes in complete_lines.split(|&b| b == b'\n') {
                    let mut line = line_bytes.to_vec();
                    if matches!(line.last(), Some(b'\r')) {
                        line.pop();
                    }
                    if !line.is_empty() {
                        lines.push(String::from_utf8_lossy(&line).into_owned());
                    }
                }
                if self.pending.len() >= self.max_line_length {
                    self.pending.clear();
                    self.limit_exceeded = true;
                }
                return lines;
            } else {
                self.pending.clear();
                self.limit_exceeded = true;
                return Vec::new();
            }
        }

        let mut lines = Vec::new();
        while let Some(newline_idx) = self.pending.iter().position(|&b| b == b'\n') {
            let mut line_bytes = self.pending.drain(..=newline_idx).collect::<Vec<u8>>();
            if matches!(line_bytes.last(), Some(b'\n')) {
                line_bytes.pop();
            }
            if matches!(line_bytes.last(), Some(b'\r')) {
                line_bytes.pop();
            }
            lines.push(String::from_utf8_lossy(&line_bytes).into_owned());
        }
        lines
    }

    pub(crate) fn finish(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned())
        }
    }

    pub(crate) fn take_error(&mut self) -> Option<String> {
        if self.limit_exceeded {
            self.limit_exceeded = false;
            self.skip_next_newline = true;
            Some(format!(
                "SSE line exceeded maximum length of {} bytes",
                self.max_line_length
            ))
        } else {
            None
        }
    }
}

impl Default for SseLineBuffer {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
            max_line_length: Self::DEFAULT_MAX_LINE_LENGTH,
            limit_exceeded: false,
            skip_next_newline: false,
        }
    }
}

/// Validate and optionally repair tool call arguments JSON.
/// Returns valid JSON string. Falls back to "{}" if repair fails.
pub fn validate_tool_call_args(args: &str, provider_name: &str, context: &str) -> String {
    if args.is_empty() {
        return "{}".to_string();
    }
    match serde_json::from_str::<serde_json::Value>(args) {
        Ok(_) => args.to_string(),
        Err(e) => {
            tracing::warn!(
                "{} tool call arguments JSON invalid {} ({}), attempting repair. args_preview={}",
                provider_name,
                context,
                e,
                args.chars().take(100).collect::<String>()
            );
            let mut repaired = args.to_string();
            let mut brace_count: i32 = 0;
            let mut bracket_count: i32 = 0;
            let mut in_string = false;
            let mut escape_next = false;
            for c in args.chars() {
                if escape_next {
                    escape_next = false;
                    continue;
                }
                if c == '\\' {
                    escape_next = true;
                    continue;
                }
                if c == '"' {
                    in_string = !in_string;
                    continue;
                }
                if !in_string {
                    match c {
                        '{' => brace_count += 1,
                        '}' => brace_count -= 1,
                        '[' => bracket_count += 1,
                        ']' => bracket_count -= 1,
                        _ => {}
                    }
                }
            }
            for _ in 0..bracket_count.max(0) {
                repaired.push(']');
            }
            for _ in 0..brace_count.max(0) {
                repaired.push('}');
            }
            if serde_json::from_str::<serde_json::Value>(&repaired).is_err() {
                tracing::warn!(
                    "{} tool call arguments JSON repair failed {}, using empty object",
                    provider_name,
                    context
                );
                "{}".to_string()
            } else {
                repaired
            }
        }
    }
}

/// Error types for LLM provider operations.
#[derive(thiserror::Error, Debug)]
pub enum ProviderError {
    #[error("network error: {0}")]
    NetworkError(String),
    #[error("authentication error: {0}")]
    AuthenticationError(String),
    #[error("rate limit error: {message}")]
    RateLimitError {
        message: String,
        retry_delay_ms: Option<u64>,
    },
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("API error: {0}")]
    ApiError(String),
    #[error("unexpected error: {0}")]
    UnexpectedError(String),
}

impl From<ProviderHttpError> for ProviderError {
    fn from(e: ProviderHttpError) -> Self {
        if e.status.is_client_error() {
            if e.status == StatusCode::UNAUTHORIZED || e.status == StatusCode::FORBIDDEN {
                ProviderError::AuthenticationError(e.to_string())
            } else if e.status == StatusCode::TOO_MANY_REQUESTS {
                ProviderError::RateLimitError {
                    message: e.to_string(),
                    retry_delay_ms: e.retry_delay_ms,
                }
            } else {
                ProviderError::InvalidRequest(e.to_string())
            }
        } else {
            ProviderError::ApiError(e.to_string())
        }
    }
}

impl From<reqwest::Error> for ProviderError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() || e.is_connect() || e.is_body() {
            ProviderError::NetworkError(e.to_string())
        } else {
            ProviderError::UnexpectedError(e.to_string())
        }
    }
}

impl From<anyhow::Error> for ProviderError {
    fn from(e: anyhow::Error) -> Self {
        if let Some(http_err) = e.downcast_ref::<ProviderHttpError>() {
            if http_err.status.is_client_error() {
                if http_err.status == StatusCode::UNAUTHORIZED
                    || http_err.status == StatusCode::FORBIDDEN
                {
                    ProviderError::AuthenticationError(http_err.to_string())
                } else if http_err.status == StatusCode::TOO_MANY_REQUESTS {
                    ProviderError::RateLimitError {
                        message: http_err.to_string(),
                        retry_delay_ms: http_err.retry_delay_ms,
                    }
                } else {
                    ProviderError::InvalidRequest(http_err.to_string())
                }
            } else {
                ProviderError::ApiError(http_err.to_string())
            }
        } else {
            ProviderError::UnexpectedError(e.to_string())
        }
    }
}

/// Core trait that all LLM providers must implement.
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    /// Create a streaming chat completion.
    async fn create_message(&self, request: ProviderRequest) -> Result<ApiStream, ProviderError>;

    /// Get the current model information.
    fn get_model(&self) -> ProviderModel;

    /// Get the provider name (e.g., "anthropic", "openai", "minimax").
    fn name(&self) -> &str;
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_json_roundtrip() {
        let msg = StorageMessage {
            id: Some("msg_123".to_string()),
            role: MessageRole::User,
            content: MessageContent::Text("Hello world".to_string()),
            model_info: None,
            metrics: None,
            ts: Some(1234567890),
        };

        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: StorageMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_content_block_json_roundtrip() {
        let block = UserContentBlock::Text(TextContentBlock {
            text: "Hello".to_string(),
            shared: SharedContentFields {
                call_id: None,
                signature: Some("sig_123".to_string()),
            },
            reasoning_details: Some(vec![ReasoningDetailParam {
                detail_type: "reasoning.text".to_string(),
                text: "thinking...".to_string(),
                signature: "sig_123".to_string(),
                format: "anthropic-claude-v1".to_string(),
                index: 0,
            }]),
        });

        let json = serde_json::to_string(&block).unwrap();
        let deserialized: UserContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, deserialized);
    }

    #[test]
    fn test_tool_use_block() {
        let block = AssistantContentBlock::ToolUse(ToolUseBlock {
            id: "tool_123".to_string(),
            name: "read_file".to_string(),
            input: serde_json::json!({"path": "/tmp/test.txt"}),
            shared: SharedContentFields {
                call_id: Some("call_123".to_string()),
                signature: None,
            },
            reasoning_details: None,
        });

        let json = serde_json::to_string(&block).unwrap();
        let deserialized: AssistantContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, deserialized);
    }

    #[test]
    fn test_sse_line_buffer_reassembles_split_line() {
        let mut buffer = SseLineBuffer::default();

        let first = buffer
            .push_chunk(br#"data: {"id":"evt","choices":[{"index":0,"delta":{"content":"hel"#);
        assert!(first.is_empty());

        let second = buffer.push_chunk(b"lo\"},\"finish_reason\":null}]}\n");
        assert_eq!(second.len(), 1);
        assert_eq!(
            second[0],
            r#"data: {"id":"evt","choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}"#
        );
    }

    #[test]
    fn test_sse_line_buffer_finishes_trailing_line() {
        let mut buffer = SseLineBuffer::default();
        let _ = buffer.push_chunk(br#"data: {"id":"evt"}"#);
        assert_eq!(buffer.finish().as_deref(), Some(r#"data: {"id":"evt"}"#));
    }

    #[test]
    fn test_sse_line_buffer_limits_oversized_line() {
        let mut buffer = SseLineBuffer::with_max_line_length(20);

        // Push 25 bytes without a newline — exceeds the 20-byte limit
        let lines = buffer.push_chunk(b"0123456789012345678901234");
        assert!(lines.is_empty());
        assert_eq!(
            buffer.take_error().as_deref(),
            Some("SSE line exceeded maximum length of 20 bytes")
        );
    }

    #[test]
    fn test_sse_line_buffer_limits_recover_after_oversized() {
        let mut buffer = SseLineBuffer::with_max_line_length(10);

        // Exceed limit
        buffer.push_chunk(b"01234567890");
        assert!(buffer.take_error().is_some());

        // Now send valid data after a newline in a new chunk
        let lines = buffer.push_chunk(b"\nok\n");
        assert_eq!(lines, vec!["ok"]);
    }

    #[test]
    fn test_sse_line_buffer_limits_salvage_complete_lines() {
        let mut buffer = SseLineBuffer::with_max_line_length(20);

        // First line is complete (6 bytes + newline), second line exceeds
        let lines = buffer.push_chunk(b"short\n01234567890123456789");
        assert_eq!(lines, vec!["short"]);
        assert!(buffer.take_error().is_some());
    }

    #[test]
    fn test_model_info_serialization() {
        let info = ModelInfo {
            name: Some("claude-sonnet-4-6".to_string()),
            max_tokens: Some(64000),
            context_window: Some(200000),
            supports_images: Some(true),
            supports_prompt_cache: true,
            supports_reasoning: Some(true),
            input_price: Some(3.0),
            output_price: Some(15.0),
            image_output_price: None,
            thinking_config: Some(ThinkingConfig {
                max_budget: Some(1024),
                output_price: None,
                output_price_tiers: None,
                gemini_thinking_level: Some("high".to_string()),
                supports_thinking_level: Some(true),
            }),
            supports_global_endpoint: None,
            cache_writes_price: Some(3.75),
            cache_reads_price: Some(0.3),
            description: None,
            tiers: Some(vec![ModelTier {
                context_window: 200000,
                input_price: Some(3.0),
                output_price: Some(15.0),
                cache_writes_price: Some(3.75),
                cache_reads_price: Some(0.3),
            }]),
            temperature: None,
            supports_tools: Some(true),
            api_format: None,
        };

        let json = serde_json::to_string(&info).unwrap();
        let deserialized: ModelInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, deserialized);
    }

    #[test]
    fn test_provider_http_error_extract_retry_delay_from_gemini_body() {
        let body = r#"{"error": {"details": [{"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "5s"}]}}"#;
        let headers = HeaderMap::new();
        let error = ProviderHttpError::new(
            "gemini",
            "https://api.example.com".to_string(),
            StatusCode::TOO_MANY_REQUESTS,
            body.to_string(),
            headers,
        );
        assert_eq!(error.retry_delay_ms, Some(5000));
    }

    #[test]
    fn test_provider_http_error_extract_retry_delay_from_retry_after_header() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "10".parse().unwrap());
        let error = ProviderHttpError::new(
            "openai",
            "https://api.example.com".to_string(),
            StatusCode::TOO_MANY_REQUESTS,
            "{}".to_string(),
            headers,
        );
        assert_eq!(error.retry_delay_ms, Some(10000));
    }

    #[test]
    fn test_provider_http_error_body_takes_precedence_over_header() {
        let body = r#"{"error": {"details": [{"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "2s"}]}}"#;
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "10".parse().unwrap());
        let error = ProviderHttpError::new(
            "gemini",
            "https://api.example.com".to_string(),
            StatusCode::TOO_MANY_REQUESTS,
            body.to_string(),
            headers,
        );
        assert_eq!(error.retry_delay_ms, Some(2000));
    }

    #[test]
    fn test_provider_http_error_no_retry_delay_on_non_429() {
        let headers = HeaderMap::new();
        let error = ProviderHttpError::new(
            "openai",
            "https://api.example.com".to_string(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "{}".to_string(),
            headers,
        );
        assert_eq!(error.retry_delay_ms, None);
    }

    #[test]
    fn test_provider_error_rate_limit_preserves_retry_delay() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "5".parse().unwrap());
        let http_error = ProviderHttpError::new(
            "openai",
            "https://api.example.com".to_string(),
            StatusCode::TOO_MANY_REQUESTS,
            "{}".to_string(),
            headers,
        );
        let provider_error: ProviderError = http_error.into();
        match provider_error {
            ProviderError::RateLimitError { retry_delay_ms, .. } => {
                assert_eq!(retry_delay_ms, Some(5000));
            }
            _ => panic!("Expected RateLimitError"),
        }
    }

    #[test]
    fn test_validate_tool_call_args_ignores_braces_in_strings() {
        let args = r#"{"text": "hello {world}"}"#;
        let result = validate_tool_call_args(args, "test", "unit");
        assert_eq!(result, args);
    }

    #[test]
    fn test_validate_tool_call_args_repairs_missing_close_brace() {
        let args = r#"{"path": "/tmp/test""#;
        let result = validate_tool_call_args(args, "test", "unit");
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["path"], "/tmp/test");
    }

    #[test]
    fn test_validate_tool_call_args_handles_braces_inside_string_values() {
        let args = r#"{"pattern": "{name} placeholder", "count": 5"#;
        let result = validate_tool_call_args(args, "test", "unit");
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["pattern"], "{name} placeholder");
        assert_eq!(parsed["count"], 5);
    }

    #[test]
    fn test_validate_tool_call_args_fallback_on_unrepairable() {
        let args = r#"totally not json"#;
        let result = validate_tool_call_args(args, "test", "unit");
        assert_eq!(result, "{}");
    }

    #[test]
    fn test_validate_tool_call_args_empty_returns_empty_object() {
        assert_eq!(validate_tool_call_args("", "test", "unit"), "{}");
    }

    #[test]
    fn test_validate_tool_call_args_valid_json_passes_through() {
        let args = r#"{"a": 1, "b": [2, 3]}"#;
        assert_eq!(validate_tool_call_args(args, "test", "unit"), args);
    }
}
