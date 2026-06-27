pub mod actionable_errors;
pub mod colors;
pub mod image_input;
pub mod interactive;
pub mod markdown;
pub mod output;
pub mod redact;
pub mod slash_commands;
pub mod subcommands;
pub mod syntax_highlight;
pub mod text_utils;
pub mod tui;

pub use interactive::{
    InteractiveSession, render_interactive_prompt_prefix, run_interactive_shell_inner,
    should_start_interactive_shell,
};
pub use subcommands::{
    format_config_output, parse_config_assignment, run_auth, run_config, run_doctor, run_history,
    run_migration,
};

use clap::{CommandFactory, Parser, Subcommand};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Custom layer that routes `json_output` target events directly to stdout
/// without `tracing_subscriber::fmt` formatting (no timestamp, no prefix).
/// This ensures `--json-output` mode produces only valid JSON lines.
struct JsonOutputLayer;

impl<S> tracing_subscriber::Layer<S> for JsonOutputLayer
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if event.metadata().target() == "json_output" {
            struct MessageVisitor(String);
            impl tracing::field::Visit for MessageVisitor {
                fn record_debug(
                    &mut self,
                    _field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    let raw = format!("{value:?}");
                    self.0 = match serde_json::from_str::<serde_json::Value>(&raw) {
                        Ok(parsed) => match parsed {
                            serde_json::Value::String(inner) => inner,
                            other => other.to_string(),
                        },
                        Err(_) => serde_json::Value::String(raw).to_string(),
                    };
                }
                fn record_str(&mut self, _field: &tracing::field::Field, value: &str) {
                    self.0 = value.to_string();
                }
            }
            let mut visitor = MessageVisitor(String::new());
            event.record(&mut visitor);
            let mut stdout = std::io::stdout().lock();
            let _ = writeln!(stdout, "{}", visitor.0);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TracingMode {
    JsonOnly,
    Human { verbose: bool },
}

fn tracing_mode(json_output: bool, verbose: bool) -> TracingMode {
    if json_output {
        TracingMode::JsonOnly
    } else {
        TracingMode::Human { verbose }
    }
}

fn init_tracing(mode: TracingMode, debug: bool, tui_mode: bool) {
    match mode {
        TracingMode::JsonOnly => {
            tracing_subscriber::registry().with(JsonOutputLayer).init();
        }
        TracingMode::Human { verbose } => {
            let log_level = if verbose || debug { "debug" } else { "warn" };
            let env_filter = EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(format!("sned={log_level}")));

            use tracing_subscriber::fmt::writer::BoxMakeWriter;

            // In TUI mode, route tracing to a log file to keep warnings
            // from corrupting the alternate screen display.  Non-TUI mode
            // writes to stderr as before.  BoxMakeWriter provides a
            // type-erased wrapper so both branches share the same
            // `Layer<…>` type.
            let writer: BoxMakeWriter = if tui_mode {
                let path =
                    std::env::temp_dir().join(format!("sned-tui-{}.log", std::process::id()));
                match std::fs::File::create(&path) {
                    Ok(file) => {
                        eprintln!(
                            "[sned] TUI mode: tracing output redirected to {}",
                            path.display()
                        );
                        BoxMakeWriter::new(std::sync::Arc::new(file))
                    }
                    Err(e) => {
                        eprintln!(
                            "[sned] Warning: could not create TUI trace log {}: {}",
                            path.display(),
                            e
                        );
                        // Fallback to stderr if file creation fails.
                        BoxMakeWriter::new(std::io::stderr)
                    }
                }
            } else {
                BoxMakeWriter::new(std::io::stderr)
            };

            let fmt_layer = tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_filter(env_filter);

            let registry = tracing_subscriber::registry()
                .with(fmt_layer)
                .with(JsonOutputLayer);

            if debug {
                let log_file = std::fs::File::create("/tmp/sned-debug.log")
                    .expect("Failed to create debug log file");
                let log_file = std::sync::Arc::new(log_file);
                let file_layer = tracing_subscriber::fmt::layer()
                    .with_writer(log_file)
                    .with_thread_ids(true)
                    .with_thread_names(true)
                    .with_target(true)
                    .with_file(true)
                    .with_line_number(true)
                    .with_filter(EnvFilter::new("debug"));
                registry.with(file_layer).init();
            } else {
                registry.init();
            }
        }
    }
}

/// Options shared between `task`, the default prompt command, and `resume`.
///
/// Source: `dirac/cli/src/index.ts` `TaskOptions` interface and the
/// `.option()` calls on both the root command and the `task` subcommand.
#[derive(Debug, Clone, Parser)]
#[command(next_help_heading = "Task Options")]
pub struct TaskOptions {
    /// Run in act mode
    #[arg(short = 'a', long)]
    pub act: bool,

    /// Run in plan mode
    #[arg(short = 'p', long)]
    pub plan: bool,

    /// Enable yes/yolo mode (auto-approve actions)
    #[arg(short = 'y', long)]
    pub yolo: bool,

    /// Enable auto-approve all actions while keeping interactive mode
    #[arg(long)]
    pub auto_approve_all: bool,

    /// Optional timeout in seconds (applies only when provided)
    #[arg(short = 't', long)]
    pub timeout: Option<String>,

    /// Model to use for the task
    #[arg(short = 'm', long)]
    pub model: Option<String>,

    /// API provider to use (requires --model). Auto-detected when --base-url is set
    #[arg(long)]
    pub provider: Option<String>,

    /// Base URL for custom OpenAI-compatible provider (or set OPENAI_API_BASE env var)
    #[arg(long)]
    pub base_url: Option<String>,

    /// API key for custom provider (or set OPENAI_API_KEY env var)
    #[arg(long)]
    pub api_key: Option<String>,

    /// Show verbose output
    #[arg(short = 'v', long)]
    pub verbose: bool,

    /// Working directory for the task
    #[arg(short = 'c', long)]
    pub cwd: Option<String>,

    /// Path to Sned configuration directory
    #[arg(long)]
    pub config: Option<String>,

    /// Enable extended thinking (default: 1024 tokens)
    #[arg(long)]
    pub thinking: Option<Option<String>>,

    /// Reasoning effort: none|low|medium|high|xhigh
    #[arg(long, value_name = "effort")]
    pub reasoning_effort: Option<String>,

    /// Maximum consecutive mistakes before halting in yolo mode
    #[arg(long, value_name = "count")]
    pub max_consecutive_mistakes: Option<String>,

    /// Output messages as JSON instead of styled text
    #[arg(long)]
    pub json: bool,

    /// Reject first completion attempt to force re-verification
    #[arg(long)]
    pub double_check_completion: bool,

    /// Enable adaptive context compaction instead of mechanical truncation (enabled by default)
    #[arg(long, default_value_t = true)]
    pub auto_condense: bool,

    /// Disable token usage display after each turn (hidden by default)
    #[arg(long)]
    pub no_token_display: bool,

    /// Enable subagents for the task
    #[arg(long)]
    pub subagents: bool,

    /// Internal flag: marks this task as running inside a subagent (prevents recursion)
    #[arg(long, hide = true)]
    pub is_subagent: bool,

    /// Custom User-Agent header for OpenAI-compatible provider
    #[arg(long)]
    pub user_agent: Option<String>,

    /// Path to additional hooks directory for runtime hook injection
    #[arg(long)]
    pub hooks_dir: Option<String>,

    /// Export conversation to file after completion (JSON or markdown)
    #[arg(long, value_name = "path")]
    pub export: Option<String>,

    /// Image files to include with the task prompt
    #[arg(short = 'i', long, value_name = "path")]
    pub image: Vec<String>,

    /// Enable automatic change tracking (shadow git for undo/versioning)
    #[arg(long)]
    pub track_changes: bool,

    /// Maximum number of context turns before pruning (default: 50)
    #[arg(long, value_name = "turns")]
    pub max_context_turns: Option<String>,

    /// Maximum provider output tokens for this task
    #[arg(long, value_name = "tokens")]
    pub max_tokens: Option<u32>,

    /// Enable debug logging to /tmp/sned-debug.log
    #[arg(long)]
    pub debug: bool,
}

/// Additional options only on the root (default) command, not on `task`.
#[derive(Debug, Clone, Parser)]
#[command(next_help_heading = "Root Command Options")]
pub struct RootOnlyOptions {
    /// Resume an existing task by ID
    #[arg(short = 'T', long)]
    pub task_id: Option<String>,

    /// Resume the most recent task from the current working directory
    #[arg(long = "continue")]
    pub continue_task: bool,
}

/// Options for the `history` subcommand.
///
/// Source: `dirac/cli/src/index.ts` `program.command("history")` options.
#[derive(Debug, Clone, Parser)]
pub struct HistoryOptions {
    /// Number of tasks to show
    #[arg(short = 'n', long, default_value = "10")]
    pub limit: u32,

    /// Page number (1-based)
    #[arg(short = 'p', long, default_value = "1")]
    pub page: u32,

    /// Show only favorited tasks
    #[arg(long)]
    pub favorites_only: bool,

    /// Show only tasks from current workspace
    #[arg(long)]
    pub workspace_only: bool,

    /// Search query to filter tasks by prompt text
    #[arg(short = 's', long)]
    pub search: Option<String>,

    /// Sort order: newest (default), oldest, or alphabetical
    #[arg(long, default_value = "newest", value_parser = ["newest", "oldest", "alphabetical"])]
    pub sort: String,

    /// Path to Sned configuration directory
    #[arg(long)]
    pub config: Option<String>,
}

/// Options for the `config` subcommand.
#[derive(Debug, Clone, Parser)]
pub struct ConfigOptions {
    #[command(subcommand)]
    pub action: Option<ConfigAction>,

    /// Path to Sned configuration directory
    #[arg(long)]
    pub config: Option<String>,

    /// Validate configuration (check provider keys and connectivity)
    #[arg(long)]
    pub validate: bool,

    /// Migrate from another Sned configuration directory
    #[arg(long)]
    pub migrate: Option<String>,

