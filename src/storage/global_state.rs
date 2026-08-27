use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Global state and settings combined (mirrors TypeScript GlobalStateAndSettings)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sned_version: Option<String>,
    #[serde(default)]
    pub task_history: Vec<HistoryItem>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_approval_settings: Option<AutoApprovalSettings>,
    #[serde(default)]
    pub auto_approve_patterns: Vec<String>,
    #[serde(default)]
    pub global_sned_rules_toggles: HashMap<String, bool>,
    #[serde(default = "default_true")]
    pub enable_checkpoints_setting: bool,
    #[serde(
        default = "default_max_mistakes",
        deserialize_with = "deserialize_max_consecutive_mistakes"
    )]
    pub max_consecutive_mistakes: i32,
    #[serde(default)]
    pub strict_plan_mode_enabled: bool,
    #[serde(default)]
    pub subagents_enabled: bool,
    #[serde(default = "default_true")]
    pub sned_web_tools_enabled: bool,
    #[serde(default = "default_act_mode")]
    pub mode: String,

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
}

impl Default for GlobalState {
    fn default() -> Self {
        Self {
            sned_version: None,
            task_history: Vec::new(),
            auto_approval_settings: None,
            auto_approve_patterns: Vec::new(),
            global_sned_rules_toggles: HashMap::new(),
            enable_checkpoints_setting: default_true(),
            max_consecutive_mistakes: default_max_mistakes(),
            strict_plan_mode_enabled: false,
            subagents_enabled: false,
            sned_web_tools_enabled: default_true(),
            mode: default_act_mode(),
            lite_llm_base_url: None,
            anthropic_base_url: None,
            open_ai_base_url: None,
            open_router_base_url: None,
            gemini_base_url: None,
            plan_mode_api_provider: default_anthropic(),
            act_mode_api_provider: default_anthropic(),
            act_mode_api_model_id: None,
            plan_mode_api_model_id: None,
            azure_api_version: None,
            enable_parallel_tool_calling: false,
        }
    }
}

// Helper functions for defaults
fn default_true() -> bool {
    true
}
fn default_max_mistakes() -> i32 {
    3
}

fn deserialize_max_consecutive_mistakes<'de, D>(deserializer: D) -> Result<i32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let Some(value) = value.as_i64() else {
        return Err(serde::de::Error::custom(
            "max_consecutive_mistakes must be a non-negative integer; 0 disables the limit",
        ));
    };
    i32::try_from(value).map_err(|_| {
        serde::de::Error::custom(
            "max_consecutive_mistakes must be an integer; 0 disables the limit",
        )
    })
}
fn default_anthropic() -> String {
    "anthropic".to_string()
}

fn default_act_mode() -> String {
    "act".to_string()
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
pub struct AutoApprovalSettings {
    pub enabled: bool,
    pub actions: Vec<String>,
    pub max_requests: i32,
    pub enable_notifications: bool,
}

/// Load global state from the active Sned storage root.
#[must_use]
pub fn load_global_state() -> GlobalState {
    load_global_state_from_path(&global_settings_path()).unwrap_or_default()
}

#[must_use]
pub fn global_settings_path() -> PathBuf {
    crate::storage::disk::get_settings_dir().join("global_settings.json")
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

fn repair_corrupt_global_state(path: &Path) {
    let Ok(data) = serde_json::to_string_pretty(&GlobalState::default()) else {
        return;
    };
    if let Err(error) = crate::storage::disk::atomic_write_file(path, &data) {
        tracing::warn!(
            file_path = %path.display(),
            error = %error,
            "Failed to replace corrupted global state with defaults"
        );
    }
}

/// Load plain JSON settings, accepting the previous checksum-prefixed format.
pub fn load_global_state_from_path(path: &Path) -> io::Result<GlobalState> {
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
                    repair_corrupt_global_state(&path);
                } else {
                    tracing::warn!(
                        file_path = %path.display(),
                        "Global state integrity check failed and backup failed; leaving original file"
                    );
                }
                return Ok(GlobalState::default());
            }

            // Parse JSON
            match serde_json::from_str(&json_data) {
                Ok(state) => Ok(state),
                Err(error) => {
                    if error.to_string().contains("max_consecutive_mistakes") {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "invalid max_consecutive_mistakes in {}: expected a non-negative integer; 0 disables the limit",
                                path.display()
                            ),
                        ));
                    }
                    // Create backup of corrupted file
                    if let Ok(backup_path) = crate::storage::disk::create_backup(&path) {
                        tracing::warn!(
                            file_path = %path.display(),
                            backup_path = %backup_path.display(),
                            error = %error,
                            "Created backup of corrupted global state JSON"
                        );
                        repair_corrupt_global_state(&path);
                    } else {
                        tracing::warn!(
                            file_path = %path.display(),
                            error = %error,
                            "Failed to back up corrupted global state JSON; leaving original file"
                        );
                    }
                    Ok(GlobalState::default())
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(GlobalState::default()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;
    use tracing::subscriber::with_default;
    use tracing_subscriber::filter::LevelFilter;

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

    #[test]
    fn test_legacy_workflow_toggles_are_ignored() {
        let state: GlobalState = serde_json::from_str(
            r#"{
                "remote_workflow_toggles": {"review": true},
                "global_workflow_toggles": {"release": false}
            }"#,
        )
        .unwrap();

        let serialized = serde_json::to_value(state).unwrap();
        assert!(serialized.get("remote_workflow_toggles").is_none());
        assert!(serialized.get("global_workflow_toggles").is_none());
    }

    #[test]
    fn test_legacy_telemetry_settings_are_ignored() {
        let state: GlobalState = serde_json::from_str(
            r#"{
                "telemetry_setting": "enabled",
                "open_telemetry_enabled": true,
                "open_telemetry_otlp_endpoint": "http://localhost:4318"
            }"#,
        )
        .unwrap();

        let serialized = serde_json::to_value(state).unwrap();
        assert!(serialized.get("telemetry_setting").is_none());
        assert!(serialized.get("open_telemetry_enabled").is_none());
        assert!(serialized.get("open_telemetry_otlp_endpoint").is_none());
    }

    #[test]
    fn test_load_global_state_warns_on_corrupt_json() {
        let temp_dir = TempDir::new().unwrap();
        let settings_path = temp_dir.path().join("global_settings.json");
        fs::write(&settings_path, b"{ this is not valid json").unwrap();

        let writer = TestWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer.clone())
            .without_time()
            .with_ansi(false)
            .with_level(false)
            .with_target(false)
            .with_max_level(LevelFilter::TRACE)
            .finish();

        let state = with_default(subscriber, || {
            load_global_state_from_path(&settings_path).unwrap()
        });

        assert_eq!(
            serde_json::to_value(&state).unwrap(),
            serde_json::to_value(GlobalState::default()).unwrap()
        );

        let output = writer.output();
        assert!(output.contains("corrupted global state JSON"), "{output}");
        assert!(output.contains("global_settings.json"), "{output}");
    }
}
