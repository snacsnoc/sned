//! MiniMax provider implementation for sned CLI.
//!
//! Uses MiniMax OpenAI-compatible API endpoint:
//! - International: https://api.minimax.io/v1 (default)
//! - China: https://api.minimaxi.com/v1 (requires api_line="china")
//!
//! ## API Format Constraints (critical - do not change without verifying against MiniMax docs)
//!
//! 1. **Tools**: Must use nested format `{"type":"function","function":{...}}`
//!    - Flat format `{"name":...,"parameters":...}` causes API errors
//!    - See: MiniMax OpenAI API docs (llm-openai-docs/md/minimax/)
//!
//! 2. **reasoning_split**: Must be top-level parameter, NOT nested in `extra_body`
//!    - `extra_body` is an OpenAI Python SDK convenience that merges into the request body
//!    - Sending `{"extra_body":{"reasoning_split":true}}` causes error 2013 "chat content is empty"
//!
//! 3. **max_completion_tokens**: MiniMax uses this parameter name, not `max_tokens`
//!
//! 4. **Message content**: Must be non-null string
//!    - `content: null` causes error 2013
//!    - Use `content: ""` for assistant messages with tool calls but no text

use crate::providers::{
    ApiStream, ApiStreamChunk, ApiStreamReasoningChunk, ApiStreamTextChunk, ApiStreamToolCall,
    ApiStreamToolCallFunction, ApiStreamToolCallsChunk, ApiStreamUsageChunk, AssistantContentBlock,
    MessageContent, MessageRole, ModelInfo, Provider, ProviderError, ProviderHttpError,
    ProviderModel, ProviderRequest, StorageMessage, UserContentBlock,
};
use futures::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc::error::TrySendError;

/// Configuration for the MiniMax provider.
#[derive(Clone)]
pub struct MinimaxConfig {
    pub api_key: String,
    /// "china" for China API, anything else for global
    pub api_line: Option<String>,
    pub model_id: String,
    pub model_info: Option<ModelInfo>,
    pub thinking_budget_tokens: Option<u32>,
}

impl std::fmt::Debug for MinimaxConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MinimaxConfig")
            .field(
                "api_key",
                &format!("***REDACTED ({} chars)***", self.api_key.len()),
            )
            .field("api_line", &self.api_line)
            .field("model_id", &self.model_id)
            .field("model_info", &self.model_info)
            .field("thinking_budget_tokens", &self.thinking_budget_tokens)
            .finish()
    }
}

/// MiniMax API provider.
#[derive(Debug)]
pub struct MinimaxProvider {
    config: MinimaxConfig,
    client: reqwest::Client,
}

impl MinimaxProvider {
    pub fn new(config: MinimaxConfig) -> anyhow::Result<Self> {
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

        if !self.config.api_key.is_empty() {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", self.config.api_key))?,
            );
        }

        Ok(headers)
    }

    fn base_url(&self) -> String {
        // Default to international endpoint (.io) - this is the correct production endpoint
        // China endpoint (.com) only used when explicitly configured via api_line="china"
        // See: llm-openai-docs/md/minimax/platform.minimax.io_docs_api-reference_text-openai-api.md
        if self.config.api_line.as_deref() == Some("china") {
            "https://api.minimaxi.com/v1".to_string()
        } else {
            "https://api.minimax.io/v1".to_string()
        }
    }

    fn canonical_model_id(&self) -> String {
        match self.config.model_id.trim() {
            "minimax-m2.7" | "MiniMax-M2.7" => "MiniMax-M2.7".to_string(),
            "minimax-m2.7-highspeed" | "MiniMax-M2.7-highspeed" => {
                "MiniMax-M2.7-highspeed".to_string()
            }
            "minimax-m2.5" | "MiniMax-M2.5" => "MiniMax-M2.5".to_string(),
            "minimax-m2.5-highspeed" | "MiniMax-M2.5-highspeed" => {
                "MiniMax-M2.5-highspeed".to_string()
            }
            "minimax-m2.1" | "MiniMax-M2.1" => "MiniMax-M2.1".to_string(),
            "minimax-m2.1-highspeed" | "MiniMax-M2.1-highspeed" => {
                "MiniMax-M2.1-highspeed".to_string()
            }
            "minimax-m2" | "MiniMax-M2" => "MiniMax-M2".to_string(),
            other => other.to_string(),
        }
    }

    fn build_request_body(&self, request: &ProviderRequest) -> anyhow::Result<serde_json::Value> {
        let model_id = self.canonical_model_id();
        let model_info = self.get_model_info();
        let native_tools_on = request.tools.as_ref().is_some_and(|t| !t.is_empty());

        let mut messages: Vec<serde_json::Value> = request
            .messages
            .iter()
            .flat_map(convert_message_to_openai)
            .collect();

        if !request.system_prompt.is_empty() {
            messages.insert(
                0,
                json!({
                    "role": "system",
                    "content": request.system_prompt,
                }),
            );
        }

        let mut body = json!({
            "model": model_id,
            "messages": messages,
            "stream": true,
        });

        if let Some(max_tokens) = request
            .max_tokens
            .or(model_info.max_tokens)
            .filter(|m| *m > 0)
        {
            body["max_completion_tokens"] = json!(max_tokens);
        }

        if let Some(temperature) = model_info.temperature {
            body["temperature"] = json!(temperature);
        }
        if let Some(top_p) = model_info.top_p {
            body["top_p"] = json!(top_p);
        }
        if let Some(top_k) = model_info.top_k {
            body["top_k"] = json!(top_k);
        }

        if native_tools_on && let Some(tools) = request.tools.as_ref() {
            let tools_json: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.function.name,
                            "description": t.function.description,
                            "parameters": t.function.parameters,
                        },
                    })
                })
                .collect();
            body["tools"] = json!(tools_json);
            let tool_choice = request
                .tool_choice
                .as_ref()
                .unwrap_or(&crate::providers::ToolChoice::Auto);
            body["tool_choice"] = match tool_choice {
                crate::providers::ToolChoice::Auto => json!("auto"),
                crate::providers::ToolChoice::Required => json!("required"),
                crate::providers::ToolChoice::None => json!("none"),
                crate::providers::ToolChoice::Named(name) => {
                    json!({"type": "function", "function": {"name": name}})
                }
            };
        }

        if model_info.supports_reasoning.unwrap_or(false) {
            body["reasoning_split"] = json!(true);
        }

        Ok(body)
    }

    fn get_model_info(&self) -> ModelInfo {
        let model_id = self.canonical_model_id();

        match model_id.as_str() {
            "MiniMax-M2.7" => ModelInfo {
                name: Some("MiniMax-M2.7".to_string()),
                max_tokens: Some(128_000),
                context_window: Some(204_800),
                supports_images: Some(false),
                supports_prompt_cache: true,
                supports_reasoning: Some(true),
                input_price: Some(0.3),
                output_price: Some(1.2),
                image_output_price: None,
                thinking_config: Some(crate::providers::ThinkingConfig {
                    max_budget: Some(1024),
                    output_price: None,
                    output_price_tiers: None,
                    gemini_thinking_level: None,
                    supports_thinking_level: None,
                }),
                supports_global_endpoint: None,
                cache_writes_price: Some(0.375),
                cache_reads_price: Some(0.06),
                description: Some(
                    "MiniMax M2.7 is built for state-of-the-art coding, agentic tool use."
                        .to_string(),
                ),
                tiers: None,
                temperature: Some(1.0),
                top_p: Some(0.95),
                top_k: Some(40),
                supports_tools: Some(true),
                api_format: None,
            },
            "MiniMax-M2.7-highspeed" => ModelInfo {
                name: Some("MiniMax-M2.7-highspeed".to_string()),
                max_tokens: Some(128_000),
                context_window: Some(204_800),
                supports_images: Some(false),
                supports_prompt_cache: true,
                supports_reasoning: Some(true),
                input_price: Some(0.6),
                output_price: Some(2.4),
                image_output_price: None,
                thinking_config: Some(crate::providers::ThinkingConfig {
                    max_budget: Some(1024),
                    output_price: None,
                    output_price_tiers: None,
                    gemini_thinking_level: None,
                    supports_thinking_level: None,
                }),
                supports_global_endpoint: None,
                cache_writes_price: Some(0.375),
                cache_reads_price: Some(0.06),
                description: Some(
                    "MiniMax M2.7 Highspeed: Same performance, faster and more agile.".to_string(),
                ),
                tiers: None,
                temperature: Some(1.0),
                top_p: Some(0.95),
                top_k: Some(40),
                supports_tools: Some(true),
                api_format: None,
            },
            "MiniMax-M2.5" => ModelInfo {
                name: Some("MiniMax-M2.5".to_string()),
                max_tokens: Some(16_384),
                context_window: Some(204_800),
                supports_images: Some(false),
                supports_prompt_cache: true,
                supports_reasoning: Some(true),
                input_price: Some(0.3),
                output_price: Some(1.2),
                image_output_price: None,
                thinking_config: Some(crate::providers::ThinkingConfig {
                    max_budget: Some(1024),
                    output_price: None,
                    output_price_tiers: None,
                    gemini_thinking_level: None,
                    supports_thinking_level: None,
                }),
                supports_global_endpoint: None,
                cache_writes_price: Some(0.375),
                cache_reads_price: Some(0.03),
                description: Some(
                    "MiniMax M2.5 is built for state-of-the-art coding, agentic tool use."
                        .to_string(),
                ),
                tiers: None,
                temperature: Some(1.0),
                top_p: Some(0.95),
                top_k: Some(40),
                supports_tools: Some(true),
                api_format: None,
            },
            "MiniMax-M2.5-highspeed" => ModelInfo {
                name: Some("MiniMax-M2.5-highspeed".to_string()),
                max_tokens: Some(16_384),
                context_window: Some(204_800),
                supports_images: Some(false),
                supports_prompt_cache: true,
                supports_reasoning: Some(true),
                input_price: Some(0.6),
                output_price: Some(2.4),
                image_output_price: None,
                thinking_config: Some(crate::providers::ThinkingConfig {
                    max_budget: Some(1024),
                    output_price: None,
                    output_price_tiers: None,
                    gemini_thinking_level: None,
                    supports_thinking_level: None,
                }),
                supports_global_endpoint: None,
                cache_writes_price: Some(0.375),
                cache_reads_price: Some(0.03),
                description: Some(
                    "MiniMax M2.5 highspeed: Same performance, faster and more agile.".to_string(),
                ),
                tiers: None,
                temperature: Some(1.0),
                top_p: Some(0.95),
                top_k: Some(40),
                supports_tools: Some(true),
                api_format: None,
            },
            "MiniMax-M2.1" => ModelInfo {
                name: Some("MiniMax-M2.1".to_string()),
                max_tokens: Some(16_384),
                context_window: Some(204_800),
                supports_images: Some(false),
                supports_prompt_cache: true,
                supports_reasoning: Some(true),
                input_price: Some(0.3),
                output_price: Some(1.2),
                image_output_price: None,
                thinking_config: Some(crate::providers::ThinkingConfig {
                    max_budget: Some(1024),
                    output_price: None,
                    output_price_tiers: None,
                    gemini_thinking_level: None,
                    supports_thinking_level: None,
                }),
                supports_global_endpoint: None,
                cache_writes_price: Some(0.375),
                cache_reads_price: Some(0.03),
                description: Some(
                    "MiniMax M2.1 is built for state-of-the-art coding, agentic tool use."
                        .to_string(),
                ),
                tiers: None,
                temperature: Some(1.0),
                top_p: Some(0.95),
                top_k: Some(40),
                supports_tools: Some(true),
                api_format: None,
            },
            "MiniMax-M2.1-highspeed" => ModelInfo {
                name: Some("MiniMax-M2.1-highspeed".to_string()),
                max_tokens: Some(16_384),
                context_window: Some(204_800),
                supports_images: Some(false),
                supports_prompt_cache: true,
                supports_reasoning: Some(true),
                input_price: Some(0.6),
                output_price: Some(2.4),
                image_output_price: None,
                thinking_config: Some(crate::providers::ThinkingConfig {
                    max_budget: Some(1024),
                    output_price: None,
                    output_price_tiers: None,
                    gemini_thinking_level: None,
                    supports_thinking_level: None,
                }),
                supports_global_endpoint: None,
                cache_writes_price: Some(0.375),
                cache_reads_price: Some(0.03),
                description: Some("MiniMax M2.1 highspeed: Faster and more agile.".to_string()),
                tiers: None,
                temperature: Some(1.0),
                top_p: Some(0.95),
                top_k: Some(40),
                supports_tools: Some(true),
                api_format: None,
            },
            "MiniMax-M2" => ModelInfo {
                name: Some("MiniMax-M2".to_string()),
                max_tokens: Some(16_384),
                context_window: Some(204_800),
                supports_images: Some(false),
                supports_prompt_cache: true,
                supports_reasoning: Some(true),
                input_price: Some(0.3),
                output_price: Some(1.2),
                image_output_price: None,
                thinking_config: Some(crate::providers::ThinkingConfig {
                    max_budget: Some(1024),
                    output_price: None,
                    output_price_tiers: None,
                    gemini_thinking_level: None,
                    supports_thinking_level: None,
                }),
                supports_global_endpoint: None,
                cache_writes_price: Some(0.375),
                cache_reads_price: Some(0.03),
                description: Some(
                    "MiniMax M2 - Agentic capabilities, Advanced reasoning.".to_string(),
                ),
                tiers: None,
                temperature: Some(1.0),
                top_p: Some(0.95),
                top_k: Some(20),
                supports_tools: Some(true),
                api_format: None,
            },
            _ => ModelInfo {
                name: Some(model_id),
                max_tokens: Some(128_000),
                context_window: Some(204_800),
                supports_images: Some(false),
                supports_prompt_cache: true,
                supports_reasoning: Some(true),
                input_price: Some(0.3),
                output_price: Some(1.2),
                image_output_price: None,
                thinking_config: None,
                supports_global_endpoint: None,
                cache_writes_price: Some(0.375),
                cache_reads_price: Some(0.03),
                description: None,
                tiers: None,
                temperature: Some(1.0),
                top_p: None,
                top_k: None,
                supports_tools: Some(true),
                api_format: None,
            },
        }
    }
}

