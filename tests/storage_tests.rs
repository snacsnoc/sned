use sned::storage::*;
use sned::providers::env_auth::get_provider_from_env;
use std::fs;
use std::path::PathBuf;

/// Get a unique temp directory for testing (uses random suffix to avoid parallel conflicts)
fn get_test_dir() -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push("sned_test");
    let unique_id = format!("storage_{}_{}", std::process::id(), fastrand::u64(..));
    dir.push(unique_id);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn test_global_file_names() {
    assert_eq!(
        GlobalFileNames::API_CONVERSATION_HISTORY,
        "api_conversation_history.json"
    );
    assert_eq!(GlobalFileNames::UI_MESSAGES, "ui_messages.json");
    assert_eq!(GlobalFileNames::TASK_METADATA, "task_metadata.json");

    let remote = GlobalFileNames::remote_config("org123");
    assert_eq!(remote, "remote_config_org123.json");
}

#[test]
fn test_atomic_write_file() {
    let test_dir = get_test_dir();
    let file_path = test_dir.join("test_atomic.json");

    disk::atomic_write_file(&file_path, "test data").unwrap();

    let contents = fs::read_to_string(&file_path).unwrap();
    assert_eq!(contents, "test data");

    // Cleanup
    let _ = fs::remove_dir_all(&test_dir);
}

#[test]
fn test_global_state_store() {
    // Test default state - note: GlobalState::default() uses Rust defaults
    // The serde defaults only apply during deserialization
    let _state = GlobalState::default();
    // After deserializing from empty JSON, the defaults will apply
    let json = "{}";
    let deserialized: GlobalState = serde_json::from_str(json).unwrap();
    assert!(deserialized.is_new_user);
    assert!(deserialized.terminal_reuse_enabled);
    assert_eq!(deserialized.mode, "act");
    assert_eq!(deserialized.plan_mode_api_provider, "anthropic");
}

#[test]
fn test_task_storage() {
    let test_dir = get_test_dir();
    let tasks_dir = test_dir.join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();

    // Create a mock task storage
    let task_id = "test-task-123";
    let task_dir = tasks_dir.join(task_id);
    fs::create_dir_all(&task_dir).unwrap();

    // Test write and read API conversation history
    let history = serde_json::json!([
        {"role": "user", "content": "Hello"},
        {"role": "assistant", "content": "Hi there!"}
    ]);

    let history_str = serde_json::to_string(&history).unwrap();
    let file_path = task_dir.join(GlobalFileNames::API_CONVERSATION_HISTORY);
    disk::atomic_write_file(&file_path, &history_str).unwrap();

    let read_contents = fs::read_to_string(&file_path).unwrap();
    let read_history: serde_json::Value = serde_json::from_str(&read_contents).unwrap();
    assert_eq!(read_history[0]["role"], "user");
    assert_eq!(read_history[1]["role"], "assistant");

    // Cleanup
    let _ = fs::remove_dir_all(&test_dir);
}

#[test]
fn test_secrets_from_env() {
    // Cannot easily test env vars in unit tests without affecting environment
    // But we can test the mapping function
    let map = secrets::env_var_to_secret_key();
    assert_eq!(map.get("ANTHROPIC_API_KEY"), Some(&"apiKey"));
    assert_eq!(map.get("OPENAI_API_KEY"), Some(&"openAiApiKey"));
    assert_eq!(map.get("GEMINI_API_KEY"), Some(&"geminiApiKey"));
}

#[test]
fn test_provider_from_env() {
    // Test that the function doesn't panic when no env vars are set
    let provider = get_provider_from_env();
    // It might return None or Some depending on the test environment
    let _ = provider;
}

#[test]
fn test_secret_keys_list() {
    let keys = sned::storage::secrets::SECRET_KEYS;
    assert!(keys.contains(&"apiKey"));
    assert!(keys.contains(&"openAiApiKey"));
    assert!(keys.contains(&"geminiApiKey"));
    assert_eq!(keys.len(), 39); // As per state-keys.ts (39 items)
}
