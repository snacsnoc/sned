//! End-to-end integration tests for the full CLI → agent loop → tool dispatch → result → exit path.
//!
//! These tests exercise the complete pipeline with a mock provider to ensure
//! all components work together correctly.

use assert_cmd::Command;
use predicates::prelude::*;
use std::time::Duration;

/// Helper to create a command with common test settings
fn sned_cmd() -> Command {
    let mut cmd = Command::cargo_bin("sned").unwrap();
    cmd.timeout(Duration::from_secs(30))
        .arg("--yolo")
        .arg("--provider")
        .arg("mock");
    cmd
}

#[test]
fn test_e2e_mock_provider_starts_and_exits() {
    let mut cmd = sned_cmd();
    cmd.arg("test");

    cmd.assert().stderr(
        predicate::str::contains("Mock provider response")
            .or(predicate::str::contains("context limit")),
    );
}

#[test]
fn test_e2e_mock_with_yolo_flag() {
    let mut cmd = sned_cmd();
    cmd.arg("hello");

    cmd.assert()
        .stderr(predicate::str::contains("sned").or(predicate::str::contains("Mock provider")));
}

#[test]
fn test_e2e_mock_provider_selection() {
    let mut cmd = Command::cargo_bin("sned").unwrap();
    cmd.timeout(Duration::from_secs(10))
        .arg("--provider")
        .arg("mock")
        .arg("--yolo")
        .arg("x");

    cmd.assert()
        .stderr(predicate::str::contains("Mock").or(predicate::str::contains("context limit")));
}

#[test]
fn test_json_text_only_completion_emits_completion_event_once() {
    let temp_dir = tempfile::tempdir().unwrap();
    let sned_dir = temp_dir.path().join("sned-home");

    let mut cmd = Command::cargo_bin("sned").unwrap();
    cmd.timeout(Duration::from_secs(30))
        .current_dir(temp_dir.path())
        .env("SNED_DIR", &sned_dir)
        .env("SNED_SYMBOL_INDEX", "off")
        .arg("--json")
        .arg("--provider")
        .arg("mock")
        .arg("test");

    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "sned failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let events: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|err| panic!("{err}: {line}")))
        .collect();

    let completions: Vec<&serde_json::Value> = events
        .iter()
        .filter(|event| event["type"] == "completion")
        .collect();
    assert_eq!(completions.len(), 1, "stdout:\n{stdout}");
    assert_eq!(
        completions[0]["result"],
        "Mock provider response - task completed successfully"
    );
    assert!(events.iter().any(|event| event["type"] == "text"));
}
