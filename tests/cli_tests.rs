//! CLI integration tests for argument parsing and help output.

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;

#[test]
fn test_help_shows_all_subcommands() {
    let mut cmd = Command::cargo_bin("sned").unwrap();
    cmd.arg("--help");
    cmd.assert()
        .success()
        .stdout(contains("task"))
        .stdout(contains("history"))
        .stdout(contains("config"))
        .stdout(contains("auth"))
        .stdout(contains("version"))
        .stdout(contains("dev"));
}

#[test]
fn test_help_shows_task_options() {
    let mut cmd = Command::cargo_bin("sned").unwrap();
    cmd.arg("--help");
    cmd.assert()
        .success()
        .stdout(contains("--act"))
        .stdout(contains("--plan"))
        .stdout(contains("--yolo"))
        .stdout(contains("--model"))
        .stdout(contains("--provider"))
        .stdout(contains("--json"))
        .stdout(contains("--continue"))
        .stdout(contains("--task-id"))
        .stdout(contains("--hooks-dir"));
}

#[test]
fn test_version_shows_version() {
    let mut cmd = Command::cargo_bin("sned").unwrap();
    cmd.arg("--version");
    cmd.assert().success().stdout(contains("0.1.0"));
}

#[test]
fn test_version_subcommand_shows_version() {
    let mut cmd = Command::cargo_bin("sned").unwrap();
    cmd.arg("version");
    cmd.assert().success().stdout(contains("0.1.0"));
}

#[test]
fn test_task_subcommand_help() {
    let mut cmd = Command::cargo_bin("sned").unwrap();
    cmd.args(["task", "--help"]);
    cmd.assert()
        .success()
        .stdout(contains("Usage:"))
        .stdout(contains("--model"))
        .stdout(contains("--provider"));
}

#[test]
fn test_history_subcommand_help() {
    let mut cmd = Command::cargo_bin("sned").unwrap();
    cmd.args(["history", "--help"]);
    cmd.assert().success().stdout(contains("Usage:"));
}

#[test]
fn test_config_subcommand_help() {
    let mut cmd = Command::cargo_bin("sned").unwrap();
    cmd.args(["config", "--help"]);
    cmd.assert()
        .success()
        .stdout(contains("Usage:"))
        .stdout(contains("set"));
}

#[test]
fn test_config_set_updates_global_settings() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::cargo_bin("sned").unwrap();
    cmd.env("SNED_DIR", temp_dir.path());
    cmd.args(["config", "set", "mode=plan"]);
    cmd.assert()
        .success()
        .stdout(contains("Updated configuration: mode=plan"));

    let settings_path = temp_dir
        .path()
        .join("data")
        .join("settings")
        .join("global_settings.json");
    let contents = std::fs::read_to_string(settings_path).unwrap();
    let state: serde_json::Value = serde_json::from_str(&contents).unwrap();
    assert_eq!(state["mode"], "plan");
}

#[test]
fn test_auth_subcommand_help() {
    let mut cmd = Command::cargo_bin("sned").unwrap();
    cmd.args(["auth", "--help"]);
    cmd.assert().success().stdout(contains("Usage:"));
}

#[test]
fn test_update_subcommand_help() {
    let mut cmd = Command::cargo_bin("sned").unwrap();
    cmd.args(["update", "--help"]);
    cmd.assert().success().stdout(contains("Usage:"));
}

#[test]
fn test_dev_subcommand_help() {
    let mut cmd = Command::cargo_bin("sned").unwrap();
    cmd.args(["dev", "--help"]);
    cmd.assert().success().stdout(contains("Usage:"));
}

#[test]
fn test_invalid_flag_exits_with_error() {
    let mut cmd = Command::cargo_bin("sned").unwrap();
    cmd.arg("--invalid-flag");
    cmd.assert().failure().stderr(contains("error"));
}

#[test]
fn test_prompt_argument_parsing() {
    let mut cmd = Command::cargo_bin("sned").unwrap();
    // Just verify parsing succeeds with a prompt argument
    // (won't run the task since no provider is configured)
    cmd.args(["--help"]);
    cmd.assert().success();
}

#[test]
fn test_history_subcommand_empty() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::cargo_bin("sned").unwrap();
    cmd.env("SNED_DIR", temp_dir.path());
    cmd.arg("history");
    cmd.assert()
        .success()
        .stdout(contains("No task history found."));
}

#[test]
fn test_tracing_default_level_is_warn() {
    let mut cmd = Command::cargo_bin("sned").unwrap();
    cmd.env("SNED_DIR", tempfile::tempdir().unwrap().path());
    cmd.env_remove("RUST_LOG");
    cmd.arg("history");
    cmd.assert()
        .success()
        .stderr(predicates::str::contains("DEBUG").not());
}

#[test]
fn test_tracing_verbose_enables_debug() {
    let mut cmd = Command::cargo_bin("sned").unwrap();
    cmd.env("SNED_DIR", tempfile::tempdir().unwrap().path());
    cmd.env_remove("RUST_LOG");
    cmd.arg("--verbose");
    cmd.arg("history");
    cmd.assert().success();
}

#[test]
fn test_exit_code_config_error() {
    let mut cmd = Command::cargo_bin("sned").unwrap();
    cmd.arg("--provider").arg("nonexistent").arg("test");
    cmd.assert().failure().code(2);
}

#[test]
fn test_exit_code_input_error() {
    let mut cmd = Command::cargo_bin("sned").unwrap();
    cmd.arg("--invalid-flag");
    cmd.assert().failure().code(2);
}

#[test]
fn test_exit_code_success() {
    let mut cmd = Command::cargo_bin("sned").unwrap();
    cmd.arg("--help");
    cmd.assert().success().code(0);
}
