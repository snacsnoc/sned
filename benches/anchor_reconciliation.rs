use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use sned::core::file_editor::AnchorStateManager;
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

/// Isolated anchor cache per bench case. The production constructor points
/// at the user's real cache dir, which would both pollute user state and
/// fold unrelated on-disk noise into the measurement. The returned guard
/// must be kept alive for the whole benchmark.
fn isolated_manager() -> (tempfile::TempDir, AnchorStateManager) {
    let dir = tempfile::tempdir().expect("bench anchor cache tempdir");
    let manager = AnchorStateManager::with_cache_file(dir.path().join("anchors.json"));
    (dir, manager)
}

/// Burn one-time cold costs (regex statics, file-lock creation, FS cache)
/// on a throwaway path so the measured iterations see steady state.
fn warm_up(manager: &AnchorStateManager, lines: &[String]) {
    let _ = manager.reconcile("/tmp/bench_anchor_warmup.txt", lines, Some("bench-task"));
}

fn bench_anchor_reconcile_first_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("anchor_reconcile_first_read");

    for &size in &[1_000, 2_000, 5_000, 10_000, 50_000, 100_000] {
        // NOTE: 50k/100k exceed MAX_TRACKED_LINES (5,000) and take the
        // snapshot-mode path, not the tracked path. Cross-size comparison
        // across that boundary is a path change, not a scaling trend.
        let (_guard, anchor_mgr) = isolated_manager();
        let lines = generate_content(size);
        warm_up(&anchor_mgr, &lines);
        // Fresh path per iteration is inherent to first-read semantics:
        // every sample below includes one full reconcile plus one shard
        // write for a previously unseen file. Unique task per iteration
        // keeps each published shard at a single document, so the cost
        // measured is one file — not a shard growing with sample count.
        let run = std::cell::Cell::new(0usize);

        group.bench_function(format!("first_read_{}_lines", size), |b| {
            b.iter(|| {
                let current_run = run.get();
                run.set(current_run + 1);
                let path = format!("/tmp/bench_anchor_first_{}_{}.txt", size, current_run);
                let task = format!("bench-task-first-{}-{}", size, current_run);
                let hashes = anchor_mgr.reconcile(black_box(&path), &lines, Some(&task));
                black_box(hashes);
            })
        });
    }

    group.finish();
}

fn bench_anchor_reconcile_unchanged(c: &mut Criterion) {
    let mut group = c.benchmark_group("anchor_reconcile_unchanged");

    for &size in &[1_000, 2_000, 5_000, 10_000, 50_000, 100_000] {
        let (_guard, anchor_mgr) = isolated_manager();
        let lines = generate_content(size);
        let path = format!("/tmp/bench_anchor_unchanged_{}.txt", size);

        // First reconcile to populate cache
        let _ = anchor_mgr.reconcile(&path, &lines, Some("bench-task"));
        warm_up(&anchor_mgr, &lines);

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

    for &size in &[5_000, 10_000, 50_000, 100_000] {
        for &percent in &[5, 50] {
            let (_guard, anchor_mgr) = isolated_manager();
            let lines = generate_content(size);

            // Generate modified content
            let modified_lines = generate_modified(&lines, percent as f64);
            warm_up(&anchor_mgr, &lines);
            let run = std::cell::Cell::new(0usize);

            // Setup (fresh path + baseline reconcile) is unmeasured;
            // only the modified reconcile is timed. Unique task per
            // iteration keeps the published shard at a constant size so
            // the measurement is one file, not task growth.
            group.bench_function(format!("modified_{}_lines_{}%", size, percent), |b| {
                b.iter_batched(
                    || {
                        let current_run = run.get();
                        run.set(current_run + 1);
                        let path = format!(
                            "/tmp/bench_anchor_mod_{}_p{}_{}.txt",
                            size, percent, current_run
                        );
                        let task = format!("bench-task-mod-{}-p{}-{}", size, percent, current_run);
                        let _ =
                            anchor_mgr.reconcile(&path, &lines, Some(&task));
                        (path, modified_lines.clone(), task)
                    },
                    |(path, modified, task)| {
                        let hashes = anchor_mgr.reconcile(
                            black_box(&path),
                            black_box(&modified),
                            Some(black_box(&task)),
                        );
                        black_box(hashes);
                    },
                    BatchSize::SmallInput,
                )
            });
        }
    }

    group.finish();
}

fn bench_anchor_reconcile_large_file_fallback(c: &mut Criterion) {
    let mut group = c.benchmark_group("anchor_reconcile_large_file_fallback");

    // Snapshot mode above MAX_TRACKED_LINES (5,000): content-addressed
    // digest only, no per-line anchor tracking.
    let (_guard, anchor_mgr) = isolated_manager();
    let lines = generate_content(100_000);
    let path = "/tmp/bench_anchor_fallback_100k.txt";
    let _ = anchor_mgr.reconcile(path, &lines, Some("bench-task"));
    warm_up(&anchor_mgr, &lines);

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
