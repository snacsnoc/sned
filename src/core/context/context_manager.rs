//! Context manager for handling conversation truncation and compaction.
//!

use crate::providers::{
    AssistantContentBlock, MessageContent, MessageRole, StorageMessage, TextContentBlock,
    UserContentBlock,
};
use serde::{Deserialize, Serialize};

/// API request info used for context management decisions.
///
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ApiReqInfo {
    pub request: Option<String>,
    #[serde(rename = "tokensIn")]
    pub tokens_in: Option<u32>,
    #[serde(rename = "tokensOut")]
    pub tokens_out: Option<u32>,
    #[serde(rename = "cacheWrites")]
    pub cache_writes: Option<u32>,
    #[serde(rename = "cacheReads")]
    pub cache_reads: Option<u32>,
    #[serde(rename = "reasoningTokens")]
    pub reasoning_tokens: Option<u32>,
    pub cost: Option<f64>,
    pub context_window: Option<u64>,
    pub context_usage_percentage: Option<f64>,
}

/// Result of getting new context messages and metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextUpdateResult {
    pub conversation_history_deleted_range: Option<(usize, usize)>,
    pub updated_conversation_history_deleted_range: bool,
    pub truncated_conversation_history: Vec<StorageMessage>,
}

/// Preserved state from compacted context - ensures critical information is retained.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PreservedState {
    #[serde(default)]
    pub current_task: String,
    #[serde(default)]
    pub user_constraints: Vec<String>,
    #[serde(default)]
    pub files_inspected: Vec<String>,
    #[serde(default)]
    pub files_modified: Vec<String>,
    #[serde(default)]
    pub commands_run: Vec<String>,
    #[serde(default)]
    pub validation_results: Vec<String>,
    #[serde(default)]
    pub errors_encountered: Vec<String>,
    #[serde(default)]
    pub design_decisions: Vec<String>,
    #[serde(default)]
    pub unresolved_blockers: Vec<String>,
    #[serde(default)]
    pub next_steps: Vec<String>,
    #[serde(default)]
    pub important_symbols: Vec<String>,
}

/// Compacted summary of conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactedSummary {
    pub summary_text: String,
    pub created_at: u64,
    pub messages_compacted: usize,
    #[serde(default)]
    pub preserved_state: PreservedState,
}

impl CompactedSummary {
    pub fn new(summary_text: String, messages_compacted: usize) -> Self {
        Self {
            summary_text,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            messages_compacted,
            preserved_state: PreservedState::default(),
        }
    }
}

/// Determines whether we should compact the context window based on token counts.
///
///
/// For OpenAI-compatible providers, `tokens_in` already includes cache tokens,
/// so cache_writes/cache_reads are not added separately.
/// For Anthropic, cache tokens are reported separately and are added.
pub fn should_compact_context_window(
    api_req_info: &ApiReqInfo,
    context_window: u64,
    max_allowed_size: u64,
    threshold_percentage: Option<f64>,
    provider_name: &str,
) -> bool {
    let cache_tokens = if provider_name == "openai" || provider_name == "minimax" {
        0
    } else {
        api_req_info.cache_writes.unwrap_or(0) as u64 + api_req_info.cache_reads.unwrap_or(0) as u64
    };

    let total_tokens = api_req_info.tokens_in.unwrap_or(0) as u64
        + api_req_info.tokens_out.unwrap_or(0) as u64
        + cache_tokens;

    // Match TypeScript falsy behavior: 0 and 0.0 fall back to max_allowed_size
    let rounded_threshold = match threshold_percentage {
        Some(pct) if pct > 0.0 => (context_window as f64 * pct) as u64,
        _ => max_allowed_size,
    };

    let threshold_tokens = rounded_threshold.min(max_allowed_size);
    total_tokens >= threshold_tokens
}

