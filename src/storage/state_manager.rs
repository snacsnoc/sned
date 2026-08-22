use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use std::time::Instant;

use crate::storage::disk;
use crate::storage::global_state::{GlobalState, HistoryItem};
use crate::storage::secrets::SecretsStore;

/// Information about a valid config key.
#[derive(Debug, Clone)]
pub struct ConfigKeyInfo {
    pub name: &'static str,
    pub key_type: &'static str,
    pub description: &'static str,
}

/// List of all valid config keys with their types and descriptions.
pub const VALID_CONFIG_KEYS: &[ConfigKeyInfo] = &[
    ConfigKeyInfo {
        name: "act_mode_api_provider",
        key_type: "string",
        description: "API provider for act mode",
    },
    ConfigKeyInfo {
        name: "plan_mode_api_provider",
        key_type: "string",
        description: "API provider for plan mode",
    },
    ConfigKeyInfo {
        name: "act_mode_api_model_id",
        key_type: "string",
        description: "Model ID for act mode",
    },
    ConfigKeyInfo {
        name: "plan_mode_api_model_id",
        key_type: "string",
        description: "Model ID for plan mode",
    },
    ConfigKeyInfo {
        name: "azure_api_version",
        key_type: "string",
        description: "Azure OpenAI API version",
    },
    ConfigKeyInfo {
        name: "lite_llm_base_url",
        key_type: "string",
        description: "LiteLLM base URL",
    },
    ConfigKeyInfo {
        name: "anthropic_base_url",
        key_type: "string",
        description: "Anthropic API base URL",
    },
    ConfigKeyInfo {
        name: "open_ai_base_url",
        key_type: "string",
        description: "OpenAI API base URL",
    },
    ConfigKeyInfo {
        name: "open_router_base_url",
        key_type: "string",
        description: "OpenRouter API base URL",
    },
    ConfigKeyInfo {
        name: "gemini_base_url",
        key_type: "string",
        description: "Gemini API base URL",
    },
    ConfigKeyInfo {
        name: "max_consecutive_mistakes",
        key_type: "number",
        description: "Max consecutive mistakes before intervention",
    },
    ConfigKeyInfo {
        name: "enable_checkpoints_setting",
        key_type: "boolean",
        description: "Enable checkpoint saves",
    },
    ConfigKeyInfo {
        name: "strict_plan_mode_enabled",
        key_type: "boolean",
        description: "Enable strict plan mode",
    },
    ConfigKeyInfo {
        name: "subagents_enabled",
        key_type: "boolean",
        description: "Enable subagents",
    },
    ConfigKeyInfo {
        name: "sned_web_tools_enabled",
        key_type: "boolean",
        description: "Enable Sned web tools",
    },
    ConfigKeyInfo {
        name: "mode",
        key_type: "string",
        description: "Default agent mode",
    },
    ConfigKeyInfo {
        name: "enable_parallel_tool_calling",
        key_type: "boolean",
        description: "Enable parallel tool calling",
    },
];

#[derive(Debug, thiserror::Error)]
pub enum ConfigFieldError {
    #[error("unsupported config key '{0}'. Run 'sned config list' for valid keys.")]
    UnsupportedField(String),
    #[error("invalid value for key '{0}': expected {1}, got '{2}'")]
    InvalidValue(String, String, String),
}

/// Task state cache (per-task settings override)
pub type TaskState = HashMap<String, serde_json::Value>;

/// Workspace state cache (rule toggles, etc.)
pub type WorkspaceState = HashMap<String, serde_json::Value>;

// ==================== Global State Key (typed dispatch) ====================

/// Typed key for GlobalState fields.
/// Replaces string-keyed dispatch to prevent typos and improve maintainability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalStateKey {
    SnedVersion,
    TaskHistory,
    SubagentsEnabled,
    GlobalSnedRulesToggles,
    EnableCheckpoints,
    ActModeApiProvider,
    PlanModeApiProvider,
    ActModeApiModelId,
    PlanModeApiModelId,
    AzureApiVersion,
    LiteLlmBaseUrl,
    AnthropicBaseUrl,
    OpenAiBaseUrl,
    OpenRouterBaseUrl,
    GeminiBaseUrl,
    MaxConsecutiveMistakes,
    StrictPlanModeEnabled,
    SnedWebToolsEnabled,
    Mode,
    EnableParallelToolCalling,
}

impl GlobalStateKey {
    /// Get the string value for this key from GlobalState (for CLI config display).
    #[must_use]
    pub fn get_string_value(&self, state: &GlobalState) -> Option<String> {
        match self {
            Self::ActModeApiProvider => Some(state.act_mode_api_provider.clone()),
            Self::PlanModeApiProvider => Some(state.plan_mode_api_provider.clone()),
            Self::ActModeApiModelId => state.act_mode_api_model_id.clone(),
            Self::PlanModeApiModelId => state.plan_mode_api_model_id.clone(),
            Self::AzureApiVersion => state.azure_api_version.clone(),
            Self::LiteLlmBaseUrl => state.lite_llm_base_url.clone(),
            Self::AnthropicBaseUrl => state.anthropic_base_url.clone(),
            Self::OpenAiBaseUrl => state.open_ai_base_url.clone(),
            Self::OpenRouterBaseUrl => state.open_router_base_url.clone(),
            Self::GeminiBaseUrl => state.gemini_base_url.clone(),
            Self::MaxConsecutiveMistakes => Some(state.max_consecutive_mistakes.to_string()),
            Self::EnableCheckpoints => Some(state.enable_checkpoints_setting.to_string()),
            Self::StrictPlanModeEnabled => Some(state.strict_plan_mode_enabled.to_string()),
            Self::SubagentsEnabled => Some(state.subagents_enabled.to_string()),
            Self::SnedWebToolsEnabled => Some(state.sned_web_tools_enabled.to_string()),
            Self::Mode => Some(state.mode.clone()),
            Self::EnableParallelToolCalling => Some(state.enable_parallel_tool_calling.to_string()),
            Self::SnedVersion => state.sned_version.clone(),
            Self::TaskHistory | Self::GlobalSnedRulesToggles => None,
        }
    }

