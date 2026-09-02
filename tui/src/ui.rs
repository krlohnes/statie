//! Rendering: pure `draw(frame, &AppState)`, unit-testable via
//! `ratatui::backend::TestBackend`.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{AppState, HistoryItem};

/// Renders one history item as one or more plain lines of text (with a
/// dimmed style for reasoning/system-ish content).
pub fn history_item_lines(item: &HistoryItem) -> Vec<Line<'static>> {
    match item {
        HistoryItem::User(text) => vec![Line::from(format!("> {text}"))],
        HistoryItem::ReasoningStreaming(text) => {
            let mut lines: Vec<Line<'static>> = Vec::new();
            for line in text.lines() {
                lines.push(Line::from(Span::styled(
                    line.to_string(),
                    Style::default().add_modifier(Modifier::DIM),
                )));
            }
            if lines.is_empty() {
                lines.push(Line::from(Span::styled(
                    String::new(),
                    Style::default().add_modifier(Modifier::DIM),
                )));
            }
            lines
        }
        HistoryItem::ReasoningCollapsed(len) => vec![Line::from(Span::styled(
            format!("\u{25b8} reasoning ({len} chars) \u{2014} /reasoning to expand"),
            Style::default().add_modifier(Modifier::DIM),
        ))],
        HistoryItem::Patch(text) => vec![Line::from(text.clone())],
        HistoryItem::Action(text) => vec![Line::from(format!("\u{2192} {text}"))],
        HistoryItem::ToolOutput(lines) => {
            let mut out: Vec<Line<'static>> = Vec::new();
            let shown = lines.iter().take(8);
            for l in shown {
                out.push(Line::from(l.clone()));
            }
            if lines.len() > 8 {
                out.push(Line::from(format!("\u{2026} (+{} lines)", lines.len() - 8)));
            }
            out
        }
        HistoryItem::System(text) => vec![Line::from(Span::styled(
            text.clone(),
            Style::default().add_modifier(Modifier::DIM),
        ))],
        HistoryItem::Parked(text) => vec![Line::from(format!("\u{23f8} waiting for you: {text}"))],
        HistoryItem::Info(text) => text
            .lines()
            .map(|l| Line::from(l.to_string()))
            .collect::<Vec<_>>(),
        HistoryItem::Error(text) => vec![Line::from(format!("error: {text}"))],
    }
}

fn status_line(state: &AppState) -> Line<'static> {
    Line::from(format!(
        "skill: {} | step: {} | version: {} | prompt tokens: {} | ctrl-c: {}",
        state.skill,
        state.step,
        state.version,
        state
            .prompt_tokens
            .map(|t| t.to_string())
            .unwrap_or_else(|| "-".to_string()),
        if state.step_in_flight {
            "cancel"
        } else {
            "quit"
        },
    ))
}

/// Draws the whole app: a scrolling history pane (all but the last 3 rows)
/// and an input line + status bar.
pub fn draw(frame: &mut Frame, state: &AppState) {
    let area = frame.area();
    let [history_area, input_area, status_area] =
        Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);

    draw_history(frame, history_area, state);
    draw_input(frame, input_area, state);
    draw_status(frame, status_area, state);
}

fn draw_history(frame: &mut Frame, area: Rect, state: &AppState) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for item in &state.history {
        lines.extend(history_item_lines(item));
    }
    let total = lines.len();
    let height = area.height as usize;
    let max_scroll = total.saturating_sub(height);
    // `scroll` is measured in lines from the bottom (0 = pinned to latest).
    let scroll = state.scroll.min(max_scroll);
    let end = total.saturating_sub(scroll);
    let start = end.saturating_sub(height.min(end));
    let visible: Vec<Line<'static>> = lines[start..end].to_vec();

    let paragraph = Paragraph::new(visible).block(Block::default().borders(Borders::NONE));
    frame.render_widget(paragraph, area);
}

fn draw_input(frame: &mut Frame, area: Rect, state: &AppState) {
    let line = Line::from(format!("> {}", state.input));
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_status(frame: &mut Frame, area: Rect, state: &AppState) {
    frame.render_widget(Paragraph::new(status_line(state)), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppState;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let mut s = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                s.push_str(buffer.cell((x, y)).unwrap().symbol());
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn draws_collapsed_reasoning_user_message_and_status_bar() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut state = AppState::new();
        state.skill = "toy".to_string();
        state.step = 3;
        state.version = 3;
        state.prompt_tokens = Some(123);
        state.history.push(HistoryItem::ReasoningCollapsed(42));
        state.history.push(HistoryItem::User("hello there".to_string()));

        terminal.draw(|f| draw(f, &state)).unwrap();

        let text = buffer_text(&terminal);
        assert!(
            text.contains("\u{25b8} reasoning ("),
            "expected collapsed reasoning marker in:\n{text}"
        );
        assert!(text.contains("hello there"), "expected user message in:\n{text}");
        assert!(text.contains("skill:"), "expected status bar in:\n{text}");
    }
}
