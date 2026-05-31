//! Plan state for the interactive Plan -> Approve -> Act workflow.
//!
//! Tracks plan steps, approval status, execution progress, and pause/resume state.

use std::fmt;

/// Status of a single plan step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanStepStatus {
    Pending,
    Running,
    Done,
    Failed,
}

impl fmt::Display for PlanStepStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanStepStatus::Pending => write!(f, "pending"),
            PlanStepStatus::Running => write!(f, "running"),
            PlanStepStatus::Done => write!(f, "done"),
            PlanStepStatus::Failed => write!(f, "failed"),
        }
    }
}

/// A single step in the plan.
#[derive(Debug, Clone)]
pub struct PlanStep {
    pub index: usize,
    pub description: String,
    pub status: PlanStepStatus,
}

impl PlanStep {
    pub fn status_icon(&self) -> &'static str {
        match self.status {
            PlanStepStatus::Pending => "○",
            PlanStepStatus::Running => "→",
            PlanStepStatus::Done => "✓",
            PlanStepStatus::Failed => "✗",
        }
    }
}

/// State of the plan mode.
#[derive(Debug, Clone)]
pub struct PlanState {
    /// The numbered plan steps.
    pub steps: Vec<PlanStep>,
    /// Index of the step currently being executed.
    pub current_step_index: usize,
    /// Whether the plan has been approved by the user.
    pub approved: bool,
    /// Whether the plan is paused.
    pub paused: bool,
    /// Whether the plan is complete.
    pub complete: bool,
}

impl PlanState {
    /// Create a new plan from step descriptions.
    pub fn create_plan(step_descriptions: Vec<String>) -> Self {
        let steps = step_descriptions
            .into_iter()
            .enumerate()
            .map(|(i, desc)| PlanStep {
                index: i,
                description: desc,
                status: PlanStepStatus::Pending,
            })
            .collect();

        Self {
            steps,
            current_step_index: 0,
            approved: false,
            paused: false,
            complete: false,
        }
    }

    /// Update a step's description.
    pub fn update_step(&mut self, index: usize, description: String) -> Result<(), String> {
        let step = self
            .steps
            .get_mut(index)
            .ok_or_else(|| format!("Step {} does not exist", index + 1))?;
        step.description = description;
        Ok(())
    }

    /// Insert a new step after the given index. Use index = usize::MAX to insert at the end.
    pub fn insert_step_after(
        &mut self,
        after_index: usize,
        description: String,
    ) -> Result<(), String> {
        let old_len = self.steps.len();
        let old_current_step_index = self.current_step_index;
        let insert_pos = if after_index == usize::MAX {
            self.steps.len()
        } else {
            after_index + 1
        };
        if insert_pos > self.steps.len() {
            return Err(format!(
                "Cannot insert after step {} (only {} steps)",
                after_index + 1,
                self.steps.len()
            ));
        }
        self.steps.insert(
            insert_pos,
            PlanStep {
                index: insert_pos,
                description,
                status: PlanStepStatus::Pending,
            },
        );
        self.renumber();
        if self.steps.is_empty() || old_len == 0 {
            self.current_step_index = 0;
        } else if old_current_step_index >= old_len {
            self.current_step_index = self.steps.len() - 1;
        } else if insert_pos <= old_current_step_index {
            self.current_step_index = old_current_step_index + 1;
        }
        Ok(())
    }

    /// Insert a new step at the beginning of the plan.
    pub fn insert_step_at_beginning(&mut self, description: String) -> Result<(), String> {
        let old_len = self.steps.len();
        let old_current_step_index = self.current_step_index;

        self.steps.insert(
            0,
            PlanStep {
                index: 0,
                description,
                status: PlanStepStatus::Pending,
            },
        );
        self.renumber();

        if old_len == 0 || old_current_step_index >= old_len {
            self.current_step_index = 0;
        } else {
            self.current_step_index = old_current_step_index + 1;
        }

        Ok(())
    }

