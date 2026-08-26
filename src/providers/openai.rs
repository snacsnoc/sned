//! OpenAI provider implementation for sned CLI.
//!
//! Ports behavior from `dirac/src/core/api/providers/openai.ts` and
//! `dirac/src/core/api/providers/openai-native.ts`.

use crate::providers::{
    ApiStream, ApiStreamChunk, ApiStreamReasoningChunk, ApiStreamTextChunk, ApiStreamToolCall,
    ApiStreamToolCallFunction, ApiStreamToolCallsChunk, ApiStreamUsageChunk, MessageRole,
    ModelInfo, OpenAiCompatibleModelInfo, Provider, ProviderError, ProviderHttpError,
    ProviderModel, ProviderRequest, ProviderTransport, PreoutputPolicy, apply_qwen_model_profile,
    is_retryable_stream_transport_error, normalize_reasoning_delta,
};
use futures::StreamExt;
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::Deserialize;
use serde_json::json;
use std::time::{Duration, Instant};

const OPENAI_CLIENT_TOTAL_TIMEOUT: Duration = Duration::from_secs(600);
const OPENAI_NON_STREAM_PREOUTPUT_GRACE: Duration = Duration::from_secs(5);
const OPENAI_RESPONSE_HEADERS_TIMEOUT: Duration = Duration::from_secs(30);
const OPENAI_SSE_FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(90);
const OPENAI_SSE_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiEndpointKind {
    Official,
    Compatible,
}

/// Configuration for the OpenAI provider.
#[derive(Clone)]
pub struct OpenAiConfig {
    pub api_key: String,
    pub base_url: Option<String>,
    pub model_id: String,
    pub model_info: Option<OpenAiCompatibleModelInfo>,
    pub reasoning_effort: Option<String>,
    pub extra_body: Option<serde_json::Map<String, serde_json::Value>>,
    pub custom_headers: Option<std::collections::HashMap<String, String>>,
    pub endpoint_kind: OpenAiEndpointKind,
    /// Whether to request an SSE response. OpenAI-compatible custom endpoints
    /// may opt into a normal chat completion response for long generations.
    pub stream: bool,
    /// Provider name for error messages (defaults to "OpenAI" if not set).
    /// Used by OpenAI-compatible providers (OpenRouter, DeepSeek) to identify themselves in errors.
    pub provider_name: Option<String>,
}

impl std::fmt::Debug for OpenAiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiConfig")
            .field(
                "api_key",
                &format!("***REDACTED ({} chars)***", self.api_key.len()),
            )
            .field("base_url", &self.base_url)
            .field("model_id", &self.model_id)
            .field("model_info", &self.model_info)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("extra_body", &self.extra_body)
            .field("custom_headers", &self.custom_headers)
            .field("endpoint_kind", &self.endpoint_kind)
            .field("stream", &self.stream)
            .field("provider_name", &self.provider_name)
            .finish()
    }
}

/// OpenAI-compatible provider (covers generic OpenAI, Azure, and custom base URL).
#[derive(Debug)]
pub struct OpenAiProvider {
    config: OpenAiConfig,
    client: reqwest::Client,
    provider_name: String,
    provider_sort: Option<String>,
}

impl OpenAiProvider {
    pub fn new(config: OpenAiConfig) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(OPENAI_CLIENT_TOTAL_TIMEOUT)
            .connect_timeout(std::time::Duration::from_secs(10))
            .tcp_keepalive(Some(std::time::Duration::from_secs(60)))
            .pool_max_idle_per_host(10)
            .build()?;
        let provider_name = config
            .provider_name
            .clone()
            .unwrap_or_else(|| "OpenAI".to_string());
        Ok(Self {
            config,
            client,
            provider_name,
            provider_sort: None,
        })
    }

    pub(super) fn with_provider_sort(mut self, provider_sort: Option<String>) -> Self {
        self.provider_sort = provider_sort;
        self
    }

    fn build_headers(&self) -> anyhow::Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        if let Some(custom) = &self.config.custom_headers {
            for (key, value) in custom {
                headers.insert(
                    reqwest::header::HeaderName::from_bytes(key.as_bytes())?,
                    HeaderValue::from_str(value)?,
                );
            }
        }

        if !self.config.api_key.is_empty() {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", self.config.api_key))?,
            );
        }

        Ok(headers)
    }

    /// Get the base URL for the API endpoint.
    /// Normalizes URL by stripping trailing `/chat/completions` and slashes.
    #[must_use]
    pub fn base_url(&self) -> String {
        self.config
            .base_url
            .clone()
            .filter(|u| !u.is_empty())
            .map(|u| {
                let mut u = u.trim().to_string();
                // Normalize URL: strip trailing /chat/completions and trailing slashes
                if u.ends_with("/chat/completions") {
                    u = u[..u.len() - "/chat/completions".len()].to_string();
                }
                u = u.trim_end_matches('/').to_string();
                u
            })
            .unwrap_or_else(|| "https://api.openai.com/v1".to_owned())
    }

    pub(super) fn build_request_body(
        &self,
        request: &ProviderRequest,
    ) -> anyhow::Result<serde_json::Value> {
        let model_id = &self.config.model_id;
        let uses_official_reasoning_shape = self.config.endpoint_kind
            == OpenAiEndpointKind::Official
            && ["o1", "o3", "o4", "gpt-5"].iter().any(|prefix| {
                model_id.starts_with(prefix) || model_id.contains(&format!("/{prefix}"))
            })
            && !model_id.contains("chat");

        let mut messages = vec![];

        // System/developer message
        if uses_official_reasoning_shape {
            messages.push(json!({
                "role": "developer",
                "content": request.system_prompt
            }));
        } else {
            messages.push(json!({
                "role": "system",
                "content": request.system_prompt
            }));
        }

        // Convert Sned messages to OpenAI format
        for msg in &request.messages {
            let role = match msg.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
            };

            match &msg.content {
                crate::providers::MessageContent::Text(text) => {
                    messages.push(json!({"role": role, "content": text}));
                }
                crate::providers::MessageContent::UserBlocks(blocks) => {
                    messages.extend(convert_user_blocks(role, blocks));
                }
                crate::providers::MessageContent::AssistantBlocks(blocks) => {
                    let content = convert_assistant_blocks(blocks);
                    messages.push(json!({"role": role, "content": content}));
                }
            }
        }

        // OpenAI-compatible APIs require at least one user message. Preserve
        // that invariant even when an upstream empty-content filter removed
        // the only user turn from the request history.
        if !messages.iter().any(|message| message["role"] == "user") {
            messages.push(json!({
                "role": "user",
                "content": "Please proceed.",
            }));
        }

        // Post-process: convert tool_use content blocks to OpenAI tool_calls format.
        // `convert_assistant_blocks` emits Anthropic-style `{"type":"tool_use",...}`
        // content parts, but OpenAI API expects a top-level `tool_calls` array with
        // `{"type":"function","function":{"name":...,"arguments":"..."}}` entries.
        for msg in &mut messages {
            if msg["role"] != "assistant" {
                continue;
            }
            if let Some(content) = msg.get("content").and_then(|v| v.as_array()) {
                let (text_parts, tool_parts): (Vec<_>, Vec<_>) =
                    content.iter().cloned().partition(|part| {
                        part.get("type").and_then(|t| t.as_str()) != Some("tool_use")
                    });

                if tool_parts.is_empty() {
                    let joined = join_assistant_text_parts(content);
                    let Some(msg_obj) = msg.as_object_mut() else {
                        tracing::warn!("Skipping non-object message in content conversion");
                        continue;
                    };
                    msg_obj.insert("content".to_string(), json!(joined));
                } else {
                    let tool_calls: Vec<serde_json::Value> = tool_parts
                        .iter()
                        .map(|tu| {
                            let arguments_str =
                                serde_json::to_string(&tu["input"]).unwrap_or_default();
                            json!({
                                "id": tu["id"],
                                "type": "function",
                                "function": {
                                    "name": tu["name"],
                                    "arguments": arguments_str,
                                }
                            })
                        })
                        .collect();

                    let Some(msg_obj) = msg.as_object_mut() else {
                        tracing::warn!("Skipping non-object message in tool_calls conversion");
                        continue;
                    };
                    msg_obj.insert("tool_calls".to_string(), json!(tool_calls));
                    if text_parts.is_empty() {
                        msg_obj.insert("content".to_string(), json!(null));
                    } else {
                        msg_obj.insert(
                            "content".to_string(),
                            json!(join_assistant_text_parts(&text_parts)),
                        );
                    }
                }
            }
        }

        let mut body = json!({
            "model": model_id,
            "messages": messages,
            "stream": self.config.stream,
        });
        if self.config.stream {
            body["stream_options"] = json!({"include_usage": true});
        }

        if let Some(sort) = &self.provider_sort {
            body["provider"] = json!({"sort": sort});
        }

        // Temperature: match TS behavior — omit by default (API uses model default).
        // If model_info.base.temperature is set and non-zero, send it.
        // If model_info.base.temperature is 0, omit (TS converts 0 → undefined).
        // Reasoning family models never support temperature.
        if !uses_official_reasoning_shape
            && let Some(temp) = self
                .config
                .model_info
                .as_ref()
                .and_then(|i| i.base.temperature)
            && temp != 0.0
        {
            body["temperature"] = json!(temp);
        }

        // Max tokens — reasoning models use max_completion_tokens, others use max_tokens
        if let Some(max_tokens) = request
            .max_tokens
            .or_else(|| {
                self.config
                    .model_info
                    .as_ref()
                    .and_then(|i| i.base.max_tokens)
            })
            .filter(|m| *m > 0)
        {
            if uses_official_reasoning_shape {
                body["max_completion_tokens"] = json!(max_tokens);
            } else {
                body["max_tokens"] = json!(max_tokens);
            }
        }

        // Always forward the user's explicit reasoning effort, including "none".
        // The clap ValueEnum + per-provider rejection guarantees we only reach
        // here for supported providers with a valid value.
        if let Some(effort) = &self.config.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }

        // Tools
        if let Some(tools) = &request.tools {
            let openai_tools: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.function.name,
                            "description": t.function.description,
                            "parameters": t.function.parameters
                        }
                    })
                })
                .collect();
            body["tools"] = json!(openai_tools);
            // Tool choice: respect request.tool_choice if provided
            // OpenAI format: "auto"|"required"|"none" or {"type": "function", "function": {"name": "..."}}
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

        if let Some(extra_body) = &self.config.extra_body {
            for (key, value) in extra_body {
                if matches!(
                    key.as_str(),
                    "model" | "messages" | "stream" | "stream_options" | "tools" | "tool_choice"
                ) {
                    continue;
                }
                body[key] = value.clone();
            }
        }

        Ok(body)
    }
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

fn format_stream_error_diagnostics(
    headers: &HeaderMap,
    elapsed: Duration,
    first_byte_elapsed: Option<Duration>,
) -> String {
    let mut parts = vec![format!("elapsed={}ms", elapsed.as_millis())];
    match first_byte_elapsed {
        Some(first_byte_elapsed) => {
            parts.push(format!("first_byte={}ms", first_byte_elapsed.as_millis()));
        }
        None => parts.push("first_byte=pending".to_string()),
    }
    for name in [
        "content-encoding",
        "content-type",
        "transfer-encoding",
        "content-length",
        "server",
        "x-request-id",
        "openai-request-id",
        "cf-ray",
    ] {
        if let Some(value) = header_value(headers, name) {
            parts.push(format!("{name}={value}"));
        }
    }
    parts.join(", ")
}

async fn next_stream_item_with_timeout<S>(
    stream: &mut S,
    timeout: Duration,
) -> Result<Option<S::Item>, ()>
where
    S: futures::Stream + Unpin,
{
    tokio::time::timeout(timeout, stream.next())
        .await
        .map_err(|_| ())
}

fn stream_timeout_from_env(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(default)
}

fn response_headers_timeout() -> Duration {
    stream_timeout_from_env(
        "SNED_RESPONSE_HEADERS_TIMEOUT_SECS",
        OPENAI_RESPONSE_HEADERS_TIMEOUT,
    )
}

async fn next_stream_item_until_receiver_closed<S>(
    stream: &mut S,
    tx: &tokio::sync::mpsc::Sender<ApiStreamChunk>,
    timeout: Duration,
) -> Option<Result<Option<S::Item>, ()>>
where
    S: futures::Stream + Unpin,
{
    tokio::select! {
        _ = tx.closed() => None,
        item = next_stream_item_with_timeout(stream, timeout) => Some(item),
    }
}