    /// Get the JSON value for this key from GlobalState.
    #[must_use]
    pub fn get_json_value(&self, state: &GlobalState) -> Option<serde_json::Value> {
        match self {
            Self::SnedVersion => serde_json::to_value(&state.sned_version).ok(),
            Self::TaskHistory => serde_json::to_value(&state.task_history).ok(),
            Self::SubagentsEnabled => serde_json::to_value(state.subagents_enabled).ok(),
            Self::GlobalSnedRulesToggles => {
                serde_json::to_value(&state.global_sned_rules_toggles).ok()
            }
            Self::EnableCheckpoints => serde_json::to_value(state.enable_checkpoints_setting).ok(),
            Self::ActModeApiProvider => serde_json::to_value(&state.act_mode_api_provider).ok(),
            Self::PlanModeApiProvider => serde_json::to_value(&state.plan_mode_api_provider).ok(),
            Self::ActModeApiModelId => serde_json::to_value(&state.act_mode_api_model_id).ok(),
            Self::PlanModeApiModelId => serde_json::to_value(&state.plan_mode_api_model_id).ok(),
            Self::AzureApiVersion => serde_json::to_value(&state.azure_api_version).ok(),
            Self::LiteLlmBaseUrl => state
                .lite_llm_base_url
                .as_ref()
                .map(|v| serde_json::to_value(v).unwrap()),
            Self::AnthropicBaseUrl => state
                .anthropic_base_url
                .as_ref()
                .map(|v| serde_json::to_value(v).unwrap()),
            Self::OpenAiBaseUrl => state
                .open_ai_base_url
                .as_ref()
                .map(|v| serde_json::to_value(v).unwrap()),
            Self::OpenRouterBaseUrl => state
                .open_router_base_url
                .as_ref()
                .map(|v| serde_json::to_value(v).unwrap()),
            Self::GeminiBaseUrl => state
                .gemini_base_url
                .as_ref()
                .map(|v| serde_json::to_value(v).unwrap()),
            Self::MaxConsecutiveMistakes => {
                serde_json::to_value(state.max_consecutive_mistakes).ok()
            }
            Self::StrictPlanModeEnabled => {
                serde_json::to_value(state.strict_plan_mode_enabled).ok()
            }
            Self::SnedWebToolsEnabled => serde_json::to_value(state.sned_web_tools_enabled).ok(),
            Self::Mode => serde_json::to_value(&state.mode).ok(),
            Self::EnableParallelToolCalling => {
                serde_json::to_value(state.enable_parallel_tool_calling).ok()
            }
        }
    }

    /// Set a JSON value on GlobalState for this key.
    pub fn set_json_value(&self, state: &mut GlobalState, value: serde_json::Value) {
        match self {
            Self::SnedVersion => state.sned_version = serde_json::from_value(value).ok(),
            Self::TaskHistory => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.task_history = v;
                }
            }
            Self::SubagentsEnabled => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.subagents_enabled = v;
                }
            }
            Self::GlobalSnedRulesToggles => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.global_sned_rules_toggles = v;
                }
            }
            Self::EnableCheckpoints => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.enable_checkpoints_setting = v;
                }
            }
            Self::ActModeApiProvider => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.act_mode_api_provider = v;
                }
            }
            Self::PlanModeApiProvider => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.plan_mode_api_provider = v;
                }
            }
            Self::ActModeApiModelId => {
                state.act_mode_api_model_id = serde_json::from_value(value).ok();
            }
            Self::PlanModeApiModelId => {
                state.plan_mode_api_model_id = serde_json::from_value(value).ok();
            }
            Self::AzureApiVersion => {
                state.azure_api_version = serde_json::from_value(value).ok();
            }
            Self::LiteLlmBaseUrl => {
                state.lite_llm_base_url = serde_json::from_value(value).ok();
            }
            Self::AnthropicBaseUrl => {
                state.anthropic_base_url = serde_json::from_value(value).ok();
            }
            Self::OpenAiBaseUrl => {
                state.open_ai_base_url = serde_json::from_value(value).ok();
            }
            Self::OpenRouterBaseUrl => {
                state.open_router_base_url = serde_json::from_value(value).ok();
            }
            Self::GeminiBaseUrl => {
                state.gemini_base_url = serde_json::from_value(value).ok();
            }
            Self::MaxConsecutiveMistakes => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.max_consecutive_mistakes = v;
                }
            }
            Self::StrictPlanModeEnabled => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.strict_plan_mode_enabled = v;
                }
            }
            Self::SnedWebToolsEnabled => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.sned_web_tools_enabled = v;
                }
            }
            Self::Mode => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.mode = v;
                }
            }
            Self::EnableParallelToolCalling => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.enable_parallel_tool_calling = v;
                }
            }
        }
    }
}

impl std::str::FromStr for GlobalStateKey {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "snedVersion" | "sned_version" => Ok(Self::SnedVersion),
            "taskHistory" | "task_history" => Ok(Self::TaskHistory),
            "subagentsEnabled" | "subagents_enabled" => Ok(Self::SubagentsEnabled),
            "globalSnedRulesToggles" | "global_sned_rules_toggles" => {
                Ok(Self::GlobalSnedRulesToggles)
            }
            "enableCheckpoints" | "enable_checkpoints" => Ok(Self::EnableCheckpoints),
            "actModeApiProvider" | "act_mode_api_provider" => Ok(Self::ActModeApiProvider),
            "planModeApiProvider" | "plan_mode_api_provider" => Ok(Self::PlanModeApiProvider),
            "actModeApiModelId" | "act_mode_api_model_id" => Ok(Self::ActModeApiModelId),
            "planModeApiModelId" | "plan_mode_api_model_id" => Ok(Self::PlanModeApiModelId),
            "azureApiVersion" | "azure_api_version" => Ok(Self::AzureApiVersion),
            "liteLlmBaseUrl" | "lite_llm_base_url" => Ok(Self::LiteLlmBaseUrl),
            "anthropicBaseUrl" | "anthropic_base_url" => Ok(Self::AnthropicBaseUrl),
            "openAiBaseUrl" | "open_ai_base_url" => Ok(Self::OpenAiBaseUrl),
            "openRouterBaseUrl" | "open_router_base_url" => Ok(Self::OpenRouterBaseUrl),
            "geminiBaseUrl" | "gemini_base_url" => Ok(Self::GeminiBaseUrl),
            "maxConsecutiveMistakes" | "max_consecutive_mistakes" => {
                Ok(Self::MaxConsecutiveMistakes)
            }
            "strictPlanModeEnabled" | "strict_plan_mode_enabled" => Ok(Self::StrictPlanModeEnabled),
            "snedWebToolsEnabled" | "sned_web_tools_enabled" => Ok(Self::SnedWebToolsEnabled),
            "mode" => Ok(Self::Mode),
            "enableParallelToolCalling" | "enable_parallel_tool_calling" => {
                Ok(Self::EnableParallelToolCalling)
            }
            _ => Err(()),
        }
    }
}

