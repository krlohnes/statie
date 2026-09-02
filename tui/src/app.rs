//! `run_tui`: the ratatui + crossterm frontend. Terminal setup/teardown,
//! the event loop, and `AppState` (the pure data the `ui` module renders).

use std::sync::Arc;

use futures_util::StreamExt;
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::mpsc;

use crossterm::event::{
    Event as CtEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use statie_core::Session;
use tokio::time::{self, Duration};

use crate::driver::{COMMANDS, Command, DriverCmd, Query, UiEvent, parse_command, run_driver};
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

/// Autocomplete popup state: the current candidate list and which one is
/// selected.
#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    pub candidates: Vec<String>,
    pub selected: usize,
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
    /// Prompt tokens as reported by the server (`UiEvent::StepDone.usage`),
    /// distinct from `prompt_tokens` which is the harness's own estimate.
    pub server_prompt_tokens: Option<u64>,
    pub step_in_flight: bool,
    pub scroll: usize,
    /// When the in-flight step started (first delta, or the moment the user
    /// submitted a message), used to show elapsed "thinking" time. Cleared
    /// when the step ends.
    pub step_started: Option<std::time::Instant>,
    /// Advances on a timer to animate the status-bar spinner.
    pub spinner_tick: usize,
    /// Index into `history` of the currently-streaming reasoning item, if
    /// any (so deltas append instead of pushing a new item each time).
    reasoning_idx: Option<usize>,
    /// Names of skills available via `/skill`, sorted, for autocomplete.
    pub skill_names: Vec<String>,
    /// The current slash-command autocomplete popup, if `input` has any
    /// candidates. Recomputed after every input edit.
    pub completion: Option<Completion>,
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
            server_prompt_tokens: None,
            step_in_flight: false,
            scroll: 0,
            step_started: None,
            spinner_tick: 0,
            reasoning_idx: None,
            skill_names: Vec::new(),
            completion: None,
        }
    }

    /// Recomputes `completion` from the current `input` and `skill_names`.
    /// Called after every input edit.
    fn update_completion(&mut self) {
        let candidates = completions(&self.input, &self.skill_names);
        self.completion = if candidates.is_empty() {
            None
        } else {
            let selected = match &self.completion {
                Some(c) => c.selected.min(candidates.len().saturating_sub(1)),
                None => 0,
            };
            Some(Completion {
                candidates,
                selected,
            })
        };
    }

    pub(crate) fn push(&mut self, item: HistoryItem) {
        self.history.push(item);
        self.scroll = 0; // stick to the bottom on new content
    }

    fn on_event(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::ReasoningDelta(s) => {
                self.step_in_flight = true;
                if self.step_started.is_none() {
                    self.step_started = Some(std::time::Instant::now());
                }
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
            UiEvent::Replay {
                entries,
                state,
                step,
                skill,
            } => {
                let n = entries.len();
                self.push(HistoryItem::System(format!(
                    "── resumed session at step {step} in `{skill}`; last {n} log entries ──"
                )));
                for e in &entries {
                    self.push(history_item_for_entry(e));
                }
                self.push(HistoryItem::Info(
                    "send a message to continue (e.g. \"continue\")".to_string(),
                ));
                self.skill = skill;
                self.step = step;
                if let Some(v) = state.pointer("/version").and_then(Value::as_u64) {
                    self.version = v;
                }
                self.scroll = 0;
            }
            UiEvent::ContentDelta(_) => {
                self.step_in_flight = true;
                if self.step_started.is_none() {
                    self.step_started = Some(std::time::Instant::now());
                }
            }
            UiEvent::StepDone {
                entries,
                outcome: _,
                prompt_tokens,
                usage,
                state,
            } => {
                self.step_in_flight = false;
                self.step_started = None;
                if let Some(u) = usage {
                    self.server_prompt_tokens = Some(u.prompt_tokens);
                }
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
                if let Some(activation) =
                    state.pointer("/harness/activation").and_then(Value::as_str)
                {
                    self.skill = activation.to_string();
                }
                for e in &entries {
                    self.push(history_item_for_entry(e));
                }
            }
            UiEvent::Parked(msg) => {
                self.step_in_flight = false;
                self.step_started = None;
                self.push(HistoryItem::Parked(msg));
            }
            UiEvent::Activated(name) => {
                self.skill = name.clone();
                self.push(HistoryItem::System(format!("activated {name}")));
            }
            UiEvent::Done => {
                self.step_in_flight = false;
                self.step_started = None;
                self.push(HistoryItem::System("done".to_string()));
            }
            UiEvent::Info(text) => {
                self.push(HistoryItem::Info(text));
            }
            UiEvent::Error(text) => {
                self.step_in_flight = false;
                self.step_started = None;
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

/// Log entry kinds accepted by `/log`, kept in sync with
/// `driver::parse_log_kind` by hand (that function is private to `driver`).
const LOG_KINDS: &[&str] = &[
    "user",
    "reasoning",
    "patch",
    "action",
    "tool_output",
    "system",
];

/// Pure autocomplete: given the current input text and the known skill
/// names, returns the list of candidate completions (empty when none
/// apply).
///
/// - `/` followed by no space: command names starting with the typed
///   prefix, in `COMMANDS` order (the prefix includes the leading `/`, so
///   `/s` matches `/skill`, `/skill!`, `/state`).
/// - `/skill ` or `/skill! ` followed by a partial name: matching skill
///   names.
/// - `/log ` followed by a partial kind (and nothing after it): matching
///   log kinds.
/// - anything else: empty.
pub fn completions(input: &str, skill_names: &[String]) -> Vec<String> {
    if !input.starts_with('/') {
        return Vec::new();
    }

    if !input.contains(char::is_whitespace) {
        return COMMANDS
            .iter()
            .map(|c| format!("/{}", c.name))
            .filter(|name| name.starts_with(input))
            .collect();
    }

    let mut parts = input.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("");

    if cmd == "/skill" || cmd == "/skill!" {
        // Only offer completions while typing the single name argument (no
        // further whitespace yet).
        if rest.contains(char::is_whitespace) {
            return Vec::new();
        }
        return skill_names
            .iter()
            .filter(|name| name.starts_with(rest))
            .cloned()
            .collect();
    }

    if cmd == "/log" {
        if rest.contains(char::is_whitespace) {
            return Vec::new();
        }
        return LOG_KINDS
            .iter()
            .filter(|kind| kind.starts_with(rest))
            .map(|s| s.to_string())
            .collect();
    }

    Vec::new()
}

/// The action a keypress should cause, as decided by `handle_key`. The event
/// loop acts on this; `handle_key` itself only mutates `AppState`'s input
/// and completion/scroll fields, keeping it unit-testable without a driver
/// channel.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyAction {
    None,
    Submit(String),
    Quit,
    /// Ctrl-C pressed with no step in flight and not already armed: send
    /// `DriverCmd::Quit` (a graceful quit distinct from breaking the loop
    /// directly on a second Ctrl-C).
    RequestQuit,
    /// Ctrl-C pressed while a step is in flight: cancel it.
    Cancel,
}

/// Handles one key press against `state`, applying input/completion/scroll
/// changes directly and returning what the event loop should do next
/// (submit a line, quit, cancel the in-flight step, etc). `cancel_armed` and
/// `step_in_flight` disambiguate Ctrl-C's three behaviors exactly as the
/// event loop did inline before this was factored out.
pub fn handle_key(state: &mut AppState, key: KeyEvent, cancel_armed: bool) -> KeyAction {
    let completing = state.completion.is_some();

    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if state.step_in_flight {
                KeyAction::Cancel
            } else if cancel_armed {
                KeyAction::Quit
            } else {
                KeyAction::RequestQuit
            }
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.input.clear();
            state.update_completion();
            KeyAction::None
        }
        KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.history.clear();
            state.scroll = 0;
            KeyAction::None
        }
        KeyCode::Tab if completing => {
            apply_completion(state);
            KeyAction::None
        }
        KeyCode::Up if completing => {
            let comp = state.completion.as_mut().unwrap();
            let len = comp.candidates.len();
            comp.selected = if comp.selected == 0 {
                len - 1
            } else {
                comp.selected - 1
            };
            KeyAction::None
        }
        KeyCode::Down if completing => {
            let comp = state.completion.as_mut().unwrap();
            let len = comp.candidates.len();
            comp.selected = (comp.selected + 1) % len;
            KeyAction::None
        }
        KeyCode::Esc if completing => {
            state.completion = None;
            KeyAction::None
        }
        KeyCode::Enter => {
            // A single candidate that's a strict extension of what's typed:
            // apply it first, then submit as usual.
            if let Some(comp) = &state.completion
                && comp.candidates.len() == 1
                && comp.candidates[0] != state.input
                && comp.candidates[0].starts_with(&state.input)
            {
                apply_completion(state);
            }
            let line = std::mem::take(&mut state.input);
            state.completion = None;
            if line.trim().is_empty() {
                KeyAction::None
            } else {
                KeyAction::Submit(line)
            }
        }
        KeyCode::Backspace => {
            state.input.pop();
            state.update_completion();
            KeyAction::None
        }
        KeyCode::Esc => {
            state.input.clear();
            KeyAction::None
        }
        KeyCode::Up => {
            state.scroll = state.scroll.saturating_add(1);
            KeyAction::None
        }
        KeyCode::Down => {
            state.scroll = state.scroll.saturating_sub(1);
            KeyAction::None
        }
        KeyCode::PageUp => {
            state.scroll = state.scroll.saturating_add(10);
            KeyAction::None
        }
        KeyCode::PageDown => {
            state.scroll = state.scroll.saturating_sub(10);
            KeyAction::None
        }
        KeyCode::Home => {
            // Clamped to the true max in `ui::draw`.
            state.scroll = usize::MAX;
            KeyAction::None
        }
        KeyCode::End => {
            state.scroll = 0;
            KeyAction::None
        }
        KeyCode::Char(c) => {
            state.input.push(c);
            state.update_completion();
            KeyAction::None
        }
        _ => KeyAction::None,
    }
}