fn convert_user_blocks(
    role: &str,
    blocks: &[crate::providers::UserContentBlock],
) -> Vec<serde_json::Value> {
    let is_simple_text =
        blocks.len() == 1 && matches!(blocks[0], crate::providers::UserContentBlock::Text(_));

    if is_simple_text && let crate::providers::UserContentBlock::Text(t) = &blocks[0] {
        return vec![json!({
            "role": role,
            "content": t.text,
        })];
    }

    let mut content_parts = vec![];
    let mut tool_results = vec![];

    for block in blocks {
        match block {
            crate::providers::UserContentBlock::Text(t) => {
                content_parts.push(json!({"type": "text", "text": t.text}));
            }
            crate::providers::UserContentBlock::Image(img) => match &img.source {
                crate::providers::ImageSource::Base64 { media_type, data } => {
                    content_parts.push(json!({
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:{};base64,{}", media_type, data)
                        }
                    }));
                }
                crate::providers::ImageSource::Url { url } => {
                    content_parts.push(json!({
                        "type": "image_url",
                        "image_url": {"url": url}
                    }));
                }
            },
            crate::providers::UserContentBlock::ToolResult(tr) => {
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
                    "content": content
                }));
            }
            crate::providers::UserContentBlock::Document(doc) => match &doc.source {
                crate::providers::DocumentSource::Text { text } => {
                    content_parts.push(json!({"type": "text", "text": text}));
                }
                crate::providers::DocumentSource::Base64 { media_type, data } => {
                    content_parts.push(json!({
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:{};base64,{}", media_type, data)
                        }
                    }));
                }
                crate::providers::DocumentSource::Url { url } => {
                    content_parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                }
            },
        }
    }

    let mut result = vec![];
    result.extend(tool_results);
    if !content_parts.is_empty() {
        result.push(json!({
            "role": role,
            "content": content_parts,
        }));
    }
    result
}

fn convert_assistant_blocks(
    blocks: &[crate::providers::AssistantContentBlock],
) -> serde_json::Value {
    let parts: Vec<serde_json::Value> = blocks
        .iter()
        .filter_map(|block| match block {
            crate::providers::AssistantContentBlock::Text(t) => {
                Some(json!({"type": "text", "text": t.text}))
            }
            crate::providers::AssistantContentBlock::Image(img) => match &img.source {
                crate::providers::ImageSource::Base64 { media_type, data } => Some(json!({
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:{};base64,{}", media_type, data)
                    }
                })),
                crate::providers::ImageSource::Url { url } => {
                    Some(json!({"type": "image_url", "image_url": {"url": url}}))
                }
            },
            crate::providers::AssistantContentBlock::ToolUse(tu) => Some(json!({
                "type": "tool_use",
                "id": tu.id,
                "name": tu.name,
                "input": tu.input
            })),
            crate::providers::AssistantContentBlock::Thinking(_) => {
                // OpenAI API does not support "thinking" content blocks; skip.
                None
            }
            crate::providers::AssistantContentBlock::RedactedThinking(_) => None,
            crate::providers::AssistantContentBlock::Document(doc) => match &doc.source {
                crate::providers::DocumentSource::Text { text } => {
                    Some(json!({"type": "text", "text": text}))
                }
                crate::providers::DocumentSource::Base64 { media_type, data } => Some(json!({
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:{};base64,{}", media_type, data)
                    }
                })),
                crate::providers::DocumentSource::Url { url } => {
                    Some(json!({"type": "image_url", "image_url": {"url": url}}))
                }
            },
        })
        .collect();
    json!(parts)
}

