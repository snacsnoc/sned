//! Effect-free provider-stream interpretation for a single attempt.
//!
//! The accumulator owns stream interpretation; the agent loop owns effects
//! (provider calls, retries, cancellation, output, timing, `TaskState`).
//! `tracing!` logging stays here so the observable log stream does not move.

use std::collections::{HashMap, HashSet};

use crate::core::context::ApiReqInfo;
use crate::core::stream_parsing::ThinkingTagStreamFilter;
use crate::providers::{
    ApiStreamChunk, ApiStreamToolCall, ApiStreamUsageChunk, MAX_TOOL_ARGUMENT_SIZE,
};

/// Provider data captured up front so the accumulator never touches
/// the provider itself.
#[derive(Debug, Clone)]
pub struct StreamProviderInfo {
    pub provider_name: String,
    pub context_window: u64,
}

/// Interpretation results the loop cannot derive itself. Counters and
/// first-chunk timing stay in the loop next to the state they update.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    VisibleText(String),
    ReasoningText(String),
    PrepareToolCall {
        call_id: String,
        name: String,
    },
    ToolCallReceived,
    UsageUpdated {
        usage: ApiReqInfo,
        deltas: UsageDeltas,
    },
    /// Snapshot at arrival: the loop's immediate output decision cannot
    /// wait for stream end, and the flag only grows from here.
    StreamError {
        error: String,
        retryable: bool,
        substantive_output: bool,
    },
}

/// Only positive raw values accumulate, so resolved-with-fallback
/// totals never leak into the cumulative counters.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UsageDeltas {
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub cache_writes: u32,
    pub cache_reads: u32,
    pub reasoning_tokens: u32,
    pub cost: f64,
}

/// Order and map stay paired: dispatch walks the order, then looks up
/// each call by id.
#[derive(Debug, Clone, Default)]
pub struct StreamOutcome {
    pub text: String,
    pub filtered_text: String,
    /// Tail flushed by `finish`; the loop's final display pass needs it
    /// separately from the full filtered text.
    pub filtered_tail: String,
    pub leaked_thinking: Option<String>,
    pub reasoning: String,
    pub signature: Option<String>,
    pub text_signature: Option<String>,
    pub redacted_data: Vec<String>,
    pub tool_call_order: Vec<String>,
    pub tool_calls: HashMap<String, ApiStreamToolCall>,
    pub usage: Option<ApiReqInfo>,
    pub substantive_output: bool,
    pub errored: bool,
    pub retryable_error_before_output: Option<String>,
    pub non_retryable_error: Option<String>,
}

/// Incremental interpreter for one provider-stream attempt.
///
/// Construct fresh per attempt: all state here describes the attempt in
/// flight, never shared turn state.
#[derive(Debug)]
pub struct StreamAccumulator {
    filter: ThinkingTagStreamFilter,
    accumulated_text: String,
    filtered_text: String,
    accumulated_reasoning: String,
    accumulated_signature: Option<String>,
    accumulated_text_signature: Option<String>,
    accumulated_redacted_data: Vec<String>,
    tool_calls_map: HashMap<String, ApiStreamToolCall>,
    tool_call_order: Vec<String>,
    announced_tool_call_ids: HashSet<String>,
    substantive_output: bool,
    stream_errored: bool,
    retryable_error_before_output: Option<String>,
    non_retryable_error: Option<String>,
    stream_usage: Option<ApiReqInfo>,
    json_output: bool,
    provider_name: String,
    context_window: u64,
}

impl StreamAccumulator {
    pub fn new(json_output: bool, provider: StreamProviderInfo) -> Self {
        Self {
            filter: ThinkingTagStreamFilter::new(),
            accumulated_text: String::new(),
            filtered_text: String::new(),
            accumulated_reasoning: String::new(),
            accumulated_signature: None,
            accumulated_text_signature: None,
            accumulated_redacted_data: Vec::new(),
            tool_calls_map: HashMap::with_capacity(4),
            tool_call_order: Vec::new(),
            announced_tool_call_ids: HashSet::new(),
            substantive_output: false,
            stream_errored: false,
            retryable_error_before_output: None,
            non_retryable_error: None,
            stream_usage: None,
            json_output,
            provider_name: provider.provider_name,
            context_window: provider.context_window,
        }
    }