    /// Show migration plan without executing (use with --migrate)
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ConfigAction {
    Set {
        #[arg(value_name = "key=value")]
        assignment: String,
    },
    /// List all valid config keys with their types and current values
    List,
}

/// Options for the `auth` subcommand.
///
/// Source: `dirac/cli/src/index.ts` `program.command("auth")` options.
#[derive(Debug, Clone, Parser)]
pub struct AuthOptions {
    /// Provider ID for quick setup (e.g., openai-native, anthropic, moonshot)
    #[arg(short = 'p', long)]
    pub provider: Option<String>,

    /// API key for the provider
    #[arg(short = 'k', long)]
    pub apikey: Option<String>,

    /// Model ID to configure (e.g., gpt-4o, claude-sonnet-4-6, kimi-k2.5)
    #[arg(short = 'm', long)]
    pub modelid: Option<String>,

    /// Base URL (optional, only for openai provider)
    #[arg(short = 'b', long)]
    pub baseurl: Option<String>,

    /// Azure API version (optional, only for azure openai)
    #[arg(long)]
    pub azure_api_version: Option<String>,

    /// Show verbose output
    #[arg(short = 'v', long)]
    pub verbose: bool,

    /// Working directory for the task
    #[arg(short = 'c', long)]
    pub cwd: Option<String>,

    /// Path to Sned configuration directory
    #[arg(long)]
    pub config: Option<String>,
}

/// Sned CLI subcommands.
///
/// Source: `dirac/cli/src/index.ts` Commander `.command()` definitions.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run a new task
    #[command(alias = "t")]
    Task {
        /// The task prompt
        prompt: String,

        #[command(flatten)]
        opts: Box<TaskOptions>,
    },

    /// List task history
    #[command(alias = "h")]
    History {
        #[command(flatten)]
        opts: HistoryOptions,
    },

    /// Show current configuration
    Config {
        #[command(flatten)]
        opts: ConfigOptions,
    },

    /// Authenticate a provider and configure what model is used
    Auth {
        #[command(flatten)]
        opts: AuthOptions,
    },

    /// Show Sned CLI version number
    Version,

    /// Developer tools and utilities
    Dev {
        #[command(subcommand)]
        subcmd: DevSubcommand,
    },

    /// Generate shell completion scripts
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },

    /// Run diagnostic checks
    Doctor,
}

/// Dev subcommands.
#[derive(Debug, Subcommand)]
pub enum DevSubcommand {
    /// Open the log file
    Log,
}

/// For custom OpenAI-compatible providers: set OPENAI_API_KEY + OPENAI_API_BASE env vars, or use --api-key + --base-url flags
///
/// Exit Codes:
/// - 0: Success
/// - 1: General error (API failure, unexpected error)
/// - 2: Configuration error (missing API key, invalid config)
/// - 3: Input error (invalid prompt, bad flag)
/// - 4: Tool error (edit_file failure, command execution failure)
/// - 5: Signal/interrupted
#[derive(Debug, Parser)]
#[command(
    name = "sned",
    version = format!("{} (commit: {}, build: {})",
        env!("CARGO_PKG_VERSION"),
        env!("GIT_COMMIT_HASH"),
        env!("BUILD_PROFILE")
    ),
    about = "Sned CLI for code editing in your terminal",
    after_help = "Exit Codes: 0=Success, 1=General error, 2=Config error, 3=Input error, 4=Tool error, 5=Interrupted"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Task prompt (starts task immediately)
    #[arg(value_name = "prompt")]
    pub prompt: Option<String>,

    #[command(flatten)]
    pub task_opts: TaskOptions,

    #[command(flatten)]
    pub root_opts: RootOnlyOptions,
}

/// Parse and return the CLI structure.
#[must_use] 
pub fn parse() -> Cli {
    Cli::parse()
}

/// Extract the config path from the parsed CLI arguments and set SNED_DIR if present.
pub fn apply_config_override(cli: &Cli) {
    let mut config_path = cli.task_opts.config.clone();

    if let Some(cmd) = &cli.command {
        match cmd {
            Command::Task { opts, .. } if opts.config.is_some() => {
                config_path = opts.config.clone();
            }
            Command::History { opts } if opts.config.is_some() => {
                config_path = opts.config.clone();
            }
            Command::Config { opts } if opts.config.is_some() => {
                config_path = opts.config.clone();
            }
            Command::Auth { opts } if opts.config.is_some() => {
                config_path = opts.config.clone();
            }
            _ => {}
        }
    }

    if let Some(path) = config_path {
        // SAFETY: called during CLI startup before any worker threads spawn
        unsafe {
            std::env::set_var("SNED_DIR", &path);
        }
    }
}

fn cli_log_file_path_from(data_dir: &Path, log_dir: Option<&Path>) -> PathBuf {
    log_dir.map_or_else(|| data_dir.join("logs"), Path::to_path_buf)
        .join("sned.1.log")
}

fn cli_log_file_path() -> PathBuf {
    let data_dir = crate::storage::disk::get_data_dir();
    let log_dir = std::env::var_os("SNED_LOG_DIR").map(PathBuf::from);
    cli_log_file_path_from(&data_dir, log_dir.as_deref())
}

fn open_path_in_default_app(path: &Path) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let status = ProcessCommand::new("open").arg(path).status()?;
        if !status.success() {
            anyhow::bail!("Failed to open log file");
        }
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
        let status = ProcessCommand::new("cmd")
            .arg("/C")
            .arg("start")
            .arg("")
            .arg(path)
            .status()?;
        if !status.success() {
            anyhow::bail!("Failed to open log file");
        }
        return Ok(());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let status = ProcessCommand::new("xdg-open").arg(path).status()?;
        if !status.success() {
            anyhow::bail!("Failed to open log file");
        }
        Ok(())
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows", unix)))]
    {
        let _ = path;
        anyhow::bail!("Opening the log file is not supported on this platform");
    }
}

fn open_cli_log_file() -> anyhow::Result<()> {
    open_path_in_default_app(&cli_log_file_path())
}

fn run_task(
    prompt: Option<String>,
    task_opts: TaskOptions,
    root_opts: RootOnlyOptions,
) -> anyhow::Result<()> {
    let json_mode = task_opts.json;
    let rt = tokio::runtime::Runtime::new()?;
    let task_id = rt.block_on(run_task_inner(prompt, task_opts, root_opts))?;
    // In JSON mode stdout is reserved for structured events, so route
    // the session ID to stderr to keep stdout parseable as JSONL.
    if json_mode {
        eprintln!("Session: {task_id}");
    } else {
        println!("Session: {task_id}");
    }
    Ok(())
}

struct TaskComponents {
    state_manager: Arc<crate::storage::state_manager::StateManager>,
    config: crate::core::agent_loop::AgentConfig,
    system_prompt_context: crate::core::context::SystemPromptContext,
    task_storage: crate::storage::task_storage::TaskStorage,
    context_loader: crate::core::context::ContextLoader,
    approval_manager: Arc<tokio::sync::Mutex<crate::core::approval::ApprovalManager>>,
    hook_manager: Arc<crate::core::hooks::HookManager>,
    checkpoint_mgr: crate::core::checkpoints::TaskCheckpointManager,
    registry: Arc<crate::core::tools::ToolRegistry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SymbolIndexMode {
    Off,
    Memory,
    Persisted,
}

const SYMBOL_INDEX_ENV: &str = "SNED_SYMBOL_INDEX";

fn parse_symbol_index_mode_value(value: &str) -> anyhow::Result<SymbolIndexMode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" => Ok(SymbolIndexMode::Off),
        "memory" => Ok(SymbolIndexMode::Memory),
        "persisted" => Ok(SymbolIndexMode::Persisted),
        other => Err(crate::error::CliError::config(format!(
            "Invalid {SYMBOL_INDEX_ENV} value '{other}'. Expected one of: off, memory, persisted."
        ))
        .into()),
    }
}

fn symbol_index_mode_from_env() -> anyhow::Result<SymbolIndexMode> {
    match std::env::var(SYMBOL_INDEX_ENV) {
        Ok(value) => parse_symbol_index_mode_value(&value),
        Err(std::env::VarError::NotPresent) => Ok(SymbolIndexMode::Persisted),
        Err(std::env::VarError::NotUnicode(_)) => Err(crate::error::CliError::config(format!(
            "{SYMBOL_INDEX_ENV} must be valid UTF-8. Expected one of: off, memory, persisted."
        ))
        .into()),
    }
}

/// Hash the cwd to produce a short collision-resistant suffix for the
/// `/tmp` symbol-index directory name. Uses SHA-256 (not `DefaultHasher`,
/// which is seeded randomly per process) so the same cwd always maps to
/// the same suffix across process restarts. The first 16 hex chars of the
/// digest are used as a short, human-inspectable identifier.
fn hash_cwd(cwd: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(cwd.as_bytes());
    // Take the first 8 bytes and format as 16 hex chars.
    let bytes: [u8; 8] = digest[..8].try_into().unwrap_or([0u8; 8]);
    format!("{:016x}", u64::from_be_bytes(bytes))
}

fn build_symbol_index_service(
    cwd_str: String,
    mode: SymbolIndexMode,
) -> crate::services::symbol_index::SymbolIndexService {
    // Derive a unique /tmp path for the symbol index DB so the workspace
    // directory stays clean. Each cwd maps to a unique /tmp path via a hash
    // of the absolute path.
    let index_root = format!("/tmp/sned-symbol-index-{}", hash_cwd(&cwd_str));
    let service = crate::services::symbol_index::SymbolIndexService::new(cwd_str.clone())
        .with_index_root(index_root.clone());
    match mode {
        SymbolIndexMode::Off => service.disabled(),
        SymbolIndexMode::Memory => service,
        SymbolIndexMode::Persisted => {
            match service.with_persistence() {
                Ok(svc) => svc,
                Err(e) => {
                    tracing::warn!(
                        "Symbol index DB corruption detected, falling back to memory mode: {}",
                        e
                    );
                    // Delete corrupted DB file so next session can start fresh.
                    // Use the same index_root path that was configured for the
                    // service, not the project-local path.
                    let db_dir = std::path::Path::new(&index_root)
                        .join(crate::services::symbol_index::INDEX_DIR);
                    let db_path = db_dir.join(crate::services::symbol_index::DB_FILENAME);
                    if db_path.exists() {
                        let _ = std::fs::remove_file(&db_path);
                    }
                    // Return in-memory service (service was moved into with_persistence, so create new one)
                    crate::services::symbol_index::SymbolIndexService::new(
                        cwd_str,
                    )
                }
            }
        }
    }
}