/// Convert internal StorageMessage to one or more OpenAI chat format messages.
///
/// OpenAI requires tool results to be separate messages with role="tool",
/// so a single StorageMessage with multiple ToolResult blocks may expand
/// into multiple OpenAI messages.
fn convert_message_to_openai(msg: &StorageMessage) -> Vec<serde_json::Value> {
    let role = match msg.role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
    };

    match &msg.content {
        MessageContent::Text(text) => {
            vec![json!({
                "role": role,
                "content": text,
            })]
        }
        MessageContent::UserBlocks(blocks) => convert_user_blocks_to_openai(role, blocks),
        MessageContent::AssistantBlocks(blocks) => convert_assistant_blocks_to_openai(role, blocks),
    }
}

fn convert_user_blocks_to_openai(
    role: &str,
    blocks: &[UserContentBlock],
) -> Vec<serde_json::Value> {
    // Check if this is a simple text-only message
    let is_simple_text = blocks.len() == 1 && matches!(blocks[0], UserContentBlock::Text(_));

    if is_simple_text && let UserContentBlock::Text(t) = &blocks[0] {
        return vec![json!({
            "role": role,
            "content": t.text,
        })];
    }

    // Separate content parts from tool results
    let mut content_parts = vec![];
    let mut tool_results = vec![];

    for block in blocks {
        match block {
            UserContentBlock::Text(t) => {
                content_parts.push(json!({
                    "type": "text",
                    "text": t.text,
                }));
            }
            UserContentBlock::ToolResult(tr) => {
                let content = match &tr.content {
                    crate::providers::ToolResultContent::Text(text) => text.clone(),
                    crate::providers::ToolResultContent::Blocks(blocks) => blocks
                        .iter()
                        .map(|b| match b {
                            crate::providers::ToolResultContentBlock::Text { text } => text.clone(),
                            _ => String::new(),
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                tool_results.push(json!({
                    "role": "tool",
                    "tool_call_id": tr.tool_use_id,
                    "content": content,
                }));
            }
            UserContentBlock::Image(img) => match &img.source {
                crate::providers::ImageSource::Base64 { media_type, data } => {
                    content_parts.push(json!({
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:{};base64,{}", media_type, data),
                        }
                    }));
                }
                crate::providers::ImageSource::Url { url } => {
                    content_parts.push(json!({
                        "type": "image_url",
                        "image_url": {
                            "url": url,
                        }
                    }));
                }
            },
            UserContentBlock::Document(doc) => match &doc.source {
                crate::providers::DocumentSource::Text { text } => {
                    content_parts.push(json!({
                        "type": "text",
                        "text": text,
                    }));
                }
                _ => {
                    tracing::warn!(
                        "MiniMax dropped unhandled document source type for user content block"
                    );
                }
            },
        }
    }

    // Build result: content message first (if any), then all tool result messages
    let mut result = vec![];

    if !content_parts.is_empty() {
        result.push(json!({
            "role": role,
            "content": content_parts,
        }));
    }

    result.extend(tool_results);
    result
}

fn convert_assistant_blocks_to_openai(
    role: &str,
    blocks: &[AssistantContentBlock],
) -> Vec<serde_json::Value> {
    let mut text_content = String::new();
    let mut tool_calls = vec![];
    let mut reasoning_details = vec![];

    for block in blocks {
        match block {
            AssistantContentBlock::Text(t) => {
                if !text_content.is_empty() {
                    text_content.push('\n');
                }
                text_content.push_str(&t.text);
                if let Some(details) = &t.reasoning_details {
                    reasoning_details.extend(
                        details
                            .iter()
                            .map(|detail| json!({ "text": detail.text.clone() })),
                    );
                }
            }
            AssistantContentBlock::ToolUse(tu) => {
                tool_calls.push(json!({
                    "id": tu.id,
                    "type": "function",
                    "function": {
                        "name": tu.name,
                        "arguments": serde_json::to_string(&tu.input).unwrap_or_else(|_| "{}".to_string()),
                    }
                }));
                if let Some(details) = &tu.reasoning_details {
                    reasoning_details.extend(
                        details
                            .iter()
                            .map(|detail| json!({ "text": detail.text.clone() })),
                    );
                }
            }
            AssistantContentBlock::Thinking(thinking) => {
                // MiniMax requires reasoning_details to be preserved across turns when
                // reasoning_split=true, so convert internal thinking blocks back into
                // the OpenAI-compatible field rather than dropping them.
                reasoning_details.push(json!({ "text": thinking.thinking.clone() }));
            }
            AssistantContentBlock::RedactedThinking(_) => {
                // Redacted thinking cannot be reconstructed into reasoning_details.
            }
            _other => {
                tracing::warn!("MiniMax dropped unhandled assistant content block variant");
            }
        }
    }

    let mut msg = json!({
        "role": role,
    });

    // Content: empty string if there are tool calls but no text
    // MiniMax requires content to be a non-null string (not nullable)
    if tool_calls.is_empty() {
        msg["content"] = json!(text_content);
    } else {
        msg["content"] = json!(if text_content.is_empty() {
            String::new()
        } else {
            text_content
        });
        msg["tool_calls"] = json!(tool_calls);
    }
    if !reasoning_details.is_empty() {
        msg["reasoning_details"] = json!(reasoning_details);
    }

    vec![msg]
}

#[derive(Debug, Deserialize)]
struct OpenAIStreamPromptTokenDetails {
    cached_tokens: u32,
}

#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)]
struct OpenAIStreamUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    prompt_tokens_details: Option<OpenAIStreamPromptTokenDetails>,
    #[serde(rename = "prompt_cache_miss_tokens")]
    prompt_cache_miss_tokens: Option<u32>,
    #[serde(default)]
    total_tokens: u32,
    #[serde(default)]
    total_characters: u32,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct OpenAIStreamResponse {
    id: String,
    choices: Vec<OpenAIStreamChoice>,
    usage: Option<OpenAIStreamUsage>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    object: Option<String>,
    #[serde(default)]
    created: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OpenAIStreamChoice {
    delta: OpenAIStreamDelta,
    finish_reason: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    index: usize,
}

#[derive(Debug, Deserialize, Default)]
struct OpenAIStreamDelta {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<OpenAIStreamToolCall>,
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning_details: Vec<MiniMaxReasoningDetail>,
    #[serde(default)]
    #[allow(dead_code)]
    role: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MiniMaxReasoningDetail {
    text: String,
}

#[derive(Debug, Deserialize)]
struct OpenAIStreamToolCall {
    index: usize,
    id: Option<String>,
    #[serde(rename = "type", default)]
    #[allow(dead_code)]
    tool_type: Option<String>,
    function: Option<OpenAIStreamFunction>,
}

#[derive(Debug, Deserialize)]
struct OpenAIStreamFunction {
    name: Option<String>,
    arguments: Option<String>,
}

/// Send a stream chunk via try_send to avoid blocking on full channel.
/// Logs a warning if the channel is full and drops the chunk.
fn try_send_chunk(
    tx: &tokio::sync::mpsc::Sender<ApiStreamChunk>,
    chunk: ApiStreamChunk,
    chunk_type: &str,
) -> bool {
    match tx.try_send(chunk) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            tracing::warn!(
                "MiniMax provider channel full, dropping {} chunk",
                chunk_type
            );
            false
        }
        Err(TrySendError::Closed(_)) => {
            tracing::debug!(
                "MiniMax provider channel closed, cannot send {} chunk",
                chunk_type
            );
            false
        }
    }
}

const MAX_XML_TOOL_CALL_BUFFER: usize = 64 * 1024;
const XML_TOOL_CALL_CLOSING_TAG: &str = "</minimax:tool_call>";
const MINIMAX_TEXT_FLUSH_THRESHOLD: usize = 64;

fn minimax_text_buffer_should_flush(buffer: &str) -> bool {
    if buffer.is_empty() {
        return false;
    }

    if buffer.contains('\n') {
        return true;
    }

    if buffer.chars().count() >= MINIMAX_TEXT_FLUSH_THRESHOLD {
        return true;
    }

    matches!(
        buffer.chars().last(),
        Some(ch) if ch.is_whitespace() || matches!(ch, '.' | '!' | '?' | ',' | ';' | ':')
    )
}

fn flush_minimax_text_buffer(
    tx: &tokio::sync::mpsc::Sender<ApiStreamChunk>,
    pending_text: &mut String,
    pending_text_signature: &mut Option<String>,
    event_id: &str,
) {
    if pending_text.is_empty() {
        return;
    }

    let text = std::mem::take(pending_text);
    try_send_chunk(
        tx,
        ApiStreamChunk::Text(ApiStreamTextChunk {
            text,
            id: Some(event_id.to_string()),
            signature: pending_text_signature.take(),
        }),
        "text",
    );
}

/// Extract and emit all complete XML tool call blocks from the buffer.
fn extract_and_emit_xml_tool_calls(
    xml_buffer: &mut String,
    tx: &tokio::sync::mpsc::Sender<ApiStreamChunk>,
    event_id: &str,
) {
    while let Some(end_tag) = xml_buffer.find(XML_TOOL_CALL_CLOSING_TAG) {
        let block_end = end_tag + XML_TOOL_CALL_CLOSING_TAG.len();
        let block = xml_buffer[..block_end].to_string();
        let remaining = xml_buffer[block_end..].to_string();
        *xml_buffer = remaining;

        let call_id = format!("minimax-{event_id}");
        let (tool_name, tool_params) = parse_minimax_xml_tool_call(&block).unwrap_or_else(|| {
            (
                "xml_tool_call".to_string(),
                serde_json::to_value(&block).unwrap_or_default(),
            )
        });
        try_send_chunk(
            tx,
            ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                tool_call: ApiStreamToolCall {
                    call_id: Some(call_id.clone()),
                    function: ApiStreamToolCallFunction {
                        id: Some(call_id),
                        name: Some(tool_name),
                        arguments: Some(
                            serde_json::to_string(&tool_params).unwrap_or_else(|_| block.clone()),
                        ),
                    },
                    signature: None,
                },
                id: Some(event_id.to_string()),
                signature: None,
            }),
            "tool_calls",
        );
    }
}

