//! Gemini provider implementation for sned CLI.
//!
//! Ports behavior from `dirac/src/core/api/providers/gemini.ts` and
//! `dirac/src/core/api/transform/gemini-format.ts`.
//!
//! Uses the native Gemini REST API (not OpenAI-compatible layer) for full feature support:
//! - Thought signatures (required for Gemini 3 function calls)
//! - Thinking level/budget configuration
//! - Google Search grounding
//! - Proper tool call format with ID synthesis

use crate::providers::{
    ApiStream, ApiStreamChunk, ApiStreamReasoningChunk, ApiStreamTextChunk, ApiStreamToolCall,
    ApiStreamToolCallFunction, ApiStreamToolCallsChunk, ApiStreamUsageChunk, ModelInfo, ModelTier,
    Provider, ProviderError, ProviderHttpError, ProviderModel, ProviderRequest, SseLineBuffer,
    ThinkingConfig, gemini_format,
};
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc::error::TrySendError;

/// Configuration for the Gemini provider.
#[derive(Debug, Clone)]
pub struct GeminiConfig {
    pub api_key: String,
    pub base_url: Option<String>,
    pub model_id: String,
    pub model_info: Option<ModelInfo>,
    pub thinking_budget_tokens: Option<u32>,
    pub reasoning_effort: Option<String>,
    pub search_enabled: bool,
}

/// Gemini API thinking level enum.
#[derive(Debug, Clone, Copy)]
pub enum GeminiThinkingLevel {
    Minimal,
    Low,
    Medium,
    High,
}

impl GeminiThinkingLevel {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// Map reasoning effort to Gemini thinking level.
fn map_reasoning_effort_to_thinking_level(effort: &str) -> GeminiThinkingLevel {
    match effort.to_lowercase().as_str() {
        "minimal" => GeminiThinkingLevel::Minimal,
        "low" => GeminiThinkingLevel::Low,
        "medium" => GeminiThinkingLevel::Medium,
        "high" | "xhigh" => GeminiThinkingLevel::High,
        _ => GeminiThinkingLevel::Low,
    }
}

/// Gemini provider.
pub struct GeminiProvider {
    config: GeminiConfig,
    client: reqwest::Client,
}

impl GeminiProvider {
    pub fn new(config: GeminiConfig) -> anyhow::Result<Self> {
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
        // Gemini uses x-goog-api-key for auth
        headers.insert(
            "x-goog-api-key",
            HeaderValue::from_str(&self.config.api_key)?,
        );
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));
        Ok(headers)
    }

    fn base_url(&self) -> String {
        self.config
            .base_url
            .as_ref()
            .cloned()
            .filter(|u| !u.is_empty())
            .map(|u| {
                let u = u.trim().trim_end_matches('/');
                u.to_string()
            })
            .unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta".to_owned())
    }

    fn build_request_body(&self, request: &ProviderRequest) -> anyhow::Result<serde_json::Value> {
        let model_id = &self.config.model_id;
        let info = self.config.model_info.as_ref();

        // Convert messages to Gemini format
        let contents = gemini_format::convert_to_gemini_contents(&request.messages);

        // Build generation config
        let mut generation_config = json!({
            "temperature": info.and_then(|i| i.temperature).unwrap_or_else(|| {
                // Default temperatures for Gemini models
                // Per gemini-3.md:497: "For all Gemini 3 models, we strongly recommend
                // keeping the temperature parameter at its default value of 1.0"
                if model_id.contains("gemini-3") {
                    1.0
                } else {
                    0.7
                }
            }),
            "topP": 0.8,
        });

        // Max output tokens
        if let Some(max_tokens) = request
            .max_tokens
            .or_else(|| info.and_then(|i| i.max_tokens))
            .filter(|m| *m > 0)
        {
            // Gemini caps at 32768 for older models, 65536 for Gemini 3
            let max_output = if self.config.model_id.contains("gemini-3") {
                65536
            } else {
                32768
            };
            let capped = max_tokens.min(max_output);
            generation_config["maxOutputTokens"] = json!(capped);
        }

        // Thinking config
        if let Some(thinking_config) = info.and_then(|i| i.thinking_config.as_ref()) {
            let thinking_budget = self.config.thinking_budget_tokens.unwrap_or(0);
            // Gemini 3 Flash defaults to minimal, other Gemini 3 models default to high
            let default_effort = if self.config.model_id.contains("gemini-3-flash") {
                "minimal"
            } else if self.config.model_id.contains("gemini-3") {
                "high"
            } else {
                "low"
            };
            let reasoning_effort = self
                .config
                .reasoning_effort
                .as_deref()
                .unwrap_or(default_effort);

            if thinking_config.supports_thinking_level.unwrap_or(false) {
                // Gemini 3.x models use thinkingLevel
                let level = map_reasoning_effort_to_thinking_level(reasoning_effort);
                generation_config["thinkingConfig"] = json!({
                    "thinkingLevel": level.as_str(),
                    "includeThoughts": true,
                });
            } else if thinking_config.max_budget.unwrap_or(0) > 0 {
                // Gemini 2.5 models use thinkingBudget
                // 0 = disable thinking, -1 = dynamic (default), 128..32768 = specific budget
                let budget: i32 = if thinking_budget == 0 {
                    // User didn't set a budget, use dynamic (default behavior)
                    -1
                } else {
                    thinking_budget.min(thinking_config.max_budget.unwrap_or(24576)) as i32
                };
                generation_config["thinkingConfig"] = json!({
                    "thinkingBudget": budget,
                    "includeThoughts": budget != 0,
                });
            }
        }

        // Build tools
        let mut tools = Vec::new();
        if let Some(tool_defs) = &request.tools {
            let function_declarations: Vec<serde_json::Value> = tool_defs
                .iter()
                .map(|t| {
                    // Gemini doesn't support additionalProperties in schemas
                    // Strip it from the parameters object
                    let mut params = t.function.parameters.clone();
                    if let Some(obj) = params.as_object_mut() {
                        obj.remove("additionalProperties");
                        // Also strip from nested property schemas
                        if let Some(props) =
                            obj.get_mut("properties").and_then(|v| v.as_object_mut())
                        {
                            for (_, prop) in props.iter_mut() {
                                if let Some(prop_obj) = prop.as_object_mut() {
                                    prop_obj.remove("additionalProperties");
                                }
                            }
                        }
                    }
                    json!({
                        "name": t.function.name,
                        "description": t.function.description,
                        "parameters": params,
                    })
                })
                .collect();

            if !function_declarations.is_empty() {
                tools.push(json!({
                    "functionDeclarations": function_declarations,
                }));
            }
        }

        // Google Search grounding (if enabled and not Vertex)
        // includeServerSideToolInvocations must be in generationConfig, not toolConfig
        if self.config.search_enabled {
            generation_config["includeServerSideToolInvocations"] = json!(true);
        }

        let mut body = json!({
            "contents": contents,
            "generationConfig": generation_config,
        });

        // System instruction (required for agent behavior)
        if !request.system_prompt.is_empty() {
            body["systemInstruction"] = json!({
                "parts": [{ "text": &request.system_prompt }]
            });
        }

        if !tools.is_empty() {
            // Tool choice: respect request.tool_choice if provided
            // Gemini format: functionCallingConfig.mode = "AUTO"|"ANY"|"NONE" or "ANY" with allowedFunctionNames
            let tool_choice = request
                .tool_choice
                .as_ref()
                .unwrap_or(&crate::providers::ToolChoice::Auto);
            let function_calling_config = match tool_choice {
                crate::providers::ToolChoice::Auto => json!({"mode": "AUTO"}),
                crate::providers::ToolChoice::Required => json!({"mode": "ANY"}),
                crate::providers::ToolChoice::None => json!({"mode": "NONE"}),
                crate::providers::ToolChoice::Named(name) => {
                    json!({"mode": "ANY", "allowedFunctionNames": [name]})
                }
            };

            let tool_config = json!({
                "functionCallingConfig": function_calling_config,
            });
            body["tools"] = json!(tools);
            body["toolConfig"] = tool_config;
        }

        Ok(body)
    }

    fn stream_url(&self) -> String {
        format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            self.base_url(),
            self.config.model_id
        )
    }
}

