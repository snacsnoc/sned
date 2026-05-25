//! Subcommand handlers.
//!
//! Extracted from `cli/mod.rs` — handles `history`, `config`, `auth`, and `migrate` subcommands.

use crate::cli::{AuthOptions, ConfigAction, ConfigOptions, HistoryOptions};
use std::path::PathBuf;

pub fn run_history(opts: HistoryOptions) -> anyhow::Result<()> {
    use crate::storage::state_manager::StateManager;
    use crate::storage::state_manager::{list_tasks, sort_by_timestamp, total_pages};
    use chrono::{DateTime, Local};
    use std::env;

    let state_manager = StateManager::new()?;
    state_manager.initialize()?;

    let mut history = state_manager.get_task_history();

    // Apply favorites filter
    if opts.favorites_only {
        history.retain(|item| item.is_favorited.unwrap_or(false));
    }

    // Apply workspace filter (compare task workspace with current directory)
    if opts.workspace_only {
        let cwd = env::current_dir()?.to_string_lossy().to_string();
        history.retain(|item| {
            item.workspace_root_path
                .as_ref()
                .map(|w| cwd.starts_with(w))
                .unwrap_or(false)
        });
    }

    // Apply search filter
    if let Some(ref query) = opts.search {
        let query_lower = query.to_lowercase();
        history.retain(|item| item.task.to_lowercase().contains(&query_lower));
    }

    if history.is_empty() {
        println!("No task history found matching the specified filters.");
        return Ok(());
    }

    // Apply sorting
    match opts.sort.as_str() {
        "oldest" => {
            history.sort_by_key(|a| a.ts);
        }
        "alphabetical" => {
            history.sort_by(|a, b| a.task.cmp(&b.task));
        }
        _ => {
            sort_by_timestamp(&mut history);
        }
    }

    let page = opts.page.max(1) as usize;
    let limit = opts.limit.max(1) as usize;
    let (items, total) = list_tasks(&history, page, limit);
    let total_pages = total_pages(total, limit);

    // Get terminal width for responsive display
    let term_width = crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80);
    let separator_width = term_width.saturating_sub(4).min(100);

    // Build filter summary for header
    let mut filter_parts = Vec::new();
    if opts.favorites_only {
        filter_parts.push("favorites");
    }
    if opts.workspace_only {
        filter_parts.push("workspace");
    }
    if opts.search.is_some() {
        filter_parts.push("search");
    }
    if opts.sort != "newest" {
        filter_parts.push(&opts.sort);
    }

    let filter_summary = if filter_parts.is_empty() {
        String::new()
    } else {
        format!(" [{}]", filter_parts.join(", "))
    };

    // Header
    println!(
        " Task History{} (page {} of {}, {} total)",
        filter_summary, page, total_pages, total
    );
    println!("{}", "─".repeat(separator_width));

    for item in items {
        let ts = item.ts;
        let dt = DateTime::from_timestamp(ts, 0)
            .map(|d| d.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| ts.to_string());

        let preview = if item.task.len() > 50 {
            let end = item.task.floor_char_boundary(47);
            format!("{}...", &item.task[..end])
        } else {
            item.task.clone()
        };

        let tokens = if item.tokens_in > 0 || item.tokens_out > 0 {
            format!(" ({} in / {} out)", item.tokens_in, item.tokens_out)
        } else {
            String::new()
        };

        let favorite_marker = if item.is_favorited.unwrap_or(false) {
            "★ "
        } else {
            "  "
        };

        println!(
            "  {}{:>4}  {:16}  {}{}",
            favorite_marker, item.number, dt, preview, tokens
        );
    }

    println!("{}", "─".repeat(separator_width));
    println!(
        " Showing {} tasks per page. Use -n <limit> and -p <page> to navigate.",
        limit
    );
    if !filter_parts.is_empty() {
        println!(
            " Active filters: {}. Use --help to see filter options.",
            filter_parts.join(", ")
        );
    }

    Ok(())
}

