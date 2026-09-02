//! Rendering: pure `draw(frame, &AppState)`, unit-testable via
//! `ratatui::backend::TestBackend`.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{AppState, HistoryItem};

/// Renders one history item as one or more styled lines of text.
pub fn history_item_lines(item: &HistoryItem) -> Vec<Line<'static>> {
    match item {
        HistoryItem::User(text) => vec![Line::from(vec![
            Span::styled(
                "you \u{203a} ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                text.clone(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ])],
        HistoryItem::ReasoningStreaming(text) => {
            let all_lines: Vec<&str> = text.lines().collect();
            let total = all_lines.len();
            let shown: Vec<&str> = if total > 6 {
                all_lines[total - 6..].to_vec()
            } else {
                all_lines.clone()
            };
            let style = Style::default()
                .add_modifier(Modifier::DIM)
                .add_modifier(Modifier::ITALIC);
            let mut lines: Vec<Line<'static>> = Vec::new();
            lines.push(Line::from(Span::styled(
                format!("\u{2026} thinking ({} chars)", text.chars().count()),
                style,
            )));
            for line in shown {
                lines.push(Line::from(Span::styled(line.to_string(), style)));
            }
            lines
        }
        HistoryItem::ReasoningCollapsed(len) => vec![Line::from(Span::styled(
            format!("\u{25b8} reasoning ({len} chars) \u{2014} /reasoning to expand"),
            Style::default().add_modifier(Modifier::DIM),
        ))],
        HistoryItem::Patch(text) => vec![Line::from(Span::styled(
            text.clone(),
            Style::default().fg(Color::Green),
        ))],
        HistoryItem::Action(text) => vec![Line::from(Span::styled(
            format!("\u{2192} {text}"),
            Style::default().fg(Color::Yellow),
        ))],
        HistoryItem::ToolOutput(lines) => {
            let mut out: Vec<Line<'static>> = Vec::new();
            let mut shown = lines.iter().take(12);
            if let Some(first) = shown.next() {
                out.push(Line::from(vec![
                    Span::styled("tool: ", Style::default().add_modifier(Modifier::DIM)),
                    Span::raw(first.clone()),
                ]));
            }
            for l in shown {
                out.push(Line::from(l.clone()));
            }
            if lines.len() > 12 {
                out.push(Line::from(Span::styled(
                    format!("\u{2026} (+{} lines)", lines.len() - 12),
                    Style::default().add_modifier(Modifier::DIM),
                )));
            }
            out
        }
        HistoryItem::System(text) => vec![Line::from(Span::styled(
            text.clone(),
            Style::default().add_modifier(Modifier::DIM),
        ))],
        HistoryItem::Parked(text) => vec![Line::from(Span::styled(
            format!("\u{23f8} waiting for you: {text}"),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ))],
        HistoryItem::Info(text) => text
            .lines()
            .map(|l| Line::from(l.to_string()))
            .collect::<Vec<_>>(),
        HistoryItem::Error(text) => vec![Line::from(Span::styled(
            format!("\u{2716} {text}"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ))],
    }
}

/// Splits `lines` into visual lines no wider than `width` display columns,
/// preserving each span's style. Breaks at spaces when possible; hard-breaks
/// otherwise. `width == 0` returns the input unchanged (nothing sane to do).
pub fn wrap_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return lines;
    }

    let mut out: Vec<Line<'static>> = Vec::new();
    for line in lines {
        let alignment = line.alignment;
        let style = line.style;
        let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
        let mut row_width: usize = 0;

        for span in line.spans {
            let span_style = span.style;
            // Split the span's content into "words" (runs of non-space) and
            // single-space separators, so we can break at spaces when
            // possible.
            let mut word = String::new();
            let flush_word =
                |word: &mut String, rows: &mut Vec<Vec<Span<'static>>>, row_width: &mut usize| {
                    if word.is_empty() {
                        return;
                    }
                    push_chunk(word, span_style, rows, row_width, width);
                    word.clear();
                };

            for ch in span.content.chars() {
                if ch == ' ' {
                    flush_word(&mut word, &mut rows, &mut row_width);
                    push_chunk(" ", span_style, &mut rows, &mut row_width, width);
                } else if ch == '\n' {
                    flush_word(&mut word, &mut rows, &mut row_width);
                    rows.push(Vec::new());
                    row_width = 0;
                } else {
                    word.push(ch);
                }
            }
            flush_word(&mut word, &mut rows, &mut row_width);
        }

        for spans in rows {
            let mut l = Line::from(spans);
            l.alignment = alignment;
            l.style = style;
            out.push(l);
        }
    }
    out
}

/// Appends `chunk` (a single word or a single space) to the current row of
/// `rows`, wrapping to a new row first if it would overflow `width`. A chunk
/// wider than `width` on its own is hard-broken by character.
fn push_chunk(
    chunk: &str,
    style: Style,
    rows: &mut Vec<Vec<Span<'static>>>,
    row_width: &mut usize,
    width: usize,
) {
    let chunk_width = chunk.width();
    if chunk_width <= width {
        if *row_width + chunk_width > width {
            // A trailing space that doesn't fit is simply dropped at the
            // wrap point; anything else starts a new row.
            if chunk == " " {
                rows.push(Vec::new());
                *row_width = 0;
                return;
            }
            rows.push(Vec::new());
            *row_width = 0;
        }
        rows.last_mut()
            .unwrap()
            .push(Span::styled(chunk.to_string(), style));
        *row_width += chunk_width;
        return;
    }

    // The chunk itself is wider than the available width: hard-break it by
    // character.
    let mut buf = String::new();
    let mut buf_width = 0usize;
    for ch in chunk.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if *row_width + buf_width + cw > width {
            if !buf.is_empty() {
                rows.last_mut()
                    .unwrap()
                    .push(Span::styled(std::mem::take(&mut buf), style));
                buf_width = 0;
            }
            rows.push(Vec::new());
            *row_width = 0;
        }
        buf.push(ch);
        buf_width += cw;
    }
    if !buf.is_empty() {
        rows.last_mut().unwrap().push(Span::styled(buf, style));
        *row_width += buf_width;
    }
}