/// Gemini SSE response chunk.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiStreamChunk {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    #[serde(default)]
    usage_metadata: Option<GeminiUsageMetadata>,
    #[serde(default)]
    response_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct GeminiCandidate {
    #[serde(default)]
    content: Option<GeminiResponseContent>,
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    grounding_metadata: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Default)]
struct GeminiResponseContent {
    #[serde(default)]
    parts: Vec<GeminiResponsePart>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct GeminiResponsePart {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thought: Option<bool>,
    #[serde(default)]
    thought_signature: Option<String>,
    #[serde(default)]
    function_call: Option<GeminiResponseFunctionCall>,
}

#[derive(Debug, Deserialize)]
struct GeminiResponseFunctionCall {
    name: String,
    #[serde(default)]
    args: Option<serde_json::Value>,
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiUsageMetadata {
    #[serde(default)]
    prompt_token_count: Option<u32>,
    #[serde(default)]
    candidates_token_count: Option<u32>,
    #[serde(default)]
    thoughts_token_count: Option<u32>,
    #[serde(default)]
    cached_content_token_count: Option<u32>,
}

/// Send a stream chunk via try_send to avoid blocking on full channel.
fn try_send_chunk(
    tx: &tokio::sync::mpsc::Sender<ApiStreamChunk>,
    chunk: ApiStreamChunk,
    chunk_type: &str,
) -> bool {
    match tx.try_send(chunk) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            tracing::warn!(
                "Gemini provider channel full, dropping {} chunk",
                chunk_type
            );
            false
        }
        Err(TrySendError::Closed(_)) => {
            tracing::debug!(
                "Gemini provider channel closed, cannot send {} chunk",
                chunk_type
            );
            false
        }
    }
}