    /// Remove a step by index (0-based).
    pub fn remove_step(&mut self, index: usize) -> Result<(), String> {
        let old_len = self.steps.len();
        let old_current_step_index = self.current_step_index;
        if index >= self.steps.len() {
            return Err(format!("Step index {} out of range (0-{})", index, self.steps.len().saturating_sub(1)));
        }
        self.steps.remove(index);
        self.renumber();
        if self.steps.is_empty() {
            self.current_step_index = 0;
        } else if old_current_step_index >= old_len {
            self.current_step_index = self.steps.len() - 1;
        } else if index < old_current_step_index {
            self.current_step_index = old_current_step_index - 1;
        } else if self.current_step_index >= self.steps.len() {
            self.current_step_index = self.steps.len() - 1;
        }
        Ok(())
    }

    /// Mark a step with a given status.
    pub fn mark_step(&mut self, index: usize, status: PlanStepStatus) -> Result<(), String> {
        let step = self
            .steps
            .get_mut(index)
            .ok_or_else(|| format!("Step {} does not exist", index + 1))?;
        step.status = status;
        Ok(())
    }

    /// Get the current step.
    pub fn current_step(&self) -> Option<&PlanStep> {
        self.steps.get(self.current_step_index)
    }

    /// Advance to the next pending step. Returns the index of the new current step, or None if done.
    ///
    /// NOTE: Returns `usize` instead of `&PlanStep` to avoid reference-escaping-the-mutex issues
    /// when called through `Arc<Mutex<Session>>`. This is a deliberate Rust-native adaptation.
    pub fn advance(&mut self) -> Option<usize> {
        // Bounds check: if current_step_index is out of range, treat as complete
        if self.current_step_index >= self.steps.len() {
            if self.is_complete() {
                self.complete = true;
            }
            return None;
        }

        if let Some(step) = self.steps.get_mut(self.current_step_index)
            && step.status == PlanStepStatus::Running
        {
            step.status = PlanStepStatus::Done;
        }

        // Check if current step is Failed — do NOT skip it
        if let Some(step) = self.steps.get(self.current_step_index)
            && step.status == PlanStepStatus::Failed
        {
            return None;
        }

        // Find next pending step
        if let Some(next_idx) = self.steps.iter().position(|s| s.status == PlanStepStatus::Pending)
        {
            self.current_step_index = next_idx;
            self.steps[next_idx].status = PlanStepStatus::Running;
            Some(self.current_step_index)
        } else {
            // Only mark complete if ALL steps are actually Done (not Failed)
            if self.is_complete() {
                self.complete = true;
            }
            None
        }
    }

    /// Check if all steps are done.
    pub fn is_complete(&self) -> bool {
        !self.steps.is_empty() && self.steps.iter().all(|s| s.status == PlanStepStatus::Done)
    }

    /// Re-number all steps after insert/remove.
    pub fn renumber(&mut self) {
        for (i, step) in self.steps.iter_mut().enumerate() {
            step.index = i;
        }
    }

    /// Parse a plan response text into numbered step descriptions.
    ///
    /// Accepts formats:
    /// - `1. First step`
    /// - `1) First step`
    /// - `Step 1: First step`
    pub fn parse_plan(text: &str) -> Option<Vec<String>> {
        let mut steps = Vec::new();

        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Try "N. description" or "N) description"
            if let Some(step) = parse_numbered_line(trimmed) {
                steps.push(step);
                continue;
            }

            // Try "- Step N: description" or "- description"
            if let Some(step) = parse_bullet_line(trimmed) {
                steps.push(step);
            }
        }

