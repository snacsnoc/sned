use criterion::{Criterion, criterion_group, criterion_main};
use sned::core::hooks::{HookData, HookInput, HookManager, HookName, PreToolUseData};
use std::hint::black_box;

fn bench_hook_serialization(c: &mut Criterion) {
    let input = HookInput {
        task_id: "task_123".to_string(),
        model: None,
        data: HookData::PreToolUse {
            pre_tool_use: PreToolUseData {
                tool: "read_file".to_string(),
                input: serde_json::json!({"path": "/tmp/test.txt", "offset": 0, "limit": 100}),
            },
        },
    };

    c.bench_function("hook_serialize", |b| {
        b.iter(|| {
            let _ = serde_json::to_string(black_box(&input)).unwrap();
        })
    });

    let json = serde_json::to_string(&input).unwrap();
    c.bench_function("hook_deserialize", |b| {
        b.iter(|| {
            let _: HookInput = serde_json::from_str(black_box(&json)).unwrap();
        })
    });
}

fn bench_hook_discovery(c: &mut Criterion) {
    let manager = HookManager::new("benchmark-user");
    let input = HookInput {
        task_id: "task_123".to_string(),
        model: None,
        data: HookData::PreToolUse {
            pre_tool_use: PreToolUseData {
                tool: "read_file".to_string(),
                input: serde_json::json!({"path": "/tmp/test.txt"}),
            },
        },
    };

    c.bench_function("hook_discover_pre_tool_use", |b| {
        b.iter(|| {
            let hooks = manager.discover_hooks(HookName::PreToolUse);
            black_box(hooks);
        })
    });

    c.bench_function("hook_execute_empty", |b| {
        b.iter(|| {
            let result = manager.execute_hook(HookName::PreToolUse, black_box(&input), None);
            black_box(result);
        })
    });
}

criterion_group!(benches, bench_hook_serialization, bench_hook_discovery);
criterion_main!(benches);