async fn process_gemini_sse_line(
    line: &str,
    tx: &tokio::sync::mpsc::Sender<ApiStreamChunk>,
    accumulated_tool_calls: &mut HashMap<String, (String, String, String, Option<String>)>,
    completed_tool_call_ids: &mut HashSet<String>,
    last_stop_reason: &mut Option<String>,
    last_grounding_metadata: &mut Option<serde_json::Value>,
    model_info: &Option<crate::providers::ModelInfo>,
) {
    let line = line.trim();
    if line.is_empty() || line == "data: [DONE]" {
        return;
    }

    let data = line
        .strip_prefix("data:")
        .map(|s| s.strip_prefix(' ').unwrap_or(s))
        .unwrap_or(line);

    let chunk = match serde_json::from_str::<GeminiStreamChunk>(data) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(line = %line, error = %e, "Gemini SSE: failed to parse chunk");
            return;
        }
    };

    let response_id = chunk
        .response_id
        .clone()
        .unwrap_or_else(|| "gemini-response".to_string());

    // Process candidates
    for candidate in chunk.candidates {
        if let Some(content) = candidate.content {
            let mut current_thought_signature: Option<String> = None;

            for part in content.parts {
                let mut emitted_chunk = false;

                // Track thought signature for carry-forward
                if let Some(sig) = &part.thought_signature {
                    current_thought_signature = Some(sig.clone());
                }
                let part_signature = part.thought_signature.clone();
                let signature = part_signature
                    .clone()
                    .or_else(|| current_thought_signature.clone());

                // Handle thinking content
                if part.thought == Some(true) {
                    if let Some(text) = &part.text {
                        try_send_chunk(
                            tx,
                            ApiStreamChunk::Reasoning(ApiStreamReasoningChunk {
                                reasoning: text.clone(),
                                details: None,
                                signature: signature.clone(),
                                redacted_data: None,
                                id: Some(response_id.clone()),
                            }),
                            "reasoning",
                        );
                        emitted_chunk = true;
                    }
                }
                // Handle text content
                else if let Some(text) = &part.text {
                    try_send_chunk(
                        tx,
                        ApiStreamChunk::Text(ApiStreamTextChunk {
                            text: text.clone(),
                            id: Some(response_id.clone()),
                            signature: signature.clone(),
                        }),
                        "text",
                    );
                    emitted_chunk = true;
                }

                // Handle function calls (tool use)
                if let Some(fc) = &part.function_call {
                    // Synthesize ID if missing (Gemini quirk)
                    let call_id = fc.id.clone().unwrap_or_else(|| {
                        format!("{}-tool-{}", response_id, accumulated_tool_calls.len())
                    });

                    if !completed_tool_call_ids.contains(&call_id) {
                        // Enforce MAX_TOOL_ARGUMENT_SIZE — Gemini receives
                        // complete args in one chunk rather than streaming deltas,
                        // but the size limit still applies.
                        let args_str = fc.args.as_ref().map(|a| a.to_string()).unwrap_or_default();
                        let args_str = if args_str.len() <= crate::providers::MAX_TOOL_ARGUMENT_SIZE
                        {
                            args_str
                        } else {
                            tracing::warn!(
                                tool_name = %fc.name,
                                args_size = args_str.len(),
                                "Gemini tool call arguments exceeded MAX_TOOL_ARGUMENT_SIZE, truncating"
                            );
                            let truncated = crate::providers::MAX_TOOL_ARGUMENT_SIZE;
                            let safe_end = args_str.floor_char_boundary(truncated);
                            args_str[..safe_end].to_string()
                        };
                        accumulated_tool_calls.insert(
                            call_id.clone(),
                            (
                                call_id.clone(),
                                fc.name.clone(),
                                args_str,
                                signature.clone(),
                            ),
                        );
                    }
                    emitted_chunk = true;
                }

                // Emit signature-only chunk if we have a signature but no text/function_call
                if part_signature.is_some() && !emitted_chunk && part.text.is_none() {
                    try_send_chunk(
                        tx,
                        ApiStreamChunk::Text(ApiStreamTextChunk {
                            text: String::new(),
                            id: Some(response_id.clone()),
                            signature: part_signature.clone(),
                        }),
                        "signature_only",
                    );
                }

                // Reset carry-forward after functionCall - parallel FCs should NOT inherit signature
                if part.function_call.is_some() {
                    current_thought_signature = None;
                }
            }
        }

        // Track finish reason
        if let Some(finish) = candidate.finish_reason {
            *last_stop_reason = Some(finish.clone());
        }

        // Track grounding metadata
        if let Some(gm) = candidate.grounding_metadata {
            *last_grounding_metadata = Some(gm);
        }
    }

    // Emit tool calls when we have finish_reason or at stream end
    if let Some(finish) = last_stop_reason.as_ref()
        && (finish == "STOP" || finish == "MAX_TOKENS" || finish == "SAFETY")
    {
        for (call_id, (id, name, args, signature)) in accumulated_tool_calls.iter() {
            if !completed_tool_call_ids.contains(call_id) {
                completed_tool_call_ids.insert(call_id.clone());
                try_send_chunk(
                    tx,
                    ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                        tool_call: ApiStreamToolCall {
                            call_id: Some(id.clone()),
                            function: ApiStreamToolCallFunction {
                                id: Some(id.clone()),
                                name: Some(name.clone()),
                                arguments: Some(crate::providers::validate_tool_call_args(
                                    args,
                                    "Gemini",
                                    "on finish",
                                )),
                            },
                            signature: signature.clone(),
                        },
                        id: Some(response_id.clone()),
                        signature: signature.clone(),
                    }),
                    "tool_calls",
                );
            }
        }
    }

    // Handle usage metadata
    if let Some(usage) = chunk.usage_metadata {
        let input_tokens = usage.prompt_token_count.unwrap_or(0);
        let cache_read_tokens = usage.cached_content_token_count;
        let output_tokens = usage.candidates_token_count.unwrap_or(0);
        let thoughts_tokens = usage.thoughts_token_count.unwrap_or(0);

        // Calculate cost using model pricing
        let total_cost = model_info.as_ref().and_then(|info| {
            let mut input_price = info.input_price?;
            let mut output_price = info.output_price?;
            let cache_reads_price = info.cache_reads_price.unwrap_or(0.0);

            // Apply tiered pricing if available
            if let Some(tiers) = &info.tiers
                && let Some(tier) = tiers
                    .iter()
                    .find(|t| input_tokens as u64 <= t.context_window)
            {
                input_price = tier.input_price.unwrap_or(input_price);
                output_price = tier.output_price.unwrap_or(output_price);
            }

            let cache_read = cache_read_tokens.unwrap_or(0);
            let uncached_input = input_tokens.saturating_sub(cache_read) as f64;

            let input_cost = input_price * (uncached_input / 1_000_000.0);
            let output_cost =
                output_price * ((output_tokens + thoughts_tokens) as f64 / 1_000_000.0);
            let cache_cost = if cache_read > 0 {
                cache_reads_price * (cache_read as f64 / 1_000_000.0)
            } else {
                0.0
            };

            Some(input_cost + output_cost + cache_cost)
        });

        try_send_chunk(
            tx,
            ApiStreamChunk::Usage(ApiStreamUsageChunk {
                input_tokens: input_tokens.saturating_sub(cache_read_tokens.unwrap_or(0)),
                output_tokens,
                cache_write_tokens: None,
                cache_read_tokens,
                reasoning_tokens: usage.thoughts_token_count,
                thoughts_token_count: usage.thoughts_token_count,
                total_cost,
                stop_reason: last_stop_reason.clone(),
                id: Some(response_id),
            }),
            "usage",
        );
    }
}