fn parse_minimax_xml_tool_call(xml: &str) -> Option<(String, serde_json::Value)> {
    use regex::Regex;
    use std::sync::LazyLock;

    static NAME_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| Regex::new(r#"<invoke\s+name=["']([^"']+)["']>"#).expect("valid regex"));
    static PARAM_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        Regex::new(r#"(?s)<parameter\s+name=["']([^"']+)["']>(.*?)</parameter>"#)
            .expect("valid regex")
    });

    let tool_name = NAME_RE.captures(xml)?.get(1)?.as_str().to_string();

    let mut params = serde_json::Map::new();
    for cap in PARAM_RE.captures_iter(xml) {
        let name = cap.get(1)?.as_str().to_string();
        let value = cap.get(2)?.as_str().to_string();
        let trimmed = value.trim();
        if ((trimmed.starts_with('{') && trimmed.ends_with('}'))
            || (trimmed.starts_with('[') && trimmed.ends_with(']')))
            && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed)
        {
            params.insert(name, parsed);
            continue;
        }
        params.insert(name, serde_json::Value::String(value));
    }

    Some((tool_name, serde_json::Value::Object(params)))
}

#[derive(Debug, Default)]
struct MinimaxReasoningState {
    emitted_reasoning: String,
    in_thinking_tag: bool,
    tag_carry: String,
}

const THINK_OPEN_MARKERS: [&str; 2] = ["<think>", "<!-- think -->"];
const THINK_CLOSE_MARKERS: [&str; 2] = ["</think>", "<!-- /think -->"];

fn partial_marker_suffix_len(text: &str, markers: &[&str]) -> usize {
    let max_len = markers
        .iter()
        .map(|marker| marker.len().saturating_sub(1))
        .max()
        .unwrap_or(0)
        .min(text.len());
    for len in (1..=max_len).rev() {
        let start = text.len() - len;
        if !text.is_char_boundary(start) {
            continue;
        }
        let suffix = &text[start..];
        if markers
            .iter()
            .any(|marker| marker.len() > suffix.len() && marker.starts_with(suffix))
        {
            return len;
        }
    }
    0
}

fn longest_reasoning_overlap(left: &str, right: &str) -> usize {
    let max_overlap = left.len().min(right.len());
    for overlap in (1..=max_overlap).rev() {
        if !left.is_char_boundary(left.len() - overlap) || !right.is_char_boundary(overlap) {
            continue;
        }
        if left[left.len() - overlap..] == right[..overlap] {
            return overlap;
        }
    }
    0
}

fn normalize_minimax_reasoning_delta(
    state: &mut MinimaxReasoningState,
    reasoning: String,
) -> Option<String> {
    if reasoning.is_empty() {
        return None;
    }

    let overlap = longest_reasoning_overlap(&state.emitted_reasoning, &reasoning);
    let normalized = reasoning[overlap..].to_string();
    if normalized.is_empty() {
        return None;
    }

    state.emitted_reasoning.push_str(&normalized);
    Some(normalized)
}

fn emit_minimax_reasoning(
    tx: &tokio::sync::mpsc::Sender<ApiStreamChunk>,
    reasoning_state: &mut MinimaxReasoningState,
    event_id: &str,
    reasoning: String,
    chunk_type: &str,
) {
    if let Some(reasoning) = normalize_minimax_reasoning_delta(reasoning_state, reasoning) {
        try_send_chunk(
            tx,
            ApiStreamChunk::Reasoning(ApiStreamReasoningChunk {
                reasoning,
                details: None,
                signature: None,
                redacted_data: None,
                id: Some(event_id.to_string()),
            }),
            chunk_type,
        );
    }
}

fn find_next_think_open(text: &str) -> Option<(usize, usize)> {
    let tag = text.find("<think>");
    let comment = text.find("<!-- think -->");
    match (tag, comment) {
        (Some(a), Some(b)) => {
            if a <= b {
                Some((a, "<think>".len()))
            } else {
                Some((b, "<!-- think -->".len()))
            }
        }
        (Some(a), None) => Some((a, "<think>".len())),
        (None, Some(b)) => Some((b, "<!-- think -->".len())),
        (None, None) => None,
    }
}

fn find_next_think_close(text: &str) -> Option<(usize, usize)> {
    let tag = text.find("</think>");
    let comment = text.find("<!-- /think -->");
    match (tag, comment) {
        (Some(a), Some(b)) => {
            if a <= b {
                Some((a, "</think>".len()))
            } else {
                Some((b, "<!-- /think -->".len()))
            }
        }
        (Some(a), None) => Some((a, "</think>".len())),
        (None, Some(b)) => Some((b, "<!-- /think -->".len())),
        (None, None) => None,
    }
}

fn process_minimax_content_delta(
    tx: &tokio::sync::mpsc::Sender<ApiStreamChunk>,
    event_id: &str,
    content_ref: &str,
    pending_text: &mut String,
    pending_text_signature: &mut Option<String>,
    reasoning_state: &mut MinimaxReasoningState,
) {
    let mut content = std::mem::take(&mut reasoning_state.tag_carry);
    content.push_str(content_ref);
    let mut pos = 0usize;
    let mut force_flush_visible = false;

    while pos < content.len() {
        if !reasoning_state.in_thinking_tag {
            if let Some((tag_start, tag_len)) = find_next_think_open(&content[pos..]) {
                let abs_start = pos + tag_start;
                pending_text.push_str(&content[pos..abs_start]);
                *pending_text_signature = Some(event_id.to_string());
                flush_minimax_text_buffer(tx, pending_text, pending_text_signature, event_id);
                reasoning_state.in_thinking_tag = true;
                pos = abs_start + tag_len;
                continue;
            }

            let carry_len = partial_marker_suffix_len(&content[pos..], &THINK_OPEN_MARKERS);
            let emit_end = content.len().saturating_sub(carry_len);
            pending_text.push_str(&content[pos..emit_end]);
            *pending_text_signature = Some(event_id.to_string());
            reasoning_state.tag_carry.push_str(&content[emit_end..]);
            break;
        }

        if let Some((tag_start, tag_len)) = find_next_think_close(&content[pos..]) {
            let abs_start = pos + tag_start;
            emit_minimax_reasoning(
                tx,
                reasoning_state,
                event_id,
                content[pos..abs_start].to_string(),
                "reasoning_from_content",
            );
            reasoning_state.in_thinking_tag = false;
            force_flush_visible = true;
            pos = abs_start + tag_len;
            continue;
        }

        let carry_len = partial_marker_suffix_len(&content[pos..], &THINK_CLOSE_MARKERS);
        let emit_end = content.len().saturating_sub(carry_len);
        emit_minimax_reasoning(
            tx,
            reasoning_state,
            event_id,
            content[pos..emit_end].to_string(),
            "reasoning_from_content",
        );
        reasoning_state.tag_carry.push_str(&content[emit_end..]);
        break;
    }

    if !reasoning_state.in_thinking_tag {
        if force_flush_visible && !pending_text.is_empty() {
            flush_minimax_text_buffer(tx, pending_text, pending_text_signature, event_id);
        } else if minimax_text_buffer_should_flush(pending_text) {
            flush_minimax_text_buffer(tx, pending_text, pending_text_signature, event_id);
        }
    }
}

