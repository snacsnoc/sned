//! Startup Time Benchmark
//!
//! Measures time from CLI entry to first provider request.
//! This captures the initialization overhead: argument parsing, config loading,
//! state manager creation, provider initialization, and tool registry setup.
//!
//! Target: cold start < 200ms, --help < 50ms

use clap::Parser;
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use tokio::runtime::Runtime;

/// Benchmark CLI argument parsing only (fastest path)
fn bench_cli_parse(c: &mut Criterion) {
    c.bench_function("cli_parse_empty", |b| {
        b.iter(|| {
            // Simulate `sned` with no arguments (parses to interactive shell mode)
            let args = vec!["sned"];
            let _ = black_box(sned::cli::Cli::parse_from(args));
        })
    });

    c.bench_function("cli_parse_prompt", |b| {
        b.iter(|| {
            let args = vec!["sned", "hello"];
            let _ = black_box(sned::cli::Cli::parse_from(args));
        })
    });
}

/// Benchmark state manager initialization
fn bench_state_manager_init(c: &mut Criterion) {
    c.bench_function("state_manager_init", |b| {
        b.iter(|| {
            let rt = Runtime::new().unwrap();
            rt.block_on(async {
                let state_manager =
                    black_box(sned::storage::state_manager::StateManager::new().unwrap());
                let _ = black_box(state_manager.initialize());
            })
        })
    });
}

/// Benchmark provider creation (mock provider)
fn bench_provider_creation(c: &mut Criterion) {
    c.bench_function("provider_creation_anthropic", |b| {
        b.iter(|| {
            let config = sned::providers::anthropic::AnthropicConfig {
                api_key: "test-key".to_string(),
                base_url: None,
                model_id: "claude-3-5-sonnet-20240620".to_string(),
                model_info: Some(sned::providers::ModelInfo::default()),
                thinking_budget_tokens: None,
            };
            let _ = black_box(sned::providers::anthropic::AnthropicProvider::new(config).unwrap());
        })
    });

    c.bench_function("provider_creation_openai", |b| {
        b.iter(|| {
            let config = sned::providers::openai::OpenAiConfig {
                model_id: "gpt-4o".to_string(),
                api_key: "test-key".to_string(),
                base_url: None,
                model_info: None,
                reasoning_effort: None,
                custom_headers: None,
            };
            let _ = black_box(sned::providers::openai::OpenAiProvider::new(config).unwrap());
        })
    });
}

