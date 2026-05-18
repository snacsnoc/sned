use futures::StreamExt;
use serde_json::json;
use sned::providers::mock::{MockProvider, MockResponse, MockToolCall};
use sned::providers::{ApiStreamChunk, Provider, ProviderRequest};

fn empty_request() -> ProviderRequest {
    ProviderRequest {
        system_prompt: String::new(),
        messages: vec![],
        tools: None,
        tool_choice: None,
        use_response_api: None,
    }
}

#[tokio::test]
async fn test_mock_provider_basic() {
    let provider = MockProvider::single_text_response("Hello, I can help you with that!");
    let model = provider.get_model();

    assert_eq!(model.id, "mock-model");
    assert!(model.info.supports_tools.unwrap_or(false));
}

#[tokio::test]
async fn test_mock_provider_text_response() {
    let provider = MockProvider::single_text_response("Test response");
    let model = provider.get_model();

    assert_eq!(model.id, "mock-model");
    assert!(model.info.supports_tools.unwrap_or(false));
}

#[tokio::test]
async fn test_mock_provider_tool_call_response() {
    let provider = MockProvider::single_tool_call(
        "call_1",
        "execute_command",
        json!({"command": "echo hello"}),
    );

    let stream = provider.create_message(empty_request()).await.unwrap();
    let chunks: Vec<ApiStreamChunk> = stream.collect().await;

    let has_tool_call = chunks
        .iter()
        .any(|c| matches!(c, ApiStreamChunk::ToolCalls(_)));
    let has_usage = chunks.iter().any(|c| matches!(c, ApiStreamChunk::Usage(_)));
    assert!(has_tool_call, "expected a ToolCalls chunk");
    assert!(has_usage, "expected a Usage chunk");
}

#[tokio::test]
async fn test_mock_provider_multiple_responses() {
    let provider = MockProvider::new(vec![
        MockResponse::Text("First response".to_string()),
        MockResponse::ToolCalls(vec![MockToolCall {
            call_id: "call_1".to_string(),
            name: "read_file".to_string(),
            arguments: json!({"path": "/tmp/test.txt"}),
        }]),
        MockResponse::Text("Final response".to_string()),
    ]);

    let model = provider.get_model();
    assert_eq!(model.id, "mock-model");
}
