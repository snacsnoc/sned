//! TypeScript-vs-Rust parity harness for context selection.
//!
//! Validates Rust `ContextManager` behavior against TypeScript test fixtures from
//! `dirac/src/core/context/context-management/__tests__/ContextManager.test.ts`.

use sned::core::context::context_manager::{self, TruncationKeep};
use sned::providers::{
    AssistantContentBlock, MessageContent, MessageRole, SharedContentFields, StorageMessage,
    TextContentBlock, ToolResultBlock, ToolResultContent, ToolUseBlock, UserContentBlock,
};

// ============================================================================
// Helpers
// ============================================================================

fn create_test_messages(count: usize) -> Vec<StorageMessage> {
    let mut messages = Vec::with_capacity(count);

    messages.push(StorageMessage {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text("Initial task message".to_string()),
        model_info: None,
        metrics: None,
        ts: None,
    });

    let mut role = MessageRole::Assistant;
    for i in 1..count {
        messages.push(StorageMessage {
            id: None,
            role,
            content: MessageContent::Text(format!("Message {i}")),
            model_info: None,
            metrics: None,
            ts: None,
        });
        role = if role == MessageRole::Assistant {
            MessageRole::User
        } else {
            MessageRole::Assistant
        };
    }

    messages
}

fn create_api_req_info(
    tokens_in: Option<u32>,
    tokens_out: Option<u32>,
    cache_writes: Option<u32>,
    cache_reads: Option<u32>,
    context_window: Option<u64>,
) -> sned::core::context::context_manager::ApiReqInfo {
    sned::core::context::context_manager::ApiReqInfo {
        request: None,
        tokens_in,
        tokens_out,
        cache_writes,
        cache_reads,
        reasoning_tokens: None,
        cost: None,
        context_window,
        context_usage_percentage: None,
    }
}

// ============================================================================
// getNextTruncationRange parity
// ============================================================================

#[test]
fn truncation_range_first_half_keep() {
    let messages = create_test_messages(11);
    let result = context_manager::get_next_truncation_range(&messages, None, TruncationKeep::Half);
    assert_eq!(result, (2, 5));
}

#[test]
fn truncation_range_first_quarter_keep() {
    let messages = create_test_messages(11);
    let result =
        context_manager::get_next_truncation_range(&messages, None, TruncationKeep::LastQuarter);
    assert_eq!(result, (2, 7));
}

#[test]
fn truncation_range_sequential_half_keep() {
    let messages = create_test_messages(21);

    let first_range =
        context_manager::get_next_truncation_range(&messages, None, TruncationKeep::Half);
    assert_eq!(first_range, (2, 9));

    let second_range = context_manager::get_next_truncation_range(
        &messages,
        Some(first_range),
        TruncationKeep::Half,
    );
    assert_eq!(second_range, (2, 13));
}

#[test]
fn truncation_range_sequential_quarter_keep() {
    let messages = create_test_messages(41);

    let first_range =
        context_manager::get_next_truncation_range(&messages, None, TruncationKeep::LastQuarter);
    let second_range = context_manager::get_next_truncation_range(
        &messages,
        Some(first_range),
        TruncationKeep::LastQuarter,
    );

    assert_eq!(second_range.0, 2);
    assert!(second_range.1 > first_range.1);
}

#[test]
fn truncation_range_ensures_last_removed_is_assistant() {
    let messages = create_test_messages(14);
    let result = context_manager::get_next_truncation_range(&messages, None, TruncationKeep::Half);

    let last_removed = &messages[result.1];
    assert_eq!(last_removed.role, MessageRole::Assistant);

    let next_message = &messages[result.1 + 1];
    assert_eq!(next_message.role, MessageRole::User);
}

#[test]
fn truncation_range_small_message_arrays() {
    let messages = create_test_messages(3);
    let result = context_manager::get_next_truncation_range(&messages, None, TruncationKeep::Half);
    assert_eq!(result, (2, 2));
}

#[test]
fn truncation_range_preserves_message_structure() {
    let messages = create_test_messages(20);
    let result = context_manager::get_next_truncation_range(&messages, None, TruncationKeep::Half);

    let effective_messages: Vec<_> = messages[..result.0]
        .iter()
        .chain(&messages[result.1 + 1..])
        .cloned()
        .collect();

    assert_eq!(effective_messages[0].role, MessageRole::User);
    for (i, message) in effective_messages.iter().enumerate().skip(1) {
        let expected = if i % 2 == 1 {
            MessageRole::Assistant
        } else {
            MessageRole::User
        };
        assert_eq!(message.role, expected);
    }
}

// ============================================================================
// getTruncatedMessages parity
// ============================================================================

#[test]
fn truncated_messages_no_range_returns_original() {
    let messages = create_test_messages(3);
    let result = context_manager::get_truncated_messages(&messages, None, None);
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].role, MessageRole::User);
    assert_eq!(result[1].role, MessageRole::Assistant);
    assert_eq!(result[2].role, MessageRole::User);
}