impl std::fmt::Display for GlobalStateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SnedVersion => write!(f, "snedVersion"),
            Self::TaskHistory => write!(f, "taskHistory"),
            Self::SubagentsEnabled => write!(f, "subagentsEnabled"),
            Self::GlobalSnedRulesToggles => write!(f, "globalSnedRulesToggles"),
            Self::EnableCheckpoints => write!(f, "enableCheckpoints"),
            Self::ActModeApiProvider => write!(f, "actModeApiProvider"),
            Self::PlanModeApiProvider => write!(f, "planModeApiProvider"),
            Self::ActModeApiModelId => write!(f, "actModeApiModelId"),
            Self::PlanModeApiModelId => write!(f, "planModeApiModelId"),
            Self::AzureApiVersion => write!(f, "azureApiVersion"),
            Self::LiteLlmBaseUrl => write!(f, "liteLlmBaseUrl"),
            Self::AnthropicBaseUrl => write!(f, "anthropicBaseUrl"),
            Self::OpenAiBaseUrl => write!(f, "openAiBaseUrl"),
            Self::OpenRouterBaseUrl => write!(f, "openRouterBaseUrl"),
            Self::GeminiBaseUrl => write!(f, "geminiBaseUrl"),
            Self::MaxConsecutiveMistakes => write!(f, "maxConsecutiveMistakes"),
            Self::StrictPlanModeEnabled => write!(f, "strictPlanModeEnabled"),
            Self::SnedWebToolsEnabled => write!(f, "snedWebToolsEnabled"),
            Self::Mode => write!(f, "mode"),
            Self::EnableParallelToolCalling => write!(f, "enableParallelToolCalling"),
        }
    }
}

/// In-memory state manager with async disk persistence.
///
///
/// Key behaviors preserved:
/// - In-memory cache for fast reads (no disk I/O on reads after init)
/// - Async disk persistence with debouncing (1-second delay)
/// - Separate persistence paths for global state, task history, task state, secrets, workspace state
/// - Task history routed to its own file (`~/.sned/data/state/taskHistory.json`)
/// - Per-task settings routed to task directories
pub struct StateManager {
    /// Global state + settings cache
    global_state: RwLock<GlobalState>,

    /// Task state cache (per-task settings)
    task_state: RwLock<HashMap<String, TaskState>>,

    /// Secrets cache
    secrets: RwLock<HashMap<String, String>>,

    /// Workspace state cache
    workspace_state: RwLock<WorkspaceState>,

    /// Pending keys to persist (debounced)
    pending_global_keys: Mutex<HashSet<String>>,
    pending_task_states: Mutex<HashMap<String, HashSet<String>>>,
    pending_secrets: Mutex<HashSet<String>>,

    /// Last persistence time
    last_persist: Mutex<Option<Instant>>,

    /// True if workspace_state has unsaved changes since the last
    /// persist_workspace_state() call. Skips the disk write when false.
    workspace_state_dirty: AtomicBool,

    /// Secrets store for file-backed storage
    secrets_store: SecretsStore,

    /// State directory
    state_dir: PathBuf,
}

impl StateManager {
    /// Create a new StateManager with default paths
    pub fn new() -> io::Result<Self> {
        let state_dir = disk::get_data_dir().join("state");
        fs::create_dir_all(&state_dir)?;

        let secrets_store = SecretsStore::new()?;

        Ok(Self {
            global_state: RwLock::new(GlobalState::default()),
            task_state: RwLock::new(HashMap::with_capacity(8)),
            secrets: RwLock::new(HashMap::with_capacity(4)),
            workspace_state: RwLock::new(HashMap::with_capacity(8)),
            pending_global_keys: Mutex::new(HashSet::new()),
            pending_task_states: Mutex::new(HashMap::with_capacity(4)),
            pending_secrets: Mutex::new(HashSet::new()),
            last_persist: Mutex::new(None),
            workspace_state_dirty: AtomicBool::new(false),
            secrets_store,
            state_dir,
        })
    }

    /// Initialize the state manager from disk.
    /// Loads global state, task history, secrets, and workspace state.
    /// Cleans up orphaned atomic write temp files older than 24 hours.
    pub fn initialize(&self) -> io::Result<()> {
        // Clean up orphaned temp files from crashed atomic writes
        let settings_dir = self.state_dir.join("..").join("settings");
        let _ = crate::storage::disk::cleanup_orphaned_temp_files(
            &settings_dir,
            std::time::Duration::from_hours(24), // 24 hours
        );

        // TaskStorage uses atomic_write_file too; orphaned .tmp.*.json
        // files in the tasks dir need the same cleanup on startup.
        let tasks_dir = self.state_dir.join("..").join("tasks");
        let _ = crate::storage::disk::cleanup_orphaned_temp_files(
            &tasks_dir,
            std::time::Duration::from_hours(24), // 24 hours
        );

        // Load global state
        let global_state = self.load_global_state()?;
        *self
            .global_state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = global_state;

        // Load secrets
        let secrets = self.secrets_store.load();
        *self
            .secrets
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = secrets;

        // Load workspace state (if exists)
        if let Ok(workspace_state) = self.load_workspace_state() {
            *self
                .workspace_state
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = workspace_state;
        }

        // Load task states from disk (SM4 fix)
        let tasks_dir = self.state_dir.join("..").join("tasks");
        if tasks_dir.exists() {
            let mut task_states = self
                .task_state
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut pending_task_states = self
                .pending_task_states
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);

            if let Ok(entries) = fs::read_dir(&tasks_dir) {
                for entry in entries.flatten() {
                    let task_dir = entry.path();
                    if task_dir.is_dir()
                        && let Some(task_id) = task_dir.file_name().and_then(|n| n.to_str())
                    {
                        let settings_path = task_dir.join("settings.json");
                        if settings_path.exists()
                            && let Some(parsed) =
                                self.read_task_settings_with_backup(&settings_path)
                        {
                            // Convert Map to HashMap to match task_state type
                            let task_state_map: HashMap<String, serde_json::Value> =
                                parsed.into_iter().collect();
                            let keys: HashSet<String> = task_state_map.keys().cloned().collect();
                            task_states.insert(task_id.to_string(), task_state_map);
                            // Mark all loaded keys as pending to ensure they're persisted
                            pending_task_states
                                .entry(task_id.to_string())
                                .or_default()
                                .extend(keys);
                        }
                    }
                }
            }
            drop(task_states);
            drop(pending_task_states);
        }

