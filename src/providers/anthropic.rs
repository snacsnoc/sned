//! Anthropic provider implementation for sned CLI.
//!
//! Ports behavior from `dirac/src/core/api/providers/anthropic.ts`.

use crate::providers::{
    ApiStream, ApiStreamChunk, ApiStreamReasoningChunk, ApiStreamTextChunk, ApiStreamToolCall,
    ApiStreamToolCallFunction, ApiStreamToolCallsChunk, ApiStreamUsageChunk, MessageRole,
    ModelInfo, Provider, ProviderError, ProviderHttpError, ProviderModel, ProviderRequest,
};
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc::error::TrySendError;

/// Configuration for the Anthropic provider.
#[derive(Clone)]
pub struct AnthropicConfig {
    pub api_key: String,
    pub base_url: Option<String>,
    pub model_id: String,
    pub model_info: Option<ModelInfo>,
    pub thinking_budget_tokens: Option<u32>,
}

impl std::fmt::Debug for AnthropicConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicConfig")
            .field(
                "api_key",
                &format!("***REDACTED ({} chars)***", self.api_key.len()),
            )
            .field("base_url", &self.base_url)
            .field("model_id", &self.model_id)
            .field("model_info", &self.model_info)
            .field("thinking_budget_tokens", &self.thinking_budget_tokens)
            .finish()
    }
}

/// Anthropic API provider.
pub struct AnthropicProvider {
    config: AnthropicConfig,
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(config: AnthropicConfig) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .connect_timeout(std::time::Duration::from_secs(10))
            .tcp_keepalive(Some(std::time::Duration::from_secs(60)))
            .pool_max_idle_per_host(10)
            .build()?;
        Ok(Self { config, client })
    }

    fn build_headers(&self) -> anyhow::Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        // Updated to 2025-04-14 to support prompt caching and extended thinking without beta headers
        headers.insert("anthropic-version", HeaderValue::from_static("2025-04-14"));

        if !self.config.api_key.is_empty() {
            headers.insert("x-api-key", HeaderValue::from_str(&self.config.api_key)?);
        }

        // Fast mode and 1M context window beta headers
        // Handle both :fast:1m and :1m:fast orderings
        let model_id = &self.config.model_id;
        let mut beta_values = Vec::new();
        if model_id.ends_with(":fast") || model_id.contains(":fast:") {
            beta_values.push("fast-mode-2026-02-01");
        }
        if model_id.ends_with(":1m") || model_id.contains(":1m:") {
            beta_values.push("context-1m-2025-08-07");
        }
        if !beta_values.is_empty() {
            headers.insert(
                "anthropic-beta",
                HeaderValue::from_str(&beta_values.join(","))?,
            );
        }

        Ok(headers)
    }

    fn base_url(&self) -> String {
        self.config
            .base_url
            .as_ref()
            .cloned()
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| "https://api.anthropic.com/v1".to_string())
    }

    fn sanitize_model_id(&self) -> std::borrow::Cow<'_, str> {
        let id = &self.config.model_id;
        let mut stripped = false;
        let mut result = id.as_str();

        // Strip both :fast and :1m suffixes in any order (e.g., :fast:1m or :1m:fast).
        // A loop is needed because the first pass may expose a suffix that was
        // originally in the middle (e.g., ":1m:fast" -> strip :fast leaves :1m at end).
        loop {
            let before = result;
            if result.ends_with(":fast") {
                result = &result[..result.len() - ":fast".len()];
                stripped = true;
            }
            if result.ends_with(":1m") {
                result = &result[..result.len() - ":1m".len()];
                stripped = true;
            }
            if result == before {
                break;
            }
        }

        if stripped {
            std::borrow::Cow::Owned(result.to_string())
        } else {
            std::borrow::Cow::Borrowed(result)
        }
    }

    fn build_request_body(&self, request: &ProviderRequest) -> anyhow::Result<serde_json::Value> {
        let model_id = self.sanitize_model_id();
        let model_info = self.get_model().info;
        let budget_tokens = self.config.thinking_budget_tokens.unwrap_or(0);
        let reasoning_on = model_info.supports_reasoning.unwrap_or(false) && budget_tokens != 0;
        let native_tools_on = request
            .tools
            .as_ref()
            .map(|t| !t.is_empty())
            .unwrap_or(false);
        let supports_cache = model_info.supports_prompt_cache;

        let max_tokens = request
            .max_tokens
            .filter(|m| *m > 0)
            .or(model_info.max_tokens)
            .unwrap_or(8192);

        // Convert messages to Anthropic format
        let total_messages = request.messages.len();
        let messages: Vec<serde_json::Value> = request
            .messages
            .iter()
            .enumerate()
            .map(|(msg_index, msg)| {
                let role = match msg.role {
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                };

                match &msg.content {
                    crate::providers::MessageContent::Text(text) => {
                        json!({
                            "role": role,
                            "content": [{"type": "text", "text": text}]
                        })
                    }
                    crate::providers::MessageContent::UserBlocks(blocks) => {
                        let content =
                            convert_user_blocks(blocks, supports_cache, msg_index, total_messages);
                        json!({"role": role, "content": content})
                    }
                    crate::providers::MessageContent::AssistantBlocks(blocks) => {
                        let content = convert_assistant_blocks(
                            blocks,
                            supports_cache,
                            msg_index,
                            total_messages,
                        );
                        json!({"role": role, "content": content})
                    }
                }
            })
            .collect();

        let mut system_content = vec![json!({
            "type": "text",
            "text": request.system_prompt,
        })];

        if supports_cache {
            system_content[0]["cache_control"] = json!({"type": "ephemeral"});
        }

        let mut body = json!({
            "model": model_id,
            "max_tokens": max_tokens,
            "system": system_content,
            "messages": messages,
            "stream": true,
        });

        // Temperature: undefined when reasoning is on
        if !reasoning_on {
            body["temperature"] = json!(0);
        }

        // Speed mode: check both :fast at end and :fast: in middle (e.g., :fast:1m or :1m:fast)
        if self.config.model_id.ends_with(":fast") || self.config.model_id.contains(":fast:") {
            body["speed"] = json!("fast");
        }

        // Thinking budget
        if reasoning_on {
            let thinking_type = if model_id.contains("opus-4-7") || model_id.contains("opus-4.7") {
                "adaptive"
            } else {
                "enabled"
            };
            body["thinking"] = json!({
                "type": thinking_type,
                "budget_tokens": budget_tokens,
            });
        }

        // Tools
        if native_tools_on && let Some(tools) = request.tools.as_ref() {
            let tools_json: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.function.name,
                        "description": t.function.description,
                        "input_schema": t.function.parameters,
                    })
                })
                .collect();
            body["tools"] = json!(tools_json);

            // Tool choice: respect request.tool_choice if provided
            // Anthropic API: "auto" (default), "any" (force tool use), "none" (no tools), "tool" (specific tool)
            // Tool choice is now supported with extended thinking (Anthropic lifted restriction)
            if let Some(tool_choice) = &request.tool_choice {
                body["tool_choice"] = match tool_choice {
                    crate::providers::ToolChoice::Auto => json!({"type": "auto"}),
                    crate::providers::ToolChoice::Required => json!({"type": "any"}),
                    crate::providers::ToolChoice::None => json!({"type": "none"}),
                    crate::providers::ToolChoice::Named(name) => {
                        json!({"type": "tool", "name": name})
                    }
                };
            } else {
                // Default: auto (model decides whether to use tools)
                // Changed from "any" to fix bug where model was forced to always call a tool
                body["tool_choice"] = json!({"type": "auto"});
            }
        }

        Ok(body)
    }
}