#[test]
fn truncated_messages_removes_specified_range() {
    let messages = create_test_messages(5);
    let result = context_manager::get_truncated_messages(&messages, Some((1, 3)), None);
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].role, MessageRole::User);
    assert_eq!(result[1].role, MessageRole::Assistant);
    assert_eq!(result[2].role, MessageRole::User);
}

#[test]
fn truncated_messages_range_starts_at_first_after_task() {
    let messages = create_test_messages(4);
    let result = context_manager::get_truncated_messages(&messages, Some((1, 2)), None);
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].role, MessageRole::User);
    assert_eq!(result[1].role, MessageRole::Assistant);
    assert_eq!(result[2].role, MessageRole::Assistant);
}

#[test]
fn truncated_messages_preserves_alternation() {
    let messages = create_test_messages(5);
    let result = context_manager::get_truncated_messages(&messages, Some((2, 3)), None);
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].role, MessageRole::User);
    assert_eq!(result[1].role, MessageRole::Assistant);
    assert_eq!(result[2].role, MessageRole::User);
}

#[test]
fn truncated_messages_removes_orphaned_tool_results() {
    let messages = vec![
        StorageMessage {
            id: None,
            role: MessageRole::User,
            content: MessageContent::Text("Initial task".to_string()),
            model_info: None,
            metrics: None,
            ts: None,
        },
        StorageMessage {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::Text("Response 1".to_string()),
            model_info: None,
            metrics: None,
            ts: None,
        },
        // Assistant message with tool_use that will be truncated
        StorageMessage {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::AssistantBlocks(vec![
                AssistantContentBlock::Text(TextContentBlock {
                    text: "Using a tool".to_string(),
                    shared: SharedContentFields {
                        call_id: None,
                        signature: None,
                    },
                    reasoning_details: None,
                }),
                AssistantContentBlock::ToolUse(ToolUseBlock {
                    id: "tool_123".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::json!({"path": "test.ts"}),
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
        },
        // User message with tool_result - should have tool_result removed after truncation
        StorageMessage {
            id: None,
            role: MessageRole::User,
            content: MessageContent::UserBlocks(vec![
                UserContentBlock::ToolResult(ToolResultBlock {
                    tool_use_id: "tool_123".to_string(),
                    content: ToolResultContent::Text("file content here".to_string()),
                    shared: SharedContentFields {
                        call_id: None,
                        signature: None,
                    },
                }),
                UserContentBlock::Text(TextContentBlock {
                    text: "Additional user text".to_string(),
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
        },
        StorageMessage {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::Text("Response 2".to_string()),
            model_info: None,
            metrics: None,
            ts: None,
        },
    ];

    let result = context_manager::get_truncated_messages(&messages, Some((2, 2)), None);
    assert_eq!(result.len(), 4);

    let user_message_after_truncation = &result[2];
    assert_eq!(user_message_after_truncation.role, MessageRole::User);

    if let MessageContent::UserBlocks(blocks) = &user_message_after_truncation.content {
        assert_eq!(blocks.len(), 1);
        assert!(matches!(blocks[0], UserContentBlock::Text(_)));
        if let UserContentBlock::Text(text_block) = &blocks[0] {
            assert_eq!(text_block.text, "Additional user text");
        }
    } else {
        panic!("Expected UserBlocks");
    }
}

// ============================================================================
// shouldCompactContextWindow parity
// ============================================================================

#[test]
fn compact_does_not_compact_at_33k_with_default_threshold() {
    let info = create_api_req_info(Some(30_000), Some(3_000), None, None, Some(200_000));
    let max_allowed = (200_000.0 * 0.8) as u64;
    assert!(!context_manager::should_compact_context_window(
        &info,
        200_000,
        max_allowed,
        Some(0.75),
        "openai"
    ));
}

#[test]
fn compact_compacts_when_tokens_exceed_threshold() {
    let info = create_api_req_info(Some(140_000), Some(15_000), None, None, Some(200_000));
    let max_allowed = (200_000.0 * 0.8) as u64;
    assert!(context_manager::should_compact_context_window(
        &info,
        200_000,
        max_allowed,
        Some(0.75),
        "openai"
    ));
}

#[test]
fn compact_accidental_low_threshold() {
    let context_window = 200_000;
    let accidental_threshold = 0.05;
    let compaction_triggers_at = (context_window as f64 * accidental_threshold) as u64;
    let total_tokens = compaction_triggers_at + 500;
    let tokens_in = total_tokens - 1_500;
    let tokens_out = 1_500;

    let info = create_api_req_info(
        Some(tokens_in as u32),
        Some(tokens_out as u32),
        None,
        None,
        Some(context_window),
    );
    let max_allowed = (context_window as f64 * 0.8) as u64;
    assert!(context_manager::should_compact_context_window(
        &info,
        context_window,
        max_allowed,
        Some(accidental_threshold),
        "openai"
    ));
}

#[test]
fn compact_falls_back_to_max_allowed_when_threshold_undefined() {
    let info = create_api_req_info(Some(150_000), Some(5_000), None, None, Some(200_000));
    let max_allowed = (200_000.0 * 0.8) as u64;
    // 155K < 160K max_allowed → false
    assert!(!context_manager::should_compact_context_window(
        &info,
        200_000,
        max_allowed,
        None,
        "openai"
    ));
}

#[test]
fn compact_falls_back_to_max_allowed_when_threshold_zero() {
    let info = create_api_req_info(Some(150_000), Some(5_000), None, None, Some(200_000));
    let max_allowed = (200_000.0 * 0.8) as u64;
    assert!(!context_manager::should_compact_context_window(
        &info,
        200_000,
        max_allowed,
        Some(0.0),
        "openai"
    ));
}

#[test]
fn compact_includes_cache_tokens() {
    let info = create_api_req_info(
        Some(5_000),
        Some(500),
        Some(0),
        Some(150_000),
        Some(200_000),
    );
    let max_allowed = (200_000.0 * 0.8) as u64;
    assert!(context_manager::should_compact_context_window(
        &info,
        200_000,
        max_allowed,
        Some(0.75),
        "anthropic"
    ));
}

#[test]
fn compact_threshold_capped_at_max_allowed() {
    let info = create_api_req_info(Some(165_000), None, None, None, Some(200_000));
    let max_allowed = (200_000.0 * 0.8) as u64;
    assert!(context_manager::should_compact_context_window(
        &info,
        200_000,
        max_allowed,
        Some(1.0),
        "openai"
    ));
}

// ============================================================================
// getNewContextMessagesAndMetadata parity
// ============================================================================

#[test]
fn new_context_auto_condense_skips_truncation() {
    let messages = create_test_messages(10);
    let info = create_api_req_info(Some(10_000), Some(0), None, None, Some(200_000));

    let result = context_manager::get_new_context_messages_and_metadata(
        &messages,
        Some(&info),
        None,
        true, // use_auto_condense = true
        None,
        "test",
    );

    assert!(!result.updated_conversation_history_deleted_range);
    assert_eq!(result.conversation_history_deleted_range, None);
    assert_eq!(result.truncated_conversation_history.len(), 10);
}

#[test]
fn new_context_mechanical_truncation_half() {
    let messages = create_test_messages(10);
    let info = create_api_req_info(Some(180_000), Some(10_000), None, None, Some(200_000));

    let result = context_manager::get_new_context_messages_and_metadata(
        &messages,
        Some(&info),
        None,
        false, // use_auto_condense = false
        None,
        "test",
    );

    assert!(result.updated_conversation_history_deleted_range);
    assert_eq!(result.conversation_history_deleted_range, Some((2, 5)));
    assert_eq!(result.truncated_conversation_history.len(), 6);
}

#[test]
fn new_context_mechanical_truncation_quarter_when_large() {
    let messages = create_test_messages(10);
    let info = create_api_req_info(Some(300_000), Some(50_000), None, None, Some(200_000));

    let result = context_manager::get_new_context_messages_and_metadata(
        &messages,
        Some(&info),
        None,
        false,
        None,
        "test",
    );

    assert!(result.updated_conversation_history_deleted_range);
    // total_tokens/2 = 175K > max_allowed (160K) → LastQuarter keep
    assert_eq!(result.conversation_history_deleted_range, Some((2, 7)));
    assert_eq!(result.truncated_conversation_history.len(), 4);
}

// ============================================================================
// Context window info parity
// ============================================================================

use sned::core::context::get_context_window_info;
use sned::providers::{mock::MockResponse, MockProvider, Providers};

#[test]
fn context_window_default_256k() {
    let provider = MockProvider::new(vec![MockResponse::Text("test".to_string())]);
    let provider = Providers::Mock(provider);
    let info = get_context_window_info(&provider);
    assert_eq!(info.context_window, 256_000);
    let expected = f64::max(256_000.0 - 40_000.0, 256_000.0 * 0.8) as u64;
    assert_eq!(info.max_allowed_size, expected);
}

#[test]
fn context_window_large_respects_hard_limit() {
    let provider = MockProvider::new_with_context_window(
        vec![MockResponse::Text("test".to_string())],
        2_000_000,
    );
    let provider = Providers::Mock(provider);
    let info = get_context_window_info(&provider);
    assert_eq!(info.context_window, 2_000_000);
    assert_eq!(info.max_allowed_size, 1_000_000);
}

#[test]
fn context_window_small_uses_80_percent() {
    let provider = MockProvider::new_with_context_window(
        vec![MockResponse::Text("test".to_string())],
        64_000,
    );
    let provider = Providers::Mock(provider);
    let info = get_context_window_info(&provider);
    assert_eq!(info.context_window, 64_000);
    let expected = f64::max(64_000.0 - 40_000.0, 64_000.0 * 0.8) as u64;
    assert_eq!(info.max_allowed_size, expected);
}
