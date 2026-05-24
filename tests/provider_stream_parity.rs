//! TypeScript-vs-Rust parity harness for provider stream parsing.
//!
//! Validates Rust provider SSE parsing behavior against TypeScript source
//! from `dirac/src/core/api/providers/openai.ts` and `dirac/src/core/api/providers/anthropic.ts`.

use sned::providers::{
    ApiStreamChunk, ApiStreamTextChunk, ApiStreamToolCallsChunk, ApiStreamUsageChunk,
    SseLineBuffer,
    anthropic::{
        AnthropicToolCallState, finish_anthropic_sse_to_chunks, parse_anthropic_sse_to_chunks,
    },
    openai::{finish_openai_sse_to_chunks, parse_openai_sse_to_chunks},
};

// ============================================================================
// Helpers
// ============================================================================

async fn collect_openai_chunks(sse: &str) -> Vec<ApiStreamChunk> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ApiStreamChunk>(100);
    let mut buffer = SseLineBuffer::default();
    let mut accumulated_tool_calls = std::collections::HashMap::new();
    let mut completed_tool_call_indices = std::collections::HashSet::new();
    let mut last_stop_reason = None;
    parse_openai_sse_to_chunks(
        sse.as_bytes(),
        &mut buffer,
        &tx,
        &mut accumulated_tool_calls,
        &mut completed_tool_call_indices,
        &mut last_stop_reason,
        &None,
    )
    .await;
    finish_openai_sse_to_chunks(
        &mut buffer,
        &tx,
        &mut accumulated_tool_calls,
        &mut completed_tool_call_indices,
        &mut last_stop_reason,
        &None,
    )
    .await;
    drop(tx);
    let mut chunks = Vec::new();
    while let Some(chunk) = rx.recv().await {
        chunks.push(chunk);
    }
    chunks
}

async fn collect_anthropic_chunks(sse: &str) -> Vec<ApiStreamChunk> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ApiStreamChunk>(100);
    let mut buffer = SseLineBuffer::default();
    let mut tool_state = AnthropicToolCallState::default();
    parse_anthropic_sse_to_chunks(sse.as_bytes(), &mut buffer, &tx, &mut tool_state).await;
    finish_anthropic_sse_to_chunks(&mut buffer, &tx, &mut tool_state).await;
    drop(tx);
    let mut chunks = Vec::new();
    while let Some(chunk) = rx.recv().await {
        chunks.push(chunk);
    }
    chunks
}

// ============================================================================
// OpenAI Stream Parsing Parity
// ============================================================================

#[tokio::test]
async fn openai_text_only_stream() {
    let sse = r#"
data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}

data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"gpt-4","choices":[{"index":0,"delta":{"content":" world"},"finish_reason":null}]}

data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"gpt-4","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: [DONE]
"#;

    let chunks = collect_openai_chunks(sse).await;

    // Should have 2 text chunks + 1 usage chunk with stop_reason
    assert_eq!(chunks.len(), 3);

    match &chunks[0] {
        ApiStreamChunk::Text(t) => {
            assert_eq!(t.text, "Hello");
            assert_eq!(t.id, Some("chatcmpl-123".to_string()));
        }
        _ => panic!("Expected text chunk, got {:?}", chunks[0]),
    }

    match &chunks[1] {
        ApiStreamChunk::Text(t) => {
            assert_eq!(t.text, " world");
            assert_eq!(t.id, Some("chatcmpl-123".to_string()));
        }
        _ => panic!("Expected text chunk, got {:?}", chunks[1]),
    }

    match &chunks[2] {
        ApiStreamChunk::Usage(u) => {
            assert_eq!(u.stop_reason, Some("stop".to_string()));
        }
        _ => panic!("Expected usage chunk with stop_reason, got {:?}", chunks[2]),
    }
}

#[tokio::test]
async fn openai_reasoning_stream() {
    let sse = r#"
data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"deepseek-chat","choices":[{"index":0,"delta":{"content":"","reasoning_content":"Let me think"},"finish_reason":null}]}

data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"deepseek-chat","choices":[{"index":0,"delta":{"content":"","reasoning_content":" about this"},"finish_reason":null}]}

data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"deepseek-chat","choices":[{"index":0,"delta":{"content":"Answer"},"finish_reason":null}]}