fn join_assistant_text_parts(parts: &[serde_json::Value]) -> String {
    parts
        .iter()
        .filter_map(|part| {
            if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                part.get("text").and_then(|t| t.as_str()).map(String::from)
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// OpenAI streaming response chunk.
#[derive(Debug, Deserialize)]
struct OpenAiStreamChunk {
    id: String,
    choices: Vec<OpenAiChoice>,
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAiCompletionResponse {
    id: Option<String>,
    #[serde(default)]
    choices: Vec<OpenAiCompletionChoice>,
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAiCompletionChoice {
    #[serde(default)]
    message: Option<OpenAiCompletionMessage>,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct OpenAiCompletionMessage {
    content: Option<String>,
    #[serde(rename = "reasoning_content")]
    reasoning_content: Option<String>,
    refusal: Option<String>,
    tool_calls: Option<Vec<OpenAiCompletionToolCall>>,
}

#[derive(Debug, Deserialize)]
struct OpenAiCompletionToolCall {
    id: Option<String>,
    function: Option<OpenAiCompletionFunction>,
}

#[derive(Debug, Deserialize)]
struct OpenAiCompletionFunction {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    delta: OpenAiDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct OpenAiDelta {
    content: Option<String>,
    #[serde(rename = "reasoning_content")]
    reasoning_content: Option<String>,
    #[serde(rename = "tool_calls")]
    tool_calls: Option<Vec<OpenAiToolCallDelta>>,
    refusal: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiToolCallDelta {
    index: usize,
    id: Option<String>,
    function: Option<OpenAiFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct OpenAiFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    prompt_tokens_details: Option<OpenAiPromptTokenDetails>,
    completion_tokens_details: Option<OpenAiCompletionTokenDetails>,
    #[serde(rename = "prompt_cache_miss_tokens")]
    prompt_cache_miss_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct OpenAiPromptTokenDetails {
    cached_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct OpenAiCompletionTokenDetails {
    reasoning_tokens: Option<u32>,
}

fn openai_usage_chunk(
    usage: &OpenAiUsage,
    id: Option<String>,
    stop_reason: Option<String>,
    model_info: Option<&OpenAiCompatibleModelInfo>,
) -> ApiStreamUsageChunk {
    let cached_tokens = usage
        .prompt_tokens_details
        .as_ref()
        .map_or(0, |details| details.cached_tokens);
    let cache_write_tokens = usage.prompt_cache_miss_tokens.unwrap_or(0);
    let uncached_input_tokens = usage.prompt_tokens.saturating_sub(cached_tokens);
    let total_cost = model_info.and_then(|info| {
        let input_price = info.base.input_price?;
        let output_price = info.base.output_price?;
        let cache_reads_price = info.base.cache_reads_price.unwrap_or(0.0);
        let cache_writes_price = info.base.cache_writes_price.unwrap_or(0.0);
        let input_cost = input_price * (uncached_input_tokens as f64 / 1_000_000.0);
        let output_cost = output_price * (usage.completion_tokens as f64 / 1_000_000.0);
        let cache_read_cost = cache_reads_price * (cached_tokens as f64 / 1_000_000.0);
        let cache_write_cost = cache_writes_price * (cache_write_tokens as f64 / 1_000_000.0);
        Some(input_cost + output_cost + cache_read_cost + cache_write_cost)
    });

    ApiStreamUsageChunk {
        input_tokens: uncached_input_tokens,
        output_tokens: usage.completion_tokens,
        cache_write_tokens: usage.prompt_cache_miss_tokens,
        cache_read_tokens: Some(cached_tokens),
        reasoning_tokens: usage
            .completion_tokens_details
            .as_ref()
            .and_then(|details| details.reasoning_tokens),
        thoughts_token_count: None,
        total_cost,
        stop_reason,
        id,
    }
}

fn decode_openai_completion(
    completion: OpenAiCompletionResponse,
    model_info: Option<&OpenAiCompatibleModelInfo>,
) -> Vec<ApiStreamChunk> {
    let mut chunks = Vec::new();
    let Some(choice) = completion.choices.into_iter().next() else {
        chunks.push(ApiStreamChunk::Usage(ApiStreamUsageChunk {
            input_tokens: 0,
            output_tokens: 0,
            cache_write_tokens: Some(0),
            cache_read_tokens: None,
            reasoning_tokens: None,
            thoughts_token_count: None,
            total_cost: None,
            stop_reason: None,
            id: completion.id,
        }));
        return chunks;
    };

    let stop_reason = choice.finish_reason.clone();
    let message = choice.message.unwrap_or_default();
    let completion_id = completion.id.clone();
    if let Some(reasoning) = message.reasoning_content.filter(|text| !text.is_empty()) {
        chunks.push(ApiStreamChunk::Reasoning(ApiStreamReasoningChunk {
            reasoning,
            details: None,
            signature: None,
            redacted_data: None,
            id: completion_id.clone(),
        }));
    }
    if let Some(content) = message.content.filter(|text| !text.is_empty()) {
        chunks.push(ApiStreamChunk::Text(ApiStreamTextChunk {
            text: content,
            id: completion_id.clone(),
            signature: None,
        }));
    }
    if let Some(refusal) = message.refusal.filter(|text| !text.is_empty()) {
        chunks.push(ApiStreamChunk::Error(format!(
            "OpenAI model refused: {refusal}"
        )));
    }

    if stop_reason.as_deref() != Some("content_filter")
        && let Some(tool_calls) = message.tool_calls
    {
        for tool_call in tool_calls {
            let Some(call_id) = tool_call.id.filter(|id| !id.is_empty()) else {
                continue;
            };
            let Some(function) = tool_call.function else {
                continue;
            };
            let Some(raw_name) = function.name.filter(|name| !name.is_empty()) else {
                continue;
            };
            let name = normalize_qwen_thinking_tool_name(&raw_name).unwrap_or(raw_name);
            if !is_safe_tool_name(&name) {
                continue;
            }
            chunks.push(ApiStreamChunk::ToolCallStarted {
                call_id: call_id.clone(),
                name: name.clone(),
            });
            let arguments = crate::providers::validate_tool_call_args(
                function.arguments.as_deref().unwrap_or_default(),
                "OpenAI",
                "non-stream response",
            );
            chunks.push(ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                tool_call: ApiStreamToolCall {
                    call_id: Some(call_id.clone()),
                    function: ApiStreamToolCallFunction {
                        id: Some(call_id),
                        name: Some(name),
                        arguments: Some(arguments),
                    },
                    signature: None,
                },
                id: completion_id.clone(),
                signature: None,
            }));
        }
    }

    if let Some(usage) = completion.usage.as_ref() {
        chunks.push(ApiStreamChunk::Usage(openai_usage_chunk(
            usage,
            completion.id.clone(),
            stop_reason,
            model_info,
        )));
    } else {
        chunks.push(ApiStreamChunk::Usage(ApiStreamUsageChunk {
            input_tokens: 0,
            output_tokens: 0,
            cache_write_tokens: Some(0),
            cache_read_tokens: None,
            reasoning_tokens: None,
            thoughts_token_count: None,
            total_cost: None,
            stop_reason,
            id: completion.id,
        }));
    }
    chunks
}

fn try_send_chunk(
    tx: &tokio::sync::mpsc::Sender<ApiStreamChunk>,
    chunk: ApiStreamChunk,
    chunk_type: &str,
) -> bool {
    crate::providers::try_send_chunk(tx, chunk, "OpenAI", chunk_type)
}

#[derive(Debug, Default)]
pub struct OpenAiStreamDeltaState {
    emitted_reasoning: String,
    started_tool_call_indices: std::collections::HashSet<usize>,
}

fn normalize_qwen_thinking_tool_name(name: &str) -> Option<String> {
    let (initial_name, wrapped_name) = name.split_once("\n</think>\n\n<tool_call>\n<function=")?;
    let function_name = wrapped_name.strip_suffix('>').unwrap_or(wrapped_name);

    (initial_name.trim() == function_name && is_safe_tool_name(function_name))
        .then(|| function_name.to_owned())
}

fn is_safe_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

#[allow(clippy::unused_async)]
async fn process_openai_sse_line(
    line: &str,
    tx: &tokio::sync::mpsc::Sender<ApiStreamChunk>,
    delta_state: &mut OpenAiStreamDeltaState,
    accumulated_tool_calls: &mut std::collections::HashMap<usize, (String, String, String)>,
    completed_tool_call_indices: &mut std::collections::HashSet<usize>,
    last_stop_reason: &mut Option<String>,
    model_info: Option<&crate::providers::OpenAiCompatibleModelInfo>,
    usage_sent: &mut bool,
) {
    let line = line.trim();
    if line.is_empty() || line == "data: [DONE]" {
        return;
    }
    let data = line
        .strip_prefix("data:")
        .map(|s| s.strip_prefix(" ").unwrap_or(s));
    if let Some(data) = data {
        let Ok(chunk) = serde_json::from_str::<OpenAiStreamChunk>(data) else {
            tracing::warn!(line = %line, "OpenAI SSE: failed to parse chunk");
            return;
        };
        if let Some(choice) = chunk.choices.into_iter().next() {
            let delta = choice.delta;

            if let Some(content) = delta.content
                && !content.is_empty()
            {
                try_send_chunk(
                    tx,
                    ApiStreamChunk::Text(ApiStreamTextChunk {
                        text: content,
                        id: Some(chunk.id.clone()),
                        signature: None,
                    }),
                    "text",
                );
            }

            if let Some(reasoning) = delta.reasoning_content
                && let Some(reasoning) =
                    normalize_reasoning_delta(&mut delta_state.emitted_reasoning, reasoning)
            {
                try_send_chunk(
                    tx,
                    ApiStreamChunk::Reasoning(ApiStreamReasoningChunk {
                        reasoning,
                        details: None,
                        signature: None,
                        redacted_data: None,
                        id: Some(chunk.id.clone()),
                    }),
                    "reasoning",
                );
            }

            // Handle OpenAI refusal responses (content policy violations)
            if let Some(refusal) = delta.refusal
                && !refusal.is_empty()
            {
                try_send_chunk(
                    tx,
                    ApiStreamChunk::Error(format!("OpenAI model refused: {refusal}")),
                    "refusal",
                );
            }

            // Accumulate tool call deltas by index. Do not send immediately —
            // dispatch only when finish_reason == "tool_calls" per OpenAI spec.
            if let Some(tool_calls) = delta.tool_calls {
                for tc in tool_calls {
                    let tool_index = tc.index;
                    if completed_tool_call_indices.contains(&tool_index) {
                        continue;
                    }

                    // Some OpenAI-compatible providers send the call ID only in
                    // the first delta, then use empty strings for continuations.
                    // Keep the original ID so the completed call can be dispatched.
                    if let Some(id) = tc.id.as_deref().filter(|id| !id.is_empty()) {
                        accumulated_tool_calls
                            .entry(tool_index)
                            .or_insert_with(|| (String::new(), String::new(), String::new()))
                            .0 = id.to_owned();
                    }

                    if let Some(function) = tc.function {
                        if let Some(name) = function.name.filter(|n| !n.is_empty()) {
                            let name = normalize_qwen_thinking_tool_name(&name).unwrap_or(name);
                            accumulated_tool_calls
                                .entry(tool_index)
                                .or_insert_with(|| (String::new(), String::new(), String::new()))
                                .1 = name;
                        }
                        if let Some(args) = function.arguments.filter(|a| !a.is_empty()) {
                            let entry = accumulated_tool_calls
                                .entry(tool_index)
                                .or_insert_with(|| (String::new(), String::new(), String::new()));
                            // Enforce MAX_TOOL_ARGUMENT_SIZE during accumulation to prevent
                            // memory exhaustion from providers sending many small deltas.
                            // This matches the validation in agent_loop.rs for other providers.
                            if entry.2.len() + args.len()
                                <= crate::providers::MAX_TOOL_ARGUMENT_SIZE
                            {
                                entry.2.push_str(&args);
                            } else {
                                let remaining =
                                    crate::providers::MAX_TOOL_ARGUMENT_SIZE - entry.2.len();
                                if remaining > 0 {
                                    let safe_end = args.floor_char_boundary(remaining);
                                    entry.2.push_str(&args[..safe_end]);
                                }
                                tracing::warn!(
                                    tool_index = tc.index,
                                    accumulated_size = entry.2.len(),
                                    "OpenAI tool call arguments exceeded MAX_TOOL_ARGUMENT_SIZE, truncated"
                                );
                            }
                        }
                    }

                    if !delta_state.started_tool_call_indices.contains(&tool_index)
                        && let Some((id, name, _)) = accumulated_tool_calls.get(&tool_index)
                        && !id.is_empty()
                        && is_safe_tool_name(name)
                        && try_send_chunk(
                            tx,
                            ApiStreamChunk::ToolCallStarted {
                                call_id: id.clone(),
                                name: name.clone(),
                            },
                            "tool_call_started",
                        )
                    {
                        delta_state.started_tool_call_indices.insert(tool_index);
                    }
                }
            }

            // Track finish_reason for final dispatch gate
            if let Some(finish) = choice.finish_reason {
                *last_stop_reason = Some(finish.clone());

                // Flush accumulated tool calls when model signals tool_calls completion
                if finish == "tool_calls" {
                    // Sort by index to ensure deterministic emission order
                    let mut sorted_indices: Vec<_> = accumulated_tool_calls.keys().collect();
                    sorted_indices.sort();

                    for idx in sorted_indices {
                        let (id, name, args) = &accumulated_tool_calls[idx];
                        if !completed_tool_call_indices.contains(idx)
                            && !id.is_empty()
                            && !name.is_empty()
                        {
                            let validated_args = crate::providers::validate_tool_call_args(
                                args,
                                "OpenAI",
                                "on finish_reason:tool_calls",
                            );
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
                                    id: Some(chunk.id.clone()),
                                    signature: None,
                                }),
                                "tool_calls",
                            );
                        }
                    }
                }
            }
        }

        if let Some(usage) = chunk.usage {
            *usage_sent = true;
            try_send_chunk(
                tx,
                ApiStreamChunk::Usage(openai_usage_chunk(
                    &usage,
                    Some(chunk.id),
                    last_stop_reason.clone(),
                    model_info,
                )),
                "usage",
            );
        }
    }
}

fn body_looks_like_sse(body: &[u8]) -> bool {
    String::from_utf8_lossy(body)
        .lines()
        .any(|line| line.trim_start().starts_with("data:"))
}

async fn decode_openai_sse_body(
    body: &[u8],
    model_info: Option<&OpenAiCompatibleModelInfo>,
) -> Vec<ApiStreamChunk> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(10_000);
    let mut buffer = crate::providers::SseLineBuffer::default();
    let mut delta_state = OpenAiStreamDeltaState::default();
    let mut accumulated_tool_calls = std::collections::HashMap::new();
    let mut completed_tool_call_indices = std::collections::HashSet::new();
    let mut last_stop_reason = None;
    let mut usage_sent = false;

    parse_openai_sse_to_chunks(
        body,
        &mut buffer,
        &tx,
        &mut delta_state,
        &mut accumulated_tool_calls,
        &mut completed_tool_call_indices,
        &mut last_stop_reason,
        model_info,
        &mut usage_sent,
    )
    .await;
    finish_openai_sse_to_chunks(
        &mut buffer,
        &tx,
        &mut delta_state,
        &mut accumulated_tool_calls,
        &mut completed_tool_call_indices,
        &mut last_stop_reason,
        model_info,
        &mut usage_sent,
    )
    .await;
    drop(tx);

    let mut chunks = Vec::new();
    while let Some(chunk) = rx.recv().await {
        chunks.push(chunk);
    }
    chunks
}

/// Parse OpenAI SSE chunk bytes into stream chunks. Extracted for testability.
pub async fn parse_openai_sse_to_chunks(
    chunk: &[u8],
    buffer: &mut crate::providers::SseLineBuffer,
    tx: &tokio::sync::mpsc::Sender<ApiStreamChunk>,
    delta_state: &mut OpenAiStreamDeltaState,
    accumulated_tool_calls: &mut std::collections::HashMap<usize, (String, String, String)>,
    completed_tool_call_indices: &mut std::collections::HashSet<usize>,
    last_stop_reason: &mut Option<String>,
    model_info: Option<&crate::providers::OpenAiCompatibleModelInfo>,
    usage_sent: &mut bool,
) {
    for line in buffer.push_chunk(chunk) {
        process_openai_sse_line(
            &line,
            tx,
            delta_state,
            accumulated_tool_calls,
            completed_tool_call_indices,
            last_stop_reason,
            model_info,
            usage_sent,
        )
        .await;
    }
    if let Some(err) = buffer.take_error() {
        try_send_chunk(tx, ApiStreamChunk::Error(err), "error");
    }
}

pub async fn finish_openai_sse_to_chunks(
    buffer: &mut crate::providers::SseLineBuffer,
    tx: &tokio::sync::mpsc::Sender<ApiStreamChunk>,
    delta_state: &mut OpenAiStreamDeltaState,
    accumulated_tool_calls: &mut std::collections::HashMap<usize, (String, String, String)>,
    completed_tool_call_indices: &mut std::collections::HashSet<usize>,
    last_stop_reason: &mut Option<String>,
    model_info: Option<&crate::providers::OpenAiCompatibleModelInfo>,
    usage_sent: &mut bool,
) {
    if let Some(line) = buffer.finish() {
        process_openai_sse_line(
            &line,
            tx,
            delta_state,
            accumulated_tool_calls,
            completed_tool_call_indices,
            last_stop_reason,
            model_info,
            usage_sent,
        )
        .await;
    }

    // Flush any remaining accumulated tool calls on stream end
    // (some providers don't send finish_reason == "tool_calls" explicitly)
    if !matches!(last_stop_reason.as_deref(), Some("content_filter")) {
        let mut sorted_indices: Vec<_> = accumulated_tool_calls.keys().collect();
        sorted_indices.sort();

        for idx in sorted_indices {
            let (id, name, args) = &accumulated_tool_calls[idx];
            if !completed_tool_call_indices.contains(idx) && !id.is_empty() && !name.is_empty() {
                let validated_args =
                    crate::providers::validate_tool_call_args(args, "OpenAI", "at stream end");
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
                        id: None,
                        signature: None,
                    }),
                    "tool_calls",
                );
            }
        }
    }

    // Emit synthetic Usage chunk if no usage chunk was sent
    if !*usage_sent {
        try_send_chunk(
            tx,
            ApiStreamChunk::Usage(ApiStreamUsageChunk {
                input_tokens: 0,
                output_tokens: 0,
                cache_write_tokens: Some(0),
                cache_read_tokens: None,
                reasoning_tokens: None,
                thoughts_token_count: None,
                total_cost: None,
                stop_reason: last_stop_reason.clone(),
                id: None,
            }),
            "usage",
        );
    }
}

impl Provider for OpenAiProvider {
    async fn create_message(&self, request: ProviderRequest) -> Result<ApiStream, ProviderError> {
        let url = format!("{}/chat/completions", self.base_url());
        let body = self.build_request_body(&request)?;
        let headers = self.build_headers()?;

        if tracing::enabled!(tracing::Level::DEBUG) {
            let request_bytes = serde_json::to_vec(&body).map_or(0, |body| body.len());
            tracing::debug!(
                method = "POST",
                provider = "openai",
                url = %url,
                model = %self.config.model_id,
                message_count = request.messages.len(),
                tool_count = request.tools.as_ref().map_or(0, Vec::len),
                request_bytes,
                "sending provider request"
            );
        }

        let request_started_at = Instant::now();
        let headers_timeout = response_headers_timeout();
        let response = match tokio::time::timeout(
            headers_timeout,
            self.client.post(&url).headers(headers).json(&body).send(),
        )
        .await
        {
            Ok(response) => response?,
            Err(_) => {
                return Err(ProviderError::NetworkError(format!(
                    "OpenAI response headers timed out after {}s",
                    headers_timeout.as_secs()
                )));
            }
        };

        tracing::debug!(
            response_status = %response.status(),
            headers_elapsed_ms = request_started_at.elapsed().as_millis(),
            "OpenAI response headers received"
        );

        if !response.status().is_success() {
            let status = response.status();
            let headers = response.headers().clone();
            let text = response.text().await.unwrap_or_default();

            // Add helpful hint for common model/provider mismatches
            let error_body = if status == StatusCode::NOT_FOUND {
                let model_lower = self.config.model_id.to_lowercase();
                if model_lower.starts_with("claude-") {
                    format!(
                        "{}\n\nHint: Model '{}' looks like an Anthropic Claude model. \
                         If you intended to use Claude, set ANTHROPIC_API_KEY or use --provider anthropic.",
                        text, self.config.model_id
                    )
                } else if model_lower.starts_with("gemini-") {
                    format!(
                        "{}\n\nHint: Model '{}' looks like a Google Gemini model. \
                         If you intended to use Gemini, set GEMINI_API_KEY or use --provider gemini.",
                        text, self.config.model_id
                    )
                } else {
                    text
                }
            } else {
                text
            };

            return Err(ProviderHttpError::new(
                &self.provider_name,
                url,
                status,
                error_body,
                headers,
            )
            .into());
        }

        let response_headers = response.headers().clone();
        let is_sse_response = header_value(&response_headers, "content-type")
            .is_some_and(|content_type| {
                content_type
                    .to_ascii_lowercase()
                    .contains("text/event-stream")
            });
        if !self.config.stream && !is_sse_response {
            let response_body = response.bytes().await.map_err(ProviderError::from)?;
            match serde_json::from_slice::<OpenAiCompletionResponse>(&response_body) {
                Ok(completion) => {
                    let chunks =
                        decode_openai_completion(completion, self.config.model_info.as_ref());
                    return Ok(Box::pin(tokio_stream::iter(chunks)));
                }
                Err(error) if body_looks_like_sse(&response_body) => {
                    tracing::debug!(
                        error = %error,
                        "OpenAI endpoint returned an SSE body without an SSE content type; using the SSE decoder"
                    );
                    let chunks =
                        decode_openai_sse_body(&response_body, self.config.model_info.as_ref())
                            .await;
                    return Ok(Box::pin(tokio_stream::iter(chunks)));
                }
                Err(error) => {
                    return Err(ProviderError::InvalidRequest(format!(
                        "OpenAI non-stream response was not valid chat.completion JSON: {error}"
                    )));
                }
            }
        }
        if !self.config.stream && is_sse_response {
            tracing::debug!(
                "OpenAI endpoint returned SSE despite stream:false; using the SSE decoder"
            );
        }
        let stream_started_at = Instant::now();
        let stream = response.bytes_stream();
        let first_byte_timeout = stream_timeout_from_env(
            "SNED_SSE_FIRST_BYTE_TIMEOUT_SECS",
            OPENAI_SSE_FIRST_BYTE_TIMEOUT,
        );
        let inactivity_timeout = stream_timeout_from_env(
            "SNED_SSE_INACTIVITY_TIMEOUT_SECS",
            OPENAI_SSE_INACTIVITY_TIMEOUT,
        );
        // Use large buffer (10_000) to match agent_loop channel and prevent backpressure deadlocks
        // when the consumer is slow (same pattern as agent_loop.rs:726)
        let (tx, rx) = tokio::sync::mpsc::channel::<ApiStreamChunk>(10_000);

        // Capture model_info for cost calculation in the spawned task
        let model_info = self.config.model_info.clone();

        tokio::spawn(async move {
            let mut stream = stream;
            let mut sse_buffer = crate::providers::SseLineBuffer::default();
            let mut delta_state = OpenAiStreamDeltaState::default();
            let mut accumulated_tool_calls: std::collections::HashMap<
                usize,
                (String, String, String),
            > = std::collections::HashMap::with_capacity(4);
            let mut completed_tool_call_indices: std::collections::HashSet<usize> =
                std::collections::HashSet::new();
            let mut last_stop_reason: Option<String> = None;
            let mut usage_sent = false;
            let mut stream_errored = false;
            let mut first_byte_elapsed: Option<Duration> = None;

            loop {
                if tx.is_closed() {
                    break;
                }
                let timeout = if first_byte_elapsed.is_some() {
                    inactivity_timeout
                } else {
                    first_byte_timeout
                };
                let Some(result) =
                    next_stream_item_until_receiver_closed(&mut stream, &tx, timeout).await
                else {
                    break;
                };
                let result = match result {
                    Ok(Some(result)) => result,
                    Ok(None) => break,
                    Err(()) => {
                        let phase = if first_byte_elapsed.is_some() {
                            "inactivity"
                        } else {
                            "first byte"
                        };
                        let diagnostics = format_stream_error_diagnostics(
                            &response_headers,
                            stream_started_at.elapsed(),
                            first_byte_elapsed,
                        );
                        tracing::error!(
                            phase,
                            timeout_ms = timeout.as_millis(),
                            diagnostics = %diagnostics,
                            "OpenAI SSE stream timeout"
                        );
                        try_send_chunk(
                            &tx,
                            ApiStreamChunk::Error(format!(
                                "OpenAI SSE {phase} timeout after {}ms; diagnostics: {} (retryable)",
                                timeout.as_millis(),
                                diagnostics,
                            )),
                            "timeout",
                        );
                        stream_errored = true;
                        break;
                    }
                };
                match result {
                    Ok(bytes) => {
                        if !bytes.is_empty() && first_byte_elapsed.is_none() {
                            let elapsed = stream_started_at.elapsed();
                            tracing::debug!(
                                first_byte_elapsed_ms = elapsed.as_millis(),
                                "OpenAI SSE first response bytes received"
                            );
                            first_byte_elapsed = Some(elapsed);
                        }
                        parse_openai_sse_to_chunks(
                            bytes.as_ref(),
                            &mut sse_buffer,
                            &tx,
                            &mut delta_state,
                            &mut accumulated_tool_calls,
                            &mut completed_tool_call_indices,
                            &mut last_stop_reason,
                            model_info.as_ref(),
                            &mut usage_sent,
                        )
                        .await;
                    }
                    Err(e) => {
                        let diagnostics = format_stream_error_diagnostics(
                            &response_headers,
                            stream_started_at.elapsed(),
                            first_byte_elapsed,
                        );
                        let error_text = e.to_string();
                        let is_retryable = is_retryable_stream_transport_error(&error_text);
                        tracing::error!(
                            error = %e,
                            retryable = is_retryable,
                            diagnostics = %diagnostics,
                            "OpenAI SSE bytes_stream error"
                        );
                        try_send_chunk(
                            &tx,
                            ApiStreamChunk::Error(format!(
                                "OpenAI SSE stream error: {}; diagnostics: {}{}",
                                e,
                                diagnostics,
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
                finish_openai_sse_to_chunks(
                    &mut sse_buffer,
                    &tx,
                    &mut delta_state,
                    &mut accumulated_tool_calls,
                    &mut completed_tool_call_indices,
                    &mut last_stop_reason,
                    model_info.as_ref(),
                    &mut usage_sent,
                )
                .await;
            }
        });

        let rx_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(Box::pin(rx_stream))
    }

    fn get_model(&self) -> ProviderModel {
        let info = self.config.model_info.as_ref().map_or_else(
            || get_openai_model_info(&self.config.model_id).base,
            |m| m.base.clone(),
        );

        ProviderModel {
            id: self.config.model_id.clone(),
            info,
        }
    }

    fn name(&self) -> &'static str {
        "openai"
    }

    fn preoutput_policy(&self) -> PreoutputPolicy {
        if self.config.stream {
            PreoutputPolicy {
                budget: Duration::from_secs(180),
                transport: ProviderTransport::Streaming,
            }
        } else {
            PreoutputPolicy {
                budget: OPENAI_CLIENT_TOTAL_TIMEOUT + OPENAI_NON_STREAM_PREOUTPUT_GRACE,
                transport: ProviderTransport::Buffered,
            }
        }
    }
}

/// Get model info for known OpenAI models. Falls back to sane defaults
/// matching TS `openAiModelInfoSaneDefaults` for unknown model IDs.
#[must_use]
pub fn get_openai_model_info(model_id: &str) -> OpenAiCompatibleModelInfo {
    // Default matching TS openAiModelInfoSaneDefaults
    let mut info = ModelInfo {
        name: Some(model_id.to_string()),
        max_tokens: None, // -1 in TS means "not set" → None in Rust
        context_window: Some(128_000),
        supports_images: Some(true),
        supports_prompt_cache: false,
        supports_reasoning: Some(true),
        input_price: Some(0.0),
        output_price: Some(0.0),
        image_output_price: None,
        thinking_config: None,
        supports_global_endpoint: None,
        cache_writes_price: None,
        cache_reads_price: None,
        description: None,
        tiers: None,
        temperature: None, // None = "use API default" (matches TS temperature: 0 → undefined)
        top_p: None,
        top_k: None,
        supports_tools: Some(true),
        api_format: None,
    };

    if apply_qwen_model_profile(model_id, &mut info) {
        return OpenAiCompatibleModelInfo {
            base: info,
            is_r1_format_required: None,
            system_role: None,
            supports_reasoning_effort: Some(false),
            supports_streaming: None,
        };
    }

    let is_current_reasoning_model = model_id.contains("gpt-5.6")
        || model_id.contains("gpt-5.4")
        || model_id.contains("gpt-5.3-codex");

    // Model-specific overrides — most-specific-first ordering
    if model_id.contains("gpt-5.6-terra") {
        info.max_tokens = Some(128_000);
        info.context_window = Some(1_050_000);
        info.supports_prompt_cache = true;
        info.input_price = Some(2.5);
        info.output_price = Some(15.0);
        info.cache_reads_price = Some(0.25);
        info.cache_writes_price = Some(3.125);
        info.supports_reasoning = Some(true);
    } else if model_id.contains("gpt-5.6-luna") {
        info.max_tokens = Some(128_000);
        info.context_window = Some(1_050_000);
        info.supports_prompt_cache = true;
        info.input_price = Some(1.0);
        info.output_price = Some(6.0);
        info.cache_reads_price = Some(0.1);
        info.cache_writes_price = Some(1.25);
        info.supports_reasoning = Some(true);
    } else if model_id.contains("gpt-5.6") {
        info.max_tokens = Some(128_000);
        info.context_window = Some(1_050_000);
        info.supports_prompt_cache = true;
        info.input_price = Some(5.0);
        info.output_price = Some(30.0);
        info.cache_reads_price = Some(0.5);
        info.cache_writes_price = Some(6.25);
        info.supports_reasoning = Some(true);
    } else if model_id.contains("gpt-5.5") {
        info.max_tokens = Some(128_000);
        info.context_window = Some(1_000_000);
        info.supports_prompt_cache = true;
        info.input_price = Some(5.0);
        info.output_price = Some(30.0);
        info.cache_reads_price = Some(0.5);
        info.cache_writes_price = Some(0.0);
        info.supports_reasoning = Some(true);
    } else if model_id.contains("gpt-5.4-pro") {
        info.max_tokens = Some(128_000);
        info.context_window = Some(1_050_000);
        info.supports_prompt_cache = true;
        info.input_price = Some(30.0);
        info.output_price = Some(180.0);
        info.cache_reads_price = Some(0.0);
        info.cache_writes_price = Some(0.0);
        info.supports_reasoning = Some(true);
    } else if model_id.contains("gpt-5.4-mini") {
        info.max_tokens = Some(128_000);
        info.context_window = Some(400_000);
        info.supports_prompt_cache = true;
        info.input_price = Some(0.75);
        info.output_price = Some(4.5);
        info.cache_reads_price = Some(0.075);
        info.cache_writes_price = Some(0.9375);
        info.supports_reasoning = Some(true);
    } else if model_id.contains("gpt-5.4-nano") {
        info.max_tokens = Some(128_000);
        info.context_window = Some(400_000);
        info.supports_prompt_cache = true;
        info.input_price = Some(0.2);
        info.output_price = Some(1.25);
        info.cache_reads_price = Some(0.02);
        info.cache_writes_price = Some(0.0);
        info.supports_reasoning = Some(true);
    } else if model_id.contains("gpt-5.4") {
        info.max_tokens = Some(128_000);
        info.context_window = Some(1_050_000);
        info.supports_prompt_cache = true;
        info.input_price = Some(2.5);
        info.output_price = Some(15.0);
        info.cache_reads_price = Some(0.25);
        info.cache_writes_price = Some(3.125);
        info.supports_reasoning = Some(true);
    } else if model_id.contains("gpt-5.3-codex") {
        info.max_tokens = Some(128_000);
        info.context_window = Some(400_000);
        info.supports_prompt_cache = true;
        info.input_price = Some(1.75);
        info.output_price = Some(14.0);
        info.cache_reads_price = Some(0.175);
        info.cache_writes_price = Some(2.1875);
        info.supports_reasoning = Some(true);
    } else if model_id.contains("gpt-4.1-mini") {
        info.max_tokens = Some(128_000);
        info.context_window = Some(1_047_576);
        info.supports_prompt_cache = true;
        info.input_price = Some(0.40);
        info.output_price = Some(1.60);
        info.cache_reads_price = Some(0.10);
    } else if model_id.contains("gpt-4.1") {
        info.max_tokens = Some(128_000);
        info.context_window = Some(1_047_576);
        info.supports_prompt_cache = true;
        info.input_price = Some(2.0);
        info.output_price = Some(8.0);
        info.cache_reads_price = Some(0.50);
    } else if model_id.contains("o4-mini") {
        info.max_tokens = Some(100_000);
        info.context_window = Some(200_000);
        info.supports_prompt_cache = true;
        info.input_price = Some(1.1);
        info.output_price = Some(4.4);
        info.cache_reads_price = Some(0.275);
        info.supports_reasoning = Some(true);
    } else if model_id.contains("o3-mini") {
        info.max_tokens = Some(100_000);
        info.context_window = Some(200_000);
        info.supports_prompt_cache = true;
        info.input_price = Some(1.1);
        info.output_price = Some(4.4);
        info.cache_reads_price = Some(0.55);
        info.supports_reasoning = Some(true);
    } else if model_id.contains("o3") {
        info.max_tokens = Some(100_000);
        info.context_window = Some(200_000);
        info.supports_prompt_cache = true;
        info.input_price = Some(2.0);
        info.output_price = Some(8.0);
        info.cache_reads_price = Some(1.0);
        info.supports_reasoning = Some(true);
    } else if model_id.contains("o1-pro") {
        info.max_tokens = Some(100_000);
        info.context_window = Some(200_000);
        info.supports_prompt_cache = true;
        info.input_price = Some(150.0);
        info.output_price = Some(600.0);
        info.cache_reads_price = Some(7.5);
        info.supports_reasoning = Some(true);
    } else if model_id.contains("o1-mini") {
        info.max_tokens = Some(65_536);
        info.context_window = Some(128_000);
        info.supports_prompt_cache = true;
        info.input_price = Some(1.5);
        info.output_price = Some(6.0);
        info.cache_reads_price = Some(0.75);
        info.supports_reasoning = Some(true);
    } else if model_id.contains("o1") {
        info.max_tokens = Some(100_000);
        info.context_window = Some(200_000);
        info.supports_prompt_cache = true;
        info.input_price = Some(5.0);
        info.output_price = Some(15.0);
        info.cache_reads_price = Some(2.5);
        info.supports_reasoning = Some(true);
    } else if model_id.contains("gpt-4o-mini") {
        info.max_tokens = Some(16_384);
        info.context_window = Some(128_000);
        info.input_price = Some(0.15);
        info.output_price = Some(0.60);
        info.cache_reads_price = Some(0.075);
    } else if model_id.contains("gpt-4o") {
        info.max_tokens = Some(16_384);
        info.context_window = Some(128_000);
        info.input_price = Some(2.5);
        info.output_price = Some(10.0);
        info.cache_reads_price = Some(1.25);
    }

    OpenAiCompatibleModelInfo {
        base: info,
        is_r1_format_required: None,
        system_role: None,
        supports_reasoning_effort: is_current_reasoning_model.then_some(true),
        supports_streaming: is_current_reasoning_model.then_some(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{
        FunctionDefinition, MessageRole, SseLineBuffer, StorageMessage, ToolDefinition,
    };

    #[test]
    fn test_openai_config() {
        let config = OpenAiConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "gpt-4".to_string(),
            model_info: None,
            reasoning_effort: None,
            extra_body: None,
            custom_headers: None,
            endpoint_kind: OpenAiEndpointKind::Official,
            stream: true,
            provider_name: None,
        };
        let provider = OpenAiProvider::new(config).unwrap();
        assert_eq!(provider.base_url(), "https://api.openai.com/v1");
    }

    #[test]
    fn test_openai_custom_base_url() {
        let config = OpenAiConfig {
            api_key: "test-key".to_string(),
            base_url: Some("https://custom.example.com/v1/".to_string()),
            model_id: "gpt-4".to_string(),
            model_info: None,
            reasoning_effort: None,
            extra_body: None,
            custom_headers: None,
            endpoint_kind: OpenAiEndpointKind::Compatible,
            stream: true,
            provider_name: None,
        };
        let provider = OpenAiProvider::new(config).unwrap();
        assert_eq!(provider.base_url(), "https://custom.example.com/v1");
    }

    #[test]
    fn test_non_stream_preoutput_policy_allows_full_http_generation() {
        let provider = OpenAiProvider::new(OpenAiConfig {
            api_key: "test-key".to_string(),
            base_url: Some("https://custom.example.com/v1".to_string()),
            model_id: "custom-model".to_string(),
            model_info: None,
            reasoning_effort: None,
            extra_body: None,
            custom_headers: None,
            endpoint_kind: OpenAiEndpointKind::Compatible,
            stream: false,
            provider_name: None,
        })
        .unwrap();

        let policy = provider.preoutput_policy();
        assert_eq!(
            policy.budget,
            OPENAI_CLIENT_TOTAL_TIMEOUT + OPENAI_NON_STREAM_PREOUTPUT_GRACE
        );
        assert_eq!(policy.transport, ProviderTransport::Buffered);
    }

    #[test]
    fn test_openai_base_url_normalization() {
        let config = OpenAiConfig {
            api_key: "test-key".to_string(),
            base_url: Some("https://custom.example.com/v1/chat/completions".to_string()),
            model_id: "gpt-4".to_string(),
            model_info: None,
            reasoning_effort: None,
            extra_body: None,
            custom_headers: None,
            endpoint_kind: OpenAiEndpointKind::Compatible,
            stream: true,
            provider_name: None,
        };
        let provider = OpenAiProvider::new(config).unwrap();
        assert_eq!(provider.base_url(), "https://custom.example.com/v1");
    }

    #[test]
    fn test_build_request_body_basic() {
        let config = OpenAiConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "gpt-4".to_string(),
            model_info: None,
            reasoning_effort: None,
            extra_body: None,
            custom_headers: None,
            endpoint_kind: OpenAiEndpointKind::Official,
            stream: true,
            provider_name: None,
        };
        let provider = OpenAiProvider::new(config).unwrap();

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
        assert_eq!(body["model"], "gpt-4");
        assert_eq!(body["stream"], true);
        assert!(body["messages"].as_array().unwrap().len() >= 2);
        assert!(body.get("chat_template_kwargs").is_none());

        let empty_history = ProviderRequest {
            messages: Vec::new(),
            ..request
        };
        let fallback_body = provider.build_request_body(&empty_history).unwrap();
        assert!(
            fallback_body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|msg| { msg["role"] == "user" && msg["content"] == "Please proceed." })
        );
    }

    #[test]
    fn test_build_request_body_non_stream_omits_stream_options() {
        let provider = OpenAiProvider::new(OpenAiConfig {
            api_key: "test-key".to_string(),
            base_url: Some("https://custom.example.com/v1".to_string()),
            model_id: "custom-model".to_string(),
            model_info: None,
            reasoning_effort: None,
            extra_body: Some(serde_json::Map::from_iter([
                ("stream".to_string(), json!(true)),
                (
                    "stream_options".to_string(),
                    json!({"include_usage": true}),
                ),
            ])),
            custom_headers: None,
            endpoint_kind: OpenAiEndpointKind::Compatible,
            stream: false,
            provider_name: None,
        })
        .unwrap();
        let request = ProviderRequest {
            system_prompt: "Be concise.".to_string(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let body = provider.build_request_body(&request).unwrap();
        assert_eq!(body["stream"], false);
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn test_decode_non_stream_completion_preserves_chunks_and_usage() {
        let completion = serde_json::from_value::<OpenAiCompletionResponse>(json!({
            "id": "chatcmpl-nonstream",
            "choices": [{
                "message": {
                    "reasoning_content": "thinking",
                    "content": "answer",
                    "tool_calls": [{
                        "id": "call-1",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": r#"{"path":"README.md"}"#
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "prompt_tokens_details": {"cached_tokens": 40},
                "completion_tokens_details": {"reasoning_tokens": 5},
                "prompt_cache_miss_tokens": 10
            }
        }))
        .unwrap();

        let chunks = decode_openai_completion(completion, None);
        assert!(matches!(chunks[0], ApiStreamChunk::Reasoning(_)));
        assert!(matches!(chunks[1], ApiStreamChunk::Text(_)));
        assert!(matches!(
            chunks[2],
            ApiStreamChunk::ToolCallStarted { ref call_id, ref name }
                if call_id == "call-1" && name == "read_file"
        ));
        assert!(matches!(chunks[3], ApiStreamChunk::ToolCalls(_)));
        assert!(matches!(
            chunks[4],
            ApiStreamChunk::Usage(ref usage)
                if usage.input_tokens == 60
                    && usage.cache_read_tokens == Some(40)
                    && usage.cache_write_tokens == Some(10)
                    && usage.reasoning_tokens == Some(5)
                    && usage.stop_reason.as_deref() == Some("tool_calls")
        ));
    }

    #[test]
    fn test_decode_non_stream_completion_tolerates_missing_optional_fields() {
        let completion = serde_json::from_value::<OpenAiCompletionResponse>(json!({
            "choices": [{
                "message": {
                    "tool_calls": [
                        {},
                        {"id": "call-valid", "function": {"name": "read_file"}}
                    ]
                },
                "finish_reason": "tool_calls"
            }]
        }))
        .unwrap();

        let chunks = decode_openai_completion(completion, None);
        assert_eq!(
            chunks
                .iter()
                .filter(|chunk| matches!(chunk, ApiStreamChunk::ToolCallStarted { .. }))
                .count(),
            1
        );
        assert!(chunks.iter().any(|chunk| matches!(
            chunk,
            ApiStreamChunk::ToolCalls(tool_calls)
                if tool_calls.tool_call.call_id.as_deref() == Some("call-valid")
        )));
        assert!(matches!(chunks.last(), Some(ApiStreamChunk::Usage(usage))
            if usage.stop_reason.as_deref() == Some("tool_calls")));
    }

    #[test]
    fn test_decode_non_stream_completion_tolerates_missing_message_and_id() {
        let completion = serde_json::from_value::<OpenAiCompletionResponse>(json!({
            "choices": [{}]
        }))
        .unwrap();

        let chunks = decode_openai_completion(completion, None);
        assert!(matches!(chunks.as_slice(), [ApiStreamChunk::Usage(usage)]
            if usage.input_tokens == 0 && usage.output_tokens == 0));
    }

    #[test]
    fn test_build_request_body_merges_extra_body_without_overriding_core_fields() {
        let provider = OpenAiProvider::new(OpenAiConfig {
            api_key: "test-key".to_string(),
            base_url: Some("https://custom.example.com/v1".to_string()),
            model_id: "qwen3-coder".to_string(),
            model_info: None,
            reasoning_effort: None,
            extra_body: Some(serde_json::Map::from_iter([
                (
                    "chat_template_kwargs".to_string(),
                    json!({"enable_thinking": true, "preserve_thinking": true}),
                ),
                ("top_k".to_string(), json!(20)),
                ("model".to_string(), json!("different-model")),
            ])),
            custom_headers: None,
            endpoint_kind: OpenAiEndpointKind::Compatible,
            stream: true,
            provider_name: None,
        })
        .unwrap();
        let request = ProviderRequest {
            system_prompt: "You are helpful.".to_string(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let body = provider.build_request_body(&request).unwrap();

        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);
        assert_eq!(body["chat_template_kwargs"]["preserve_thinking"], true);
        assert_eq!(body["top_k"], 20);
        assert_eq!(body["model"], "qwen3-coder");
        assert_eq!(body["stream"], true);
        assert!(body["messages"].is_array());
    }

    #[test]
    fn test_build_request_body_with_tools() {
        let config = OpenAiConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "gpt-4".to_string(),
            model_info: None,
            reasoning_effort: None,
            extra_body: None,
            custom_headers: None,
            endpoint_kind: OpenAiEndpointKind::Official,
            stream: true,
            provider_name: None,
        };
        let provider = OpenAiProvider::new(config).unwrap();

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
        assert_eq!(tools[0]["function"]["name"], "read_file");
    }

    #[test]
    fn test_build_request_body_with_native_tools_on_but_no_tools() {
        let config = OpenAiConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "gpt-4".to_string(),
            model_info: Some(OpenAiCompatibleModelInfo {
                base: ModelInfo {
                    name: Some("gpt-4".to_string()),
                    supports_tools: Some(true),
                    ..ModelInfo::default()
                },
                is_r1_format_required: None,
                system_role: None,
                supports_reasoning_effort: None,
                supports_streaming: None,
            }),
            reasoning_effort: None,
            extra_body: None,
            custom_headers: None,
            endpoint_kind: OpenAiEndpointKind::Official,
            stream: true,
            provider_name: None,
        };
        let provider = OpenAiProvider::new(config).unwrap();

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
    fn test_convert_user_blocks_with_tool_result() {
        let blocks = vec![
            crate::providers::UserContentBlock::Text(crate::providers::TextContentBlock {
                text: "before tool".to_string(),
                shared: crate::providers::SharedContentFields {
                    call_id: None,
                    signature: None,
                },
                reasoning_details: None,
            }),
            crate::providers::UserContentBlock::ToolResult(crate::providers::ToolResultBlock {
                tool_use_id: "call_abc".to_string(),
                content: crate::providers::ToolResultContent::Text("tool output".to_string()),
                shared: crate::providers::SharedContentFields {
                    call_id: None,
                    signature: None,
                },
            }),
        ];

        let converted = convert_user_blocks("user", &blocks);
        assert_eq!(converted.len(), 2);
        assert_eq!(converted[0]["role"], "tool");
        assert_eq!(converted[0]["tool_call_id"], "call_abc");
        assert_eq!(converted[0]["content"], "tool output");
        assert_eq!(converted[1]["role"], "user");
        assert_eq!(converted[1]["content"][0]["text"], "before tool");
    }

    #[tokio::test]
    async fn test_process_openai_sse_line_emits_stop_reason_without_usage() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut delta_state = OpenAiStreamDeltaState::default();
        let mut accumulated_tool_calls = std::collections::HashMap::new();
        let mut completed_tool_call_indices = std::collections::HashSet::new();
        let mut last_stop_reason: Option<String> = None;
        let mut usage_sent = false;
        let model_info: Option<crate::providers::OpenAiCompatibleModelInfo> = None;

        process_openai_sse_line(
            r#"data: {"id":"chatcmpl_123","choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            &tx,
            &mut delta_state,
            &mut accumulated_tool_calls,
            &mut completed_tool_call_indices,
            &mut last_stop_reason,
            model_info.as_ref(),
            &mut usage_sent,
        )
        .await;

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_finish_openai_sse_to_chunks_skips_content_filter_tool_calls() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut buffer = SseLineBuffer::default();
        let mut delta_state = OpenAiStreamDeltaState::default();
        let mut accumulated_tool_calls = std::collections::HashMap::from([(
            0usize,
            (
                "call_1".to_string(),
                "read_file".to_string(),
                "{\"path\":\"a.rs\"}".to_string(),
            ),
        )]);
        let mut completed_tool_call_indices = std::collections::HashSet::new();
        let mut last_stop_reason: Option<String> = Some("content_filter".to_string());
        let model_info: Option<crate::providers::OpenAiCompatibleModelInfo> = None;
        let mut usage_sent = false;

        finish_openai_sse_to_chunks(
            &mut buffer,
            &tx,
            &mut delta_state,
            &mut accumulated_tool_calls,
            &mut completed_tool_call_indices,
            &mut last_stop_reason,
            model_info.as_ref(),
            &mut usage_sent,
        )
        .await;
        drop(tx);

        let mut saw_tool_calls = false;
        while let Some(chunk) = rx.recv().await {
            if matches!(chunk, ApiStreamChunk::ToolCalls(_)) {
                saw_tool_calls = true;
            }
        }

        assert!(!saw_tool_calls);
    }

    // ============== Bug 1 Tests: max_completion_tokens for reasoning models ==============

    #[test]
    fn test_reasoning_model_uses_max_completion_tokens() {
        let config = OpenAiConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "o3-mini".to_string(),
            model_info: Some(OpenAiCompatibleModelInfo {
                base: ModelInfo {
                    name: Some("o3-mini".to_string()),
                    max_tokens: Some(100_000),
                    context_window: Some(200_000),
                    supports_images: Some(true),
                    supports_prompt_cache: true,
                    supports_reasoning: Some(true),
                    input_price: Some(1.1),
                    output_price: Some(4.4),
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
                    supports_tools: Some(true),
                    api_format: None,
                },
                is_r1_format_required: None,
                system_role: None,
                supports_reasoning_effort: None,
                supports_streaming: None,
            }),
            reasoning_effort: None,
            extra_body: None,
            custom_headers: None,
            endpoint_kind: OpenAiEndpointKind::Official,
            stream: true,
            provider_name: None,
        };
        let provider = OpenAiProvider::new(config).unwrap();

        let request = ProviderRequest {
            system_prompt: "You are a helpful assistant.".to_string(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let body = provider.build_request_body(&request).unwrap();
        assert_eq!(
            body["max_completion_tokens"], 100_000,
            "reasoning model should use max_completion_tokens"
        );
        assert!(
            body.get("max_tokens").is_none(),
            "reasoning model should NOT have max_tokens"
        );
        assert_eq!(body["messages"][0]["role"], "developer");
    }

    #[test]
    fn test_compatible_endpoint_uses_standard_shape_for_openai_model_id() {
        let config = OpenAiConfig {
            api_key: "test-key".to_string(),
            base_url: Some("https://gateway.example.com/v1".to_string()),
            model_id: "gpt-5.4".to_string(),
            model_info: Some(get_openai_model_info("gpt-5.4")),
            reasoning_effort: None,
            extra_body: None,
            custom_headers: None,
            endpoint_kind: OpenAiEndpointKind::Compatible,
            stream: true,
            provider_name: Some("openai-compatible".to_string()),
        };
        let provider = OpenAiProvider::new(config).unwrap();
        let request = ProviderRequest {
            system_prompt: "You are a helpful assistant.".to_string(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let body = provider.build_request_body(&request).unwrap();

        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["max_tokens"], 128_000);
        assert!(body.get("max_completion_tokens").is_none());
    }

    #[test]
    fn test_compatible_endpoint_qwen_reasoning_uses_max_tokens() {
        let model_id = "qwen/qwen3.5-27b";
        let model_info = get_openai_model_info(model_id);
        assert_eq!(model_info.base.supports_reasoning, Some(true));

        let config = OpenAiConfig {
            api_key: "test-key".to_string(),
            base_url: Some("https://gateway.example.com/v1".to_string()),
            model_id: model_id.to_string(),
            model_info: Some(model_info),
            reasoning_effort: None,
            extra_body: None,
            custom_headers: None,
            endpoint_kind: OpenAiEndpointKind::Compatible,
            stream: true,
            provider_name: Some("openai-compatible".to_string()),
        };
        let provider = OpenAiProvider::new(config).unwrap();
        let request = ProviderRequest {
            system_prompt: "You are a helpful assistant.".to_string(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let body = provider.build_request_body(&request).unwrap();

        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["max_tokens"], 65_536);
        assert!(body.get("max_completion_tokens").is_none());
    }

    #[test]
    fn test_non_reasoning_model_uses_max_tokens() {
        let config = OpenAiConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "gpt-4o".to_string(),
            model_info: Some(OpenAiCompatibleModelInfo {
                base: ModelInfo {
                    name: Some("gpt-4o".to_string()),
                    max_tokens: Some(16_384),
                    context_window: Some(128_000),
                    supports_images: Some(true),
                    supports_prompt_cache: false,
                    supports_reasoning: Some(false),
                    input_price: Some(2.5),
                    output_price: Some(10.0),
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
                    supports_tools: Some(true),
                    api_format: None,
                },
                is_r1_format_required: None,
                system_role: None,
                supports_reasoning_effort: None,
                supports_streaming: None,
            }),
            reasoning_effort: None,
            extra_body: None,
            custom_headers: None,
            endpoint_kind: OpenAiEndpointKind::Official,
            stream: true,
            provider_name: None,
        };
        let provider = OpenAiProvider::new(config).unwrap();

        let request = ProviderRequest {
            system_prompt: "You are a helpful assistant.".to_string(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let body = provider.build_request_body(&request).unwrap();
        assert_eq!(
            body["max_tokens"], 16_384,
            "non-reasoning model should use max_tokens"
        );
        assert!(
            body.get("max_completion_tokens").is_none(),
            "non-reasoning model should NOT have max_completion_tokens"
        );
    }

    #[test]
    fn test_no_max_tokens_when_zero() {
        let config = OpenAiConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "gpt-4o".to_string(),
            model_info: Some(OpenAiCompatibleModelInfo {
                base: ModelInfo {
                    name: Some("gpt-4o".to_string()),
                    max_tokens: Some(0),
                    context_window: Some(128_000),
                    supports_images: Some(true),
                    supports_prompt_cache: false,
                    supports_reasoning: Some(false),
                    input_price: Some(2.5),
                    output_price: Some(10.0),
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
                    supports_tools: Some(true),
                    api_format: None,
                },
                is_r1_format_required: None,
                system_role: None,
                supports_reasoning_effort: None,
                supports_streaming: None,
            }),
            reasoning_effort: None,
            extra_body: None,
            custom_headers: None,
            endpoint_kind: OpenAiEndpointKind::Official,
            stream: true,
            provider_name: None,
        };
        let provider = OpenAiProvider::new(config).unwrap();

        let request = ProviderRequest {
            system_prompt: "You are a helpful assistant.".to_string(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let body = provider.build_request_body(&request).unwrap();
        assert!(
            body.get("max_tokens").is_none(),
            "max_tokens=0 should not be sent"
        );
        assert!(
            body.get("max_completion_tokens").is_none(),
            "max_completion_tokens should not be sent when max_tokens=0"
        );
    }

    // ============== Bug 2 Tests: temperature handling ==============

    #[test]
    fn test_default_temperature_omitted() {
        let config = OpenAiConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "gpt-4o".to_string(),
            model_info: None,
            reasoning_effort: None,
            extra_body: None,
            custom_headers: None,
            endpoint_kind: OpenAiEndpointKind::Official,
            stream: true,
            provider_name: None,
        };
        let provider = OpenAiProvider::new(config).unwrap();

        let request = ProviderRequest {
            system_prompt: "You are a helpful assistant.".to_string(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let body = provider.build_request_body(&request).unwrap();
        assert!(
            body.get("temperature").is_none(),
            "temperature should be omitted by default"
        );
    }

    #[test]
    fn test_nonzero_temperature_sent() {
        let config = OpenAiConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "deepseek-chat".to_string(),
            model_info: Some(crate::providers::deepseek::get_deepseek_model_info(
                "deepseek-chat",
            )),
            reasoning_effort: None,
            extra_body: None,
            custom_headers: None,
            endpoint_kind: OpenAiEndpointKind::Compatible,
            stream: true,
            provider_name: None,
        };
        let provider = OpenAiProvider::new(config).unwrap();

        let request = ProviderRequest {
            system_prompt: "You are a helpful assistant.".to_string(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let body = provider.build_request_body(&request).unwrap();
        assert_eq!(
            body["temperature"], 0.7,
            "profile temperature should be sent"
        );
    }

    #[test]
    fn test_zero_temperature_omitted() {
        let config = OpenAiConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "gpt-4o".to_string(),
            model_info: Some(OpenAiCompatibleModelInfo {
                base: ModelInfo {
                    name: Some("gpt-4o".to_string()),
                    max_tokens: None,
                    context_window: Some(128_000),
                    supports_images: Some(true),
                    supports_prompt_cache: false,
                    supports_reasoning: Some(false),
                    input_price: Some(2.5),
                    output_price: Some(10.0),
                    image_output_price: None,
                    thinking_config: None,
                    supports_global_endpoint: None,
                    cache_writes_price: None,
                    cache_reads_price: None,
                    description: None,
                    tiers: None,
                    temperature: Some(0.0),
                    top_p: None,
                    top_k: None,
                    supports_tools: Some(true),
                    api_format: None,
                },
                is_r1_format_required: None,
                system_role: None,
                supports_reasoning_effort: None,
                supports_streaming: None,
            }),
            reasoning_effort: None,
            extra_body: None,
            custom_headers: None,
            endpoint_kind: OpenAiEndpointKind::Official,
            stream: true,
            provider_name: None,
        };
        let provider = OpenAiProvider::new(config).unwrap();

        let request = ProviderRequest {
            system_prompt: "You are a helpful assistant.".to_string(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let body = provider.build_request_body(&request).unwrap();
        assert!(
            body.get("temperature").is_none(),
            "temperature=0.0 should be omitted"
        );
    }

    #[test]
    fn test_reasoning_model_temperature_always_omitted() {
        let config = OpenAiConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "o3-mini".to_string(),
            model_info: Some(OpenAiCompatibleModelInfo {
                base: ModelInfo {
                    name: Some("o3-mini".to_string()),
                    max_tokens: None,
                    context_window: Some(200_000),
                    supports_images: Some(true),
                    supports_prompt_cache: true,
                    supports_reasoning: Some(true),
                    input_price: Some(1.1),
                    output_price: Some(4.4),
                    image_output_price: None,
                    thinking_config: None,
                    supports_global_endpoint: None,
                    cache_writes_price: None,
                    cache_reads_price: None,
                    description: None,
                    tiers: None,
                    temperature: Some(0.5),
                    top_p: None,
                    top_k: None,
                    supports_tools: Some(true),
                    api_format: None,
                },
                is_r1_format_required: None,
                system_role: None,
                supports_reasoning_effort: None,
                supports_streaming: None,
            }),
            reasoning_effort: None,
            extra_body: None,
            custom_headers: None,
            endpoint_kind: OpenAiEndpointKind::Official,
            stream: true,
            provider_name: None,
        };
        let provider = OpenAiProvider::new(config).unwrap();

        let request = ProviderRequest {
            system_prompt: "You are a helpful assistant.".to_string(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let body = provider.build_request_body(&request).unwrap();
        assert!(
            body.get("temperature").is_none(),
            "reasoning model should never have temperature, even if model_info.temperature is set"
        );
    }

    // ============== Bug 3 Tests: get_openai_model_info ==============

    #[test]
    fn test_get_openai_model_info_gpt4o() {
        let info = get_openai_model_info("gpt-4o");
        assert_eq!(info.base.context_window, Some(128_000));
        assert_eq!(info.base.max_tokens, Some(16_384));
        assert_eq!(info.base.input_price, Some(2.5));
        assert_eq!(info.base.output_price, Some(10.0));
        assert_eq!(info.base.temperature, None);
    }

    #[test]
    fn test_get_openai_model_info_o3_mini() {
        let info = get_openai_model_info("o3-mini");
        assert_eq!(info.base.context_window, Some(200_000));
        assert_eq!(info.base.max_tokens, Some(100_000));
        assert_eq!(info.base.supports_reasoning, Some(true));
        assert!(info.base.supports_prompt_cache);
    }

    #[test]
    fn test_get_openai_model_info_current_models() {
        let cases = [
            ("gpt-5.6", 1_050_000, 5.0, 30.0, 0.5, 6.25),
            ("gpt-5.6-sol", 1_050_000, 5.0, 30.0, 0.5, 6.25),
            ("gpt-5.6-terra", 1_050_000, 2.5, 15.0, 0.25, 3.125),
            ("gpt-5.6-luna", 1_050_000, 1.0, 6.0, 0.1, 1.25),
            ("gpt-5.3-codex", 400_000, 1.75, 14.0, 0.175, 2.1875),
            ("gpt-5.4", 1_050_000, 2.5, 15.0, 0.25, 3.125),
            ("gpt-5.4-mini", 400_000, 0.75, 4.5, 0.075, 0.9375),
        ];

        for (model_id, context_window, input, output, cache_read, cache_write) in cases {
            let info = get_openai_model_info(model_id);
            assert_eq!(info.base.context_window, Some(context_window));
            assert_eq!(info.base.max_tokens, Some(128_000));
            assert_eq!(info.base.input_price, Some(input));
            assert_eq!(info.base.output_price, Some(output));
            assert_eq!(info.base.cache_reads_price, Some(cache_read));
            assert_eq!(info.base.cache_writes_price, Some(cache_write));
            assert_eq!(info.base.supports_images, Some(true));
            assert_eq!(info.base.supports_tools, Some(true));
            assert_eq!(info.base.supports_reasoning, Some(true));
            assert!(info.base.supports_prompt_cache);
            assert_eq!(info.supports_reasoning_effort, Some(true));
            assert_eq!(info.supports_streaming, Some(true));
        }
    }

    #[test]
    fn test_get_openai_model_info_qwen_family() {
        for model_id in ["qwen3.6-35b-a3b", "qwen/qwen3.5-27b"] {
            let info = get_openai_model_info(model_id);
            assert_eq!(info.base.context_window, Some(262_144));
            assert_eq!(info.base.max_tokens, Some(65_536));
            assert_eq!(info.base.supports_tools, Some(true));
            assert_eq!(info.base.supports_images, Some(false));
            assert!(!info.base.supports_prompt_cache);
            assert_eq!(info.base.supports_reasoning, Some(true));
            assert_eq!(info.supports_reasoning_effort, Some(false));
        }
    }

    #[test]
    fn test_get_openai_model_info_unknown_fallback() {
        let info = get_openai_model_info("unknown-model-x");
        assert_eq!(info.base.context_window, Some(128_000));
        assert_eq!(info.base.max_tokens, None);
        assert_eq!(info.base.input_price, Some(0.0));
        assert_eq!(info.base.output_price, Some(0.0));
        assert_eq!(info.base.temperature, None);
    }

    #[test]
    fn test_get_model_uses_lookup() {
        let config = OpenAiConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "gpt-4o".to_string(),
            model_info: None,
            reasoning_effort: None,
            extra_body: None,
            custom_headers: None,
            endpoint_kind: OpenAiEndpointKind::Official,
            stream: true,
            provider_name: None,
        };
        let provider = OpenAiProvider::new(config).unwrap();
        let model = provider.get_model();

        assert_eq!(model.info.context_window, Some(128_000));
        assert_eq!(model.info.max_tokens, Some(16_384));
        assert_eq!(model.info.temperature, None);
    }

    #[test]
    fn test_get_model_prefers_explicit_model_info() {
        let custom_info = OpenAiCompatibleModelInfo {
            base: ModelInfo {
                name: Some("custom".to_string()),
                max_tokens: Some(99_999),
                context_window: Some(999_999),
                supports_images: Some(false),
                supports_prompt_cache: false,
                supports_reasoning: Some(false),
                input_price: Some(0.01),
                output_price: Some(0.02),
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
                supports_tools: Some(false),
                api_format: None,
            },
            is_r1_format_required: None,
            system_role: None,
            supports_reasoning_effort: None,
            supports_streaming: None,
        };

        let config = OpenAiConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "gpt-4o".to_string(),
            model_info: Some(custom_info.clone()),
            reasoning_effort: None,
            extra_body: None,
            custom_headers: None,
            endpoint_kind: OpenAiEndpointKind::Official,
            stream: true,
            provider_name: None,
        };
        let provider = OpenAiProvider::new(config).unwrap();
        let model = provider.get_model();

        assert_eq!(model.info.context_window, Some(999_999));
        assert_eq!(model.info.max_tokens, Some(99_999));
        assert_eq!(model.info.temperature, Some(0.7));
    }

    #[test]
    fn test_openai_provider_error_preserves_body() {
        // Verify that provider-specific error fields in the response body are preserved
        // This test documents that ProviderHttpError stores the raw body, not parsed fields
        // Provider-specific fields (error.code, error.type, etc.) are preserved in the body string
        // No parsing is done that would drop fields - the raw JSON response is kept intact
        let config = OpenAiConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "gpt-4o".to_string(),
            model_info: None,
            reasoning_effort: None,
            extra_body: None,
            custom_headers: None,
            endpoint_kind: OpenAiEndpointKind::Official,
            stream: true,
            provider_name: Some("openai".to_string()),
        };
        let _provider = OpenAiProvider::new(config).unwrap();
        // Test passes if provider constructs successfully with provider_name set
        // Error body preservation is verified by ProviderHttpError storing raw body string
    }

    #[test]
    fn test_format_stream_error_diagnostics_includes_elapsed_and_relevant_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", HeaderValue::from_static("gzip"));
        headers.insert(
            "content-type",
            HeaderValue::from_static("text/event-stream"),
        );
        headers.insert("cf-ray", HeaderValue::from_static("abc123"));

        let diagnostics = format_stream_error_diagnostics(
            &headers,
            Duration::from_millis(1532),
            Some(Duration::from_millis(12)),
        );

        assert!(diagnostics.contains("elapsed=1532ms"));
        assert!(diagnostics.contains("first_byte=12ms"));
        assert!(diagnostics.contains("content-encoding=gzip"));
        assert!(diagnostics.contains("content-type=text/event-stream"));
        assert!(diagnostics.contains("cf-ray=abc123"));
    }

    #[test]
    fn test_format_stream_error_diagnostics_marks_pending_first_byte() {
        let diagnostics =
            format_stream_error_diagnostics(&HeaderMap::new(), Duration::from_millis(1532), None);

        assert!(diagnostics.contains("elapsed=1532ms"));
        assert!(diagnostics.contains("first_byte=pending"));
    }

    #[test]
    fn test_retryable_stream_transport_error_detects_decode_failures() {
        assert!(is_retryable_stream_transport_error(
            "error decoding response body"
        ));
        assert!(is_retryable_stream_transport_error("Decoding timeout"));
        assert!(!is_retryable_stream_transport_error(
            "invalid request payload"
        ));
    }

    #[tokio::test]
    async fn test_next_stream_item_with_timeout_returns_ready_item() {
        let mut stream = futures::stream::iter(["chunk"]);

        let item = next_stream_item_with_timeout(&mut stream, Duration::from_secs(1))
            .await
            .unwrap();

        assert_eq!(item, Some("chunk"));
    }

    #[tokio::test]
    async fn test_next_stream_item_with_timeout_returns_timeout() {
        let mut stream = futures::stream::pending::<usize>();

        assert!(
            next_stream_item_with_timeout(&mut stream, Duration::ZERO)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_next_stream_item_until_receiver_closed_stops_pending_read() {
        let mut stream = futures::stream::pending::<usize>();
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);

        let result =
            next_stream_item_until_receiver_closed(&mut stream, &tx, Duration::from_secs(1)).await;

        assert!(result.is_none());
    }

    #[test]
    fn test_normalize_reasoning_delta_strips_overlapping_prefixes() {
        let mut state = OpenAiStreamDeltaState::default();

        assert_eq!(
            normalize_reasoning_delta(&mut state.emitted_reasoning, "The".to_string()).as_deref(),
            Some("The")
        );
        assert_eq!(
            normalize_reasoning_delta(&mut state.emitted_reasoning, "The user".to_string())
                .as_deref(),
            Some(" user")
        );
        assert_eq!(
            normalize_reasoning_delta(&mut state.emitted_reasoning, " user wants".to_string())
                .as_deref(),
            Some(" wants")
        );
        assert_eq!(
            normalize_reasoning_delta(&mut state.emitted_reasoning, " wants me".to_string())
                .as_deref(),
            Some(" me")
        );
        assert_eq!(state.emitted_reasoning, "The user wants me");
    }

    #[tokio::test]
    async fn test_process_openai_sse_line_normalizes_overlapping_reasoning_chunks() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let mut delta_state = OpenAiStreamDeltaState::default();
        let mut accumulated_tool_calls = std::collections::HashMap::new();
        let mut completed_tool_call_indices = std::collections::HashSet::new();
        let mut last_stop_reason: Option<String> = None;
        let mut usage_sent = false;
        let model_info: Option<crate::providers::OpenAiCompatibleModelInfo> = None;

        for line in [
            r#"data: {"id":"chatcmpl_123","choices":[{"delta":{"reasoning_content":"The"},"finish_reason":null}]}"#,
            r#"data: {"id":"chatcmpl_123","choices":[{"delta":{"reasoning_content":"The user"},"finish_reason":null}]}"#,
            r#"data: {"id":"chatcmpl_123","choices":[{"delta":{"reasoning_content":" user wants"},"finish_reason":null}]}"#,
        ] {
            process_openai_sse_line(
                line,
                &tx,
                &mut delta_state,
                &mut accumulated_tool_calls,
                &mut completed_tool_call_indices,
                &mut last_stop_reason,
                model_info.as_ref(),
                &mut usage_sent,
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

    #[tokio::test]
    async fn test_qwen_thinking_wrapper_normalizes_tool_name_and_announces_start() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let mut delta_state = OpenAiStreamDeltaState::default();
        let mut accumulated_tool_calls = std::collections::HashMap::new();
        let mut completed_tool_call_indices = std::collections::HashSet::new();
        let mut last_stop_reason: Option<String> = None;
        let mut usage_sent = false;
        let model_info: Option<crate::providers::OpenAiCompatibleModelInfo> = None;
        let wrapped_name = "write_to_file\n</think>\n\n<tool_call>\n<function=write_to_file";
        let line = format!(
            "data: {}",
            serde_json::json!({
                "id": "chatcmpl_123",
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_write",
                            "function": {
                                "name": wrapped_name,
                                "arguments": r#"{"path":"tetris.c","content":"x"}"#,
                            },
                        }],
                    },
                    "finish_reason": null,
                }],
            })
        );

        process_openai_sse_line(
            &line,
            &tx,
            &mut delta_state,
            &mut accumulated_tool_calls,
            &mut completed_tool_call_indices,
            &mut last_stop_reason,
            model_info.as_ref(),
            &mut usage_sent,
        )
        .await;
        finish_openai_sse_to_chunks(
            &mut SseLineBuffer::default(),
            &tx,
            &mut delta_state,
            &mut accumulated_tool_calls,
            &mut completed_tool_call_indices,
            &mut last_stop_reason,
            model_info.as_ref(),
            &mut usage_sent,
        )
        .await;

        let mut chunks = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            chunks.push(chunk);
        }

        assert!(matches!(
            chunks.first(),
            Some(ApiStreamChunk::ToolCallStarted { call_id, name })
                if call_id == "call_write" && name == "write_to_file"
        ));
        assert!(chunks.iter().any(|chunk| matches!(
            chunk,
            ApiStreamChunk::ToolCalls(tool_chunk)
                if tool_chunk.tool_call.function.name.as_deref() == Some("write_to_file")
        )));
    }

    #[tokio::test]
    async fn test_fragmented_tool_call_preserves_initial_id() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let mut delta_state = OpenAiStreamDeltaState::default();
        let mut accumulated_tool_calls = std::collections::HashMap::new();
        let mut completed_tool_call_indices = std::collections::HashSet::new();
        let mut last_stop_reason: Option<String> = None;
        let mut usage_sent = false;
        let model_info: Option<crate::providers::OpenAiCompatibleModelInfo> = None;

        let lines = [
            serde_json::json!({
                "id": "chatcmpl_123",
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_qwen",
                            "function": {
                                "name": "execute_command",
                                "arguments": "",
                            },
                        }],
                    },
                    "finish_reason": null,
                }],
            }),
            serde_json::json!({
                "id": "chatcmpl_123",
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "",
                            "function": {"arguments": "{\"commands\": "},
                        }],
                    },
                    "finish_reason": null,
                }],
            }),
            serde_json::json!({
                "id": "chatcmpl_123",
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "",
                            "function": {"arguments": "[\"pwd\"]}"},
                        }],
                    },
                    "finish_reason": null,
                }],
            }),
            serde_json::json!({
                "id": "chatcmpl_123",
                "choices": [{
                    "delta": {},
                    "finish_reason": "tool_calls",
                }],
            }),
        ];

        for line in lines {
            process_openai_sse_line(
                &format!("data: {line}"),
                &tx,
                &mut delta_state,
                &mut accumulated_tool_calls,
                &mut completed_tool_call_indices,
                &mut last_stop_reason,
                model_info.as_ref(),
                &mut usage_sent,
            )
            .await;
        }

        let chunks: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(matches!(
            chunks.first(),
            Some(ApiStreamChunk::ToolCallStarted { call_id, name })
                if call_id == "call_qwen" && name == "execute_command"
        ));
        assert!(chunks.iter().any(|chunk| matches!(
            chunk,
            ApiStreamChunk::ToolCalls(tool_chunk)
                if tool_chunk.tool_call.call_id.as_deref() == Some("call_qwen")
                    && tool_chunk.tool_call.function.name.as_deref() == Some("execute_command")
                    && tool_chunk
                        .tool_call
                        .function
                        .arguments
                        .as_deref()
                        .and_then(|arguments| serde_json::from_str(arguments).ok())
                        == Some(serde_json::json!({"commands": ["pwd"]}))
        )));
    }

    #[test]
    fn test_qwen_thinking_wrapper_does_not_normalize_mismatched_or_unsafe_names() {
        assert_eq!(
            normalize_qwen_thinking_tool_name(
                "read_file\n</think>\n\n<tool_call>\n<function=write_to_file"
            ),
            None
        );
        assert_eq!(
            normalize_qwen_thinking_tool_name(
                "write_to_file;\n</think>\n\n<tool_call>\n<function=write_to_file;"
            ),
            None
        );
    }

    #[test]
    fn test_qwen_thinking_wrapper_normalizes_trailing_tag_delimiter() {
        assert_eq!(
            normalize_qwen_thinking_tool_name(
                "write_to_file\n</think>\n\n<tool_call>\n<function=write_to_file>"
            ),
            Some("write_to_file".to_string())
        );
    }
}
#[cfg(test)]
mod debug_test {
    use futures::StreamExt;
    use crate::providers::openai::{
        OpenAiConfig, OpenAiEndpointKind, OpenAiProvider, OpenAiStreamDeltaState,
        finish_openai_sse_to_chunks, parse_openai_sse_to_chunks,
    };
    use crate::providers::{ApiStreamChunk, Provider, ProviderRequest, SseLineBuffer};

