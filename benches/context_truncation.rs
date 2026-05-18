use criterion::{Criterion, black_box, criterion_group, criterion_main};
use sned::core::context::context_manager::{
    TruncationKeep, get_next_truncation_range, get_truncated_messages,
};
use sned::providers::{MessageContent, MessageRole, StorageMessage};

/// Create test messages with specified count
fn create_messages(count: usize) -> Vec<StorageMessage> {
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
            content: MessageContent::Text(format!("Message {} with conversation content", i)),
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

fn bench_truncation_range_small(c: &mut Criterion) {
    let messages = create_messages(20);

    c.bench_function("truncation_range_20_messages", |b| {
        b.iter(|| {
            let range = get_next_truncation_range(black_box(&messages), None, TruncationKeep::Half);
            black_box(range);
        })
    });
}

fn bench_truncation_range_medium(c: &mut Criterion) {
    let messages = create_messages(200);

    c.bench_function("truncation_range_200_messages", |b| {
        b.iter(|| {
            let range = get_next_truncation_range(black_box(&messages), None, TruncationKeep::Half);
            black_box(range);
        })
    });
}

fn bench_truncation_range_large(c: &mut Criterion) {
    let messages = create_messages(2_000);

    c.bench_function("truncation_range_2000_messages", |b| {
        b.iter(|| {
            let range = get_next_truncation_range(black_box(&messages), None, TruncationKeep::Half);
            black_box(range);
        })
    });
}

fn bench_get_truncated_messages_small(c: &mut Criterion) {
    let messages = create_messages(20);
    let range = get_next_truncation_range(&messages, None, TruncationKeep::Half);

    c.bench_function("truncated_messages_20_with_range", |b| {
        b.iter(|| {
            let truncated = get_truncated_messages(black_box(&messages), Some(range), None);
            black_box(truncated);
        })
    });
}

fn bench_get_truncated_messages_medium(c: &mut Criterion) {
    let messages = create_messages(200);
    let range = get_next_truncation_range(&messages, None, TruncationKeep::Half);

    c.bench_function("truncated_messages_200_with_range", |b| {
        b.iter(|| {
            let truncated = get_truncated_messages(black_box(&messages), Some(range), None);
            black_box(truncated);
        })
    });
}

fn bench_get_truncated_messages_large(c: &mut Criterion) {
    let messages = create_messages(2_000);
    let range = get_next_truncation_range(&messages, None, TruncationKeep::Half);

    c.bench_function("truncated_messages_2000_with_range", |b| {
        b.iter(|| {
            let truncated = get_truncated_messages(black_box(&messages), Some(range), None);
            black_box(truncated);
        })
    });
}

criterion_group!(
    benches,
    bench_truncation_range_small,
    bench_truncation_range_medium,
    bench_truncation_range_large,
    bench_get_truncated_messages_small,
    bench_get_truncated_messages_medium,
    bench_get_truncated_messages_large,
);
criterion_main!(benches);
