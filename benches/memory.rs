// Memory baseline benchmark using criterion
// Measures heap allocations at key lifecycle points

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use std::time::Duration;

/// Measure heap allocations for basic data structure initialization
fn benchmark_cli_init(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory/cli_init");
    group.throughput(Throughput::Elements(1));
    group.measurement_time(Duration::from_secs(10));

    group.bench_function(BenchmarkId::new("heap_allocations", "cold"), |b| {
        b.iter(|| {
            // Simulate CLI initialization with basic data structures
            let mut config_map = std::collections::HashMap::new();
            config_map.insert("task_id".to_string(), "test".to_string());
            config_map.insert("mode".to_string(), "act".to_string());

            let mut history = Vec::new();
            for i in 0..10 {
                history.push(format!("message_{}", i));
            }

            black_box((config_map, history));
        })
    });

    group.finish();
}

/// Measure heap allocations for provider config and client creation
fn benchmark_provider_creation(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory/provider_creation");
    group.throughput(Throughput::Elements(1));
    group.measurement_time(Duration::from_secs(10));

    // Test OpenAI config creation
    group.bench_function(BenchmarkId::new("openai_config", "creation"), |b| {
        b.iter(|| {
            let config = sned::providers::openai::OpenAiConfig {
                api_key: "test_key".to_string(),
                base_url: Some("https://api.openai.com/v1".to_string()),
                model_id: "gpt-4o".to_string(),
                model_info: None,
                reasoning_effort: None,
                custom_headers: None,
                provider_name: None,
            };
            black_box(config);
        })
    });

    // Test Anthropic config creation
    group.bench_function(BenchmarkId::new("anthropic_config", "creation"), |b| {
        b.iter(|| {
            let config = sned::providers::anthropic::AnthropicConfig {
                api_key: "test_key".to_string(),
                base_url: Some("https://api.anthropic.com/v1".to_string()),
                model_id: "claude-sonnet-4-20250514".to_string(),
                model_info: None,
                thinking_budget_tokens: Some(1024),
            };
            black_box(config);
        })
    });

    group.finish();
}

/// Measure heap allocations for message and content block creation
fn benchmark_message_creation(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory/message_creation");
    group.throughput(Throughput::Elements(1));
    group.measurement_time(Duration::from_secs(10));

    group.bench_function(BenchmarkId::new("storage_message", "creation"), |b| {
        b.iter(|| {
            use sned::providers::{MessageContent, MessageRole, StorageMessage};

            let message = StorageMessage {
                id: None,
                role: MessageRole::Assistant,
                content: MessageContent::Text("Test message".to_string()),
                model_info: None,
                metrics: None,
                ts: None,
            };
            black_box(message);
        })
    });

    group.finish();
}

/// Measure heap allocations for context message accumulation
fn benchmark_context_management(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory/context_management");
    group.throughput(Throughput::Elements(1));
    group.measurement_time(Duration::from_secs(15));

    // Test with different message counts
    for msg_count in [10, 50, 100] {
        group.bench_function(BenchmarkId::new("context_messages", msg_count), |b| {
            b.iter(|| {
                use sned::providers::{MessageContent, MessageRole, StorageMessage};

                let mut messages = Vec::with_capacity(msg_count);
                for i in 0..msg_count {
                    messages.push(StorageMessage {
                        id: None,
                        role: if i % 2 == 0 {
                            MessageRole::User
                        } else {
                            MessageRole::Assistant
                        },
                        content: MessageContent::Text(format!("Message {}", i)),
                        model_info: None,
                        metrics: None,
                        ts: None,
                    });
                }
                black_box(messages);
            })
        });
    }

    group.finish();
}

/// Measure heap allocations for file content with anchor tracking
fn benchmark_file_editing(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory/file_editing");
    group.throughput(Throughput::Elements(1));
    group.measurement_time(Duration::from_secs(15));

    // Test with different file sizes
    for line_count in [100, 1000, 10000] {
        group.bench_function(
            BenchmarkId::new("file_content_with_anchors", line_count),
            |b| {
                b.iter(|| {
                    // Simulate file content
                    let mut content = String::with_capacity(line_count * 50);
                    for i in 0..line_count {
                        content.push_str(&format!(
                            "Line {}: This is some test content for anchor hashing\n",
                            i
                        ));
                    }

                    // Simulate anchor state tracking (one anchor per 10 lines)
                    let anchor_count = line_count / 10;
                    let mut anchors = Vec::with_capacity(anchor_count);
                    for i in 0..anchor_count {
                        anchors.push(format!("anchor_{}", i));
                    }

                    black_box((content, anchors));
                })
            },
        );
    }

    group.finish();
}

/// Measure heap allocations for tool call streaming simulation
fn benchmark_tool_streaming(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory/tool_streaming");
    group.throughput(Throughput::Elements(1));
    group.measurement_time(Duration::from_secs(15));

    group.bench_function(
        BenchmarkId::new("tool_call_accumulation", "streaming"),
        |b| {
            b.iter(|| {
                // Simulate streaming in a tool call in chunks
                let mut tool_name = String::new();
                let tool_name_chunks = ["read", "_file"];
                for chunk in &tool_name_chunks {
                    tool_name.push_str(chunk);
                }

                let mut arguments = String::new();
                let args_chunks = ["{", "\"path", "\":", "\"", "test", ".rs", "\"", "}"];
                for chunk in &args_chunks {
                    arguments.push_str(chunk);
                }

                black_box((tool_name, arguments));
            })
        },
    );

    group.finish();
}

criterion_group!(
    benches,
    benchmark_cli_init,
    benchmark_provider_creation,
    benchmark_message_creation,
    benchmark_context_management,
    benchmark_file_editing,
    benchmark_tool_streaming,
);

criterion_main!(benches);