data: [DONE]
"#;

    let chunks = collect_openai_chunks(sse).await;

    // Should have 2 reasoning chunks + 1 text chunk + 1 synthetic usage = 4 chunks
    assert_eq!(chunks.len(), 4);

    match &chunks[0] {
        ApiStreamChunk::Reasoning(r) => {
            assert_eq!(r.reasoning, "Let me think");
            assert_eq!(r.id, Some("chatcmpl-123".to_string()));
        }
        _ => panic!("Expected reasoning chunk, got {:?}", chunks[0]),
    }

    match &chunks[1] {
        ApiStreamChunk::Reasoning(r) => {
            assert_eq!(r.reasoning, " about this");
        }
        _ => panic!("Expected reasoning chunk, got {:?}", chunks[1]),
    }

    match &chunks[2] {
        ApiStreamChunk::Text(t) => {
            assert_eq!(t.text, "Answer");
        }
        _ => panic!("Expected text chunk, got {:?}", chunks[2]),
    }

    match &chunks[3] {
        ApiStreamChunk::Usage(u) => {
            assert_eq!(u.stop_reason, None);
        }
        _ => panic!("Expected usage chunk, got {:?}", chunks[3]),
    }
}

#[tokio::test]
async fn openai_tool_call_stream() {
    let sse = r#"
data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"gpt-4","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"read_file","arguments":""}}]},"finish_reason":null}]}

data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"gpt-4","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\": \"/tmp/test.txt\"}"}}]},"finish_reason":null}]}

data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"gpt-4","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}

data: [DONE]
"#;

    let chunks = collect_openai_chunks(sse).await;

    // Tool calls are accumulated and flushed on finish_reason:tool_calls
    // Should have 1 tool call chunk (accumulated) + 1 synthetic usage = 2 chunks
    assert_eq!(chunks.len(), 2);

    match &chunks[0] {
        ApiStreamChunk::ToolCalls(tc) => {
            assert_eq!(tc.tool_call.call_id, Some("call_abc".to_string()));
            assert_eq!(tc.tool_call.function.name, Some("read_file".to_string()));
            assert_eq!(
                tc.tool_call.function.arguments,
                Some("{\"path\": \"/tmp/test.txt\"}".to_string())
            );
        }
        _ => panic!("Expected tool_calls chunk, got {:?}", chunks[0]),
    }

    match &chunks[1] {
        ApiStreamChunk::Usage(u) => {
            assert_eq!(u.stop_reason, Some("tool_calls".to_string()));
        }
        _ => panic!("Expected usage chunk with stop_reason, got {:?}", chunks[1]),
    }
}

#[tokio::test]
async fn openai_usage_stream() {
    let sse = r#"
data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"gpt-4","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":5},"prompt_cache_miss_tokens":3}}

data: [DONE]
"#;

    let chunks = collect_openai_chunks(sse).await;

    assert_eq!(chunks.len(), 1);

    match &chunks[0] {
        ApiStreamChunk::Usage(u) => {
            assert_eq!(u.input_tokens, 10);
            assert_eq!(u.output_tokens, 20);
            assert_eq!(u.cache_read_tokens, Some(5));
            assert_eq!(u.cache_write_tokens, Some(3));
        }
        _ => panic!("Expected usage chunk, got {:?}", chunks[0]),
    }
}

#[tokio::test]
async fn openai_mixed_stream() {
    let sse = r#"
data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"gpt-4","choices":[{"index":0,"delta":{"content":"I'll"},"finish_reason":null}]}

data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"gpt-4","choices":[{"index":0,"delta":{"content":" help"},"finish_reason":null}]}

data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"gpt-4","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_xyz","type":"function","function":{"name":"list_files","arguments":""}}]},"finish_reason":null}]}

data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"gpt-4","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\": \"/\"}"}}]},"finish_reason":null}]}

data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"gpt-4","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":15,"completion_tokens":25,"prompt_tokens_details":{"cached_tokens":0}}}

data: [DONE]
"#;

    let chunks = collect_openai_chunks(sse).await;

    // 2 text + 1 tool_calls (accumulated) + 1 usage from SSE = 4 chunks
    assert_eq!(chunks.len(), 4);

    assert!(
        matches!(&chunks[0], ApiStreamChunk::Text(ApiStreamTextChunk { text, .. }) if text == "I'll")
    );
    assert!(
        matches!(&chunks[1], ApiStreamChunk::Text(ApiStreamTextChunk { text, .. }) if text == " help")
    );
    assert!(
        matches!(&chunks[2], ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk { tool_call, .. }) if tool_call.function.name == Some("list_files".to_string()))
    );
    assert!(matches!(
        &chunks[3],
        ApiStreamChunk::Usage(ApiStreamUsageChunk {
            input_tokens: 15,
            output_tokens: 25,
            ..
        })
    ));
}

