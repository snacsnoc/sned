//! Core agent loop, streaming, task execution, and file editing.
//!
//! Source map:
//! - Agent loop: `dirac/src/core/task/`, `dirac/src/core/controller/task/`
//! - Streaming: `dirac/src/core/controller/`
//! - Tool execution: `dirac/src/core/task/tools/`
//! - File editing: `dirac/src/integrations/editor/FileEditProvider.ts`
//! - Context: `dirac/src/core/context/`

pub mod agent_loop;
pub mod agent_types;
pub mod anchor_dictionary;
pub mod approval;
pub mod cancellation;
pub mod checkpoints;
pub mod context;
pub mod context_tracking;
pub mod edit_batch;
pub mod file_editor;
pub mod file_search;
pub mod hash_utils;
pub mod hook_cache;
pub mod hooks;
pub mod mentions;
pub mod provider_retry;
pub mod shadow_git;
pub mod stream_parsing;
pub mod tool_output;
pub mod tools;
pub mod workspace;
