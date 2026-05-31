//! Core agent loop, streaming, task execution, and file editing.
//!
//! # Lock Ordering
//!
//! To prevent deadlocks, always acquire locks in this order:
//! 1. `self.state` (TaskState)
//! 2. `self.conversation_history` (Vec<StorageMessage>)
//! 3. `self.message_queue` (VecDeque<StorageMessage>)
//!
//! Never acquire a lower-priority lock while holding a higher-priority one.
//! When multiple locks are needed, acquire them in order and release them in
//! reverse order when possible.

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
pub mod hooks;
pub mod mentions;
pub mod plan_state;
pub mod provider_retry;
pub mod shadow_git;
pub mod stream_parsing;
pub mod tool_output;
pub mod tools;
pub mod workspace;