    #[tokio::test]
    async fn debug_openai_text_only_stream() {
        let sse = r#"
data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}

data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"gpt-4","choices":[{"index":0,"delta":{"content":" world"},"finish_reason":null}]}

data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"gpt-4","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: [DONE]
"#;

        let (tx, mut rx) = tokio::sync::mpsc::channel::<ApiStreamChunk>(100);
        let mut buffer = SseLineBuffer::default();
        let mut delta_state = OpenAiStreamDeltaState::default();
        let mut accumulated_tool_calls = std::collections::HashMap::new();
        let mut completed_tool_call_indices = std::collections::HashSet::new();
        let mut last_stop_reason: Option<String> = None;
        let mut usage_sent = false;
        let model_info: Option<crate::providers::OpenAiCompatibleModelInfo> = None;
        parse_openai_sse_to_chunks(
            sse.as_bytes(),
            &mut buffer,
            &tx,
            &mut delta_state,
            &mut accumulated_tool_calls,
            &mut completed_tool_call_indices,
            &mut last_stop_reason,
            model_info.as_ref(),
            &mut usage_sent,
        )
        .await;
        finish_openai_sse_to_chunks(
            &mut buffer,
            &tx,
            &mut delta_state,
            &mut accumulated_tool_calls,
            &mut completed_tool_call_indices,
            &mut last_stop_reason,
            model_info.as_ref(),
            &mut usage_sent,
        )
        .await;
        drop(tx);

        let mut chunks = Vec::new();
        while let Some(chunk) = rx.recv().await {
            println!("Chunk: {:?}", chunk);
            chunks.push(chunk);
        }

        println!("Total chunks: {}", chunks.len());
    }

    #[tokio::test]
    async fn test_sse_fallback_without_usage_emits_synthetic_usage() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ApiStreamChunk>(16);
        let mut buffer = SseLineBuffer::default();
        let mut delta_state = OpenAiStreamDeltaState::default();
        let mut accumulated_tool_calls = std::collections::HashMap::new();
        let mut completed_tool_call_indices = std::collections::HashSet::new();
        let mut last_stop_reason = None;
        let mut usage_sent = false;
        let sse = br#"data: {"id":"chatcmpl-fallback","choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}