fn convert_user_blocks(
    blocks: &[crate::providers::UserContentBlock],
    supports_cache: bool,
    msg_index: usize,
    total_messages: usize,
) -> Vec<serde_json::Value> {
    let mut content = vec![];
    for (i, block) in blocks.iter().enumerate() {
        let mut item = match block {
            crate::providers::UserContentBlock::Text(t) => {
                json!({"type": "text", "text": t.text})
            }
            crate::providers::UserContentBlock::Image(img) => match &img.source {
                crate::providers::ImageSource::Base64 { media_type, data } => {
                    json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": media_type,
                            "data": data,
                        }
                    })
                }
                crate::providers::ImageSource::Url { url } => {
                    json!({
                        "type": "image",
                        "source": {
                            "type": "url",
                            "url": url,
                        }
                    })
                }
            },
            crate::providers::UserContentBlock::ToolResult(tr) => {
                let content = match &tr.content {
                    crate::providers::ToolResultContent::Text(text) => {
                        json!([{"type": "text", "text": text}])
                    }
                    crate::providers::ToolResultContent::Blocks(blocks) => {
                        let parts: Vec<_> = blocks
                            .iter()
                            .map(|b| match b {
                                crate::providers::ToolResultContentBlock::Text { text } => {
                                    json!({"type": "text", "text": text})
                                }
                                crate::providers::ToolResultContentBlock::Image { source } => {
                                    match source {
                                        crate::providers::ImageSource::Base64 {
                                            media_type,
                                            data,
                                        } => {
                                            json!({
                                                "type": "image",
                                                "source": {
                                                    "type": "base64",
                                                    "media_type": media_type,
                                                    "data": data,
                                                }
                                            })
                                        }
                                        crate::providers::ImageSource::Url { url } => {
                                            json!({
                                                "type": "image",
                                                "source": {
                                                    "type": "url",
                                                    "url": url,
                                                }
                                            })
                                        }
                                    }
                                }
                            })
                            .collect();
                        json!(parts)
                    }
                };
                json!({
                    "type": "tool_result",
                    "tool_use_id": tr.tool_use_id,
                    "content": content,
                })
            }
            crate::providers::UserContentBlock::Document(doc) => match &doc.source {
                crate::providers::DocumentSource::Text { text } => {
                    json!({"type": "text", "text": text})
                }
                crate::providers::DocumentSource::Base64 { media_type, data } => {
                    // Detect content type: image/* → "image", else → "document"
                    // Anthropic API requires "type": "document" for PDFs and other non-image files
                    let content_type = if media_type.starts_with("image/") {
                        "image"
                    } else {
                        "document"
                    };
                    json!({
                        "type": content_type,
                        "source": {
                            "type": "base64",
                            "media_type": media_type,
                            "data": data,
                        }
                    })
                }
                crate::providers::DocumentSource::Url { url } => {
                    // For URL sources, assume document type (images should use Image block)
                    json!({
                        "type": "document",
                        "source": {
                            "type": "url",
                            "url": url,
                        }
                    })
                }
            },
        };

        // Add cache_control to last block of last 2 messages only (Anthropic limit: 4 breakpoints)
        // Budget: 1 for system + 2 for last messages = 3 breakpoints (1 under limit)
        if supports_cache && i == blocks.len() - 1 && msg_index >= total_messages.saturating_sub(2)
        {
            item["cache_control"] = json!({"type": "ephemeral"});
        }

        content.push(item);
    }
    content
}