async fn finish_gemini_sse_to_chunks(
    tx: &tokio::sync::mpsc::Sender<ApiStreamChunk>,
    accumulated_tool_calls: &mut HashMap<String, (String, String, String, Option<String>)>,
    completed_tool_call_ids: &mut HashSet<String>,
    _last_stop_reason: &mut Option<String>,
    last_grounding_metadata: &mut Option<serde_json::Value>,
) {
    // Flush accumulated tool calls on stream end
    for (call_id, (id, name, args, signature)) in accumulated_tool_calls.iter() {
        if !completed_tool_call_ids.contains(call_id) {
            completed_tool_call_ids.insert(call_id.clone());
            try_send_chunk(
                tx,
                ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                    tool_call: ApiStreamToolCall {
                        call_id: Some(id.clone()),
                        function: ApiStreamToolCallFunction {
                            id: Some(id.clone()),
                            name: Some(name.clone()),
                            arguments: Some(crate::providers::validate_tool_call_args(
                                args,
                                "Gemini",
                                "at stream end",
                            )),
                        },
                        signature: signature.clone(),
                    },
                    id: None,
                    signature: signature.clone(),
                }),
                "tool_calls",
            );
        }
    }

    // Emit grounding sources if present
    if let Some(gm) = last_grounding_metadata
        && let Some(chunks) = gm.get("groundingChunks").and_then(|v| v.as_array())
        && !chunks.is_empty()
    {
        let mut sources = String::from("\n\n**Sources:**\n");
        for (i, chunk) in chunks.iter().enumerate() {
            if let Some(web) = chunk.get("web") {
                let title = web.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let uri = web.get("uri").and_then(|v| v.as_str()).unwrap_or("");
                if !uri.is_empty() {
                    sources.push_str(&format!("{}. [{}]({})\n", i + 1, title, uri));
                }
            }
        }
        if sources.len() > 17 {
            try_send_chunk(
                tx,
                ApiStreamChunk::Text(ApiStreamTextChunk {
                    text: sources,
                    id: None,
                    signature: None,
                }),
                "grounding_sources",
            );
        }
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    async fn create_message(&self, request: ProviderRequest) -> Result<ApiStream, ProviderError> {
        let url = self.stream_url();
        let body = self.build_request_body(&request)?;
        let headers = self.build_headers()?;

        tracing::debug!(
            method = "POST",
            provider = "gemini",
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
            let resp_headers = response.headers().clone();
            let text = response.text().await.unwrap_or_default();
            return Err(ProviderHttpError::new("Gemini", url, status, text, resp_headers).into());
        }

        let stream = response.bytes_stream();
        let (tx, rx) = tokio::sync::mpsc::channel::<ApiStreamChunk>(10_000);
        let model_info = self.config.model_info.clone();

        tokio::spawn(async move {
            let mut stream = stream;
            let mut sse_buffer = SseLineBuffer::default();
            let mut accumulated_tool_calls: HashMap<
                String,
                (String, String, String, Option<String>),
            > = HashMap::with_capacity(4);
            let mut completed_tool_call_ids: HashSet<String> = HashSet::new();
            let mut last_stop_reason: Option<String> = None;
            let mut stream_errored = false;
            let mut last_grounding_metadata: Option<serde_json::Value> = None;

            while let Some(result) = stream.next().await {
                if tx.is_closed() {
                    break;
                }
                match result {
                    Ok(bytes) => {
                        for line in sse_buffer.push_chunk(&bytes) {
                            process_gemini_sse_line(
                                &line,
                                &tx,
                                &mut accumulated_tool_calls,
                                &mut completed_tool_call_ids,
                                &mut last_stop_reason,
                                &mut last_grounding_metadata,
                                &model_info,
                            )
                            .await;
                        }
                        if let Some(err) = sse_buffer.take_error() {
                            try_send_chunk(&tx, ApiStreamChunk::Error(err), "error");
                        }
                    }
                    Err(e) => {
                        let error_msg = format!("Gemini SSE stream error: {}", e);
                        let is_retryable = e.to_string().contains("timeout")
                            || e.to_string().contains("connection")
                            || e.to_string().contains("incomplete")
                            || e.to_string().contains("decode");
                        tracing::debug!(error = %e, retryable = is_retryable, "Gemini SSE bytes_stream error");
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
                finish_gemini_sse_to_chunks(
                    &tx,
                    &mut accumulated_tool_calls,
                    &mut completed_tool_call_ids,
                    &mut last_stop_reason,
                    &mut last_grounding_metadata,
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
            .cloned()
            .unwrap_or_else(|| get_gemini_model_info(&self.config.model_id));

        ProviderModel {
            id: self.config.model_id.clone(),
            info,
        }
    }

    fn name(&self) -> &str {
        "gemini"
    }
}

/// Get model info for Gemini models.
fn get_gemini_model_info(model_id: &str) -> ModelInfo {
    // Default model info for unknown Gemini models
    let mut info = ModelInfo {
        name: Some(model_id.to_string()),
        max_tokens: Some(65536),
        context_window: Some(1048576),
        supports_images: Some(true),
        supports_prompt_cache: true,
        supports_reasoning: Some(true),
        input_price: Some(4.0),
        output_price: Some(18.0),
        image_output_price: None,
        thinking_config: Some(ThinkingConfig {
            max_budget: None,
            output_price: None,
            output_price_tiers: None,
            gemini_thinking_level: Some("high".to_string()),
            supports_thinking_level: Some(true),
        }),
        supports_global_endpoint: Some(true),
        cache_writes_price: Some(4.0),
        cache_reads_price: Some(0.4),
        description: None,
        tiers: None,
        temperature: None,
        supports_tools: Some(true),
        api_format: Some("gemini".to_string()),
    };

    // Model-specific overrides - most-specific-first ordering
    if model_id.contains("gemini-3.1-flash-image") {
        // 128k context, $0.25 input, $0.067/image output
        info.context_window = Some(128_000);
        info.input_price = Some(0.25);
        info.output_price = Some(0.067);
    } else if model_id.contains("gemini-3-pro-image") {
        // 65k context, $2 input, $0.134/image output
        info.context_window = Some(65_000);
        info.input_price = Some(2.0);
        info.output_price = Some(0.134);
    } else if model_id.contains("gemini-3.1-flash-lite") {
        // 1M context, $0.25 input, $1.50 output, cache_reads_price: 0.05
        info.context_window = Some(1_048_576);
        info.input_price = Some(0.25);
        info.output_price = Some(1.50);
        info.cache_reads_price = Some(0.05);
        info.thinking_config = Some(ThinkingConfig {
            max_budget: None,
            output_price: None,
            output_price_tiers: None,
            gemini_thinking_level: Some("minimal".to_string()),
            supports_thinking_level: Some(true),
        });
    } else if model_id.contains("gemini-3.1-pro") {
        // 1M context, tiered pricing: <$200k = $2/$12, >$200k = $4/$18
        info.context_window = Some(1_048_576);
        info.tiers = Some(vec![
            ModelTier {
                context_window: 200_000,
                input_price: Some(2.0),
                output_price: Some(12.0),
                cache_writes_price: Some(2.0),
                cache_reads_price: Some(0.2),
            },
            ModelTier {
                context_window: u64::MAX,
                input_price: Some(4.0),
                output_price: Some(18.0),
                cache_writes_price: Some(4.0),
                cache_reads_price: Some(0.4),
            },
        ]);
        info.temperature = Some(1.0);
    } else if model_id.contains("gemini-3-pro") {
        // 1M context, tiered pricing: <$200k = $2/$12, >$200k = $4/$18
        info.context_window = Some(1_048_576);
        info.tiers = Some(vec![
            ModelTier {
                context_window: 200_000,
                input_price: Some(2.0),
                output_price: Some(12.0),
                cache_writes_price: Some(2.0),
                cache_reads_price: Some(0.2),
            },
            ModelTier {
                context_window: u64::MAX,
                input_price: Some(4.0),
                output_price: Some(18.0),
                cache_writes_price: Some(4.0),
                cache_reads_price: Some(0.4),
            },
        ]);
        info.temperature = Some(1.0);
    } else if model_id.contains("gemini-3-flash") {
        // 1M context, $0.50 input, $3.00 output, cache_reads_price: 0.05
        info.context_window = Some(1_048_576);
        info.input_price = Some(0.50);
        info.output_price = Some(3.00);
        info.cache_reads_price = Some(0.05);
        info.temperature = Some(1.0);
        // Gemini 3 Flash defaults to minimal thinking level per docs
        info.thinking_config = Some(ThinkingConfig {
            max_budget: None,
            output_price: None,
            output_price_tiers: None,
            gemini_thinking_level: Some("minimal".to_string()),
            supports_thinking_level: Some(true),
        });
    } else if model_id.contains("gemini-2.5-pro") {
        info.context_window = Some(1048576);
        info.input_price = Some(2.5);
        info.output_price = Some(15.0);
        info.thinking_config = Some(ThinkingConfig {
            max_budget: Some(32767),
            output_price: Some(15.0),
            output_price_tiers: None,
            gemini_thinking_level: None,
            supports_thinking_level: Some(false),
        });
    } else if model_id.contains("gemini-2.5-flash-lite") {
        info.context_window = Some(1048576);
        info.input_price = Some(0.1);
        info.output_price = Some(0.4);
        info.thinking_config = Some(ThinkingConfig {
            max_budget: Some(24576),
            output_price: None,
            output_price_tiers: None,
            gemini_thinking_level: None,
            supports_thinking_level: Some(false),
        });
    } else if model_id.contains("gemini-2.5-flash") {
        info.context_window = Some(1048576);
        info.input_price = Some(0.3);
        info.output_price = Some(2.5);
        info.thinking_config = Some(ThinkingConfig {
            max_budget: Some(24576),
            output_price: Some(3.5),
            output_price_tiers: None,
            gemini_thinking_level: None,
            supports_thinking_level: Some(false),
        });
    }

    info
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{FunctionDefinition, MessageRole, StorageMessage, ToolDefinition};

    #[test]
    fn test_gemini_config() {
        let config = GeminiConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "gemini-3.1-pro-preview".to_string(),
            model_info: None,
            thinking_budget_tokens: None,
            reasoning_effort: None,
            search_enabled: false,
        };
        let provider = GeminiProvider::new(config).unwrap();
        assert_eq!(
            provider.base_url(),
            "https://generativelanguage.googleapis.com/v1beta"
        );
    }

    #[test]
    fn test_gemini_custom_base_url() {
        let config = GeminiConfig {
            api_key: "test-key".to_string(),
            base_url: Some("https://custom.example.com/v1/".to_string()),
            model_id: "gemini-3.1-pro-preview".to_string(),
            model_info: None,
            thinking_budget_tokens: None,
            reasoning_effort: None,
            search_enabled: false,
        };
        let provider = GeminiProvider::new(config).unwrap();
        assert_eq!(provider.base_url(), "https://custom.example.com/v1");
    }

    #[test]
    fn test_build_request_body_basic() {
        let config = GeminiConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "gemini-3.1-pro-preview".to_string(),
            model_info: None,
            thinking_budget_tokens: None,
            reasoning_effort: None,
            search_enabled: false,
        };
        let provider = GeminiProvider::new(config).unwrap();

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
        assert!(body.get("contents").is_some());
        assert!(body.get("generationConfig").is_some());
        assert!(body.get("systemInstruction").is_some());
        assert_eq!(
            body["systemInstruction"]["parts"][0]["text"],
            "You are a helpful assistant."
        );
    }

    #[test]
    fn test_build_request_body_with_tools() {
        let config = GeminiConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "gemini-3.1-pro-preview".to_string(),
            model_info: None,
            thinking_budget_tokens: None,
            reasoning_effort: None,
            search_enabled: false,
        };
        let provider = GeminiProvider::new(config).unwrap();

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
        let func = &tools[0]["functionDeclarations"].as_array().unwrap()[0];
        assert_eq!(func["name"], "read_file");
    }

    #[test]
    fn test_build_request_body_strips_additional_properties() {
        // Gemini API rejects additionalProperties in tool parameter schemas
        let config = GeminiConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            model_id: "gemini-3-flash-preview".to_string(),
            model_info: None,
            thinking_budget_tokens: None,
            reasoning_effort: None,
            search_enabled: false,
        };
        let provider = GeminiProvider::new(config).unwrap();

        let request = ProviderRequest {
            system_prompt: "You are a helpful assistant.".to_string(),
            messages: vec![],
            tools: Some(vec![ToolDefinition {
                tool_type: "function".to_string(),
                function: FunctionDefinition {
                    name: "test_tool".to_string(),
                    description: "A test tool".to_string(),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "File path"
                            }
                        },
                        "required": ["path"],
                        "additionalProperties": false,
                    }),
                },
            }]),
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let body = provider.build_request_body(&request).unwrap();
        let tools = body["tools"].as_array().unwrap();
        let func = &tools[0]["functionDeclarations"].as_array().unwrap()[0];
        let params = &func["parameters"];

        // Verify additionalProperties was stripped
        assert!(
            params.get("additionalProperties").is_none(),
            "additionalProperties should be stripped for Gemini"
        );
        // Verify other fields are preserved
        assert_eq!(params["type"], "object");
        assert!(params["properties"].is_object());
        assert_eq!(params["required"], json!(["path"]));
    }

    #[test]
    fn test_map_reasoning_effort() {
        assert_eq!(
            map_reasoning_effort_to_thinking_level("low").as_str(),
            "low"
        );
        assert_eq!(
            map_reasoning_effort_to_thinking_level("medium").as_str(),
            "medium"
        );
        assert_eq!(
            map_reasoning_effort_to_thinking_level("high").as_str(),
            "high"
        );
        assert_eq!(
            map_reasoning_effort_to_thinking_level("xhigh").as_str(),
            "high"
        );
        assert_eq!(
            map_reasoning_effort_to_thinking_level("unknown").as_str(),
            "low"
        );
        assert_eq!(
            map_reasoning_effort_to_thinking_level("minimal").as_str(),
            "minimal"
        );
    }

    #[test]
    fn test_get_gemini_model_info() {
        let info_pro = get_gemini_model_info("gemini-3.1-pro-preview");
        assert_eq!(info_pro.context_window, Some(1048576));
        assert_eq!(info_pro.temperature, Some(1.0));
        assert_eq!(
            info_pro
                .thinking_config
                .as_ref()
                .unwrap()
                .gemini_thinking_level,
            Some("high".to_string())
        );

        let info_flash = get_gemini_model_info("gemini-3-flash-preview");
        assert_eq!(info_flash.temperature, Some(1.0));
        // Gemini 3 Flash should default to minimal thinking level
        assert_eq!(
            info_flash
                .thinking_config
                .as_ref()
                .unwrap()
                .gemini_thinking_level,
            Some("minimal".to_string())
        );

        let info_25_pro = get_gemini_model_info("gemini-2.5-pro");
        assert_eq!(
            info_25_pro.thinking_config.as_ref().unwrap().max_budget,
            Some(32767)
        );
    }

    // ============== Model Ordering Regression Tests ==============
    // These tests verify that all Gemini models resolve to correct pricing.
    // The order of contains() checks in get_gemini_model_info() matters:
    // more-specific patterns (e.g., "flash-lite") must come before less-specific
    // ones (e.g., "flash") to avoid incorrect matches.

    #[test]
    fn test_gemini_model_pricing_gemini_3_1_flash_image() {
        let info = get_gemini_model_info("gemini-3.1-flash-image");
        assert_eq!(info.context_window, Some(128_000));
        assert_eq!(info.input_price, Some(0.25));
        assert_eq!(info.output_price, Some(0.067));
    }

    #[test]
    fn test_gemini_model_pricing_gemini_3_pro_image() {
        let info = get_gemini_model_info("gemini-3-pro-image");
        assert_eq!(info.context_window, Some(65_000));
        assert_eq!(info.input_price, Some(2.0));
        assert_eq!(info.output_price, Some(0.134));
    }

    #[test]
    fn test_gemini_model_pricing_gemini_3_1_flash_lite() {
        let info = get_gemini_model_info("gemini-3.1-flash-lite");
        assert_eq!(info.context_window, Some(1_048_576));
        assert_eq!(info.input_price, Some(0.25));
        assert_eq!(info.output_price, Some(1.50));
        assert_eq!(info.cache_reads_price, Some(0.05));
        assert_eq!(
            info.thinking_config.as_ref().unwrap().gemini_thinking_level,
            Some("minimal".to_string())
        );
    }

    #[test]
    fn test_gemini_model_pricing_gemini_3_1_pro() {
        let info = get_gemini_model_info("gemini-3.1-pro");
        assert_eq!(info.context_window, Some(1_048_576));
        assert_eq!(info.temperature, Some(1.0));
        assert!(info.tiers.is_some());
        let tiers = info.tiers.unwrap();
        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers[0].input_price, Some(2.0));
        assert_eq!(tiers[0].output_price, Some(12.0));
    }

    #[test]
    fn test_gemini_model_pricing_gemini_3_pro() {
        let info = get_gemini_model_info("gemini-3-pro");
        assert_eq!(info.context_window, Some(1_048_576));
        assert_eq!(info.temperature, Some(1.0));
        assert!(info.tiers.is_some());
        let tiers = info.tiers.unwrap();
        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers[0].input_price, Some(2.0));
        assert_eq!(tiers[0].output_price, Some(12.0));
    }

    #[test]
    fn test_gemini_model_pricing_gemini_3_flash() {
        let info = get_gemini_model_info("gemini-3-flash");
        assert_eq!(info.context_window, Some(1_048_576));
        assert_eq!(info.input_price, Some(0.50));
        assert_eq!(info.output_price, Some(3.00));
        assert_eq!(info.cache_reads_price, Some(0.05));
        assert_eq!(
            info.thinking_config.as_ref().unwrap().gemini_thinking_level,
            Some("minimal".to_string())
        );
    }

    #[test]
    fn test_gemini_model_pricing_gemini_2_5_pro() {
        let info = get_gemini_model_info("gemini-2.5-pro");
        assert_eq!(info.context_window, Some(1_048_576));
        assert_eq!(info.input_price, Some(2.5));
        assert_eq!(info.output_price, Some(15.0));
        assert_eq!(
            info.thinking_config.as_ref().unwrap().max_budget,
            Some(32767)
        );
    }

    #[test]
    fn test_gemini_model_pricing_gemini_2_5_flash() {
        let info = get_gemini_model_info("gemini-2.5-flash");
        assert_eq!(info.context_window, Some(1_048_576));
        assert_eq!(info.input_price, Some(0.3));
        assert_eq!(info.output_price, Some(2.5));
        assert_eq!(
            info.thinking_config.as_ref().unwrap().max_budget,
            Some(24576)
        );
    }

    #[test]
    fn test_gemini_model_pricing_gemini_2_5_flash_lite() {
        let info = get_gemini_model_info("gemini-2.5-flash-lite");
        assert_eq!(info.context_window, Some(1_048_576));
        assert_eq!(info.input_price, Some(0.1));
        assert_eq!(info.output_price, Some(0.4));
        assert_eq!(
            info.thinking_config.as_ref().unwrap().max_budget,
            Some(24576)
        );
    }

    #[test]
    fn test_gemini_model_ordering_flash_vs_flash_lite() {
        // Regression test: verify "flash-lite" models don't match "flash" pricing
        let flash_lite = get_gemini_model_info("gemini-2.5-flash-lite");
        let flash = get_gemini_model_info("gemini-2.5-flash");

        // flash-lite should have different (lower) pricing than flash
        assert_eq!(flash_lite.input_price, Some(0.1));
        assert_eq!(flash_lite.output_price, Some(0.4));
        assert_eq!(flash.input_price, Some(0.3));
        assert_eq!(flash.output_price, Some(2.5));

        // Verify they are not the same (would indicate ordering bug)
        assert_ne!(flash_lite.input_price, flash.input_price);
        assert_ne!(flash_lite.output_price, flash.output_price);
    }

    #[test]
    fn test_gemini_model_ordering_3_1_flash_lite_vs_3_flash() {
        // Regression test: verify "gemini-3.1-flash-lite" doesn't match "gemini-3-flash"
        let lite = get_gemini_model_info("gemini-3.1-flash-lite");
        let flash = get_gemini_model_info("gemini-3-flash");

        assert_eq!(lite.input_price, Some(0.25));
        assert_eq!(lite.output_price, Some(1.50));
        assert_eq!(flash.input_price, Some(0.50));
        assert_eq!(flash.output_price, Some(3.00));

        assert_ne!(lite.input_price, flash.input_price);
        assert_ne!(lite.output_price, flash.output_price);
    }

    #[test]
    fn test_gemini_model_ordering_3_1_pro_vs_3_pro() {
        // Regression test: verify "gemini-3.1-pro" doesn't match "gemini-3-pro"
        let pro_3_1 = get_gemini_model_info("gemini-3.1-pro");
        let pro_3 = get_gemini_model_info("gemini-3-pro");

        // Both should have tiers, but verify they're matched correctly
        assert!(pro_3_1.tiers.is_some());
        assert!(pro_3.tiers.is_some());
        // Both should have temperature 1.0
        assert_eq!(pro_3_1.temperature, Some(1.0));
        assert_eq!(pro_3.temperature, Some(1.0));
    }
}
