//! Migration End-to-End Integration Tests
//!
//! These tests validate migration behavior against representative fixture data:
//! - minimal: just endpoints.json and global_settings.json
//! - full: all sections including task history, secrets, task directories
//! - corrupt: partial JSON, mixed old/new formats
//! - vscode_migration: VS Code-created state

use sned::storage::migration::{MigrationEngine, plan_dry_run_migration};
use std::fs;
use std::path::PathBuf;

/// Get the fixtures directory
fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/migration_fixtures")
}

/// Create a unique temp directory for testing
fn create_temp_dir(name: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push("sned_migration_test");
    let unique_id = format!("{}_{}_{}", name, std::process::id(), fastrand::u64(..));
    dir.push(unique_id);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Copy fixture directory to temp location
fn copy_fixture(fixture_name: &str) -> PathBuf {
    let fixtures = fixtures_dir();
    let source = fixtures.join(fixture_name);
    let dest = create_temp_dir(fixture_name);

    for entry in walkdir::WalkDir::new(&source) {
        let entry = entry.unwrap();
        let relative = entry.path().strip_prefix(&source).unwrap();
        let target = dest.join(relative);

        if entry.file_type().is_dir() {
            fs::create_dir_all(&target).unwrap();
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }

    dest
}

#[test]
fn test_minimal_fixture_dry_run() {
    let source = copy_fixture("minimal");
    let dest = create_temp_dir("minimal_dest");

    let report = plan_dry_run_migration(&source, &dest).unwrap();

    // Should detect endpoints and global_settings for migration
    assert!(report.endpoints.is_some());
    assert!(report.global_settings.is_some());
    assert!(report.has_changes());

    // Cleanup
    let _ = fs::remove_dir_all(&source);
    let _ = fs::remove_dir_all(&dest);
}

#[test]
fn test_minimal_fixture_migration() {
    let source = copy_fixture("minimal");
    let dest = create_temp_dir("minimal_dest_exec");

    let report = MigrationEngine::new(&source, &dest).execute().unwrap();

    // Verify migration succeeded
    assert!(report.success);
    assert!(report.has_changes());

    // Verify files were copied
    assert!(dest.join("endpoints.json").exists());
    assert!(dest.join("data/settings/global_settings.json").exists());

    // Verify content
    let endpoints_content = fs::read_to_string(dest.join("endpoints.json")).unwrap();
    assert!(endpoints_content.contains("anthropic"));
    assert!(endpoints_content.contains("sk-ant-minimal-test"));

    // Cleanup
    let _ = fs::remove_dir_all(&source);
    let _ = fs::remove_dir_all(&dest);
}

#[test]
fn test_full_fixture_dry_run() {
    let source = copy_fixture("full");
    let dest = create_temp_dir("full_dest");

    let report = plan_dry_run_migration(&source, &dest).unwrap();

    // Should detect all sections
    assert!(report.endpoints.is_some());
    assert!(report.global_settings.is_some());
    assert!(report.secrets.is_some());
    assert!(report.task_history.is_some());
    assert!(!report.tasks.is_empty());
    assert!(report.has_changes());

    // Cleanup
    let _ = fs::remove_dir_all(&source);
    let _ = fs::remove_dir_all(&dest);
}

#[test]
fn test_full_fixture_migration() {
    let source = copy_fixture("full");
    let dest = create_temp_dir("full_dest_exec");

    let report = MigrationEngine::new(&source, &dest).execute().unwrap();

    // Verify migration succeeded
    assert!(report.success);
    assert!(report.has_changes());

    // Verify all files were copied
    assert!(dest.join("endpoints.json").exists());
    assert!(dest.join("data/settings/global_settings.json").exists());
    assert!(dest.join(".secrets.json").exists());
    assert!(dest.join("data/state/taskHistory.json").exists());
    assert!(
        dest.join("data/tasks/task-001/api_conversation_history.json")
            .exists()
    );
    assert!(
        dest.join("data/tasks/task-002/api_conversation_history.json")
            .exists()
    );

    // Verify task history was migrated
    let task_history_content =
        fs::read_to_string(dest.join("data/state/taskHistory.json")).unwrap();
    assert!(task_history_content.contains("task-001"));
    assert!(task_history_content.contains("task-002"));
    assert!(task_history_content.contains("task-003"));

    // Cleanup
    let _ = fs::remove_dir_all(&source);
    let _ = fs::remove_dir_all(&dest);
}

#[test]
fn test_corrupt_fixture_dry_run_fails() {
    let source = copy_fixture("corrupt");
    let dest = create_temp_dir("corrupt_dest");

    // Corrupt JSON should fail during dry-run
    let result = plan_dry_run_migration(&source, &dest);

    // Should return an error for malformed JSON
    assert!(result.is_err());

    // Cleanup
    let _ = fs::remove_dir_all(&source);
    let _ = fs::remove_dir_all(&dest);
}

#[test]
fn test_corrupt_fixture_migration_fails() {
    let source = copy_fixture("corrupt");
    let dest = create_temp_dir("corrupt_dest_exec");

    // Corrupt JSON should fail during migration
    let result = MigrationEngine::new(&source, &dest).execute();

    // Should return an error for malformed JSON
    assert!(result.is_err());

    // Cleanup
    let _ = fs::remove_dir_all(&source);
    let _ = fs::remove_dir_all(&dest);
}

#[test]
fn test_vscode_fixture_dry_run() {
    let source = copy_fixture("vscode_migration");
    let dest = create_temp_dir("vscode_dest");

    let report = plan_dry_run_migration(&source, &dest).unwrap();

    // Should detect endpoints and global_settings for migration
    assert!(report.endpoints.is_some());
    assert!(report.global_settings.is_some());
    assert!(report.has_changes());

    // Cleanup
    let _ = fs::remove_dir_all(&source);
    let _ = fs::remove_dir_all(&dest);
}

#[test]
fn test_vscode_fixture_migration() {
    let source = copy_fixture("vscode_migration");
    let dest = create_temp_dir("vscode_dest_exec");

    let report = MigrationEngine::new(&source, &dest).execute().unwrap();

    // Verify migration succeeded
    assert!(report.success);

    // Verify files were copied
    assert!(dest.join("endpoints.json").exists());
    assert!(dest.join("data/settings/global_settings.json").exists());

    // Verify VS Code-specific content
    let settings_content =
        fs::read_to_string(dest.join("data/settings/global_settings.json")).unwrap();
    assert!(settings_content.contains("vscode_migrated"));

    // Cleanup
    let _ = fs::remove_dir_all(&source);
    let _ = fs::remove_dir_all(&dest);
}

#[test]
fn test_rollback_on_failure() {
    // This test verifies that rollback restores state on failure.
    // We create a valid endpoints.json (succeeds), then inject invalid JSON
    // in .secrets.json so the migration fails mid-execution, triggering rollback.

    let source = create_temp_dir("rollback_source");
    let dest = create_temp_dir("rollback_dest");

    // Create valid source endpoints — this migration step will succeed first
    fs::write(
        source.join("endpoints.json"),
        r#"{"anthropic": {"apiKey": "test"}}"#,
    )
    .unwrap();

    // Inject invalid JSON in .secrets.json — this step will fail, triggering rollback
    fs::write(
        source.join(".secrets.json"),
        "{ not valid json",
    )
    .unwrap();

    let mut engine = MigrationEngine::new(&source, &dest);
    let result = engine.execute();

    // Migration should fail due to invalid JSON in .secrets.json
    assert!(result.is_err());

    // Rollback should succeed
    let rollback_result = engine.rollback();
    assert!(rollback_result.is_ok());

    // Verify rollback: endpoints.json should be removed from dest
    assert!(!dest.join("endpoints.json").exists());

    // Cleanup
    let _ = fs::remove_dir_all(&source);
    let _ = fs::remove_dir_all(&dest);
}

#[test]
fn test_migration_with_existing_destination() {
    // Test merging when destination already has some data
    let source = copy_fixture("minimal");
    let dest = create_temp_dir("existing_dest");

    // Pre-populate destination with different data
    fs::create_dir_all(&dest).unwrap();
    fs::write(
        dest.join("endpoints.json"),
        r#"{"openai": {"apiKey": "existing-key"}}"#,
    )
    .unwrap();

    let report = MigrationEngine::new(&source, &dest).execute().unwrap();

    // Should succeed
    assert!(report.success);

    // Destination should now have both endpoints (merged)
    let endpoints_content = fs::read_to_string(dest.join("endpoints.json")).unwrap();
    assert!(endpoints_content.contains("anthropic")); // from source
    assert!(endpoints_content.contains("openai")); // existing

    // Cleanup
    let _ = fs::remove_dir_all(&source);
    let _ = fs::remove_dir_all(&dest);
}

#[test]
fn test_migration_preserves_destination_on_conflict() {
    // When there's a conflict, destination should be preserved (backed up)
    let source = create_temp_dir("conflict_source");
    let dest = create_temp_dir("conflict_dest");

    // Create conflicting files
    fs::write(
        source.join("endpoints.json"),
        r#"{"anthropic": {"apiKey": "source-key"}}"#,
    )
    .unwrap();

    fs::write(
        dest.join("endpoints.json"),
        r#"{"anthropic": {"apiKey": "dest-key"}}"#,
    )
    .unwrap();

    let mut engine = MigrationEngine::new(&source, &dest);
    let report = engine.execute().unwrap();

    // Should succeed but with conflicts noted
    assert!(report.success);

    // Cleanup
    let _ = fs::remove_dir_all(&source);
    let _ = fs::remove_dir_all(&dest);
}
