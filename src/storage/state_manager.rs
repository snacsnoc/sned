use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use ulid::Ulid;

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
    // String fields
    ConfigKeyInfo {
        name: "mode",
        key_type: "string",
        description: "Operating mode (act/plan)",
    },
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
        name: "preferred_language",
        key_type: "string",
        description: "Preferred language for responses",
    },
    ConfigKeyInfo {
        name: "telemetry_setting",
        key_type: "string",
        description: "Telemetry setting (unset/enabled/disabled)",
    },
    ConfigKeyInfo {
        name: "default_terminal_profile",
        key_type: "string",
        description: "Default terminal profile",
    },
    ConfigKeyInfo {
        name: "custom_prompt",
        key_type: "string",
        description: "Custom system prompt",
    },
    ConfigKeyInfo {
        name: "worktree_auto_open_path",
        key_type: "string",
        description: "Worktree auto-open path",
    },
    ConfigKeyInfo {
        name: "last_shown_announcement_id",
        key_type: "string",
        description: "Last shown announcement ID",
    },
    ConfigKeyInfo {
        name: "write_prompt_metadata_directory",
        key_type: "string",
        description: "Directory for prompt metadata",
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
        name: "aws_region",
        key_type: "string",
        description: "AWS region",
    },
    ConfigKeyInfo {
        name: "open_telemetry_metrics_exporter",
        key_type: "string",
        description: "OpenTelemetry metrics exporter",
    },
    ConfigKeyInfo {
        name: "open_telemetry_logs_exporter",
        key_type: "string",
        description: "OpenTelemetry logs exporter",
    },
    ConfigKeyInfo {
        name: "open_telemetry_otlp_protocol",
        key_type: "string",
        description: "OpenTelemetry OTLP protocol",
    },
    ConfigKeyInfo {
        name: "open_telemetry_otlp_endpoint",
        key_type: "string",
        description: "OpenTelemetry OTLP endpoint",
    },
    ConfigKeyInfo {
        name: "open_telemetry_otlp_metrics_protocol",
        key_type: "string",
        description: "OpenTelemetry OTLP metrics protocol",
    },
    ConfigKeyInfo {
        name: "open_telemetry_otlp_metrics_endpoint",
        key_type: "string",
        description: "OpenTelemetry OTLP metrics endpoint",
    },
    ConfigKeyInfo {
        name: "open_telemetry_otlp_logs_protocol",
        key_type: "string",
        description: "OpenTelemetry OTLP logs protocol",
    },
    ConfigKeyInfo {
        name: "open_telemetry_otlp_logs_endpoint",
        key_type: "string",
        description: "OpenTelemetry OTLP logs endpoint",
    },
    // Numeric fields
    ConfigKeyInfo {
        name: "shell_integration_timeout",
        key_type: "number",
        description: "Shell integration timeout (ms)",
    },
    ConfigKeyInfo {
        name: "terminal_output_line_limit",
        key_type: "number",
        description: "Terminal output line limit",
    },
    ConfigKeyInfo {
        name: "max_consecutive_mistakes",
        key_type: "number",
        description: "Max consecutive mistakes before intervention",
    },
    ConfigKeyInfo {
        name: "open_telemetry_metric_export_interval",
        key_type: "number",
        description: "OpenTelemetry metric export interval (ms)",
    },
    ConfigKeyInfo {
        name: "open_telemetry_log_batch_size",
        key_type: "number",
        description: "OpenTelemetry log batch size",
    },
    ConfigKeyInfo {
        name: "open_telemetry_log_batch_timeout",
        key_type: "number",
        description: "OpenTelemetry log batch timeout (ms)",
    },
    ConfigKeyInfo {
        name: "open_telemetry_log_max_queue_size",
        key_type: "number",
        description: "OpenTelemetry log max queue size",
    },
    ConfigKeyInfo {
        name: "request_timeout_ms",
        key_type: "number",
        description: "API request timeout (ms)",
    },
    // Boolean fields
    ConfigKeyInfo {
        name: "enable_checkpoints_setting",
        key_type: "boolean",
        description: "Enable checkpoint saves",
    },
    ConfigKeyInfo {
        name: "plan_act_separate_models_setting",
        key_type: "boolean",
        description: "Use separate models for plan/act",
    },
    ConfigKeyInfo {
        name: "strict_plan_mode_enabled",
        key_type: "boolean",
        description: "Enable strict plan mode",
    },
    ConfigKeyInfo {
        name: "hooks_enabled",
        key_type: "boolean",
        description: "Enable hooks",
    },
    ConfigKeyInfo {
        name: "use_auto_condense",
        key_type: "boolean",
        description: "Enable auto-condensing",
    },
    ConfigKeyInfo {
        name: "show_token_usage",
        key_type: "boolean",
        description: "Show token usage in UI",
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
        name: "worktrees_enabled",
        key_type: "boolean",
        description: "Enable worktrees",
    },
    ConfigKeyInfo {
        name: "background_edit_enabled",
        key_type: "boolean",
        description: "Enable background edits",
    },
    ConfigKeyInfo {
        name: "opt_out_of_remote_config",
        key_type: "boolean",
        description: "Opt out of remote config",
    },
    ConfigKeyInfo {
        name: "double_check_completion_enabled",
        key_type: "boolean",
        description: "Enable double-check completion",
    },
    ConfigKeyInfo {
        name: "open_telemetry_enabled",
        key_type: "boolean",
        description: "Enable OpenTelemetry",
    },
    ConfigKeyInfo {
        name: "open_telemetry_otlp_insecure",
        key_type: "boolean",
        description: "Use insecure OTLP connection",
    },
    ConfigKeyInfo {
        name: "write_prompt_metadata_enabled",
        key_type: "boolean",
        description: "Enable prompt metadata writing",
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
    // Persistence fields (original)
    SnedVersion,
    TaskHistory,
    FavoritedModelIds,
    TerminalReuseEnabled,
    IsNewUser,
    Mode,
    SubagentsEnabled,
    GlobalSnedRulesToggles,
    EnableCheckpoints,
    // Config fields (expanded to match VALID_CONFIG_KEYS)
    ActModeApiProvider,
    PlanModeApiProvider,
    ActModeApiModelId,
    PlanModeApiModelId,
    AzureApiVersion,
    PreferredLanguage,
    TelemetrySetting,
    DefaultTerminalProfile,
    CustomPrompt,
    WorktreeAutoOpenPath,
    LastShownAnnouncementId,
    WritePromptMetadataDirectory,
    LiteLlmBaseUrl,
    AnthropicBaseUrl,
    OpenAiBaseUrl,
    OpenRouterBaseUrl,
    GeminiBaseUrl,
    AwsRegion,
    OpenTelemetryMetricsExporter,
    OpenTelemetryLogsExporter,
    OpenTelemetryOtlpProtocol,
    OpenTelemetryOtlpEndpoint,
    OpenTelemetryOtlpMetricsProtocol,
    OpenTelemetryOtlpMetricsEndpoint,
    OpenTelemetryOtlpLogsProtocol,
    OpenTelemetryOtlpLogsEndpoint,
    ShellIntegrationTimeout,
    TerminalOutputLineLimit,
    MaxConsecutiveMistakes,
    OpenTelemetryMetricExportInterval,
    OpenTelemetryLogBatchSize,
    OpenTelemetryLogBatchTimeout,
    OpenTelemetryLogMaxQueueSize,
    RequestTimeoutMs,
    PlanActSeparateModelsSetting,
    StrictPlanModeEnabled,
    HooksEnabled,
    UseAutoCondense,
    ShowTokenUsage,
    SnedWebToolsEnabled,
    WorktreesEnabled,
    BackgroundEditEnabled,
    OptOutOfRemoteConfig,
    DoubleCheckCompletionEnabled,
    OpenTelemetryEnabled,
    OpenTelemetryOtlpInsecure,
    WritePromptMetadataEnabled,
    EnableParallelToolCalling,
}