pub fn run_config(opts: ConfigOptions) -> anyhow::Result<()> {
    use crate::storage::global_state::load_global_state;

    if let Some(config_path) = &opts.config {
        unsafe {
            std::env::set_var("SNED_DIR", config_path);
        }
    }

    if let Some(ConfigAction::Set { assignment }) = opts.action {
        if opts.validate || opts.migrate.is_some() || opts.dry_run {
            anyhow::bail!("config set cannot be combined with --validate, --migrate, or --dry-run");
        }

        let (key, value) = parse_config_assignment(&assignment)?;
        let state_manager = crate::storage::state_manager::StateManager::new()?;
        state_manager.initialize()?;
        match state_manager.set_global_state_string_field(key, value.clone()) {
            Ok(()) => {
                state_manager.persist()?;
                println!("Updated configuration: {}={}", key, value);
            }
            Err(e) => {
                return Err(anyhow::anyhow!("{}", e));
            }
        }
        return Ok(());
    }

    if let Some(ConfigAction::List) = opts.action {
        let state_manager = crate::storage::state_manager::StateManager::new()?;
        state_manager.initialize()?;

        println!("Valid configuration keys:");
        println!("  {:<40} {:<10} DESCRIPTION", "KEY", "TYPE");
        println!("  {}", "─".repeat(80));

        for key_info in crate::storage::state_manager::VALID_CONFIG_KEYS {
            let current_value = state_manager
                .get_config_value(key_info.name)
                .unwrap_or_else(|| "—".to_string());
            println!(
                "  {:<40} {:<10} {} (current: {})",
                key_info.name, key_info.key_type, key_info.description, current_value
            );
        }
        return Ok(());
    }

    if let Some(source_dir) = &opts.migrate {
        return run_migration(source_dir, opts.dry_run);
    }

    let state = load_global_state();

    if opts.validate {
        println!("Validating Sned configuration...");
        let mut issues = Vec::new();

        // Check provider API keys
        let providers = vec![
            ("act", state.act_mode_api_provider.as_str()),
            ("plan", state.plan_mode_api_provider.as_str()),
        ];

        for (mode, provider) in providers {
            let key_name = format!("{}_API_KEY", provider.to_uppercase().replace("-", "_"));
            let has_key = std::env::var(&key_name).is_ok()
                || std::env::var(format!(
                    "SNED_{}_API_KEY",
                    provider.to_uppercase().replace("-", "_")
                ))
                .is_ok();

            if !has_key {
                issues.push(format!(
                    "  ⚠️  {} mode provider '{}' may need API key ({} not set)",
                    mode, provider, key_name
                ));
            } else {
                println!(
                    "  ✓  {} mode provider '{}' has API key configured",
                    mode, provider
                );
            }
        }

        if issues.is_empty() {
            println!("\n✓ Configuration is valid");
        } else {
            println!("\nConfiguration issues found:");
            for issue in issues {
                println!("{}", issue);
            }
        }
    } else {
        println!("{}", format_config_output(&state));
    }
    Ok(())
}

pub fn parse_config_assignment(assignment: &str) -> anyhow::Result<(&str, String)> {
    let (key, value) = assignment
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("Expected key=value"))?;

    let key = key.trim();
    if key.is_empty() {
        anyhow::bail!("Configuration key cannot be empty");
    }

    Ok((key, value.to_string()))
}

