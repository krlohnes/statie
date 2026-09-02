//! `run_tui`: the ratatui + crossterm frontend. Terminal setup/teardown,
//! the event loop, and `AppState` (the pure data the `ui` module renders).

use std::sync::Arc;

use futures_util::StreamExt;
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::mpsc;

use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use statie_core::Session;

use crate::driver::{Command, DriverCmd, Query, UiEvent, parse_command, run_driver};
use crate::error::TuiError;
use crate::registry::{RunOptions, SkillRegistry};
use crate::ui::draw;

/// One line/block in the scrolling history pane.
#[derive(Debug, Clone)]
pub enum HistoryItem {
    User(String),
    /// Reasoning text still streaming in for the in-flight step.
    ReasoningStreaming(String),
    /// A completed step's reasoning, collapsed to one dimmed summary line.
    ReasoningCollapsed(usize),
    Patch(String),
    Action(String),
    ToolOutput(Vec<String>),
    System(String),
    Parked(String),
    Info(String),
    Error(String),
}

/// The TUI's own state: history, input, status bar fields, and scroll
/// position. Pure data; `ui::draw` renders it.
pub struct AppState {
    pub history: Vec<HistoryItem>,
    pub input: String,
    pub skill: String,
    pub step: u64,
    pub version: u64,
    pub prompt_tokens: Option<usize>,
    pub step_in_flight: bool,
    pub scroll: usize,
    /// Index into `history` of the currently-streaming reasoning item, if
    /// any (so deltas append instead of pushing a new item each time).
    reasoning_idx: Option<usize>,
}

impl AppState {
    pub fn new() -> Self {
        AppState {
            history: Vec::new(),
            input: String::new(),
            skill: String::new(),
            step: 0,
            version: 0,
            prompt_tokens: None,
            step_in_flight: false,
            scroll: 0,
        reasoning_idx: None,
        }
    }

    fn push(&mut self, item: HistoryItem) {
        self.history.push(item);
        self.scroll = 0; // stick to the bottom on new content
    }