impl GlobalStateKey {
    /// Get the string value for this key from GlobalState (for CLI config display).
    pub fn get_string_value(&self, state: &GlobalState) -> Option<String> {
        match self {
            GlobalStateKey::Mode => Some(state.mode.clone()),
            GlobalStateKey::ActModeApiProvider => Some(state.act_mode_api_provider.clone()),
            GlobalStateKey::PlanModeApiProvider => Some(state.plan_mode_api_provider.clone()),
            GlobalStateKey::ActModeApiModelId => state.act_mode_api_model_id.clone(),
            GlobalStateKey::PlanModeApiModelId => state.plan_mode_api_model_id.clone(),
            GlobalStateKey::AzureApiVersion => state.azure_api_version.clone(),
            GlobalStateKey::PreferredLanguage => Some(state.preferred_language.clone()),
            GlobalStateKey::TelemetrySetting => Some(state.telemetry_setting.clone()),
            GlobalStateKey::DefaultTerminalProfile => Some(state.default_terminal_profile.clone()),
            GlobalStateKey::CustomPrompt => state.custom_prompt.clone(),
            GlobalStateKey::WorktreeAutoOpenPath => state.worktree_auto_open_path.clone(),
            GlobalStateKey::LastShownAnnouncementId => state.last_shown_announcement_id.clone(),
            GlobalStateKey::WritePromptMetadataDirectory => {
                state.write_prompt_metadata_directory.clone()
            }
            GlobalStateKey::LiteLlmBaseUrl => state.lite_llm_base_url.clone(),
            GlobalStateKey::AnthropicBaseUrl => state.anthropic_base_url.clone(),
            GlobalStateKey::OpenAiBaseUrl => state.open_ai_base_url.clone(),
            GlobalStateKey::OpenRouterBaseUrl => state.open_router_base_url.clone(),
            GlobalStateKey::GeminiBaseUrl => state.gemini_base_url.clone(),
            GlobalStateKey::AwsRegion => state.aws_region.clone(),
            GlobalStateKey::OpenTelemetryMetricsExporter => {
                state.open_telemetry_metrics_exporter.clone()
            }
            GlobalStateKey::OpenTelemetryLogsExporter => state.open_telemetry_logs_exporter.clone(),
            GlobalStateKey::OpenTelemetryOtlpProtocol => {
                Some(state.open_telemetry_otlp_protocol.clone())
            }
            GlobalStateKey::OpenTelemetryOtlpEndpoint => {
                Some(state.open_telemetry_otlp_endpoint.clone())
            }
            GlobalStateKey::OpenTelemetryOtlpMetricsProtocol => {
                state.open_telemetry_otlp_metrics_protocol.clone()
            }
            GlobalStateKey::OpenTelemetryOtlpMetricsEndpoint => {
                state.open_telemetry_otlp_metrics_endpoint.clone()
            }
            GlobalStateKey::OpenTelemetryOtlpLogsProtocol => {
                state.open_telemetry_otlp_logs_protocol.clone()
            }
            GlobalStateKey::OpenTelemetryOtlpLogsEndpoint => {
                state.open_telemetry_otlp_logs_endpoint.clone()
            }
            GlobalStateKey::ShellIntegrationTimeout => {
                Some(state.shell_integration_timeout.to_string())
            }
            GlobalStateKey::TerminalOutputLineLimit => {
                Some(state.terminal_output_line_limit.to_string())
            }
            GlobalStateKey::MaxConsecutiveMistakes => {
                Some(state.max_consecutive_mistakes.to_string())
            }
            GlobalStateKey::OpenTelemetryMetricExportInterval => {
                Some(state.open_telemetry_metric_export_interval.to_string())
            }
            GlobalStateKey::OpenTelemetryLogBatchSize => {
                Some(state.open_telemetry_log_batch_size.to_string())
            }
            GlobalStateKey::OpenTelemetryLogBatchTimeout => {
                Some(state.open_telemetry_log_batch_timeout.to_string())
            }
            GlobalStateKey::OpenTelemetryLogMaxQueueSize => {
                Some(state.open_telemetry_log_max_queue_size.to_string())
            }
            GlobalStateKey::RequestTimeoutMs => state.request_timeout_ms.map(|v| v.to_string()),
            GlobalStateKey::EnableCheckpoints => Some(state.enable_checkpoints_setting.to_string()),
            GlobalStateKey::PlanActSeparateModelsSetting => {
                Some(state.plan_act_separate_models_setting.to_string())
            }
            GlobalStateKey::StrictPlanModeEnabled => {
                Some(state.strict_plan_mode_enabled.to_string())
            }
            GlobalStateKey::HooksEnabled => Some(state.hooks_enabled.to_string()),
            GlobalStateKey::UseAutoCondense => Some(state.use_auto_condense.to_string()),
            GlobalStateKey::ShowTokenUsage => Some(state.show_token_usage.to_string()),
            GlobalStateKey::SubagentsEnabled => Some(state.subagents_enabled.to_string()),
            GlobalStateKey::SnedWebToolsEnabled => Some(state.sned_web_tools_enabled.to_string()),
            GlobalStateKey::WorktreesEnabled => Some(state.worktrees_enabled.to_string()),
            GlobalStateKey::BackgroundEditEnabled => {
                Some(state.background_edit_enabled.to_string())
            }
            GlobalStateKey::OptOutOfRemoteConfig => {
                Some(state.opt_out_of_remote_config.to_string())
            }
            GlobalStateKey::DoubleCheckCompletionEnabled => {
                Some(state.double_check_completion_enabled.to_string())
            }
            GlobalStateKey::OpenTelemetryEnabled => Some(state.open_telemetry_enabled.to_string()),
            GlobalStateKey::OpenTelemetryOtlpInsecure => {
                Some(state.open_telemetry_otlp_insecure.to_string())
            }
            GlobalStateKey::WritePromptMetadataEnabled => {
                Some(state.write_prompt_metadata_enabled.to_string())
            }
            GlobalStateKey::EnableParallelToolCalling => {
                Some(state.enable_parallel_tool_calling.to_string())
            }
            GlobalStateKey::SnedVersion => state.sned_version.clone(),
            GlobalStateKey::TaskHistory => None,
            GlobalStateKey::FavoritedModelIds => None,
            GlobalStateKey::TerminalReuseEnabled => Some(state.terminal_reuse_enabled.to_string()),
            GlobalStateKey::IsNewUser => Some(state.is_new_user.to_string()),
            GlobalStateKey::GlobalSnedRulesToggles => None,
        }
    }

