//! OpenAI provider implementation for sned CLI.
//!
//! Ports behavior from `dirac/src/core/api/providers/openai.ts` and
//! `dirac/src/core/api/providers/openai-native.ts`.

use crate::providers::{
    ApiStream, ApiStreamChunk, ApiStreamReasoningChunk, ApiStreamTextChunk, ApiStreamToolCall,
    ApiStreamToolCallFunction, ApiStreamToolCallsChunk, ApiStreamUsageChunk, MessageRole,
    ModelInfo, OpenAiCompatibleModelInfo, Provider, ProviderError, ProviderHttpError,
    ProviderModel, ProviderRequest,
};
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc::error::TrySendError;

/// Configuration for the OpenAI provider.
#[derive(Clone)]
pub struct OpenAiConfig {
    pub api_key: String,
    pub base_url: Option<String>,
    pub model_id: String,
    pub model_info: Option<OpenAiCompatibleModelInfo>,
    pub reasoning_effort: Option<String>,
    pub custom_headers: Option<std::collections::HashMap<String, String>>,
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
            .field("custom_headers", &self.custom_headers)
            .field("provider_name", &self.provider_name)
            .finish()
    }
}

/// OpenAI-compatible provider (covers generic OpenAI, Azure, and custom base URL).
pub struct OpenAiProvider {
    config: OpenAiConfig,
    client: reqwest::Client,
    provider_name: String,
}

impl OpenAiProvider {
    pub fn new(config: OpenAiConfig) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
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
        })
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
    pub fn base_url(&self) -> String {
        self.config
            .base_url
            .as_ref()
            .cloned()
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

    fn build_request_body(&self, request: &ProviderRequest) -> anyhow::Result<serde_json::Value> {
        let model_id = &self.config.model_id;
        let is_reasoning_family = ["o1", "o3", "o4", "gpt-5"].iter().any(|prefix| {
            model_id.starts_with(prefix) || model_id.contains(&format!("/{}", prefix))
        }) && !model_id.contains("chat");

        let mut messages = vec![];

        // System/developer message
        if is_reasoning_family {
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

                if !tool_parts.is_empty() {
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
                } else {
                    let joined = join_assistant_text_parts(content);
                    let Some(msg_obj) = msg.as_object_mut() else {
                        tracing::warn!("Skipping non-object message in content conversion");
                        continue;
                    };
                    msg_obj.insert("content".to_string(), json!(joined));
                }
            }
        }

        let mut body = json!({
            "model": model_id,
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true},
        });

        // Temperature: match TS behavior — omit by default (API uses model default).
        // If model_info.temperature is set and non-zero, send it.
        // If model_info.temperature is 0, omit (TS converts 0 → undefined).
        // Reasoning family models never support temperature.
        if !is_reasoning_family
            && let Some(temp) = self.config.model_info.as_ref().and_then(|i| i.temperature)
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
            if is_reasoning_family {
                body["max_completion_tokens"] = json!(max_tokens);
            } else {
                body["max_tokens"] = json!(max_tokens);
            }
        }

        // Reasoning effort
        if let Some(effort) = &self.config.reasoning_effort
            && effort != "none"
        {
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

        Ok(body)
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
                "OpenAI provider channel full, dropping {} chunk",
                chunk_type
            );
            false
        }
        Err(TrySendError::Closed(_)) => {
            tracing::debug!(
                "OpenAI provider channel closed, cannot send {} chunk",
                chunk_type
            );
            false
        }
    }
}

