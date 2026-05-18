// Provider streaming throughput benchmark
// Measures SSE parsing throughput for OpenAI and Anthropic providers

use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use std::time::Duration;

use sned::providers::SseLineBuffer;

/// Generate synthetic OpenAI SSE stream data
fn generate_openai_sse(size_bytes: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(size_bytes);
    let mut offset = 0;
    let mut tool_call_idx = 0;

    while offset < size_bytes {
        // Simulate a mix of text deltas and occasional tool calls
        if offset % 500 < 100 && offset > 0 {
            // Tool call event (~10% of events)
            let event = format!(
                "data: {{\"id\":\"chatcmpl-{}\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"gpt-4\",\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":{},\"id\":\"call_{}\",\"type\":\"function\",\"function\":{{\"name\":\"search\",\"arguments\":\"{{\\\"query\\\":\\\"test\\\"}}\"}}]}}}}}}]}}\n\n",
                offset, tool_call_idx, tool_call_idx
            );
            tool_call_idx += 1;
            data.extend_from_slice(event.as_bytes());
        } else {
            // Text delta event (~90% of events)
            let event = format!(
                "data: {{\"id\":\"chatcmpl-{}\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"gpt-4\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"Hello world this is some test content to fill up the buffer \"}}}}]}}\n\n",
                offset
            );
            data.extend_from_slice(event.as_bytes());
        }
        offset += 200; // Each event is roughly 200 bytes
    }

    data
}

/// Generate synthetic Anthropic SSE stream data
fn generate_anthropic_sse(size_bytes: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(size_bytes);
    let mut offset = 0;
    let mut tool_call_idx = 0;

    while offset < size_bytes {
        // Simulate a mix of text deltas and tool calls
        if offset % 500 < 100 && offset > 0 {
            // Tool use event (~10% of events)
            let event = format!(
                "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":{},\"content_block\":{{\"type\":\"tool_use\",\"id\":\"toolu_{}\",\"name\":\"search\",\"input\":{{\"query\":\"test\"}}}}}}\n\n",
                tool_call_idx, tool_call_idx
            );
            tool_call_idx += 1;
            data.extend_from_slice(event.as_bytes());
        } else {
            // Text delta event (~90% of events)
            let event = format!(
                "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"Hello world this is some test content to fill up the buffer \"}}}}\n\n"
            );
            data.extend_from_slice(event.as_bytes());
        }
        offset += 200; // Each event is roughly 200 bytes
    }

    data
}

/// Benchmark OpenAI SSE line buffer push_chunk
fn bench_openai_sse_buffer(c: &mut Criterion) {
    let mut group = c.benchmark_group("provider_stream/openai_sse_buffer");
    group.measurement_time(Duration::from_secs(10));
    group.sample_size(50);

    for size in [1024, 10 * 1024, 100 * 1024, 1024 * 1024] {
        let sse_data = generate_openai_sse(size);
        group.throughput(Throughput::Bytes(sse_data.len() as u64));

        group.bench_function(format!("push_chunk_{}", size), |b| {
            b.iter(|| {
                let mut buffer = SseLineBuffer::default();
                let lines = buffer.push_chunk(black_box(&sse_data));
                black_box(lines.len());
            })
        });
    }

    group.finish();
}

/// Benchmark Anthropic SSE line buffer push_chunk
fn bench_anthropic_sse_buffer(c: &mut Criterion) {
    let mut group = c.benchmark_group("provider_stream/anthropic_sse_buffer");
    group.measurement_time(Duration::from_secs(10));
    group.sample_size(50);

    for size in [1024, 10 * 1024, 100 * 1024, 1024 * 1024] {
        let sse_data = generate_anthropic_sse(size);
        group.throughput(Throughput::Bytes(sse_data.len() as u64));

        group.bench_function(format!("push_chunk_{}", size), |b| {
            b.iter(|| {
                let mut buffer = SseLineBuffer::default();
                let lines = buffer.push_chunk(black_box(&sse_data));
                black_box(lines.len());
            })
        });
    }

    group.finish();
}

/// Benchmark chunked SSE parsing (simulating network chunks)
fn bench_openai_chunked_parsing(c: &mut Criterion) {
    let mut group = c.benchmark_group("provider_stream/openai_chunked");
    group.measurement_time(Duration::from_secs(10));
    group.sample_size(50);

    for size in [10 * 1024, 100 * 1024, 1024 * 1024] {
        let sse_data = generate_openai_sse(size);
        let chunk_size = 1024; // Simulate 1KB network chunks
        group.throughput(Throughput::Bytes(sse_data.len() as u64));

        group.bench_function(format!("chunked_parse_{}", size), |b| {
            b.iter(|| {
                let mut buffer = SseLineBuffer::default();
                let mut total_lines = 0;
                for chunk in sse_data.chunks(chunk_size) {
                    let lines = buffer.push_chunk(black_box(chunk));
                    total_lines += lines.len();
                }
                black_box(total_lines);
            })
        });
    }

    group.finish();
}

/// Benchmark chunked Anthropic SSE parsing
fn bench_anthropic_chunked_parsing(c: &mut Criterion) {
    let mut group = c.benchmark_group("provider_stream/anthropic_chunked");
    group.measurement_time(Duration::from_secs(10));
    group.sample_size(50);

    for size in [10 * 1024, 100 * 1024, 1024 * 1024] {
        let sse_data = generate_anthropic_sse(size);
        let chunk_size = 1024; // Simulate 1KB network chunks
        group.throughput(Throughput::Bytes(sse_data.len() as u64));

        group.bench_function(format!("chunked_parse_{}", size), |b| {
            b.iter(|| {
                let mut buffer = SseLineBuffer::default();
                let mut total_lines = 0;
                for chunk in sse_data.chunks(chunk_size) {
                    let lines = buffer.push_chunk(black_box(chunk));
                    total_lines += lines.len();
                }
                black_box(total_lines);
            })
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_openai_sse_buffer,
    bench_anthropic_sse_buffer,
    bench_openai_chunked_parsing,
    bench_anthropic_chunked_parsing,
);

criterion_main!(benches);