    /// Get the JSON value for this key from GlobalState.
    pub fn get_json_value(&self, state: &GlobalState) -> Option<serde_json::Value> {
        match self {
            GlobalStateKey::SnedVersion => serde_json::to_value(&state.sned_version).ok(),
            GlobalStateKey::TaskHistory => serde_json::to_value(&state.task_history).ok(),
            GlobalStateKey::FavoritedModelIds => {
                serde_json::to_value(&state.favorited_model_ids).ok()
            }
            GlobalStateKey::TerminalReuseEnabled => {
                serde_json::to_value(state.terminal_reuse_enabled).ok()
            }
            GlobalStateKey::IsNewUser => serde_json::to_value(state.is_new_user).ok(),
            GlobalStateKey::Mode => serde_json::to_value(&state.mode).ok(),
            GlobalStateKey::SubagentsEnabled => serde_json::to_value(state.subagents_enabled).ok(),
            GlobalStateKey::GlobalSnedRulesToggles => {
                serde_json::to_value(&state.global_sned_rules_toggles).ok()
            }
            GlobalStateKey::EnableCheckpoints => {
                serde_json::to_value(state.enable_checkpoints_setting).ok()
            }
            GlobalStateKey::ActModeApiProvider => {
                serde_json::to_value(&state.act_mode_api_provider).ok()
            }
            GlobalStateKey::PlanModeApiProvider => {
                serde_json::to_value(&state.plan_mode_api_provider).ok()
            }
            GlobalStateKey::ActModeApiModelId => {
                serde_json::to_value(&state.act_mode_api_model_id).ok()
            }
            GlobalStateKey::PlanModeApiModelId => {
                serde_json::to_value(&state.plan_mode_api_model_id).ok()
            }
            GlobalStateKey::AzureApiVersion => serde_json::to_value(&state.azure_api_version).ok(),
            GlobalStateKey::PreferredLanguage => {
                serde_json::to_value(&state.preferred_language).ok()
            }
            GlobalStateKey::TelemetrySetting => serde_json::to_value(&state.telemetry_setting).ok(),
            GlobalStateKey::DefaultTerminalProfile => {
                serde_json::to_value(&state.default_terminal_profile).ok()
            }
            GlobalStateKey::CustomPrompt => state
                .custom_prompt
                .as_ref()
                .map(|v| serde_json::to_value(v).unwrap()),
            GlobalStateKey::WorktreeAutoOpenPath => state
                .worktree_auto_open_path
                .as_ref()
                .map(|v| serde_json::to_value(v).unwrap()),
            GlobalStateKey::LastShownAnnouncementId => state
                .last_shown_announcement_id
                .as_ref()
                .map(|v| serde_json::to_value(v).unwrap()),
            GlobalStateKey::WritePromptMetadataDirectory => state
                .write_prompt_metadata_directory
                .as_ref()
                .map(|v| serde_json::to_value(v).unwrap()),
            GlobalStateKey::LiteLlmBaseUrl => state
                .lite_llm_base_url
                .as_ref()
                .map(|v| serde_json::to_value(v).unwrap()),
            GlobalStateKey::AnthropicBaseUrl => state
                .anthropic_base_url
                .as_ref()
                .map(|v| serde_json::to_value(v).unwrap()),
            GlobalStateKey::OpenAiBaseUrl => state
                .open_ai_base_url
                .as_ref()
                .map(|v| serde_json::to_value(v).unwrap()),
            GlobalStateKey::OpenRouterBaseUrl => state
                .open_router_base_url
                .as_ref()
                .map(|v| serde_json::to_value(v).unwrap()),
            GlobalStateKey::GeminiBaseUrl => state
                .gemini_base_url
                .as_ref()
                .map(|v| serde_json::to_value(v).unwrap()),
            GlobalStateKey::AwsRegion => state
                .aws_region
                .as_ref()
                .map(|v| serde_json::to_value(v).unwrap()),
            GlobalStateKey::OpenTelemetryMetricsExporter => state
                .open_telemetry_metrics_exporter
                .as_ref()
                .map(|v| serde_json::to_value(v).unwrap()),
            GlobalStateKey::OpenTelemetryLogsExporter => state
                .open_telemetry_logs_exporter
                .as_ref()
                .map(|v| serde_json::to_value(v).unwrap()),
            GlobalStateKey::OpenTelemetryOtlpProtocol => {
                serde_json::to_value(&state.open_telemetry_otlp_protocol).ok()
            }
            GlobalStateKey::OpenTelemetryOtlpEndpoint => {
                serde_json::to_value(&state.open_telemetry_otlp_endpoint).ok()
            }
            GlobalStateKey::OpenTelemetryOtlpMetricsProtocol => state
                .open_telemetry_otlp_metrics_protocol
                .as_ref()
                .map(|v| serde_json::to_value(v).unwrap()),
            GlobalStateKey::OpenTelemetryOtlpMetricsEndpoint => state
                .open_telemetry_otlp_metrics_endpoint
                .as_ref()
                .map(|v| serde_json::to_value(v).unwrap()),
            GlobalStateKey::OpenTelemetryOtlpLogsProtocol => state
                .open_telemetry_otlp_logs_protocol
                .as_ref()
                .map(|v| serde_json::to_value(v).unwrap()),
            GlobalStateKey::OpenTelemetryOtlpLogsEndpoint => state
                .open_telemetry_otlp_logs_endpoint
                .as_ref()
                .map(|v| serde_json::to_value(v).unwrap()),
            GlobalStateKey::ShellIntegrationTimeout => {
                serde_json::to_value(state.shell_integration_timeout).ok()
            }
            GlobalStateKey::TerminalOutputLineLimit => {
                serde_json::to_value(state.terminal_output_line_limit).ok()
            }
            GlobalStateKey::MaxConsecutiveMistakes => {
                serde_json::to_value(state.max_consecutive_mistakes).ok()
            }
            GlobalStateKey::OpenTelemetryMetricExportInterval => {
                serde_json::to_value(state.open_telemetry_metric_export_interval).ok()
            }
            GlobalStateKey::OpenTelemetryLogBatchSize => {
                serde_json::to_value(state.open_telemetry_log_batch_size).ok()
            }
            GlobalStateKey::OpenTelemetryLogBatchTimeout => {
                serde_json::to_value(state.open_telemetry_log_batch_timeout).ok()
            }
            GlobalStateKey::OpenTelemetryLogMaxQueueSize => {
                serde_json::to_value(state.open_telemetry_log_max_queue_size).ok()
            }
            GlobalStateKey::RequestTimeoutMs => state
                .request_timeout_ms
                .as_ref()
                .map(|v| serde_json::to_value(v).unwrap()),
            GlobalStateKey::PlanActSeparateModelsSetting => {
                serde_json::to_value(state.plan_act_separate_models_setting).ok()
            }
            GlobalStateKey::StrictPlanModeEnabled => {
                serde_json::to_value(state.strict_plan_mode_enabled).ok()
            }
            GlobalStateKey::HooksEnabled => serde_json::to_value(state.hooks_enabled).ok(),
            GlobalStateKey::UseAutoCondense => serde_json::to_value(state.use_auto_condense).ok(),
            GlobalStateKey::ShowTokenUsage => serde_json::to_value(state.show_token_usage).ok(),
            GlobalStateKey::SnedWebToolsEnabled => {
                serde_json::to_value(state.sned_web_tools_enabled).ok()
            }
            GlobalStateKey::WorktreesEnabled => serde_json::to_value(state.worktrees_enabled).ok(),
            GlobalStateKey::BackgroundEditEnabled => {
                serde_json::to_value(state.background_edit_enabled).ok()
            }
            GlobalStateKey::OptOutOfRemoteConfig => {
                serde_json::to_value(state.opt_out_of_remote_config).ok()
            }
            GlobalStateKey::DoubleCheckCompletionEnabled => {
                serde_json::to_value(state.double_check_completion_enabled).ok()
            }
            GlobalStateKey::OpenTelemetryEnabled => {
                serde_json::to_value(state.open_telemetry_enabled).ok()
            }
            GlobalStateKey::OpenTelemetryOtlpInsecure => {
                serde_json::to_value(state.open_telemetry_otlp_insecure).ok()
            }
            GlobalStateKey::WritePromptMetadataEnabled => {
                serde_json::to_value(state.write_prompt_metadata_enabled).ok()
            }
            GlobalStateKey::EnableParallelToolCalling => {
                serde_json::to_value(state.enable_parallel_tool_calling).ok()
            }
        }
    }