    /// Pre-output failure is decided by loop orchestration; recorded here
    /// so the retry decision sees it alongside chunk errors.
    pub fn note_preoutput_failure(&mut self, message: String) {
        self.retryable_error_before_output = Some(message);
    }

    pub fn push(&mut self, chunk: &ApiStreamChunk) -> Vec<StreamEvent> {
        match chunk {
            ApiStreamChunk::Text(text_chunk) => self.push_text(text_chunk),
            ApiStreamChunk::Reasoning(reasoning_chunk) => self.push_reasoning(reasoning_chunk),
            ApiStreamChunk::ToolCallStarted { call_id, name } => {
                self.push_tool_call_started(call_id, name)
            }
            ApiStreamChunk::ToolCalls(tool_chunk) => self.push_tool_calls(tool_chunk),
            ApiStreamChunk::Usage(usage_chunk) => self.push_usage(usage_chunk),
            ApiStreamChunk::Timing(_) => Vec::new(),
            ApiStreamChunk::Error(err) => self.push_error(err),
        }
    }

    pub fn finish(mut self) -> StreamOutcome {
        let filtered_tail = self.filter.finish();
        self.filtered_text.push_str(&filtered_tail);
        let leaked_thinking = self.filter.take_hidden();
        StreamOutcome {
            text: self.accumulated_text,
            filtered_text: self.filtered_text,
            filtered_tail,
            leaked_thinking,
            reasoning: self.accumulated_reasoning,
            signature: self.accumulated_signature,
            text_signature: self.accumulated_text_signature,
            redacted_data: self.accumulated_redacted_data,
            tool_call_order: self.tool_call_order,
            tool_calls: self.tool_calls_map,
            usage: self.stream_usage,
            substantive_output: self.substantive_output,
            errored: self.stream_errored,
            retryable_error_before_output: self.retryable_error_before_output,
            non_retryable_error: self.non_retryable_error,
        }
    }

    fn push_text(&mut self, text_chunk: &crate::providers::ApiStreamTextChunk) -> Vec<StreamEvent> {
        tracing::debug!(text = %text_chunk.text, "received text chunk");
        let processed = self.filter.push(&text_chunk.text);
        self.filtered_text.push_str(&processed);
        if self.json_output {
            self.substantive_output |= !text_chunk.text.is_empty();
            tracing::info!(
                target: "json_output",
                "{}",
                serde_json::json!({
                    "type": "text",
                    "text": text_chunk.text
                })
                .to_string()
            );
        } else if !processed.is_empty() {
            self.substantive_output = true;
        }
        if text_chunk.signature.is_some() {
            self.accumulated_text_signature
                .clone_from(&text_chunk.signature);
        }
        self.accumulated_text.push_str(&text_chunk.text);
        vec![StreamEvent::VisibleText(processed)]
    }

    fn push_reasoning(
        &mut self,
        reasoning_chunk: &crate::providers::ApiStreamReasoningChunk,
    ) -> Vec<StreamEvent> {
        self.substantive_output |= !reasoning_chunk.reasoning.is_empty();
        if self.json_output {
            tracing::info!(
                target: "json_output",
                "{}",
                serde_json::json!({
                    "type": "reasoning",
                    "reasoning": reasoning_chunk.reasoning,
                    "signature": reasoning_chunk.signature,
                    "redacted_data": reasoning_chunk.redacted_data,
                })
                .to_string()
            );
        }
        self.accumulated_reasoning
            .push_str(&reasoning_chunk.reasoning);
        if reasoning_chunk.signature.is_some() {
            self.accumulated_signature
                .clone_from(&reasoning_chunk.signature);
        }
        if let Some(redacted_data) = reasoning_chunk.redacted_data.clone() {
            self.accumulated_redacted_data.push(redacted_data);
        }
        vec![StreamEvent::ReasoningText(
            reasoning_chunk.reasoning.clone(),
        )]
    }