        Ok(())
    }

    /// Get the distinct ID used by hooks.
    ///
    /// Sned no longer persists a machine identifier, so hooks receive a stable
    /// anonymous value unless they add their own identifier.
    pub fn get_distinct_id(&self) -> String {
        "anonymous".to_string()
    }

    // ==================== Task History ====================

    /// Get task history from cache
    pub fn get_task_history(&self) -> Vec<HistoryItem> {
        self.global_state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .task_history
            .clone()
    }

    /// Set task history in cache and mark for persistence
    pub fn set_task_history(&self, history: Vec<HistoryItem>) {
        self.global_state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .task_history = history;
        self.mark_global_key_pending("taskHistory".to_string());
    }

    /// Add a task to history (or update existing)
    pub fn add_task_to_history(&self, item: HistoryItem) {
        let mut state = self
            .global_state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        state.task_history.retain(|h| h.id != item.id);
        state.task_history.push(item);
        state.task_history.sort_by_key(|b| std::cmp::Reverse(b.ts));

        drop(state);
        self.mark_global_key_pending("taskHistory".to_string());
    }

    /// Remove a task from history
    pub fn remove_task_from_history(&self, task_id: &str) {
        let mut state = self
            .global_state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.task_history.retain(|h| h.id != task_id);
        drop(state);
        self.mark_global_key_pending("taskHistory".to_string());
    }

    /// Find a task in history by ID
    pub fn find_task_in_history(&self, task_id: &str) -> Option<HistoryItem> {
        self.global_state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .task_history
            .iter()
            .find(|h| h.id == task_id)
            .cloned()
    }

    /// Get the most recent task for a workspace
    pub fn get_most_recent_task_for_workspace(&self, workspace_path: &str) -> Option<HistoryItem> {
        self.global_state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .task_history
            .iter()
            .filter(|h| {
                // Check if workspace path matches
                h.workspace_root_path
                    .as_ref()
                    .is_some_and(|p| workspace_paths_match(p, workspace_path))
            })
            .max_by_key(|h| h.ts)
            .cloned()
    }

    // ==================== Global State ====================

    /// Return a consistent snapshot for runtime consumers that need multiple settings.
    #[must_use]
    pub fn global_state_snapshot(&self) -> GlobalState {
        self.global_state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Get a global state key (typed enum version)
    pub fn get_global_state_key<T>(&self, key: GlobalStateKey) -> Option<T>
    where
        T: Clone + for<'de> Deserialize<'de>,
    {
        let state = self
            .global_state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let json_value = key.get_json_value(&state)?;
        drop(state);
        match serde_json::from_value(json_value) {
            Ok(value) => Some(value),
            Err(error) => {
                tracing::warn!(
                    key = %key,
                    error = %error,
                    "failed to deserialize global state value"
                );
                None
            }
        }
    }

    /// Get a config value by string key name.
    pub fn get_config_value(&self, key: &str) -> Option<String> {
        let enum_key = key.parse::<GlobalStateKey>().ok()?;
        let state = self
            .global_state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        enum_key.get_string_value(&state)
    }

    /// Set a global state key (typed enum version)
    pub fn set_global_state_key(&self, key: GlobalStateKey, value: serde_json::Value) {
        {
            let mut state = self
                .global_state
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            key.set_json_value(&mut state, value);
        }
        self.mark_global_key_pending(key.to_string());
    }

    /// Set a string-backed global config field by its JSON field name.
    pub fn set_global_state_string_field(
        &self,
        key: &str,
        value: String,
    ) -> Result<(), ConfigFieldError> {
        // First, check if the key is valid
        let Some(key_info) = VALID_CONFIG_KEYS.iter().find(|k| k.name == key) else {
            return Err(ConfigFieldError::UnsupportedField(key.to_string()));
        };

        // Validate value type matches expected type
        match key_info.key_type {
            "number" => {
                let parsed = value.parse::<i32>().map_err(|_| {
                    ConfigFieldError::InvalidValue(
                        key.to_string(),
                        "a non-negative integer (0 disables the limit)".to_string(),
                        value.clone(),
                    )
                })?;
                if parsed < 0 {
                    return Err(ConfigFieldError::InvalidValue(
                        key.to_string(),
                        "a non-negative integer (0 disables the limit)".to_string(),
                        value,
                    ));
                }
            }
            "boolean" if !matches!(value.to_lowercase().as_str(), "true" | "false" | "1" | "0") => {
                return Err(ConfigFieldError::InvalidValue(
                    key.to_string(),
                    "boolean".to_string(),
                    value,
                ));
            }
            _ => {} // String fields accept any value
        }

        let mut state = self
            .global_state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let handled = match key {
            "act_mode_api_provider" => {
                state.act_mode_api_provider = value;
                true
            }
            "plan_mode_api_provider" => {
                state.plan_mode_api_provider = value;
                true
            }
            "act_mode_api_model_id" => {
                state.act_mode_api_model_id = Some(value);
                true
            }
            "plan_mode_api_model_id" => {
                state.plan_mode_api_model_id = Some(value);
                true
            }
            "azure_api_version" => {
                state.azure_api_version = Some(value);
                true
            }
            "lite_llm_base_url" => {
                state.lite_llm_base_url = Some(value);
                true
            }
            "anthropic_base_url" => {
                state.anthropic_base_url = Some(value);
                true
            }
            "open_ai_base_url" => {
                state.open_ai_base_url = Some(value);
                true
            }
            "open_router_base_url" => {
                state.open_router_base_url = Some(value);
                true
            }
            "gemini_base_url" => {
                state.gemini_base_url = Some(value);
                true
            }
            "max_consecutive_mistakes" => {
                state.max_consecutive_mistakes = value.parse().expect("validated above");
                true
            }
            "enable_checkpoints_setting" => {
                state.enable_checkpoints_setting =
                    matches!(value.to_lowercase().as_str(), "true" | "1");
                true
            }
            "strict_plan_mode_enabled" => {
                state.strict_plan_mode_enabled =
                    matches!(value.to_lowercase().as_str(), "true" | "1");
                true
            }
            "sned_web_tools_enabled" => {
                state.sned_web_tools_enabled =
                    matches!(value.to_lowercase().as_str(), "true" | "1");
                true
            }
            "mode" => {
                state.mode = value;
                true
            }
            "enable_parallel_tool_calling" => {
                state.enable_parallel_tool_calling =
                    matches!(value.to_lowercase().as_str(), "true" | "1");
                true
            }
            _ => false,
        };
        drop(state);

        if handled {
            self.mark_global_key_pending(key.to_string());
            Ok(())
        } else {
            Err(ConfigFieldError::UnsupportedField(key.to_string()))
        }
    }

    /// Get per-path auto-approval patterns from global settings.
    pub fn get_auto_approve_patterns(&self) -> Vec<String> {
        self.global_state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .auto_approve_patterns
            .clone()
    }

    /// Get task state for a specific task
    pub fn get_task_state(&self, task_id: &str, key: &str) -> Option<serde_json::Value> {
        self.task_state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(task_id)
            .and_then(|state| state.get(key).cloned())
    }

    /// Set task state for a specific task
    pub fn set_task_state(&self, task_id: &str, key: &str, value: serde_json::Value) {
        {
            let mut task_states = self
                .task_state
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let task_state = task_states.entry(task_id.to_string()).or_default();
            task_state.insert(key.to_string(), value);
            drop(task_states);
        }

        let mut pending = self
            .pending_task_states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending
            .entry(task_id.to_string())
            .or_default()
            .insert(key.to_string());
        drop(pending);
    }

    // ==================== Secrets ====================

    /// Get a secret
    pub fn get_secret(&self, key: &str) -> Option<String> {
        let cached_secret = self
            .secrets
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .cloned();
        if let Some(secret) = cached_secret {
            return Some(secret);
        }

        let secret = self.secrets_store.get(key)?;
        self.secrets
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key.to_string(), secret.clone());
        Some(secret)
    }

    /// Set a secret
    pub fn set_secret(&self, key: &str, value: String) {
        {
            let mut secrets = self
                .secrets
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            secrets.insert(key.to_string(), value);
        }
        self.mark_secret_pending(key.to_string());
    }

    // ==================== Persistence ====================

    /// Load global state from disk
    fn load_global_state(&self) -> io::Result<GlobalState> {
        crate::storage::global_state::load_global_state_from_path(
            &crate::storage::global_state::global_settings_path(),
        )
    }

    /// Load workspace state from disk
    fn load_workspace_state(&self) -> io::Result<WorkspaceState> {
        let file_path = self.state_dir.join("workspace_state.json");
        if !file_path.exists() {
            return Ok(HashMap::with_capacity(0));
        }

        let contents = fs::read_to_string(&file_path)?;
        match serde_json::from_str(&contents) {
            Ok(state) => Ok(state),
            Err(e) => {
                // Create backup of corrupted file before discarding
                if let Ok(backup_path) = crate::storage::disk::create_backup(&file_path) {
                    tracing::warn!(
                        file_path = %file_path.display(),
                        backup_path = %backup_path.display(),
                        error = %e,
                        "Created backup of corrupted workspace state JSON"
                    );
                } else {
                    tracing::warn!(
                        file_path = %file_path.display(),
                        error = %e,
                        "Failed to parse workspace state JSON and backup failed"
                    );
                }
                Ok(HashMap::with_capacity(0))
            }
        }
    }

    /// Mark a global key as pending persistence
    fn mark_global_key_pending(&self, key: String) {
        let mut pending = self
            .pending_global_keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.insert(key);
    }

    /// Mark a secret as pending persistence
    fn mark_secret_pending(&self, key: String) {
        let mut pending = self
            .pending_secrets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.insert(key);
    }

    #[allow(clippy::unused_self)]
    fn read_task_settings_with_backup(
        &self,
        file_path: &Path,
    ) -> Option<serde_json::Map<String, Value>> {
        match fs::read_to_string(file_path) {
            Ok(contents) => match serde_json::from_str::<serde_json::Map<String, Value>>(&contents)
            {
                Ok(data) => Some(data),
                Err(e) => {
                    if let Ok(backup_path) = crate::storage::disk::create_backup(file_path) {
                        tracing::warn!(
                            file_path = %file_path.display(),
                            backup_path = %backup_path.display(),
                            error = %e,
                            "Created backup of corrupted task settings JSON"
                        );
                    } else {
                        tracing::warn!(
                            file_path = %file_path.display(),
                            error = %e,
                            "Failed to parse task settings JSON and backup failed"
                        );
                    }
                    None
                }
            },
            Err(_) => None,
        }
    }

    /// Persist all pending changes to disk.
    /// This is called periodically or on explicit flush.
    pub fn persist(&self) -> io::Result<()> {
        // Persist global state
        let global_keys: HashSet<String> = {
            let pending = self
                .pending_global_keys
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.clone()
        };

        if !global_keys.is_empty() {
            self.persist_global_state(&global_keys)?;
            let mut pending = self
                .pending_global_keys
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for key in &global_keys {
                pending.remove(key);
            }
        }

        // Persist task states
        let task_states: HashMap<String, HashSet<String>> = {
            let pending = self
                .pending_task_states
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.clone()
        };

        if !task_states.is_empty() {
            self.persist_task_states(&task_states)?;
            let mut pending = self
                .pending_task_states
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for task_id in task_states.keys() {
                pending.remove(task_id);
            }
        }

        // Persist secrets
        let secrets: HashSet<String> = {
            let pending = self
                .pending_secrets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.clone()
        };

        if !secrets.is_empty() {
            self.persist_secrets(&secrets)?;
            let mut pending = self
                .pending_secrets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for key in &secrets {
                pending.remove(key);
            }
        }

        // Persist workspace state
        self.persist_workspace_state()?;

        // Update last persist time
        *self
            .last_persist
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());

        Ok(())
    }

    /// Persist all pending changes to disk asynchronously.
    /// Wraps sync `persist()` in spawn_blocking to avoid blocking tokio workers.
    /// Call with Arc::clone(&state_manager) to avoid borrowing issues.
    pub async fn persist_async(this: Arc<Self>) -> io::Result<()> {
        tokio::task::spawn_blocking(move || this.persist())
            .await
            .map_err(io::Error::other)?
    }

    fn persist_global_state(&self, _keys: &HashSet<String>) -> io::Result<()> {
        let state = self
            .global_state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let settings_dir = self.state_dir.join("..").join("settings");
        fs::create_dir_all(&settings_dir)?;

        self.persist_full_global_state(&state, &settings_dir)
    }

    #[allow(clippy::unused_self)]
    fn persist_full_global_state(
        &self,
        state: &GlobalState,
        settings_dir: &Path,
    ) -> io::Result<()> {
        let data = serde_json::to_string_pretty(state)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let file_path = settings_dir.join("global_settings.json");
        disk::atomic_write_file(&file_path, &data)?;
        Ok(())
    }

    /// Persist task states to disk
    fn persist_task_states(
        &self,
        task_states: &HashMap<String, HashSet<String>>,
    ) -> io::Result<()> {
        let states = self
            .task_state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        for (task_id, keys) in task_states {
            if let Some(task_state) = states.get(task_id) {
                let task_dir = self.state_dir.join("..").join("tasks").join(task_id);
                fs::create_dir_all(&task_dir)?;

                // Read existing settings for read-merge-write (SM3 fix)
                let file_path = task_dir.join("settings.json");
                let mut existing_settings = if file_path.exists()
                    && let Some(parsed) = self.read_task_settings_with_backup(&file_path)
                {
                    parsed
                } else {
                    serde_json::Map::new()
                };

                // Merge pending keys into existing settings
                for key in keys {
                    if let Some(value) = task_state.get(key) {
                        existing_settings.insert(key.clone(), value.clone());
                    }
                }

                if !existing_settings.is_empty() {
                    let data = serde_json::to_string_pretty(&existing_settings)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                    disk::atomic_write_file(&file_path, &data)?;
                }
            }
        }

        Ok(())
    }

    /// Persist secrets to disk
    fn persist_secrets(&self, keys: &HashSet<String>) -> io::Result<()> {
        let secrets = self
            .secrets
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Use secrets_store.set() for each key to do proper read-merge-write
        // This prevents overwriting the entire file with only pending keys (S7 fix)
        for key in keys {
            if let Some(value) = secrets.get(key) {
                self.secrets_store.set(key, value)?;
            }
        }
        Ok(())
    }

    /// Persist workspace state to disk.
    /// Writes the entire workspace state atomically. Skips the write
    /// when the dirty flag is false (no mutations since last persist).
    fn persist_workspace_state(&self) -> io::Result<()> {
        if !self.workspace_state_dirty.swap(false, Ordering::AcqRel) {
            return Ok(());
        }

        let workspace_state = self
            .workspace_state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let file_path = self.state_dir.join("workspace_state.json");
        let data = serde_json::to_string_pretty(&*workspace_state)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        drop(workspace_state);

        crate::storage::disk::atomic_write_file(&file_path, &data)?;
        Ok(())
    }
}