    /// Set a JSON value on GlobalState for this key.
    pub fn set_json_value(&self, state: &mut GlobalState, value: serde_json::Value) {
        match self {
            GlobalStateKey::SnedVersion => state.sned_version = serde_json::from_value(value).ok(),
            GlobalStateKey::TaskHistory => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.task_history = v;
                }
            }
            GlobalStateKey::FavoritedModelIds => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.favorited_model_ids = v;
                }
            }
            GlobalStateKey::TerminalReuseEnabled => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.terminal_reuse_enabled = v;
                }
            }
            GlobalStateKey::IsNewUser => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.is_new_user = v;
                }
            }
            GlobalStateKey::Mode => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.mode = v;
                }
            }
            GlobalStateKey::SubagentsEnabled => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.subagents_enabled = v;
                }
            }
            GlobalStateKey::GlobalSnedRulesToggles => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.global_sned_rules_toggles = v;
                }
            }
            GlobalStateKey::EnableCheckpoints => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.enable_checkpoints_setting = v;
                }
            }
            GlobalStateKey::ActModeApiProvider => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.act_mode_api_provider = v;
                }
            }
            GlobalStateKey::PlanModeApiProvider => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.plan_mode_api_provider = v;
                }
            }
            GlobalStateKey::ActModeApiModelId => {
                state.act_mode_api_model_id = serde_json::from_value(value).ok()
            }
            GlobalStateKey::PlanModeApiModelId => {
                state.plan_mode_api_model_id = serde_json::from_value(value).ok()
            }
            GlobalStateKey::AzureApiVersion => {
                state.azure_api_version = serde_json::from_value(value).ok()
            }
            GlobalStateKey::PreferredLanguage => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.preferred_language = v;
                }
            }
            GlobalStateKey::TelemetrySetting => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.telemetry_setting = v;
                }
            }
            GlobalStateKey::DefaultTerminalProfile => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.default_terminal_profile = v;
                }
            }
            GlobalStateKey::CustomPrompt => {
                state.custom_prompt = serde_json::from_value(value).ok()
            }
            GlobalStateKey::WorktreeAutoOpenPath => {
                state.worktree_auto_open_path = serde_json::from_value(value).ok()
            }
            GlobalStateKey::LastShownAnnouncementId => {
                state.last_shown_announcement_id = serde_json::from_value(value).ok()
            }
            GlobalStateKey::WritePromptMetadataDirectory => {
                state.write_prompt_metadata_directory = serde_json::from_value(value).ok()
            }
            GlobalStateKey::LiteLlmBaseUrl => {
                state.lite_llm_base_url = serde_json::from_value(value).ok()
            }
            GlobalStateKey::AnthropicBaseUrl => {
                state.anthropic_base_url = serde_json::from_value(value).ok()
            }
            GlobalStateKey::OpenAiBaseUrl => {
                state.open_ai_base_url = serde_json::from_value(value).ok()
            }
            GlobalStateKey::OpenRouterBaseUrl => {
                state.open_router_base_url = serde_json::from_value(value).ok()
            }
            GlobalStateKey::GeminiBaseUrl => {
                state.gemini_base_url = serde_json::from_value(value).ok()
            }
            GlobalStateKey::AwsRegion => state.aws_region = serde_json::from_value(value).ok(),
            GlobalStateKey::OpenTelemetryMetricsExporter => {
                state.open_telemetry_metrics_exporter = serde_json::from_value(value).ok()
            }
            GlobalStateKey::OpenTelemetryLogsExporter => {
                state.open_telemetry_logs_exporter = serde_json::from_value(value).ok()
            }
            GlobalStateKey::OpenTelemetryOtlpProtocol => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.open_telemetry_otlp_protocol = v;
                }
            }
            GlobalStateKey::OpenTelemetryOtlpEndpoint => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.open_telemetry_otlp_endpoint = v;
                }
            }
            GlobalStateKey::OpenTelemetryOtlpMetricsProtocol => {
                state.open_telemetry_otlp_metrics_protocol = serde_json::from_value(value).ok()
            }
            GlobalStateKey::OpenTelemetryOtlpMetricsEndpoint => {
                state.open_telemetry_otlp_metrics_endpoint = serde_json::from_value(value).ok()
            }
            GlobalStateKey::OpenTelemetryOtlpLogsProtocol => {
                state.open_telemetry_otlp_logs_protocol = serde_json::from_value(value).ok()
            }
            GlobalStateKey::OpenTelemetryOtlpLogsEndpoint => {
                state.open_telemetry_otlp_logs_endpoint = serde_json::from_value(value).ok()
            }
            GlobalStateKey::ShellIntegrationTimeout => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.shell_integration_timeout = v;
                }
            }
            GlobalStateKey::TerminalOutputLineLimit => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.terminal_output_line_limit = v;
                }
            }
            GlobalStateKey::MaxConsecutiveMistakes => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.max_consecutive_mistakes = v;
                }
            }
            GlobalStateKey::OpenTelemetryMetricExportInterval => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.open_telemetry_metric_export_interval = v;
                }
            }
            GlobalStateKey::OpenTelemetryLogBatchSize => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.open_telemetry_log_batch_size = v;
                }
            }
            GlobalStateKey::OpenTelemetryLogBatchTimeout => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.open_telemetry_log_batch_timeout = v;
                }
            }
            GlobalStateKey::OpenTelemetryLogMaxQueueSize => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.open_telemetry_log_max_queue_size = v;
                }
            }
            GlobalStateKey::RequestTimeoutMs => {
                state.request_timeout_ms = serde_json::from_value(value).ok()
            }
            GlobalStateKey::PlanActSeparateModelsSetting => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.plan_act_separate_models_setting = v;
                }
            }
            GlobalStateKey::StrictPlanModeEnabled => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.strict_plan_mode_enabled = v;
                }
            }
            GlobalStateKey::HooksEnabled => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.hooks_enabled = v;
                }
            }
            GlobalStateKey::UseAutoCondense => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.use_auto_condense = v;
                }
            }
            GlobalStateKey::ShowTokenUsage => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.show_token_usage = v;
                }
            }
            GlobalStateKey::SnedWebToolsEnabled => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.sned_web_tools_enabled = v;
                }
            }
            GlobalStateKey::WorktreesEnabled => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.worktrees_enabled = v;
                }
            }
            GlobalStateKey::BackgroundEditEnabled => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.background_edit_enabled = v;
                }
            }
            GlobalStateKey::OptOutOfRemoteConfig => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.opt_out_of_remote_config = v;
                }
            }
            GlobalStateKey::DoubleCheckCompletionEnabled => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.double_check_completion_enabled = v;
                }
            }
            GlobalStateKey::OpenTelemetryEnabled => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.open_telemetry_enabled = v;
                }
            }
            GlobalStateKey::OpenTelemetryOtlpInsecure => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.open_telemetry_otlp_insecure = v;
                }
            }
            GlobalStateKey::WritePromptMetadataEnabled => {
                if let Ok(v) = serde_json::from_value(value) {
                    state.write_prompt_metadata_enabled = v;
                }
            }
            GlobalStateKey::EnableParallelToolCalling => {
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
        // Support both camelCase (original) and snake_case (VALID_CONFIG_KEYS) names
        match s {
            // snedVersion
            "snedVersion" | "sned_version" => Ok(GlobalStateKey::SnedVersion),
            // taskHistory
            "taskHistory" | "task_history" => Ok(GlobalStateKey::TaskHistory),
            // favoritedModelIds
            "favoritedModelIds" | "favorited_model_ids" => Ok(GlobalStateKey::FavoritedModelIds),
            // terminalReuseEnabled
            "terminalReuseEnabled" | "terminal_reuse_enabled" => {
                Ok(GlobalStateKey::TerminalReuseEnabled)
            }
            // isNewUser
            "isNewUser" | "is_new_user" => Ok(GlobalStateKey::IsNewUser),
            // mode
            "mode" => Ok(GlobalStateKey::Mode),
            // subagentsEnabled
            "subagentsEnabled" | "subagents_enabled" => Ok(GlobalStateKey::SubagentsEnabled),
            // globalSnedRulesToggles
            "globalSnedRulesToggles" | "global_sned_rules_toggles" => {
                Ok(GlobalStateKey::GlobalSnedRulesToggles)
            }
            // enableCheckpoints
            "enableCheckpoints" | "enable_checkpoints" => Ok(GlobalStateKey::EnableCheckpoints),
            // actModeApiProvider
            "actModeApiProvider" | "act_mode_api_provider" => {
                Ok(GlobalStateKey::ActModeApiProvider)
            }
            // planModeApiProvider
            "planModeApiProvider" | "plan_mode_api_provider" => {
                Ok(GlobalStateKey::PlanModeApiProvider)
            }
            // actModeApiModelId
            "actModeApiModelId" | "act_mode_api_model_id" => Ok(GlobalStateKey::ActModeApiModelId),
            // planModeApiModelId
            "planModeApiModelId" | "plan_mode_api_model_id" => {
                Ok(GlobalStateKey::PlanModeApiModelId)
            }
            // azureApiVersion
            "azureApiVersion" | "azure_api_version" => Ok(GlobalStateKey::AzureApiVersion),
            // preferredLanguage
            "preferredLanguage" | "preferred_language" => Ok(GlobalStateKey::PreferredLanguage),
            // telemetrySetting
            "telemetrySetting" | "telemetry_setting" => Ok(GlobalStateKey::TelemetrySetting),
            // defaultTerminalProfile
            "defaultTerminalProfile" | "default_terminal_profile" => {
                Ok(GlobalStateKey::DefaultTerminalProfile)
            }
            // customPrompt
            "customPrompt" | "custom_prompt" => Ok(GlobalStateKey::CustomPrompt),
            // worktreeAutoOpenPath
            "worktreeAutoOpenPath" | "worktree_auto_open_path" => {
                Ok(GlobalStateKey::WorktreeAutoOpenPath)
            }
            // lastShownAnnouncementId
            "lastShownAnnouncementId" | "last_shown_announcement_id" => {
                Ok(GlobalStateKey::LastShownAnnouncementId)
            }
            // writePromptMetadataDirectory
            "writePromptMetadataDirectory" | "write_prompt_metadata_directory" => {
                Ok(GlobalStateKey::WritePromptMetadataDirectory)
            }
            // liteLlmBaseUrl
            "liteLlmBaseUrl" | "lite_llm_base_url" => Ok(GlobalStateKey::LiteLlmBaseUrl),
            // anthropicBaseUrl
            "anthropicBaseUrl" | "anthropic_base_url" => Ok(GlobalStateKey::AnthropicBaseUrl),
            // openAiBaseUrl
            "openAiBaseUrl" | "open_ai_base_url" => Ok(GlobalStateKey::OpenAiBaseUrl),
            // openRouterBaseUrl
            "openRouterBaseUrl" | "open_router_base_url" => Ok(GlobalStateKey::OpenRouterBaseUrl),
            // geminiBaseUrl
            "geminiBaseUrl" | "gemini_base_url" => Ok(GlobalStateKey::GeminiBaseUrl),
            // awsRegion
            "awsRegion" | "aws_region" => Ok(GlobalStateKey::AwsRegion),
            // openTelemetryMetricsExporter
            "openTelemetryMetricsExporter" | "open_telemetry_metrics_exporter" => {
                Ok(GlobalStateKey::OpenTelemetryMetricsExporter)
            }
            // openTelemetryLogsExporter
            "openTelemetryLogsExporter" | "open_telemetry_logs_exporter" => {
                Ok(GlobalStateKey::OpenTelemetryLogsExporter)
            }
            // openTelemetryOtlpProtocol
            "openTelemetryOtlpProtocol" | "open_telemetry_otlp_protocol" => {
                Ok(GlobalStateKey::OpenTelemetryOtlpProtocol)
            }
            // openTelemetryOtlpEndpoint
            "openTelemetryOtlpEndpoint" | "open_telemetry_otlp_endpoint" => {
                Ok(GlobalStateKey::OpenTelemetryOtlpEndpoint)
            }
            // openTelemetryOtlpMetricsProtocol
            "openTelemetryOtlpMetricsProtocol" | "open_telemetry_otlp_metrics_protocol" => {
                Ok(GlobalStateKey::OpenTelemetryOtlpMetricsProtocol)
            }
            // openTelemetryOtlpMetricsEndpoint
            "openTelemetryOtlpMetricsEndpoint" | "open_telemetry_otlp_metrics_endpoint" => {
                Ok(GlobalStateKey::OpenTelemetryOtlpMetricsEndpoint)
            }
            // openTelemetryOtlpLogsProtocol
            "openTelemetryOtlpLogsProtocol" | "open_telemetry_otlp_logs_protocol" => {
                Ok(GlobalStateKey::OpenTelemetryOtlpLogsProtocol)
            }
            // openTelemetryOtlpLogsEndpoint
            "openTelemetryOtlpLogsEndpoint" | "open_telemetry_otlp_logs_endpoint" => {
                Ok(GlobalStateKey::OpenTelemetryOtlpLogsEndpoint)
            }
            // shellIntegrationTimeout
            "shellIntegrationTimeout" | "shell_integration_timeout" => {
                Ok(GlobalStateKey::ShellIntegrationTimeout)
            }
            // terminalOutputLineLimit
            "terminalOutputLineLimit" | "terminal_output_line_limit" => {
                Ok(GlobalStateKey::TerminalOutputLineLimit)
            }
            // maxConsecutiveMistakes
            "maxConsecutiveMistakes" | "max_consecutive_mistakes" => {
                Ok(GlobalStateKey::MaxConsecutiveMistakes)
            }
            // openTelemetryMetricExportInterval
            "openTelemetryMetricExportInterval" | "open_telemetry_metric_export_interval" => {
                Ok(GlobalStateKey::OpenTelemetryMetricExportInterval)
            }
            // openTelemetryLogBatchSize
            "openTelemetryLogBatchSize" | "open_telemetry_log_batch_size" => {
                Ok(GlobalStateKey::OpenTelemetryLogBatchSize)
            }
            // openTelemetryLogBatchTimeout
            "openTelemetryLogBatchTimeout" | "open_telemetry_log_batch_timeout" => {
                Ok(GlobalStateKey::OpenTelemetryLogBatchTimeout)
            }
            // openTelemetryLogMaxQueueSize
            "openTelemetryLogMaxQueueSize" | "open_telemetry_log_max_queue_size" => {
                Ok(GlobalStateKey::OpenTelemetryLogMaxQueueSize)
            }
            // requestTimeoutMs
            "requestTimeoutMs" | "request_timeout_ms" => Ok(GlobalStateKey::RequestTimeoutMs),
            // planActSeparateModelsSetting
            "planActSeparateModelsSetting" | "plan_act_separate_models_setting" => {
                Ok(GlobalStateKey::PlanActSeparateModelsSetting)
            }
            // strictPlanModeEnabled
            "strictPlanModeEnabled" | "strict_plan_mode_enabled" => {
                Ok(GlobalStateKey::StrictPlanModeEnabled)
            }
            // hooksEnabled
            "hooksEnabled" | "hooks_enabled" => Ok(GlobalStateKey::HooksEnabled),
            // useAutoCondense
            "useAutoCondense" | "use_auto_condense" => Ok(GlobalStateKey::UseAutoCondense),
            // showTokenUsage
            "showTokenUsage" | "show_token_usage" => Ok(GlobalStateKey::ShowTokenUsage),
            // snedWebToolsEnabled
            "snedWebToolsEnabled" | "sned_web_tools_enabled" => {
                Ok(GlobalStateKey::SnedWebToolsEnabled)
            }
            // worktreesEnabled
            "worktreesEnabled" | "worktrees_enabled" => Ok(GlobalStateKey::WorktreesEnabled),
            // backgroundEditEnabled
            "backgroundEditEnabled" | "background_edit_enabled" => {
                Ok(GlobalStateKey::BackgroundEditEnabled)
            }
            // optOutOfRemoteConfig
            "optOutOfRemoteConfig" | "opt_out_of_remote_config" => {
                Ok(GlobalStateKey::OptOutOfRemoteConfig)
            }
            // doubleCheckCompletionEnabled
            "doubleCheckCompletionEnabled" | "double_check_completion_enabled" => {
                Ok(GlobalStateKey::DoubleCheckCompletionEnabled)
            }
            // openTelemetryEnabled
            "openTelemetryEnabled" | "open_telemetry_enabled" => {
                Ok(GlobalStateKey::OpenTelemetryEnabled)
            }
            // openTelemetryOtlpInsecure
            "openTelemetryOtlpInsecure" | "open_telemetry_otlp_insecure" => {
                Ok(GlobalStateKey::OpenTelemetryOtlpInsecure)
            }
            // writePromptMetadataEnabled
            "writePromptMetadataEnabled" | "write_prompt_metadata_enabled" => {
                Ok(GlobalStateKey::WritePromptMetadataEnabled)
            }
            // enableParallelToolCalling
            "enableParallelToolCalling" | "enable_parallel_tool_calling" => {
                Ok(GlobalStateKey::EnableParallelToolCalling)
            }
            _ => Err(()),
        }
    }
}