    fn push_tool_call_started(&mut self, call_id: &str, name: &str) -> Vec<StreamEvent> {
        if self.announced_tool_call_ids.insert(call_id.to_string()) {
            return vec![StreamEvent::PrepareToolCall {
                call_id: call_id.to_string(),
                name: name.to_string(),
            }];
        }
        Vec::new()
    }

    fn push_tool_calls(
        &mut self,
        tool_chunk: &crate::providers::ApiStreamToolCallsChunk,
    ) -> Vec<StreamEvent> {
        self.substantive_output = true;
        let tc = tool_chunk.tool_call.clone();
        let key = tc
            .call_id
            .clone()
            .unwrap_or_else(|| tc.function.id.clone().unwrap_or_default());
        // Prevent empty-key collisions when provider sends tool calls without IDs.
        // Two calls both keyed by "" would overwrite each other in tool_calls_map.
        let key = if key.is_empty() {
            ulid::Ulid::new().to_string()
        } else {
            key
        };
        tracing::info!(
            tool_name = ?tc.function.name,
            tool_id = ?key,
            has_args = tc.function.arguments.is_some(),
            "received tool call from stream"
        );
        if self.json_output {
            tracing::info!(
                target: "json_output",
                "{}",
                serde_json::json!({
                    "type": "tool_calls",
                    "tool_call": {
                        "call_id": tc.call_id,
                        "function": {
                            "id": tc.function.id,
                            "name": tc.function.name,
                            "arguments": tc.function.arguments,
                        }
                    },
                    "id": tool_chunk.id,
                    "signature": tool_chunk.signature,
                })
                .to_string()
            );
        }
        // Allow partial tool call deltas with arguments even when name is missing.
        // Provider may send name in a later chunk; merge logic assembles complete call.
        let args_absent = tc.function.arguments.is_none()
            || tc
                .function
                .arguments
                .as_ref()
                .is_some_and(std::string::String::is_empty);
        if (tc.function.name.is_none()
            || tc
                .function
                .name
                .as_ref()
                .is_some_and(std::string::String::is_empty))
            && args_absent
        {
            tracing::warn!("received tool call with empty name and no arguments, skipping");
            return Vec::new();
        }
        // Merge partial tool call chunks by ID using HashMap for O(1) lookup (P4)
        // Preserve insertion order via tool_call_order vec
        if let Some(existing) = self.tool_calls_map.get_mut(&key) {
            if let Some(new_args) = tc.function.arguments
                && !new_args.is_empty()
            {
                let merged = existing
                    .function
                    .arguments
                    .as_ref()
                    .map(|a| a.clone() + &new_args)
                    .unwrap_or(new_args);
                // Oversized merges stay stored as-is so the stream
                // keeps draining; parse rejects them before
                // dispatch instead of repairing them into
                // executable calls.
                if merged.len() > MAX_TOOL_ARGUMENT_SIZE {
                    tracing::warn!(
                        args_len = merged.len(),
                        "merged tool call arguments exceed size limit; call will not execute"
                    );
                }
                existing.function.arguments = Some(merged);
            }
            if tc.function.name.is_some() {
                existing.function.name = tc.function.name;
            }
            if tc.call_id.is_some() {
                existing.call_id = tc.call_id;
            }
        } else {
            if let Some(ref args) = tc.function.arguments
                && args.len() > MAX_TOOL_ARGUMENT_SIZE
            {
                tracing::warn!(
                    args_len = args.len(),
                    "tool call arguments exceed size limit; call will not execute"
                );
            }
            self.tool_call_order.push(key.clone());
            self.tool_calls_map.insert(key, tc);
        }
        vec![StreamEvent::ToolCallReceived]
    }