#[tokio::test]
async fn openai_empty_lines_ignored() {
    let sse = "\n\n\ndata: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1694268190,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n\n";

    let chunks = collect_openai_chunks(sse).await;
    // 1 text + 1 synthetic usage = 2 chunks
    assert_eq!(chunks.len(), 2);
    assert!(
        matches!(&chunks[0], ApiStreamChunk::Text(ApiStreamTextChunk { text, .. }) if text == "Hello")
    );
}

// ============================================================================
// Anthropic Stream Parsing Parity
// ============================================================================

#[tokio::test]
async fn anthropic_text_only_stream() {
    let sse = r#"
data: {"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":2}}

data: {"type":"message_stop"}

data: [DONE]
"#;

    let chunks = collect_anthropic_chunks(sse).await;

    // Should have: 1 usage (message_start) + 1 text (empty from start) + 1 text + 1 text + 1 usage (message_delta) = 5 chunks
    assert_eq!(chunks.len(), 5);

    match &chunks[0] {
        ApiStreamChunk::Usage(u) => {
            assert_eq!(u.input_tokens, 10);
            assert_eq!(u.output_tokens, 0);
        }
        _ => panic!("Expected usage chunk, got {:?}", chunks[0]),
    }

    // content_block_start with text: "" sends empty text chunk
    match &chunks[1] {
        ApiStreamChunk::Text(t) => {
            assert_eq!(t.text, "");
        }
        _ => panic!("Expected text chunk, got {:?}", chunks[1]),
    }

    match &chunks[2] {
        ApiStreamChunk::Text(t) => {
            assert_eq!(t.text, "Hello");
        }
        _ => panic!("Expected text chunk, got {:?}", chunks[2]),
    }

    match &chunks[3] {
        ApiStreamChunk::Text(t) => {
            assert_eq!(t.text, " world");
        }
        _ => panic!("Expected text chunk, got {:?}", chunks[3]),
    }

    match &chunks[4] {
        ApiStreamChunk::Usage(u) => {
            assert_eq!(u.output_tokens, 2);
            assert_eq!(u.stop_reason, Some("end_turn".to_string()));
        }
        _ => panic!("Expected usage chunk, got {:?}", chunks[4]),
    }
}

#[tokio::test]
async fn anthropic_thinking_stream() {
    let sse = r#"
data: {"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me think"}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig123"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}

data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Answer"}}

data: {"type":"content_block_stop","index":1}

data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}

data: {"type":"message_stop"}

data: [DONE]
"#;

    let chunks = collect_anthropic_chunks(sse).await;

    // 1 usage + 1 reasoning (empty from start) + 1 reasoning + 1 reasoning (signature) + 1 text (empty from start) + 1 text + 1 usage = 7 chunks
    assert_eq!(chunks.len(), 7);

    match &chunks[0] {
        ApiStreamChunk::Usage(u) => {
            assert_eq!(u.input_tokens, 10);
        }
        _ => panic!("Expected usage chunk, got {:?}", chunks[0]),
    }

    // content_block_start with thinking: "" sends empty reasoning chunk
    match &chunks[1] {
        ApiStreamChunk::Reasoning(r) => {
            assert_eq!(r.reasoning, "");
        }
        _ => panic!("Expected reasoning chunk, got {:?}", chunks[1]),
    }

    match &chunks[2] {
        ApiStreamChunk::Reasoning(r) => {
            assert_eq!(r.reasoning, "Let me think");
        }
        _ => panic!("Expected reasoning chunk, got {:?}", chunks[2]),
    }

    match &chunks[3] {
        ApiStreamChunk::Reasoning(r) => {
            assert_eq!(r.reasoning, "");
            assert_eq!(r.signature, Some("sig123".to_string()));
        }
        _ => panic!("Expected reasoning chunk, got {:?}", chunks[3]),
    }

    // content_block_start with text: "" sends empty text chunk
    match &chunks[4] {
        ApiStreamChunk::Text(t) => {
            assert_eq!(t.text, "");
        }
        _ => panic!("Expected text chunk, got {:?}", chunks[4]),
    }

    match &chunks[5] {
        ApiStreamChunk::Text(t) => {
            assert_eq!(t.text, "Answer");
        }
        _ => panic!("Expected text chunk, got {:?}", chunks[5]),
    }

    match &chunks[6] {
        ApiStreamChunk::Usage(u) => {
            assert_eq!(u.output_tokens, 2);
            assert_eq!(u.stop_reason, Some("end_turn".to_string()));
        }
        _ => panic!("Expected usage chunk, got {:?}", chunks[6]),
    }
}

