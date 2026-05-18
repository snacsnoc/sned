/** Context management for sned CLI.
 *
 * Ports behavior from `dirac/src/core/context/`.
 *
 * ## Source Map
 * - Context management / truncation: `dirac/src/core/context/context-management/ContextManager.ts`
 * - Context window utilities: `dirac/src/core/context/context-management/context-window-utils.ts`
 * - Context trackers: `dirac/src/core/context/context-tracking/`
 * - System prompt construction: `dirac/src/core/prompts/system-prompt/`
 */
pub mod context_loader;
pub mod context_manager;
pub mod context_window;
pub mod instructions;
pub mod system_prompt;
pub mod trackers;

pub use context_loader::ContextLoader;
pub use context_manager::{
    ApiReqInfo, CompactedSummary, ContextUpdateResult, PreservedState, TruncationKeep,
    get_new_context_messages_and_metadata, get_next_truncation_range, get_truncated_messages,
    should_compact_context_window,
};
pub use context_window::get_context_window_info;
pub use instructions::{
    RuleToggles, SkillContent, SkillMetadata, SkillSource, SkillSupportingFiles,
    combine_rule_toggles, discover_skills, find_agents_md_files, get_available_skills,
    get_local_agents_rules, get_local_cursor_rules, get_local_windsurf_rules, get_skill_content,
    list_supporting_files, scan_skills_directory, synchronize_rule_toggles,
};
pub use system_prompt::{PromptBuilder, SystemPromptContext};
pub use trackers::FileContextTracker;