    fn push_usage(&mut self, usage_chunk: &ApiStreamUsageChunk) -> Vec<StreamEvent> {
        if self.json_output {
            tracing::info!(
                target: "json_output",
                "{}",
                serde_json::json!({
                    "type": "usage",
                    "input_tokens": usage_chunk.input_tokens,
                    "output_tokens": usage_chunk.output_tokens,
                    "cache_write_tokens": usage_chunk.cache_write_tokens,
                    "cache_read_tokens": usage_chunk.cache_read_tokens,
                    "reasoning_tokens": usage_chunk.reasoning_tokens,
                    "total_cost": usage_chunk.total_cost,
                    "stop_reason": usage_chunk.stop_reason,
                    "id": usage_chunk.id,
                })
                .to_string()
            );
        }
        let is_synthetic_empty_usage = usage_chunk.input_tokens == 0
            && usage_chunk.output_tokens == 0
            && usage_chunk.cache_write_tokens == Some(0)
            && usage_chunk.cache_read_tokens.is_none()
            && usage_chunk.reasoning_tokens.is_none()
            && usage_chunk.total_cost.is_none()
            && usage_chunk.id.is_none();
        if is_synthetic_empty_usage {
            // Keep the last measured usage when this provider
            // response has no usage data. Do not replace it
            // with a fabricated zero or an estimate.
            return Vec::new();
        }
        let prev_info = self.stream_usage.as_ref();
        let tokens_in = if usage_chunk.input_tokens > 0 {
            usage_chunk.input_tokens
        } else {
            prev_info.and_then(|r| r.tokens_in).unwrap_or(0)
        };
        let tokens_out = if usage_chunk.output_tokens > 0 {
            usage_chunk.output_tokens
        } else {
            prev_info.and_then(|r| r.tokens_out).unwrap_or(0)
        };
        let cache_writes = usage_chunk
            .cache_write_tokens
            .or_else(|| prev_info.and_then(|r| r.cache_writes));
        let cache_reads = usage_chunk
            .cache_read_tokens
            .or_else(|| prev_info.and_then(|r| r.cache_reads));
        let reasoning_tokens = usage_chunk
            .reasoning_tokens
            .or_else(|| prev_info.and_then(|r| r.reasoning_tokens));
        // Gemini marks thinking tokens separately from candidate output;
        // OpenAI-compatible providers include reasoning in completion_tokens.
        let context_output_tokens = if usage_chunk.thoughts_token_count.is_some() {
            tokens_out.saturating_add(reasoning_tokens.unwrap_or(0))
        } else {
            tokens_out
        };
        let context_tokens = crate::core::context::context_window::calculate_context_tokens(
            tokens_in,
            context_output_tokens,
            cache_writes,
            cache_reads,
            &self.provider_name,
        );
        let context_usage_pct =
            crate::core::context::context_window::calculate_context_usage_percentage(
                tokens_in,
                context_output_tokens,
                cache_writes,
                cache_reads,
                self.context_window,
                &self.provider_name,
            );
        let usage = ApiReqInfo {
            request: None,
            tokens_in: Some(tokens_in),
            tokens_out: Some(tokens_out),
            cache_writes,
            cache_reads,
            reasoning_tokens,
            context_tokens: Some(context_tokens),
            cost: usage_chunk
                .total_cost
                .or_else(|| prev_info.and_then(|r| r.cost)),
            context_window: Some(self.context_window),
            context_usage_percentage: Some(context_usage_pct),
        };
        self.stream_usage = Some(usage.clone());
        let deltas = UsageDeltas {
            tokens_in: if usage_chunk.input_tokens > 0 {
                usage_chunk.input_tokens
            } else {
                0
            },
            tokens_out: if usage_chunk.output_tokens > 0 {
                usage_chunk.output_tokens
            } else {
                0
            },
            cache_writes: usage_chunk.cache_write_tokens.unwrap_or(0),
            cache_reads: usage_chunk.cache_read_tokens.unwrap_or(0),
            reasoning_tokens: usage_chunk.reasoning_tokens.unwrap_or(0),
            cost: usage_chunk.total_cost.unwrap_or(0.0),
        };
        vec![StreamEvent::UsageUpdated { usage, deltas }]
    }