/// Applies the selected completion candidate to `state.input`, replacing
/// the token currently being completed. Appends a trailing space when the
/// completed command takes arguments (so the user can start typing them
/// immediately); a bare command with no arguments gets nothing appended.
fn apply_completion(state: &mut AppState) {
    let Some(comp) = state.completion.take() else {
        return;
    };
    let candidate = comp.candidates[comp.selected].clone();

    if !state.input.contains(char::is_whitespace) {
        // Completing the command name itself.
        let takes_args = COMMANDS
            .iter()
            .find(|c| format!("/{}", c.name) == candidate)
            .map(|c| !c.args.is_empty())
            .unwrap_or(false);
        state.input = if takes_args {
            format!("{candidate} ")
        } else {
            candidate
        };
    } else {
        // Completing an argument token: replace everything after the last
        // whitespace-delimited boundary (the command plus a single space).
        let mut parts = state.input.splitn(2, char::is_whitespace);
        let cmd = parts.next().unwrap_or("").to_string();
        state.input = format!("{cmd} {candidate}");
    }
    state.update_completion();
}

fn history_item_for_entry(e: &statie_core::LogEntry) -> HistoryItem {
    use statie_core::LogKind;
    match e.kind {
        LogKind::Patch => {
            let version = e
                .payload
                .get("version")
                .and_then(Value::as_u64)
                .unwrap_or(0);
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
                    format!(
                        "tool {name} {}",
                        serde_json::to_string(&args).unwrap_or_default()
                    )
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
        LogKind::System => {
            HistoryItem::System(serde_json::to_string(&e.payload).unwrap_or_default())
        }
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

    let mut skill_names = registry.names();
    skill_names.sort();

    let driver_cancel = cancel.clone();
    let driver_handle = tokio::spawn(async move {
        run_driver(session, registry, opts, cmd_rx, ev_tx, driver_cancel).await
    });

    let mut terminal = ratatui::try_init()?;
    let result = run_event_loop(&mut terminal, cmd_tx, &mut ev_rx, cancel, skill_names).await;
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
    skill_names: Vec<String>,
) -> Result<(), TuiError> {
    let mut state = AppState::new();
    state.skill_names = skill_names;
    let mut term_events = EventStream::new();
    let mut cancel_armed = false;
    let mut spinner_interval = time::interval(Duration::from_millis(250));
    spinner_interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

    push_startup_info(&mut state);
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
                    match handle_key(&mut state, key, cancel_armed) {
                        KeyAction::None => {}
                        KeyAction::Cancel => {
                            cancel.cancel();
                            cancel_armed = true;
                        }
                        KeyAction::Quit => break,
                        KeyAction::RequestQuit => {
                            let _ = cmd_tx.send(DriverCmd::Quit).await;
                            break;
                        }
                        KeyAction::Submit(line) => {
                            handle_input_line(&line, &cmd_tx, &mut state).await;
                        }
                    }
                } else {
                    // Resize and other non-key events (mouse, focus, paste)
                    // still warrant a redraw so the frame never goes stale.
                }
                terminal.draw(|f| draw(&mut *f, &state))?;
            }

            _ = spinner_interval.tick() => {
                state.spinner_tick = state.spinner_tick.wrapping_add(1);
                terminal.draw(|f| draw(&mut *f, &state))?;
            }
        }
    }

    Ok(())
}