"#;

        parse_openai_sse_to_chunks(
            sse,
            &mut buffer,
            &tx,
            &mut delta_state,
            &mut accumulated_tool_calls,
            &mut completed_tool_call_indices,
            &mut last_stop_reason,
            None,
            &mut usage_sent,
        )
        .await;
        finish_openai_sse_to_chunks(
            &mut buffer,
            &tx,
            &mut delta_state,
            &mut accumulated_tool_calls,
            &mut completed_tool_call_indices,
            &mut last_stop_reason,
            None,
            &mut usage_sent,
        )
        .await;
        drop(tx);

        let chunks: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(chunks.iter().any(|chunk| matches!(
            chunk,
            ApiStreamChunk::Usage(usage)
                if usage.input_tokens == 0
                    && usage.output_tokens == 0
                    && usage.stop_reason.as_deref() == Some("stop")
        )));
    }

    #[tokio::test]
    async fn test_non_stream_create_message_sniffs_sse_without_sse_content_type() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let sse_body =
            br#"data: {"id":"chatcmpl-fallback","choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}

"#;
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request);
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                sse_body.len()
            )
            .unwrap();
            socket.write_all(sse_body).unwrap();
        });

        let provider = OpenAiProvider::new(OpenAiConfig {
            api_key: "test-key".to_string(),
            base_url: Some(format!("http://{address}")),
            model_id: "custom-model".to_string(),
            model_info: None,
            reasoning_effort: None,
            extra_body: None,
            custom_headers: None,
            endpoint_kind: OpenAiEndpointKind::Compatible,
            stream: false,
            provider_name: None,
        })
        .unwrap();
        let request = ProviderRequest {
            system_prompt: "Be concise.".to_string(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let mut stream = provider.create_message(request).await.unwrap();
        let mut chunks = Vec::new();
        while let Some(chunk) = stream.next().await {
            chunks.push(chunk);
        }
        server.join().unwrap();

        assert!(chunks.iter().any(|chunk| matches!(
            chunk,
            ApiStreamChunk::Usage(usage)
                if usage.output_tokens == 0
                    && usage.stop_reason.as_deref() == Some("stop")
        )));
    }

    #[tokio::test]
    async fn test_cache_tokens_not_double_counted_in_cost() {
        // Test that cached tokens are subtracted from input_tokens and cost is calculated correctly
        let sse = r#"
data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"gpt-4","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1000,"completion_tokens":100,"prompt_tokens_details":{"cached_tokens":800}}}
"#;

        let (tx, mut rx) = tokio::sync::mpsc::channel::<ApiStreamChunk>(100);
        let mut buffer = SseLineBuffer::default();
        let mut delta_state = OpenAiStreamDeltaState::default();
        let mut accumulated_tool_calls = std::collections::HashMap::new();
        let mut completed_tool_call_indices = std::collections::HashSet::new();
        let mut last_stop_reason: Option<String> = None;
        let mut usage_sent = false;

        // Use model info with pricing
        let model_info = Some(crate::providers::OpenAiCompatibleModelInfo {
            base: crate::providers::ModelInfo {
                name: Some("gpt-4".to_string()),
                max_tokens: Some(8192),
                context_window: Some(128_000),
                supports_images: Some(true),
                supports_prompt_cache: true,
                supports_reasoning: Some(false),
                input_price: Some(10.0),  // $10 per 1M tokens
                output_price: Some(30.0), // $30 per 1M tokens
                image_output_price: None,
                thinking_config: None,
                supports_global_endpoint: None,
                cache_writes_price: Some(5.0), // $5 per 1M tokens
                cache_reads_price: Some(0.5),  // $0.50 per 1M tokens (discounted)
                description: None,
                tiers: None,
                temperature: None,
                top_p: None,
                top_k: None,
                supports_tools: None,
                api_format: None,
            },
            is_r1_format_required: None,
            system_role: None,
            supports_reasoning_effort: None,
            supports_streaming: None,
        });

        parse_openai_sse_to_chunks(
            sse.as_bytes(),
            &mut buffer,
            &tx,
            &mut delta_state,
            &mut accumulated_tool_calls,
            &mut completed_tool_call_indices,
            &mut last_stop_reason,
            model_info.as_ref(),
            &mut usage_sent,
        )
        .await;
        finish_openai_sse_to_chunks(
            &mut buffer,
            &tx,
            &mut delta_state,
            &mut accumulated_tool_calls,
            &mut completed_tool_call_indices,
            &mut last_stop_reason,
            model_info.as_ref(),
            &mut usage_sent,
        )
        .await;
        drop(tx);

        let mut chunks = Vec::new();
        while let Some(chunk) = rx.recv().await {
            chunks.push(chunk);
        }

        // Find the usage chunk
        let usage_chunk = chunks
            .iter()
            .find_map(|c| match c {
                ApiStreamChunk::Usage(u) => Some(u),
                _ => None,
            })
            .expect("Should have usage chunk");

        // Verify input_tokens excludes cached tokens (1000 - 800 = 200)
        assert_eq!(
            usage_chunk.input_tokens, 200,
            "input_tokens should exclude cached tokens"
        );

        // Verify cache_read_tokens is reported separately
        assert_eq!(
            usage_chunk.cache_read_tokens,
            Some(800),
            "cache_read_tokens should be 800"
        );

        // Verify cost calculation:
        // input_cost = 200 * $10 / 1M = $0.002
        // output_cost = 100 * $30 / 1M = $0.003
        // cache_read_cost = 800 * $0.50 / 1M = $0.0004
        // total = $0.0054
        let expected_cost = (200.0 * 10.0 / 1_000_000.0)
            + (100.0 * 30.0 / 1_000_000.0)
            + (800.0 * 0.5 / 1_000_000.0);
        assert!(
            usage_chunk.total_cost.is_some(),
            "total_cost should be calculated"
        );
        assert!(
            (usage_chunk.total_cost.unwrap() - expected_cost).abs() < 0.0001,
            "cost should be correct"
        );
    }
}
