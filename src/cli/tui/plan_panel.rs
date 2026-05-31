//! Plan panel widget for displaying the interactive plan workflow.

use crate::core::plan_state::{PlanState, PlanStepStatus};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Paragraph, Wrap},
};
use super::theme;

/// Render the plan panel within the given area.
pub fn render_plan_panel(plan: &PlanState, frame: &mut Frame, area: Rect) {
    let lines = build_plan_lines(plan);

    let block = Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::ACCENT))
        .title(Span::styled(" Plan ", Style::default().fg(theme::ACCENT)));

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .style(Style::default().fg(theme::ACCENT));

    frame.render_widget(paragraph, area);
}

fn build_plan_lines(plan: &PlanState) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    // Status line
    let has_failed = plan.steps.iter().any(|s| s.status == PlanStepStatus::Failed);
    let status = if plan.complete {
        "complete".to_string()
    } else if has_failed {
        "failed".to_string()
    } else if plan.paused {
        "paused".to_string()
    } else if plan.approved {
        format!("{} / {}", plan.current_step_index + 1, plan.steps.len())
    } else {
        "awaiting approval".to_string()
    };

    lines.push(Line::from(Span::styled(
        format!("Status: {}", status),
        Style::default().fg(theme::ACCENT),
    )));

    // Progress bar
    let done_count = plan.steps.iter().filter(|s| s.status == PlanStepStatus::Done).count();
    let total = plan.steps.len();
    let progress_pct = if total > 0 { (done_count as f64 / total as f64) * 100.0 } else { 0.0 };
    let bar_width: usize = 20;
    let filled = (progress_pct / 100.0 * bar_width as f64) as usize;
    let bar_str: String = "█".repeat(filled) + &"░".repeat(bar_width.saturating_sub(filled));
    lines.push(Line::from(Span::styled(
        format!("[{}] {:.0}% ({}/{})", bar_str, progress_pct, done_count, total),
        theme::dim_style(),
    )));

    lines.push(Line::from(""));

    // Step list
    for step in &plan.steps {
        let is_current = plan.approved && step.index == plan.current_step_index;
        let style = if is_current {
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        lines.push(Line::from(vec![
            Span::styled(format!("{} {}.", step.status_icon(), step.index + 1), style),
            Span::styled(format!(" {}", step.description), style),
        ]));
    }

    // Approval prompt when not yet approved
    if !plan.approved && !plan.complete {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Type /plan approve to begin execution",
            theme::dim_style(),
        )));
    }

    lines
}