fn push_startup_info(state: &mut AppState) {
    state.history.push(HistoryItem::Info(
        "type a message and press Enter \u{b7} type / for commands \u{b7} \u{2191}/\u{2193} PgUp/PgDn scroll \u{b7} ctrl-c quit"
            .to_string(),
    ));
}

async fn handle_input_line(line: &str, cmd_tx: &mpsc::Sender<DriverCmd>, state: &mut AppState) {
    match parse_command(line) {
        None => {
            state.push(HistoryItem::User(line.to_string()));
            state.step_in_flight = true;
            state.step_started = Some(std::time::Instant::now());
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
            state.push(HistoryItem::Info(help_text()));
        }
        Some(Command::Quit) => {
            let _ = cmd_tx.send(DriverCmd::Quit).await;
        }
        Some(Command::Unknown(cmd)) => {
            state.push(HistoryItem::Error(format!("unknown command: {cmd}")));
        }
    }
}

fn help_text() -> String {
    let usages: Vec<String> = COMMANDS
        .iter()
        .map(|c| {
            if c.args.is_empty() {
                format!("/{}", c.name)
            } else {
                format!("/{} {}", c.name, c.args)
            }
        })
        .collect();
    let width = usages.iter().map(|u| u.len()).max().unwrap_or(0);
    COMMANDS
        .iter()
        .zip(usages)
        .map(|(c, usage)| format!("{usage:<width$}  \u{2014}  {}", c.help))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn skills() -> Vec<String> {
        vec!["implement".to_string(), "plan".to_string()]
    }

    #[test]
    fn slash_alone_lists_all_commands() {
        let cands = completions("/", &skills());
        assert_eq!(cands.len(), COMMANDS.len());
        assert_eq!(cands[0], "/state");
    }

    #[test]
    fn partial_command_prefix_lists_matches_in_command_order() {
        let cands = completions("/s", &skills());
        // Matches in `COMMANDS` order (state, then skill, then skill!).
        assert_eq!(cands, vec!["/state", "/skill", "/skill!"]);
    }

    #[test]
    fn skill_partial_name_matches_registry_names() {
        let cands = completions("/skill im", &skills());
        assert_eq!(cands, vec!["implement"]);

        let cands = completions("/skill! im", &skills());
        assert_eq!(cands, vec!["implement"]);
    }

    #[test]
    fn log_partial_kind_matches() {
        let cands = completions("/log to", &skills());
        assert_eq!(cands, vec!["tool_output"]);
    }

    #[test]
    fn plain_text_has_no_completions() {
        assert!(completions("hello there", &skills()).is_empty());
        assert!(completions("", &skills()).is_empty());
    }

    #[test]
    fn completion_stops_once_argument_has_a_space() {
        // Once past the single name argument, no more completions apply.
        assert!(completions("/skill implement extra", &skills()).is_empty());
        assert!(completions("/log tool_output 5", &skills()).is_empty());
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn tab_completes_command_with_trailing_space_when_it_takes_args() {
        let mut state = AppState::new();
        for c in "/sk".chars() {
            state.input.push(c);
            state.update_completion();
        }
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Tab), false),
            KeyAction::None
        );
        assert_eq!(state.input, "/skill ");
    }

    #[test]
    fn up_down_cycle_selection_while_completing() {
        let mut state = AppState::new();
        state.input = "/s".to_string();
        state.update_completion();
        assert_eq!(state.completion.as_ref().unwrap().selected, 0);

        handle_key(&mut state, key(KeyCode::Down), false);
        assert_eq!(state.completion.as_ref().unwrap().selected, 1);

        handle_key(&mut state, key(KeyCode::Down), false);
        assert_eq!(state.completion.as_ref().unwrap().selected, 2);

        // wraps back to 0
        handle_key(&mut state, key(KeyCode::Down), false);
        assert_eq!(state.completion.as_ref().unwrap().selected, 0);

        // wraps to the last candidate going up from 0
        handle_key(&mut state, key(KeyCode::Up), false);
        assert_eq!(state.completion.as_ref().unwrap().selected, 2);
    }

    #[test]
    fn esc_twice_dismisses_popup_then_clears_input() {
        let mut state = AppState::new();
        state.input = "/s".to_string();
        state.update_completion();
        assert!(state.completion.is_some());

        handle_key(&mut state, key(KeyCode::Esc), false);
        assert!(state.completion.is_none());
        assert_eq!(state.input, "/s");

        handle_key(&mut state, key(KeyCode::Esc), false);
        assert_eq!(state.input, "");
    }

    #[test]
    fn enter_applies_single_strict_prefix_candidate_then_submits() {
        let mut state = AppState::new();
        state.input = "/quit".to_string();
        state.update_completion();
        // "/quit" is already an exact match to a single candidate, not a
        // strict prefix, so it submits as-is.
        let action = handle_key(&mut state, key(KeyCode::Enter), false);
        assert_eq!(action, KeyAction::Submit("/quit".to_string()));
    }

    #[test]
    fn ctrl_c_variants() {
        let mut state = AppState::new();
        state.step_in_flight = true;
        assert_eq!(
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                false
            ),
            KeyAction::Cancel
        );

        state.step_in_flight = false;
        assert_eq!(
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                false
            ),
            KeyAction::RequestQuit
        );
        assert_eq!(
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                true
            ),
            KeyAction::Quit
        );
    }
}