        if steps.is_empty() {
            None
        } else {
            Some(steps)
        }
    }

    /// Format the plan state for model context injection.
    pub fn format_state(&self) -> String {
        let mut out = String::new();

        let has_failed = self.steps.iter().any(|s| s.status == PlanStepStatus::Failed);
        let mode_label = if self.complete {
            "COMPLETE"
        } else if has_failed {
            "FAILED"
        } else if self.paused {
            "PAUSED"
        } else if self.approved {
            "ACT"
        } else {
            "APPROVAL"
        };

        out.push_str("Plan state:\n");
        out.push_str(&format!("mode: {}\n", mode_label));
        out.push_str(&format!("approved: {}\n", self.approved));
        out.push_str(&format!("paused: {}\n", self.paused));
        out.push_str(&format!("complete: {}\n", self.complete));
        out.push_str(&format!(
            "current_step_index: {}\n",
            self.current_step_index
        ));
        out.push('\n');

        out.push_str("Steps:\n");
        for step in &self.steps {
            out.push_str(&format!(
                "{}. [{}] {}\n",
                step.index + 1,
                step.status,
                step.description
            ));
        }

        if let Some(current) = self.current_step() {
            out.push_str(&format!(
                "\nCurrent step:\n{}. {}\n",
                current.index + 1,
                current.description
            ));
        }

        out
    }

    /// Format the plan for display in the TUI panel.
    pub fn format_display(&self) -> String {
        let mut out = String::new();

        for step in &self.steps {
            out.push_str(&format!(
                "  {} {}. {}\n",
                step.status_icon(),
                step.index + 1,
                step.description
            ));
        }

        let done = self
            .steps
            .iter()
            .filter(|s| s.status == PlanStepStatus::Done)
            .count();
        out.push_str(&format!(
            "\n  Progress: {}/{} steps complete\n",
            done,
            self.steps.len()
        ));

        out
    }

    /// Get the status summary string for the status bar.
    pub fn status_summary(&self) -> String {
        let total = self.steps.len();
        let current = self.current_step_index + 1;
        let has_failed = self.steps.iter().any(|s| s.status == PlanStepStatus::Failed);
        let complete = self.complete;
        let paused = self.paused;
        let approved = self.approved;

        if complete {
            format!("Plan: complete {}/{}", total, total)
        } else if paused {
            format!("Plan: paused at {}/{}", current, total)
        } else if has_failed {
            format!("Plan: failed at {}/{}", current, total)
        } else if approved {
            format!("Plan: {}/{} running", current, total)
        } else {
            "Plan: awaiting approval".to_string()
        }
    }
}

/// Parse a line like "1. description" or "1) description".
fn parse_numbered_line(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut i = 0;

    // Skip leading digits
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }

    if i == 0 || i >= bytes.len() {
        return None;
    }

    // Must be followed by '.' or ')'
    if bytes[i] != b'.' && bytes[i] != b')' {
        return None;
    }

    i += 1;

    // Must be followed by whitespace
    if i >= bytes.len() || !bytes[i].is_ascii_whitespace() {
        return None;
    }

    // Skip whitespace
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }

    let desc = line[i..].trim().to_string();
    if desc.is_empty() {
        return None;
    }
    Some(desc)
}

/// Parse a bullet line like "- Step N: description" or "Step N: description".
/// Bare bullets ("- description") are rejected per spec — only numbered formats are accepted.
fn parse_bullet_line(line: &str) -> Option<String> {
    // Strip leading "- " or "* " bullet prefix if present
    let stripped = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")).unwrap_or(line);
    // Handle "Step N: description" format
    parse_step_prefix_line(stripped)
}