    fn on_event(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::ReasoningDelta(s) => {
                self.step_in_flight = true;
                match self.reasoning_idx {
                    Some(idx) => {
                        if let HistoryItem::ReasoningStreaming(text) = &mut self.history[idx] {
                            text.push_str(&s);
                        }
                    }
                    None => {
                        self.reasoning_idx = Some(self.history.len());
                        self.push(HistoryItem::ReasoningStreaming(s));
                    }
                }
            }
            UiEvent::ContentDelta(_) => {
                self.step_in_flight = true;
            }
            UiEvent::StepDone {
                entries,
                outcome: _,
                prompt_tokens,
                usage: _,
                state,
            } => {
                self.step_in_flight = false;
                if let Some(idx) = self.reasoning_idx.take()
                    && let HistoryItem::ReasoningStreaming(text) = &self.history[idx]
                {
                    let len = text.chars().count();
                    self.history[idx] = HistoryItem::ReasoningCollapsed(len);
                }
                if let Some(t) = prompt_tokens {
                    self.prompt_tokens = Some(t);
                }
                if let Some(step) = state.pointer("/harness/step").and_then(Value::as_u64) {
                    self.step = step;
                }
                if let Some(activation) = state
                    .pointer("/harness/activation")
                    .and_then(Value::as_str)
                {
                    self.skill = activation.to_string();
                }
                for e in &entries {
                    self.push(history_item_for_entry(e));
                }
            }
            UiEvent::Parked(msg) => {
                self.step_in_flight = false;
                self.push(HistoryItem::Parked(msg));
            }
            UiEvent::Activated(name) => {
                self.skill = name.clone();
                self.push(HistoryItem::System(format!("activated {name}")));
            }
            UiEvent::Done => {
                self.step_in_flight = false;
                self.push(HistoryItem::System("done".to_string()));
            }
            UiEvent::Info(text) => {
                self.push(HistoryItem::Info(text));
            }
            UiEvent::Error(text) => {
                self.push(HistoryItem::Error(text));
            }
            UiEvent::Quit => {}
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

fn history_item_for_entry(e: &statie_core::LogEntry) -> HistoryItem {
    use statie_core::LogKind;
    match e.kind {
        LogKind::Patch => {
            let version = e.payload.get("version").and_then(Value::as_u64).unwrap_or(0);
            let patch = e.payload.get("patch").cloned().unwrap_or(Value::Null);
            let compact = serde_json::to_string(&patch).unwrap_or_default();
            let truncated = if compact.chars().count() > 100 {
                let head: String = compact.chars().take(97).collect();
                format!("{head}...")
            } else {
                compact
            };
            HistoryItem::Patch(format!("patch v{version}: {truncated}"))
        }
        LogKind::Action => {
            let text = match e.payload.get("type").and_then(Value::as_str) {
                Some("tool") => {
                    let name = e.payload.get("name").and_then(Value::as_str).unwrap_or("?");
                    let args = e.payload.get("args").cloned().unwrap_or(Value::Null);
                    format!("tool {name} {}", serde_json::to_string(&args).unwrap_or_default())
                }
                Some(other) => other.to_string(),
                None => serde_json::to_string(&e.payload).unwrap_or_default(),
            };
            HistoryItem::Action(text)
        }
        LogKind::ToolOutput => {
            let output = e
                .payload
                .get("output")
                .and_then(Value::as_str)
                .unwrap_or("");
            HistoryItem::ToolOutput(output.lines().map(|l| l.to_string()).collect())
        }
        LogKind::System => HistoryItem::System(
            serde_json::to_string(&e.payload).unwrap_or_default(),
        ),
        LogKind::User => {
            let text = e.payload.get("text").and_then(Value::as_str).unwrap_or("");
            HistoryItem::User(text.to_string())
        }
        LogKind::Reasoning => {
            let text = e.payload.get("text").and_then(Value::as_str).unwrap_or("");
            HistoryItem::ReasoningCollapsed(text.chars().count())
        }
    }
}

/// Runs the ratatui + crossterm TUI against `session`, using `registry` to
/// resolve skills by name.
pub async fn run_tui<W>(
    session: Session<W>,
    registry: Arc<dyn SkillRegistry<W>>,
    opts: RunOptions,
) -> Result<(), TuiError>
where
    W: Clone + Serialize + DeserializeOwned + JsonSchema + Send + Sync + 'static,
{
    let cancel = session.cancel_handle();
    let (cmd_tx, cmd_rx) = mpsc::channel::<DriverCmd>(32);
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<UiEvent>();

    let driver_cancel = cancel.clone();
    let driver_handle = tokio::spawn(async move {
        run_driver(session, registry, opts, cmd_rx, ev_tx, driver_cancel).await
    });

    let mut terminal = ratatui::try_init()?;
    let result = run_event_loop(&mut terminal, cmd_tx, &mut ev_rx, cancel).await;
    ratatui::try_restore()?;

    // Propagate a driver-side error if the UI loop ended cleanly but the
    // driver task failed.
    if driver_handle.is_finished()
        && let Ok(Err(e)) = driver_handle.await
    {
        return Err(e);
    }

    result
}

async fn run_event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    cmd_tx: mpsc::Sender<DriverCmd>,
    ev_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
    cancel: statie_core::CancelHandle,
) -> Result<(), TuiError> {
    let mut state = AppState::new();
    let mut term_events = EventStream::new();
    let mut cancel_armed = false;

    terminal.draw(|f| draw(f, &state))?;

    loop {
        tokio::select! {
            biased;

            ui_ev = ev_rx.recv() => {
                match ui_ev {
                    Some(UiEvent::Quit) => break,
                    Some(ev) => {
                        cancel_armed = false;
                        state.on_event(ev);
                    }
                    None => break,
                }
                terminal.draw(|f| draw(&mut *f, &state))?;
            }

            term_ev = term_events.next() => {
                let Some(term_ev) = term_ev else { break };
                let Ok(term_ev) = term_ev else { continue };
                if let CtEvent::Key(key) = term_ev {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            if state.step_in_flight {
                                cancel.cancel();
                                cancel_armed = true;
                            } else if cancel_armed {
                                break;
                            } else {
                                let _ = cmd_tx.send(DriverCmd::Quit).await;
                                break;
                            }
                        }
                        KeyCode::Enter => {
                            let line = std::mem::take(&mut state.input);
                            if !line.trim().is_empty() {
                                handle_input_line(&line, &cmd_tx, &mut state).await;
                            }
                        }
                        KeyCode::Backspace => {
                            state.input.pop();
                        }
                        KeyCode::Esc => {
                            state.input.clear();
                        }
                        KeyCode::Up => {
                            state.scroll = state.scroll.saturating_add(1);
                        }
                        KeyCode::Down => {
                            state.scroll = state.scroll.saturating_sub(1);
                        }
                        KeyCode::PageUp => {
                            state.scroll = state.scroll.saturating_add(10);
                        }
                        KeyCode::PageDown => {
                            state.scroll = state.scroll.saturating_sub(10);
                        }
                        KeyCode::Char(c) => {
                            state.input.push(c);
                        }
                        _ => {}
                    }
                    terminal.draw(|f| draw(&mut *f, &state))?;
                }
            }
        }
    }

    Ok(())
}

async fn handle_input_line(line: &str, cmd_tx: &mpsc::Sender<DriverCmd>, state: &mut AppState) {
    match parse_command(line) {
        None => {
            state.history.push(HistoryItem::User(line.to_string()));
            let _ = cmd_tx.send(DriverCmd::User(line.to_string())).await;
        }
        Some(Command::State) => {
            let _ = cmd_tx.send(DriverCmd::Query(Query::State)).await;
        }
        Some(Command::Reasoning(step)) => {
            let _ = cmd_tx.send(DriverCmd::Query(Query::Reasoning(step))).await;
        }
        Some(Command::Patches(n)) => {
            let _ = cmd_tx.send(DriverCmd::Query(Query::Patches(n))).await;
        }
        Some(Command::Log(kind, n)) => {
            let _ = cmd_tx.send(DriverCmd::Query(Query::Log(kind, n))).await;
        }
        Some(Command::Tools) => {
            let _ = cmd_tx.send(DriverCmd::Query(Query::Tools)).await;
        }
        Some(Command::Skill(name, forced)) => {
            let _ = cmd_tx.send(DriverCmd::Switch { name, forced }).await;
        }
        Some(Command::Edit(v)) => {
            let _ = cmd_tx.send(DriverCmd::Edit(v)).await;
        }
        Some(Command::Help) => {
            state.history.push(HistoryItem::Info(help_text()));
        }
        Some(Command::Quit) => {
            let _ = cmd_tx.send(DriverCmd::Quit).await;
        }
        Some(Command::Unknown(cmd)) => {
            state
                .history
                .push(HistoryItem::Error(format!("unknown command: {cmd}")));
        }
    }
}

fn help_text() -> String {
    let commands = [
        "/state", "/reasoning [step]", "/patches [n]", "/log [kind]", "/tools",
        "/skill <name>", "/skill! <name>", "/edit <json merge patch>", "/help", "/quit",
    ];
    commands.join("\n")
}
