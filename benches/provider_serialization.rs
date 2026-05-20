use criterion::{Criterion, black_box, criterion_group, criterion_main};
use sned::providers::*;

fn bench_message_serialization(c: &mut Criterion) {
    let message = StorageMessage {
        id: Some("msg_123456789".to_string()),
        role: MessageRole::User,
        content: MessageContent::Text(
            "Please help me refactor this function to be more efficient. \
             The current implementation uses nested loops and I think it could be optimized. \
             Here is the code I want to improve."
                .to_string(),
        ),
        model_info: None,
        metrics: None,
        ts: Some(1700000000),
    };

    c.bench_function("message_serialize", |b| {
        b.iter(|| {
            let json = serde_json::to_string(black_box(&message)).unwrap();
            black_box(json);
        })
    });

    let json = serde_json::to_string(&message).unwrap();
    c.bench_function("message_deserialize", |b| {
        b.iter(|| {
            let _: StorageMessage = serde_json::from_str(black_box(&json)).unwrap();
        })
    });
}

fn bench_content_block_serialization(c: &mut Criterion) {
    let blocks = vec![
        UserContentBlock::Text(TextContentBlock {
            text: "First paragraph of user input".to_string(),
            shared: SharedContentFields {
                call_id: None,
                signature: None,
            },
            reasoning_details: None,
        }),
        UserContentBlock::Text(TextContentBlock {
            text: "Second paragraph with more context".to_string(),
            shared: SharedContentFields {
                call_id: None,
                signature: None,
            },
            reasoning_details: None,
        }),
    ];

    c.bench_function("content_blocks_serialize", |b| {
        b.iter(|| {
            let json = serde_json::to_string(black_box(&blocks)).unwrap();
            black_box(json);
        })
    });

    let json = serde_json::to_string(&blocks).unwrap();
    c.bench_function("content_blocks_deserialize", |b| {
        b.iter(|| {
            let _: Vec<UserContentBlock> = serde_json::from_str(black_box(&json)).unwrap();
        })
    });
}

fn bench_tool_use_serialization(c: &mut Criterion) {
    let tool_use = AssistantContentBlock::ToolUse(ToolUseBlock {
        id: "tool_abc123".to_string(),
        name: "edit_file".to_string(),
        input: serde_json::json!({
            "path": "/project/src/main.rs",
            "edits": [
                {
                    "old_string": "fn old_function() {}",
                    "new_string": "fn new_function() -> i32 { 42 }"
                }
            ]
        }),
        shared: SharedContentFields {
            call_id: Some("call_456".to_string()),
            signature: None,
        },
        reasoning_details: None,
    });

    c.bench_function("tool_use_serialize", |b| {
        b.iter(|| {
            let json = serde_json::to_string(black_box(&tool_use)).unwrap();
            black_box(json);
        })
    });

    let json = serde_json::to_string(&tool_use).unwrap();
    c.bench_function("tool_use_deserialize", |b| {
        b.iter(|| {
            let _: AssistantContentBlock = serde_json::from_str(black_box(&json)).unwrap();
        })
    });
}

fn bench_request_serialization(c: &mut Criterion) {
    let request = ProviderRequest {
        system_prompt: "You are a helpful coding assistant.".to_string(),
        messages: vec![
            StorageMessage {
                id: Some("msg_1".to_string()),
                role: MessageRole::User,
                content: MessageContent::UserBlocks(vec![UserContentBlock::Text(
                    TextContentBlock {
                        text: "Hello, can you help me with a coding task?".to_string(),
                        shared: SharedContentFields {
                            call_id: None,
                            signature: None,
                        },
                        reasoning_details: None,
                    },
                )]),
                model_info: None,
                metrics: None,
                ts: Some(1700000000),
            },
            StorageMessage {
                id: Some("msg_2".to_string()),
                role: MessageRole::Assistant,
                content: MessageContent::AssistantBlocks(vec![AssistantContentBlock::Text(
                    TextContentBlock {
                        text: "I'd be happy to help! What would you like to work on?".to_string(),
                        shared: SharedContentFields {
                            call_id: None,
                            signature: None,
                        },
                        reasoning_details: None,
                    },
                )]),
                model_info: None,
                metrics: None,
                ts: Some(1700000001),
            },
        ],
        tools: None,
        tool_choice: None,
        use_response_api: Some(false),
        max_tokens: None,
    };

    c.bench_function("provider_request_serialize", |b| {
        b.iter(|| {
            let json = serde_json::to_string(black_box(&request)).unwrap();
            black_box(json);
        })
    });

    let json = serde_json::to_string(&request).unwrap();
    c.bench_function("provider_request_deserialize", |b| {
        b.iter(|| {
            let _: ProviderRequest = serde_json::from_str(black_box(&json)).unwrap();
        })
    });
}

criterion_group!(
    benches,
    bench_message_serialization,
    bench_content_block_serialization,
    bench_tool_use_serialization,
    bench_request_serialization
);
criterion_main!(benches);
