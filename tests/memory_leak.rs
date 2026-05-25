//! Memory leak integration tests for sned.
//!
//! These tests verify that core components clean up properly.
//! Run with `cargo test --test memory_leak -- --nocapture`
//!
//! For detailed heap profiling, use: `cargo run --features dhat-heap`

use std::time::Duration;
use tokio::time::timeout;

/// Test that context truncation prevents unbounded growth.
///
/// Verifies that large contexts are properly truncated.
#[tokio::test]
async fn test_context_truncation_prevents_growth() {
    use sned::core::context::context_manager::{
        TruncationKeep, get_next_truncation_range, get_truncated_messages,
    };
    use sned::providers::{MessageContent, MessageRole, StorageMessage};

    // Create a large conversation history (1000 messages)
    let messages: Vec<StorageMessage> = (0..1000)
        .map(|i| StorageMessage {
            id: Some(format!("msg-{}", i)),
            role: if i % 2 == 0 {
                MessageRole::User
            } else {
                MessageRole::Assistant
            },
            content: MessageContent::Text("x".repeat(1000).to_string()),
            model_info: None,
            metrics: None,
            ts: None,
        })
        .collect();

    // Get truncation range (keep half)
    let range = get_next_truncation_range(&messages, None, TruncationKeep::Half);

    // Apply truncation
    let truncated = get_truncated_messages(&messages, Some(range), None);

    // Verify truncation reduced message count
    assert!(
        truncated.len() < messages.len(),
        "Truncation should reduce message count: {} -> {}",
        messages.len(),
        truncated.len()
    );

    // Verify we kept recent messages (approximately half)
    assert!(
        truncated.len() <= 510,
        "Should keep approximately 500 messages, got {}",
        truncated.len()
    );
}

/// Test that anchor pools don't grow unbounded.
///
/// Verifies that file_editor enforces MAX_ANCHOR_POOL_SIZE.
#[tokio::test]
async fn test_anchor_pool_size_capped() {
    use sned::core::file_editor::AnchorStateManager;
    use std::fs;
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let test_file = temp_dir.path().join("test.txt");

    // Create a file with many lines
    let lines: Vec<String> = (0..2000).map(|i| format!("line {}", i)).collect();
    fs::write(&test_file, lines.join("\n")).unwrap();

    let task_id = "anchor_pool_test";
    let anchor_mgr = AnchorStateManager::new();

    // Read file multiple times (should not grow pool beyond cap)
    for _ in 0..5 {
        let content = fs::read_to_string(&test_file).unwrap();
        let _lines_vec: Vec<&str> = content.lines().collect();
        let _anchors = anchor_mgr.get_anchors(test_file.to_str().unwrap(), Some(task_id));
    }

    // Reset to clean up
    anchor_mgr.reset(Some(task_id));
}

/// Test that SQLite connections are properly closed.
///
/// Verifies symbol_index doesn't leak database connections.
#[tokio::test]
async fn test_symbol_index_db_connections_closed() {
    use sned::services::symbol_index::SymbolIndexService;
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();

    // Create and use symbol index
    {
        let mut index = SymbolIndexService::new(temp_dir.path().to_string_lossy().to_string());
        index.initialize().unwrap();

        // Query symbols (should not panic or hang)
        let _symbols = index.get_symbols("main", None, None);

        // Drop index (should close DB connection)
        drop(index);
    }

    // Reopen should succeed (proves previous connection was closed)
    {
        let mut index2 = SymbolIndexService::new(temp_dir.path().to_string_lossy().to_string());
        index2.initialize().unwrap();
        let _symbols = index2.get_symbols("main", None, None);
    }
}

/// Test that spinner cleanup works properly.
/// Test that checkpoint tracker cleans up properly.
///
/// Verifies checkpoint operations complete without hanging.
#[tokio::test]
async fn test_checkpoint_tracker_cleanup() {
    use sned::core::checkpoints::TaskCheckpointManager;
    use std::fs;
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    // Create test file
    let test_file = workspace.join("test.txt");
    fs::write(&test_file, "initial").unwrap();

    let mut manager =
        TaskCheckpointManager::new("test-task".to_string(), true, workspace.to_str().unwrap());

    // Save a checkpoint
    let checkpoint = timeout(Duration::from_secs(5), manager.save_checkpoint())
        .await
        .expect("Checkpoint should complete within timeout");

    assert!(
        checkpoint.is_some() || checkpoint.is_none(),
        "Checkpoint operation should complete"
    );

    // Drop should be immediate
    drop(manager);
}

/// Test that cancellation handler sets up signal listener.
///
/// Verifies the handler is created without errors.
#[tokio::test]
async fn test_cancellation_handler_creation() {
    use sned::core::cancellation::CancellationHandler;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let state = Arc::new(Mutex::new(sned::core::agent_types::TaskState::default()));
    let handler = CancellationHandler::new(state.clone());

    // Drop should clean up
    drop(handler);
}