pub(crate) fn create_provider(
    task_opts: &TaskOptions,
) -> anyhow::Result<Arc<crate::providers::Providers>> {
    use crate::providers::env_auth::get_provider_from_env;

    // Determine provider: --provider flag > auto-detection from env > custom base_url (with api_key from flag/env) > error
    let (provider_name, was_auto_detected) = if let Some(explicit) = &task_opts.provider {
        (explicit.as_str(), false)
    } else if let Some(detected) = get_provider_from_env() {
        (detected, true)
    } else if task_opts.base_url.is_some() {
        // Custom OpenAI-compatible endpoint with explicit base URL
        // API key will be resolved from --api-key flag or OPENAI_API_KEY env var
        ("openai", false)
    } else {
        // No provider flag and no auto-detected keys - show helpful error
        anyhow::bail!(
            "No API provider configured. Set one of these environment variables:\n\
             \n\
             Common providers:\n\
             \x1b[36m  ANTHROPIC_API_KEY\x1b[0m       - Anthropic Claude\n\
             \x1b[36m  OPENAI_API_KEY\x1b[0m          - OpenAI GPT\n\
             \x1b[36m  GEMINI_API_KEY\x1b[0m          - Google Gemini\n\
             \x1b[36m  OPENROUTER_API_KEY\x1b[0m      - OpenRouter\n\
             \x1b[36m  DEEPSEEK_API_KEY\x1b[0m       - DeepSeek\n\
             \x1b[36m  QWEN_API_KEY\x1b[0m            - Qwen\n\
             \n\
             Or use --provider flag with --api-key and/or --base-url"
        );
    };

    // Only override to openai for custom base URL if user didn't explicitly specify provider
    let is_custom_provider =
        task_opts.base_url.is_some() || std::env::var("OPENAI_API_BASE").is_ok();
    let provider_name = if is_custom_provider && task_opts.provider.is_none() {
        "openai"
    } else {
        provider_name
    };

    if was_auto_detected && !task_opts.json {
        tracing::info!("Auto-detected provider: {}", provider_name);
    }

    let model_id = task_opts.model.clone();
    let thinking_budget = task_opts
        .thinking
        .as_ref()
        .and_then(|t| t.as_ref())
        .and_then(|s| s.parse::<u32>().ok())
        .or_else(|| {
            if task_opts.thinking.is_some() {
                Some(1024u32)
            } else {
                None
            }
        });
    let user_agent = task_opts.user_agent.clone();

    let provider: Arc<crate::providers::Providers> = match provider_name {
        "anthropic" => {
            let api_key = std::env::var("ANTHROPIC_API_KEY")
                .ok()
                .filter(|k| !k.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "ANTHROPIC_API_KEY is not set. \n\
                         Set the environment variable or pass --api-key."
                    )
                })?;
            // Use stored model ID default and base URL if not specified
            let state = crate::storage::global_state::load_global_state();
            let default_model = model_id
                .or_else(|| state.act_mode_api_model_id.clone())
                .unwrap_or_else(|| "claude-3-5-sonnet-20240620".to_string());
            let base_url = state.anthropic_base_url.filter(|u| !u.is_empty());
            Arc::new(crate::providers::Providers::Anthropic(
                crate::providers::anthropic::AnthropicProvider::new(
                    crate::providers::anthropic::AnthropicConfig {
                        api_key,
                        base_url,
                        model_id: default_model,
                        model_info: Some(crate::providers::ModelInfo::default()),
                        thinking_budget_tokens: thinking_budget,
                    },
                )?,
            ))
        }
        "minimax" => {
            // Use stored model ID default if not specified
            let default_model = model_id
                .or_else(|| {
                    let state = crate::storage::global_state::load_global_state();
                    state.act_mode_api_model_id
                })
                .unwrap_or_else(|| "MiniMax-M2.7".to_string());
            // Determine api_line: "china" if MINIMAX_CN_API_KEY is set, otherwise default
            let api_line = if std::env::var("MINIMAX_CN_API_KEY").is_ok() {
                Some("china".to_string())
            } else {
                std::env::var("MINIMAX_API_LINE").ok()
            };
            let api_key = std::env::var("MINIMAX_CN_API_KEY")
                .or_else(|_| std::env::var("MINIMAX_API_KEY"))
                .unwrap_or_default();
            Arc::new(crate::providers::Providers::Minimax(
                crate::providers::minimax::MinimaxProvider::new(
                    crate::providers::minimax::MinimaxConfig {
                        api_key,
                        api_line,
                        model_id: default_model,
                        model_info: None,
                        thinking_budget_tokens: thinking_budget,
                    },
                )?,
            ))
        }
        "openai" | "openai-native" => {
            let api_key = task_opts
                .api_key
                .clone()
                .or_else(|| std::env::var("OPENAI_API_KEY").ok())
                .unwrap_or_default();
            if api_key.is_empty() {
                anyhow::bail!(
                    "No API key found for OpenAI-compatible provider. \n\
                     Set OPENAI_API_KEY environment variable or use --api-key flag."
                );
            }
            let base_url = task_opts
                .base_url
                .clone()
                .or_else(|| std::env::var("OPENAI_API_BASE").ok());
            // Use stored model ID default if not specified
            let default_model = model_id
                .or_else(|| {
                    let state = crate::storage::global_state::load_global_state();
                    state.act_mode_api_model_id
                })
                .unwrap_or_else(|| "gpt-4o".to_string());
            Arc::new(crate::providers::Providers::OpenAi(
                crate::providers::openai::OpenAiProvider::new(
                    crate::providers::openai::OpenAiConfig {
                        model_id: default_model,
                        api_key,
                        base_url,
                        model_info: None,
                        reasoning_effort: task_opts.reasoning_effort.clone(),
                        custom_headers: user_agent.map(|ua| {
                            let mut headers = std::collections::HashMap::with_capacity(1);
                            headers.insert("User-Agent".to_string(), ua);
                            headers
                        }),
                        provider_name: None, // Use default "OpenAI"
                    },
                )?,
            ))
        }
        "gemini" => {
            let api_key = task_opts
                .api_key
                .clone()
                .or_else(|| std::env::var("GEMINI_API_KEY").ok())
                .unwrap_or_default();
            let base_url = task_opts
                .base_url
                .clone()
                .or_else(|| std::env::var("GEMINI_BASE_URL").ok());
            // Use stored model ID default if not specified
            let default_model = model_id
                .or_else(|| {
                    let state = crate::storage::global_state::load_global_state();
                    state.act_mode_api_model_id
                })
                .unwrap_or_else(|| "gemini-3.1-pro-preview".to_string());
            Arc::new(crate::providers::Providers::Gemini(
                crate::providers::gemini::GeminiProvider::new(
                    crate::providers::gemini::GeminiConfig {
                        model_id: default_model,
                        api_key,
                        base_url,
                        model_info: None,
                        thinking_budget_tokens: thinking_budget,
                        reasoning_effort: task_opts.reasoning_effort.clone(),
                        search_enabled: false,
                    },
                )?,
            ))
        }
        "deepseek" => {
            let api_key = task_opts
                .api_key
                .clone()
                .or_else(|| std::env::var("DEEPSEEK_API_KEY").ok())
                .unwrap_or_default();
            let model_id_str = model_id
                .unwrap_or_else(|| "deepseek-chat".to_string());
            Arc::new(crate::providers::Providers::DeepSeek(
                crate::providers::deepseek::DeepSeekProvider::new(
                    crate::providers::deepseek::DeepSeekConfig {
                        api_key,
                        model_id: model_id_str.clone(),
                        model_info: Some(crate::providers::deepseek::get_deepseek_model_info(
                            &model_id_str,
                        )),
                    },
                )?,
            ))
        }
        "openrouter" => {
            let api_key = task_opts
                .api_key
                .clone()
                .or_else(|| std::env::var("OPENROUTER_API_KEY").ok())
                .unwrap_or_default();
            let model_id_str = model_id
                .unwrap_or_else(|| "anthropic/claude-sonnet-4.5".to_string());
            Arc::new(crate::providers::Providers::OpenRouter(
                crate::providers::openrouter::OpenRouterProvider::new(
                    crate::providers::openrouter::OpenRouterConfig {
                        api_key,
                        model_id: model_id_str.clone(),
                        model_info: Some(crate::providers::openrouter::get_openrouter_model_info(
                            &model_id_str,
                        )),
                        provider_sort: None,
                        provider_name: None, // Use default "openrouter"
                    },
                )?,
            ))
        }
        "mock" => {
            if std::env::var_os("SNED_MOCK_APPROVAL_SCROLL").is_some() {
                Arc::new(crate::providers::Providers::Mock(
                    crate::providers::mock::MockProvider::approval_scroll_scenario(),
                ))
            } else if std::env::var_os("SNED_MOCK_BUSY_STREAM").is_some() {
                Arc::new(crate::providers::Providers::Mock(
                    crate::providers::mock::MockProvider::busy_stream_scenario(),
                ))
            } else {
                Arc::new(crate::providers::Providers::Mock(
                    crate::providers::mock::MockProvider::single_text_response_repeat(
                        "Mock provider response - task completed successfully",
                    ),
                ))
            }
        }
        _ => {
            return Err(crate::error::CliError::config(format!(
                "Unsupported provider: {provider_name}"
            ))
            .into());
        }
    };

    Ok(provider)
}

fn load_skills() -> Vec<crate::core::context::SkillMetadata> {
    use crate::core::context::{discover_skills, get_available_skills};

    std::env::current_dir()
        .ok()
        .map(|cwd| get_available_skills(discover_skills(&cwd)))
        .unwrap_or_default()
}

struct RulesContext {
    agents_rules: Option<String>,
    cursor_rules_file: Option<String>,
    cursor_rules_dir: Option<String>,
    windsurf_rules: Option<String>,
}