fn convert_assistant_blocks(
    blocks: &[crate::providers::AssistantContentBlock],
    supports_cache: bool,
    msg_index: usize,
    total_messages: usize,
) -> Vec<serde_json::Value> {
    let mut content = vec![];
    for (i, block) in blocks.iter().enumerate() {
        let mut item = match block {
            crate::providers::AssistantContentBlock::Text(t) => {
                json!({"type": "text", "text": t.text})
            }
            crate::providers::AssistantContentBlock::Image(img) => match &img.source {
                crate::providers::ImageSource::Base64 { media_type, data } => {
                    json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": media_type,
                            "data": data,
                        }
                    })
                }
                crate::providers::ImageSource::Url { url } => {
                    json!({
                        "type": "image",
                        "source": {
                            "type": "url",
                            "url": url,
                        }
                    })
                }
            },
            crate::providers::AssistantContentBlock::ToolUse(tu) => {
                json!({
                    "type": "tool_use",
                    "id": tu.id,
                    "name": tu.name,
                    "input": tu.input,
                })
            }
            crate::providers::AssistantContentBlock::Thinking(th) => {
                json!({
                    "type": "thinking",
                    "thinking": th.thinking,
                    "signature": th.signature,
                })
            }
            crate::providers::AssistantContentBlock::RedactedThinking(rt) => {
                json!({
                    "type": "redacted_thinking",
                    "data": rt.data,
                })
            }
            crate::providers::AssistantContentBlock::Document(doc) => match &doc.source {
                crate::providers::DocumentSource::Text { text } => {
                    json!({"type": "text", "text": text})
                }
                crate::providers::DocumentSource::Base64 { media_type, data } => {
                    // Detect content type: image/* → "image", else → "document"
                    // Anthropic API requires "type": "document" for PDFs and other non-image files
                    let content_type = if media_type.starts_with("image/") {
                        "image"
                    } else {
                        "document"
                    };
                    json!({
                        "type": content_type,
                        "source": {
                            "type": "base64",
                            "media_type": media_type,
                            "data": data,
                        }
                    })
                }
                crate::providers::DocumentSource::Url { url } => {
                    // For URL sources, assume document type (images should use Image block)
                    json!({
                        "type": "document",
                        "source": {
                            "type": "url",
                            "url": url,
                        }
                    })
                }
            },
        };

        if supports_cache && i == blocks.len() - 1 && msg_index >= total_messages.saturating_sub(2)
        {
            item["cache_control"] = json!({"type": "ephemeral"});
        }

        content.push(item);
    }
    content
}

// Anthropic streaming event types
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AnthropicStreamEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: AnthropicMessage },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: AnthropicMessageDelta,
        usage: AnthropicUsage,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        content_block: AnthropicContentBlock,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { delta: AnthropicContentDelta },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop,
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "error")]
    Error { error: AnthropicError },
}

#[derive(Debug, Deserialize)]
struct AnthropicError {
    #[serde(rename = "type")]
    error_type: String,
    #[serde(rename = "message")]
    message: String,
}