/// Primary entry point for getting up-to-date context.
pub fn get_new_context_messages_and_metadata(
    api_conversation_history: &[StorageMessage],
    api_req_info: Option<&ApiReqInfo>,
    conversation_history_deleted_range: Option<(usize, usize)>,
    use_auto_condense: bool,
    compacted_summary: Option<&CompactedSummary>,
    provider_name: &str,
) -> ContextUpdateResult {
    let mut updated_conversation_history_deleted_range = false;
    let mut new_deleted_range = conversation_history_deleted_range;

    if let Some(info) = api_req_info {
        // Include cache tokens for Anthropic (tokens_in already includes cache for OpenAI)
        let cache_tokens = if provider_name == "openai" || provider_name == "minimax" {
            0
        } else {
            info.cache_writes.unwrap_or(0) as u64 + info.cache_reads.unwrap_or(0) as u64
        };
        let total_tokens = (info.tokens_in.unwrap_or(0) as u64)
            + (info.tokens_out.unwrap_or(0) as u64)
            + cache_tokens;

        let threshold_pct = if use_auto_condense { 0.7 } else { 0.8 };
        let max_allowed_size = info
            .context_window
            .map(|cw| (cw as f64 * threshold_pct) as u64)
            .unwrap_or((256_000.0 * threshold_pct) as u64);

        if total_tokens >= max_allowed_size {
            let keep = if (total_tokens as u64 / 2) > max_allowed_size {
                TruncationKeep::LastQuarter
            } else {
                TruncationKeep::Half
            };

            new_deleted_range = Some(get_next_truncation_range(
                api_conversation_history,
                conversation_history_deleted_range,
                keep,
            ));

            updated_conversation_history_deleted_range = true;
        }
    }

    let truncated_conversation_history = get_and_alter_truncated_messages(
        api_conversation_history,
        new_deleted_range,
        compacted_summary,
    );

    ContextUpdateResult {
        conversation_history_deleted_range: new_deleted_range,
        updated_conversation_history_deleted_range,
        truncated_conversation_history,
    }
}

/// Gets the next truncation range for context window management.
///
/// # Safety
///
/// This function uses saturating arithmetic to prevent underflow, but callers
/// should ensure `api_messages.len() >= 2` to avoid degenerate cases.
pub fn get_next_truncation_range(
    api_messages: &[StorageMessage],
    current_deleted_range: Option<(usize, usize)>,
    keep: TruncationKeep,
) -> (usize, usize) {
    let range_start_index = 2;
    let start_of_rest = current_deleted_range.map(|r| r.1 + 1).unwrap_or(2);

    // Use saturating_sub to prevent underflow when message count is small
    let messages_to_remove: usize = match keep {
        TruncationKeep::None => api_messages.len().saturating_sub(start_of_rest),
        TruncationKeep::LastTwo => api_messages.len().saturating_sub(start_of_rest + 2),
        TruncationKeep::Half => {
            // Saturate the subtraction to handle small message counts
            let diff = api_messages.len().saturating_sub(start_of_rest);
            (diff / 4) * 2
        }
        TruncationKeep::LastQuarter => {
            // Saturate the subtraction to handle small message counts
            let diff = api_messages.len().saturating_sub(start_of_rest);
            ((diff * 3) / 4 / 2) * 2
        }
    };

    // Prevent underflow: if no messages to remove, keep range at start_of_rest
    let mut range_end_index = if messages_to_remove == 0 {
        start_of_rest.saturating_sub(1).max(range_start_index)
    } else {
        start_of_rest + messages_to_remove.saturating_sub(1)
    };

    // Adjust to end on an assistant message if possible
    if range_end_index < api_messages.len()
        && api_messages[range_end_index].role != MessageRole::Assistant
    {
        range_end_index = range_end_index.saturating_sub(1);
    }

    // Ensure range_end_index doesn't go below range_start_index
    range_end_index = range_end_index.max(range_start_index);

    (range_start_index, range_end_index)
}

pub fn get_truncated_messages(
    messages: &[StorageMessage],
    deleted_range: Option<(usize, usize)>,
    compacted_summary: Option<&CompactedSummary>,
) -> Vec<StorageMessage> {
    get_and_alter_truncated_messages(messages, deleted_range, compacted_summary)
}

fn get_and_alter_truncated_messages(
    messages: &[StorageMessage],
    deleted_range: Option<(usize, usize)>,
    compacted_summary: Option<&CompactedSummary>,
) -> Vec<StorageMessage> {
    if messages.len() <= 1 {
        return messages.to_vec();
    }

    // Fast path: no truncation or summary needed, return clone
    if deleted_range.is_none() && compacted_summary.is_none() {
        return messages.to_vec();
    }

    let start_from_index = deleted_range.map(|r| r.1 + 1).unwrap_or(2);
    let mut updated_messages =
        apply_context_history_updates(messages, start_from_index, compacted_summary);

    ensure_tool_results_follow_tool_use(&mut updated_messages);

    updated_messages
}