fn flush_minimax_tag_carry(
    tx: &tokio::sync::mpsc::Sender<ApiStreamChunk>,
    event_id: &str,
    pending_text: &mut String,
    pending_text_signature: &mut Option<String>,
    reasoning_state: &mut MinimaxReasoningState,
) {
    let carry = std::mem::take(&mut reasoning_state.tag_carry);
    if carry.is_empty() {
        return;
    }
    if reasoning_state.in_thinking_tag {
        emit_minimax_reasoning(
            tx,
            reasoning_state,
            event_id,
            carry,
            "reasoning_from_content",
        );
    } else {
        pending_text.push_str(&carry);
        *pending_text_signature = Some(event_id.to_string());
    }
}

#[allow(clippy::unused_async)]
async fn process_minimax_sse_line(
    line: &str,
    tx: &tokio::sync::mpsc::Sender<ApiStreamChunk>,
    accumulated_tool_calls: &mut std::collections::HashMap<usize, (String, String, String)>,
    completed_tool_call_indices: &mut std::collections::HashSet<usize>,
    last_stop_reason: &mut Option<String>,
    xml_buffer: &mut String,
    pending_text: &mut String,
    pending_text_signature: &mut Option<String>,
    reasoning_state: &mut MinimaxReasoningState,
) {
    let line = line.trim();
    if line.is_empty() || line == "data: [DONE]" || line == "[DONE]" {
        return;
    }
    let data = line
        .strip_prefix("data:")
        .map(|s| s.strip_prefix(" ").unwrap_or(s));
    if let Some(inner) = data
        && let Ok(event) = serde_json::from_str::<OpenAIStreamResponse>(inner)
    {
        let mut stop_reason: Option<String> = None;

        if let Some(choice) = event.choices.first() {
            // Handle reasoning_content (MiniMax M2.7+ interleaved thinking)
            if let Some(reasoning) = &choice.delta.reasoning_content {
                flush_minimax_text_buffer(tx, pending_text, pending_text_signature, &event.id);
                emit_minimax_reasoning(
                    tx,
                    reasoning_state,
                    &event.id,
                    reasoning.clone(),
                    "reasoning",
                );
            }
            // Handle reasoning_details array (MiniMax with reasoning_split=true)
            for detail in &choice.delta.reasoning_details {
                flush_minimax_text_buffer(tx, pending_text, pending_text_signature, &event.id);
                emit_minimax_reasoning(
                    tx,
                    reasoning_state,
                    &event.id,
                    detail.text.clone(),
                    "reasoning_details",
                );
            }

            // Handle content: check for MiniMax-M2 XML tool calls
            if let Some(content) = &choice.delta.content {
                if content.contains("<minimax:tool_call>") {
                    flush_minimax_text_buffer(tx, pending_text, pending_text_signature, &event.id);
                    // Buffer XML content (bounded to prevent unbounded memory growth)
                    if xml_buffer.len() + content.len() <= MAX_XML_TOOL_CALL_BUFFER {
                        xml_buffer.push_str(content);
                    } else {
                        tracing::warn!(
                            "MiniMax XML tool call buffer overflow ({} bytes), discarding",
                            xml_buffer.len()
                        );
                        xml_buffer.clear();
                    }

                    // Extract and emit ALL complete XML tool call blocks
                    extract_and_emit_xml_tool_calls(xml_buffer, tx, &event.id);
                    // Don't emit XML as text
                } else if !xml_buffer.is_empty() {
                    flush_minimax_text_buffer(tx, pending_text, pending_text_signature, &event.id);
                    // Continue buffering (bounded)
                    if xml_buffer.len() + content.len() <= MAX_XML_TOOL_CALL_BUFFER {
                        xml_buffer.push_str(content);
                    } else {
                        tracing::warn!(
                            "MiniMax XML tool call buffer overflow ({} bytes), discarding",
                            xml_buffer.len()
                        );
                        xml_buffer.clear();
                    }
                    extract_and_emit_xml_tool_calls(xml_buffer, tx, &event.id);
                } else {
                    process_minimax_content_delta(
                        tx,
                        &event.id,
                        content,
                        pending_text,
                        pending_text_signature,
                        reasoning_state,
                    );
                }
            }

            for tool_call in &choice.delta.tool_calls {
                let idx = tool_call.index;

                if completed_tool_call_indices.contains(&idx) {
                    continue;
                }

                flush_minimax_text_buffer(tx, pending_text, pending_text_signature, &event.id);

                if let Some(id) = &tool_call.id {
                    let entry = accumulated_tool_calls
                        .entry(idx)
                        .or_insert_with(|| (id.clone(), String::new(), String::new()));
                    if entry.0 != id.clone() && !entry.0.is_empty() {
                        tracing::warn!(
                            tool_index = idx,
                            old_id = %entry.0,
                            new_id = %id,
                            "MiniMax tool call id changed at index, resetting accumulated data"
                        );
                        entry.0 = id.clone();
                        entry.1 = String::new();
                        entry.2 = String::new();
                    } else {
                        entry.0 = id.clone();
                    }
                }

                if let Some(func) = &tool_call.function {
                    let entry = accumulated_tool_calls
                        .entry(idx)
                        .or_insert_with(|| (String::new(), String::new(), String::new()));

                    if let Some(name) = &func.name {
                        entry.1 = name.clone();
                    }
                    if let Some(args) = &func.arguments {
                        // Skip empty or whitespace-only argument chunks - MiniMax sends these
                        // before actual JSON content arrives. These aren't garbled, just partial.
                        if args.trim().is_empty() {
                            continue;
                        }
                        if entry.2.is_empty()
                            && !args.starts_with('{')
                            && !args.starts_with('[')
                            && !args.starts_with('"')
                        {
                            tracing::warn!(
                                tool_index = idx,
                                args_preview = args.chars().take(40).collect::<String>(),
                                "MiniMax tool call arguments start with garbled content, discarding chunk"
                            );
                            continue;
                        }
                        if entry.2.len() + args.len() <= crate::providers::MAX_TOOL_ARGUMENT_SIZE {
                            entry.2.push_str(args);
                        } else {
                            let remaining =
                                crate::providers::MAX_TOOL_ARGUMENT_SIZE - entry.2.len();
                            if remaining > 0 {
                                let safe_end = args.floor_char_boundary(remaining);
                                entry.2.push_str(&args[..safe_end]);
                            }
                            tracing::warn!(
                                tool_index = idx,
                                accumulated_size = entry.2.len(),
                                "MiniMax tool call arguments exceeded MAX_TOOL_ARGUMENT_SIZE, truncated"
                            );
                        }
                    }
                }
            }

            if let Some(finish_reason) = &choice.finish_reason
                && (finish_reason == "tool_calls" || finish_reason == "tool_call")
            {
                flush_minimax_text_buffer(tx, pending_text, pending_text_signature, &event.id);
                for (idx, (id, name, args)) in accumulated_tool_calls.iter() {
                    if !id.is_empty()
                        && !name.is_empty()
                        && !completed_tool_call_indices.contains(idx)
                        && let Some(validated_args) = crate::providers::validate_tool_call_args(
                            args,
                            "MiniMax",
                            "on finish_reason:tool_calls",
                        )
                    {
                        completed_tool_call_indices.insert(*idx);
                        try_send_chunk(
                            tx,
                            ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                                tool_call: ApiStreamToolCall {
                                    call_id: Some(id.clone()),
                                    function: ApiStreamToolCallFunction {
                                        id: Some(id.clone()),
                                        name: Some(name.clone()),
                                        arguments: Some(validated_args),
                                    },
                                    signature: None,
                                },
                                id: Some(event.id.clone()),
                                signature: None,
                            }),
                            "tool_calls",
                        );
                    }
                }
            }

            stop_reason = choice.finish_reason.clone();
            if stop_reason.is_some() {
                *last_stop_reason = stop_reason.clone();
            }
        }

        if let Some(usage) = event.usage {
            let cached_tokens = usage
                .prompt_tokens_details
                .as_ref()
                .map_or(0, |d| d.cached_tokens);
            let input_tokens = usage.prompt_tokens.saturating_sub(cached_tokens);
            try_send_chunk(
                tx,
                ApiStreamChunk::Usage(ApiStreamUsageChunk {
                    input_tokens,
                    output_tokens: usage.completion_tokens,
                    cache_write_tokens: usage.prompt_cache_miss_tokens,
                    cache_read_tokens: usage.prompt_tokens_details.map(|d| d.cached_tokens),
                    reasoning_tokens: None,
                    thoughts_token_count: None,
                    total_cost: None,
                    stop_reason,
                    id: Some(event.id.clone()),
                }),
                "usage",
            );
        }
    } else {
        tracing::warn!(
            "MiniMax SSE parse failure for line: {}",
            data.unwrap_or("").chars().take(500).collect::<String>()
        );
        try_send_chunk(
            tx,
            ApiStreamChunk::Error(format!(
                "MiniMax SSE parse failure: {}",
                data.unwrap_or("").chars().take(200).collect::<String>()
            )),
            "error",
        );
    }
}