#[derive(Debug, Deserialize)]
struct AnthropicMessage {
    usage: AnthropicUsage,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AnthropicMessageDelta {
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(rename = "cache_creation_input_tokens")]
    cache_creation_input_tokens: Option<u32>,
    #[serde(rename = "cache_read_input_tokens")]
    cache_read_input_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AnthropicContentBlock {
    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
    #[serde(rename = "redacted_thinking")]
    RedactedThinking { data: String },
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String },
    #[serde(rename = "text")]
    Text { text: String },
}

#[derive(Debug, Deserialize)]
#[allow(clippy::enum_variant_names)]
#[serde(tag = "type")]
enum AnthropicContentDelta {
    #[serde(rename = "thinking_delta")]
    ThinkingDelta { thinking: String },
    #[serde(rename = "signature_delta")]
    SignatureDelta { signature: String },
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn create_message(&self, request: ProviderRequest) -> Result<ApiStream, ProviderError> {
        let url = format!("{}/messages", self.base_url());
        let body = self.build_request_body(&request)?;
        let headers = self.build_headers()?;

        tracing::debug!(
            method = "POST",
            provider = "anthropic",
            url = %url,
            message_count = request.messages.len(),
            "sending provider request"
        );
        tracing::debug!(request_body = %serde_json::to_string_pretty(&body).unwrap_or_default(), "request body");

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let headers = response.headers().clone();
            let text = response.text().await.unwrap_or_default();
            return Err(ProviderHttpError::new("Anthropic", url, status, text, headers).into());
        }

        let stream = response.bytes_stream();
        // Use large buffer (10_000) to match agent_loop channel and prevent backpressure deadlocks
        // when the consumer is slow (same pattern as agent_loop.rs:726)
        let (tx, rx) = tokio::sync::mpsc::channel::<ApiStreamChunk>(10_000);

        tokio::spawn(async move {
            let mut stream = stream;
            let mut sse_buffer = crate::providers::SseLineBuffer::default();
            let mut last_tool_call = AnthropicToolCallState::default();

            while let Some(result) = stream.next().await {
                if tx.is_closed() {
                    break;
                }
                match result {
                    Ok(bytes) => {
                        parse_anthropic_sse_to_chunks(
                            bytes.as_ref(),
                            &mut sse_buffer,
                            &tx,
                            &mut last_tool_call,
                        )
                        .await;
                    }
                    Err(e) => {
                        let error_msg = format!("Anthropic SSE stream error: {}", e);
                        let is_retryable = e.to_string().contains("timeout")
                            || e.to_string().contains("connection")
                            || e.to_string().contains("incomplete")
                            || e.to_string().contains("decode");
                        tracing::debug!(error = %e, retryable = is_retryable, "Anthropic SSE bytes_stream error");
                        try_send_chunk(
                            &tx,
                            ApiStreamChunk::Error(format!(
                                "{}{}",
                                error_msg,
                                if is_retryable { " (retryable)" } else { "" }
                            )),
                            "error",
                        );
                        break;
                    }
                }
            }
            if !tx.is_closed() {
                finish_anthropic_sse_to_chunks(&mut sse_buffer, &tx, &mut last_tool_call).await;
            }
        });

        let rx_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(Box::pin(rx_stream))
    }

    fn get_model(&self) -> ProviderModel {
        let model_id = &self.config.model_id;
        let info = self
            .config
            .model_info
            .clone()
            .unwrap_or_else(|| get_anthropic_model_info(model_id));

        ProviderModel {
            id: self.config.model_id.clone(),
            info,
        }
    }

    fn name(&self) -> &str {
        "anthropic"
    }
}

#[derive(Default)]
pub struct AnthropicToolCallState {
    id: String,
    name: String,
    arguments: String,
    last_was_text: bool,
}

