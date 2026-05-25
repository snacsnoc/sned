//! Message format conversion for Gemini API.
//!
//! Converts Sned's internal message format (Anthropic-style) to Gemini's native format.
//! Ports behavior from `dirac/src/core/api/transform/gemini-format.ts`.

use crate::providers::{
    AssistantContentBlock, DocumentSource, ImageSource, MessageContent, MessageRole,
    StorageMessage, TextContentBlock, ToolResultContent, UserContentBlock,
};
use serde::{Deserialize, Serialize};

/// Gemini API content part.
///
/// Reference: https://ai.google.dev/api/generate-content#Part
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiPart {
    /// Inline text content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,

    /// Thinking/reasoning content (Gemini 2.5+).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought: Option<bool>,

    /// Thought signature for round-tripping (critical for Gemini 3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,

    /// Function call (tool use).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<GeminiFunctionCall>,

    /// Function response (tool result).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_response: Option<GeminiFunctionResponse>,

    /// Inline image data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline_data: Option<GeminiInlineData>,

    /// File data for URL-based media (images, audio, video).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_data: Option<GeminiFileData>,
}

/// Gemini function call structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiFunctionCall {
    /// Function name.
    pub name: String,

    /// Function arguments as a JSON object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,

    /// Unique ID for the function call (Gemini 3+).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// Gemini function response structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiFunctionResponse {
    /// Function name.
    pub name: String,

    /// Response content.
    pub response: serde_json::Value,

    /// Function call ID being responded to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// Gemini inline data for images.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiInlineData {
    /// MIME type of the data.
    #[serde(rename = "mimeType")]
    pub mime_type: String,

    /// Base64-encoded data.
    pub data: String,
}

/// Gemini file data for URL-based media.
/// Reference: https://ai.google.dev/api/generate-content#FileData
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiFileData {
    /// URI of the file (can be HTTP URL or Google Cloud Storage URI).
    pub file_uri: String,

    /// MIME type of the file.
    #[serde(rename = "mimeType")]
    pub mime_type: String,
}

/// Gemini API content message.
///
/// Reference: https://ai.google.dev/api/generate-content#Content
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiContent {
    /// Role: "user" or "model".
    pub role: String,

    /// Content parts.
    pub parts: Vec<GeminiPart>,
}

/// Build a tool_use_id → function_name mapping from conversation history.
fn build_tool_use_id_to_name(
    messages: &[StorageMessage],
) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::with_capacity(8);
    for msg in messages {
        if let MessageContent::AssistantBlocks(blocks) = &msg.content {
            for block in blocks {
                if let AssistantContentBlock::ToolUse(tu) = block {
                    map.insert(tu.id.clone(), tu.name.clone());
                }
            }
        }
    }
    map
}

/// Convert Sned messages to Gemini content format.
///
/// Key conversions:
/// - `role: "assistant"` → `role: "model"`
/// - `role: "user"` → `role: "user"`
/// - `thinking` blocks → `{ thought: true, text, thoughtSignature }`
/// - `tool_use` blocks → `{ functionCall: { id, name, args }, thoughtSignature }`
/// - `tool_result` blocks → `{ functionResponse: { id, name, response } }`
/// - Signatures carry forward across parts within a message
pub fn convert_to_gemini_contents(messages: &[StorageMessage]) -> Vec<GeminiContent> {
    let tool_use_id_to_name = build_tool_use_id_to_name(messages);

    messages
        .iter()
        .map(|msg| GeminiContent {
            role: match msg.role {
                MessageRole::User => "user".to_string(),
                MessageRole::Assistant => "model".to_string(),
            },
            parts: convert_content_to_gemini_parts(&msg.content, &tool_use_id_to_name),
        })
        .collect()
}

