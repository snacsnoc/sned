use criterion::{Criterion, black_box, criterion_group, criterion_main};
use sned::core::context::trackers::{FileContextTracker, FileRecordSource};
use std::io::Write;

fn bench_track_file(c: &mut Criterion) {
    let mut tracker = FileContextTracker::new();
    let temp_file = tempfile::NamedTempFile::new().unwrap();
    let path = temp_file.path();

    c.bench_function("track_file", |b| {
        b.iter(|| {
            tracker.track_file(black_box(path));
        })
    });
}

fn bench_check_stale(c: &mut Criterion) {
    let mut tracker = FileContextTracker::new();
    let mut temp_file = tempfile::NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    // Track the file first
    tracker.track_file(&path);

    c.bench_function("check_stale_unchanged", |b| {
        b.iter(|| {
            let result = tracker.check_stale(black_box(&path));
            black_box(result);
        })
    });

    // Modify the file
    temp_file.write_all(b"modified content").unwrap();
    temp_file.flush().unwrap();

    c.bench_function("check_stale_modified", |b| {
        b.iter(|| {
            let result = tracker.check_stale(black_box(&path));
            black_box(result);
        })
    });
}

fn bench_track_file_context(c: &mut Criterion) {
    let mut tracker = FileContextTracker::new();
    let temp_file = tempfile::NamedTempFile::new().unwrap();
    let path = temp_file.path().to_str().unwrap();

    c.bench_function("track_file_context", |b| {
        b.iter(|| {
            tracker.track_file_context(black_box(path), FileRecordSource::ReadTool);
        })
    });
}

criterion_group!(
    benches,
    bench_track_file,
    bench_check_stale,
    bench_track_file_context
);
criterion_main!(benches);