/// Task history operations
/// List tasks with pagination
#[must_use]
pub fn list_tasks(items: &[HistoryItem], page: usize, limit: usize) -> (Vec<HistoryItem>, usize) {
    let total = items.len();
    let start = (page - 1) * limit;
    let end = (start + limit).min(total);

    if start >= total {
        return (Vec::new(), total);
    }

    let page_items: Vec<HistoryItem> = items[start..end].to_vec();
    (page_items, total)
}

/// Sort tasks by timestamp (newest first)
pub fn sort_by_timestamp(items: &mut [HistoryItem]) {
    items.sort_by_key(|b| std::cmp::Reverse(b.ts));
}

/// Get total pages
#[must_use]
pub fn total_pages(total: usize, limit: usize) -> usize {
    total.div_ceil(limit)
}

fn workspace_paths_match(history_path: &str, workspace_path: &str) -> bool {
    if history_path == workspace_path {
        return true;
    }

    match (
        Path::new(history_path).canonicalize(),
        Path::new(workspace_path).canonicalize(),
    ) {
        (Ok(history), Ok(workspace)) => history == workspace,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex, OnceLock};
    use tempfile::TempDir;

    fn with_temp_data_dir<T>(f: impl FnOnce() -> T) -> T {
        static TEST_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _guard = TEST_ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let temp_dir = TempDir::new().unwrap();
        let sned_dir = temp_dir.path().join("sned");
        fs::create_dir_all(sned_dir.join("data")).unwrap();

        let previous_sned_dir = std::env::var_os("SNED_DIR");

        // SAFETY: single-threaded test helper; env mutation scoped to this closure.
        unsafe {
            std::env::set_var("SNED_DIR", &sned_dir);
        }

        let result = f();

        // SAFETY: single-threaded test helper; restoring env after test.
        unsafe {
            if let Some(previous_sned_dir) = previous_sned_dir {
                std::env::set_var("SNED_DIR", previous_sned_dir);
            } else {
                std::env::remove_var("SNED_DIR");
            }
        }

        result
    }

    #[test]
    fn test_state_manager_creation() {
        with_temp_data_dir(|| {
            let manager = StateManager::new();
            assert!(manager.is_ok());
        });
    }

    #[test]
    fn test_telemetry_keys_are_not_configurable() {
        assert!("telemetry_setting".parse::<GlobalStateKey>().is_err());
        assert!("open_telemetry_enabled".parse::<GlobalStateKey>().is_err());
        assert!(
            !VALID_CONFIG_KEYS
                .iter()
                .any(|key| key.name.contains("telemetry"))
        );
    }

    #[test]
    fn test_task_history_operations() {
        with_temp_data_dir(|| {
            let manager = StateManager::new().unwrap();
            manager.initialize().unwrap();

            // Clear any existing history first
            manager.set_task_history(Vec::new());

            // Add tasks
            let task1 = HistoryItem {
                id: "task-1".to_string(),
                number: 1,
                ts: 1000,
                task: "Test task 1".to_string(),
                tokens_in: 100,
                tokens_out: 50,
                cache_writes: None,
                cache_reads: None,
                total_cost: 0.01,
                ..Default::default()
            };

            let task2 = HistoryItem {
                id: "task-2".to_string(),
                number: 2,
                ts: 2000,
                task: "Test task 2".to_string(),
                tokens_in: 200,
                tokens_out: 100,
                cache_writes: None,
                cache_reads: None,
                total_cost: 0.02,
                ..Default::default()
            };

            manager.add_task_to_history(task1.clone());
            manager.add_task_to_history(task2.clone());

            // Get history
            let history = manager.get_task_history();
            assert_eq!(history.len(), 2);

            // Should be sorted by timestamp (descending)
            assert_eq!(history[0].id, "task-2");
            assert_eq!(history[1].id, "task-1");

            // Find task
            let found = manager.find_task_in_history("task-1");
            assert!(found.is_some());
            assert_eq!(found.unwrap().task, "Test task 1");

            // Remove task
            manager.remove_task_from_history("task-1");
            let history = manager.get_task_history();
            assert_eq!(history.len(), 1);
            assert_eq!(history[0].id, "task-2");
        });
    }

    #[test]
    fn test_get_most_recent_task_for_workspace_matches_canonical_equivalent_paths() {
        with_temp_data_dir(|| {
            let manager = StateManager::new().unwrap();
            manager.initialize().unwrap();
            manager.set_task_history(Vec::new());

            let workspace_root = TempDir::new().unwrap();
            let workspace = workspace_root.path().join("workspace");
            fs::create_dir_all(&workspace).unwrap();

            let older = HistoryItem {
                id: "task-1".to_string(),
                ts: 1000,
                task: "Older".to_string(),
                workspace_root_path: Some(workspace.join(".").to_string_lossy().into_owned()),
                ..Default::default()
            };
            let newer = HistoryItem {
                id: "task-2".to_string(),
                ts: 2000,
                task: "Newer".to_string(),
                workspace_root_path: Some(
                    workspace.join("subdir/..").to_string_lossy().into_owned(),
                ),
                ..Default::default()
            };

            fs::create_dir_all(workspace.join("subdir")).unwrap();
            manager.add_task_to_history(older);
            manager.add_task_to_history(newer);

            let found = manager
                .get_most_recent_task_for_workspace(workspace.to_str().unwrap())
                .unwrap();
            assert_eq!(found.id, "task-2");
        });
    }

    #[test]
    fn test_task_history_pagination() {
        let mut items = vec![
            HistoryItem {
                id: "1".to_string(),
                number: 1,
                ts: 1000,
                task: "Task 1".to_string(),
                tokens_in: 0,
                tokens_out: 0,
                cache_writes: None,
                cache_reads: None,
                total_cost: 0.0,
                ..Default::default()
            },
            HistoryItem {
                id: "2".to_string(),
                number: 2,
                ts: 2000,
                task: "Task 2".to_string(),
                tokens_in: 0,
                tokens_out: 0,
                cache_writes: None,
                cache_reads: None,
                total_cost: 0.0,
                ..Default::default()
            },
            HistoryItem {
                id: "3".to_string(),
                number: 3,
                ts: 3000,
                task: "Task 3".to_string(),
                tokens_in: 0,
                tokens_out: 0,
                cache_writes: None,
                cache_reads: None,
                total_cost: 0.0,
                ..Default::default()
            },
            HistoryItem {
                id: "4".to_string(),
                number: 4,
                ts: 4000,
                task: "Task 4".to_string(),
                tokens_in: 0,
                tokens_out: 0,
                cache_writes: None,
                cache_reads: None,
                total_cost: 0.0,
                ..Default::default()
            },
            HistoryItem {
                id: "5".to_string(),
                number: 5,
                ts: 5000,
                task: "Task 5".to_string(),
                tokens_in: 0,
                tokens_out: 0,
                cache_writes: None,
                cache_reads: None,
                total_cost: 0.0,
                ..Default::default()
            },
        ];

        sort_by_timestamp(&mut items);

        let (page, total) = list_tasks(&items, 1, 2);
        assert_eq!(total, 5);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].id, "5");
        assert_eq!(page[1].id, "4");

        let (page, _) = list_tasks(&items, 2, 2);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].id, "3");
        assert_eq!(page[1].id, "2");

        let (page, _) = list_tasks(&items, 3, 2);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].id, "1");
    }

    #[test]
    fn test_persist_and_load() {
        with_temp_data_dir(|| {
            let manager = StateManager::new().unwrap();
            manager.initialize().unwrap();

            // Add a task
            let task = HistoryItem {
                id: "persist-test".to_string(),
                number: 1,
                ts: 1000,
                task: "Persist test".to_string(),
                tokens_in: 100,
                tokens_out: 50,
                cache_writes: None,
                cache_reads: None,
                total_cost: 0.01,
                ..Default::default()
            };

            manager.add_task_to_history(task);

            // Persist
            manager.persist().unwrap();

            // Create a new manager and load
            let manager2 = StateManager::new().unwrap();
            manager2.initialize().unwrap();

            let history = manager2.get_task_history();
            assert!(history.iter().any(|h| h.id == "persist-test"));
        });
    }

    #[test]
    fn test_set_secret_persist_makes_it_available_to_a_new_manager() {
        with_temp_data_dir(|| {
            let manager = Arc::new(StateManager::new().unwrap());
            manager.set_secret("apiKey", "persisted-test-key".to_string());
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(StateManager::persist_async(Arc::clone(&manager)))
                .unwrap();

            let reloaded = StateManager::new().unwrap();
            assert_eq!(
                reloaded.get_secret("apiKey").as_deref(),
                Some("persisted-test-key")
            );
        });
    }

    #[test]
    fn test_workspace_state_dirty_flag_skips_unchanged_writes() {
        with_temp_data_dir(|| {
            let manager = StateManager::new().unwrap();
            manager.initialize().unwrap();

            // Capture the file mtime after initialize (which is a no-op
            // persist because no workspace_state mutator has run, but
            // the dirty flag starts false so persist_workspace_state
            // short-circuits).
            let file_path = manager.state_dir.join("workspace_state.json");
            let initial_exists = file_path.exists();

            // Run persist multiple times; since workspace_state is
            // never mutated, the dirty flag stays false and no write
            // happens.
            for _ in 0..3 {
                manager.persist().unwrap();
            }

            // The file should not have been created by persist (it only
            // exists if load_workspace_state saw it on disk, which it
            // didn't in a fresh temp dir).
            assert!(!file_path.exists() || initial_exists);
        });
    }

    #[test]
    fn test_global_state_persists_retained_keys() {
        with_temp_data_dir(|| {
            let manager = StateManager::new().unwrap();
            manager.initialize().unwrap();

            manager.set_global_state_key(
                GlobalStateKey::SubagentsEnabled,
                serde_json::Value::Bool(false),
            );
            manager.set_global_state_key(
                GlobalStateKey::StrictPlanModeEnabled,
                serde_json::Value::Bool(true),
            );

            manager.persist().unwrap();

            let manager2 = StateManager::new().unwrap();
            manager2.initialize().unwrap();

            let subagents_enabled: Option<serde_json::Value> =
                manager2.get_global_state_key(GlobalStateKey::SubagentsEnabled);
            assert_eq!(subagents_enabled, Some(serde_json::Value::Bool(false)));

            let strict_plan: Option<serde_json::Value> =
                manager2.get_global_state_key(GlobalStateKey::StrictPlanModeEnabled);
            assert_eq!(strict_plan, Some(serde_json::Value::Bool(true)));
        });
    }

    #[test]
    fn test_max_consecutive_mistakes_rejects_invalid_values() {
        with_temp_data_dir(|| {
            let manager = StateManager::new().unwrap();
            manager.initialize().unwrap();

            for value in ["-1", "many"] {
                let error = manager
                    .set_global_state_string_field("max_consecutive_mistakes", value.to_string())
                    .expect_err("invalid mistake limits must be rejected");
                assert!(error.to_string().contains("0 disables the limit"));
            }

            manager
                .set_global_state_string_field("max_consecutive_mistakes", "0".to_string())
                .unwrap();
            assert_eq!(
                manager.get_config_value("max_consecutive_mistakes").as_deref(),
                Some("0")
            );
        });
    }

    #[test]
    fn test_invalid_persisted_mistake_limit_is_actionable() {
        with_temp_data_dir(|| {
            let settings_dir = crate::storage::disk::get_settings_dir();
            fs::create_dir_all(&settings_dir).unwrap();
            fs::write(
                settings_dir.join("global_settings.json"),
                r#"{"max_consecutive_mistakes":"many"}"#,
            )
            .unwrap();

            let error = StateManager::new().unwrap().initialize().unwrap_err();
            assert!(error.to_string().contains("max_consecutive_mistakes"));
            assert!(error.to_string().contains("0 disables the limit"));
        });
    }

    #[test]
    fn test_legacy_negative_mistake_limit_can_be_repaired() {
        with_temp_data_dir(|| {
            let settings_dir = crate::storage::disk::get_settings_dir();
            fs::create_dir_all(&settings_dir).unwrap();
            fs::write(
                settings_dir.join("global_settings.json"),
                r#"{"max_consecutive_mistakes":-1}"#,
            )
            .unwrap();

            let manager = StateManager::new().unwrap();
            manager.initialize().unwrap();
            manager
                .set_global_state_string_field("max_consecutive_mistakes", "3".to_string())
                .unwrap();
            manager.persist().unwrap();

            let repaired = StateManager::new().unwrap();
            repaired.initialize().unwrap();
            assert_eq!(repaired.get_config_value("max_consecutive_mistakes").as_deref(), Some("3"));
        });
    }

    #[test]
    fn test_global_state_persist_removes_unused_fields() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path().join("data");
        let state_dir = data_dir.join("state");
        let settings_dir = data_dir.join("settings");
        fs::create_dir_all(&state_dir).unwrap();
        fs::create_dir_all(&settings_dir).unwrap();

        let initial_state = serde_json::json!({
            "subagents_enabled": false,
            "terminal_reuse_enabled": false,
        });
        let mut manager = StateManager::new().unwrap();
        manager.state_dir = state_dir;
        *manager
            .global_state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            serde_json::from_value(initial_state).unwrap();

        manager.set_global_state_key(
            GlobalStateKey::SubagentsEnabled,
            serde_json::Value::Bool(true),
        );
        manager.persist().unwrap();

        let persisted: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(settings_dir.join("global_settings.json")).unwrap(),
        )
        .unwrap();

        assert_eq!(
            persisted["subagents_enabled"],
            serde_json::Value::Bool(true)
        );
        assert!(persisted.get("terminal_reuse_enabled").is_none());
    }

    #[test]
    fn test_get_global_state_key_warns_on_deserialization_failure() {
        use std::io::{self, Write};
        use std::sync::Arc;

        struct TestWriter(Arc<Mutex<Vec<u8>>>);

        impl Write for TestWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        with_temp_data_dir(|| {
            let manager = StateManager::new().unwrap();
            manager.initialize().unwrap();
            manager.set_global_state_key(
                GlobalStateKey::SubagentsEnabled,
                serde_json::Value::Bool(true),
            );

            let captured = Arc::new(Mutex::new(Vec::new()));
            let captured_for_writer = captured.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::WARN)
                .with_ansi(false)
                .with_writer(move || TestWriter(captured_for_writer.clone()))
                .finish();
            let _guard = tracing::subscriber::set_default(subscriber);

            let value: Option<String> =
                manager.get_global_state_key(GlobalStateKey::SubagentsEnabled);
            assert!(value.is_none());

            let log_output = String::from_utf8(
                captured
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
            )
            .unwrap();
            assert!(log_output.contains("failed to deserialize global state value"));
            assert!(log_output.contains("key=subagentsEnabled"));
            assert!(log_output.contains("error="));
        });
    }

    #[test]
    fn test_distinct_id_is_anonymous_without_machine_identity() {
        with_temp_data_dir(|| {
            let manager = StateManager::new().unwrap();
            manager.initialize().unwrap();

            let id = manager.get_distinct_id();
            assert_eq!(id, "anonymous");
        });
    }

    #[test]
    fn test_load_global_state_creates_backup_on_corruption() {
        use std::fs;

        with_temp_data_dir(|| {
            let manager = StateManager::new().unwrap();
            manager.initialize().unwrap();

            // Write corrupted JSON to global_settings.json
            let settings_path = manager
                .state_dir
                .join("..")
                .join("settings")
                .join("global_settings.json");
            fs::create_dir_all(settings_path.parent().unwrap()).unwrap();

            let corrupted_content = r#"{"invalid json"#;
            fs::write(&settings_path, corrupted_content).unwrap();

            // Load global state - should create backup and return defaults
            let result = manager.load_global_state();
            assert!(result.is_ok(), "Should return Ok even with corrupted file");

            let state = result.unwrap();
            assert_eq!(state.max_consecutive_mistakes, 3);

            // Verify backup was created
            let backup_path = settings_path.with_extension("json.bak");
            assert!(
                backup_path.exists(),
                "Backup file should be created for corrupted JSON"
            );

            // Verify backup contains original corrupted content
            let backup_content = fs::read_to_string(&backup_path).unwrap();
            assert_eq!(
                backup_content, corrupted_content,
                "Backup should contain original corrupted content"
            );
        });
    }

    #[test]
    fn test_initialize_creates_backup_on_corrupt_task_settings() {
        use std::fs;

        with_temp_data_dir(|| {
            let tasks_dir = crate::storage::disk::get_tasks_dir();
            let task_dir = tasks_dir.join("task-a");
            fs::create_dir_all(&task_dir).unwrap();

            let settings_path = task_dir.join("settings.json");
            let corrupted_content = r#"{"mode":"act""#;
            fs::write(&settings_path, corrupted_content).unwrap();

            let manager = StateManager::new().unwrap();
            manager.initialize().unwrap();

            let backup_path = settings_path.with_extension("json.bak");
            assert!(
                backup_path.exists(),
                "Backup file should be created for corrupted task settings"
            );
            let backup_content = fs::read_to_string(&backup_path).unwrap();
            assert_eq!(backup_content, corrupted_content);
            assert!(manager.get_task_state("task-a", "mode").is_none());
        });
    }

    #[test]
    fn test_persist_task_states_creates_backup_on_corrupt_task_settings() {
        use std::fs;

        with_temp_data_dir(|| {
            let manager = StateManager::new().unwrap();
            manager.initialize().unwrap();

            let task_dir = crate::storage::disk::get_tasks_dir().join("task-a");
            fs::create_dir_all(&task_dir).unwrap();
            let settings_path = task_dir.join("settings.json");
            let corrupted_content = r#"{"mode":"act""#;
            fs::write(&settings_path, corrupted_content).unwrap();

            manager.set_task_state("task-a", "mode", serde_json::Value::String("plan".into()));
            manager.persist().unwrap();

            let backup_path = settings_path.with_extension("json.bak");
            assert!(
                backup_path.exists(),
                "Backup file should be created before overwriting corrupted task settings"
            );
            let backup_content = fs::read_to_string(&backup_path).unwrap();
            assert_eq!(backup_content, corrupted_content);

            let persisted: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
            assert_eq!(persisted["mode"], serde_json::Value::String("plan".into()));
        });
    }
}