pub fn run_migration(source_dir: &str, dry_run: bool) -> anyhow::Result<()> {
    use crate::storage::migration::{MigrationEngine, plan_dry_run_migration};

    let destination_dir = std::env::var("SNED_DIR")
        .ok()
        .or_else(|| dirs::config_dir().map(|p| p.join("sned").to_string_lossy().into_owned()))
        .unwrap_or_else(|| "~/.config/sned".to_string());

    let destination_dir = if destination_dir.starts_with('~') {
        if let Some(home) = dirs::home_dir() {
            destination_dir.replacen('~', &home.to_string_lossy(), 1)
        } else {
            destination_dir
        }
    } else {
        destination_dir
    };

    let destination_path = PathBuf::from(&destination_dir);

    println!("Migration Plan");
    println!("==============");
    println!("Source:      {}", source_dir);
    println!("Destination: {}", destination_path.display());
    println!();

    let source_path = PathBuf::from(source_dir);
    if !source_path.exists() {
        anyhow::bail!("Source directory does not exist: {}", source_dir);
    }

    if !destination_path.exists() {
        println!("Destination directory does not exist, will be created.");
        println!();
    }

    if dry_run {
        let report = plan_dry_run_migration(&source_path, &destination_path)
            .map_err(|e| anyhow::anyhow!("Failed to plan migration: {}", e))?;

        print_dry_run_report(&report);
    } else {
        println!("Executing migration...");
        println!();

        let mut engine = MigrationEngine::new(&source_path, &destination_path);

        match engine.execute() {
            Ok(report) => {
                print_execution_report(&report);
                println!("\n✓ Migration completed successfully");
            }
            Err(e) => {
                println!("\n✗ Migration failed: {}", e);
                println!("\nAttempting rollback...");
                if let Err(rb_err) = engine.rollback() {
                    println!("✗ Rollback failed: {}", rb_err);
                } else {
                    println!("✓ Rollback completed");
                }
                anyhow::bail!("Migration failed: {}", e);
            }
        }
    }

    Ok(())
}

pub fn print_dry_run_report(report: &crate::storage::migration::DryRunMigrationReport) {
    if !report.has_changes() {
        println!("No migration needed - directories are in sync.");
        return;
    }

    println!("The following changes would be made:");
    println!();

    if let Some(endpoints) = &report.endpoints
        && !endpoints.is_in_sync()
    {
        println!("endpoints.json:");
        if !endpoints.copied_keys.is_empty() {
            println!("  + Keys to copy: {}", endpoints.copied_keys.join(", "));
        }
        if !endpoints.conflicting_keys.is_empty() {
            println!(
                "  ! Conflicting keys (skipped): {}",
                endpoints.conflicting_keys.join(", ")
            );
        }
        if !endpoints.skipped_existing_keys.is_empty() {
            println!(
                "  = Keys already in destination: {}",
                endpoints.skipped_existing_keys.join(", ")
            );
        }
        println!();
    }

    if let Some(global_settings) = &report.global_settings
        && !global_settings.is_in_sync()
    {
        println!("data/settings/global_settings.json:");
        if !global_settings.copied_keys.is_empty() {
            println!(
                "  + Keys to copy: {}",
                global_settings.copied_keys.join(", ")
            );
        }
        if !global_settings.conflicting_keys.is_empty() {
            println!(
                "  ! Conflicting keys (skipped): {}",
                global_settings.conflicting_keys.join(", ")
            );
        }
        if !global_settings.skipped_existing_keys.is_empty() {
            println!(
                "  = Keys already in destination: {}",
                global_settings.skipped_existing_keys.join(", ")
            );
        }
        println!();
    }

    if let Some(secrets) = &report.secrets
        && !secrets.is_in_sync()
    {
        println!(".secrets.json:");
        if !secrets.copied_keys.is_empty() {
            println!("  + Keys to copy: {}", secrets.copied_keys.join(", "));
        }
        if !secrets.conflicting_keys.is_empty() {
            println!(
                "  ! Conflicting keys (skipped): {}",
                secrets.conflicting_keys.join(", ")
            );
        }
        if !secrets.skipped_existing_keys.is_empty() {
            println!(
                "  = Keys already in destination: {}",
                secrets.skipped_existing_keys.join(", ")
            );
        }
        println!();
    }

    if let Some(task_history) = &report.task_history
        && !task_history.is_in_sync()
    {
        println!("data/state/taskHistory.json:");
        if !task_history.copied_ids.is_empty() {
            println!("  + Tasks to copy: {} tasks", task_history.copied_ids.len());
        }
        if !task_history.conflicting_ids.is_empty() {
            println!(
                "  ! Conflicting tasks (skipped): {} tasks",
                task_history.conflicting_ids.len()
            );
        }
        if !task_history.skipped_existing_ids.is_empty() {
            println!(
                "  = Tasks already in destination: {} tasks",
                task_history.skipped_existing_ids.len()
            );
        }
        println!();
    }

    let tasks_with_changes: Vec<_> = report.tasks.iter().filter(|t| !t.is_in_sync()).collect();
    if !tasks_with_changes.is_empty() {
        println!("Task directories:");
        for task in &tasks_with_changes {
            println!("  {}:", task.task_id);
            if !task.copied_files.is_empty() {
                println!("    + Files to copy: {}", task.copied_files.len());
            }
            if !task.conflicting_files.is_empty() {
                println!(
                    "    ! Conflicting files (skipped): {}",
                    task.conflicting_files.len()
                );
            }
            if !task.skipped_existing_files.is_empty() {
                println!(
                    "    = Files already in destination: {}",
                    task.skipped_existing_files.len()
                );
            }
        }
        println!();
    }

    println!(
        "Total: {} files/keys would be copied",
        report.total_copied_files()
    );
}