fn spinner_frame(tick: usize) -> char {
    const FRAMES: [char; 8] = [
        '\u{280b}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283c}', '\u{2834}', '\u{2826}',
        '\u{2827}',
    ];
    FRAMES[tick % FRAMES.len()]
}

fn status_line(state: &AppState) -> Line<'static> {
    let prompt = state
        .prompt_tokens
        .map(|t| t.to_string())
        .unwrap_or_else(|| "-".to_string());
    let server_prompt = state
        .server_prompt_tokens
        .map(|t| t.to_string())
        .unwrap_or_else(|| "-".to_string());

    let activity = if state.step_in_flight {
        let elapsed = state
            .step_started
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        format!(
            "{} thinking {}s",
            spinner_frame(state.spinner_tick),
            elapsed
        )
    } else {
        "idle".to_string()
    };

    let ctrl_c = if state.step_in_flight {
        "ctrl-c: cancel"
    } else {
        "ctrl-c: quit"
    };

    Line::from(Span::styled(
        format!(
            " skill {} \u{2502} step {} \u{2502} v{} \u{2502} prompt \u{2248}{} tok (srv {}) \u{2502} {} \u{2502} {}",
            state.skill, state.step, state.version, prompt, server_prompt, activity, ctrl_c,
        ),
        Style::default().add_modifier(Modifier::REVERSED),
    ))
}

/// Minimum terminal height (rows) required to draw the completion popup:
/// enough for at least a sliver of history, the popup itself (border + one
/// candidate row), the input pane, and the status bar.
const MIN_HEIGHT_FOR_POPUP: u16 = 8;

/// Draws the whole app: a bordered history pane, a bordered input pane, and
/// a status bar. When `state.completion` has candidates and the terminal is
/// tall enough, a popup is drawn above the input pane, overlapping the
/// bottom of the history pane.
pub fn draw(frame: &mut Frame, state: &AppState) {
    let area = frame.area();
    let [history_area, input_area, status_area] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_history(frame, history_area, state);
    draw_input(frame, input_area, state);
    draw_status(frame, status_area, state);

    if let Some(completion) = &state.completion
        && area.height >= MIN_HEIGHT_FOR_POPUP
    {
        draw_completion_popup(frame, history_area, input_area, completion);
    }
}