/// Send a stream chunk via try_send to avoid blocking on full channel.
/// Logs a warning if the channel is full and drops the chunk.
fn get_anthropic_model_info(model_id: &str) -> ModelInfo {
    // Model-specific defaults based on Anthropic's pricing and specs
    if model_id.contains("opus") {
        ModelInfo {
            name: Some("claude-opus-4-6".to_string()),
            max_tokens: Some(128_000),
            context_window: Some(200_000),
            supports_images: Some(true),
            supports_prompt_cache: true,
            supports_reasoning: Some(true),
            input_price: Some(5.0),
            output_price: Some(25.0),
            image_output_price: None,
            thinking_config: None,
            supports_global_endpoint: None,
            cache_writes_price: Some(6.25),
            cache_reads_price: Some(0.5),
            description: None,
            tiers: None,
            temperature: None,
            top_p: None,
            top_k: None,
            supports_tools: Some(true),
            api_format: None,
        }
    } else if model_id.contains("haiku") {
        ModelInfo {
            name: Some("claude-haiku-4-5-20251001".to_string()),
            max_tokens: Some(64_000),
            context_window: Some(200_000),
            supports_images: Some(true),
            supports_prompt_cache: true,
            supports_reasoning: Some(false),
            input_price: Some(1.0),
            output_price: Some(5.0),
            image_output_price: None,
            thinking_config: None,
            supports_global_endpoint: None,
            cache_writes_price: Some(1.25),
            cache_reads_price: Some(0.1),
            description: None,
            tiers: None,
            temperature: None,
            top_p: None,
            top_k: None,
            supports_tools: Some(true),
            api_format: None,
        }
    } else {
        // Default to Claude Sonnet 4.6
        ModelInfo {
            name: Some("claude-sonnet-4-6".to_string()),
            max_tokens: Some(64_000),
            context_window: Some(200_000),
            supports_images: Some(true),
            supports_prompt_cache: true,
            supports_reasoning: Some(true),
            input_price: Some(3.0),
            output_price: Some(15.0),
            image_output_price: None,
            thinking_config: None,
            supports_global_endpoint: None,
            cache_writes_price: Some(3.75),
            cache_reads_price: Some(0.3),
            description: None,
            tiers: None,
            temperature: None,
            top_p: None,
            top_k: None,
            supports_tools: Some(true),
            api_format: None,
        }
    }
}

fn try_send_chunk(
    tx: &tokio::sync::mpsc::Sender<ApiStreamChunk>,
    chunk: ApiStreamChunk,
    chunk_type: &str,
) -> bool {
    match tx.try_send(chunk) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            tracing::warn!(
                "Anthropic provider channel full, dropping {} chunk",
                chunk_type
            );
            false
        }
        Err(TrySendError::Closed(_)) => {
            tracing::debug!(
                "Anthropic provider channel closed, cannot send {} chunk",
                chunk_type
            );
            false
        }
    }
}

async fn process_anthropic_sse_line(
    line: &str,
    tx: &tokio::sync::mpsc::Sender<ApiStreamChunk>,
    last_tool_call: &mut AnthropicToolCallState,
) {
    let line = line.trim();
    if line.is_empty() || line == "data: [DONE]" {
        return;
    }
    if let Some(data) = line
        .strip_prefix("data:")
        .map(|s| s.strip_prefix(" ").unwrap_or(s))
    {
        if let Ok(event) = serde_json::from_str::<AnthropicStreamEvent>(data) {
            process_anthropic_event(event, tx, last_tool_call).await;
        } else {
            tracing::warn!(line = %line, "Anthropic SSE: failed to parse event");
        }
    }
}

/// Parse Anthropic SSE chunk bytes into stream chunks. Extracted for testability.
pub async fn parse_anthropic_sse_to_chunks(
    chunk: &[u8],
    buffer: &mut crate::providers::SseLineBuffer,
    tx: &tokio::sync::mpsc::Sender<ApiStreamChunk>,
    last_tool_call: &mut AnthropicToolCallState,
) {
    for line in buffer.push_chunk(chunk) {
        process_anthropic_sse_line(&line, tx, last_tool_call).await;
    }
    if let Some(err) = buffer.take_error() {
        try_send_chunk(tx, ApiStreamChunk::Error(err), "error");
    }
}

pub async fn finish_anthropic_sse_to_chunks(
    buffer: &mut crate::providers::SseLineBuffer,
    tx: &tokio::sync::mpsc::Sender<ApiStreamChunk>,
    last_tool_call: &mut AnthropicToolCallState,
) {
    if let Some(line) = buffer.finish() {
        process_anthropic_sse_line(&line, tx, last_tool_call).await;
    }
}

