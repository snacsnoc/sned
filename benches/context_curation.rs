// Context curation performance benchmark
// Measures ContextManager operations at different message counts

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use std::time::Duration;

use sned::core::context::context_manager::{
    ApiReqInfo, CompactedSummary, TruncationKeep, get_new_context_messages_and_metadata,
    get_next_truncation_range, get_truncated_messages, should_compact_context_window,
};
use sned::providers::{MessageContent, MessageRole, StorageMessage};

/// Create test messages for benchmarking
fn create_test_messages(count: usize) -> Vec<StorageMessage> {
    (0..count)
        .map(|i| StorageMessage {
            id: None,
            role: if i % 2 == 0 {
                MessageRole::User
            } else {
                MessageRole::Assistant
            },
            content: MessageContent::Text(format!("Test message {}", i)),
            model_info: None,
            metrics: None,
            ts: None,
        })
        .collect()
}

/// Create API request info for benchmarking
fn create_api_req_info(tokens_in: u32, tokens_out: u32, context_window: u64) -> ApiReqInfo {
    ApiReqInfo {
        request: None,
        tokens_in: Some(tokens_in),
        tokens_out: Some(tokens_out),
        cache_writes: None,
        cache_reads: None,
        reasoning_tokens: None,
        cost: None,
        context_window: Some(context_window),
        context_usage_percentage: None,
    }
}

/// Benchmark should_compact_context_window()
fn benchmark_should_compact(c: &mut Criterion) {
    let mut group = c.benchmark_group("context_curation/should_compact");
    group.throughput(Throughput::Elements(1));
    group.measurement_time(Duration::from_secs(5));
    group.sample_size(50);

    // Test with different token counts
    for (tokens_in, tokens_out) in [(50_000, 10_000), (100_000, 20_000), (200_000, 40_000)] {
        group.bench_function(
            BenchmarkId::new("token_check", format!("{}+{}", tokens_in, tokens_out)),
            |b| {
                b.iter(|| {
                    let api_info = create_api_req_info(tokens_in, tokens_out, 256_000);
                    let should_compact = should_compact_context_window(
                        &api_info,
                        256_000,
                        200_000,
                        Some(0.8),
                        "anthropic",
                    );
                    black_box(should_compact);
                })
            },
        );
    }

    group.finish();
}

/// Benchmark get_next_truncation_range()
fn benchmark_truncation_range(c: &mut Criterion) {
    let mut group = c.benchmark_group("context_curation/truncation_range");
    group.throughput(Throughput::Elements(1));
    group.measurement_time(Duration::from_secs(5));
    group.sample_size(50);

    let messages = create_test_messages(200);

    // Test different keep strategies
    for keep in [
        TruncationKeep::None,
        TruncationKeep::LastTwo,
        TruncationKeep::Half,
        TruncationKeep::LastQuarter,
    ] {
        group.bench_function(
            BenchmarkId::new("calculate_range", format!("{:?}", keep)),
            |b| {
                b.iter(|| {
                    let range = get_next_truncation_range(&messages, Some((2, 50)), keep);
                    black_box(range);
                })
            },
        );
    }

    group.finish();
}

/// Benchmark get_truncated_messages()
fn benchmark_truncate_messages(c: &mut Criterion) {
    let mut group = c.benchmark_group("context_curation/truncate_messages");
    group.throughput(Throughput::Elements(1));
    group.measurement_time(Duration::from_secs(10));
    group.sample_size(50);

    // Test with different message counts
    for msg_count in [20, 100, 500] {
        let messages = create_test_messages(msg_count);
        let deleted_range = Some((2, msg_count / 2));

        group.bench_function(
            BenchmarkId::new("apply_truncation", format!("{} messages", msg_count)),
            |b| {
                b.iter(|| {
                    let truncated = get_truncated_messages(&messages, deleted_range, None);
                    black_box(truncated.len());
                })
            },
        );
    }

    group.finish();
}

/// Benchmark get_new_context_messages_and_metadata()
fn benchmark_get_new_context(c: &mut Criterion) {
    let mut group = c.benchmark_group("context_curation/get_new_context");
    group.throughput(Throughput::Elements(1));
    group.measurement_time(Duration::from_secs(10));
    group.sample_size(50);

    // Test with different message counts and token usage
    for (msg_count, tokens_in, tokens_out) in [
        (20, 20_000, 4_000),
        (100, 100_000, 20_000),
        (500, 200_000, 40_000),
    ] {
        let messages = create_test_messages(msg_count);
        let api_info = create_api_req_info(tokens_in, tokens_out, 256_000);

        group.bench_function(
            BenchmarkId::new("full_context_update", format!("{} messages", msg_count)),
            |b| {
                b.iter(|| {
                    let result = get_new_context_messages_and_metadata(
                        &messages,
                        Some(&api_info),
                        None,
                        true,
                        None,
                        "anthropic",
                    );
                    black_box(result.truncated_conversation_history.len());
                })
            },
        );
    }

    group.finish();
}

/// Benchmark with compacted summary
fn benchmark_with_compacted_summary(c: &mut Criterion) {
    let mut group = c.benchmark_group("context_curation/with_summary");
    group.throughput(Throughput::Elements(1));
    group.measurement_time(Duration::from_secs(10));
    group.sample_size(50);

    let messages = create_test_messages(100);
    let summary = CompactedSummary::new("Previous conversation summary".to_string(), 50);

    group.bench_function("truncation_with_summary", |b| {
        b.iter(|| {
            let truncated = get_truncated_messages(&messages, Some((2, 50)), Some(&summary));
            black_box(truncated.len());
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    benchmark_should_compact,
    benchmark_truncation_range,
    benchmark_truncate_messages,
    benchmark_get_new_context,
    benchmark_with_compacted_summary,
);

criterion_main!(benches);
