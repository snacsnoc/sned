use serde::{Deserialize, Serialize};
use serde_json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// Global state and settings combined (mirrors TypeScript GlobalStateAndSettings)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GlobalState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sned_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sned_generated_machine_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_shown_announcement_id: Option<String>,
    #[serde(default)]
    pub task_history: Vec<HistoryItem>,
    #[serde(default)]
    pub favorited_model_ids: Vec<String>,
    #[serde(default = "default_true")]
    pub terminal_reuse_enabled: bool,
    #[serde(default = "default_vscode_terminal")]
    pub vscode_terminal_execution_mode: String,
    #[serde(default = "default_true")]
    pub is_new_user: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub welcome_view_completed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_roots: Option<Vec<WorkspaceRoot>>,
    #[serde(default)]
    pub primary_root_index: i32,
    #[serde(default = "default_true")]
    pub multi_root_enabled: bool,
    #[serde(default)]
    pub last_dismissed_info_banner_version: i32,
    #[serde(default)]
    pub last_dismissed_model_banner_version: i32,
    #[serde(default)]
    pub last_dismissed_cli_banner_version: i32,
    #[serde(default)]
    pub remote_rules_toggles: HashMap<String, bool>,
    #[serde(default)]
    pub remote_workflow_toggles: HashMap<String, bool>,
    #[serde(default)]
    pub dismissed_banners: Vec<DismissedBanner>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_auto_open_path: Option<String>,

    // User settings fields (from USER_SETTINGS_FIELDS)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_approval_settings: Option<AutoApprovalSettings>,
    #[serde(default)]
    pub auto_approve_patterns: Vec<String>,
    #[serde(default)]
    pub global_sned_rules_toggles: HashMap<String, bool>,
    #[serde(default)]
    pub global_workflow_toggles: HashMap<String, bool>,
    #[serde(default)]
    pub global_skills_toggles: HashMap<String, bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser_settings: Option<BrowserSettings>,
    #[serde(default = "default_unset")]
    pub telemetry_setting: String,
    #[serde(default)]
    pub plan_act_separate_models_setting: bool,
    #[serde(default = "default_true")]
    pub enable_checkpoints_setting: bool,
    #[serde(default = "default_shell_timeout")]
    pub shell_integration_timeout: i32,
    #[serde(default = "default_default_terminal")]
    pub default_terminal_profile: String,
    #[serde(default = "default_terminal_line_limit")]
    pub terminal_output_line_limit: i32,
    #[serde(default = "default_max_mistakes")]
    pub max_consecutive_mistakes: i32,
    #[serde(default)]
    pub strict_plan_mode_enabled: bool,
    #[serde(default = "default_true")]
    pub hooks_enabled: bool,
    #[serde(default)]
    pub yolo_mode_toggled: bool,
    #[serde(default)]
    pub auto_approve_all_toggled: bool,
    #[serde(default = "default_true")]
    pub use_auto_condense: bool,
    #[serde(default = "default_true")]
    pub show_token_usage: bool,
    #[serde(default)]
    pub subagents_enabled: bool,
    #[serde(default = "default_true")]
    pub sned_web_tools_enabled: bool,
    #[serde(default)]
    pub worktrees_enabled: bool,
    #[serde(default = "default_english")]
    pub preferred_language: String,
    #[serde(default = "default_act_mode")]
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_prompt: Option<String>,
    #[serde(default)]
    pub background_edit_enabled: bool,
    #[serde(default)]
    pub opt_out_of_remote_config: bool,
    #[serde(default)]
    pub double_check_completion_enabled: bool,

    // OpenTelemetry settings
    #[serde(default = "default_true")]
    pub open_telemetry_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_telemetry_metrics_exporter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_telemetry_logs_exporter: Option<String>,
    #[serde(default = "default_http_json")]
    pub open_telemetry_otlp_protocol: String,
    #[serde(default = "default_localhost_4318")]
    pub open_telemetry_otlp_endpoint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_telemetry_otlp_metrics_protocol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_telemetry_otlp_metrics_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_telemetry_otlp_logs_protocol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_telemetry_otlp_logs_endpoint: Option<String>,
    #[serde(default = "default_metric_interval")]
    pub open_telemetry_metric_export_interval: i32,
    #[serde(default)]
    pub open_telemetry_otlp_insecure: bool,
    #[serde(default = "default_log_batch_size")]
    pub open_telemetry_log_batch_size: i32,
    #[serde(default = "default_log_batch_timeout")]
    pub open_telemetry_log_batch_timeout: i32,
    #[serde(default = "default_log_max_queue")]
    pub open_telemetry_log_max_queue_size: i32,

    #[serde(default)]
    pub write_prompt_metadata_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_prompt_metadata_directory: Option<String>,

    // API handler settings (selected fields, full set deferred)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lite_llm_base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anthropic_base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_ai_base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_router_base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gemini_base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aws_region: Option<String>,
    #[serde(default = "default_anthropic")]
    pub plan_mode_api_provider: String,
    #[serde(default = "default_anthropic")]
    pub act_mode_api_provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub act_mode_api_model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_mode_api_model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub azure_api_version: Option<String>,
    #[serde(default)]
    pub enable_parallel_tool_calling: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_timeout_ms: Option<i32>,
}