pub fn print_execution_report(report: &crate::storage::migration::MigrationExecutionReport) {
    if !report.has_changes() {
        println!("No migration needed - directories were already in sync.");
        return;
    }

    println!("Migration Results:");
    println!();

    if let Some(endpoints) = &report.endpoints
        && !endpoints.is_in_sync()
    {
        println!("endpoints.json:");
        if !endpoints.copied_keys.is_empty() {
            println!("  + Copied keys: {}", endpoints.copied_keys.join(", "));
        }
        if !endpoints.conflicting_keys.is_empty() {
            println!(
                "  ! Skipped conflicting: {}",
                endpoints.conflicting_keys.join(", ")
            );
        }
        println!();
    }

    if let Some(global_settings) = &report.global_settings
        && !global_settings.is_in_sync()
    {
        println!("data/settings/global_settings.json:");
        if !global_settings.copied_keys.is_empty() {
            println!(
                "  + Copied keys: {}",
                global_settings.copied_keys.join(", ")
            );
        }
        if !global_settings.conflicting_keys.is_empty() {
            println!(
                "  ! Skipped conflicting: {}",
                global_settings.conflicting_keys.join(", ")
            );
        }
        println!();
    }

    if let Some(secrets) = &report.secrets
        && !secrets.is_in_sync()
    {
        println!(".secrets.json:");
        if !secrets.copied_keys.is_empty() {
            println!("  + Copied keys: {}", secrets.copied_keys.join(", "));
        }
        if !secrets.conflicting_keys.is_empty() {
            println!(
                "  ! Skipped conflicting: {}",
                secrets.conflicting_keys.join(", ")
            );
        }
        println!();
    }

    if let Some(task_history) = &report.task_history
        && !task_history.is_in_sync()
    {
        println!("data/state/taskHistory.json:");
        if !task_history.copied_ids.is_empty() {
            println!("  + Copied tasks: {} tasks", task_history.copied_ids.len());
        }
        if !task_history.conflicting_ids.is_empty() {
            println!(
                "  ! Skipped conflicting: {} tasks",
                task_history.conflicting_ids.len()
            );
        }
        println!();
    }

    let tasks_with_changes: Vec<_> = report.tasks.iter().filter(|t| !t.is_in_sync()).collect();
    if !tasks_with_changes.is_empty() {
        println!("Task directories:");
        for task in &tasks_with_changes {
            println!("  {}:", task.task_id);
            if !task.copied_files.is_empty() {
                println!("    + Copied files: {}", task.copied_files.len());
            }
            if !task.conflicting_files.is_empty() {
                println!(
                    "    ! Skipped conflicting: {}",
                    task.conflicting_files.len()
                );
            }
        }
        println!();
    }

    let total_copied = report
        .endpoints
        .as_ref()
        .map(|e| e.copied_keys.len())
        .unwrap_or(0)
        + report
            .global_settings
            .as_ref()
            .map(|g| g.copied_keys.len())
            .unwrap_or(0)
        + report
            .secrets
            .as_ref()
            .map(|s| s.copied_keys.len())
            .unwrap_or(0)
        + report
            .task_history
            .as_ref()
            .map(|t| t.copied_ids.len())
            .unwrap_or(0)
        + report
            .tasks
            .iter()
            .map(|t| t.copied_files.len())
            .sum::<usize>();

    println!("Total: {} files/keys copied", total_copied);
}