    fn push_error(&mut self, err: &str) -> Vec<StreamEvent> {
        tracing::error!(error = %err, "received error chunk from provider stream");
        let retryable = stream_error_is_retryable(err);
        if !retryable {
            self.stream_errored = true;
            if self.non_retryable_error.is_none() {
                if self.json_output {
                    tracing::info!(
                        target: "json_output",
                        "{}",
                        serde_json::json!({
                            "type": "error",
                            "error": err
                        })
                        .to_string()
                    );
                }
                self.non_retryable_error = Some(err.to_string());
            }
            return vec![StreamEvent::StreamError {
                error: err.to_string(),
                retryable: false,
                substantive_output: self.substantive_output,
            }];
        }
        if self.non_retryable_error.is_some() {
            return Vec::new();
        }
        if !self.substantive_output {
            self.retryable_error_before_output = Some(err.to_string());
            // OpenAI-compatible providers emit the transport
            // timing marker after the error. Keep draining so
            // the failed attempt remains observable and the
            // retry decision happens only at stream end.
            return Vec::new();
        }
        self.stream_errored = true;
        if self.json_output {
            tracing::info!(
                target: "json_output",
                "{}",
                serde_json::json!({
                    "type": "error",
                    "error": err
                })
                .to_string()
            );
        }
        vec![StreamEvent::StreamError {
            error: err.to_string(),
            retryable: true,
            substantive_output: self.substantive_output,
        }]
    }
}