// Helper functions for defaults
fn default_true() -> bool {
    true
}
fn default_vscode_terminal() -> String {
    "vscodeTerminal".to_string()
}
fn default_unset() -> String {
    "unset".to_string()
}
fn default_shell_timeout() -> i32 {
    4000
}
fn default_default_terminal() -> String {
    "default".to_string()
}
fn default_terminal_line_limit() -> i32 {
    500
}
fn default_max_mistakes() -> i32 {
    5
}
fn default_english() -> String {
    "English".to_string()
}
fn default_act_mode() -> String {
    "act".to_string()
}
fn default_anthropic() -> String {
    "anthropic".to_string()
}
fn default_http_json() -> String {
    "http/json".to_string()
}
fn default_localhost_4318() -> String {
    "http://localhost:4318".to_string()
}
fn default_metric_interval() -> i32 {
    60000
}
fn default_log_batch_size() -> i32 {
    512
}
fn default_log_batch_timeout() -> i32 {
    5000
}
fn default_log_max_queue() -> i32 {
    2048
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HistoryItem {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ulid: Option<String>,
    pub number: i32,
    pub ts: i64,
    pub task: String,
    pub tokens_in: i32,
    pub tokens_out: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_writes: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_reads: Option<i32>,
    pub total_cost: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadow_git_config_work_tree: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd_on_task_initialization: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_history_deleted_range: Option<Vec<i32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_favorited: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_root_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint_manager_error_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceRoot {
    pub path: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DismissedBanner {
    pub banner_id: String,
    pub dismissed_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoApprovalSettings {
    pub enabled: bool,
    pub actions: Vec<String>,
    pub max_requests: i32,
    pub enable_notifications: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserSettings {
    pub enabled: bool,
    pub viewport_size: String,
    pub headless: bool,
}

/// Load global state from the default path (~/.sned/data/settings/global_settings.json)
/// Uses integrity validation (SHA256 checksum) when available.
pub fn load_global_state() -> GlobalState {
    load_global_state_with_integrity()
}

/// Load global state from legacy format (no integrity validation)
/// Only used for backward compatibility or testing.
#[allow(dead_code)]
fn load_global_state_legacy() -> GlobalState {
    let path = get_sned_home_path()
        .join("data")
        .join("settings")
        .join("global_settings.json");
    match fs::read_to_string(&path) {
        Ok(contents) => match serde_json::from_str(&contents) {
            Ok(state) => state,
            Err(error) => {
                // Create backup of corrupted file before discarding
                if let Ok(backup_path) = crate::storage::disk::create_backup(&path) {
                    tracing::warn!(
                        file_path = %path.display(),
                        backup_path = %backup_path.display(),
                        error = %error,
                        "Created backup of corrupted global state JSON"
                    );
                } else {
                    tracing::warn!(
                        file_path = %path.display(),
                        error = %error,
                        "Failed to parse global state JSON and backup failed"
                    );
                }
                GlobalState::default()
            }
        },
        Err(_) => GlobalState::default(),
    }
}

fn get_sned_home_path() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".sned"))
        .unwrap_or_else(|| PathBuf::from(".sned"))
}

/// Compute SHA256 checksum of data for integrity validation
fn compute_checksum(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    hex::encode(hasher.finalize())
}

/// Validate checksum of loaded data
fn validate_checksum(data: &str, expected_checksum: &str) -> bool {
    compute_checksum(data) == expected_checksum
}

/// Load global state with integrity validation
/// Format: first line is checksum, rest is JSON
pub fn load_global_state_with_integrity() -> GlobalState {
    let path = get_sned_home_path()
        .join("data")
        .join("settings")
        .join("global_settings.json");

    match fs::read_to_string(&path) {
        Ok(contents) => {
            // Parse checksum and data
            let mut lines = contents.lines();
            let checksum_line = lines.next().unwrap_or("");

            // Check if file has checksum prefix (format: "sha256:<hash>")
            let (expected_checksum, json_data) =
                if let Some(checksum) = checksum_line.strip_prefix("sha256:") {
                    let json_data = lines.collect::<Vec<_>>().join("\n");
                    (Some(checksum), json_data)
                } else {
                    // Legacy format without checksum
                    (None, contents)
                };

            // Validate checksum if present
            if let Some(expected) = expected_checksum
                && !validate_checksum(&json_data, expected)
            {
                tracing::warn!(
                    file_path = %path.display(),
                    "Global state checksum mismatch - file may be corrupted or tampered"
                );
                if let Ok(backup_path) = crate::storage::disk::create_backup(&path) {
                    tracing::warn!(
                        file_path = %path.display(),
                        backup_path = %backup_path.display(),
                        "Global state integrity check failed; backed up corrupted file"
                    );
                }
                return GlobalState::default();
            }

            // Parse JSON
            match serde_json::from_str(&json_data) {
                Ok(state) => state,
                Err(error) => {
                    // Create backup of corrupted file
                    if let Ok(backup_path) = crate::storage::disk::create_backup(&path) {
                        tracing::warn!(
                            file_path = %path.display(),
                            backup_path = %backup_path.display(),
                            error = %error,
                            "Created backup of corrupted global state JSON"
                        );
                    }
                    GlobalState::default()
                }
            }
        }
        Err(_) => GlobalState::default(),
    }
}

/// Save global state with integrity checksum
pub fn save_global_state_with_integrity(state: &GlobalState) -> std::io::Result<()> {
    let path = get_sned_home_path()
        .join("data")
        .join("settings")
        .join("global_settings.json");

    let json_data = serde_json::to_string_pretty(state).map_err(std::io::Error::other)?;

    let checksum = compute_checksum(&json_data);
    let contents = format!("sha256:{}\n{}", checksum, json_data);

    // Use atomic write for safety
    crate::storage::disk::atomic_write_file(&path, &contents)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex, OnceLock};
    use tempfile::TempDir;
    use tracing::subscriber::with_default;
    use tracing_subscriber::filter::LevelFilter;

    static TEST_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    #[derive(Clone, Default)]
    struct TestWriter {
        buf: Arc<Mutex<Vec<u8>>>,
    }

    impl TestWriter {
        fn output(&self) -> String {
            let buf = self.buf.lock().unwrap();
            String::from_utf8_lossy(&buf).into_owned()
        }
    }

    impl std::io::Write for TestWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            std::io::Write::write(&mut *self.buf.lock().unwrap(), buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            std::io::Write::flush(&mut *self.buf.lock().unwrap())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for TestWriter {
        type Writer = TestWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn with_temp_home<R>(f: impl FnOnce(&TempDir) -> R) -> R {
        let _guard = TEST_ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let temp_home = TempDir::new().unwrap();
        let old_home = std::env::var_os("HOME");

        // SAFETY: env mutation guarded by mutex; no concurrent access to HOME
        unsafe {
            std::env::set_var("HOME", temp_home.path());
        }

        let result = f(&temp_home);

        // SAFETY: env mutation guarded by mutex; restoring previous value
        unsafe {
            if let Some(old_home) = old_home {
                std::env::set_var("HOME", old_home);
            } else {
                std::env::remove_var("HOME");
            }
        }

        result
    }

    #[test]
    fn test_load_global_state_warns_on_corrupt_json() {
        with_temp_home(|temp_home| {
            let settings_dir = temp_home.path().join(".sned").join("data").join("settings");
            fs::create_dir_all(&settings_dir).unwrap();
            fs::write(
                settings_dir.join("global_settings.json"),
                b"{ this is not valid json",
            )
            .unwrap();

            let writer = TestWriter::default();
            let subscriber = tracing_subscriber::fmt()
                .with_writer(writer.clone())
                .without_time()
                .with_ansi(false)
                .with_level(false)
                .with_target(false)
                .with_max_level(LevelFilter::TRACE)
                .finish();

            let state = with_default(subscriber, load_global_state);

            assert_eq!(
                serde_json::to_value(&state).unwrap(),
                serde_json::to_value(GlobalState::default()).unwrap()
            );

            let output = writer.output();
            assert!(output.contains("corrupted global state JSON"), "{output}");
            assert!(output.contains("global_settings.json"), "{output}");
        });
    }
}