#[tokio::test]
async fn anthropic_tool_use_stream() {
    let sse = r#"
data: {"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tool_01","name":"read_file"}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\": \"/tmp/test.txt\"}"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":10}}

data: {"type":"message_stop"}

data: [DONE]
"#;

    let chunks = collect_anthropic_chunks(sse).await;

    // 1 usage + 1 tool_calls (empty from start) + 1 tool_calls (with args) + 1 usage = 4 chunks
    assert_eq!(chunks.len(), 4);

    match &chunks[0] {
        ApiStreamChunk::Usage(u) => {
            assert_eq!(u.input_tokens, 10);
        }
        _ => panic!("Expected usage chunk, got {:?}", chunks[0]),
    }

    // content_block_start with tool_use sends tool_calls with empty arguments
    match &chunks[1] {
        ApiStreamChunk::ToolCalls(tc) => {
            assert_eq!(tc.tool_call.call_id, Some("tool_01".to_string()));
            assert_eq!(tc.tool_call.function.name, Some("read_file".to_string()));
            assert_eq!(tc.tool_call.function.arguments, Some("".to_string()));
        }
        _ => panic!("Expected tool_calls chunk, got {:?}", chunks[1]),
    }

    match &chunks[2] {
        ApiStreamChunk::ToolCalls(tc) => {
            assert_eq!(
                tc.tool_call.function.arguments,
                Some("{\"path\": \"/tmp/test.txt\"}".to_string())
            );
        }
        _ => panic!("Expected tool_calls chunk, got {:?}", chunks[2]),
    }

    match &chunks[3] {
        ApiStreamChunk::Usage(u) => {
            assert_eq!(u.stop_reason, Some("tool_use".to_string()));
        }
        _ => panic!("Expected usage chunk, got {:?}", chunks[3]),
    }
}

#[tokio::test]
async fn anthropic_redacted_thinking_stream() {
    let sse = r#"
data: {"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"redacted_data_here"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}

data: {"type":"message_stop"}

data: [DONE]
"#;

    let chunks = collect_anthropic_chunks(sse).await;

    // 1 usage + 1 reasoning (redacted from start) + 1 usage = 3 chunks
    assert_eq!(chunks.len(), 3);

    match &chunks[1] {
        ApiStreamChunk::Reasoning(r) => {
            assert_eq!(r.reasoning, "[Redacted thinking block]");
            assert_eq!(r.redacted_data, Some("redacted_data_here".to_string()));
        }
        _ => panic!("Expected reasoning chunk, got {:?}", chunks[1]),
    }
}

#[tokio::test]
async fn anthropic_caching_tokens_stream() {
    let sse = r#"
data: {"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0,"cache_creation_input_tokens":100,"cache_read_input_tokens":50}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}

data: {"type":"message_stop"}

data: [DONE]
"#;

    let chunks = collect_anthropic_chunks(sse).await;

    match &chunks[0] {
        ApiStreamChunk::Usage(u) => {
            assert_eq!(u.input_tokens, 10);
            assert_eq!(u.cache_write_tokens, Some(100));
            assert_eq!(u.cache_read_tokens, Some(50));
        }
        _ => panic!("Expected usage chunk, got {:?}", chunks[0]),
    }
}

#[tokio::test]
async fn anthropic_empty_lines_ignored() {
    let sse = "\n\n\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-6\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n\n";

    let chunks = collect_anthropic_chunks(sse).await;
    assert_eq!(chunks.len(), 1);
    assert!(matches!(
        &chunks[0],
        ApiStreamChunk::Usage(ApiStreamUsageChunk {
            input_tokens: 10,
            ..
        })
    ));
}

// ============================================================================
// Cross-Provider Parity: Same semantic output for equivalent inputs
// ============================================================================

#[tokio::test]
async fn both_providers_handle_done_line() {
    let openai_sse = "data: [DONE]\n";
    let anthropic_sse = "data: [DONE]\n";

    let openai_chunks = collect_openai_chunks(openai_sse).await;
    let anthropic_chunks = collect_anthropic_chunks(anthropic_sse).await;

    // OpenAI emits synthetic usage, Anthropic does not
    assert_eq!(openai_chunks.len(), 1);
    assert!(matches!(&openai_chunks[0], ApiStreamChunk::Usage(_)));
    assert_eq!(anthropic_chunks.len(), 0);
}

#[tokio::test]
async fn both_providers_ignore_malformed_json() {
    let openai_sse = "data: {not valid json}\n";
    let anthropic_sse = "data: {not valid json}\n";

    let openai_chunks = collect_openai_chunks(openai_sse).await;
    let anthropic_chunks = collect_anthropic_chunks(anthropic_sse).await;

    // OpenAI emits synthetic usage, Anthropic does not
    assert_eq!(openai_chunks.len(), 1);
    assert!(matches!(&openai_chunks[0], ApiStreamChunk::Usage(_)));
    assert_eq!(anthropic_chunks.len(), 0);
}

#[tokio::test]
async fn openai_multiple_tool_calls_same_chunk() {
    let sse = r#"
data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"gpt-4","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":""}},{"index":1,"id":"call_2","type":"function","function":{"name":"list_files","arguments":""}}]},"finish_reason":null}]}

