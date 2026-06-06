use criterion::{Criterion, criterion_group, criterion_main};
use sned::core::file_editor::{Edit, EditExecutor, ResolvedEdit};
use std::hint::black_box;

fn create_resolved_edit(line_idx: usize, edit_type: &str, text: &str) -> ResolvedEdit {
    ResolvedEdit {
        line_idx,
        end_idx: line_idx,
        edit: Edit {
            anchor: format!("hash_{}", line_idx),
            end_anchor: None,
            edit_type: edit_type.to_string(),
            text: text.to_string(),
        },
    }
}

fn bench_apply_single_edit(c: &mut Criterion) {
    let executor = EditExecutor::new();
    let lines: Vec<String> = (0..100).map(|i| format!("line {}", i)).collect();

    let edits = vec![create_resolved_edit(50, "replace", "new line 50")];

    c.bench_function("apply_edits_single", |b| {
        b.iter(|| {
            let result = executor.apply_edits(black_box(&lines), black_box(&edits));
            black_box(result);
        })
    });
}

fn bench_apply_multiple_edits(c: &mut Criterion) {
    let executor = EditExecutor::new();
    let lines: Vec<String> = (0..1_000).map(|i| format!("line {}", i)).collect();

    let edits: Vec<ResolvedEdit> = (0..10)
        .map(|i| create_resolved_edit(i * 100, "replace", &format!("new line {}", i * 100)))
        .collect();

    c.bench_function("apply_edits_10_non_overlapping", |b| {
        b.iter(|| {
            let result = executor.apply_edits(black_box(&lines), black_box(&edits));
            black_box(result);
        })
    });
}

fn bench_apply_many_edits(c: &mut Criterion) {
    let executor = EditExecutor::new();
    let lines: Vec<String> = (0..10_000).map(|i| format!("line {}", i)).collect();

    let edits: Vec<ResolvedEdit> = (0..100)
        .map(|i| create_resolved_edit(i * 100, "replace", &format!("new line {}", i * 100)))
        .collect();

    c.bench_function("apply_edits_100_non_overlapping", |b| {
        b.iter(|| {
            let result = executor.apply_edits(black_box(&lines), black_box(&edits));
            black_box(result);
        })
    });
}

fn bench_apply_insert_edit(c: &mut Criterion) {
    let executor = EditExecutor::new();
    let lines: Vec<String> = (0..1_000).map(|i| format!("line {}", i)).collect();

    let edits = vec![create_resolved_edit(
        500,
        "insert_after",
        "inserted line 1\ninserted line 2",
    )];

    c.bench_function("apply_edits_insert_2_lines", |b| {
        b.iter(|| {
            let result = executor.apply_edits(black_box(&lines), black_box(&edits));
            black_box(result);
        })
    });
}

fn bench_apply_delete_edit(c: &mut Criterion) {
    let executor = EditExecutor::new();
    let lines: Vec<String> = (0..1_000).map(|i| format!("line {}", i)).collect();

    let edits = vec![ResolvedEdit {
        line_idx: 500,
        end_idx: 500,
        edit: Edit {
            anchor: "hash_500".to_string(),
            end_anchor: None,
            edit_type: "replace".to_string(),
            text: String::new(), // Empty = delete
        },
    }];

    c.bench_function("apply_edits_delete_1_line", |b| {
        b.iter(|| {
            let result = executor.apply_edits(black_box(&lines), black_box(&edits));
            black_box(result);
        })
    });
}

criterion_group!(
    benches,
    bench_apply_single_edit,
    bench_apply_multiple_edits,
    bench_apply_many_edits,
    bench_apply_insert_edit,
    bench_apply_delete_edit,
);
criterion_main!(benches);