fn apply_context_history_updates(
    messages: &[StorageMessage],
    start_from_index: usize,
    compacted_summary: Option<&CompactedSummary>,
) -> Vec<StorageMessage> {
    let first_chunk = &messages[..2.min(messages.len())];
    let second_chunk = if start_from_index < messages.len() {
        &messages[start_from_index..]
    } else {
        &[]
    };

    let mut messages_to_update = Vec::with_capacity(first_chunk.len() + second_chunk.len() + 1);
    messages_to_update.extend_from_slice(first_chunk);

    // Insert compacted summary after first 2 messages if present
    if let Some(summary) = compacted_summary {
        messages_to_update.push(StorageMessage {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::Text(summary.summary_text.clone()),
            model_info: None,
            metrics: None,
            ts: None,
        });
    }

    messages_to_update.extend_from_slice(second_chunk);

    if start_from_index > 2
        && messages_to_update.len() > 2
        && let Some(first_message) = messages_to_update.get_mut(2)
        && first_message.role == MessageRole::User
        && let MessageContent::UserBlocks(blocks) = &first_message.content
    {
        let has_tool_results = blocks
            .iter()
            .any(|block| matches!(block, UserContentBlock::ToolResult(_)));

        if has_tool_results {
            let filtered_blocks: Vec<UserContentBlock> = blocks
                .iter()
                .filter(|block| !matches!(block, UserContentBlock::ToolResult(_)))
                .cloned()
                .collect();
            if filtered_blocks.is_empty() {
                first_message.content =
                    MessageContent::UserBlocks(vec![UserContentBlock::Text(TextContentBlock {
                        text: "[context truncated]".to_string(),
                        shared: crate::providers::SharedContentFields {
                            call_id: None,
                            signature: None,
                        },
                        reasoning_details: None,
                    })]);
            } else {
                first_message.content = MessageContent::UserBlocks(filtered_blocks);
            }
        }
    }

    messages_to_update
}

fn ensure_tool_results_follow_tool_use(messages: &mut [StorageMessage]) {
    for i in 0..messages.len().saturating_sub(1) {
        let message = &messages[i];

        if message.role != MessageRole::Assistant {
            continue;
        }

        let mut tool_use_ids: Vec<String> = Vec::new();
        if let MessageContent::AssistantBlocks(blocks) = &message.content {
            for block in blocks {
                if let AssistantContentBlock::ToolUse(tool_use) = block
                    && !tool_use.id.is_empty()
                {
                    tool_use_ids.push(tool_use.id.clone());
                }
            }
        }

        if tool_use_ids.is_empty() {
            continue;
        }

        let next_message = &messages[i + 1];

        if next_message.role != MessageRole::User {
            continue;
        }

        let mut tool_result_map: std::collections::HashMap<String, UserContentBlock> =
            std::collections::HashMap::new();
        let mut other_blocks: Vec<UserContentBlock> = Vec::new();

        if let MessageContent::UserBlocks(blocks) = &next_message.content {
            for block in blocks {
                match block {
                    UserContentBlock::ToolResult(tool_result) => {
                        tool_result_map.insert(tool_result.tool_use_id.clone(), block.clone());
                    }
                    _ => {
                        other_blocks.push(block.clone());
                    }
                }
            }
        }

        // Always reorder tool results to match tool use order when both exist.
        // This ensures the model sees results in the correct order even if
        // they were received out of sequence or if some are missing.
        let mut needs_update = !tool_use_ids.is_empty() && !tool_result_map.is_empty();

        for tool_use_id in &tool_use_ids {
            if !tool_result_map.contains_key(tool_use_id) {
                tool_result_map.insert(
                    tool_use_id.clone(),
                    UserContentBlock::ToolResult(crate::providers::ToolResultBlock {
                        tool_use_id: tool_use_id.clone(),
                        content: crate::providers::ToolResultContent::Text(
                            "result missing".to_string(),
                        ),
                        shared: crate::providers::SharedContentFields {
                            call_id: None,
                            signature: None,
                        },
                    }),
                );
                needs_update = true;
            }
        }

        if !needs_update {
            continue;
        }

        let mut new_content: Vec<UserContentBlock> = Vec::new();
        for tool_use_id in &tool_use_ids {
            if let Some(tool_result) = tool_result_map.get(tool_use_id) {
                new_content.push(tool_result.clone());
            }
        }

        new_content.extend(other_blocks);
        messages[i + 1].content = MessageContent::UserBlocks(new_content);
    }
}