/// Draws the autocomplete popup: bordered, at most 6 candidate rows (scrolled
/// so the selected row stays visible), positioned directly above
/// `input_area` and overlapping the bottom of `history_area`. Each row shows
/// `/{name} {args}` on the left and the help text dim on the right; the
/// selected row is reversed.
fn draw_completion_popup(
    frame: &mut Frame,
    history_area: Rect,
    input_area: Rect,
    completion: &crate::app::Completion,
) {
    const MAX_ROWS: usize = 6;

    let visible_rows = completion.candidates.len().min(MAX_ROWS);
    let wanted_height = (visible_rows as u16) + 2; // borders
    let popup_width = input_area.width;
    if popup_width == 0 || wanted_height == 0 {
        return;
    }

    // Clamp the popup into the history pane's vertical span: it overlaps
    // the bottom of history but never draws above its top border.
    let popup_height = wanted_height.min(history_area.height);
    if popup_height < 2 {
        return;
    }
    let popup_area = Rect {
        x: input_area.x,
        y: input_area
            .y
            .saturating_sub(popup_height)
            .max(history_area.y),
        width: popup_width,
        height: popup_height,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded);
    let inner = block.inner(popup_area);
    let inner_width = inner.width as usize;

    // Scroll the candidate window so the selected row stays visible.
    let total = completion.candidates.len();
    let window = (inner.height as usize).max(1);
    let start = if completion.selected >= window {
        completion.selected + 1 - window
    } else {
        0
    };
    let start = start.min(total.saturating_sub(window.min(total)));
    let end = (start + window).min(total);

    let mut rows: Vec<Line<'static>> = Vec::new();
    for (idx, candidate) in completion.candidates[start..end].iter().enumerate() {
        let global_idx = start + idx;
        let (name, args, help) = command_row_parts(candidate);
        let left = if args.is_empty() {
            format!("/{name}")
        } else {
            format!("/{name} {args}")
        };
        let selected = global_idx == completion.selected;
        let base_style = if selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        let help_style = if selected {
            base_style
        } else {
            base_style.add_modifier(Modifier::DIM)
        };

        let left_width = left.width();
        let help_width = help.width();
        let pad = inner_width.saturating_sub(left_width + help_width);

        let mut spans = vec![Span::styled(left, base_style)];
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), base_style));
        }
        spans.push(Span::styled(help, help_style));
        rows.push(Line::from(spans));
    }

    let paragraph = Paragraph::new(rows).block(block);
    frame.render_widget(paragraph, popup_area);
}

/// Splits a completion candidate into `(name, args, help)` by looking it up
/// in `COMMANDS` (a command-name candidate) or falling back to a bare
/// argument candidate (a skill name or `/log` kind) with no args/help of its
/// own.
fn command_row_parts(candidate: &str) -> (&str, &'static str, &'static str) {
    if let Some(name) = candidate.strip_prefix('/')
        && let Some(spec) = crate::driver::COMMANDS.iter().find(|c| c.name == name)
    {
        return (spec.name, spec.args, spec.help);
    }
    (candidate, "", "")
}

fn draw_history(frame: &mut Frame, area: Rect, state: &AppState) {
    let title = format!("statie \u{b7} {} \u{b7} step {}", state.skill, state.step);
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .title(title);

    let inner = block.inner(area);
    let inner_width = inner.width as usize;

    let mut raw_lines: Vec<Line<'static>> = Vec::new();
    for item in &state.history {
        raw_lines.extend(history_item_lines(item));
    }
    let lines = wrap_lines(raw_lines, inner_width);

    let total = lines.len();
    let height = inner.height as usize;
    let max_scroll = total.saturating_sub(height);
    // `scroll` is measured in wrapped lines from the bottom (0 = pinned to
    // latest).
    let scroll = state.scroll.min(max_scroll);

    if scroll > 0 {
        block = block.title_bottom(
            Line::from(format!("\u{25b2} {scroll} more below")).alignment(Alignment::Right),
        );
    }

    let end = total.saturating_sub(scroll);
    let start = end.saturating_sub(height.min(end));
    let visible: Vec<Line<'static>> = lines[start..end].to_vec();

    let paragraph = Paragraph::new(visible).block(block);
    frame.render_widget(paragraph, area);
}

/// Computes the visible tail of `input` that fits in `width` columns, and
/// the display-column offset of the cursor (end of text) within that tail.
pub fn input_tail(input: &str, width: usize) -> (&str, usize) {
    if width == 0 {
        return ("", 0);
    }
    let total_width = input.width();
    if total_width <= width {
        return (input, total_width);
    }
    // Walk back from the end, accumulating display width, until we've taken
    // as much as fits in `width` columns.
    let mut acc = 0usize;
    let mut start_byte = input.len();
    for (byte_idx, ch) in input.char_indices().rev() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if acc + cw > width {
            break;
        }
        acc += cw;
        start_byte = byte_idx;
    }
    let tail = &input[start_byte..];
    (tail, acc)
}