fn stream_error_is_retryable(error: &str) -> bool {
    error.contains("(retryable)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{
        ApiStreamReasoningChunk, ApiStreamTextChunk, ApiStreamToolCallFunction,
        ApiStreamToolCallsChunk,
    };

    fn test_info() -> StreamProviderInfo {
        StreamProviderInfo {
            provider_name: "test-provider".to_string(),
            context_window: 128_000,
        }
    }

    fn tool_delta(call_id: &str, name: Option<&str>, arguments: Option<&str>) -> ApiStreamChunk {
        ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
            tool_call: ApiStreamToolCall {
                call_id: Some(call_id.to_string()),
                function: ApiStreamToolCallFunction {
                    id: None,
                    name: name.map(str::to_string),
                    arguments: arguments.map(str::to_string),
                },
                signature: None,
            },
            id: None,
            signature: None,
        })
    }

    #[test]
    fn accumulator_assembles_text_fragments_usage_and_errors() {
        let mut acc = StreamAccumulator::new(false, test_info());

        assert_eq!(
            acc.push(&ApiStreamChunk::Text(ApiStreamTextChunk {
                text: "hello ".to_string(),
                id: None,
                signature: None,
            })),
            vec![StreamEvent::VisibleText("hello ".to_string())]
        );
        assert_eq!(
            acc.push(&ApiStreamChunk::Text(ApiStreamTextChunk {
                text: "world".to_string(),
                id: None,
                signature: Some("sig".to_string()),
            })),
            vec![StreamEvent::VisibleText("world".to_string())]
        );
        assert_eq!(
            acc.push(&ApiStreamChunk::Reasoning(ApiStreamReasoningChunk {
                reasoning: "thinking".to_string(),
                details: None,
                signature: None,
                redacted_data: None,
                id: None,
            })),
            vec![StreamEvent::ReasoningText("thinking".to_string())]
        );

        assert_eq!(
            acc.push(&tool_delta("c1", Some("read_file"), Some(r#"{"path": "a"#))),
            vec![StreamEvent::ToolCallReceived]
        );
        assert_eq!(
            acc.push(&tool_delta("c1", None, Some(r#".rs"}"#))),
            vec![StreamEvent::ToolCallReceived]
        );

        let events = acc.push(&ApiStreamChunk::Usage(ApiStreamUsageChunk {
            input_tokens: 100,
            output_tokens: 50,
            cache_write_tokens: None,
            cache_read_tokens: None,
            reasoning_tokens: Some(10),
            thoughts_token_count: None,
            total_cost: Some(0.001),
            stop_reason: None,
            id: None,
        }));
        assert_eq!(events.len(), 1);
        let (usage, deltas) = match &events[0] {
            StreamEvent::UsageUpdated { usage, deltas } => (usage, *deltas),
            other => panic!("expected UsageUpdated, got {other:?}"),
        };
        assert_eq!(usage.tokens_in, Some(100));
        assert_eq!(usage.tokens_out, Some(50));
        assert_eq!(
            deltas,
            UsageDeltas {
                tokens_in: 100,
                tokens_out: 50,
                cache_writes: 0,
                cache_reads: 0,
                reasoning_tokens: 10,
                cost: 0.001,
            }
        );

        assert_eq!(
            acc.push(&ApiStreamChunk::Error("boom (retryable)".to_string())),
            vec![StreamEvent::StreamError {
                error: "boom (retryable)".to_string(),
                retryable: true,
                substantive_output: true,
            }]
        );

        let outcome = acc.finish();
        assert_eq!(outcome.text, "hello world");
        assert_eq!(outcome.filtered_text, "hello world");
        assert_eq!(outcome.reasoning, "thinking");
        assert_eq!(outcome.text_signature.as_deref(), Some("sig"));
        assert_eq!(outcome.tool_call_order, vec!["c1".to_string()]);
        assert_eq!(
            outcome.tool_calls["c1"].function.arguments.as_deref(),
            Some(r#"{"path": "a.rs"}"#)
        );
        assert_eq!(outcome.usage.as_ref().and_then(|u| u.tokens_in), Some(100));
        assert!(outcome.substantive_output);
        assert!(outcome.errored);
        assert_eq!(outcome.retryable_error_before_output, None);
        assert_eq!(outcome.non_retryable_error, None);
    }

    #[test]
    fn accumulator_routes_pre_output_and_synthetic_cases() {
        let mut acc = StreamAccumulator::new(false, test_info());

        assert_eq!(
            acc.push(&ApiStreamChunk::Error("stall (retryable)".to_string())),
            Vec::new()
        );
        assert_eq!(
            acc.push(&ApiStreamChunk::Usage(ApiStreamUsageChunk {
                input_tokens: 0,
                output_tokens: 0,
                cache_write_tokens: Some(0),
                cache_read_tokens: None,
                reasoning_tokens: None,
                thoughts_token_count: None,
                total_cost: None,
                stop_reason: None,
                id: None,
            })),
            Vec::new()
        );
        assert_eq!(acc.push(&tool_delta("c9", None, None)), Vec::new());

        let outcome = acc.finish();
        assert_eq!(
            outcome.retryable_error_before_output.as_deref(),
            Some("stall (retryable)")
        );
        // Even a skipped delta counts as substantive output, matching the
        // loop's former read of the flag: the retry decision must not treat
        // a stream that produced tool-call attempts as pre-output.
        assert!(outcome.substantive_output);
        assert!(!outcome.errored);
        assert_eq!(outcome.usage, None);
        assert!(outcome.tool_calls.is_empty());

        let mut acc = StreamAccumulator::new(false, test_info());
        acc.note_preoutput_failure("no output within 5s".to_string());
        assert_eq!(
            acc.finish().retryable_error_before_output.as_deref(),
            Some("no output within 5s")
        );
    }
}