data: [DONE]
"#;

    let chunks = collect_openai_chunks(sse).await;

    // Multiple tool calls in same delta are accumulated, then flushed at stream end + synthetic usage
    assert_eq!(chunks.len(), 3);

    match &chunks[0] {
        ApiStreamChunk::ToolCalls(tc) => {
            assert_eq!(tc.tool_call.call_id, Some("call_1".to_string()));
            assert_eq!(tc.tool_call.function.name, Some("read_file".to_string()));
        }
        _ => panic!("Expected tool_calls chunk, got {:?}", chunks[0]),
    }

    match &chunks[1] {
        ApiStreamChunk::ToolCalls(tc) => {
            assert_eq!(tc.tool_call.call_id, Some("call_2".to_string()));
            assert_eq!(tc.tool_call.function.name, Some("list_files".to_string()));
        }
        _ => panic!("Expected tool_calls chunk, got {:?}", chunks[1]),
    }

    match &chunks[2] {
        ApiStreamChunk::Usage(_) => {}
        _ => panic!("Expected usage chunk, got {:?}", chunks[2]),
    }
}

#[tokio::test]
async fn anthropic_multiple_tool_uses_sequential() {
    let sse = r#"
data: {"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tool_01","name":"read_file"}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\": \"/a\"}"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tool_02","name":"list_files"}}

data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\": \"/b\"}"}}

data: {"type":"content_block_stop","index":1}

data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":10}}

data: {"type":"message_stop"}

data: [DONE]
"#;

    let chunks = collect_anthropic_chunks(sse).await;

    // 1 usage + 1 tool_calls (empty from start) + 1 tool_calls (with args) + 1 tool_calls (empty from start) + 1 tool_calls (with args) + 1 usage = 6 chunks
    assert_eq!(chunks.len(), 6);

    // Tool use 1
    match &chunks[1] {
        ApiStreamChunk::ToolCalls(tc) => {
            assert_eq!(tc.tool_call.call_id, Some("tool_01".to_string()));
            assert_eq!(tc.tool_call.function.name, Some("read_file".to_string()));
            assert_eq!(tc.tool_call.function.arguments, Some("".to_string()));
        }
        _ => panic!("Expected tool_calls chunk, got {:?}", chunks[1]),
    }

    match &chunks[2] {
        ApiStreamChunk::ToolCalls(tc) => {
            assert_eq!(
                tc.tool_call.function.arguments,
                Some("{\"path\": \"/a\"}".to_string())
            );
        }
        _ => panic!("Expected tool_calls chunk, got {:?}", chunks[2]),
    }

    // Tool use 2
    match &chunks[3] {
        ApiStreamChunk::ToolCalls(tc) => {
            assert_eq!(tc.tool_call.call_id, Some("tool_02".to_string()));
            assert_eq!(tc.tool_call.function.name, Some("list_files".to_string()));
            assert_eq!(tc.tool_call.function.arguments, Some("".to_string()));
        }
        _ => panic!("Expected tool_calls chunk, got {:?}", chunks[3]),
    }

    match &chunks[4] {
        ApiStreamChunk::ToolCalls(tc) => {
            assert_eq!(
                tc.tool_call.function.arguments,
                Some("{\"path\": \"/b\"}".to_string())
            );
        }
        _ => panic!("Expected tool_calls chunk, got {:?}", chunks[4]),
    }
}
