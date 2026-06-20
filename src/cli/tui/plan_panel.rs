//! Plan panel widget for displaying the interactive plan workflow.

use super::theme;
use crate::cli::markdown::render_markdown;
use crate::core::plan_state::{PlanState, PlanStepStatus};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Paragraph, Wrap},
};

/// Render the plan panel within the given area.
pub fn render_plan_panel(plan: &PlanState, frame: &mut Frame, area: Rect) {
    let lines = build_plan_lines(plan, area);

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

fn build_plan_lines(plan: &PlanState, area: Rect) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    // Status line
    let has_failed = plan
        .steps
        .iter()
        .any(|s| s.status == PlanStepStatus::Failed);
    let status = if plan.complete {
        "complete".to_string()
    } else if has_failed {
        "failed".to_string()
    } else if plan.paused {
        "paused".to_string()
    } else if plan.approved {
        if plan.current_step_index < plan.steps.len() {
            format!("{} / {}", plan.current_step_index + 1, plan.steps.len())
        } else {
            format!(
                "error: current step index {} is out of range ({} steps)",
                plan.current_step_index,
                plan.steps.len()
            )
        }
    } else {
        "awaiting approval".to_string()
    };

    lines.push(Line::from(Span::styled(
        format!("Status: {}", status),
        Style::default().fg(theme::ACCENT),
    )));

    // Progress bar
    let done_count = plan
        .steps
        .iter()
        .filter(|s| s.status == PlanStepStatus::Done)
        .count();
    let total = plan.steps.len();
    let progress_pct = if total > 0 {
        (done_count as f64 / total as f64) * 100.0
    } else {
        0.0
    };
    let text_overhead = 25; // "[{}] {:.0}% ({}/{})"
    let available = area.width as i32 - 2 - text_overhead; // 2 for block padding
    let bar_width = if available > 0 {
        available as usize
    } else {
        20
    };
    let filled = (progress_pct / 100.0 * bar_width as f64) as usize;
    let bar_str: String = "█".repeat(filled) + &"░".repeat(bar_width.saturating_sub(filled));
    lines.push(Line::from(Span::styled(
        format!(
            "[{}] {:.0}% ({}/{})",
            bar_str, progress_pct, done_count, total
        ),
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

        // Render description as markdown, then prepend the status icon + step number
        // to the first line.
        let rendered = render_markdown(None, &step.description);
        let prefix = Span::styled(format!("{} {}.", step.status_icon(), step.index + 1), style);
        for (i, mut line) in rendered.into_iter().enumerate() {
            if i == 0 {
                // Prepend the prefix to the first line
                let mut new_spans = vec![prefix.clone()];
                new_spans.extend(line.spans);
                line = Line::from(new_spans);
            }
            // Apply the step's style to all spans
            for span in &mut line.spans {
                span.style = span.style.patch(style);
            }
            lines.push(line);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_plan_lines_shows_error_when_current_step_is_out_of_range() {
        let mut plan =
            PlanState::create_plan(vec!["First step".to_string(), "Second step".to_string()]);
        plan.approved = true;
        plan.current_step_index = 99;

        let lines = build_plan_lines(
            &plan,
            Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 10,
            },
        );

        let status_line = lines
            .first()
            .expect("plan panel should render a status line");
        let rendered_status = status_line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(rendered_status.contains("error: current step index 99 is out of range"));
        assert!(!rendered_status.contains("100 / 2"));
    }
}