/// How much of the conversation to keep when truncating.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TruncationKeep {
    None,
    LastTwo,
    Half,
    LastQuarter,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_message(role: MessageRole) -> StorageMessage {
        StorageMessage {
            id: None,
            role,
            content: MessageContent::Text("test".to_string()),
            model_info: None,
            metrics: None,
            ts: None,
        }
    }

    #[test]
    fn test_should_compact_context_window() {
        let info = ApiReqInfo {
            tokens_in: Some(100_000),
            tokens_out: Some(70_000),
            ..Default::default()
        };

        assert!(should_compact_context_window(
            &info,
            200_000,
            160_000, // 200k * 0.8
            None,
            "anthropic"
        ));

        let info_small = ApiReqInfo {
            tokens_in: Some(10_000),
            tokens_out: Some(5_000),
            ..Default::default()
        };

        assert!(!should_compact_context_window(
            &info_small,
            200_000,
            160_000,
            None,
            "anthropic"
        ));
    }

    #[test]
    fn test_should_compact_with_threshold_percentage() {
        let info = ApiReqInfo {
            tokens_in: Some(50_000),
            tokens_out: Some(10_000),
            ..Default::default()
        };

        assert!(should_compact_context_window(
            &info,
            200_000,
            160_000,
            Some(0.25),
            "anthropic"
        ));

        assert!(!should_compact_context_window(
            &info,
            200_000,
            160_000,
            Some(0.5),
            "anthropic"
        ));
    }

    #[test]
    fn test_should_compact_provider_aware_cache_tokens() {
        let info_with_cache = ApiReqInfo {
            tokens_in: Some(100_000),
            tokens_out: Some(50_000),
            cache_writes: Some(20_000),
            cache_reads: Some(10_000),
            ..Default::default()
        };

        let result_anthropic =
            should_compact_context_window(&info_with_cache, 200_000, 160_000, None, "anthropic");
        assert!(
            result_anthropic,
            "Anthropic should count cache tokens (180k total >= 160k threshold)"
        );

        let result_openai =
            should_compact_context_window(&info_with_cache, 200_000, 160_000, None, "openai");
        assert!(
            !result_openai,
            "OpenAI should NOT count cache tokens separately (150k total < 160k threshold)"
        );

        let result_minimax =
            should_compact_context_window(&info_with_cache, 200_000, 160_000, None, "minimax");
        assert!(
            !result_minimax,
            "MiniMax should NOT count cache tokens separately (150k total < 160k threshold)"
        );
    }

    #[test]
    fn test_get_next_truncation_range_none() {
        let messages: Vec<StorageMessage> = (0..10)
            .map(|i| {
                create_test_message(if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                })
            })
            .collect();

        let range = get_next_truncation_range(&messages, None, TruncationKeep::None);
        assert_eq!(range, (2, 9));
    }

    #[test]
    fn test_get_next_truncation_range_half() {
        let messages: Vec<StorageMessage> = (0..10)
            .map(|i| {
                create_test_message(if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                })
            })
            .collect();

        let range = get_next_truncation_range(&messages, None, TruncationKeep::Half);
        assert_eq!(range, (2, 5));
    }

    #[test]
    fn test_get_next_truncation_range_quarter() {
        let messages: Vec<StorageMessage> = (0..10)
            .map(|i| {
                create_test_message(if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                })
            })
            .collect();

        let range = get_next_truncation_range(&messages, None, TruncationKeep::LastQuarter);
        assert_eq!(range, (2, 7));
    }

    #[test]
    fn test_get_truncated_messages() {
        let messages: Vec<StorageMessage> = (0..10)
            .map(|i| {
                create_test_message(if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                })
            })
            .collect();

        let truncated = get_truncated_messages(&messages, Some((2, 5)), None);
        assert_eq!(truncated.len(), 6);
    }

    #[test]
    fn test_ensure_tool_results_follow_tool_use() {
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
            StorageMessage {
                id: None,
                role: MessageRole::Assistant,
                content: MessageContent::AssistantBlocks(vec![
                    AssistantContentBlock::Text(crate::providers::TextContentBlock {
                        text: "Using a tool".to_string(),
                        shared: crate::providers::SharedContentFields {
                            call_id: None,
                            signature: None,
                        },
                        reasoning_details: None,
                    }),
                    AssistantContentBlock::ToolUse(crate::providers::ToolUseBlock {
                        id: "tool_1".to_string(),
                        name: "read_file".to_string(),
                        input: serde_json::json!({"path": "/tmp/test"}),
                        shared: crate::providers::SharedContentFields {
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
                role: MessageRole::User,
                content: MessageContent::UserBlocks(vec![
                    UserContentBlock::ToolResult(crate::providers::ToolResultBlock {
                        tool_use_id: "tool_1".to_string(),
                        content: crate::providers::ToolResultContent::Text(
                            "file content here".to_string(),
                        ),
                        shared: crate::providers::SharedContentFields {
                            call_id: None,
                            signature: None,
                        },
                    }),
                    UserContentBlock::Text(crate::providers::TextContentBlock {
                        text: "Additional user text".to_string(),
                        shared: crate::providers::SharedContentFields {
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

        let truncated = get_truncated_messages(&messages, Some((2, 2)), None);
        assert_eq!(truncated.len(), 4);

        if let MessageContent::UserBlocks(blocks) = &truncated[2].content {
            assert_eq!(blocks.len(), 1);
            assert!(matches!(blocks[0], UserContentBlock::Text(_)));
        } else {
            panic!("Expected UserBlocks");
        }
    }

    #[test]
    fn test_auto_condense_safety_net() {
        let messages: Vec<StorageMessage> = (0..10)
            .map(|i| {
                create_test_message(if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                })
            })
            .collect();

        let info = ApiReqInfo {
            tokens_in: Some(100_000),
            tokens_out: Some(70_000),
            context_window: Some(200_000),
            ..Default::default()
        };

        // With auto_condense=true, truncation still happens at 70% safety threshold
        // 170k / 200k = 85% > 70%, so it should truncate
        let result = get_new_context_messages_and_metadata(
            &messages,
            Some(&info),
            None,
            true,
            None,
            "anthropic",
        );
        assert!(
            result.updated_conversation_history_deleted_range,
            "auto_condense should still truncate at 70% safety net"
        );
        assert!(result.truncated_conversation_history.len() < 10);

        // With auto_condense=false, truncation at 80% threshold
        let result = get_new_context_messages_and_metadata(
            &messages,
            Some(&info),
            None,
            false,
            None,
            "anthropic",
        );
        assert!(result.updated_conversation_history_deleted_range);
        assert!(result.truncated_conversation_history.len() < 10);

        // Below both thresholds: no truncation regardless of auto_condense
        let info_below = ApiReqInfo {
            tokens_in: Some(50_000),
            tokens_out: Some(30_000),
            context_window: Some(200_000),
            ..Default::default()
        };
        let result_auto = get_new_context_messages_and_metadata(
            &messages,
            Some(&info_below),
            None,
            true,
            None,
            "anthropic",
        );
        assert!(
            !result_auto.updated_conversation_history_deleted_range,
            "80k/200k=40% should not trigger truncation even with auto_condense"
        );
        let result_no_auto = get_new_context_messages_and_metadata(
            &messages,
            Some(&info_below),
            None,
            false,
            None,
            "anthropic",
        );
        assert!(
            !result_no_auto.updated_conversation_history_deleted_range,
            "80k/200k=40% should not trigger truncation"
        );
    }

    #[test]
    fn test_compaction_triggers_at_80_percent() {
        let messages: Vec<StorageMessage> = (0..10)
            .map(|i| {
                create_test_message(if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                })
            })
            .collect();

        // 200k context window, 80% = 160k threshold
        // 150k total tokens = below threshold, should NOT truncate
        let info_below = ApiReqInfo {
            tokens_in: Some(80_000),
            tokens_out: Some(70_000),
            context_window: Some(200_000),
            ..Default::default()
        };

        let result = get_new_context_messages_and_metadata(
            &messages,
            Some(&info_below),
            None,
            false,
            None,
            "anthropic",
        );
        assert!(!result.updated_conversation_history_deleted_range);

        // 170k total tokens = above threshold, SHOULD truncate
        let info_above = ApiReqInfo {
            tokens_in: Some(100_000),
            tokens_out: Some(70_000),
            context_window: Some(200_000),
            ..Default::default()
        };

        let result = get_new_context_messages_and_metadata(
            &messages,
            Some(&info_above),
            None,
            false,
            None,
            "anthropic",
        );
        assert!(result.updated_conversation_history_deleted_range);
    }
}