impl std::fmt::Display for GlobalStateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GlobalStateKey::SnedVersion => write!(f, "snedVersion"),
            GlobalStateKey::TaskHistory => write!(f, "taskHistory"),
            GlobalStateKey::FavoritedModelIds => write!(f, "favoritedModelIds"),
            GlobalStateKey::TerminalReuseEnabled => write!(f, "terminalReuseEnabled"),
            GlobalStateKey::IsNewUser => write!(f, "isNewUser"),
            GlobalStateKey::Mode => write!(f, "mode"),
            GlobalStateKey::SubagentsEnabled => write!(f, "subagentsEnabled"),
            GlobalStateKey::GlobalSnedRulesToggles => write!(f, "globalSnedRulesToggles"),
            GlobalStateKey::EnableCheckpoints => write!(f, "enableCheckpoints"),
            GlobalStateKey::ActModeApiProvider => write!(f, "actModeApiProvider"),
            GlobalStateKey::PlanModeApiProvider => write!(f, "planModeApiProvider"),
            GlobalStateKey::ActModeApiModelId => write!(f, "actModeApiModelId"),
            GlobalStateKey::PlanModeApiModelId => write!(f, "planModeApiModelId"),
            GlobalStateKey::AzureApiVersion => write!(f, "azureApiVersion"),
            GlobalStateKey::PreferredLanguage => write!(f, "preferredLanguage"),
            GlobalStateKey::TelemetrySetting => write!(f, "telemetrySetting"),
            GlobalStateKey::DefaultTerminalProfile => write!(f, "defaultTerminalProfile"),
            GlobalStateKey::CustomPrompt => write!(f, "customPrompt"),
            GlobalStateKey::WorktreeAutoOpenPath => write!(f, "worktreeAutoOpenPath"),
            GlobalStateKey::LastShownAnnouncementId => write!(f, "lastShownAnnouncementId"),
            GlobalStateKey::WritePromptMetadataDirectory => {
                write!(f, "writePromptMetadataDirectory")
            }
            GlobalStateKey::LiteLlmBaseUrl => write!(f, "liteLlmBaseUrl"),
            GlobalStateKey::AnthropicBaseUrl => write!(f, "anthropicBaseUrl"),
            GlobalStateKey::OpenAiBaseUrl => write!(f, "openAiBaseUrl"),
            GlobalStateKey::OpenRouterBaseUrl => write!(f, "openRouterBaseUrl"),
            GlobalStateKey::GeminiBaseUrl => write!(f, "geminiBaseUrl"),
            GlobalStateKey::AwsRegion => write!(f, "awsRegion"),
            GlobalStateKey::OpenTelemetryMetricsExporter => {
                write!(f, "openTelemetryMetricsExporter")
            }
            GlobalStateKey::OpenTelemetryLogsExporter => write!(f, "openTelemetryLogsExporter"),
            GlobalStateKey::OpenTelemetryOtlpProtocol => write!(f, "openTelemetryOtlpProtocol"),
            GlobalStateKey::OpenTelemetryOtlpEndpoint => write!(f, "openTelemetryOtlpEndpoint"),
            GlobalStateKey::OpenTelemetryOtlpMetricsProtocol => {
                write!(f, "openTelemetryOtlpMetricsProtocol")
            }
            GlobalStateKey::OpenTelemetryOtlpMetricsEndpoint => {
                write!(f, "openTelemetryOtlpMetricsEndpoint")
            }
            GlobalStateKey::OpenTelemetryOtlpLogsProtocol => {
                write!(f, "openTelemetryOtlpLogsProtocol")
            }
            GlobalStateKey::OpenTelemetryOtlpLogsEndpoint => {
                write!(f, "openTelemetryOtlpLogsEndpoint")
            }
            GlobalStateKey::ShellIntegrationTimeout => write!(f, "shellIntegrationTimeout"),
            GlobalStateKey::TerminalOutputLineLimit => write!(f, "terminalOutputLineLimit"),
            GlobalStateKey::MaxConsecutiveMistakes => write!(f, "maxConsecutiveMistakes"),
            GlobalStateKey::OpenTelemetryMetricExportInterval => {
                write!(f, "openTelemetryMetricExportInterval")
            }
            GlobalStateKey::OpenTelemetryLogBatchSize => write!(f, "openTelemetryLogBatchSize"),
            GlobalStateKey::OpenTelemetryLogBatchTimeout => {
                write!(f, "openTelemetryLogBatchTimeout")
            }
            GlobalStateKey::OpenTelemetryLogMaxQueueSize => {
                write!(f, "openTelemetryLogMaxQueueSize")
            }
            GlobalStateKey::RequestTimeoutMs => write!(f, "requestTimeoutMs"),
            GlobalStateKey::PlanActSeparateModelsSetting => {
                write!(f, "planActSeparateModelsSetting")
            }
            GlobalStateKey::StrictPlanModeEnabled => write!(f, "strictPlanModeEnabled"),
            GlobalStateKey::HooksEnabled => write!(f, "hooksEnabled"),
            GlobalStateKey::UseAutoCondense => write!(f, "useAutoCondense"),
            GlobalStateKey::ShowTokenUsage => write!(f, "showTokenUsage"),
            GlobalStateKey::SnedWebToolsEnabled => write!(f, "snedWebToolsEnabled"),
            GlobalStateKey::WorktreesEnabled => write!(f, "worktreesEnabled"),
            GlobalStateKey::BackgroundEditEnabled => write!(f, "backgroundEditEnabled"),
            GlobalStateKey::OptOutOfRemoteConfig => write!(f, "optOutOfRemoteConfig"),
            GlobalStateKey::DoubleCheckCompletionEnabled => {
                write!(f, "doubleCheckCompletionEnabled")
            }
            GlobalStateKey::OpenTelemetryEnabled => write!(f, "openTelemetryEnabled"),
            GlobalStateKey::OpenTelemetryOtlpInsecure => write!(f, "openTelemetryOtlpInsecure"),
            GlobalStateKey::WritePromptMetadataEnabled => write!(f, "writePromptMetadataEnabled"),
            GlobalStateKey::EnableParallelToolCalling => write!(f, "enableParallelToolCalling"),
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
            secrets_store,
            state_dir,
        })
    }

    /// Initialize the state manager from disk.
    /// Loads global state, task history, secrets, and workspace state.
    /// Generates and persists a machine ID if not already present.
    /// Cleans up orphaned atomic write temp files older than 24 hours.
    pub fn initialize(&self) -> io::Result<()> {
        // Clean up orphaned temp files from crashed atomic writes
        let settings_dir = self.state_dir.join("..").join("settings");
        let _ = crate::storage::disk::cleanup_orphaned_temp_files(
            &settings_dir,
            std::time::Duration::from_secs(86400), // 24 hours
        );

        // Load global state
        let global_state = self.load_global_state()?;
        *self.global_state.write().unwrap_or_else(|e| e.into_inner()) = global_state;

        // Generate machine ID if not present
        let mut needs_persist = false;
        {
            let mut state = self.global_state.write().unwrap_or_else(|e| e.into_inner());
            if state.sned_generated_machine_id.is_none() {
                let machine_id = Ulid::new().to_string();
                state.sned_generated_machine_id = Some(machine_id);
                needs_persist = true;
            }
        }

        // Persist immediately if we generated a new machine ID
        if needs_persist {
            let settings_dir = self.state_dir.join("..").join("settings");
            fs::create_dir_all(&settings_dir)?;
            let state = self.global_state.read().unwrap_or_else(|e| e.into_inner());
            let data = serde_json::to_string_pretty(&*state)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let file_path = settings_dir.join("global_settings.json");
            crate::storage::disk::atomic_write_file(&file_path, &data)?;
        }

        // Load secrets
        let secrets = self.secrets_store.load();
        *self.secrets.write().unwrap_or_else(|e| e.into_inner()) = secrets;

        // Load workspace state (if exists)
        if let Ok(workspace_state) = self.load_workspace_state() {
            *self
                .workspace_state
                .write()
                .unwrap_or_else(|e| e.into_inner()) = workspace_state;
        }

        // Load task states from disk (SM4 fix)
        let tasks_dir = self.state_dir.join("..").join("tasks");
        if tasks_dir.exists() {
            let mut task_states = self.task_state.write().unwrap_or_else(|e| e.into_inner());
            let mut pending_task_states = self
                .pending_task_states
                .lock()
                .unwrap_or_else(|e| e.into_inner());

            if let Ok(entries) = fs::read_dir(&tasks_dir) {
                for entry in entries.flatten() {
                    let task_dir = entry.path();
                    if task_dir.is_dir()
                        && let Some(task_id) = task_dir.file_name().and_then(|n| n.to_str())
                    {
                        let settings_path = task_dir.join("settings.json");
                        if settings_path.exists()
                            && let Ok(contents) = fs::read_to_string(&settings_path)
                            && let Ok(parsed) = serde_json::from_str::<
                                serde_json::Map<String, serde_json::Value>,
                            >(&contents)
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
        }

        Ok(())
    }

    /// Get the distinct ID for telemetry/hooks.
    /// Reads from `sned_generated_machine_id` field.
    /// Falls back to "anonymous" if not set.
    pub fn get_distinct_id(&self) -> String {
        let state = self.global_state.read().unwrap_or_else(|e| e.into_inner());
        state
            .sned_generated_machine_id
            .clone()
            .unwrap_or_else(|| "anonymous".to_string())
    }

    // ==================== Task History ====================

    /// Get task history from cache
    pub fn get_task_history(&self) -> Vec<HistoryItem> {
        self.global_state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .task_history
            .clone()
    }

    /// Set task history in cache and mark for persistence
    pub fn set_task_history(&self, history: Vec<HistoryItem>) {
        self.global_state
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .task_history = history;
        self.mark_global_key_pending("taskHistory".to_string());
    }

    /// Add a task to history (or update existing)
    pub fn add_task_to_history(&self, item: HistoryItem) {
        let mut state = self.global_state.write().unwrap_or_else(|e| e.into_inner());

        state.task_history.retain(|h| h.id != item.id);
        state.task_history.push(item);
        state.task_history.sort_by_key(|b| std::cmp::Reverse(b.ts));

        drop(state);
        self.mark_global_key_pending("taskHistory".to_string());
    }

    /// Remove a task from history
    pub fn remove_task_from_history(&self, task_id: &str) {
        let mut state = self.global_state.write().unwrap_or_else(|e| e.into_inner());
        state.task_history.retain(|h| h.id != task_id);
        drop(state);
        self.mark_global_key_pending("taskHistory".to_string());
    }

    /// Find a task in history by ID
    pub fn find_task_in_history(&self, task_id: &str) -> Option<HistoryItem> {
        self.global_state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .task_history
            .iter()
            .find(|h| h.id == task_id)
            .cloned()
    }

    /// Get the most recent task for a workspace
    pub fn get_most_recent_task_for_workspace(&self, workspace_path: &str) -> Option<HistoryItem> {
        self.global_state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .task_history
            .iter()
            .filter(|h| {
                // Check if workspace path matches
                h.workspace_root_path
                    .as_ref()
                    .map(|p| p == workspace_path)
                    .unwrap_or(false)
            })
            .max_by_key(|h| h.ts)
            .cloned()
    }

    // ==================== Global State ====================

    /// Get a global state key (typed enum version)
    pub fn get_global_state_key<T>(&self, key: GlobalStateKey) -> Option<T>
    where
        T: Clone + for<'de> Deserialize<'de>,
    {
        let state = self.global_state.read().unwrap_or_else(|e| e.into_inner());
        let json_value = key.get_json_value(&state)?;
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
        let state = self.global_state.read().unwrap_or_else(|e| e.into_inner());
        enum_key.get_string_value(&state)
    }

    /// Set a global state key (typed enum version)
    pub fn set_global_state_key(&self, key: GlobalStateKey, value: serde_json::Value) {
        {
            let mut state = self.global_state.write().unwrap_or_else(|e| e.into_inner());
            key.set_json_value(&mut state, value);
        }
        self.mark_global_key_pending(key.to_string());
    }

    /// Set a global state key by string (deprecated, use enum version)
    pub fn set_global_state_key_str(&self, key: &str, value: serde_json::Value) {
        if let Ok(parsed) = key.parse::<GlobalStateKey>() {
            self.set_global_state_key(parsed, value);
        }
    }

    /// Set a string-backed global config field by its JSON field name.
    pub fn set_global_state_string_field(
        &self,
        key: &str,
        value: String,
    ) -> Result<(), ConfigFieldError> {
        // First, check if the key is valid
        let key_info = VALID_CONFIG_KEYS.iter().find(|k| k.name == key);
        let key_info = match key_info {
            Some(info) => info,
            None => return Err(ConfigFieldError::UnsupportedField(key.to_string())),
        };

        // Validate value type matches expected type
        match key_info.key_type {
            "number" if value.parse::<i32>().is_err() => {
                return Err(ConfigFieldError::InvalidValue(
                    key.to_string(),
                    "number".to_string(),
                    value.clone(),
                ));
            }
            "boolean" if !matches!(value.to_lowercase().as_str(), "true" | "false" | "1" | "0") => {
                return Err(ConfigFieldError::InvalidValue(
                    key.to_string(),
                    "boolean".to_string(),
                    value.clone(),
                ));
            }
            "number" | "boolean" => {}
            _ => {} // String fields accept any value
        }

        let mut state = self.global_state.write().unwrap_or_else(|e| e.into_inner());
        let handled = match key {
            // String fields
            "mode" => {
                state.mode = value;
                true
            }
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
            "preferred_language" => {
                state.preferred_language = value;
                true
            }
            "telemetry_setting" => {
                state.telemetry_setting = value;
                true
            }
            "default_terminal_profile" => {
                state.default_terminal_profile = value;
                true
            }
            "custom_prompt" => {
                state.custom_prompt = Some(value);
                true
            }
            "worktree_auto_open_path" => {
                state.worktree_auto_open_path = Some(value);
                true
            }
            "last_shown_announcement_id" => {
                state.last_shown_announcement_id = Some(value);
                true
            }
            "write_prompt_metadata_directory" => {
                state.write_prompt_metadata_directory = Some(value);
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
            "aws_region" => {
                state.aws_region = Some(value);
                true
            }
            "open_telemetry_metrics_exporter" => {
                state.open_telemetry_metrics_exporter = Some(value);
                true
            }
            "open_telemetry_logs_exporter" => {
                state.open_telemetry_logs_exporter = Some(value);
                true
            }
            "open_telemetry_otlp_protocol" => {
                state.open_telemetry_otlp_protocol = value;
                true
            }
            "open_telemetry_otlp_endpoint" => {
                state.open_telemetry_otlp_endpoint = value;
                true
            }
            "open_telemetry_otlp_metrics_protocol" => {
                state.open_telemetry_otlp_metrics_protocol = Some(value);
                true
            }
            "open_telemetry_otlp_metrics_endpoint" => {
                state.open_telemetry_otlp_metrics_endpoint = Some(value);
                true
            }
            "open_telemetry_otlp_logs_protocol" => {
                state.open_telemetry_otlp_logs_protocol = Some(value);
                true
            }
            "open_telemetry_otlp_logs_endpoint" => {
                state.open_telemetry_otlp_logs_endpoint = Some(value);
                true
            }
            // Numeric fields
            "shell_integration_timeout" => {
                state.shell_integration_timeout = value.parse::<i32>().unwrap_or(4000);
                true
            }
            "terminal_output_line_limit" => {
                state.terminal_output_line_limit = value.parse::<i32>().unwrap_or(500);
                true
            }
            "max_consecutive_mistakes" => {
                state.max_consecutive_mistakes = value.parse::<i32>().unwrap_or(5);
                true
            }
            "open_telemetry_metric_export_interval" => {
                state.open_telemetry_metric_export_interval = value.parse::<i32>().unwrap_or(60);
                true
            }
            "open_telemetry_log_batch_size" => {
                state.open_telemetry_log_batch_size = value.parse::<i32>().unwrap_or(512);
                true
            }
            "open_telemetry_log_batch_timeout" => {
                state.open_telemetry_log_batch_timeout = value.parse::<i32>().unwrap_or(5000);
                true
            }
            "open_telemetry_log_max_queue_size" => {
                state.open_telemetry_log_max_queue_size = value.parse::<i32>().unwrap_or(2048);
                true
            }
            "request_timeout_ms" => {
                state.request_timeout_ms = value.parse::<i32>().ok();
                true
            }
            // Boolean fields
            "enable_checkpoints_setting" => {
                state.enable_checkpoints_setting =
                    matches!(value.to_lowercase().as_str(), "true" | "1");
                true
            }
            "plan_act_separate_models_setting" => {
                state.plan_act_separate_models_setting =
                    matches!(value.to_lowercase().as_str(), "true" | "1");
                true
            }
            "strict_plan_mode_enabled" => {
                state.strict_plan_mode_enabled =
                    matches!(value.to_lowercase().as_str(), "true" | "1");
                true
            }
            "hooks_enabled" => {
                state.hooks_enabled = matches!(value.to_lowercase().as_str(), "true" | "1");
                true
            }
            "use_auto_condense" => {
                state.use_auto_condense = matches!(value.to_lowercase().as_str(), "true" | "1");
                true
            }
            "show_token_usage" => {
                state.show_token_usage = matches!(value.to_lowercase().as_str(), "true" | "1");
                true
            }
            "subagents_enabled" => {
                state.subagents_enabled = matches!(value.to_lowercase().as_str(), "true" | "1");
                true
            }
            "sned_web_tools_enabled" => {
                state.sned_web_tools_enabled =
                    matches!(value.to_lowercase().as_str(), "true" | "1");
                true
            }
            "worktrees_enabled" => {
                state.worktrees_enabled = matches!(value.to_lowercase().as_str(), "true" | "1");
                true
            }
            "background_edit_enabled" => {
                state.background_edit_enabled =
                    matches!(value.to_lowercase().as_str(), "true" | "1");
                true
            }
            "opt_out_of_remote_config" => {
                state.opt_out_of_remote_config =
                    matches!(value.to_lowercase().as_str(), "true" | "1");
                true
            }
            "double_check_completion_enabled" => {
                state.double_check_completion_enabled =
                    matches!(value.to_lowercase().as_str(), "true" | "1");
                true
            }
            "open_telemetry_enabled" => {
                state.open_telemetry_enabled =
                    matches!(value.to_lowercase().as_str(), "true" | "1");
                true
            }
            "open_telemetry_otlp_insecure" => {
                state.open_telemetry_otlp_insecure =
                    matches!(value.to_lowercase().as_str(), "true" | "1");
                true
            }
            "write_prompt_metadata_enabled" => {
                state.write_prompt_metadata_enabled =
                    matches!(value.to_lowercase().as_str(), "true" | "1");
                true
            }
            "enable_parallel_tool_calling" => {
                state.enable_parallel_tool_calling =
                    matches!(value.to_lowercase().as_str(), "true" | "1");
                true
            }
            _ => false,
        };

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
            .unwrap_or_else(|e| e.into_inner())
            .auto_approve_patterns
            .clone()
    }

    /// Get task state for a specific task
    pub fn get_task_state(&self, task_id: &str, key: &str) -> Option<serde_json::Value> {
        self.task_state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(task_id)
            .and_then(|state| state.get(key).cloned())
    }

    /// Set task state for a specific task
    pub fn set_task_state(&self, task_id: &str, key: &str, value: serde_json::Value) {
        {
            let mut task_states = self.task_state.write().unwrap_or_else(|e| e.into_inner());
            let task_state = task_states.entry(task_id.to_string()).or_default();
            task_state.insert(key.to_string(), value);
        }

        let mut pending = self
            .pending_task_states
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        pending
            .entry(task_id.to_string())
            .or_default()
            .insert(key.to_string());
    }

    // ==================== Secrets ====================

    /// Get a secret
    pub fn get_secret(&self, key: &str) -> Option<String> {
        self.secrets
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
    }

    /// Set a secret
    pub fn set_secret(&self, key: &str, value: String) {
        {
            let mut secrets = self.secrets.write().unwrap_or_else(|e| e.into_inner());
            secrets.insert(key.to_string(), value);
        }
        self.mark_secret_pending(key.to_string());
    }

    // ==================== Persistence ====================

    /// Load global state from disk
    fn load_global_state(&self) -> io::Result<GlobalState> {
        let file_path = self
            .state_dir
            .join("..")
            .join("settings")
            .join("global_settings.json");
        if !file_path.exists() {
            return Ok(GlobalState::default());
        }

        let contents = fs::read_to_string(&file_path)?;
        match serde_json::from_str(&contents) {
            Ok(state) => Ok(state),
            Err(e) => {
                // Create backup of corrupted file before discarding
                if let Ok(backup_path) = crate::storage::disk::create_backup(&file_path) {
                    eprintln!(
                        "WARNING: Corrupted global settings at '{}'. \
                         Backed up to '{}' for potential recovery. \
                         Starting with default settings.",
                        file_path.display(),
                        backup_path.display()
                    );
                    tracing::warn!(
                        file_path = %file_path.display(),
                        backup_path = %backup_path.display(),
                        error = %e,
                        "Created backup of corrupted global settings JSON"
                    );
                } else {
                    eprintln!(
                        "WARNING: Corrupted global settings at '{}'. \
                         Failed to create backup. Starting with default settings.",
                        file_path.display()
                    );
                    tracing::warn!(
                        file_path = %file_path.display(),
                        error = %e,
                        "Failed to parse global settings JSON and backup failed"
                    );
                }
                Ok(GlobalState::default())
            }
        }
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
                    eprintln!(
                        "WARNING: Corrupted workspace state at '{}'. \
                         Backed up to '{}' for potential recovery. \
                         Starting with empty state.",
                        file_path.display(),
                        backup_path.display()
                    );
                    tracing::warn!(
                        file_path = %file_path.display(),
                        backup_path = %backup_path.display(),
                        error = %e,
                        "Created backup of corrupted workspace state JSON"
                    );
                } else {
                    eprintln!(
                        "WARNING: Corrupted workspace state at '{}'. \
                         Failed to create backup. Starting with empty state.",
                        file_path.display()
                    );
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
            .unwrap_or_else(|e| e.into_inner());
        pending.insert(key);
    }

    /// Mark a secret as pending persistence
    fn mark_secret_pending(&self, key: String) {
        let mut pending = self
            .pending_secrets
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        pending.insert(key);
    }

    /// Persist all pending changes to disk.
    /// This is called periodically or on explicit flush.
    pub fn persist(&self) -> io::Result<()> {
        // Persist global state
        let global_keys: HashSet<String> = {
            let pending = self
                .pending_global_keys
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            pending.clone()
        };

        if !global_keys.is_empty() {
            self.persist_global_state(&global_keys)?;
            let mut pending = self
                .pending_global_keys
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for key in &global_keys {
                pending.remove(key);
            }
        }

        // Persist task states
        let task_states: HashMap<String, HashSet<String>> = {
            let pending = self
                .pending_task_states
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            pending.clone()
        };

        if !task_states.is_empty() {
            self.persist_task_states(&task_states)?;
            let mut pending = self
                .pending_task_states
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for task_id in task_states.keys() {
                pending.remove(task_id);
            }
        }

        // Persist secrets
        let secrets: HashSet<String> = {
            let pending = self
                .pending_secrets
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            pending.clone()
        };

        if !secrets.is_empty() {
            self.persist_secrets(&secrets)?;
            let mut pending = self
                .pending_secrets
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for key in &secrets {
                pending.remove(key);
            }
        }

        // Persist workspace state
        self.persist_workspace_state()?;

        // Update last persist time
        *self.last_persist.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());

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

    /// Persist global state to disk, writing only the specified keys.
    ///
    /// Reads the full current state, applies only the pending changes to it,
    /// then atomically writes back. This ensures atomicity for partial updates
    /// while maintaining backward compatibility with the full-state file format.
    fn persist_global_state(&self, keys: &HashSet<String>) -> io::Result<()> {
        let state = self
            .global_state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let settings_dir = self.state_dir.join("..").join("settings");
        fs::create_dir_all(&settings_dir)?;

        let partial: GlobalState = if keys.len() == 1 {
            if let Some(key) = keys.iter().next() {
                if let Ok(parsed) = key.parse::<GlobalStateKey>() {
                    let mut partial = state.clone();
                    if let Some(value) = parsed.get_json_value(&state) {
                        parsed.set_json_value(&mut partial, value);
                    }
                    partial
                } else {
                    return self.persist_full_global_state(&state, &settings_dir);
                }
            } else {
                return Ok(());
            }
        } else {
            return self.persist_full_global_state(&state, &settings_dir);
        };

        let data = serde_json::to_string_pretty(&partial)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let file_path = settings_dir.join("global_settings.json");
        disk::atomic_write_file(&file_path, &data)?;
        Ok(())
    }

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
        let states = self.task_state.read().unwrap_or_else(|e| e.into_inner());

        for (task_id, keys) in task_states {
            if let Some(task_state) = states.get(task_id) {
                let task_dir = self.state_dir.join("..").join("tasks").join(task_id);
                fs::create_dir_all(&task_dir)?;

                // Read existing settings for read-merge-write (SM3 fix)
                let file_path = task_dir.join("settings.json");
                let mut existing_settings = serde_json::Map::new();
                if file_path.exists()
                    && let Ok(contents) = fs::read_to_string(&file_path)
                    && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&contents)
                    && let Some(obj) = parsed.as_object()
                {
                    existing_settings = obj.clone();
                }

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
        let secrets = self.secrets.read().unwrap_or_else(|e| e.into_inner());

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
    /// Writes the entire workspace state atomically.
    fn persist_workspace_state(&self) -> io::Result<()> {
        let workspace_state = self
            .workspace_state
            .read()
            .unwrap_or_else(|e| e.into_inner());

        let file_path = self.state_dir.join("workspace_state.json");
        let data = serde_json::to_string_pretty(&*workspace_state)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        crate::storage::disk::atomic_write_file(&file_path, &data)?;
        Ok(())
    }
}

/// Task history operations
/// List tasks with pagination
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
pub fn total_pages(total: usize, limit: usize) -> usize {
    total.div_ceil(limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};
    use tempfile::TempDir;

    fn with_temp_data_dir<T>(f: impl FnOnce() -> T) -> T {
        static TEST_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _guard = TEST_ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path().join("data");
        fs::create_dir_all(&data_dir).unwrap();

        unsafe {
            std::env::set_var("SNED_DATA_DIR", &data_dir);
        }

        let result = f();

        unsafe {
            std::env::remove_var("SNED_DATA_DIR");
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
    fn test_partial_global_state_persist_single_key() {
        with_temp_data_dir(|| {
            let manager = StateManager::new().unwrap();
            manager.initialize().unwrap();

            manager.set_global_state_key(
                GlobalStateKey::TerminalReuseEnabled,
                serde_json::Value::Bool(false),
            );
            manager.set_global_state_key(GlobalStateKey::IsNewUser, serde_json::Value::Bool(false));

            manager.persist().unwrap();

            let manager2 = StateManager::new().unwrap();
            manager2.initialize().unwrap();

            let terminal_reuse: Option<serde_json::Value> =
                manager2.get_global_state_key(GlobalStateKey::TerminalReuseEnabled);
            assert!(terminal_reuse.is_some());
            assert_eq!(terminal_reuse.unwrap(), serde_json::Value::Bool(false));

            let is_new_user: Option<serde_json::Value> =
                manager2.get_global_state_key(GlobalStateKey::IsNewUser);
            assert!(is_new_user.is_some());
            assert_eq!(is_new_user.unwrap(), serde_json::Value::Bool(false));
        });
    }

    #[test]
    fn test_partial_global_state_persist_single_key_preserves_unrelated_fields() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path().join("data");
        let state_dir = data_dir.join("state");
        let settings_dir = data_dir.join("settings");
        fs::create_dir_all(&state_dir).unwrap();
        fs::create_dir_all(&settings_dir).unwrap();

        let initial_state = serde_json::json!({
            "terminal_reuse_enabled": false,
            "is_new_user": false,
        });
        let mut manager = StateManager::new().unwrap();
        manager.state_dir = state_dir;
        *manager
            .global_state
            .write()
            .unwrap_or_else(|e| e.into_inner()) = serde_json::from_value(initial_state).unwrap();

        manager.set_global_state_key(
            GlobalStateKey::TerminalReuseEnabled,
            serde_json::Value::Bool(true),
        );
        manager.persist().unwrap();

        let persisted: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(settings_dir.join("global_settings.json")).unwrap(),
        )
        .unwrap();

        assert_eq!(
            persisted["terminal_reuse_enabled"],
            serde_json::Value::Bool(true)
        );
        assert_eq!(persisted["is_new_user"], serde_json::Value::Bool(false));
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
                    .unwrap_or_else(|e| e.into_inner())
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
                GlobalStateKey::TerminalReuseEnabled,
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
                manager.get_global_state_key(GlobalStateKey::TerminalReuseEnabled);
            assert!(value.is_none());

            let log_output =
                String::from_utf8(captured.lock().unwrap_or_else(|e| e.into_inner()).clone())
                    .unwrap();
            assert!(log_output.contains("failed to deserialize global state value"));
            assert!(log_output.contains("key=terminalReuseEnabled"));
            assert!(log_output.contains("error="));
        });
    }

    #[test]
    fn test_machine_id_generated_on_fresh_install() {
        with_temp_data_dir(|| {
            let manager = StateManager::new().unwrap();
            manager.initialize().unwrap();

            // Should have generated a machine ID
            let id = manager.get_distinct_id();
            assert_ne!(
                id, "anonymous",
                "Machine ID should be generated on fresh install"
            );
            assert!(!id.is_empty(), "Machine ID should not be empty");

            // Verify it's a valid ULID format (26 chars, base32)
            assert_eq!(id.len(), 26, "ULID should be 26 characters");
            assert!(
                id.chars().all(|c| c.is_ascii_alphanumeric()),
                "ULID should be alphanumeric"
            );
        });
    }

    #[test]
    fn test_machine_id_persisted_and_reused() {
        use std::fs;

        with_temp_data_dir(|| {
            let manager = StateManager::new().unwrap();
            manager.initialize().unwrap();

            // Get the generated ID
            let first_id = manager.get_distinct_id();
            assert_ne!(first_id, "anonymous");

            // Verify it was persisted to disk
            let settings_path = manager
                .state_dir
                .join("..")
                .join("settings")
                .join("global_settings.json");
            assert!(
                settings_path.exists(),
                "global_settings.json should be created"
            );

            let contents = fs::read_to_string(&settings_path).unwrap();
            assert!(
                contents.contains(&first_id),
                "Persisted file should contain the machine ID"
            );

            // Simulate a second run by creating a new StateManager with same data dir
            let manager2 = StateManager::new().unwrap();
            manager2.initialize().unwrap();
            let second_id = manager2.get_distinct_id();

            // Should read the same ID from disk
            assert_eq!(
                first_id, second_id,
                "Machine ID should be reused across runs"
            );
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

            let corrupted_content = r#"{"machine_id": "invalid json here"#;
            fs::write(&settings_path, corrupted_content).unwrap();

            // Load global state - should create backup and return defaults
            let result = manager.load_global_state();
            assert!(result.is_ok(), "Should return Ok even with corrupted file");

            let state = result.unwrap();
            assert!(
                state.sned_generated_machine_id.is_none(),
                "Should return default state with no machine_id"
            );

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
}