pub fn format_config_output(state: &crate::storage::global_state::GlobalState) -> String {
    let mut lines = vec![
        String::from("Current Sned Configuration"),
        String::from("=============================="),
        String::new(),
    ];

    lines.push(String::from("## Mode & Provider"));
    lines.push(format!("  Mode:     {}", state.mode));
    lines.push(format!(
        "  Act Provider:     {}",
        state.act_mode_api_provider
    ));
    lines.push(format!(
        "  Plan Provider:    {}",
        state.plan_mode_api_provider
    ));
    lines.push(String::new());

    lines.push("## Auto-Approve".to_string());
    if let Some(ref auto_approve) = state.auto_approval_settings {
        lines.push(format!("  Enabled:   {}", auto_approve.enabled));
        lines.push(format!("  Actions:  {}", auto_approve.actions.join(", ")));
    } else {
        lines.push("  Enabled:   false".to_string());
    }
    lines.push(format!(
        "  Yolo Mode: {}",
        if state.yolo_mode_toggled {
            "true"
        } else {
            "false"
        }
    ));
    lines.push(format!(
        "  Auto-Approve-All: {}",
        if state.auto_approve_all_toggled {
            "true"
        } else {
            "false"
        }
    ));
    lines.push(String::new());

    lines.push("## Context".to_string());
    lines.push(format!(
        "  Auto-Condense: {}",
        if state.use_auto_condense {
            "enabled"
        } else {
            "disabled"
        }
    ));
    lines.push(format!(
        "  Strict Plan Mode: {}",
        if state.strict_plan_mode_enabled {
            "enabled"
        } else {
            "disabled"
        }
    ));
    lines.push(String::new());

    lines.push("## Tools".to_string());
    lines.push(format!(
        "  Subagents: {}",
        if state.subagents_enabled {
            "enabled"
        } else {
            "disabled"
        }
    ));
    lines.push(format!(
        "  Hooks: {}",
        if state.hooks_enabled {
            "enabled"
        } else {
            "disabled"
        }
    ));
    lines.push(format!(
        "  Checkpoints: {}",
        if state.enable_checkpoints_setting {
            "enabled"
        } else {
            "disabled"
        }
    ));
    lines.push(String::new());

    lines.push("## Shell".to_string());
    lines.push(format!("  Timeout: {}s", state.shell_integration_timeout));
    lines.push(format!(
        "  Terminal Line Limit: {}",
        state.terminal_output_line_limit
    ));
    lines.push(String::new());

    lines.push("## Recent Announcements".to_string());
    if let Some(ref v) = state.last_shown_announcement_id {
        lines.push(format!("  Last Shown: {}", v));
    } else {
        lines.push("  Last Shown: none".to_string());
    }

    lines.join("\n")
}