#[allow(clippy::unused_async)]
async fn process_anthropic_event(
    event: AnthropicStreamEvent,
    tx: &tokio::sync::mpsc::Sender<ApiStreamChunk>,
    last_tool_call: &mut AnthropicToolCallState,
) {
    match event {
        AnthropicStreamEvent::MessageStart { message } => {
            try_send_chunk(
                tx,
                ApiStreamChunk::Usage(ApiStreamUsageChunk {
                    input_tokens: message.usage.input_tokens,
                    output_tokens: message.usage.output_tokens,
                    cache_write_tokens: message.usage.cache_creation_input_tokens,
                    cache_read_tokens: message.usage.cache_read_input_tokens,
                    reasoning_tokens: None,
                    thoughts_token_count: None,
                    total_cost: None,
                    stop_reason: None,
                    id: None,
                }),
                "usage",
            );
        }
        AnthropicStreamEvent::MessageDelta { delta, usage } => {
            // Emit final output_tokens from MessageDelta — MessageStart only
            // provides initial counts (often output_tokens=0). Without this,
            // Anthropic token accounting and cost tracking are wrong.
            try_send_chunk(
                tx,
                ApiStreamChunk::Usage(ApiStreamUsageChunk {
                    input_tokens: 0,
                    output_tokens: usage.output_tokens,
                    cache_write_tokens: usage.cache_creation_input_tokens,
                    cache_read_tokens: usage.cache_read_input_tokens,
                    reasoning_tokens: None,
                    thoughts_token_count: None,
                    total_cost: None,
                    stop_reason: delta.stop_reason,
                    id: None,
                }),
                "usage_delta",
            );
        }
        AnthropicStreamEvent::MessageStop => {}
        AnthropicStreamEvent::Ping => {
            // Ping events are keepalive, no action needed
        }
        AnthropicStreamEvent::Error { error } => {
            try_send_chunk(
                tx,
                ApiStreamChunk::Error(format!(
                    "Anthropic API error ({}): {}",
                    error.error_type, error.message
                )),
                "error",
            );
        }
        AnthropicStreamEvent::ContentBlockStart { content_block } => match content_block {
            AnthropicContentBlock::Thinking {
                thinking,
                signature,
            } => {
                last_tool_call.last_was_text = false;
                try_send_chunk(
                    tx,
                    ApiStreamChunk::Reasoning(ApiStreamReasoningChunk {
                        reasoning: thinking,
                        details: None,
                        signature,
                        redacted_data: None,
                        id: None,
                    }),
                    "reasoning",
                );
            }
            AnthropicContentBlock::RedactedThinking { data } => {
                last_tool_call.last_was_text = false;
                try_send_chunk(
                    tx,
                    ApiStreamChunk::Reasoning(ApiStreamReasoningChunk {
                        reasoning: "[Redacted thinking block]".to_string(),
                        details: None,
                        signature: None,
                        redacted_data: Some(data.clone()),
                        id: None,
                    }),
                    "reasoning",
                );
            }
            AnthropicContentBlock::ToolUse { id, name } => {
                last_tool_call.id = id.clone();
                last_tool_call.name = name.clone();
                last_tool_call.arguments.clear();
                last_tool_call.last_was_text = false;

                try_send_chunk(
                    tx,
                    ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                        tool_call: ApiStreamToolCall {
                            call_id: Some(id.clone()),
                            function: ApiStreamToolCallFunction {
                                id: Some(id),
                                name: Some(name),
                                arguments: Some("".to_string()),
                            },
                            signature: None,
                        },
                        id: None,
                        signature: None,
                    }),
                    "tool_calls",
                );
            }
            AnthropicContentBlock::Text { text } => {
                // Emit newline between consecutive text blocks
                if last_tool_call.last_was_text {
                    try_send_chunk(
                        tx,
                        ApiStreamChunk::Text(ApiStreamTextChunk {
                            text: "\n".to_string(),
                            id: None,
                            signature: None,
                        }),
                        "text_newline",
                    );
                }
                last_tool_call.last_was_text = true;
                try_send_chunk(
                    tx,
                    ApiStreamChunk::Text(ApiStreamTextChunk {
                        text,
                        id: None,
                        signature: None,
                    }),
                    "text",
                );
            }
        },
        AnthropicStreamEvent::ContentBlockDelta { delta } => match delta {
            AnthropicContentDelta::ThinkingDelta { thinking } => {
                try_send_chunk(
                    tx,
                    ApiStreamChunk::Reasoning(ApiStreamReasoningChunk {
                        reasoning: thinking,
                        details: None,
                        signature: None,
                        redacted_data: None,
                        id: None,
                    }),
                    "reasoning",
                );
            }
            AnthropicContentDelta::SignatureDelta { signature } => {
                try_send_chunk(
                    tx,
                    ApiStreamChunk::Reasoning(ApiStreamReasoningChunk {
                        reasoning: "".to_string(),
                        details: None,
                        signature: Some(signature),
                        redacted_data: None,
                        id: None,
                    }),
                    "reasoning",
                );
            }
            AnthropicContentDelta::TextDelta { text } => {
                try_send_chunk(
                    tx,
                    ApiStreamChunk::Text(ApiStreamTextChunk {
                        text,
                        id: None,
                        signature: None,
                    }),
                    "text",
                );
            }
            AnthropicContentDelta::InputJsonDelta { partial_json } => {
                if !last_tool_call.id.is_empty() && !last_tool_call.name.is_empty() {
                    // Enforce MAX_TOOL_ARGUMENT_SIZE during accumulation to prevent
                    // memory exhaustion from providers sending many small deltas.
                    // This matches the enforcement in openai.rs and minimax.rs.
                    if last_tool_call.arguments.len() + partial_json.len()
                        <= crate::providers::MAX_TOOL_ARGUMENT_SIZE
                    {
                        last_tool_call.arguments.push_str(&partial_json);
                    } else {
                        let remaining = crate::providers::MAX_TOOL_ARGUMENT_SIZE
                            - last_tool_call.arguments.len();
                        if remaining > 0 {
                            let safe_end = partial_json.floor_char_boundary(remaining);
                            last_tool_call.arguments.push_str(&partial_json[..safe_end]);
                        }
                        tracing::warn!(
                            accumulated_size = last_tool_call.arguments.len(),
                            "Anthropic tool call arguments exceeded MAX_TOOL_ARGUMENT_SIZE, truncated"
                        );
                    }
                }
            }
        },
        AnthropicStreamEvent::ContentBlockStop => {
            if !last_tool_call.id.is_empty()
                && !last_tool_call.name.is_empty()
                && let Some(validated_args) = crate::providers::validate_tool_call_args(
                    &last_tool_call.arguments,
                    "Anthropic",
                    "at content_block_stop",
                )
            {
                try_send_chunk(
                    tx,
                    ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                        tool_call: ApiStreamToolCall {
                            call_id: Some(last_tool_call.id.clone()),
                            function: ApiStreamToolCallFunction {
                                id: Some(last_tool_call.id.clone()),
                                name: Some(last_tool_call.name.clone()),
                                arguments: Some(validated_args),
                            },
                            signature: None,
                        },
                        id: None,
                        signature: None,
                    }),
                    "tool_calls",
                );
            }
            last_tool_call.id.clear();
            last_tool_call.name.clear();
            last_tool_call.arguments.clear();
            last_tool_call.last_was_text = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{FunctionDefinition, MessageRole, StorageMessage, ToolDefinition};

    #[test]
    fn test_anthropic_config() {
        let config = AnthropicConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "claude-sonnet-4-6".to_string(),
            model_info: None,
            thinking_budget_tokens: None,
        };
        let provider = AnthropicProvider::new(config).unwrap();
        assert_eq!(provider.base_url(), "https://api.anthropic.com/v1");
    }

    #[test]
    fn test_sanitize_model_id() {
        let config = AnthropicConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "claude-sonnet-4-6:1m:fast".to_string(),
            model_info: None,
            thinking_budget_tokens: None,
        };
        let provider = AnthropicProvider::new(config).unwrap();
        assert_eq!(provider.sanitize_model_id(), "claude-sonnet-4-6");
    }

    #[test]
    fn test_sanitize_model_id_reverse_order() {
        // :fast:1m ordering was broken by sequential ends_with checks
        let config = AnthropicConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "claude-sonnet-4-6:fast:1m".to_string(),
            model_info: None,
            thinking_budget_tokens: None,
        };
        let provider = AnthropicProvider::new(config).unwrap();
        assert_eq!(provider.sanitize_model_id(), "claude-sonnet-4-6");
    }

    #[test]
    fn test_build_request_body_fast_1m_ordering() {
        // :fast:1m ordering should set both beta header and speed field
        let config = AnthropicConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "claude-sonnet-4-6:fast:1m".to_string(),
            model_info: None,
            thinking_budget_tokens: None,
        };
        let provider = AnthropicProvider::new(config).unwrap();
        let request = ProviderRequest {
            system_prompt: "You are a helpful assistant.".to_string(),
            messages: vec![StorageMessage {
                id: None,
                role: MessageRole::User,
                content: crate::providers::MessageContent::Text("Hello".to_string()),
                model_info: None,
                metrics: None,
                ts: None,
            }],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };
        let body = provider.build_request_body(&request).unwrap();
        assert_eq!(
            body["speed"], "fast",
            "speed field should be set for :fast:1m ordering"
        );
        assert_eq!(
            body["model"], "claude-sonnet-4-6",
            "model should be sanitized"
        );
    }

    #[test]
    fn test_build_request_body_1m_fast_ordering() {
        // :1m:fast ordering should also set speed field (regression check)
        let config = AnthropicConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "claude-sonnet-4-6:1m:fast".to_string(),
            model_info: None,
            thinking_budget_tokens: None,
        };
        let provider = AnthropicProvider::new(config).unwrap();
        let request = ProviderRequest {
            system_prompt: "You are a helpful assistant.".to_string(),
            messages: vec![StorageMessage {
                id: None,
                role: MessageRole::User,
                content: crate::providers::MessageContent::Text("Hello".to_string()),
                model_info: None,
                metrics: None,
                ts: None,
            }],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };
        let body = provider.build_request_body(&request).unwrap();
        assert_eq!(
            body["speed"], "fast",
            "speed field should be set for :1m:fast ordering"
        );
        assert_eq!(
            body["model"], "claude-sonnet-4-6",
            "model should be sanitized"
        );
    }

    #[test]
    fn test_build_request_body_basic() {
        let config = AnthropicConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "claude-sonnet-4-6".to_string(),
            model_info: None,
            thinking_budget_tokens: None,
        };
        let provider = AnthropicProvider::new(config).unwrap();

        let request = ProviderRequest {
            system_prompt: "You are a helpful assistant.".to_string(),
            messages: vec![StorageMessage {
                id: None,
                role: MessageRole::User,
                content: crate::providers::MessageContent::Text("Hello".to_string()),
                model_info: None,
                metrics: None,
                ts: None,
            }],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let body = provider.build_request_body(&request).unwrap();
        assert_eq!(body["model"], "claude-sonnet-4-6");
        assert_eq!(body["stream"], true);
        assert_eq!(body["temperature"], 0);
        assert!(!body["system"].as_array().unwrap().is_empty());
    }

    #[test]
    fn test_build_request_body_with_thinking() {
        let config = AnthropicConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "claude-sonnet-4-6".to_string(),
            model_info: Some(ModelInfo {
                name: None,
                max_tokens: Some(64000),
                context_window: Some(200000),
                supports_images: Some(true),
                supports_prompt_cache: true,
                supports_reasoning: Some(true),
                input_price: None,
                output_price: None,
                image_output_price: None,
                thinking_config: None,
                supports_global_endpoint: None,
                cache_writes_price: None,
                cache_reads_price: None,
                description: None,
                tiers: None,
                temperature: None,
                top_p: None,
                top_k: None,
                supports_tools: None,
                api_format: None,
            }),
            thinking_budget_tokens: Some(1024),
        };
        let provider = AnthropicProvider::new(config).unwrap();

        let request = ProviderRequest {
            system_prompt: "You are a helpful assistant.".to_string(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let body = provider.build_request_body(&request).unwrap();
        assert!(body.get("temperature").is_none());
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 1024);
    }

    #[test]
    fn test_build_request_body_with_tools() {
        let config = AnthropicConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "claude-sonnet-4-6".to_string(),
            model_info: None,
            thinking_budget_tokens: None,
        };
        let provider = AnthropicProvider::new(config).unwrap();

        let request = ProviderRequest {
            system_prompt: "You are a helpful assistant.".to_string(),
            messages: vec![],
            tools: Some(vec![ToolDefinition {
                tool_type: "function".to_string(),
                function: FunctionDefinition {
                    name: "read_file".to_string(),
                    description: "Read a file".to_string(),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"}
                        }
                    }),
                },
            }]),
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let body = provider.build_request_body(&request).unwrap();
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "read_file");
        // Default tool_choice is now "auto" (model decides), not "any" (forced tool use)
        assert_eq!(body["tool_choice"]["type"], "auto");
    }

    #[test]
    fn test_build_request_body_with_native_tools_on_but_no_tools() {
        let config = AnthropicConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "claude-sonnet-4-6".to_string(),
            model_info: Some(ModelInfo {
                name: Some("claude-sonnet-4-6".to_string()),
                supports_tools: Some(true),
                ..ModelInfo::default()
            }),
            thinking_budget_tokens: None,
        };
        let provider = AnthropicProvider::new(config).unwrap();

        let request = ProviderRequest {
            system_prompt: "You are a helpful assistant.".to_string(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        // Should not panic even with native_tools_on and tools: None
        let body = provider.build_request_body(&request).unwrap();
        // tools field should not be present when tools is None
        assert!(body.get("tools").is_none());
    }

    #[tokio::test]
    async fn test_try_send_prevents_deadlock_with_slow_consumer() {
        use crate::providers::ApiStreamChunk;
        use tokio::sync::mpsc;
        use tokio::time::{Duration, timeout};

        let (tx, mut rx) = mpsc::channel::<ApiStreamChunk>(10);

        // Fill the channel
        for _ in 0..10 {
            tx.try_send(ApiStreamChunk::Text(ApiStreamTextChunk {
                text: "test".to_string(),
                id: None,
                signature: None,
            }))
            .unwrap();
        }

        // Verify channel is full
        assert!(
            tx.try_send(ApiStreamChunk::Text(ApiStreamTextChunk {
                text: "test".to_string(),
                id: None,
                signature: None,
            }))
            .is_err()
        );

        // Try to send with try_send_chunk - should not block
        let send_result = timeout(
            Duration::from_millis(100),
            tokio::spawn(async move {
                try_send_chunk(
                    &tx,
                    ApiStreamChunk::Text(ApiStreamTextChunk {
                        text: "dropped".to_string(),
                        id: None,
                        signature: None,
                    }),
                    "text",
                );
            }),
        )
        .await;

        // Should complete immediately (not deadlock)
        assert!(send_result.is_ok());

        // Consumer should still be able to receive the original messages
        let mut count = 0;
        while timeout(Duration::from_millis(10), rx.recv()).await.is_ok() {
            count += 1;
            if count >= 10 {
                break;
            }
        }
        assert_eq!(count, 10);
    }
}