fn load_rules(
    cwd_path: &std::path::Path,
    state_manager: &crate::storage::state_manager::StateManager,
) -> RulesContext {
    use crate::core::context::{
        get_local_agents_rules, get_local_cursor_rules, get_local_windsurf_rules,
    };

    let rule_toggles: crate::core::context::RuleToggles = state_manager
        .get_global_state_key(crate::storage::state_manager::GlobalStateKey::GlobalSnedRulesToggles)
        .unwrap_or_default();

    let agents = get_local_agents_rules(cwd_path, &rule_toggles);
    let cursor = get_local_cursor_rules(cwd_path, &rule_toggles);
    let windsurf = get_local_windsurf_rules(cwd_path, &rule_toggles);

    RulesContext {
        agents_rules: agents,
        cursor_rules_file: cursor.first().cloned().flatten(),
        cursor_rules_dir: cursor.get(1).cloned().flatten(),
        windsurf_rules: windsurf,
    }
}

fn build_tool_registry(
    approval_manager: Arc<tokio::sync::Mutex<crate::core::approval::ApprovalManager>>,
    symbol_index_service: Arc<std::sync::Mutex<crate::services::symbol_index::SymbolIndexService>>,
    yolo_mode: bool,
) -> crate::core::tools::ToolRegistry {
    use crate::core::tools::ToolRegistry;

    let mut registry = ToolRegistry::new();
    registry.register(
        crate::core::tools::SnedTool::ExecuteCommand,
        Arc::new(
            crate::core::tools::handlers::execute_command::ExecuteCommandHandler::new()
                .with_yolo(yolo_mode),
        ),
    );
    registry.register(
        crate::core::tools::SnedTool::WriteToFile,
        Arc::new(crate::core::tools::handlers::write_to_file::WriteToFileHandler::new()),
    );
    registry.register(
        crate::core::tools::SnedTool::ReadFile,
        Arc::new(crate::core::tools::handlers::read_file::ReadFileHandler::new()),
    );
    registry.register(
        crate::core::tools::SnedTool::ListFiles,
        Arc::new(crate::core::tools::handlers::list_files::ListFilesHandler::new()),
    );
    registry.register(
        crate::core::tools::SnedTool::SearchFiles,
        Arc::new(crate::core::tools::handlers::search_files::SearchFilesHandler::new()),
    );
    registry.register(
        crate::core::tools::SnedTool::EditFile,
        Arc::new(
            crate::core::tools::handlers::edit_file::EditFileHandler::new()
                .with_approval_manager(approval_manager),
        ),
    );
    registry.register(
        crate::core::tools::SnedTool::AskFollowupQuestion,
        Arc::new(
            crate::core::tools::handlers::ask_followup_question::AskFollowupQuestionHandler::new(),
        ),
    );
    registry.register(
        crate::core::tools::SnedTool::AttemptCompletion,
        Arc::new(crate::core::tools::handlers::attempt_completion::AttemptCompletionHandler::new()),
    );
    registry.register(
        crate::core::tools::SnedTool::PlanModeRespond,
        Arc::new(crate::core::tools::handlers::plan_mode_respond::PlanModeRespondHandler::new()),
    );
    registry.register(
        crate::core::tools::SnedTool::GetFileSkeleton,
        Arc::new(crate::core::tools::handlers::get_file_skeleton::GetFileSkeletonHandler),
    );
    registry.register(
        crate::core::tools::SnedTool::GetFunction,
        Arc::new(crate::core::tools::handlers::get_function::GetFunctionHandler),
    );
    registry.register(
        crate::core::tools::SnedTool::FindSymbolReferences,
        Arc::new(crate::core::tools::handlers::find_symbol_references::FindSymbolReferencesHandler),
    );
    registry.register(
        crate::core::tools::SnedTool::ReplaceSymbol,
        Arc::new(
            crate::core::tools::handlers::replace_symbol::ReplaceSymbolHandler::new()
                .with_symbol_index(Arc::clone(&symbol_index_service)),
        ),
    );
    registry.register(
        crate::core::tools::SnedTool::RenameSymbol,
        Arc::new(
            crate::core::tools::handlers::rename_symbol::RenameSymbolHandler::new()
                .with_symbol_index(symbol_index_service),
        ),
    );
    registry.register(
        crate::core::tools::SnedTool::SummarizeTask,
        Arc::new(crate::core::tools::handlers::summarize_task::SummarizeTaskHandler::new()),
    );
    registry.register(
        crate::core::tools::SnedTool::Condense,
        Arc::new(crate::core::tools::handlers::condense::CondenseHandler::new()),
    );
    registry.register(
        crate::core::tools::SnedTool::WebFetch,
        Arc::new(crate::core::tools::handlers::web_fetch::WebFetchHandler),
    );
    registry.register(
        crate::core::tools::SnedTool::UseSkill,
        Arc::new(crate::core::tools::handlers::use_skill::UseSkillHandler::new()),
    );
    registry.register(
        crate::core::tools::SnedTool::ListSkills,
        Arc::new(crate::core::tools::handlers::list_skills::ListSkillsHandler::new()),
    );
    registry.register(
        crate::core::tools::SnedTool::DiagnosticsScan,
        Arc::new(crate::core::tools::handlers::diagnostics_scan::DiagnosticsScanHandler::new()),
    );
    registry.register(
        crate::core::tools::SnedTool::UseSubagents,
        Arc::new(crate::core::tools::handlers::use_subagents::UseSubagentsHandler::new()),
    );
    registry.register(
        crate::core::tools::SnedTool::NewTask,
        Arc::new(crate::core::tools::handlers::new_task::NewTaskHandler::new()),
    );

    registry
}

fn setup_hook_manager(
    task_opts: &TaskOptions,
    state_manager: &crate::storage::state_manager::StateManager,
) -> Arc<crate::core::hooks::HookManager> {
    let mut hook_manager = crate::core::hooks::HookManager::new(state_manager.get_distinct_id());
    if let Some(hooks_dir) = task_opts.hooks_dir.clone() {
        hook_manager.add_workspace_hooks_dir(std::path::PathBuf::from(hooks_dir));
    }

    Arc::new(hook_manager)
}

#[allow(clippy::unused_async)]
async fn build_task_components(
    task_opts: TaskOptions,
    root_opts: RootOnlyOptions,
    output_writer: Option<crate::cli::output::OutputWriterArc>,
) -> anyhow::Result<TaskComponents> {
    use crate::core::agent_loop::{AgentConfig, AgentMode};
    use crate::core::context::SystemPromptContext;
    use crate::storage::state_manager::StateManager;

    if let Some(ref cwd) = task_opts.cwd {
        std::env::set_current_dir(cwd)?;
    }

    let symbol_index_mode = symbol_index_mode_from_env()?;

    let state_manager = Arc::new(StateManager::new()?);
    state_manager.initialize()?;

    let provider = create_provider(&task_opts)?;

    let mode = if task_opts.plan {
        AgentMode::Plan
    } else {
        AgentMode::Act
    };
    let has_task_id = root_opts.task_id.is_some();
    let task_id = if let Some(id) = root_opts.task_id.clone() {
        id
    } else if root_opts.continue_task {
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(String::from))
            .ok_or_else(|| {
                anyhow::anyhow!("--continue requires a valid UTF-8 current directory")
            })?;
        state_manager
            .get_most_recent_task_for_workspace(&cwd)
            .map(|h| h.id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No previous task found for workspace '{cwd}'. \
                     Start a new task without --continue, or use --taskId to specify one."
                )
            })?
    } else {
        ulid::Ulid::new().to_string()
    };

    if task_opts.json {
        println!(
            "{}",
            serde_json::json!({ "type": "task_started", "taskId": task_id })
        );
    }

    let config = AgentConfig {
        provider: Arc::new(std::sync::Mutex::new(provider)),
        mode,
        task_id: task_id.clone(),
        enable_checkpoints: state_manager
            .get_global_state_key::<bool>(
                crate::storage::state_manager::GlobalStateKey::EnableCheckpoints,
            )
            .unwrap_or(true),
        use_auto_condense: task_opts.auto_condense,
        show_token_usage: !task_opts.no_token_display,
        json_output: task_opts.json,
        max_turns: 100,
        max_consecutive_mistakes: task_opts
            .max_consecutive_mistakes
            .clone()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3u32),
        double_check_completion: task_opts.double_check_completion,
        timeout_secs: task_opts
            .timeout
            .clone()
            .and_then(|s| s.parse().ok())
            .unwrap_or(300),
        track_changes: task_opts.track_changes,
        is_subagent_execution: task_opts.is_subagent,
        max_context_turns: task_opts
            .max_context_turns
            .as_ref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(50),
        max_tokens: task_opts.max_tokens,
        interactive_mode: false,
        output_writer: output_writer
            .unwrap_or_else(|| Arc::new(crate::cli::output::StderrOutputWriter)),
        strict_plan_mode_enabled: state_manager
            .get_global_state_key::<bool>(
                crate::storage::state_manager::GlobalStateKey::StrictPlanModeEnabled,
            )
            .unwrap_or(true),
    };

    let shell_path = std::env::var("SHELL").ok();
    let shell_type = shell_path.as_deref().and_then(|s| {
        std::path::Path::new(s)
            .file_name()
            .and_then(|n| n.to_str().map(String::from))
    });
    let available_cores = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from));

    let skills = load_skills();

    let (agents_rules, cursor_rules_file, cursor_rules_dir, windsurf_rules) =
        if let Ok(cwd_path) = std::env::current_dir() {
            let rules = load_rules(&cwd_path, &state_manager);
            (
                rules.agents_rules,
                rules.cursor_rules_file,
                rules.cursor_rules_dir,
                rules.windsurf_rules,
            )
        } else {
            (None, None, None, None)
        };

    let system_prompt_context = SystemPromptContext {
        cwd,
        model_id: task_opts.model.clone(),
        active_shell_path: shell_path,
        active_shell_type: shell_type,
        active_shell_is_posix: true,
        available_cores: Some(available_cores),
        enable_parallel_tool_calling: true,
        skills,
        local_agents_rules_file_instructions: agents_rules,
        local_cursor_rules_file_instructions: cursor_rules_file,
        local_cursor_rules_dir_instructions: cursor_rules_dir,
        local_windsurf_rules_file_instructions: windsurf_rules,
        ..Default::default()
    };

    let task_storage = crate::storage::task_storage::TaskStorage::new(&task_id)?;
    let cwd_str = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| ".".to_string());
    let is_new_task = !root_opts.continue_task && !has_task_id;
    if is_new_task {
        let _ = task_storage.create_initial_metadata(&cwd_str, None);
    }

    let symbol_index_service = Arc::new(std::sync::Mutex::new(build_symbol_index_service(
        cwd_str.clone(),
        symbol_index_mode,
    )));

    let context_loader = crate::core::context::ContextLoader::new(cwd_str.clone())
        .with_symbol_index_service(Arc::clone(&symbol_index_service));
    let approval_manager = Arc::new(tokio::sync::Mutex::new(
        crate::core::approval::ApprovalManager::new()
            .with_yolo(task_opts.yolo)
            .with_auto_approve_all(task_opts.auto_approve_all)
            .with_workspace_root(cwd_str.clone()),
    ));

    let registry = build_tool_registry(
        approval_manager.clone(),
        symbol_index_service,
        task_opts.yolo,
    );
    let hook_manager = setup_hook_manager(&task_opts, &state_manager);

    let checkpoint_mgr = crate::core::checkpoints::TaskCheckpointManager::new(
        task_id,
        config.enable_checkpoints,
        &cwd_str,
    );

    Ok(TaskComponents {
        state_manager,
        config,
        system_prompt_context,
        task_storage,
        context_loader,
        approval_manager,
        hook_manager,
        checkpoint_mgr,
        registry: Arc::new(registry),
    })
}