/// Benchmark tool registry creation
fn bench_tool_registry_creation(c: &mut Criterion) {
    c.bench_function("tool_registry_creation", |b| {
        b.iter(|| {
            use sned::core::approval::ApprovalManager;
            use sned::core::tools::ToolRegistry;
            use sned::services::symbol_index::SymbolIndexService;
            use std::sync::Arc;
            use tokio::sync::Mutex;

            let approval_manager = Arc::new(Mutex::new(
                ApprovalManager::new()
                    .with_yolo(false)
                    .with_auto_approve_all(false)
                    .with_workspace_root("/tmp/test".to_string()),
            ));

            let symbol_index = Arc::new(std::sync::Mutex::new(
                SymbolIndexService::new("/tmp/test".to_string()),
            ));

            let mut registry = ToolRegistry::new();
            registry.register(
                sned::core::tools::SnedTool::ExecuteCommand,
                Arc::new(
                    sned::core::tools::handlers::execute_command::ExecuteCommandHandler::new()
                        .with_yolo(false),
                ),
            );
            registry.register(
                sned::core::tools::SnedTool::WriteToFile,
                Arc::new(
                    sned::core::tools::handlers::write_to_file::WriteToFileHandler::new(),
                ),
            );
            registry.register(
                sned::core::tools::SnedTool::ReadFile,
                Arc::new(
                    sned::core::tools::handlers::read_file::ReadFileHandler::new(),
                ),
            );
            registry.register(
                sned::core::tools::SnedTool::ListFiles,
                Arc::new(
                    sned::core::tools::handlers::list_files::ListFilesHandler::new(),
                ),
            );
            registry.register(
                sned::core::tools::SnedTool::SearchFiles,
                Arc::new(
                    sned::core::tools::handlers::search_files::SearchFilesHandler::new(),
                ),
            );
            registry.register(
                sned::core::tools::SnedTool::EditFile,
                Arc::new(
                    sned::core::tools::handlers::edit_file::EditFileHandler::new()
                        .with_approval_manager(approval_manager.clone()),
                ),
            );
            registry.register(
                sned::core::tools::SnedTool::AskFollowupQuestion,
                Arc::new(
                    sned::core::tools::handlers::ask_followup_question::AskFollowupQuestionHandler::new(),
                ),
            );
            registry.register(
                sned::core::tools::SnedTool::AttemptCompletion,
                Arc::new(
                    sned::core::tools::handlers::attempt_completion::AttemptCompletionHandler::new(),
                ),
            );
            registry.register(
                sned::core::tools::SnedTool::PlanModeRespond,
                Arc::new(
                    sned::core::tools::handlers::plan_mode_respond::PlanModeRespondHandler::new(),
                ),
            );
            registry.register(
                sned::core::tools::SnedTool::GetFileSkeleton,
                Arc::new(sned::core::tools::handlers::get_file_skeleton::GetFileSkeletonHandler),
            );
            registry.register(
                sned::core::tools::SnedTool::GetFunction,
                Arc::new(sned::core::tools::handlers::get_function::GetFunctionHandler),
            );
            registry.register(
                sned::core::tools::SnedTool::FindSymbolReferences,
                Arc::new(sned::core::tools::handlers::find_symbol_references::FindSymbolReferencesHandler),
            );
            registry.register(
                sned::core::tools::SnedTool::ReplaceSymbol,
                Arc::new(
                    sned::core::tools::handlers::replace_symbol::ReplaceSymbolHandler::new()
                        .with_symbol_index(Arc::clone(&symbol_index)),
                ),
            );
            registry.register(
                sned::core::tools::SnedTool::RenameSymbol,
                Arc::new(
                    sned::core::tools::handlers::rename_symbol::RenameSymbolHandler::new()
                        .with_symbol_index(symbol_index.clone()),
                ),
            );
            registry.register(
                sned::core::tools::SnedTool::SummarizeTask,
                Arc::new(
                    sned::core::tools::handlers::summarize_task::SummarizeTaskHandler::new(),
                ),
            );
            registry.register(
                sned::core::tools::SnedTool::Condense,
                Arc::new(
                    sned::core::tools::handlers::condense::CondenseHandler::new(),
                ),
            );
            registry.register(
                sned::core::tools::SnedTool::UseSkill,
                Arc::new(
                    sned::core::tools::handlers::use_skill::UseSkillHandler::new(),
                ),
            );
            registry.register(
                sned::core::tools::SnedTool::ListSkills,
                Arc::new(
                    sned::core::tools::handlers::list_skills::ListSkillsHandler::new(),
                ),
            );

            black_box(registry);
        })
    });
}

/// Benchmark full startup: CLI parse + state manager + provider + tool registry
fn bench_full_startup(c: &mut Criterion) {
    c.bench_function("full_startup_cold", |b| {
        b.iter(|| {
            use sned::cli::Cli;
            use sned::core::tools::ToolRegistry;
            use sned::providers::anthropic::AnthropicConfig;
            use sned::providers::anthropic::AnthropicProvider;
            use sned::storage::state_manager::StateManager;
            use std::sync::Arc;

            // Step 1: CLI parse
            let args = vec!["sned", "test prompt"];
            let _cli = black_box(Cli::parse_from(args));

            // Step 2: State manager
            let rt = Runtime::new().unwrap();
            rt.block_on(async {
                let state_manager = black_box(StateManager::new().unwrap());
                let _ = black_box(state_manager.initialize());
            });

            // Step 3: Provider creation
            let config = AnthropicConfig {
                api_key: "test-key".to_string(),
                base_url: None,
                model_id: "claude-3-5-sonnet-20240620".to_string(),
                model_info: Some(sned::providers::ModelInfo::default()),
                thinking_budget_tokens: None,
            };
            let _provider = black_box(AnthropicProvider::new(config).unwrap());

            // Step 4: Tool registry (minimal)
            let mut registry = ToolRegistry::new();
            registry.register(
                sned::core::tools::SnedTool::ReadFile,
                Arc::new(sned::core::tools::handlers::read_file::ReadFileHandler::new()),
            );
            black_box(registry);
        })
    });
}

criterion_group!(
    benches,
    bench_cli_parse,
    bench_state_manager_init,
    bench_provider_creation,
    bench_tool_registry_creation,
    bench_full_startup,
);
criterion_main!(benches);