impl Provider for MinimaxProvider {
    async fn create_message(&self, request: ProviderRequest) -> Result<ApiStream, ProviderError> {
        let url = format!("{}/chat/completions", self.base_url());
        let body = self.build_request_body(&request)?;
        let headers = self.build_headers()?;

        tracing::debug!(
            method = "POST",
            provider = "minimax",
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
            return Err(ProviderHttpError::new("MiniMax", url, status, text, headers).into());
        }

        let stream = response.bytes_stream();
        // Use large buffer (10_000) to match agent_loop channel and prevent backpressure deadlocks
        // when the consumer is slow (same pattern as agent_loop.rs:726)
        let (tx, rx) = tokio::sync::mpsc::channel::<ApiStreamChunk>(10_000);

        tokio::spawn(async move {
            let mut stream = stream;
            let mut sse_buffer = crate::providers::SseLineBuffer::default();
            let mut accumulated_tool_calls: std::collections::HashMap<
                usize,
                (String, String, String),
            > = std::collections::HashMap::with_capacity(4);
            let mut completed_tool_call_indices: std::collections::HashSet<usize> =
                std::collections::HashSet::new();
            let mut last_stop_reason: Option<String> = None;
            let mut xml_buffer = String::new();
            let mut pending_text = String::new();
            let mut pending_text_signature: Option<String> = None;
            let mut reasoning_state = MinimaxReasoningState::default();
            let mut stream_errored = false;

            while let Some(result) = stream.next().await {
                if tx.is_closed() {
                    break;
                }
                match result {
                    Ok(bytes) => {
                        for line in sse_buffer.push_chunk(bytes.as_ref()) {
                            process_minimax_sse_line(
                                &line,
                                &tx,
                                &mut accumulated_tool_calls,
                                &mut completed_tool_call_indices,
                                &mut last_stop_reason,
                                &mut xml_buffer,
                                &mut pending_text,
                                &mut pending_text_signature,
                                &mut reasoning_state,
                            )
                            .await;
                        }
                        if let Some(err) = sse_buffer.take_error() {
                            try_send_chunk(&tx, ApiStreamChunk::Error(err), "error");
                        }
                    }
                    Err(e) => {
                        let error_msg = format!("MiniMax SSE stream error: {e}");
                        let is_retryable = e.to_string().contains("timeout")
                            || e.to_string().contains("connection")
                            || e.to_string().contains("incomplete")
                            || e.to_string().contains("decode");
                        tracing::debug!(error = %e, retryable = is_retryable, "MiniMax SSE bytes_stream error");
                        try_send_chunk(
                            &tx,
                            ApiStreamChunk::Error(format!(
                                "{}{}",
                                error_msg,
                                if is_retryable { " (retryable)" } else { "" }
                            )),
                            "error",
                        );
                        stream_errored = true;
                        break;
                    }
                }
            }
            if !tx.is_closed() && !stream_errored {
                if let Some(line) = sse_buffer.finish() {
                    process_minimax_sse_line(
                        &line,
                        &tx,
                        &mut accumulated_tool_calls,
                        &mut completed_tool_call_indices,
                        &mut last_stop_reason,
                        &mut xml_buffer,
                        &mut pending_text,
                        &mut pending_text_signature,
                        &mut reasoning_state,
                    )
                    .await;
                }

                // Flush any remaining XML tool calls from the buffer
                // Use helper function with "flush" as event ID
                if !xml_buffer.is_empty() && xml_buffer.contains("<minimax:tool_call>") {
                    extract_and_emit_xml_tool_calls(&mut xml_buffer, &tx, "flush");
                }
                flush_minimax_tag_carry(
                    &tx,
                    "flush",
                    &mut pending_text,
                    &mut pending_text_signature,
                    &mut reasoning_state,
                );
                flush_minimax_text_buffer(
                    &tx,
                    &mut pending_text,
                    &mut pending_text_signature,
                    "flush",
                );

                // Flush any accumulated native tool calls that were never emitted
                // (some providers send finish_reason:"stop" instead of "tool_calls")
                for (idx, (id, name, args)) in &accumulated_tool_calls {
                    if !id.is_empty()
                        && !name.is_empty()
                        && !completed_tool_call_indices.contains(idx)
                        && let Some(validated_args) = crate::providers::validate_tool_call_args(
                            args,
                            "MiniMax",
                            "at stream end",
                        )
                    {
                        try_send_chunk(
                            &tx,
                            ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                                tool_call: ApiStreamToolCall {
                                    call_id: Some(id.clone()),
                                    function: ApiStreamToolCallFunction {
                                        id: Some(id.clone()),
                                        name: Some(name.clone()),
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
                }
            }
        });

        let rx_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(Box::pin(rx_stream))
    }

    fn get_model(&self) -> ProviderModel {
        let model_id = self.canonical_model_id();
        ProviderModel {
            id: model_id,
            info: self.get_model_info(),
        }
    }

    fn name(&self) -> &'static str {
        "minimax"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{MessageRole, SharedContentFields, StorageMessage, ToolDefinition};
    use tokio::sync::mpsc;

    #[test]
    fn test_minimax_config() {
        let config = MinimaxConfig {
            api_key: "test-key".to_string(),
            api_line: None,
            model_id: "MiniMax-M2.7".to_string(),
            model_info: None,
            thinking_budget_tokens: Some(1024),
        };
        let provider = MinimaxProvider::new(config).unwrap();
        assert_eq!(provider.base_url(), "https://api.minimax.io/v1");
    }

    #[test]
    fn test_minimax_china_api() {
        let config = MinimaxConfig {
            api_key: "test-key".to_string(),
            api_line: Some("china".to_string()),
            model_id: "MiniMax-M2.7".to_string(),
            model_info: None,
            thinking_budget_tokens: None,
        };
        let provider = MinimaxProvider::new(config).unwrap();
        assert_eq!(provider.base_url(), "https://api.minimaxi.com/v1");
    }

    #[test]
    fn test_build_request_body_basic() {
        let config = MinimaxConfig {
            api_key: "test-key".to_string(),
            api_line: None,
            model_id: "MiniMax-M2.7".to_string(),
            model_info: None,
            thinking_budget_tokens: None,
        };
        let provider = MinimaxProvider::new(config).unwrap();

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
        assert_eq!(body["model"], "MiniMax-M2.7");
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(
            body["messages"][0]["content"],
            "You are a helpful assistant."
        );
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "Hello");
    }

    #[test]
    fn test_build_request_body_sampling_params_m27() {
        let config = MinimaxConfig {
            api_key: "test-key".to_string(),
            api_line: None,
            model_id: "MiniMax-M2.7".to_string(),
            model_info: None,
            thinking_budget_tokens: None,
        };
        let provider = MinimaxProvider::new(config).unwrap();

        let request = ProviderRequest {
            system_prompt: String::new(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let body = provider.build_request_body(&request).unwrap();
        assert_eq!(body["temperature"], 1.0);
        assert_eq!(body["top_p"], 0.95);
        assert_eq!(body["top_k"], 40);
        assert!(
            body.get("topP").is_none(),
            "camelCase topP should be absent"
        );
        assert!(
            body.get("topK").is_none(),
            "camelCase topK should be absent"
        );
    }

    #[test]
    fn test_build_request_body_sampling_params_m2() {
        let config = MinimaxConfig {
            api_key: "test-key".to_string(),
            api_line: None,
            model_id: "MiniMax-M2".to_string(),
            model_info: None,
            thinking_budget_tokens: None,
        };
        let provider = MinimaxProvider::new(config).unwrap();

        let request = ProviderRequest {
            system_prompt: String::new(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let body = provider.build_request_body(&request).unwrap();
        assert_eq!(body["temperature"], 1.0);
        assert_eq!(body["top_p"], 0.95);
        assert_eq!(body["top_k"], 20);
    }

    #[test]
    fn test_build_request_body_sampling_params_unknown_model() {
        let config = MinimaxConfig {
            api_key: "test-key".to_string(),
            api_line: None,
            model_id: "some-unknown-model".to_string(),
            model_info: None,
            thinking_budget_tokens: None,
        };
        let provider = MinimaxProvider::new(config).unwrap();

        let request = ProviderRequest {
            system_prompt: String::new(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let body = provider.build_request_body(&request).unwrap();
        assert_eq!(body["temperature"], 1.0);
        assert!(
            body.get("top_p").is_none(),
            "unknown model should not send top_p"
        );
        assert!(
            body.get("top_k").is_none(),
            "unknown model should not send top_k"
        );
    }

    #[test]
    fn test_build_request_body_sampling_and_reasoning_invariants() {
        let config = MinimaxConfig {
            api_key: "test-key".to_string(),
            api_line: None,
            model_id: "MiniMax-M2.7".to_string(),
            model_info: None,
            thinking_budget_tokens: None,
        };
        let provider = MinimaxProvider::new(config).unwrap();

        let request = ProviderRequest {
            system_prompt: String::new(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: Some(1024),
        };

        let body = provider.build_request_body(&request).unwrap();
        assert_eq!(body["max_completion_tokens"], 1024);
        assert!(
            body.get("max_tokens").is_none(),
            "must use max_completion_tokens, not max_tokens"
        );
        assert_eq!(body["temperature"], 1.0);
        assert_eq!(body["top_p"], 0.95);
        assert_eq!(body["top_k"], 40);
        assert_eq!(body["reasoning_split"], true);
        assert!(
            body.get("extra_body").is_none(),
            "reasoning_split must be top-level, not in extra_body"
        );
    }

    #[test]
    fn test_build_request_body_with_tools() {
        let config = MinimaxConfig {
            api_key: "test-key".to_string(),
            api_line: None,
            model_id: "MiniMax-M2.7".to_string(),
            model_info: None,
            thinking_budget_tokens: None,
        };
        let provider = MinimaxProvider::new(config).unwrap();

        let tools = vec![ToolDefinition {
            tool_type: "function".to_string(),
            function: crate::providers::FunctionDefinition {
                name: "read_file".to_string(),
                description: "Read a file".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"}
                    },
                    "required": ["path"]
                }),
            },
        }];

        let request = ProviderRequest {
            system_prompt: "You are a helpful assistant.".to_string(),
            messages: vec![StorageMessage {
                id: None,
                role: MessageRole::User,
                content: crate::providers::MessageContent::Text("Read Cargo.toml".to_string()),
                model_info: None,
                metrics: None,
                ts: None,
            }],
            tools: Some(tools),
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let body = provider.build_request_body(&request).unwrap();
        assert!(body["tools"].is_array());
        let tools_arr = body["tools"].as_array().unwrap();
        assert_eq!(tools_arr.len(), 1);
        // MiniMax OpenAI-compatible API requires nested "function" object with "type": "function"
        assert_eq!(tools_arr[0]["type"], "function");
        assert_eq!(tools_arr[0]["function"]["name"], "read_file");
        assert_eq!(tools_arr[0]["function"]["description"], "Read a file");
    }

    #[test]
    fn test_build_request_body_with_native_tools_on_but_no_tools() {
        let config = MinimaxConfig {
            api_key: "test-key".to_string(),
            api_line: None,
            model_id: "MiniMax-M2.7".to_string(),
            model_info: Some(ModelInfo {
                name: Some("MiniMax-M2.7".to_string()),
                supports_tools: Some(true),
                ..ModelInfo::default()
            }),
            thinking_budget_tokens: None,
        };
        let provider = MinimaxProvider::new(config).unwrap();

        let request = ProviderRequest {
            system_prompt: "You are a helpful assistant.".to_string(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let body = provider.build_request_body(&request).unwrap();
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn test_build_request_body_uses_request_max_tokens_override() {
        let config = MinimaxConfig {
            api_key: "test-key".to_string(),
            api_line: None,
            model_id: "MiniMax-M2.7".to_string(),
            model_info: None,
            thinking_budget_tokens: None,
        };
        let provider = MinimaxProvider::new(config).unwrap();

        let request = ProviderRequest {
            system_prompt: String::new(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: Some(2048),
        };

        let body = provider.build_request_body(&request).unwrap();
        // MiniMax uses max_completion_tokens (not max_tokens)
        assert_eq!(body["max_completion_tokens"], 2048);
    }

    #[test]
    fn test_build_request_body_canonicalizes_model_alias() {
        let config = MinimaxConfig {
            api_key: "test-key".to_string(),
            api_line: None,
            model_id: "minimax-m2.7".to_string(),
            model_info: None,
            thinking_budget_tokens: None,
        };
        let provider = MinimaxProvider::new(config).unwrap();

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
        assert_eq!(body["model"], "MiniMax-M2.7");
        assert_eq!(provider.get_model().id, "MiniMax-M2.7");
    }

    #[test]
    fn test_convert_assistant_with_tool_calls() {
        let msg = StorageMessage {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::AssistantBlocks(vec![
                AssistantContentBlock::Text(crate::providers::TextContentBlock {
                    text: "I'll read the file.".to_string(),
                    shared: SharedContentFields {
                        call_id: None,
                        signature: None,
                    },
                    reasoning_details: None,
                }),
                AssistantContentBlock::ToolUse(crate::providers::ToolUseBlock {
                    id: "call_abc".to_string(),
                    name: "read_file".to_string(),
                    input: json!({"path": "Cargo.toml"}),
                    shared: SharedContentFields {
                        call_id: None,
                        signature: None,
                    },
                    reasoning_details: None,
                }),
            ]),
            model_info: None,
            metrics: None,
            ts: None,
        };

        let openai_msgs = convert_message_to_openai(&msg);
        assert_eq!(openai_msgs.len(), 1);
        let openai_msg = &openai_msgs[0];
        assert_eq!(openai_msg["role"], "assistant");
        assert!(openai_msg["tool_calls"].is_array());
        let tool_calls = openai_msg["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "call_abc");
        assert_eq!(tool_calls[0]["type"], "function");
        assert_eq!(tool_calls[0]["function"]["name"], "read_file");
        // arguments should be a JSON string
        let args = tool_calls[0]["function"]["arguments"].as_str().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(args).unwrap();
        assert_eq!(parsed["path"], "Cargo.toml");
    }

    #[test]
    fn test_build_request_body_preserves_reasoning_details_in_history() {
        let config = MinimaxConfig {
            api_key: "test-key".to_string(),
            api_line: None,
            model_id: "MiniMax-M2.7".to_string(),
            model_info: None,
            thinking_budget_tokens: None,
        };
        let provider = MinimaxProvider::new(config).unwrap();

        let request = ProviderRequest {
            system_prompt: String::new(),
            messages: vec![StorageMessage {
                id: None,
                role: MessageRole::Assistant,
                content: MessageContent::AssistantBlocks(vec![
                    AssistantContentBlock::Thinking(crate::providers::ThinkingBlock {
                        thinking: "Need to inspect the tool output before replying.".to_string(),
                        signature: "sig_1".to_string(),
                        shared: SharedContentFields {
                            call_id: None,
                            signature: None,
                        },
                        summary: None,
                    }),
                    AssistantContentBlock::ToolUse(crate::providers::ToolUseBlock {
                        id: "call_abc".to_string(),
                        name: "read_file".to_string(),
                        input: json!({"path": "Cargo.toml"}),
                        shared: SharedContentFields {
                            call_id: None,
                            signature: None,
                        },
                        reasoning_details: None,
                    }),
                ]),
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
        let message = &body["messages"][0];
        assert_eq!(message["role"], "assistant");
        assert_eq!(
            message["reasoning_details"][0]["text"],
            "Need to inspect the tool output before replying."
        );
        assert_eq!(message["tool_calls"][0]["function"]["name"], "read_file");
    }

    #[test]
    fn test_convert_tool_result() {
        let msg = StorageMessage {
            id: None,
            role: MessageRole::User,
            content: MessageContent::UserBlocks(vec![UserContentBlock::ToolResult(
                crate::providers::ToolResultBlock {
                    tool_use_id: "call_abc".to_string(),
                    content: crate::providers::ToolResultContent::Text(
                        "[package]\nname = \"test\"".to_string(),
                    ),
                    shared: SharedContentFields {
                        call_id: None,
                        signature: None,
                    },
                },
            )]),
            model_info: None,
            metrics: None,
            ts: None,
        };

        let openai_msgs = convert_message_to_openai(&msg);
        assert_eq!(openai_msgs.len(), 1);
        let openai_msg = &openai_msgs[0];
        assert_eq!(openai_msg["role"], "tool");
        assert_eq!(openai_msg["tool_call_id"], "call_abc");
        assert_eq!(openai_msg["content"], "[package]\nname = \"test\"");
    }

    #[test]
    fn test_build_request_body_includes_reasoning_split() {
        let config = MinimaxConfig {
            api_key: "test-key".to_string(),
            api_line: None,
            model_id: "MiniMax-M2.7".to_string(),
            model_info: None,
            thinking_budget_tokens: None,
        };
        let provider = MinimaxProvider::new(config).unwrap();

        let request = ProviderRequest {
            system_prompt: String::new(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let body = provider.build_request_body(&request).unwrap();
        // reasoning_split must be a top-level parameter (not nested in extra_body)
        assert_eq!(body["reasoning_split"], true);
        assert!(body.get("extra_body").is_none());
    }

    #[test]
    fn test_convert_assistant_tool_calls_only_empty_content_string() {
        // MiniMax requires content to be a non-null string
        // When assistant has tool calls but no text, content should be "" not null
        let msg = StorageMessage {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::AssistantBlocks(vec![AssistantContentBlock::ToolUse(
                crate::providers::ToolUseBlock {
                    id: "call_abc".to_string(),
                    name: "read_file".to_string(),
                    input: json!({"path": "Cargo.toml"}),
                    shared: SharedContentFields {
                        call_id: None,
                        signature: None,
                    },
                    reasoning_details: None,
                },
            )]),
            model_info: None,
            metrics: None,
            ts: None,
        };

        let openai_msgs = convert_message_to_openai(&msg);
        assert_eq!(openai_msgs.len(), 1);
        let openai_msg = &openai_msgs[0];
        assert_eq!(openai_msg["role"], "assistant");
        // content must be a string (not null) for MiniMax API
        assert_eq!(openai_msg["content"], "");
        assert!(openai_msg["tool_calls"].is_array());
    }

    #[test]
    fn test_request_body_schema_invariants() {
        // Validates critical MiniMax API format constraints.
        // If this test fails after a "fix", read the MINIMAX_API_GOTCHAS doc comment
        // at the top of this file before proceeding.
        let config = MinimaxConfig {
            api_key: "test-key".to_string(),
            api_line: None,
            model_id: "MiniMax-M2.7".to_string(),
            model_info: None,
            thinking_budget_tokens: None,
        };
        let provider = MinimaxProvider::new(config).unwrap();

        let tools = vec![ToolDefinition {
            tool_type: "function".to_string(),
            function: crate::providers::FunctionDefinition {
                name: "read_file".to_string(),
                description: "Read a file".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }),
            },
        }];

        let request = ProviderRequest {
            system_prompt: "You are helpful.".to_string(),
            messages: vec![StorageMessage {
                id: None,
                role: MessageRole::User,
                content: crate::providers::MessageContent::Text("Read Cargo.toml".to_string()),
                model_info: None,
                metrics: None,
                ts: None,
            }],
            tools: Some(tools),
            tool_choice: None,
            use_response_api: None,
            max_tokens: Some(1024),
        };

        let body = provider.build_request_body(&request).unwrap();

        // 1. reasoning_split must be top-level (not nested in extra_body)
        assert_eq!(
            body["reasoning_split"], true,
            "reasoning_split must be top-level"
        );
        assert!(
            body.get("extra_body").is_none(),
            "extra_body should not exist in request body"
        );

        // 2. Tools must use nested format with "type":"function"
        let tools_arr = body["tools"].as_array().expect("tools should be array");
        assert_eq!(tools_arr.len(), 1);
        let tool = &tools_arr[0];
        assert_eq!(tool["type"], "function", "tool must have type='function'");
        assert!(
            tool["function"].is_object(),
            "tool must have nested 'function' object"
        );
        assert_eq!(tool["function"]["name"], "read_file");

        // 3. Must use max_completion_tokens (not max_tokens)
        assert_eq!(
            body["max_completion_tokens"], 1024,
            "must use max_completion_tokens"
        );
        assert!(
            body.get("max_tokens").is_none(),
            "max_tokens should not exist in request body"
        );

        // 4. Messages must have non-null content
        let messages = body["messages"]
            .as_array()
            .expect("messages should be array");
        for (i, msg) in messages.iter().enumerate() {
            assert!(
                msg.get("content").is_some(),
                "message[{i}] must have 'content' field"
            );
            if let Some(content) = msg.get("content") {
                assert!(
                    !content.is_null(),
                    "message[{i}] content must not be null (use empty string if no text)"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_split_sse_line_keeps_minimax_tool_call_arguments_intact() {
        let payload = serde_json::json!({
            "id": "evt_1",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": "MiniMax-M2.7",
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "function": {
                            "name": "write_to_file",
                            "arguments": "{\"path\":\"README.md\",\"content\":\"hello world\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let line = format!("data: {}\n", payload);
        let split = line.len() / 2;
        let (first, second) = line.as_bytes().split_at(split);

        let mut buffer = crate::providers::SseLineBuffer::default();
        let (tx, mut rx) = mpsc::channel(4);
        let mut accumulated_tool_calls = std::collections::HashMap::with_capacity(4);
        let mut completed_tool_call_indices = std::collections::HashSet::new();

        let mut last_stop_reason: Option<String> = None;
        let mut xml_buffer = String::new();
        let mut pending_text = String::new();
        let mut pending_text_signature: Option<String> = None;
        let mut reasoning_state = MinimaxReasoningState::default();

        assert!(buffer.push_chunk(first).is_empty());
        for line in buffer.push_chunk(second) {
            process_minimax_sse_line(
                &line,
                &tx,
                &mut accumulated_tool_calls,
                &mut completed_tool_call_indices,
                &mut last_stop_reason,
                &mut xml_buffer,
                &mut pending_text,
                &mut pending_text_signature,
                &mut reasoning_state,
            )
            .await;
        }

        let chunk = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out waiting for tool call chunk")
            .expect("expected a tool call chunk");
        match chunk {
            crate::providers::ApiStreamChunk::ToolCalls(tool_chunk) => {
                assert_eq!(tool_chunk.tool_call.call_id.as_deref(), Some("call_1"));
                assert_eq!(
                    tool_chunk.tool_call.function.arguments.as_deref(),
                    Some("{\"path\":\"README.md\",\"content\":\"hello world\"}")
                );
            }
            other => panic!("expected tool call chunk, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_minimax_text_chunks_coalesce_until_sentence_boundary() {
        let mut accumulated_tool_calls = std::collections::HashMap::with_capacity(4);
        let mut completed_tool_call_indices = std::collections::HashSet::new();
        let mut last_stop_reason: Option<String> = None;
        let mut xml_buffer = String::new();
        let mut pending_text = String::new();
        let mut pending_text_signature: Option<String> = None;
        let mut reasoning_state = MinimaxReasoningState::default();
        let (tx, mut rx) = mpsc::channel(4);

        let first =
            r#"data: {"id":"evt_1","choices":[{"index":0,"delta":{"content":"I have a"}}]}"#;
        process_minimax_sse_line(
            first,
            &tx,
            &mut accumulated_tool_calls,
            &mut completed_tool_call_indices,
            &mut last_stop_reason,
            &mut xml_buffer,
            &mut pending_text,
            &mut pending_text_signature,
            &mut reasoning_state,
        )
        .await;
        assert!(rx.try_recv().is_err(), "partial text should stay buffered");

        let second =
            r#"data: {"id":"evt_2","choices":[{"index":0,"delta":{"content":" complete."}}]}"#;
        process_minimax_sse_line(
            second,
            &tx,
            &mut accumulated_tool_calls,
            &mut completed_tool_call_indices,
            &mut last_stop_reason,
            &mut xml_buffer,
            &mut pending_text,
            &mut pending_text_signature,
            &mut reasoning_state,
        )
        .await;

        let chunk = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out waiting for coalesced text chunk")
            .expect("expected a text chunk");
        match chunk {
            crate::providers::ApiStreamChunk::Text(text_chunk) => {
                assert_eq!(text_chunk.text, "I have a complete.");
            }
            other => panic!("expected text chunk, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_minimax_reasoning_chunks_strip_overlapping_prefixes() {
        let mut accumulated_tool_calls = std::collections::HashMap::with_capacity(4);
        let mut completed_tool_call_indices = std::collections::HashSet::new();
        let mut last_stop_reason: Option<String> = None;
        let mut xml_buffer = String::new();
        let mut pending_text = String::new();
        let mut pending_text_signature: Option<String> = None;
        let mut reasoning_state = MinimaxReasoningState::default();
        let (tx, mut rx) = mpsc::channel(8);

        for line in [
            r#"data: {"id":"evt_1","choices":[{"index":0,"delta":{"reasoning_content":"The"}}]}"#,
            r#"data: {"id":"evt_2","choices":[{"index":0,"delta":{"reasoning_content":"The user"}}]}"#,
            r#"data: {"id":"evt_3","choices":[{"index":0,"delta":{"reasoning_content":" user wants"}}]}"#,
        ] {
            process_minimax_sse_line(
                line,
                &tx,
                &mut accumulated_tool_calls,
                &mut completed_tool_call_indices,
                &mut last_stop_reason,
                &mut xml_buffer,
                &mut pending_text,
                &mut pending_text_signature,
                &mut reasoning_state,
            )
            .await;
        }

        let mut reasoning = String::new();
        while let Ok(chunk) = rx.try_recv() {
            if let ApiStreamChunk::Reasoning(reasoning_chunk) = chunk {
                reasoning.push_str(&reasoning_chunk.reasoning);
            }
        }

        assert_eq!(reasoning, "The user wants");
    }

    /// Regression tests for Unicode edge cases in
    /// `longest_reasoning_overlap`. The function uses
    /// `is_char_boundary` to avoid splitting multi-byte UTF-8
    /// sequences, but the existing tests only exercise ASCII.
    /// These tests guard against regressions where a refactor
    /// drops the char-boundary check and silently corrupts
    /// CJK / emoji / combining-character content.

    #[test]
    fn test_longest_reasoning_overlap_ascii() {
        assert_eq!(longest_reasoning_overlap("hello world", "world peace"), 5);
        assert_eq!(longest_reasoning_overlap("abc", "xyz"), 0);
        assert_eq!(longest_reasoning_overlap("", "anything"), 0);
        assert_eq!(longest_reasoning_overlap("anything", ""), 0);
    }

    #[test]
    fn test_longest_reasoning_overlap_cjk() {
        // CJK characters are 3 bytes each in UTF-8. The overlap
        // must be measured in bytes but only at char boundaries.
        let left = "思考一下这个问题的解决方案";
        let right = "方案应该是这样的";
        // "方案" is the overlapping 6-byte (2-char) suffix/prefix.
        assert_eq!(longest_reasoning_overlap(left, right), 6);
    }

    #[test]
    fn test_longest_reasoning_overlap_cjk_no_overlap() {
        let left = "思考问题";
        let right = "完全不同的内容";
        assert_eq!(longest_reasoning_overlap(left, right), 0);
    }

    #[test]
    fn test_longest_reasoning_overlap_emoji() {
        // Emoji are 4 bytes each in UTF-8. An overlap at a
        // non-char-boundary would be a bug.
        let left = "I love 🦀 Rust";
        let right = "🦀 Rust is great";
        // "🦀 Rust" is the overlap: 4 + 1 + 4 = 9 bytes (2 chars).
        assert_eq!(longest_reasoning_overlap(left, right), 9);
    }

    #[test]
    fn test_longest_reasoning_overlap_combining_characters() {
        // "é" can be encoded as a single codepoint (U+00E9, 2 bytes)
        // or as "e" + combining acute (U+0065 U+0301, 3 bytes).
        // The overlap must respect char boundaries in both forms.
        let left = "café"; // precomposed é
        let right = "fé"; // shares "fé" suffix/prefix
        // "fé" = 1 + 2 = 3 bytes.
        assert_eq!(longest_reasoning_overlap(left, right), 3);
    }

    #[test]
    fn test_longest_reasoning_overlap_mixed_scripts() {
        // Mixed ASCII + CJK. The overlap must not split a CJK
        // character mid-byte.
        let left = "answer: 思考";
        let right = "思考过程";
        // "思考" is 6 bytes (2 × 3-byte CJK chars).
        assert_eq!(longest_reasoning_overlap(left, right), 6);
    }

    #[test]
    fn test_longest_reasoning_overlap_does_not_split_multibyte() {
        // Construct a case where a naive byte-based overlap
        // (without is_char_boundary checks) would find a
        // "false positive" by matching partial bytes of a
        // CJK character.
        //
        // "思考" is 6 bytes: [0xE6, 0x80, 0x9D, 0xE8, 0x80, 0x83]
        // If left = "X思考" and right = "考Y", a naive byte
        // check might try to match "考" (bytes 3..6) against
        // the start of right. "考" is 3 bytes [0xxE8,0x80,0x83].
        // right starts with "考Y" = [0xE8,0x80,0x83,0x59].
        // The last 3 bytes of left ARE the first 3 bytes of right
        // (both are "考"), so overlap = 3 is correct.
        // This test verifies the char-boundary check does NOT
        // prevent a legitimate same-character overlap.
        let left = "X思考";
        let right = "考Y";
        assert_eq!(longest_reasoning_overlap(left, right), 3);
    }

    #[test]
    fn test_longest_reasoning_overlap_emoji_in_middle() {
        // Emoji in the middle of both strings, with the overlap
        // landing on the emoji itself.
        let left = "hello 🌍 world";
        let right = "🌍 world peace";
        // "🌍 world" = 4 + 1 + 5 = 10 bytes.
        assert_eq!(longest_reasoning_overlap(left, right), 10);
    }

    fn collect_minimax_content_deltas(chunks: &[String]) -> (String, String) {
        let (tx, mut rx) = mpsc::channel(32);
        let mut pending_text = String::new();
        let mut pending_text_signature = None;
        let mut reasoning_state = MinimaxReasoningState::default();

        for (index, chunk) in chunks.iter().enumerate() {
            process_minimax_content_delta(
                &tx,
                &format!("evt_{index}"),
                chunk,
                &mut pending_text,
                &mut pending_text_signature,
                &mut reasoning_state,
            );
        }
        flush_minimax_tag_carry(
            &tx,
            "flush",
            &mut pending_text,
            &mut pending_text_signature,
            &mut reasoning_state,
        );
        flush_minimax_text_buffer(&tx, &mut pending_text, &mut pending_text_signature, "flush");

        let mut visible = String::new();
        let mut reasoning = String::new();
        while let Ok(chunk) = rx.try_recv() {
            match chunk {
                ApiStreamChunk::Text(text_chunk) => visible.push_str(&text_chunk.text),
                ApiStreamChunk::Reasoning(reasoning_chunk) => {
                    reasoning.push_str(&reasoning_chunk.reasoning)
                }
                other => panic!("unexpected chunk: {other:?}"),
            }
        }
        (visible, reasoning)
    }

    #[test]
    fn test_minimax_content_think_markers_survive_every_chunk_boundary() {
        for (open, close) in [
            ("<think>", "</think>"),
            ("<!-- think -->", "<!-- /think -->"),
        ] {
            for split in 1..open.len() {
                let chunks = vec![
                    format!("before {}", &open[..split]),
                    format!("{}hidden{close} after", &open[split..]),
                ];
                let (visible, reasoning) = collect_minimax_content_deltas(&chunks);
                assert_eq!(visible, "before  after", "open marker split at {split}");
                assert_eq!(reasoning, "hidden", "open marker split at {split}");
            }

            for split in 1..close.len() {
                let chunks = vec![
                    format!("before {open}hidden{}", &close[..split]),
                    format!("{} after", &close[split..]),
                ];
                let (visible, reasoning) = collect_minimax_content_deltas(&chunks);
                assert_eq!(visible, "before  after", "close marker split at {split}");
                assert_eq!(reasoning, "hidden", "close marker split at {split}");
            }
        }
    }

    #[test]
    fn test_minimax_incomplete_think_marker_carry_flushes_to_current_stream() {
        let (visible, reasoning) = collect_minimax_content_deltas(&["visible <thi".to_string()]);
        assert_eq!(visible, "visible <thi");
        assert!(reasoning.is_empty());

        let (visible, reasoning) =
            collect_minimax_content_deltas(&["<think>hidden</thi".to_string()]);
        assert!(visible.is_empty());
        assert_eq!(reasoning, "hidden</thi");
    }

    #[tokio::test]
    async fn test_minimax_content_think_tags_emit_reasoning_not_visible_text() {
        let mut accumulated_tool_calls = std::collections::HashMap::with_capacity(4);
        let mut completed_tool_call_indices = std::collections::HashSet::new();
        let mut last_stop_reason: Option<String> = None;
        let mut xml_buffer = String::new();
        let mut pending_text = String::new();
        let mut pending_text_signature: Option<String> = None;
        let mut reasoning_state = MinimaxReasoningState::default();
        let (tx, mut rx) = mpsc::channel(8);

        process_minimax_sse_line(
            r#"data: {"id":"evt_1","choices":[{"index":0,"delta":{"content":"Visible prefix <think>hidden analysis</think> visible answer"}}]}"#,
            &tx,
            &mut accumulated_tool_calls,
            &mut completed_tool_call_indices,
            &mut last_stop_reason,
            &mut xml_buffer,
            &mut pending_text,
            &mut pending_text_signature,
            &mut reasoning_state,
        )
        .await;
        flush_minimax_text_buffer(&tx, &mut pending_text, &mut pending_text_signature, "flush");

        let mut visible = String::new();
        let mut reasoning = String::new();
        while let Ok(chunk) = rx.try_recv() {
            match chunk {
                ApiStreamChunk::Text(text_chunk) => visible.push_str(&text_chunk.text),
                ApiStreamChunk::Reasoning(reasoning_chunk) => {
                    reasoning.push_str(&reasoning_chunk.reasoning)
                }
                _ => {}
            }
        }

        assert_eq!(visible, "Visible prefix  visible answer");
        assert_eq!(reasoning, "hidden analysis");
    }

    #[tokio::test]
    async fn test_minimax_content_think_close_flushes_visible_answer_promptly() {
        let mut accumulated_tool_calls = std::collections::HashMap::with_capacity(4);
        let mut completed_tool_call_indices = std::collections::HashSet::new();
        let mut last_stop_reason: Option<String> = None;
        let mut xml_buffer = String::new();
        let mut pending_text = String::new();
        let mut pending_text_signature: Option<String> = None;
        let mut reasoning_state = MinimaxReasoningState::default();
        let (tx, mut rx) = mpsc::channel(8);

        process_minimax_sse_line(
            r#"data: {"id":"evt_1","choices":[{"index":0,"delta":{"content":"I<think>hidden start"}}]}"#,
            &tx,
            &mut accumulated_tool_calls,
            &mut completed_tool_call_indices,
            &mut last_stop_reason,
            &mut xml_buffer,
            &mut pending_text,
            &mut pending_text_signature,
            &mut reasoning_state,
        )
        .await;

        let first_chunk = rx
            .try_recv()
            .expect("prefix should flush before think block");
        match first_chunk {
            ApiStreamChunk::Text(text_chunk) => assert_eq!(text_chunk.text, "I"),
            other => panic!("expected text chunk, got {other:?}"),
        }

        process_minimax_sse_line(
            r#"data: {"id":"evt_2","choices":[{"index":0,"delta":{"content":" and more</think>final answer"}}]}"#,
            &tx,
            &mut accumulated_tool_calls,
            &mut completed_tool_call_indices,
            &mut last_stop_reason,
            &mut xml_buffer,
            &mut pending_text,
            &mut pending_text_signature,
            &mut reasoning_state,
        )
        .await;

        let mut reasoning = String::new();
        let mut answer = String::new();
        while let Ok(chunk) = rx.try_recv() {
            match chunk {
                ApiStreamChunk::Reasoning(reasoning_chunk) => {
                    reasoning.push_str(&reasoning_chunk.reasoning);
                }
                ApiStreamChunk::Text(text_chunk) => {
                    answer.push_str(&text_chunk.text);
                }
                other => panic!("unexpected chunk after think close: {other:?}"),
            }
        }

        assert_eq!(reasoning, "hidden start and more");
        assert_eq!(answer, "final answer");
    }

    #[test]
    fn test_parse_xml_tool_call_json_values() {
        let xml = r#"<minimax:tool_call><invoke name="read_file"><parameter name="paths">["src/main.rs","lib.rs"]</parameter><parameter name="start_line">10</parameter></invoke></tool_call>"#;
        let (name, params) = parse_minimax_xml_tool_call(xml).unwrap();
        assert_eq!(name, "read_file");
        assert!(params["paths"].is_array());
        assert_eq!(params["paths"].as_array().unwrap().len(), 2);
        assert_eq!(
            params["start_line"],
            serde_json::Value::String("10".to_string())
        );
    }

    #[test]
    fn test_parse_xml_tool_call_scalar_paths() {
        let xml = r#"<minimax:tool_call><invoke name="read_file"><parameter name="paths">tetris.c</parameter></invoke></tool_call>"#;
        let (name, params) = parse_minimax_xml_tool_call(xml).unwrap();
        assert_eq!(name, "read_file");
        assert_eq!(
            params["paths"],
            serde_json::Value::String("tetris.c".to_string())
        );
    }

    #[test]
    fn test_sse_parse_minimax_delta_with_name_and_role_fields() {
        let line = r#"{"id":"06606d9933ccdc19dddfa3af953a03d3","choices":[{"index":0,"delta":{"role":"assistant","name":"MiniMax AI","tool_calls":[{"function":{"arguments":"|event::\"}"},"index":0}]}}],"created":1779514009,"model":"MiniMax-M2.7","object":"chat.completion.chunk","usage":{"total_tokens":0,"total_characters":0},"input_sensitive":false,"output_sensitive":false,"input_sensitive_type":0,"output_sensitive_type":0,"output_sensitive_int":0}"#;
        let result = serde_json::from_str::<OpenAIStreamResponse>(line);
        match &result {
            Ok(resp) => {
                assert_eq!(resp.choices.len(), 1);
                let tc = &resp.choices[0].delta.tool_calls;
                assert_eq!(tc.len(), 1);
                assert_eq!(tc[0].index, 0);
                assert!(tc[0].function.is_some());
                assert_eq!(
                    tc[0].function.as_ref().unwrap().arguments.as_deref(),
                    Some("|event::\"}")
                );
            }
            Err(e) => panic!("SSE line with delta.name/role should parse: {}", e),
        }
    }

    #[test]
    fn test_sse_parse_minimax_tool_call_with_type_field() {
        let line = r#"{"id":"06606d99","choices":[{"finish_reason":"tool_calls","index":0,"delta":{"role":"assistant","name":"MiniMax AI","tool_calls":[{"id":"call_function_jpaq2z2bgfh7_2","type":"function","function":{"name":"read_file","arguments":"{\"paths\": [\"src/providers/minimax.rs\"]}"},"index":1}]}}],"created":1779514009,"model":"MiniMax-M2.7","object":"chat.completion.chunk"}"#;
        let result = serde_json::from_str::<OpenAIStreamResponse>(line);
        match &result {
            Ok(resp) => {
                assert_eq!(resp.choices.len(), 1);
                let tc = &resp.choices[0].delta.tool_calls;
                assert_eq!(tc.len(), 1);
                assert_eq!(tc[0].index, 1);
            }
            Err(e) => panic!("SSE line with tool_call.type should parse: {}", e),
        }
    }

    #[tokio::test]
    async fn test_finish_reason_singular_tool_call_emits_tool_call() {
        use tokio::sync::mpsc;
        let (tx, mut rx) = mpsc::channel(8);

        // Use the actual process_minimax_sse_line function so we exercise the
        // exact condition path that was broken. The model streams a complete
        // tool call, then signals completion with the singular finish_reason
        // "tool_call" (undocumented but observed in production logs).
        let mut accumulated = std::collections::HashMap::new();
        let mut completed = std::collections::HashSet::new();
        let mut last_stop_reason = None;
        let mut xml_buffer = String::new();
        let mut pending_text = String::new();
        let mut pending_text_signature = None;
        let mut reasoning_state = MinimaxReasoningState::default();

        // Chunk 1: tool call id + name + first args fragment
        process_minimax_sse_line(
            r#"data: {"id":"evt1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"pa"}}]}}],"model":"MiniMax-M2.7"}"#,
            &tx,
            &mut accumulated,
            &mut completed,
            &mut last_stop_reason,
            &mut xml_buffer,
            &mut pending_text,
            &mut pending_text_signature,
            &mut reasoning_state,
        )
        .await;

        // Chunk 2: remaining args + finish_reason: "tool_call" (singular)
        process_minimax_sse_line(
            r#"data: {"id":"evt2","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ths\": [\"src/main.rs\"]}"}}]},"finish_reason":"tool_call"}],"model":"MiniMax-M2.7"}"#,
            &tx,
            &mut accumulated,
            &mut completed,
            &mut last_stop_reason,
            &mut xml_buffer,
            &mut pending_text,
            &mut pending_text_signature,
            &mut reasoning_state,
        )
        .await;

        drop(tx);

        // Collect all emitted chunks
        let mut tool_call_chunks = Vec::new();
        while let Some(chunk) = rx.recv().await {
            if let ApiStreamChunk::ToolCalls(tc) = chunk {
                tool_call_chunks.push(tc);
            }
        }

        assert_eq!(
            tool_call_chunks.len(),
            1,
            "singular finish_reason \"tool_call\" must trigger tool call emission at finish_reason"
        );
        let tc = &tool_call_chunks[0].tool_call;
        assert_eq!(tc.call_id.as_deref(), Some("call_1"));
        assert_eq!(tc.function.name.as_deref(), Some("read_file"));
        let args = tc.function.arguments.as_deref().unwrap_or("{}");
        let parsed: serde_json::Value =
            serde_json::from_str(args).expect("arguments must be valid JSON");
        assert_eq!(
            parsed["paths"][0], "src/main.rs",
            "complete args must arrive (not truncated by late stream-end flush)"
        );
    }
}