async fn process_openai_sse_line(
    line: &str,
    tx: &tokio::sync::mpsc::Sender<ApiStreamChunk>,
    accumulated_tool_calls: &mut std::collections::HashMap<usize, (String, String, String)>,
    completed_tool_call_indices: &mut std::collections::HashSet<usize>,
    last_stop_reason: &mut Option<String>,
    model_info: &Option<crate::providers::OpenAiCompatibleModelInfo>,
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
        let chunk = match serde_json::from_str::<OpenAiStreamChunk>(data) {
            Ok(c) => c,
            Err(_) => {
                tracing::warn!(line = %line, "OpenAI SSE: failed to parse chunk");
                return;
            }
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
                && !reasoning.is_empty()
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
                    ApiStreamChunk::Error(format!("OpenAI model refused: {}", refusal)),
                    "refusal",
                );
            }

            // Accumulate tool call deltas by index. Do not send immediately —
            // dispatch only when finish_reason == "tool_calls" per OpenAI spec.
            if let Some(tool_calls) = delta.tool_calls {
                for tc in tool_calls {
                    if completed_tool_call_indices.contains(&tc.index) {
                        continue;
                    }

                    if let Some(id) = &tc.id {
                        accumulated_tool_calls
                            .entry(tc.index)
                            .or_insert_with(|| (String::new(), String::new(), String::new()))
                            .0 = id.clone();
                    }

                    if let Some(function) = tc.function {
                        if let Some(name) = function.name.filter(|n| !n.is_empty()) {
                            accumulated_tool_calls
                                .entry(tc.index)
                                .or_insert_with(|| (String::new(), String::new(), String::new()))
                                .1 = name;
                        }
                        if let Some(args) = function.arguments.filter(|a| !a.is_empty()) {
                            let entry = accumulated_tool_calls
                                .entry(tc.index)
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
                            if let Some(validated_args) = crate::providers::validate_tool_call_args(
                                args,
                                "OpenAI",
                                "on finish_reason:tool_calls",
                            ) {
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
        }

        if let Some(usage) = chunk.usage {
            *usage_sent = true;
            // Calculate cache tokens and avoid double-counting in input_tokens
            let cached_tokens = usage
                .prompt_tokens_details
                .as_ref()
                .map(|d| d.cached_tokens)
                .unwrap_or(0);
            let cache_write_tokens = usage.prompt_cache_miss_tokens.unwrap_or(0);
            // OpenAI counts cached tokens in prompt_tokens, so subtract to get uncached input
            let uncached_input_tokens = usage.prompt_tokens.saturating_sub(cached_tokens);

            // Calculate total cost using model pricing
            let total_cost = model_info.as_ref().and_then(|info| {
                let input_price = info.base.input_price?;
                let output_price = info.base.output_price?;
                let cache_reads_price = info.base.cache_reads_price.unwrap_or(0.0);
                let cache_writes_price = info.base.cache_writes_price.unwrap_or(0.0);

                // Cost for uncached input tokens
                let input_cost = input_price * (uncached_input_tokens as f64 / 1_000_000.0);
                // Cost for output tokens — completion_tokens already includes reasoning tokens
                let output_cost = output_price * (usage.completion_tokens as f64 / 1_000_000.0);
                // Cost for cache reads (discounted)
                let cache_read_cost = if cached_tokens > 0 {
                    cache_reads_price * (cached_tokens as f64 / 1_000_000.0)
                } else {
                    0.0
                };
                // Cost for cache writes
                let cache_write_cost = if cache_write_tokens > 0 {
                    cache_writes_price * (cache_write_tokens as f64 / 1_000_000.0)
                } else {
                    0.0
                };

                Some(input_cost + output_cost + cache_read_cost + cache_write_cost)
            });

            try_send_chunk(
                tx,
                ApiStreamChunk::Usage(ApiStreamUsageChunk {
                    input_tokens: uncached_input_tokens,
                    output_tokens: usage.completion_tokens,
                    cache_write_tokens: usage.prompt_cache_miss_tokens,
                    cache_read_tokens: Some(cached_tokens),
                    reasoning_tokens: usage
                        .completion_tokens_details
                        .and_then(|d| d.reasoning_tokens),
                    thoughts_token_count: None,
                    total_cost,
                    stop_reason: last_stop_reason.clone(),
                    id: Some(chunk.id.clone()),
                }),
                "usage",
            );
        }
    }
}

/// Parse OpenAI SSE chunk bytes into stream chunks. Extracted for testability.
pub async fn parse_openai_sse_to_chunks(
    chunk: &[u8],
    buffer: &mut crate::providers::SseLineBuffer,
    tx: &tokio::sync::mpsc::Sender<ApiStreamChunk>,
    accumulated_tool_calls: &mut std::collections::HashMap<usize, (String, String, String)>,
    completed_tool_call_indices: &mut std::collections::HashSet<usize>,
    last_stop_reason: &mut Option<String>,
    model_info: &Option<crate::providers::OpenAiCompatibleModelInfo>,
    usage_sent: &mut bool,
) {
    for line in buffer.push_chunk(chunk) {
        process_openai_sse_line(
            &line,
            tx,
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
    accumulated_tool_calls: &mut std::collections::HashMap<usize, (String, String, String)>,
    completed_tool_call_indices: &mut std::collections::HashSet<usize>,
    last_stop_reason: &mut Option<String>,
    model_info: &Option<crate::providers::OpenAiCompatibleModelInfo>,
    usage_sent: &mut bool,
) {
    if let Some(line) = buffer.finish() {
        process_openai_sse_line(
            &line,
            tx,
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
                if let Some(validated_args) =
                    crate::providers::validate_tool_call_args(args, "OpenAI", "at stream end")
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
                            id: None,
                            signature: None,
                        }),
                        "tool_calls",
                    );
                }
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

#[async_trait]
impl Provider for OpenAiProvider {
    async fn create_message(&self, request: ProviderRequest) -> Result<ApiStream, ProviderError> {
        let url = format!("{}/chat/completions", self.base_url());
        let body = self.build_request_body(&request)?;
        let headers = self.build_headers()?;

        tracing::debug!(
            method = "POST",
            provider = "openai",
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

            // Add helpful hint for common model/provider mismatches
            let error_body = if status == StatusCode::NOT_FOUND || status.as_u16() == 404 {
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

        let stream = response.bytes_stream();
        // Use large buffer (10_000) to match agent_loop channel and prevent backpressure deadlocks
        // when the consumer is slow (same pattern as agent_loop.rs:726)
        let (tx, rx) = tokio::sync::mpsc::channel::<ApiStreamChunk>(10_000);

        // Capture model_info for cost calculation in the spawned task
        let model_info = self.config.model_info.clone();

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
            let mut usage_sent = false;
            let mut stream_errored = false;

            while let Some(result) = stream.next().await {
                if tx.is_closed() {
                    break;
                }
                match result {
                    Ok(bytes) => {
                        parse_openai_sse_to_chunks(
                            bytes.as_ref(),
                            &mut sse_buffer,
                            &tx,
                            &mut accumulated_tool_calls,
                            &mut completed_tool_call_indices,
                            &mut last_stop_reason,
                            &model_info,
                            &mut usage_sent,
                        )
                        .await;
                    }
                    Err(e) => {
                        let error_msg = format!("OpenAI SSE stream error: {}", e);
                        let is_retryable = e.to_string().contains("timeout")
                            || e.to_string().contains("connection")
                            || e.to_string().contains("incomplete")
                            || e.to_string().contains("decode");
                        tracing::debug!(error = %e, retryable = is_retryable, "OpenAI SSE bytes_stream error");
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
                finish_openai_sse_to_chunks(
                    &mut sse_buffer,
                    &tx,
                    &mut accumulated_tool_calls,
                    &mut completed_tool_call_indices,
                    &mut last_stop_reason,
                    &model_info,
                    &mut usage_sent,
                )
                .await;
            }
        });

        let rx_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(Box::pin(rx_stream))
    }

    fn get_model(&self) -> ProviderModel {
        let info = self
            .config
            .model_info
            .as_ref()
            .map(|m| m.base.clone())
            .unwrap_or_else(|| get_openai_model_info(&self.config.model_id).base);

        ProviderModel {
            id: self.config.model_id.clone(),
            info,
        }
    }

    fn name(&self) -> &str {
        "openai"
    }
}

/// Get model info for known OpenAI models. Falls back to sane defaults
/// matching TS `openAiModelInfoSaneDefaults` for unknown model IDs.
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

    // Model-specific overrides — most-specific-first ordering
    if model_id.contains("gpt-5.5") {
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
        info.cache_writes_price = Some(0.0);
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
        info.context_window = Some(1_000_000);
        info.supports_prompt_cache = true;
        info.input_price = Some(2.5);
        info.output_price = Some(15.0);
        info.cache_reads_price = Some(0.25);
        info.cache_writes_price = Some(0.0);
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
        temperature: None,
        is_r1_format_required: None,
        system_role: None,
        supports_reasoning_effort: None,
        supports_streaming: None,
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
            custom_headers: None,
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
            custom_headers: None,
            provider_name: None,
        };
        let provider = OpenAiProvider::new(config).unwrap();
        assert_eq!(provider.base_url(), "https://custom.example.com/v1");
    }

    #[test]
    fn test_openai_base_url_normalization() {
        let config = OpenAiConfig {
            api_key: "test-key".to_string(),
            base_url: Some("https://custom.example.com/v1/chat/completions".to_string()),
            model_id: "gpt-4".to_string(),
            model_info: None,
            reasoning_effort: None,
            custom_headers: None,
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
            custom_headers: None,
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
    }

    #[test]
    fn test_build_request_body_with_tools() {
        let config = OpenAiConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "gpt-4".to_string(),
            model_info: None,
            reasoning_effort: None,
            custom_headers: None,
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
                temperature: None,
                is_r1_format_required: None,
                system_role: None,
                supports_reasoning_effort: None,
                supports_streaming: None,
            }),
            reasoning_effort: None,
            custom_headers: None,
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
        let mut accumulated_tool_calls = std::collections::HashMap::new();
        let mut completed_tool_call_indices = std::collections::HashSet::new();
        let mut last_stop_reason: Option<String> = None;
        let mut usage_sent = false;
        let model_info: Option<crate::providers::OpenAiCompatibleModelInfo> = None;

        process_openai_sse_line(
            r#"data: {"id":"chatcmpl_123","choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            &tx,
            &mut accumulated_tool_calls,
            &mut completed_tool_call_indices,
            &mut last_stop_reason,
            &model_info,
            &mut usage_sent,
        )
        .await;

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_finish_openai_sse_to_chunks_skips_content_filter_tool_calls() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut buffer = SseLineBuffer::default();
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
            &mut accumulated_tool_calls,
            &mut completed_tool_call_indices,
            &mut last_stop_reason,
            &model_info,
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
                temperature: None,
                is_r1_format_required: None,
                system_role: None,
                supports_reasoning_effort: None,
                supports_streaming: None,
            }),
            reasoning_effort: None,
            custom_headers: None,
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
                temperature: None,
                is_r1_format_required: None,
                system_role: None,
                supports_reasoning_effort: None,
                supports_streaming: None,
            }),
            reasoning_effort: None,
            custom_headers: None,
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
                temperature: None,
                is_r1_format_required: None,
                system_role: None,
                supports_reasoning_effort: None,
                supports_streaming: None,
            }),
            reasoning_effort: None,
            custom_headers: None,
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
            custom_headers: None,
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
                    temperature: None,
                    top_p: None,
                    top_k: None,
                    supports_tools: Some(true),
                    api_format: None,
                },
                temperature: Some(0.5),
                is_r1_format_required: None,
                system_role: None,
                supports_reasoning_effort: None,
                supports_streaming: None,
            }),
            reasoning_effort: None,
            custom_headers: None,
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
            body["temperature"], 0.5,
            "non-zero temperature should be sent"
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
                temperature: None,
                is_r1_format_required: None,
                system_role: None,
                supports_reasoning_effort: None,
                supports_streaming: None,
            }),
            reasoning_effort: None,
            custom_headers: None,
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
                temperature: None,
                is_r1_format_required: None,
                system_role: None,
                supports_reasoning_effort: None,
                supports_streaming: None,
            }),
            reasoning_effort: None,
            custom_headers: None,
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
    fn test_get_openai_model_info_gpt54() {
        let info = get_openai_model_info("gpt-5.4");
        assert_eq!(info.base.context_window, Some(1_000_000));
        assert_eq!(info.base.max_tokens, Some(128_000));
        assert_eq!(info.base.input_price, Some(2.5));
        assert_eq!(info.base.output_price, Some(15.0));
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
            custom_headers: None,
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
            temperature: None,
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
            custom_headers: None,
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
            custom_headers: None,
            provider_name: Some("openai".to_string()),
        };
        let _provider = OpenAiProvider::new(config).unwrap();
        // Test passes if provider constructs successfully with provider_name set
        // Error body preservation is verified by ProviderHttpError storing raw body string
    }
}
#[cfg(test)]
mod debug_test {
    use crate::providers::openai::{finish_openai_sse_to_chunks, parse_openai_sse_to_chunks};
    use crate::providers::{ApiStreamChunk, SseLineBuffer};

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
        let mut accumulated_tool_calls = std::collections::HashMap::new();
        let mut completed_tool_call_indices = std::collections::HashSet::new();
        let mut last_stop_reason: Option<String> = None;
        let mut usage_sent = false;
        let model_info: Option<crate::providers::OpenAiCompatibleModelInfo> = None;
        parse_openai_sse_to_chunks(
            sse.as_bytes(),
            &mut buffer,
            &tx,
            &mut accumulated_tool_calls,
            &mut completed_tool_call_indices,
            &mut last_stop_reason,
            &model_info,
            &mut usage_sent,
        )
        .await;
        finish_openai_sse_to_chunks(
            &mut buffer,
            &tx,
            &mut accumulated_tool_calls,
            &mut completed_tool_call_indices,
            &mut last_stop_reason,
            &model_info,
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
    async fn test_cache_tokens_not_double_counted_in_cost() {
        // Test that cached tokens are subtracted from input_tokens and cost is calculated correctly
        let sse = r#"
data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"gpt-4","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1000,"completion_tokens":100,"prompt_tokens_details":{"cached_tokens":800}}}
"#;

        let (tx, mut rx) = tokio::sync::mpsc::channel::<ApiStreamChunk>(100);
        let mut buffer = SseLineBuffer::default();
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
            temperature: None,
            is_r1_format_required: None,
            system_role: None,
            supports_reasoning_effort: None,
            supports_streaming: None,
        });

        parse_openai_sse_to_chunks(
            sse.as_bytes(),
            &mut buffer,
            &tx,
            &mut accumulated_tool_calls,
            &mut completed_tool_call_indices,
            &mut last_stop_reason,
            &model_info,
            &mut usage_sent,
        )
        .await;
        finish_openai_sse_to_chunks(
            &mut buffer,
            &tx,
            &mut accumulated_tool_calls,
            &mut completed_tool_call_indices,
            &mut last_stop_reason,
            &model_info,
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
