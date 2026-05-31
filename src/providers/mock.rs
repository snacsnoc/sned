use async_trait::async_trait;
use std::time::Duration;

use crate::providers::{
    ApiStream, ApiStreamChunk, ApiStreamTextChunk, ApiStreamToolCall, ApiStreamToolCallFunction,
    ApiStreamToolCallsChunk, ApiStreamUsageChunk, ModelInfo, Provider, ProviderError,
    ProviderModel, ProviderRequest,
};

/// A mock provider for testing that returns predefined responses.
pub struct MockProvider {
    responses: Vec<MockResponse>,
    response_index: std::sync::Mutex<usize>,
    repeat_last: bool,
}

#[derive(Debug, Clone)]
pub enum MockResponse {
    Text(String),
    ToolCalls(Vec<MockToolCall>),
    Stream(Vec<MockStreamEvent>),
    Error(String),
}

#[derive(Debug, Clone)]
pub enum MockStreamEvent {
    Chunk(ApiStreamChunk),
    Delay(Duration),
}

#[derive(Debug, Clone)]
pub struct MockToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

impl MockProvider {
    pub fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            responses,
            response_index: std::sync::Mutex::new(0),
            repeat_last: false,
        }
    }

    pub fn new_with_repeat(responses: Vec<MockResponse>) -> Self {
        Self {
            responses,
            response_index: std::sync::Mutex::new(0),
            repeat_last: true,
        }
    }

    pub fn single_text_response(text: &str) -> Self {
        Self::new(vec![MockResponse::Text(text.to_string())])
    }

    pub fn single_text_response_repeat(text: &str) -> Self {
        Self::new_with_repeat(vec![MockResponse::Text(text.to_string())])
    }

    pub fn single_tool_call(call_id: &str, name: &str, arguments: serde_json::Value) -> Self {
        Self::new(vec![MockResponse::ToolCalls(vec![MockToolCall {
            call_id: call_id.to_string(),
            name: name.to_string(),
            arguments,
        }])])
    }

    pub fn approval_scroll_scenario() -> Self {
        let mut first_text = String::new();
        for i in 1..=30 {
            first_text.push_str(&format!(
                "approval scroll line {:02}: this output fills the viewport\n",
                i
            ));
        }

        let mut second_text = String::new();
        for i in 31..=60 {
            second_text.push_str(&format!(
                "approval scroll line {:02}: keep scrolling through output\n",
                i
            ));
        }

        let execute_command = MockToolCall {
            call_id: "approval-scroll-exec".to_string(),
            name: "execute_command".to_string(),
            arguments: serde_json::json!({
                "commands": ["touch /tmp/sned-approval-scroll-smoke"]
            }),
        };
        let attempt_completion = MockToolCall {
            call_id: "approval-scroll-complete".to_string(),
            name: "attempt_completion".to_string(),
            arguments: serde_json::json!({
                "result": "approval-scroll smoke test complete"
            }),
        };

        Self::new_with_repeat(vec![
            MockResponse::Stream(vec![
                MockStreamEvent::Chunk(ApiStreamChunk::Text(ApiStreamTextChunk {
                    text: first_text,
                    id: None,
                    signature: None,
                })),
                MockStreamEvent::Delay(Duration::from_millis(250)),
                MockStreamEvent::Chunk(ApiStreamChunk::Text(ApiStreamTextChunk {
                    text: second_text,
                    id: None,
                    signature: None,
                })),
                MockStreamEvent::Delay(Duration::from_millis(250)),
                MockStreamEvent::Chunk(ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                    tool_call: ApiStreamToolCall {
                        call_id: Some(execute_command.call_id),
                        function: ApiStreamToolCallFunction {
                            id: None,
                            name: Some(execute_command.name),
                            arguments: Some(execute_command.arguments.to_string()),
                        },
                        signature: None,
                    },
                    id: None,
                    signature: None,
                })),
                MockStreamEvent::Chunk(ApiStreamChunk::Usage(ApiStreamUsageChunk {
                    input_tokens: 10,
                    output_tokens: 20,
                    cache_write_tokens: None,
                    cache_read_tokens: None,
                    reasoning_tokens: None,
                    thoughts_token_count: None,
                    total_cost: None,
                    stop_reason: None,
                    id: None,
                })),
            ]),
            MockResponse::ToolCalls(vec![attempt_completion]),
        ])
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn create_message(&self, _request: ProviderRequest) -> Result<ApiStream, ProviderError> {
        let (_index, response) = {
            let mut idx = self.response_index.lock().unwrap();
            let current = *idx;

            let response = if self.repeat_last && current >= self.responses.len() {
                self.responses.last().cloned()
            } else {
                self.responses.get(current).cloned()
            };

            *idx += 1;
            (current, response)
        };

        let stream: ApiStream = match response {
            Some(MockResponse::Text(text)) => {
                let chunks = vec![
                    ApiStreamChunk::Text(ApiStreamTextChunk {
                        text,
                        id: None,
                        signature: None,
                    }),
                    ApiStreamChunk::Usage(ApiStreamUsageChunk {
                        input_tokens: 10,
                        output_tokens: 20,
                        cache_write_tokens: None,
                        cache_read_tokens: None,
                        reasoning_tokens: None,
                        thoughts_token_count: None,
                        total_cost: None,
                        stop_reason: None,
                        id: None,
                    }),
                ];
                Box::pin(tokio_stream::iter(chunks))
            }
            Some(MockResponse::ToolCalls(calls)) => {
                if let Some(call) = calls.into_iter().next() {
                    let chunks = vec![
                        ApiStreamChunk::ToolCalls(ApiStreamToolCallsChunk {
                            tool_call: ApiStreamToolCall {
                                call_id: Some(call.call_id),
                                function: ApiStreamToolCallFunction {
                                    id: None,
                                    name: Some(call.name),
                                    arguments: Some(call.arguments.to_string()),
                                },
                                signature: None,
                            },
                            id: None,
                            signature: None,
                        }),
                        ApiStreamChunk::Usage(ApiStreamUsageChunk {
                            input_tokens: 10,
                            output_tokens: 20,
                            cache_write_tokens: None,
                            cache_read_tokens: None,
                            reasoning_tokens: None,
                            thoughts_token_count: None,
                            total_cost: None,
                            stop_reason: None,
                            id: None,
                        }),
                    ];
                    Box::pin(tokio_stream::iter(chunks))
                } else {
                    Box::pin(tokio_stream::iter(Vec::new()))
                }
            }
            Some(MockResponse::Stream(events)) => {
                let (tx, rx) = tokio::sync::mpsc::channel(events.len().max(1));
                tokio::spawn(async move {
                    for event in events {
                        match event {
                            MockStreamEvent::Chunk(chunk) => {
                                if tx.send(chunk).await.is_err() {
                                    break;
                                }
                            }
                            MockStreamEvent::Delay(delay) => {
                                tokio::time::sleep(delay).await;
                            }
                        }
                    }
                });
                Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
            }
            Some(MockResponse::Error(msg)) => {
                return Err(ProviderError::ApiError(msg));
            }
            None => Box::pin(tokio_stream::iter(Vec::new())),
        };

        Ok(stream)
    }

    fn get_model(&self) -> ProviderModel {
        ProviderModel {
            id: "mock-model".to_string(),
            info: ModelInfo {
                name: Some("Mock Model".to_string()),
                max_tokens: Some(4096),
                context_window: Some(200_000),
                supports_images: Some(false),
                supports_prompt_cache: false,
                supports_reasoning: Some(false),
                input_price: None,
                output_price: None,
                image_output_price: None,
                thinking_config: None,
                supports_global_endpoint: None,
                cache_writes_price: None,
                cache_reads_price: None,
                description: Some("Mock model for testing".to_string()),
                tiers: None,
                temperature: None,
                top_p: None,
                top_k: None,
                supports_tools: Some(true),
                api_format: None,
            },
        }
    }

    fn name(&self) -> &str {
        "mock"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderRequest;
    use futures::StreamExt;

    #[tokio::test]
    async fn test_mock_provider_error_response() {
        let provider = MockProvider::new(vec![MockResponse::Error("test error".to_string())]);
        let request = ProviderRequest {
            system_prompt: "test".to_string(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let result = provider.create_message(request).await;
        assert!(result.is_err());
        if let Err(ProviderError::ApiError(msg)) = result {
            assert_eq!(msg, "test error");
        } else {
            panic!("Expected ApiError");
        }
    }

    #[tokio::test]
    async fn test_mock_provider_text_response() {
        let provider = MockProvider::single_text_response("hello");
        let request = ProviderRequest {
            system_prompt: "test".to_string(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let result = provider.create_message(request).await;
        assert!(result.is_ok());
        let mut stream = result.unwrap();
        let chunk = stream.next().await.unwrap();
        match chunk {
            ApiStreamChunk::Text(t) => assert_eq!(t.text, "hello"),
            _ => panic!("Expected Text chunk"),
        }
    }

    #[tokio::test]
    async fn test_mock_provider_stream_response() {
        let provider = MockProvider::new(vec![MockResponse::Stream(vec![
            MockStreamEvent::Chunk(ApiStreamChunk::Text(ApiStreamTextChunk {
                text: "hello".to_string(),
                id: None,
                signature: None,
            })),
            MockStreamEvent::Delay(Duration::from_millis(1)),
            MockStreamEvent::Chunk(ApiStreamChunk::Usage(ApiStreamUsageChunk {
                input_tokens: 1,
                output_tokens: 2,
                cache_write_tokens: None,
                cache_read_tokens: None,
                reasoning_tokens: None,
                thoughts_token_count: None,
                total_cost: None,
                stop_reason: None,
                id: None,
            })),
        ])]);
        let request = ProviderRequest {
            system_prompt: "test".to_string(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            use_response_api: None,
            max_tokens: None,
        };

        let result = provider.create_message(request).await;
        assert!(result.is_ok());
        let mut stream = result.unwrap();
        let chunk = stream.next().await.unwrap();
        match chunk {
            ApiStreamChunk::Text(t) => assert_eq!(t.text, "hello"),
            _ => panic!("Expected Text chunk"),
        }
    }
}
