use criterion::{Criterion, criterion_group, criterion_main};
use sned::core::file_editor::AnchorStateManager;
use std::cell::Cell;
use std::hint::black_box;

/// Generate test file content with specified line count
fn generate_content(line_count: usize) -> Vec<String> {
    (0..line_count)
        .map(|i| format!("line {} with some content to make it realistic", i))
        .collect()
}

/// Generate modified content with specified percentage of lines changed
fn generate_modified(original: &[String], modification_percent: f64) -> Vec<String> {
    let mut modified = original.to_vec();
    let modify_count = (original.len() as f64 * modification_percent / 100.0) as usize;
    let step = original.len().saturating_div(modify_count.max(1));

    for i in (0..modified.len()).step_by(step.max(1)).take(modify_count) {
        modified[i] = format!("MODIFIED line {} with new content", i);
    }

    modified
}

fn bench_anchor_reconcile_first_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("anchor_reconcile_first_read");

    for &size in &[1_000, 10_000, 50_000, 100_000] {
        let anchor_mgr = AnchorStateManager::new();
        let lines = generate_content(size);
        let run = Cell::new(0usize);

        group.bench_function(format!("first_read_{}_lines", size), |b| {
            b.iter(|| {
                let current_run = run.get();
                run.set(current_run + 1);
                let path = format!("/tmp/bench_anchor_first_{}_{}.txt", size, current_run);
                let hashes = anchor_mgr.reconcile(black_box(&path), &lines, Some("bench-task"));
                black_box(hashes);
            })
        });
    }

    group.finish();
}

fn bench_anchor_reconcile_unchanged(c: &mut Criterion) {
    let mut group = c.benchmark_group("anchor_reconcile_unchanged");

    for &size in &[1_000, 10_000, 50_000, 100_000] {
        let anchor_mgr = AnchorStateManager::new();
        let lines = generate_content(size);
        let path = format!("/tmp/bench_anchor_unchanged_{}.txt", size);

        // First reconcile to populate cache
        let _ = anchor_mgr.reconcile(&path, &lines, Some("bench-task"));

        group.bench_function(format!("unchanged_reread_{}_lines", size), |b| {
            b.iter(|| {
                let hashes = anchor_mgr.reconcile(black_box(&path), &lines, Some("bench-task"));
                black_box(hashes);
            })
        });
    }

    group.finish();
}

fn bench_anchor_reconcile_modified(c: &mut Criterion) {
    let mut group = c.benchmark_group("anchor_reconcile_modified");

    for &size in &[10_000, 50_000, 100_000] {
        for &percent in &[5, 50] {
            let anchor_mgr = AnchorStateManager::new();
            let lines = generate_content(size);
            let run = Cell::new(0usize);

            // Generate modified content
            let modified_lines = generate_modified(&lines, percent as f64);

            group.bench_function(format!("modified_{}_lines_{}%", size, percent), |b| {
                b.iter(|| {
                    let current_run = run.get();
                    run.set(current_run + 1);
                    let path = format!(
                        "/tmp/bench_anchor_mod_{}_p{}_{}.txt",
                        size, percent, current_run
                    );
                    let _ = anchor_mgr.reconcile(&path, &lines, Some("bench-task"));
                    let hashes =
                        anchor_mgr.reconcile(black_box(&path), &modified_lines, Some("bench-task"));
                    black_box(hashes);
                })
            });
        }
    }

    group.finish();
}

fn bench_anchor_reconcile_large_file_fallback(c: &mut Criterion) {
    let mut group = c.benchmark_group("anchor_reconcile_large_file_fallback");

    // Test large-file fallback (>50K lines uses L1, L2, ... anchors instead of words)
    let anchor_mgr = AnchorStateManager::new();
    let lines = generate_content(100_000);
    let path = "/tmp/bench_anchor_fallback_100k.txt";

    group.bench_function("large_file_fallback_100k_lines", |b| {
        b.iter(|| {
            let hashes = anchor_mgr.reconcile(black_box(path), &lines, Some("bench-task"));
            black_box(hashes);
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_anchor_reconcile_first_read,
    bench_anchor_reconcile_unchanged,
    bench_anchor_reconcile_modified,
    bench_anchor_reconcile_large_file_fallback,
);
criterion_main!(benches);