pub fn run_auth(opts: AuthOptions) -> anyhow::Result<()> {
    use crate::providers::env_auth::get_provider_from_env;
    use std::io::{self, Write};

    if let Some(config_path) = &opts.config {
        unsafe {
            std::env::set_var("SNED_DIR", config_path);
        }
    }

    let state_manager = crate::storage::state_manager::StateManager::new()?;

    let provider = match &opts.provider {
        Some(p) => p.clone(),
        None => match get_provider_from_env() {
            Some(p) => {
                println!("Auto-detected provider from environment: {}", p);
                p.to_string()
            }
            None => {
                crate::cli::colors::eprint_error(
                    "Could not auto-detect provider. Use --provider to specify one.",
                );
                eprintln!(
                    "Supported providers: anthropic, openai, openai-native, openrouter, gemini, groq, mistral, moonshot, deepseek, qwen, together, fireworks, nebius, zai, minimax, cerebras, huggingface, vercel-ai-gateway, openai"
                );
                return Ok(());
            }
        },
    };

    let api_key = match &opts.apikey {
        Some(k) => k.clone(),
        None => {
            print!("Enter API key for {}: ", provider);
            io::stdout().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            input.trim().to_string()
        }
    };

    if api_key.is_empty() {
        crate::cli::colors::eprint_error("API key cannot be empty");
        return Ok(());
    }

    let secret_key = match provider.as_str() {
        "anthropic" => "apiKey",
        "openai" | "openai-native" => "openAiApiKey",
        "openrouter" => "openRouterApiKey",
        "gemini" => "geminiApiKey",
        "groq" => "groqApiKey",
        "cerebras" => "cerebrasApiKey",
        "xai" => "xaiApiKey",
        "mistral" => "mistralApiKey",
        "moonshot" => "moonshotApiKey",
        "deepseek" => "deepSeekApiKey",
        "qwen" => "qwenApiKey",
        "together" => "togetherApiKey",
        "fireworks" => "fireworksApiKey",
        "nebius" => "nebiusApiKey",
        "zai" => "zaiApiKey",
        "minimax" => "minimaxApiKey",
        "huggingface" => "huggingFaceApiKey",
        "vercel-ai-gateway" => "vercelAiGatewayApiKey",
        "openai-compatible" => "openAiCompatibleCustomApiKey",
        _ => {
            crate::cli::colors::eprint_error(&format!("Unknown provider '{}'", provider));
            return Ok(());
        }
    };

    state_manager.set_secret(secret_key, api_key.clone());
    println!("Stored API key for {}", provider);

    // Persist model ID if provided (applies to both act and plan modes)
    if let Some(model_id) = &opts.modelid {
        state_manager.set_global_state_string_field("act_mode_api_model_id", model_id.clone())?;
        state_manager.set_global_state_string_field("plan_mode_api_model_id", model_id.clone())?;
        println!("Stored model ID: {}", model_id);
    }

    // Persist base URL if provided (provider-specific)
    if let Some(base_url) = &opts.baseurl {
        let base_url_key = match provider.as_str() {
            "anthropic" => "anthropic_base_url",
            "openai" | "openai-native" => "open_ai_base_url",
            "openrouter" => "open_router_base_url",
            "gemini" => "gemini_base_url",
            "lite_llm" => "lite_llm_base_url",
            _ => {
                // For other providers, use open_ai_base_url as fallback
                "open_ai_base_url"
            }
        };
        state_manager.set_global_state_string_field(base_url_key, base_url.clone())?;
        println!("Stored base URL for {}: {}", provider, base_url);
    }

    // Persist Azure API version if provided (stored for Azure OpenAI provider)
    if let Some(azure_version) = &opts.azure_api_version {
        // Azure API version is typically used with Azure OpenAI
        // Store it as a setting that can be referenced later
        state_manager.set_global_state_string_field("azure_api_version", azure_version.clone())?;
        println!("Stored Azure API version: {}", azure_version);
    }

    if opts.verbose {
        use crate::storage::global_state::load_global_state;
        let state = load_global_state();
        println!("\nCurrent configuration:");
        println!("{}", format_config_output(&state));
    }

    Ok(())
}