fn draw_input(frame: &mut Frame, area: Rect, state: &AppState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .title("message (Enter to send, /help)");
    let inner = block.inner(area);
    let inner_width = inner.width as usize;

    let prefix = "\u{203a} ";
    let prefix_width = prefix.width();
    let avail = inner_width.saturating_sub(prefix_width);
    let (tail, cursor_col) = input_tail(&state.input, avail);

    let line = Line::from(vec![Span::raw(prefix), Span::raw(tail.to_string())]);
    let paragraph = Paragraph::new(line).block(block);
    frame.render_widget(paragraph, area);

    let cursor_x = inner.x + (prefix_width + cursor_col) as u16;
    let cursor_y = inner.y;
    frame.set_cursor_position(Position::new(cursor_x, cursor_y));
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
    use ratatui::style::Modifier;

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
        state
            .history
            .push(HistoryItem::User("hello there".to_string()));

        terminal.draw(|f| draw(f, &state)).unwrap();

        let text = buffer_text(&terminal);
        assert!(
            text.contains("\u{25b8} reasoning ("),
            "expected collapsed reasoning marker in:\n{text}"
        );
        assert!(
            text.contains("hello there"),
            "expected user message in:\n{text}"
        );
        assert!(text.contains("skill"), "expected status bar in:\n{text}");
    }

    #[test]
    fn wide_tool_output_line_wraps_and_preserves_content() {
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        let long_line: String = (0..200).map(|i| (b'a' + (i % 26) as u8) as char).collect();
        let mut state = AppState::new();
        state
            .history
            .push(HistoryItem::ToolOutput(vec![long_line.clone()]));

        terminal.draw(|f| draw(f, &state)).unwrap();

        // The history pane is bordered: inner width is frame width - 2.
        let inner_width = 60 - 2;
        let lines = wrap_lines(
            history_item_lines(&HistoryItem::ToolOutput(vec![long_line.clone()])),
            inner_width,
        );
        assert!(
            lines.len() > 1,
            "expected the 200-char line to wrap across multiple rows"
        );
        for l in &lines {
            let w: usize = l.spans.iter().map(|s| s.content.width()).sum();
            assert!(
                w <= inner_width,
                "row {w} exceeds inner width {inner_width}"
            );
        }
        let concatenated: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        // The "tool: " prefix is prepended to the first line; strip it before
        // comparing the remainder of the content.
        let expected = format!("tool: {long_line}");
        assert_eq!(concatenated, expected);
    }

    #[test]
    fn narrow_backend_wraps_without_overflow() {
        let backend = TestBackend::new(30, 12);
        let mut terminal = Terminal::new(backend).unwrap();

        let long_line: String = (0..200).map(|i| (b'a' + (i % 26) as u8) as char).collect();
        let mut state = AppState::new();
        state.history.push(HistoryItem::ToolOutput(vec![long_line]));

        // Should not panic and should not overflow rows.
        terminal.draw(|f| draw(f, &state)).unwrap();
        let text = buffer_text(&terminal);
        for line in text.lines() {
            assert!(line.chars().count() <= 30);
        }
    }

    #[test]
    fn scroll_marker_shown_only_when_scrolled() {
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut state = AppState::new();
        for i in 0..50 {
            state.history.push(HistoryItem::Info(format!("line {i}")));
        }
        state.scroll = 5;

        terminal.draw(|f| draw(f, &state)).unwrap();
        let text = buffer_text(&terminal);
        assert!(
            text.contains("more below"),
            "expected 'more below' marker when scroll > 0:\n{text}"
        );

        state.scroll = 0;
        terminal.draw(|f| draw(f, &state)).unwrap();
        let text = buffer_text(&terminal);
        assert!(
            !text.contains("more below"),
            "did not expect 'more below' marker when scroll == 0:\n{text}"
        );
    }

    #[test]
    fn status_bar_shows_thinking_and_elapsed_seconds() {
        let backend = TestBackend::new(100, 10);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut state = AppState::new();
        state.step_in_flight = true;
        state.step_started = Some(std::time::Instant::now() - std::time::Duration::from_secs(12));

        terminal.draw(|f| draw(f, &state)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("thinking"), "expected 'thinking' in:\n{text}");
        assert!(text.contains("12s"), "expected elapsed seconds in:\n{text}");

        state.step_in_flight = false;
        state.step_started = None;
        terminal.draw(|f| draw(f, &state)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("idle"), "expected 'idle' in:\n{text}");
    }

    #[test]
    fn input_tail_shows_end_and_reports_cursor_column() {
        let long: String = "x".repeat(50);
        let (tail, col) = input_tail(&long, 10);
        assert_eq!(tail.chars().count(), 10);
        assert_eq!(col, 10);
        assert!(long.ends_with(tail));

        let short = "hi";
        let (tail, col) = input_tail(short, 10);
        assert_eq!(tail, "hi");
        assert_eq!(col, 2);
    }

    #[test]
    fn titles_contain_skill_name_and_step() {
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut state = AppState::new();
        state.skill = "coding".to_string();
        state.step = 7;

        terminal.draw(|f| draw(f, &state)).unwrap();
        let text = buffer_text(&terminal);
        assert!(
            text.contains("coding"),
            "expected skill name in title:\n{text}"
        );
        assert!(text.contains("step 7"), "expected step in title:\n{text}");
    }

    #[test]
    fn reasoning_streaming_shows_last_six_lines_and_header() {
        let mut text = String::new();
        for i in 0..10 {
            text.push_str(&format!("line {i}\n"));
        }
        let text = text.trim_end().to_string();
        let lines = history_item_lines(&HistoryItem::ReasoningStreaming(text.clone()));
        // header + 6 lines
        assert_eq!(lines.len(), 7);
        let rendered: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(rendered[0].contains("thinking"));
        assert!(rendered[0].contains(&text.chars().count().to_string()));
        assert!(rendered[1].contains("line 4"));
        assert!(rendered[6].contains("line 9"));
    }

    #[test]
    fn user_error_and_parked_have_expected_prefixes() {
        let user = history_item_lines(&HistoryItem::User("hi".to_string()));
        let joined: String = user[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(joined.contains("you \u{203a}"));

        let err = history_item_lines(&HistoryItem::Error("boom".to_string()));
        let joined: String = err[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(joined.starts_with('\u{2716}'));
        assert!(
            err[0]
                .spans
                .iter()
                .all(|s| s.style.add_modifier.contains(Modifier::BOLD))
        );

        let parked = history_item_lines(&HistoryItem::Parked("go on".to_string()));
        let joined: String = parked[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(joined.contains("waiting for you"));
    }

    #[test]
    fn completion_popup_renders_candidates_with_selected_visible() {
        use crate::app::Completion;

        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut state = AppState::new();
        state.input = "/s".to_string();
        state.completion = Some(Completion {
            candidates: vec![
                "/skill".to_string(),
                "/skill!".to_string(),
                "/state".to_string(),
            ],
            selected: 1,
        });

        terminal.draw(|f| draw(f, &state)).unwrap();
        let text = buffer_text(&terminal);

        // Bordered popup: rounded corners appear somewhere above the input.
        assert!(
            text.contains('\u{256d}') || text.contains('\u{2570}'),
            "expected a rounded popup border in:\n{text}"
        );
        assert!(text.contains("/skill"), "expected /skill in:\n{text}");
        assert!(text.contains("/skill!"), "expected /skill! in:\n{text}");
        assert!(text.contains("/state"), "expected /state in:\n{text}");
    }

    #[test]
    fn no_popup_rows_when_completion_is_none() {
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut state = AppState::new();
        state.input = "/s".to_string();
        state.completion = None;

        terminal.draw(|f| draw(f, &state)).unwrap();
        let text = buffer_text(&terminal);
        assert!(
            !text.contains("/skill!"),
            "did not expect popup candidate text with completion=None:\n{text}"
        );
    }

    #[test]
    fn popup_not_drawn_when_terminal_too_short() {
        use crate::app::Completion;

        let backend = TestBackend::new(60, 7); // below MIN_HEIGHT_FOR_POPUP
        let mut terminal = Terminal::new(backend).unwrap();

        let mut state = AppState::new();
        state.input = "/s".to_string();
        state.completion = Some(Completion {
            candidates: vec!["/skill".to_string(), "/state".to_string()],
            selected: 0,
        });

        // Should not panic, and should not render popup candidate text.
        terminal.draw(|f| draw(f, &state)).unwrap();
        let text = buffer_text(&terminal);
        assert!(
            !text.contains("/skill"),
            "did not expect popup text on a too-short terminal:\n{text}"
        );
    }
}