async fn run_task_inner(
    prompt: Option<String>,
    task_opts: TaskOptions,
    root_opts: RootOnlyOptions,
) -> anyhow::Result<String> {
    let session = InteractiveSession::build(task_opts, root_opts).await?;
    session.run(prompt).await?;
    let task_id = session.agent_loop().await.task_id().to_string();
    Ok(task_id)
}

fn run_interactive_shell(task_opts: TaskOptions, root_opts: RootOnlyOptions) -> anyhow::Result<()> {
    // ARCHITECTURAL GUARD: Interactive shell MUST use run_interactive_shell_inner
    // which has the TUI loop. Never call session.run(None) directly here.
    // See: AGENTS.md §Interactive Shell Architecture
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_interactive_shell_inner(task_opts, root_opts))
}

/// Query the terminal for the current cursor position.
/// Returns (row, col) as 1-based coordinates.

/// Restore raw mode after temporarily dropping it for confirmation input.
/// If restoration fails, cleans up terminal state and returns an error.

fn read_piped_stdin() -> anyhow::Result<Option<String>> {
    let stdin = io::stdin();
    if stdin.is_terminal() {
        return Ok(None);
    }

    let mut stdin = stdin;
    let mut input = String::new();
    stdin.read_to_string(&mut input)?;
    Ok(Some(input))
}

fn combine_prompt_with_stdin(
    prompt: Option<String>,
    stdin_input: Option<String>,
) -> Option<String> {
    match (prompt, stdin_input) {
        (Some(prompt), Some(stdin_input)) if !stdin_input.is_empty() => {
            Some(format!("{stdin_input}\n\n{prompt}"))
        }
        (Some(prompt), _) => Some(prompt),
        (None, Some(stdin_input)) if !stdin_input.is_empty() => Some(stdin_input),
        _ => None,
    }
}