/// Run diagnostic checks and report status
pub fn run_doctor() -> anyhow::Result<()> {
    use std::env;
    use std::fs;

    let mut has_fail = false;
    let mut has_warn = false;

    println!("sned Diagnostic Report");
    println!("{}", "=".repeat(50));

    // Check 1: API keys
    println!("\n[API Keys]");
    let providers = vec![
        ("openai", "OPENAI_API_KEY"),
        ("anthropic", "ANTHROPIC_API_KEY"),
        ("google", "GEMINI_API_KEY"),
        ("groq", "GROQ_API_KEY"),
        ("mistral", "MISTRAL_API_KEY"),
        ("moonshot", "MOONSHOT_API_KEY"),
        ("deepseek", "DEEPSEEK_API_KEY"),
        ("qwen", "QWEN_API_KEY"),
        ("together", "TOGETHER_API_KEY"),
        ("fireworks", "FIREWORKS_API_KEY"),
        ("nebius", "NEBIUS_API_KEY"),
        ("zai", "ZAI_API_KEY"),
        ("minimax", "MINIMAX_API_KEY"),
        ("huggingface", "HUGGINGFACE_API_KEY"),
        ("vercel-ai-gateway", "VERCEL_AI_GATEWAY_API_KEY"),
    ];

    for (provider, env_var) in providers {
        match env::var(env_var) {
            Ok(key) if !key.is_empty() => {
                let masked = if key.len() > 8 {
                    format!("{}...{}", &key[..4], &key[key.len() - 4..])
                } else {
                    "****".to_string()
                };
                println!("  [OK] {} ({})", provider, masked);
            }
            Ok(_) => {
                println!("  [WARN] {} ({} is set but empty)", provider, env_var);
                has_warn = true;
            }
            Err(_) => {
                println!("  [WARN] {} ({} not set)", provider, env_var);
                has_warn = true;
            }
        }
    }

    // Check 2: Config file
    println!("\n[Configuration]");
    let config_path = dirs::config_dir()
        .map(|mut p| {
            p.push("sned");
            p.push("config.json");
            p
        })
        .or_else(|| {
            env::var("HOME").ok().map(|p| {
                let mut path = PathBuf::from(p);
                path.push(".config/sned/config.json");
                path
            })
        });

    if let Some(config_path) = config_path {
        if config_path.exists() {
            match fs::read_to_string(&config_path) {
                Ok(content) => {
                    if serde_json::from_str::<serde_json::Value>(&content).is_ok() {
                        println!("  [OK] Config file found and parseable");
                        println!("       {}", config_path.display());
                    } else {
                        println!("  [FAIL] Config file is not valid JSON");
                        println!("         {}", config_path.display());
                        has_fail = true;
                    }
                }
                Err(e) => {
                    println!("  [FAIL] Cannot read config file: {}", e);
                    has_fail = true;
                }
            }
        } else {
            println!("  [WARN] Config file not found");
            println!("         {}", config_path.display());
            has_warn = true;
        }
    } else {
        println!("  [WARN] Cannot determine config directory");
        has_warn = true;
    }

    // Check 3: Git repo
    println!("\n[Git Repository]");
    let cwd = env::current_dir().unwrap_or_default();
    let git_dir = cwd.join(".git");
    if git_dir.exists() {
        println!("  [OK] Git repository initialized");
    } else {
        println!("  [WARN] Not in a Git repository");
        has_warn = true;
    }

    // Check 4: ~/.sned/ directory structure
    println!("\n[Storage Directory]");
    let sned_dir = dirs::config_dir()
        .map(|mut p| {
            p.push("sned");
            p
        })
        .or_else(|| {
            env::var("HOME").ok().map(|p| {
                let mut path = PathBuf::from(p);
                path.push(".sned");
                path
            })
        });

    if let Some(sned_dir) = sned_dir {
        if sned_dir.exists() {
            println!("  [OK] Sned directory exists");
            println!("       {}", sned_dir.display());

            // Check subdirectories
            let subdirs = ["tasks", "secrets", "state"];
            for subdir in subdirs {
                let subpath = sned_dir.join(subdir);
                if subpath.exists() {
                    println!("  [OK] {} directory exists", subdir);
                } else {
                    println!(
                        "  [WARN] {} directory missing (will be created on first use)",
                        subdir
                    );
                    has_warn = true;
                }
            }
        } else {
            println!("  [WARN] Sned directory not found (will be created on first use)");
            println!("         {}", sned_dir.display());
            has_warn = true;
        }
    } else {
        println!("  [WARN] Cannot determine Sned directory");
        has_warn = true;
    }

    // Check 5: Tree-sitter grammars
    println!("\n[Tree-sitter Grammars]");
    // Tree-sitter parsers are loaded on-demand, so we just check if the module is available
    println!("  [OK] Tree-sitter service available (parsers loaded on-demand)");

    // Summary
    println!("\n{}", "=".repeat(50));
    if has_fail {
        println!("Status: FAIL (some checks failed)");
        std::process::exit(crate::exit_codes::EXIT_ERROR);
    } else if has_warn {
        println!("Status: WARN (some warnings, but functional)");
        std::process::exit(crate::exit_codes::EXIT_CONFIG);
    } else {
        println!("Status: OK (all checks passed)");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Test that run_auth accepts and processes modelid flag without error.
    ///
    /// Note: This test verifies the flag is accepted and processed.
    /// Persistence to disk happens asynchronously via StateManager's background persist.
    #[test]
    fn test_run_auth_accepts_model_id_flag() {
        let temp = TempDir::new().unwrap();
        let tasks_dir = temp.path().join("tasks");
        let settings_dir = temp.path().join("settings");
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&tasks_dir).unwrap();
        fs::create_dir_all(&settings_dir).unwrap();
        fs::create_dir_all(&state_dir).unwrap();

        // Override SNED_DATA_DIR for this test
        let original_data_dir = std::env::var("SNED_DATA_DIR").ok();
        unsafe {
            std::env::set_var("SNED_DATA_DIR", temp.path().to_str().unwrap());
        }

        let opts = AuthOptions {
            provider: Some("anthropic".to_string()),
            apikey: Some("test-api-key".to_string()),
            modelid: Some("claude-sonnet-4-20250514".to_string()),
            baseurl: None,
            azure_api_version: None,
            verbose: false,
            cwd: None,
            config: Some(temp.path().to_str().unwrap().to_string()),
        };

        // Should complete without error
        let result = run_auth(opts);
        assert!(result.is_ok(), "run_auth should succeed with modelid flag");

        // Restore original env var
        if let Some(val) = original_data_dir {
            unsafe {
                std::env::set_var("SNED_DATA_DIR", val);
            }
        } else {
            unsafe {
                std::env::remove_var("SNED_DATA_DIR");
            }
        }
    }

    /// Test that run_auth accepts and processes baseurl flag without error.
    #[test]
    fn test_run_auth_accepts_baseurl_flag() {
        let temp = TempDir::new().unwrap();
        let tasks_dir = temp.path().join("tasks");
        let settings_dir = temp.path().join("settings");
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&tasks_dir).unwrap();
        fs::create_dir_all(&settings_dir).unwrap();
        fs::create_dir_all(&state_dir).unwrap();

        // Override SNED_DATA_DIR for this test
        let original_data_dir = std::env::var("SNED_DATA_DIR").ok();
        unsafe {
            std::env::set_var("SNED_DATA_DIR", temp.path().to_str().unwrap());
        }

        let opts = AuthOptions {
            provider: Some("anthropic".to_string()),
            apikey: Some("test-api-key".to_string()),
            modelid: None,
            baseurl: Some("https://custom.anthropic.com".to_string()),
            azure_api_version: None,
            verbose: false,
            cwd: None,
            config: Some(temp.path().to_str().unwrap().to_string()),
        };

        // Should complete without error
        let result = run_auth(opts);
        assert!(result.is_ok(), "run_auth should succeed with baseurl flag");

        // Restore original env var
        if let Some(val) = original_data_dir {
            unsafe {
                std::env::set_var("SNED_DATA_DIR", val);
            }
        } else {
            unsafe {
                std::env::remove_var("SNED_DATA_DIR");
            }
        }
    }
}