/// Parse a line like "Step 1: description" or "Step 1. description".
fn parse_step_prefix_line(line: &str) -> Option<String> {
    let lower = line.to_lowercase();
    if !lower.starts_with("step ") {
        return None;
    }
    let rest = &line[5..];
    let bytes = rest.as_bytes();
    let mut i = 0;
    // Skip digits
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 || i >= bytes.len() {
        return None;
    }
    // Must be followed by ':' or '.' or ')'
    if bytes[i] != b':' && bytes[i] != b'.' && bytes[i] != b')' {
        return None;
    }
    i += 1;
    // Skip whitespace
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let desc = rest[i..].trim().to_string();
    if desc.is_empty() {
        return None;
    }
    Some(desc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_plan() {
        let plan = PlanState::create_plan(vec![
            "First step".to_string(),
            "Second step".to_string(),
            "Third step".to_string(),
        ]);
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.steps[0].description, "First step");
        assert_eq!(plan.steps[1].description, "Second step");
        assert_eq!(plan.steps[2].description, "Third step");
        assert!(!plan.approved);
        assert!(!plan.paused);
        assert!(!plan.complete);
    }

    #[test]
    fn test_update_step() {
        let mut plan = PlanState::create_plan(vec![
            "Step one".to_string(),
            "Step two".to_string(),
        ]);
        plan.update_step(0, "Updated step one".to_string()).unwrap();
        assert_eq!(plan.steps[0].description, "Updated step one");
    }

    #[test]
    fn test_insert_step_after() {
        let mut plan = PlanState::create_plan(vec![
            "Step one".to_string(),
            "Step three".to_string(),
        ]);
        plan.insert_step_after(0, "Step two".to_string()).unwrap();
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.steps[0].description, "Step one");
        assert_eq!(plan.steps[1].description, "Step two");
        assert_eq!(plan.steps[2].description, "Step three");
        // Check renumbering
        assert_eq!(plan.steps[0].index, 0);
        assert_eq!(plan.steps[1].index, 1);
        assert_eq!(plan.steps[2].index, 2);
    }

    #[test]
    fn test_insert_step_after_preserves_state() {
        let mut plan = PlanState::create_plan(vec![
            "First".to_string(),
            "Second".to_string(),
            "Third".to_string(),
        ]);
        assert!(plan.insert_step_after(0, "Inserted".to_string()).is_ok());
        assert_eq!(plan.steps.len(), 4);
        assert_eq!(plan.steps[1].description, "Inserted");
        assert_eq!(plan.steps[1].index, 1);
        assert_eq!(plan.steps[1].status, PlanStepStatus::Pending);
        assert_eq!(plan.current_step_index, 0);
    }

    #[test]
    fn test_insert_step_before_current_advances_index() {
        let mut plan = PlanState::create_plan(vec![
            "First".to_string(),
            "Second".to_string(),
            "Third".to_string(),
        ]);
        plan.current_step_index = 1;
        assert!(plan.insert_step_after(0, "Inserted".to_string()).is_ok());
        assert_eq!(plan.current_step_index, 2);
        assert_eq!(plan.steps[2].description, "Second");
    }

    #[test]
    fn test_insert_step_after_at_end() {
        let mut plan = PlanState::create_plan(vec![
            "First".to_string(),
            "Second".to_string(),
        ]);
        assert!(plan.insert_step_after(usize::MAX, "Last".to_string()).is_ok());
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.steps[2].description, "Last");
        assert_eq!(plan.steps[2].index, 2);
    }

    #[test]
    fn test_mark_step() {
        let mut plan = PlanState::create_plan(vec![
            "Step one".to_string(),
            "Step two".to_string(),
        ]);
        plan.mark_step(0, PlanStepStatus::Running).unwrap();
        assert_eq!(plan.steps[0].status, PlanStepStatus::Running);
    }

    #[test]
    fn test_advance() {
        let mut plan = PlanState::create_plan(vec![
            "Step one".to_string(),
            "Step two".to_string(),
            "Step three".to_string(),
        ]);
        plan.mark_step(0, PlanStepStatus::Running).unwrap();

        let next_idx = plan.advance();
        assert!(next_idx.is_some());
        assert_eq!(next_idx.unwrap(), 1);
        assert_eq!(plan.steps[1].description, "Step two");
        assert_eq!(plan.steps[0].status, PlanStepStatus::Done);
        assert_eq!(plan.steps[1].status, PlanStepStatus::Running);
    }

    #[test]
    fn test_completion_detection() {
        let mut plan = PlanState::create_plan(vec![
            "Step one".to_string(),
            "Step two".to_string(),
        ]);
        assert!(!plan.is_complete());

        plan.mark_step(0, PlanStepStatus::Done).unwrap();
        plan.mark_step(1, PlanStepStatus::Done).unwrap();
        assert!(plan.is_complete());
    }

    #[test]
    fn test_parse_plan_dot_format() {
        let text = "1. First step\n2. Second step\n3. Third step";
        let steps = PlanState::parse_plan(text).unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0], "First step");
        assert_eq!(steps[1], "Second step");
        assert_eq!(steps[2], "Third step");
    }

    #[test]
    fn test_parse_plan_paren_format() {
        let text = "1) First step\n2) Second step";
        let steps = PlanState::parse_plan(text).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0], "First step");
        assert_eq!(steps[1], "Second step");
    }

    #[test]
    fn test_parse_plan_empty_returns_none() {
        assert!(PlanState::parse_plan("").is_none());
        assert!(PlanState::parse_plan("no numbers here").is_none());
    }

    #[test]
    fn test_format_state() {
        let plan = PlanState::create_plan(vec![
            "Step one".to_string(),
            "Step two".to_string(),
        ]);
        let state = plan.format_state();
        assert!(state.contains("approved: false"));
        assert!(state.contains("1. [pending] Step one"));
        assert!(state.contains("2. [pending] Step two"));
    }

    #[test]
    fn test_status_summary() {
        let mut plan = PlanState::create_plan(vec![
            "Step one".to_string(),
            "Step two".to_string(),
        ]);
        assert_eq!(plan.status_summary(), "Plan: awaiting approval");

        plan.approved = true;
        plan.mark_step(0, PlanStepStatus::Running).unwrap();
        assert_eq!(plan.status_summary(), "Plan: 1/2 running");

        plan.paused = true;
        assert_eq!(plan.status_summary(), "Plan: paused at 1/2");

        plan.paused = false;
        plan.mark_step(0, PlanStepStatus::Failed).unwrap();
        assert_eq!(plan.status_summary(), "Plan: failed at 1/2");

        plan.mark_step(0, PlanStepStatus::Done).unwrap();
        plan.mark_step(1, PlanStepStatus::Done).unwrap();
        plan.complete = true;
        assert!(plan.is_complete());
        assert_eq!(plan.status_summary(), "Plan: complete 2/2");
    }

    #[test]
    fn test_parse_plan_step_prefix_format() {
        let text = "Step 1: do this\nStep 2: do that";
        let steps = PlanState::parse_plan(text).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0], "do this");
        assert_eq!(steps[1], "do that");
    }

    #[test]
    fn test_parse_plan_mixed_formats() {
        let text = "1. First numbered step\nStep 2: A prefix step\n3) Third numbered step";
        let steps = PlanState::parse_plan(text).unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0], "First numbered step");
        assert_eq!(steps[1], "A prefix step");
        assert_eq!(steps[2], "Third numbered step");
    }

    #[test]
    fn test_remove_step() {
        let mut plan = PlanState::create_plan(vec![
            "First".to_string(),
            "Second".to_string(),
            "Third".to_string(),
        ]);
        plan.current_step_index = 1;
        plan.remove_step(1).unwrap();
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[0].description, "First");
        assert_eq!(plan.steps[1].description, "Third");
        assert_eq!(plan.steps[0].index, 0);
        assert_eq!(plan.steps[1].index, 1);
        assert_eq!(plan.current_step_index, 1);
    }

    #[test]
    fn test_remove_step_before_current_updates_index() {
        let mut plan = PlanState::create_plan(vec![
            "First".to_string(),
            "Second".to_string(),
            "Third".to_string(),
        ]);
        plan.current_step_index = 2;
        plan.remove_step(0).unwrap();
        assert_eq!(plan.current_step_index, 1);
        assert_eq!(plan.steps[0].description, "Second");
        assert_eq!(plan.steps[1].description, "Third");
    }

    #[test]
    fn test_remove_current_last_step_clamps_index() {
        let mut plan = PlanState::create_plan(vec![
            "First".to_string(),
            "Second".to_string(),
        ]);
        plan.current_step_index = 1;
        plan.remove_step(1).unwrap();
        assert_eq!(plan.current_step_index, 0);
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].description, "First");
    }

    #[test]
    fn test_remove_step_invalid_index() {
        let mut plan = PlanState::create_plan(vec![
            "First".to_string(),
            "Second".to_string(),
        ]);
        let err = plan.remove_step(5).unwrap_err();
        assert!(err.contains("out of range"));
    }

    #[test]
    fn test_advance_to_completion() {
        let mut plan = PlanState::create_plan(vec![
            "Step 1".to_string(),
            "Step 2".to_string(),
            "Step 3".to_string(),
        ]);

        plan.mark_step(0, PlanStepStatus::Running).unwrap();
        assert_eq!(plan.advance().unwrap(), 1);
        assert_eq!(plan.steps[0].status, PlanStepStatus::Done);
        assert_eq!(plan.steps[1].status, PlanStepStatus::Running);

        assert_eq!(plan.advance().unwrap(), 2);
        assert_eq!(plan.steps[1].status, PlanStepStatus::Done);
        assert_eq!(plan.steps[2].status, PlanStepStatus::Running);

        assert!(plan.advance().is_none());
        assert!(plan.is_complete());
        assert!(plan.complete);
    }

    #[test]
    fn test_advance_only_runs_pending_steps() {
        let mut plan = PlanState::create_plan(vec![
            "Step 1".to_string(),
            "Step 2".to_string(),
            "Step 3".to_string(),
        ]);

        plan.mark_step(0, PlanStepStatus::Done).unwrap();
        plan.current_step_index = 1;
        plan.mark_step(1, PlanStepStatus::Running).unwrap();

        assert_eq!(plan.advance().unwrap(), 2);
        assert_eq!(plan.steps[2].status, PlanStepStatus::Running);
        assert!(!plan.is_complete());
    }

    #[test]
    fn test_format_state_with_running_step() {
        let mut plan = PlanState::create_plan(vec![
            "Step 1".to_string(),
            "Step 2".to_string(),
        ]);
        plan.mark_step(0, PlanStepStatus::Running).unwrap();
        plan.approved = true;
        plan.paused = false;
        let state = plan.format_state();
        assert!(state.contains("approved: true"));
        assert!(state.contains("paused: false"));
        assert!(state.contains("[running] Step 1"));
        assert!(state.contains("[pending] Step 2"));
        assert!(state.contains("Current step:"));
    }

    #[test]
    fn test_format_display() {
        let mut plan = PlanState::create_plan(vec![
            "Step 1".to_string(),
            "Step 2".to_string(),
        ]);
        plan.mark_step(0, PlanStepStatus::Done).unwrap();
        plan.mark_step(1, PlanStepStatus::Running).unwrap();
        let display = plan.format_display();
        assert!(display.contains("✓ 1. Step 1"));
        assert!(display.contains("→ 2. Step 2"));
        assert!(display.contains("Progress: 1/2"));
    }

    #[test]
    fn test_status_summary_all_states() {
        let mut plan = PlanState::create_plan(vec![
            "Step 1".to_string(),
            "Step 2".to_string(),
            "Step 3".to_string(),
        ]);

        assert_eq!(plan.status_summary(), "Plan: awaiting approval");

        plan.approved = true;
        plan.mark_step(0, PlanStepStatus::Running).unwrap();
        assert_eq!(plan.status_summary(), "Plan: 1/3 running");

        plan.paused = true;
        assert_eq!(plan.status_summary(), "Plan: paused at 1/3");

        plan.paused = false;
        plan.mark_step(0, PlanStepStatus::Failed).unwrap();
        assert_eq!(plan.status_summary(), "Plan: failed at 1/3");

        plan.mark_step(0, PlanStepStatus::Done).unwrap();
        plan.mark_step(1, PlanStepStatus::Done).unwrap();
        plan.mark_step(2, PlanStepStatus::Done).unwrap();
        plan.complete = true;
        assert_eq!(plan.status_summary(), "Plan: complete 3/3");
    }

    #[test]
    fn test_renumber_after_remove() {
        let mut plan = PlanState::create_plan(vec![
            "A".to_string(),
            "B".to_string(),
            "C".to_string(),
            "D".to_string(),
        ]);
        plan.remove_step(1).unwrap();
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.steps[0].index, 0);
        assert_eq!(plan.steps[1].index, 1);
        assert_eq!(plan.steps[2].index, 2);
        assert_eq!(plan.steps[0].description, "A");
        assert_eq!(plan.steps[1].description, "C");
        assert_eq!(plan.steps[2].description, "D");
    }

    #[test]
    fn test_renumber_after_insert() {
        let mut plan = PlanState::create_plan(vec![
            "A".to_string(),
            "C".to_string(),
        ]);
        plan.insert_step_after(0, "B".to_string()).unwrap();
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.steps[0].index, 0);
        assert_eq!(plan.steps[1].index, 1);
        assert_eq!(plan.steps[2].index, 2);
    }

    #[test]
    fn test_plan_state_pause_resume_cycle() {
        let mut plan = PlanState::create_plan(vec![
            "Step 1".to_string(),
            "Step 2".to_string(),
        ]);
        plan.approved = true;
        plan.mark_step(0, PlanStepStatus::Running).unwrap();

        plan.paused = true;
        assert!(plan.paused);

        plan.paused = false;
        assert!(!plan.paused);
        assert!(plan.approved);
    }

    #[test]
    fn test_abort_clears_plan() {
        let mut plan = PlanState::create_plan(vec![
            "Step 1".to_string(),
            "Step 2".to_string(),
        ]);
        plan.approved = true;
        plan.mark_step(0, PlanStepStatus::Running).unwrap();
        plan.current_step_index = plan.steps.len();
        plan.approved = false;
        assert!(!plan.approved);
        assert_eq!(plan.current_step_index, 2);
    }

    #[test]
    fn test_format_state_approval_mode() {
        let plan = PlanState::create_plan(vec![
            "Step one".to_string(),
            "Step two".to_string(),
        ]);
        let state = plan.format_state();
        assert!(state.contains("mode: APPROVAL"));
        assert!(state.contains("approved: false"));
    }

    #[test]
    fn test_format_state_failed_mode() {
        let mut plan = PlanState::create_plan(vec![
            "Step one".to_string(),
            "Step two".to_string(),
        ]);
        plan.approved = true;
        plan.mark_step(0, PlanStepStatus::Running).unwrap();
        plan.mark_step(0, PlanStepStatus::Failed).unwrap();
        let state = plan.format_state();
        assert!(state.contains("mode: FAILED"));
    }

    #[test]
    fn test_format_state_paused_mode() {
        let mut plan = PlanState::create_plan(vec![
            "Step one".to_string(),
            "Step two".to_string(),
        ]);
        plan.approved = true;
        plan.mark_step(0, PlanStepStatus::Running).unwrap();
        plan.paused = true;
        let state = plan.format_state();
        assert!(state.contains("mode: PAUSED"));
    }

    #[test]
    fn test_format_state_act_mode() {
        let mut plan = PlanState::create_plan(vec![
            "Step one".to_string(),
            "Step two".to_string(),
        ]);
        plan.approved = true;
        plan.mark_step(0, PlanStepStatus::Running).unwrap();
        let state = plan.format_state();
        assert!(state.contains("mode: ACT"));
    }

    #[test]
    fn test_format_state_complete_mode() {
        let mut plan = PlanState::create_plan(vec![
            "Step one".to_string(),
            "Step two".to_string(),
        ]);
        plan.complete = true;
        let state = plan.format_state();
        assert!(state.contains("mode: COMPLETE"));
    }

    #[test]
    fn test_current_step_returns_none_on_empty() {
        let plan = PlanState::create_plan(vec![]);
        assert!(plan.current_step().is_none());
    }

    #[test]
    fn test_current_step_returns_running_step() {
        let mut plan = PlanState::create_plan(vec![
            "Step 1".to_string(),
            "Step 2".to_string(),
        ]);
        plan.mark_step(0, PlanStepStatus::Running).unwrap();
        let step = plan.current_step().unwrap();
        assert_eq!(step.index, 0);
        assert_eq!(step.status, PlanStepStatus::Running);
    }

    #[test]
    fn test_status_icon() {
        let mut plan = PlanState::create_plan(vec![
            "Pending step".to_string(),
            "Running step".to_string(),
            "Done step".to_string(),
            "Failed step".to_string(),
        ]);
        assert_eq!(plan.steps[0].status_icon(), "○");
        plan.mark_step(1, PlanStepStatus::Running).unwrap();
        assert_eq!(plan.steps[1].status_icon(), "→");
        plan.mark_step(2, PlanStepStatus::Done).unwrap();
        assert_eq!(plan.steps[2].status_icon(), "✓");
        plan.mark_step(3, PlanStepStatus::Failed).unwrap();
        assert_eq!(plan.steps[3].status_icon(), "✗");
    }

    #[test]
    fn test_approve_validates_current_step_index() {
        let mut plan = PlanState::create_plan(vec![
            "Step 1".to_string(),
            "Step 2".to_string(),
            "Step 3".to_string(),
        ]);
        // Mark step 0 as Done, step 1 as Failed, set current to step 1
        plan.mark_step(0, PlanStepStatus::Done).unwrap();
        plan.mark_step(1, PlanStepStatus::Failed).unwrap();
        plan.current_step_index = 1;
        // approve should find step 2 (first pending) not step 1 (failed)
        let start_index = if plan.current_step_index < plan.steps.len()
            && plan.steps[plan.current_step_index].status == PlanStepStatus::Pending
        {
            plan.current_step_index
        } else {
            plan.steps.iter().position(|s| s.status == PlanStepStatus::Pending)
                .unwrap_or(0)
        };
        plan.current_step_index = start_index;
        plan.approved = true;
        plan.steps[start_index].status = PlanStepStatus::Running;
        assert_eq!(start_index, 2);
        assert_eq!(plan.steps[2].status, PlanStepStatus::Running);
        assert_eq!(plan.steps[1].status, PlanStepStatus::Failed);
    }

    #[test]
    fn test_approve_finds_first_pending_when_index_out_of_bounds() {
        let mut plan = PlanState::create_plan(vec![
            "Step 1".to_string(),
            "Step 2".to_string(),
        ]);
        plan.current_step_index = 5; // out of bounds
        let start_index = if plan.current_step_index < plan.steps.len()
            && plan.steps[plan.current_step_index].status == PlanStepStatus::Pending
        {
            plan.current_step_index
        } else {
            plan.steps.iter().position(|s| s.status == PlanStepStatus::Pending)
                .unwrap_or(0)
        };
        assert_eq!(start_index, 0);
    }

    #[test]
    fn test_pause_resume_failure_cycle() {
        let mut plan = PlanState::create_plan(vec![
            "Step 1".to_string(),
            "Step 2".to_string(),
        ]);
        plan.approved = true;
        plan.mark_step(0, PlanStepStatus::Running).unwrap();

        // Simulate failure
        plan.mark_step(0, PlanStepStatus::Failed).unwrap();
        plan.paused = true;
        assert!(plan.paused);
        assert_eq!(plan.steps[0].status, PlanStepStatus::Failed);

        // Resume: mark failed step as running again
        plan.paused = false;
        plan.mark_step(0, PlanStepStatus::Running).unwrap();
        assert!(!plan.paused);
        assert_eq!(plan.steps[0].status, PlanStepStatus::Running);
    }

    #[test]
    fn test_parse_plan_rejects_unnumbered_bullets() {
        let text = "- first step\n- second step";
        let result = PlanState::parse_plan(text);
        assert!(result.is_none(), "unnumbered bullets should be rejected");
    }

    #[test]
    fn test_parse_plan_accepts_step_prefix_with_bullet() {
        let text = "- Step 1: first step\n- Step 2: second step";
        let steps = PlanState::parse_plan(text).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0], "first step");
        assert_eq!(steps[1], "second step");
    }

    #[test]
    fn test_parse_plan_accepts_single_step() {
        let text = "1. Only one step";
        let steps = PlanState::parse_plan(text).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0], "Only one step");
    }

    #[test]
    fn test_advance_handles_out_of_bounds_index() {
        let mut plan = PlanState::create_plan(vec!["Step 1".to_string()]);
        plan.current_step_index = 99; // Out of bounds
        assert!(plan.advance().is_none());
        assert!(!plan.complete);
    }

    #[test]
    fn test_insert_step_at_beginning() {
        let mut plan = PlanState::create_plan(vec![
            "Step 1".to_string(),
            "Step 2".to_string(),
        ]);
        plan.current_step_index = 1;
        plan.insert_step_at_beginning("Inserted first".to_string()).unwrap();

        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.steps[0].description, "Inserted first");
        assert_eq!(plan.steps[1].description, "Step 1");
        assert_eq!(plan.steps[2].description, "Step 2");
        assert_eq!(plan.current_step_index, 2);
    }
}