/// Run the CLI. Dispatches to subcommand handlers or the default task.
pub fn run() -> anyhow::Result<()> {
    let cli = parse();
    apply_config_override(&cli);

    // The dispatch table for the `None` subcommand decides at runtime
    // whether to enter the interactive TUI shell. We mirror that
    // decision here so tracing can be redirected to a log file in TUI
    // mode (stderr writes inside the alternate screen corrupt the
    // display).
    let tui_mode = cli.command.is_none()
        && cli.root_opts.task_id.is_none()
        && interactive::should_start_interactive_shell(
            cli.prompt.is_some(),
            io::stdin().is_terminal(),
            io::stdout().is_terminal(),
            cli.task_opts.json,
        );

    init_tracing(
        tracing_mode(cli.task_opts.json, cli.task_opts.verbose),
        cli.task_opts.debug,
        tui_mode,
    );

    match cli.command {
        Some(Command::Task { prompt, opts }) => run_task(Some(prompt), *opts, cli.root_opts),
        Some(Command::History { opts }) => run_history(opts),
        Some(Command::Config { opts }) => run_config(opts),
        Some(Command::Auth { opts }) => run_auth(opts),
        Some(Command::Version) => {
            let commit = env!("GIT_COMMIT_HASH");
            let profile = env!("BUILD_PROFILE");
            println!("sned {}", env!("CARGO_PKG_VERSION"));
            println!("commit: {commit}");
            println!("build: {profile}");
            Ok(())
        }
        Some(Command::Dev { subcmd }) => match subcmd {
            DevSubcommand::Log => open_cli_log_file(),
        },
        Some(Command::Completions { shell }) => {
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_string();
            clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
            Ok(())
        }
        Some(Command::Doctor) => match run_doctor() {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                eprintln!("doctor failed: {e}");
                std::process::exit(crate::exit_codes::EXIT_ERROR)
            }
        },
        None => {
            let stdin_input = read_piped_stdin()?;
            let stdin_was_piped = stdin_input.is_some();

            if cli.root_opts.task_id.is_some() && cli.root_opts.continue_task {
                anyhow::bail!("Use either --taskId or --continue, not both.")
            }

            if cli.root_opts.continue_task {
                if cli.prompt.is_some() {
                    anyhow::bail!("Use --continue without a prompt.")
                }
                if stdin_was_piped {
                    anyhow::bail!("Use --continue without piped input.")
                }
                return run_interactive_shell(cli.task_opts, cli.root_opts);
            }

            if stdin_input.as_deref() == Some("") && cli.prompt.is_none() {
                anyhow::bail!("Empty input received from stdin. Please provide content to process.")
            }

            let effective_prompt = combine_prompt_with_stdin(cli.prompt.clone(), stdin_input)
                .map(|prompt| prompt.trim().to_string())
                .filter(|prompt| !prompt.is_empty());

            if cli.root_opts.task_id.is_some() {
                if effective_prompt.is_some() {
                    anyhow::bail!(
                        "Use --taskId without a prompt. To resume and add a message, use sned task <id> <prompt>."
                    );
                }
                return run_task(None, cli.task_opts, cli.root_opts);
            }

            match effective_prompt {
                Some(prompt) => run_task(Some(prompt), cli.task_opts, cli.root_opts),
                None => {
                    if should_start_interactive_shell(
                        false,
                        io::stdin().is_terminal(),
                        io::stdout().is_terminal(),
                        cli.task_opts.json,
                    ) {
                        run_interactive_shell(cli.task_opts, cli.root_opts)
                    } else {
                        if !cli.task_opts.json {
                            crate::cli::colors::eprint_error("no prompt provided on stdin");
                        }
                        Ok(())
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::sync::Mutex;
    use crate::providers::Provider;
    static PROVIDER_ENV_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn parse_task_subcommand() {
        let cli = Cli::try_parse_from(["sned", "task", "fix the bug", "--act"]).unwrap();
        match cli.command {
            Some(Command::Task { prompt, opts }) => {
                assert_eq!(prompt, "fix the bug");
                assert!(opts.act);
            }
            _ => panic!("expected Task command"),
        }
    }

    #[test]
    fn parse_task_alias() {
        let cli = Cli::try_parse_from(["sned", "t", "hello world"]).unwrap();
        match cli.command {
            Some(Command::Task { prompt, .. }) => {
                assert_eq!(prompt, "hello world");
            }
            _ => panic!("expected Task command via alias"),
        }
    }

    #[test]
    fn parse_history_subcommand() {
        let cli = Cli::try_parse_from(["sned", "history", "-n", "5", "-p", "2"]).unwrap();
        match cli.command {
            Some(Command::History { opts }) => {
                assert_eq!(opts.limit, 5);
                assert_eq!(opts.page, 2);
            }
            _ => panic!("expected History command"),
        }
    }

    #[test]
    fn parse_history_alias() {
        let cli = Cli::try_parse_from(["sned", "h"]).unwrap();
        match cli.command {
            Some(Command::History { .. }) => {}
            _ => panic!("expected History command via alias"),
        }
    }

    #[test]
    fn parse_history_favorites_only() {
        let cli = Cli::try_parse_from(["sned", "history", "--favorites-only"]).unwrap();
        match cli.command {
            Some(Command::History { opts }) => {
                assert!(opts.favorites_only);
            }
            _ => panic!("expected History command"),
        }
    }

    #[test]
    fn parse_history_workspace_only() {
        let cli = Cli::try_parse_from(["sned", "history", "--workspace-only"]).unwrap();
        match cli.command {
            Some(Command::History { opts }) => {
                assert!(opts.workspace_only);
            }
            _ => panic!("expected History command"),
        }
    }

    #[test]
    fn test_run_interactive_shell_exists() {
        let _ = run_interactive_shell as fn(TaskOptions, RootOnlyOptions) -> anyhow::Result<()>;
    }

    #[test]
    fn parse_history_search() {
        let cli = Cli::try_parse_from(["sned", "history", "-s", "auth"]).unwrap();
        match cli.command {
            Some(Command::History { opts }) => {
                assert_eq!(opts.search, Some("auth".to_string()));
            }
            _ => panic!("expected History command"),
        }
    }

    #[test]
    fn parse_history_sort() {
        let cli = Cli::try_parse_from(["sned", "history", "--sort", "alphabetical"]).unwrap();
        match cli.command {
            Some(Command::History { opts }) => {
                assert_eq!(opts.sort, "alphabetical");
            }
            _ => panic!("expected History command"),
        }
    }

    #[test]
    fn parse_config_subcommand() {
        let cli = Cli::try_parse_from(["sned", "config"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Config { .. })));
    }

    #[test]
    fn parse_config_set_subcommand() {
        let cli = Cli::try_parse_from(["sned", "config", "set", "mode=plan"]).unwrap();
        match cli.command {
            Some(Command::Config { opts }) => match opts.action {
                Some(ConfigAction::Set { assignment }) => {
                    assert_eq!(assignment, "mode=plan");
                }
                _ => panic!("expected config set action"),
            },
            _ => panic!("expected Config command"),
        }
    }

    #[test]
    fn parse_auth_subcommand() {
        let cli = Cli::try_parse_from([
            "sned",
            "auth",
            "-p",
            "anthropic",
            "-k",
            "sk-test",
            "-m",
            "claude-sonnet-4-6",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Auth { opts }) => {
                assert_eq!(opts.provider.as_deref(), Some("anthropic"));
                assert_eq!(opts.apikey.as_deref(), Some("sk-test"));
                assert_eq!(opts.modelid.as_deref(), Some("claude-sonnet-4-6"));
            }
            _ => panic!("expected Auth command"),
        }
    }

    #[test]
    fn parse_version_subcommand() {
        let cli = Cli::try_parse_from(["sned", "version"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Version)));
    }

    #[test]
    fn parse_kanban_as_prompt() {
        // Kanban subcommand is intentionally NOT implemented in native CLI.
        // "sned kanban" is parsed as a prompt, not a subcommand.
        let cli = Cli::try_parse_from(["sned", "kanban"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.prompt.as_deref(), Some("kanban"));
    }

    #[test]
    fn parse_dev_log_subcommand() {
        let cli = Cli::try_parse_from(["sned", "dev", "log"]).unwrap();
        match cli.command {
            Some(Command::Dev { subcmd }) => {
                assert!(matches!(subcmd, DevSubcommand::Log));
            }
            _ => panic!("expected Dev command"),
        }
    }

    #[test]
    fn cli_log_file_path_uses_dir_override() {
        let path = cli_log_file_path_from(
            Path::new("/tmp/sned-data"),
            Some(Path::new("/var/log/sned")),
        );
        assert_eq!(path, PathBuf::from("/var/log/sned/sned.1.log"));
    }

    #[test]
    fn cli_log_file_path_defaults_to_data_logs() {
        let path = cli_log_file_path_from(Path::new("/tmp/sned-data"), None);
        assert_eq!(path, PathBuf::from("/tmp/sned-data/logs/sned.1.log"));
    }

    #[test]
    fn parse_default_prompt() {
        let cli = Cli::try_parse_from(["sned", "fix the bug"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.prompt.as_deref(), Some("fix the bug"));
    }

    #[test]
    fn parse_root_continue_flag() {
        let cli = Cli::try_parse_from(["sned", "--continue"]).unwrap();
        assert!(cli.root_opts.continue_task);
    }

    #[test]
    fn parse_root_task_id_flag() {
        let cli = Cli::try_parse_from(["sned", "-T", "abc-123"]).unwrap();
        assert_eq!(cli.root_opts.task_id.as_deref(), Some("abc-123"));
    }

    #[test]
    fn parse_task_options_on_root() {
        let cli = Cli::try_parse_from([
            "sned",
            "--act",
            "--model",
            "gpt-4o",
            "--provider",
            "openai-native",
            "--json",
            "--auto-condense",
            "--thinking",
            "--reasoning-effort",
            "high",
        ])
        .unwrap();
        assert!(cli.task_opts.act);
        assert_eq!(cli.task_opts.model.as_deref(), Some("gpt-4o"));
        assert_eq!(cli.task_opts.provider.as_deref(), Some("openai-native"));
        assert!(cli.task_opts.json);
        assert!(cli.task_opts.auto_condense);
        assert!(cli.task_opts.thinking.is_some());
        assert_eq!(cli.task_opts.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn parse_no_token_display_flag() {
        let cli = Cli::try_parse_from(["sned", "--no-token-display", "test"]).unwrap();
        assert!(cli.task_opts.no_token_display);
    }

    #[test]
    fn parse_token_display_default() {
        let cli = Cli::try_parse_from(["sned", "test"]).unwrap();
        assert!(!cli.task_opts.no_token_display);
    }

    #[test]
    fn parse_max_tokens_flag() {
        let cli = Cli::try_parse_from(["sned", "--max-tokens", "2048", "test"]).unwrap();
        assert_eq!(cli.task_opts.max_tokens, Some(2048));
    }

    #[test]
    fn interactive_shell_requires_real_terminal_and_non_json_mode() {
        assert!(should_start_interactive_shell(false, true, true, false));
        assert!(!should_start_interactive_shell(true, true, true, false));
        assert!(!should_start_interactive_shell(false, false, true, false));
        assert!(!should_start_interactive_shell(false, true, false, false));
        assert!(!should_start_interactive_shell(false, true, true, true));
    }

    #[test]
    fn render_interactive_prompt_prefix_returns_chevron() {
        let prompt = render_interactive_prompt_prefix();
        assert!(prompt.contains("❯"));
    }

    #[test]
    fn render_interactive_prompt_prefix_no_model() {
        let prompt = render_interactive_prompt_prefix();
        assert!(prompt.contains("❯"));
    }

    #[test]
    fn render_interactive_prompt_prefix_no_turns() {
        let prompt = render_interactive_prompt_prefix();
        assert!(prompt.contains("❯"));
    }

    #[test]
    fn combine_prompt_with_piped_stdin_prefers_piped_input() {
        let prompt =
            combine_prompt_with_stdin(Some("prompt".to_string()), Some("stdin".to_string()))
                .unwrap();
        assert_eq!(prompt, "stdin\n\nprompt");
    }

    #[test]
    fn combine_prompt_with_empty_stdin_keeps_prompt() {
        let prompt =
            combine_prompt_with_stdin(Some("prompt".to_string()), Some(String::new())).unwrap();
        assert_eq!(prompt, "prompt");
    }

    #[test]
    fn combine_prompt_with_only_stdin_uses_stdin() {
        let prompt = combine_prompt_with_stdin(None, Some("stdin".to_string())).unwrap();
        assert_eq!(prompt, "stdin");
    }

    #[test]
    fn combine_prompt_with_no_input_returns_none() {
        assert!(combine_prompt_with_stdin(None, None).is_none());
    }

    #[test]
    fn parse_user_agent_option() {
        let cli = Cli::try_parse_from(["sned", "--user-agent", "my-custom-agent/1.0"]).unwrap();
        assert_eq!(
            cli.task_opts.user_agent.as_deref(),
            Some("my-custom-agent/1.0")
        );
    }

    #[test]
    fn parse_hooks_dir_option() {
        let cli = Cli::try_parse_from(["sned", "--hooks-dir", "/tmp/hooks"]).unwrap();
        assert_eq!(cli.task_opts.hooks_dir.as_deref(), Some("/tmp/hooks"));
    }

    #[test]
    fn parse_image_option_single() {
        let cli = Cli::try_parse_from(["sned", "--image", "/path/to/img.png"]).unwrap();
        assert_eq!(cli.task_opts.image, vec!["/path/to/img.png"]);
    }

    #[test]
    fn parse_image_option_multiple() {
        let cli = Cli::try_parse_from([
            "sned",
            "--image",
            "/path/to/img1.png",
            "--image",
            "/path/to/img2.jpg",
        ])
        .unwrap();
        assert_eq!(
            cli.task_opts.image,
            vec!["/path/to/img1.png", "/path/to/img2.jpg"]
        );
    }

    #[test]
    fn parse_image_option_short_flag() {
        let cli = Cli::try_parse_from(["sned", "-i", "/path/to/img.png"]).unwrap();
        assert_eq!(cli.task_opts.image, vec!["/path/to/img.png"]);
    }

    #[test]
    fn parse_config_option_on_root() {
        let cli = Cli::try_parse_from(["sned", "--config", "/custom/sned"]).unwrap();
        assert_eq!(cli.task_opts.config.as_deref(), Some("/custom/sned"));
    }

    #[test]
    fn test_prompt_context_values() {
        use crate::core::context::{PromptBuilder, SystemPromptContext};

        let context = SystemPromptContext {
            cwd: Some("/tmp/test".to_string()),
            active_shell_path: Some("/bin/zsh".to_string()),
            active_shell_type: Some("zsh".to_string()),
            active_shell_is_posix: true,
            available_cores: Some(8),
            enable_parallel_tool_calling: true,
            ..Default::default()
        };

        let prompt = PromptBuilder::new(context).build();

        assert!(
            prompt.contains("You are Sned"),
            "Prompt should contain 'You are Sned'"
        );
        assert!(
            prompt.contains("PRIME DIRECTIVES"),
            "Prompt should contain 'PRIME DIRECTIVES'"
        );
        // Environment info (CWD, shell, CPU) is now provided by context_loader, not in system prompt
        assert!(
            !prompt.contains("Current Working Directory:"),
            "System prompt should NOT contain CWD (moved to context_loader)"
        );
        assert!(
            !prompt.contains("Default Shell:"),
            "System prompt should NOT contain shell path (moved to context_loader)"
        );
        assert!(
            !prompt.contains("Available CPU Cores:"),
            "System prompt should NOT contain CPU cores (moved to context_loader)"
        );
    }

    #[test]
    fn test_rules_in_prompt() {
        use crate::core::context::{PromptBuilder, SystemPromptContext};

        let context = SystemPromptContext {
            cwd: Some("/workspace".to_string()),
            local_agents_rules_file_instructions: Some(
                "# AGENTS.md Rules\n\nAlways write tests.".to_string(),
            ),
            local_cursor_rules_file_instructions: Some(
                "# Cursor Rules\n\nUse idiomatic Rust.".to_string(),
            ),
            local_windsurf_rules_file_instructions: Some(
                "# Windsurf Rules\n\nFormat with rustfmt.".to_string(),
            ),
            ..Default::default()
        };

        let prompt = PromptBuilder::new(context).build();

        // Verify custom instructions section exists when rules are present
        assert!(
            prompt.contains("USER'S CUSTOM INSTRUCTIONS"),
            "Prompt should contain custom instructions section"
        );

        // Verify all rule sources are included
        assert!(
            prompt.contains("Always write tests."),
            "Prompt should contain agents rules"
        );
        assert!(
            prompt.contains("Use idiomatic Rust."),
            "Prompt should contain cursor rules"
        );
        assert!(
            prompt.contains("Format with rustfmt."),
            "Prompt should contain windsurf rules"
        );
    }

    #[test]
    fn test_rules_discovery_from_filesystem() {
        use crate::core::context::instructions::{
            RuleToggles, get_local_agents_rules, get_local_cursor_rules, get_local_windsurf_rules,
        };
        use std::io::Write;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let cwd = temp_dir.path();

        let mut agents_file = std::fs::File::create(cwd.join("AGENTS.md")).unwrap();
        writeln!(
            agents_file,
            "# Project Rules\n\nAlways document public APIs."
        )
        .unwrap();

        let mut cursor_file = std::fs::File::create(cwd.join(".cursorrules")).unwrap();
        writeln!(cursor_file, "Use snake_case for variables.").unwrap();

        let mut windsurf_file = std::fs::File::create(cwd.join(".windsurfrules")).unwrap();
        writeln!(windsurf_file, "Max line length: 100.").unwrap();

        let cursor_rules_dir = cwd.join(".cursor/rules");
        std::fs::create_dir_all(&cursor_rules_dir).unwrap();
        let mut cursor_dir_file = std::fs::File::create(cursor_rules_dir.join("rust.mdc")).unwrap();
        writeln!(cursor_dir_file, "Prefer Result over panic.").unwrap();

        let empty_toggles = RuleToggles::new();

        let agents_rules = get_local_agents_rules(cwd, &empty_toggles);
        assert!(agents_rules.is_some(), "Should discover AGENTS.md");
        let agents = agents_rules.unwrap();
        assert!(
            agents.contains("Always document public APIs."),
            "Should contain agents rules content"
        );

        let cursor_rules = get_local_cursor_rules(cwd, &empty_toggles);
        assert_eq!(
            cursor_rules.len(),
            2,
            "Should discover .cursorrules and .cursor/rules/"
        );
        assert!(
            cursor_rules[0].as_ref().unwrap().contains("snake_case"),
            "Should contain cursor file rules"
        );
        assert!(
            cursor_rules[1].as_ref().unwrap().contains("Prefer Result"),
            "Should contain cursor dir rules"
        );

        let windsurf_rules = get_local_windsurf_rules(cwd, &empty_toggles);
        assert!(windsurf_rules.is_some(), "Should discover .windsurfrules");
        let windsurf = windsurf_rules.unwrap();
        assert!(
            windsurf.contains("Max line length"),
            "Should contain windsurf rules content"
        );
    }

    #[test]
    fn test_disabled_rules_excluded() {
        use crate::core::context::instructions::{RuleToggles, get_local_agents_rules};
        use std::io::Write;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let cwd = temp_dir.path();

        let mut agents_file = std::fs::File::create(cwd.join("AGENTS.md")).unwrap();
        writeln!(agents_file, "# Project Rules\n\nAlways document.").unwrap();

        let mut toggles = RuleToggles::new();
        let agents_path = cwd.join("AGENTS.md").to_string_lossy().into_owned();
        toggles.insert(agents_path, false);

        let agents_rules = get_local_agents_rules(cwd, &toggles);
        assert!(agents_rules.is_none(), "Disabled rules should be excluded");
    }

    #[test]
    fn test_format_config_output() {
        use crate::storage::global_state::GlobalState;

        let state = GlobalState::default();
        let output = format_config_output(&state);

        assert!(output.contains("Current Sned Configuration"));
        assert!(output.contains("Mode & Provider"));
        assert!(output.contains("Auto-Approve"));
        assert!(output.contains("Context"));
    }

    #[test]
    fn test_tracing_init_with_verbose_flag() {
        let cli = Cli::try_parse_from(["sned", "test prompt"]).unwrap();
        assert!(!cli.task_opts.verbose, "Default should be non-verbose");

        let cli_verbose = Cli::try_parse_from(["sned", "--verbose", "test prompt"]).unwrap();
        assert!(
            cli_verbose.task_opts.verbose,
            "--verbose flag should be set"
        );
    }

    #[test]
    fn test_parse_symbol_index_mode_value() {
        assert_eq!(
            parse_symbol_index_mode_value("off").unwrap(),
            SymbolIndexMode::Off
        );
        assert_eq!(
            parse_symbol_index_mode_value("memory").unwrap(),
            SymbolIndexMode::Memory
        );
        assert_eq!(
            parse_symbol_index_mode_value("persisted").unwrap(),
            SymbolIndexMode::Persisted
        );
        assert_eq!(
            parse_symbol_index_mode_value(" PERSISTED ").unwrap(),
            SymbolIndexMode::Persisted
        );

        let err = parse_symbol_index_mode_value("sqlite").unwrap_err();
        assert!(err.to_string().contains("Invalid SNED_SYMBOL_INDEX value"));
        assert!(err.to_string().contains("off, memory, persisted"));
    }

    #[test]
    fn test_build_symbol_index_service_fallback_on_corrupted_db() {
        use std::fs;
        use std::io::Write;

        let temp_dir = "/tmp/test_cli_symbol_fallback";
        let _ = fs::remove_dir_all(temp_dir);
        fs::create_dir_all(temp_dir).unwrap();

        // build_symbol_index_service now stores the DB at
        // /tmp/sned-symbol-index-{hash}/{INDEX_DIR}/data.db. Compute the same
        // hash the service uses, then create a corrupted DB at that path.
        let index_root = format!("/tmp/sned-symbol-index-{}", hash_cwd(temp_dir));
        let db_dir = std::path::Path::new(&index_root)
            .join(crate::services::symbol_index::INDEX_DIR);
        fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join(crate::services::symbol_index::DB_FILENAME);
        {
            let mut f = fs::File::create(&db_path).unwrap();
            f.write_all(b"This is not a valid SQLite database file")
                .unwrap();
        }

        let service = build_symbol_index_service(temp_dir.to_string(), SymbolIndexMode::Persisted);

        // Verify service is functional (not disabled)
        assert!(
            !service.is_disabled(),
            "Service should be functional after fallback"
        );

        // Verify corrupted DB file was deleted
        assert!(
            !db_path.exists(),
            "Corrupted DB file should be deleted after fallback"
        );

        // Clean up
        let _ = fs::remove_dir_all(temp_dir);
        let _ = fs::remove_dir_all(&index_root);
    }

    #[test]
    fn test_hash_cwd_is_deterministic_across_calls() {
        // SHA-256 must produce the same hash for the same input regardless
        // of when it's called. This guards against accidental regression to
        // a randomized hasher (e.g. DefaultHasher) which would defeat
        // symbol-index persistence across process restarts.
        let cwd = "/Users/easto/projects/dirac-fork";
        let h1 = hash_cwd(cwd);
        let h2 = hash_cwd(cwd);
        assert_eq!(h1, h2, "hash_cwd must be deterministic");
        assert_eq!(h1.len(), 16, "hash output must be 16 hex chars");
    }

    #[test]
    fn test_hash_cwd_different_inputs_produce_different_hashes() {
        let h1 = hash_cwd("/path/one");
        let h2 = hash_cwd("/path/two");
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_provider_auto_detection() {
        use crate::providers::env_auth::get_provider_from_env;
        use std::env;

        // Helper to clear test env vars
        fn clear_env() {
            for var in &[
                "ANTHROPIC_API_KEY",
                "OPENAI_API_KEY",
                "GEMINI_API_KEY",
                "OPENROUTER_API_KEY",
                "DEEPSEEK_API_KEY",
                "MINIMAX_API_KEY",
                "MINIMAX_CN_API_KEY",
                "MISTRAL_API_KEY",
                "MOONSHOT_API_KEY",
                "ZAI_API_KEY",
                "QWEN_API_KEY",
                "TOGETHER_API_KEY",
                "FIREWORKS_API_KEY",
                "NEBIUS_API_KEY",
                "CEREBRAS_API_KEY",
                "HF_TOKEN",
                "OPENCODE_API_KEY",
                "KIMI_API_KEY",
                "AI_GATEWAY_API_KEY",
                "AWS_ACCESS_KEY_ID",
                "AWS_BEDROCK_MODEL",
                "GOOGLE_CLOUD_PROJECT",
                "GCP_PROJECT",
            ] {
                // SAFETY: single-threaded test; clearing env before each assertion
                unsafe { env::remove_var(var) };
            }
        }

        clear_env();
        assert_eq!(get_provider_from_env(), None);

        clear_env();
        // SAFETY: single-threaded test; sequential env mutation
        unsafe { env::set_var("ANTHROPIC_API_KEY", "test-key") };
        assert_eq!(get_provider_from_env(), Some("anthropic"));

        clear_env();
        // SAFETY: single-threaded test; sequential env mutation
        unsafe { env::set_var("OPENAI_API_KEY", "test-key") };
        assert_eq!(get_provider_from_env(), Some("openai-native"));

        clear_env();
        // SAFETY: single-threaded test; sequential env mutation
        unsafe {
            env::set_var("ANTHROPIC_API_KEY", "ant-key");
            env::set_var("OPENAI_API_KEY", "openai-key");
        }
        assert_eq!(get_provider_from_env(), Some("anthropic"));

        clear_env();
        // SAFETY: single-threaded test; sequential env mutation
        unsafe { env::set_var("OPENROUTER_API_KEY", "or-key") };
        assert_eq!(get_provider_from_env(), Some("openrouter"));

        clear_env();
        // SAFETY: single-threaded test; sequential env mutation
        unsafe { env::set_var("DEEPSEEK_API_KEY", "ds-key") };
        assert_eq!(get_provider_from_env(), Some("deepseek"));

        clear_env();
    }

    #[test]
    fn test_create_provider_auto_detects_anthropic() {
        use std::env;
        {
            let _guard = PROVIDER_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let all_provider_vars = vec![
                "ANTHROPIC_API_KEY",
                "OPENAI_API_KEY",
                "GEMINI_API_KEY",
                "OPENROUTER_API_KEY",
                "DEEPSEEK_API_KEY",
                "QWEN_API_KEY",
                "MINIMAX_API_KEY",
                "MINIMAX_CN_API_KEY",
                "MISTRAL_API_KEY",
                "MOONSHOT_API_KEY",
                "HF_TOKEN",
                "ZAI_API_KEY",
                "CEREBRAS_API_KEY",
                "AI_GATEWAY_API_KEY",
                "TOGETHER_API_KEY",
                "FIREWORKS_API_KEY",
                "NEBIUS_API_KEY",
                "AWS_ACCESS_KEY_ID",
                "AWS_SECRET_ACCESS_KEY",
                "AWS_SESSION_TOKEN",
                "AWS_BEDROCK_MODEL",
                "AWS_BEDROCK_MODEL_ACT",
                "AWS_BEDROCK_MODEL_PLAN",
                "GOOGLE_CLOUD_PROJECT",
                "GCP_PROJECT",
                "GOOGLE_CLOUD_LOCATION",
                "GOOGLE_CLOUD_REGION",
                "OPENAI_API_BASE",
                "OPENCODE_API_KEY",
                "KIMI_API_KEY",
                "OPENAI_COMPATIBLE_CUSTOM_KEY",
            ];
            for var in &all_provider_vars {
                // SAFETY: clearing under mutex lock
                unsafe { env::remove_var(var) };
            }
            // SAFETY: setting under mutex lock
            unsafe { env::set_var("ANTHROPIC_API_KEY", "test-key") };
            let task_opts = TaskOptions {
                act: false,
                plan: false,
                yolo: false,
                auto_approve_all: false,
                timeout: None,
                model: None,
                provider: None,
                base_url: None,
                api_key: None,
                verbose: false,
                cwd: None,
                config: None,
                thinking: None,
                reasoning_effort: None,
                max_consecutive_mistakes: None,
                json: false,
                double_check_completion: false,
                auto_condense: true,
                no_token_display: false,
                subagents: false,
                is_subagent: false,
                user_agent: None,
                hooks_dir: None,
                export: None,
                image: vec![],
                track_changes: false,
                max_context_turns: None,
                max_tokens: None,
                debug: false,
            };
            let result = create_provider(&task_opts);
            assert!(result.is_ok(), "Expected Ok when ANTHROPIC_API_KEY is set");
            // SAFETY: cleanup under mutex lock
            unsafe { env::remove_var("ANTHROPIC_API_KEY") };
        }
    }

    #[test]
    fn test_create_provider_anthropic_bails_when_key_unset() {
        use std::env;
        {
            let _guard = PROVIDER_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let all_provider_vars = vec![
                "ANTHROPIC_API_KEY",
                "OPENAI_API_KEY",
                "GEMINI_API_KEY",
                "OPENROUTER_API_KEY",
                "DEEPSEEK_API_KEY",
                "QWEN_API_KEY",
                "MINIMAX_API_KEY",
                "MINIMAX_CN_API_KEY",
                "MISTRAL_API_KEY",
                "MOONSHOT_API_KEY",
                "HF_TOKEN",
                "ZAI_API_KEY",
                "CEREBRAS_API_KEY",
                "AI_GATEWAY_API_KEY",
                "TOGETHER_API_KEY",
                "FIREWORKS_API_KEY",
                "NEBIUS_API_KEY",
                "OPENAI_API_BASE",
                "OPENCODE_API_KEY",
                "KIMI_API_KEY",
                "OPENAI_COMPATIBLE_CUSTOM_KEY",
            ];
            for var in &all_provider_vars {
                // SAFETY: clearing under mutex lock
                unsafe { env::remove_var(var) };
            }
            let task_opts = TaskOptions {
                act: false,
                plan: false,
                yolo: false,
                auto_approve_all: false,
                timeout: None,
                model: None,
                provider: Some("anthropic".to_string()),
                base_url: None,
                api_key: None,
                verbose: false,
                cwd: None,
                config: None,
                thinking: None,
                reasoning_effort: None,
                max_consecutive_mistakes: None,
                json: false,
                double_check_completion: false,
                auto_condense: true,
                no_token_display: false,
                subagents: false,
                is_subagent: false,
                user_agent: None,
                hooks_dir: None,
                export: None,
                image: vec![],
                track_changes: false,
                max_context_turns: None,
                max_tokens: None,
                debug: false,
            };
            let result = create_provider(&task_opts);
            assert!(
                result.is_err(),
                "Expected Err when ANTHROPIC_API_KEY is unset (not a dummy-key Ok)"
            );
            let err = result.err().unwrap();
            let msg = format!("{}", err);
            assert!(
                msg.contains("ANTHROPIC_API_KEY"),
                "Error should mention ANTHROPIC_API_KEY, got: {}",
                msg
            );
        }
    }

    #[test]
    fn test_create_provider_explicit_flag_takes_precedence() {
        use std::env;
        {
            let _guard = PROVIDER_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let all_provider_vars = vec![
                "ANTHROPIC_API_KEY",
                "OPENAI_API_KEY",
                "GEMINI_API_KEY",
                "OPENROUTER_API_KEY",
                "DEEPSEEK_API_KEY",
                "QWEN_API_KEY",
                "MINIMAX_API_KEY",
                "MINIMAX_CN_API_KEY",
                "MISTRAL_API_KEY",
                "MOONSHOT_API_KEY",
                "HF_TOKEN",
                "ZAI_API_KEY",
                "CEREBRAS_API_KEY",
                "AI_GATEWAY_API_KEY",
                "TOGETHER_API_KEY",
                "FIREWORKS_API_KEY",
                "NEBIUS_API_KEY",
                "AWS_ACCESS_KEY_ID",
                "AWS_SECRET_ACCESS_KEY",
                "AWS_SESSION_TOKEN",
                "AWS_BEDROCK_MODEL",
                "AWS_BEDROCK_MODEL_ACT",
                "AWS_BEDROCK_MODEL_PLAN",
                "GOOGLE_CLOUD_PROJECT",
                "GCP_PROJECT",
                "GOOGLE_CLOUD_LOCATION",
                "GOOGLE_CLOUD_REGION",
                "OPENAI_API_BASE",
                "OPENCODE_API_KEY",
                "KIMI_API_KEY",
                "OPENAI_COMPATIBLE_CUSTOM_KEY",
            ];
            for var in &all_provider_vars {
                unsafe { env::remove_var(var) };
            }
            unsafe { env::set_var("ANTHROPIC_API_KEY", "ant-key") };
            let task_opts = TaskOptions {
                act: false,
                plan: false,
                yolo: false,
                auto_approve_all: false,
                timeout: None,
                model: None,
                provider: Some("deepseek".to_string()),
                base_url: None,
                api_key: Some("deepseek-key".to_string()),
                verbose: false,
                cwd: None,
                config: None,
                thinking: None,
                reasoning_effort: None,
                max_consecutive_mistakes: None,
                json: false,
                double_check_completion: false,
                auto_condense: true,
                no_token_display: false,
                subagents: false,
                is_subagent: false,
                user_agent: None,
                hooks_dir: None,
                export: None,
                image: vec![],
                track_changes: false,
                max_context_turns: None,
                max_tokens: None,
                debug: false,
            };
            let result = create_provider(&task_opts);
            assert!(result.is_ok(), "Expected Ok with explicit provider");
            unsafe { env::remove_var("ANTHROPIC_API_KEY") };
        }
    }

    #[test]
    fn test_create_provider_returns_typed_config_error_for_unsupported_provider() {
        let cli = Cli::try_parse_from(["sned", "--provider", "nope", "test prompt"]).unwrap();
        let err = match create_provider(&cli.task_opts) {
            Ok(_) => panic!("unsupported provider should fail"),
            Err(err) => err,
        };

        let cli_err = err
            .downcast_ref::<crate::error::CliError>()
            .expect("unsupported provider should be a typed CliError");
        assert!(matches!(
            cli_err,
            crate::error::CliError::Config(message) if message == "Unsupported provider: nope"
        ));
    }

    #[test]
    fn test_create_provider_custom_base_url_and_api_key() {
        let cli = Cli::try_parse_from([
            "sned",
            "--base-url",
            "https://custom.example.com/v1",
            "--api-key",
            "sk-test123",
            "--model",
            "custom-model",
            "test prompt",
        ])
        .unwrap();
        match create_provider(&cli.task_opts) {
            Ok(provider) => {
                assert_eq!(provider.name(), "openai");
            }
            Err(err) => panic!("custom base_url+api_key should work: {}", err),
        }
    }

    // Note: OPENAI_API_BASE env var detection tested in providers::env_auth::tests

    #[test]
    fn test_utf8_cursor_movement() {
        // Simulate the cursor tracking logic to ensure UTF-8 chars work
        let mut buf = String::new();
        let mut cursor: usize = 0;

        // Insert emoji (4 bytes)
        buf.insert(cursor, '🦀');
        cursor += '🦀'.len_utf8();
        assert_eq!(cursor, 4);
        assert_eq!(buf, "🦀");

        // Insert ASCII
        buf.insert(cursor, 'x');
        cursor += 'x'.len_utf8();
        assert_eq!(cursor, 5);
        assert_eq!(buf, "🦀x");

        // Move left (should jump 4 bytes back)
        cursor = buf[..cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        assert_eq!(cursor, 4);

        // Move left again (to start)
        cursor = buf[..cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        assert_eq!(cursor, 0);

        // Move right (should jump to 4)
        if let Some((i, ch)) = buf[cursor..].char_indices().next() {
            cursor = cursor + i + ch.len_utf8();
        }
        assert_eq!(cursor, 4);

        // Backspace at position 4 should remove emoji
        let prev_pos = buf[..cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        buf.remove(prev_pos);
        cursor = prev_pos;
        assert_eq!(cursor, 0);
        assert_eq!(buf, "x");
    }

    #[test]
    fn json_mode_uses_json_only_tracing() {
        assert_eq!(tracing_mode(true, false), TracingMode::JsonOnly);
        assert_eq!(tracing_mode(true, true), TracingMode::JsonOnly);
        assert_eq!(
            tracing_mode(false, true),
            TracingMode::Human { verbose: true }
        );
    }
}