/// Convert Sned content to Gemini parts with signature carry-forward.
fn convert_content_to_gemini_parts(
    content: &MessageContent,
    tool_use_id_to_name: &std::collections::HashMap<String, String>,
) -> Vec<GeminiPart> {
    match content {
        MessageContent::Text(text) => {
            vec![GeminiPart {
                text: Some(text.clone()),
                thought: None,
                thought_signature: None,
                function_call: None,
                function_response: None,
                inline_data: None,
                file_data: None,
            }]
        }
        MessageContent::UserBlocks(blocks) => {
            let mut parts = Vec::new();
            let mut last_signature: Option<String> = None;

            for block in blocks {
                match block {
                    UserContentBlock::Text(TextContentBlock { text, shared, .. }) => {
                        if let Some(sig) = &shared.signature {
                            last_signature = Some(sig.clone());
                        }
                        parts.push(GeminiPart {
                            text: Some(text.clone()),
                            thought: None,
                            thought_signature: shared
                                .signature
                                .clone()
                                .or_else(|| last_signature.clone()),
                            function_call: None,
                            function_response: None,
                            inline_data: None,
                            file_data: None,
                        });
                    }
                    UserContentBlock::Image(img) => {
                        if let ImageSource::Base64 { media_type, data } = &img.source {
                            parts.push(GeminiPart {
                                text: None,
                                thought: None,
                                thought_signature: img.shared.signature.clone(),
                                function_call: None,
                                function_response: None,
                                inline_data: Some(GeminiInlineData {
                                    mime_type: media_type.clone(),
                                    data: data.clone(),
                                }),
                                file_data: None,
                            });
                        } else if let ImageSource::Url { url } = &img.source {
                            // URL-based images: use file_data format
                            // Infer MIME type from URL extension or default to image/jpeg
                            let mime_type = if url.ends_with(".png") {
                                "image/png"
                            } else if url.ends_with(".gif") {
                                "image/gif"
                            } else if url.ends_with(".webp") {
                                "image/webp"
                            } else {
                                "image/jpeg"
                            };
                            parts.push(GeminiPart {
                                text: None,
                                thought: None,
                                thought_signature: img.shared.signature.clone(),
                                function_call: None,
                                function_response: None,
                                inline_data: None,
                                file_data: Some(GeminiFileData {
                                    file_uri: url.clone(),
                                    mime_type: mime_type.to_string(),
                                }),
                            });
                        }
                    }
                    UserContentBlock::Document(doc) => {
                        match &doc.source {
                            DocumentSource::Text { text } => {
                                parts.push(GeminiPart {
                                    text: Some(text.clone()),
                                    thought: None,
                                    thought_signature: doc.shared.signature.clone(),
                                    function_call: None,
                                    function_response: None,
                                    inline_data: None,
                                    file_data: None,
                                });
                            }
                            DocumentSource::Base64 { media_type, data } => {
                                parts.push(GeminiPart {
                                    text: None,
                                    thought: None,
                                    thought_signature: doc.shared.signature.clone(),
                                    function_call: None,
                                    function_response: None,
                                    inline_data: Some(GeminiInlineData {
                                        mime_type: media_type.clone(),
                                        data: data.clone(),
                                    }),
                                    file_data: None,
                                });
                            }
                            DocumentSource::Url { url } => {
                                // Gemini doesn't support URL-based documents directly
                                // Convert to text placeholder
                                parts.push(GeminiPart {
                                    text: Some(format!("[Document: {}]", url)),
                                    thought: None,
                                    thought_signature: doc.shared.signature.clone(),
                                    function_call: None,
                                    function_response: None,
                                    inline_data: None,
                                    file_data: None,
                                });
                            }
                        }
                    }
                    UserContentBlock::ToolResult(tr) => {
                        let result_content = match &tr.content {
                            ToolResultContent::Text(text) => {
                                serde_json::Value::String(text.clone())
                            }
                            ToolResultContent::Blocks(blocks) => {
                                let text = blocks
                                    .iter()
                                    .filter_map(|b| match b {
                                        crate::providers::ToolResultContentBlock::Text { text } => {
                                            Some(text.clone())
                                        }
                                        _ => None,
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                serde_json::Value::String(text)
                            }
                        };

                        let function_name = tool_use_id_to_name
                            .get(&tr.tool_use_id)
                            .cloned()
                            .unwrap_or_else(|| tr.tool_use_id.clone());

                        parts.push(GeminiPart {
                            text: None,
                            thought: None,
                            thought_signature: None,
                            function_call: None,
                            function_response: Some(GeminiFunctionResponse {
                                name: function_name,
                                response: serde_json::json!({ "result": result_content }),
                                id: Some(tr.tool_use_id.clone()),
                            }),
                            inline_data: None,
                            file_data: None,
                        });
                    }
                }
            }

            parts
        }
        MessageContent::AssistantBlocks(blocks) => {
            let mut parts = Vec::new();
            let mut last_signature: Option<String> = None;

            for block in blocks {
                match block {
                    AssistantContentBlock::Text(TextContentBlock { text, shared, .. }) => {
                        if let Some(sig) = &shared.signature {
                            last_signature = Some(sig.clone());
                        }
                        parts.push(GeminiPart {
                            text: Some(text.clone()),
                            thought: None,
                            thought_signature: shared
                                .signature
                                .clone()
                                .or_else(|| last_signature.clone()),
                            function_call: None,
                            function_response: None,
                            inline_data: None,
                            file_data: None,
                        });
                    }
                    AssistantContentBlock::Thinking(thinking) => {
                        parts.push(GeminiPart {
                            text: Some(thinking.thinking.clone()),
                            thought: Some(true),
                            thought_signature: Some(thinking.signature.clone()),
                            function_call: None,
                            function_response: None,
                            inline_data: None,
                            file_data: None,
                        });
                    }
                    AssistantContentBlock::ToolUse(tu) => {
                        if let Some(sig) = &tu.shared.signature {
                            last_signature = Some(sig.clone());
                        }
                        parts.push(GeminiPart {
                            text: None,
                            thought: None,
                            thought_signature: tu.shared.signature.clone(),
                            function_call: Some(GeminiFunctionCall {
                                name: tu.name.clone(),
                                args: Some(tu.input.clone()),
                                id: Some(tu.id.clone()),
                            }),
                            function_response: None,
                            inline_data: None,
                            file_data: None,
                        });
                    }
                    AssistantContentBlock::Image(img) => {
                        if let ImageSource::Base64 { media_type, data } = &img.source {
                            parts.push(GeminiPart {
                                text: None,
                                thought: None,
                                thought_signature: img.shared.signature.clone(),
                                function_call: None,
                                function_response: None,
                                inline_data: Some(GeminiInlineData {
                                    mime_type: media_type.clone(),
                                    data: data.clone(),
                                }),
                                file_data: None,
                            });
                        } else if let ImageSource::Url { url } = &img.source {
                            // URL-based images in assistant messages: use file_data format
                            // Infer MIME type from URL extension or default to image/jpeg
                            let mime_type = if url.ends_with(".png") {
                                "image/png"
                            } else if url.ends_with(".gif") {
                                "image/gif"
                            } else if url.ends_with(".webp") {
                                "image/webp"
                            } else {
                                "image/jpeg"
                            };
                            parts.push(GeminiPart {
                                text: None,
                                thought: None,
                                thought_signature: img.shared.signature.clone(),
                                function_call: None,
                                function_response: None,
                                inline_data: None,
                                file_data: Some(GeminiFileData {
                                    file_uri: url.clone(),
                                    mime_type: mime_type.to_string(),
                                }),
                            });
                        }
                    }
                    AssistantContentBlock::Document(doc) => {
                        if let DocumentSource::Text { text } = &doc.source {
                            parts.push(GeminiPart {
                                text: Some(text.clone()),
                                thought: None,
                                thought_signature: doc.shared.signature.clone(),
                                function_call: None,
                                function_response: None,
                                inline_data: None,
                                file_data: None,
                            });
                        }
                    }
                    AssistantContentBlock::RedactedThinking(_) => {
                        // Skip redacted thinking - Gemini doesn't support it
                    }
                }
            }

            parts
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{SharedContentFields, ToolResultBlock, ToolUseBlock};

    #[test]
    fn test_convert_simple_text_messages() {
        let messages = vec![
            StorageMessage {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text("Hello".to_string()),
                model_info: None,
                metrics: None,
                ts: None,
            },
            StorageMessage {
                id: None,
                role: MessageRole::Assistant,
                content: MessageContent::Text("Hi there!".to_string()),
                model_info: None,
                metrics: None,
                ts: None,
            },
        ];

        let gemini = convert_to_gemini_contents(&messages);
        assert_eq!(gemini.len(), 2);
        assert_eq!(gemini[0].role, "user");
        assert_eq!(gemini[1].role, "model");
        assert_eq!(gemini[0].parts[0].text, Some("Hello".to_string()));
        assert_eq!(gemini[1].parts[0].text, Some("Hi there!".to_string()));
    }

    #[test]
    fn test_convert_tool_use_and_result() {
        let tool_use = AssistantContentBlock::ToolUse(ToolUseBlock {
            id: "call_abc123".to_string(),
            name: "read_file".to_string(),
            input: serde_json::json!({"path": "/test.txt"}),
            shared: SharedContentFields {
                call_id: None,
                signature: Some("sig_123".to_string()),
            },
            reasoning_details: None,
        });

        let tool_result = UserContentBlock::ToolResult(ToolResultBlock {
            tool_use_id: "call_abc123".to_string(),
            content: ToolResultContent::Text("file contents".to_string()),
            shared: SharedContentFields {
                call_id: None,
                signature: Some("sig_456".to_string()),
            },
        });

        let messages = vec![
            StorageMessage {
                id: None,
                role: MessageRole::Assistant,
                content: MessageContent::AssistantBlocks(vec![tool_use]),
                model_info: None,
                metrics: None,
                ts: None,
            },
            StorageMessage {
                id: None,
                role: MessageRole::User,
                content: MessageContent::UserBlocks(vec![tool_result]),
                model_info: None,
                metrics: None,
                ts: None,
            },
        ];

        let gemini = convert_to_gemini_contents(&messages);
        assert_eq!(gemini.len(), 2);

        // Check tool use
        let tool_call = &gemini[0].parts[0].function_call.as_ref().unwrap();
        assert_eq!(tool_call.name, "read_file");
        assert_eq!(tool_call.id, Some("call_abc123".to_string()));
        assert_eq!(
            gemini[0].parts[0].thought_signature,
            Some("sig_123".to_string())
        );

        // Check tool result
        let tool_response = &gemini[1].parts[0].function_response.as_ref().unwrap();
        assert_eq!(tool_response.name, "read_file");
        assert_eq!(tool_response.id, Some("call_abc123".to_string()));
        assert_eq!(
            tool_response.response,
            serde_json::json!({"result": "file contents"})
        );
    }

    #[test]
    fn test_convert_thinking_block() {
        let thinking = AssistantContentBlock::Thinking(crate::providers::ThinkingBlock {
            thinking: "Let me think about this...".to_string(),
            signature: "think_sig_789".to_string(),
            shared: SharedContentFields {
                call_id: None,
                signature: None,
            },
            summary: None,
        });

        let messages = vec![StorageMessage {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::AssistantBlocks(vec![thinking]),
            model_info: None,
            metrics: None,
            ts: None,
        }];

        let gemini = convert_to_gemini_contents(&messages);
        assert_eq!(gemini.len(), 1);
        let part = &gemini[0].parts[0];
        assert_eq!(part.thought, Some(true));
        assert_eq!(part.text, Some("Let me think about this...".to_string()));
        assert_eq!(part.thought_signature, Some("think_sig_789".to_string()));
    }

    #[test]
    fn test_signature_carry_forward() {
        let blocks = vec![
            AssistantContentBlock::Text(TextContentBlock {
                text: "First".to_string(),
                shared: SharedContentFields {
                    call_id: None,
                    signature: Some("sig_first".to_string()),
                },
                reasoning_details: None,
            }),
            AssistantContentBlock::Text(TextContentBlock {
                text: "Second".to_string(),
                shared: SharedContentFields {
                    call_id: None,
                    signature: None,
                },
                reasoning_details: None,
            }),
        ];

        let messages = vec![StorageMessage {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::AssistantBlocks(blocks),
            model_info: None,
            metrics: None,
            ts: None,
        }];

        let gemini = convert_to_gemini_contents(&messages);
        assert_eq!(gemini[0].parts.len(), 2);
        assert_eq!(
            gemini[0].parts[0].thought_signature,
            Some("sig_first".to_string())
        );
        // Second part should carry forward the signature
        assert_eq!(
            gemini[0].parts[1].thought_signature,
            Some("sig_first".to_string())
        );
    }

    #[test]
    fn test_tool_use_with_thought_signature() {
        // Test that tool use blocks preserve thought signatures
        // Critical for Gemini 3 function calling validation
        let tool_use = AssistantContentBlock::ToolUse(ToolUseBlock {
            id: "call_abc123".to_string(),
            name: "check_flight".to_string(),
            input: serde_json::json!({"flight_number": "AA100"}),
            shared: SharedContentFields {
                call_id: None,
                signature: Some("sig_flight_check".to_string()),
            },
            reasoning_details: None,
        });

        let messages = vec![StorageMessage {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::AssistantBlocks(vec![tool_use]),
            model_info: None,
            metrics: None,
            ts: None,
        }];

        let gemini = convert_to_gemini_contents(&messages);
        assert_eq!(gemini.len(), 1);
        assert_eq!(gemini[0].role, "model");

        let part = &gemini[0].parts[0];
        assert!(part.function_call.is_some());
        let fc = part.function_call.as_ref().unwrap();
        assert_eq!(fc.name, "check_flight");
        assert_eq!(fc.id, Some("call_abc123".to_string()));

        // Critical: thought signature must be preserved for Gemini 3 validation
        assert_eq!(part.thought_signature, Some("sig_flight_check".to_string()));
    }

    #[test]
    fn test_parallel_tool_calls_signature_handling() {
        // Test parallel function calls - only first FC has signature
        // Per Gemini 3 docs: "thought_signature is attached only to the first functionCall"
        let tool_use_1 = AssistantContentBlock::ToolUse(ToolUseBlock {
            id: "call_001".to_string(),
            name: "check_flight".to_string(),
            input: serde_json::json!({"flight_number": "AA100"}),
            shared: SharedContentFields {
                call_id: None,
                signature: Some("sig_first".to_string()),
            },
            reasoning_details: None,
        });

        let tool_use_2 = AssistantContentBlock::ToolUse(ToolUseBlock {
            id: "call_002".to_string(),
            name: "book_taxi".to_string(),
            input: serde_json::json!({"pickup": "airport"}),
            shared: SharedContentFields {
                call_id: None,
                signature: None, // Second FC has no signature
            },
            reasoning_details: None,
        });

        let messages = vec![StorageMessage {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::AssistantBlocks(vec![tool_use_1, tool_use_2]),
            model_info: None,
            metrics: None,
            ts: None,
        }];

        let gemini = convert_to_gemini_contents(&messages);
        assert_eq!(gemini.len(), 1);
        assert_eq!(gemini[0].parts.len(), 2);

        // First FC must have signature
        assert_eq!(
            gemini[0].parts[0].thought_signature,
            Some("sig_first".to_string())
        );
        assert_eq!(
            gemini[0].parts[0].function_call.as_ref().unwrap().name,
            "check_flight"
        );

        // Second FC must NOT have signature (parallel FCs don't inherit)
        assert_eq!(gemini[0].parts[1].thought_signature, None);
        assert_eq!(
            gemini[0].parts[1].function_call.as_ref().unwrap().name,
            "book_taxi"
        );
    }

    #[test]
    fn test_tool_response_no_signature() {
        // Tool responses (functionResponse) should NOT have signatures
        // Signatures are only on functionCall from the model
        let tool_result = UserContentBlock::ToolResult(ToolResultBlock {
            tool_use_id: "call_abc123".to_string(),
            content: ToolResultContent::Text("Flight AA100 is delayed".to_string()),
            shared: SharedContentFields {
                call_id: None,
                signature: None, // Tool results don't have signatures
            },
        });

        let messages = vec![StorageMessage {
            id: None,
            role: MessageRole::User,
            content: MessageContent::UserBlocks(vec![tool_result]),
            model_info: None,
            metrics: None,
            ts: None,
        }];

        let gemini = convert_to_gemini_contents(&messages);
        assert_eq!(gemini.len(), 1);
        assert_eq!(gemini[0].role, "user");

        let part = &gemini[0].parts[0];
        assert!(part.function_response.is_some());
        // Tool response should not have thought_signature
        assert_eq!(part.thought_signature, None);
    }

    #[test]
    fn test_thought_signature_round_trip() {
        // Full round-trip: thought → tool_use → tool_result → next turn
        // Per Gemini 3 docs, thought_signature must be preserved on functionCall,
        // must NOT appear on functionResponse, and must survive across turns.
        let thinking = AssistantContentBlock::Thinking(crate::providers::ThinkingBlock {
            thinking: "I need to check the weather.".to_string(),
            signature: "think_sig_round_trip".to_string(),
            shared: SharedContentFields {
                call_id: None,
                signature: None,
            },
            summary: None,
        });

        let tool_use = AssistantContentBlock::ToolUse(ToolUseBlock {
            id: "call_weather_001".to_string(),
            name: "get_weather".to_string(),
            input: serde_json::json!({"city": "Paris"}),
            shared: SharedContentFields {
                call_id: None,
                signature: Some("think_sig_round_trip".to_string()),
            },
            reasoning_details: None,
        });

        let tool_result = UserContentBlock::ToolResult(ToolResultBlock {
            tool_use_id: "call_weather_001".to_string(),
            content: ToolResultContent::Text("Sunny, 22C".to_string()),
            shared: SharedContentFields {
                call_id: None,
                signature: Some("should_be_ignored".to_string()),
            },
        });

        let next_user_msg = UserContentBlock::Text(TextContentBlock {
            text: "Thanks!".to_string(),
            shared: SharedContentFields {
                call_id: None,
                signature: None,
            },
            reasoning_details: None,
        });

        let messages = vec![
            StorageMessage {
                id: None,
                role: MessageRole::Assistant,
                content: MessageContent::AssistantBlocks(vec![thinking, tool_use]),
                model_info: None,
                metrics: None,
                ts: None,
            },
            StorageMessage {
                id: None,
                role: MessageRole::User,
                content: MessageContent::UserBlocks(vec![tool_result]),
                model_info: None,
                metrics: None,
                ts: None,
            },
            StorageMessage {
                id: None,
                role: MessageRole::User,
                content: MessageContent::UserBlocks(vec![next_user_msg]),
                model_info: None,
                metrics: None,
                ts: None,
            },
        ];

        let gemini = convert_to_gemini_contents(&messages);
        assert_eq!(gemini.len(), 3);

        // Turn 1: Assistant — thinking + functionCall
        let turn1 = &gemini[0];
        assert_eq!(turn1.role, "model");
        assert_eq!(turn1.parts.len(), 2);

        // Thinking part
        let think_part = &turn1.parts[0];
        assert_eq!(think_part.thought, Some(true));
        assert_eq!(
            think_part.thought_signature,
            Some("think_sig_round_trip".to_string())
        );

        // FunctionCall part — must have signature
        let fc_part = &turn1.parts[1];
        assert!(fc_part.function_call.is_some());
        assert_eq!(
            fc_part.thought_signature,
            Some("think_sig_round_trip".to_string())
        );
        let fc = fc_part.function_call.as_ref().unwrap();
        assert_eq!(fc.name, "get_weather");
        assert_eq!(fc.id, Some("call_weather_001".to_string()));

        // Turn 2: User — functionResponse — must NOT have thought_signature
        let turn2 = &gemini[1];
        assert_eq!(turn2.role, "user");
        let fr_part = &turn2.parts[0];
        assert!(fr_part.function_response.is_some());
        assert_eq!(
            fr_part.thought_signature, None,
            "functionResponse must NEVER have thought_signature"
        );
        let fr = fr_part.function_response.as_ref().unwrap();
        assert_eq!(fr.id, Some("call_weather_001".to_string()));
        assert_eq!(fr.name, "get_weather");

        // Turn 3: User — plain text
        let turn3 = &gemini[2];
        assert_eq!(turn3.role, "user");
        assert_eq!(turn3.parts[0].text, Some("Thanks!".to_string()));
    }

    #[test]
    fn test_tool_response_signature_always_none_even_when_shared_has_signature() {
        // Regression test: the storage layer may store signatures on ToolResult,
        // but the Gemini functionResponse must NEVER include them.
        let tool_result = UserContentBlock::ToolResult(ToolResultBlock {
            tool_use_id: "call_abc123".to_string(),
            content: ToolResultContent::Text("result".to_string()),
            shared: SharedContentFields {
                call_id: None,
                signature: Some("leaked_signature".to_string()),
            },
        });

        let messages = vec![StorageMessage {
            id: None,
            role: MessageRole::User,
            content: MessageContent::UserBlocks(vec![tool_result]),
            model_info: None,
            metrics: None,
            ts: None,
        }];

        let gemini = convert_to_gemini_contents(&messages);
        let part = &gemini[0].parts[0];
        assert!(
            part.function_response.is_some(),
            "Should have functionResponse"
        );
        assert_eq!(
            part.thought_signature, None,
            "thought_signature must be None on functionResponse even when shared.signature is Some"
        );
    }
}
