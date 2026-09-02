//! The shared engine: commands in, `UiEvent`s out. No terminal code lives
//! here so both `run_tui` and `run_headless` (and tests) can drive it.

use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::mpsc;

use schemars::JsonSchema;
use statie_core::{
    CancelHandle, Event, LogEntry, LogKind, Outcome, Park, Session, SessionLogExt, StepSink, Usage,
};

use crate::error::TuiError;
use crate::registry::{RunOptions, SkillRegistry};

/// A command sent to the driver from a UI (TUI, headless feeder, or test).
#[derive(Debug, Clone)]
pub enum DriverCmd {
    User(String),
    Cancel,
    Switch { name: String, forced: bool },
    Edit(Value),
    Query(Query),
    Quit,
}

/// A read-only inspection request.
#[derive(Debug, Clone)]
pub enum Query {
    State,
    Reasoning(Option<u64>),
    Patches(usize),
    Log(Option<LogKind>, usize),
    Tools,
}

/// An event emitted by the driver for a UI to render.
#[derive(Debug, Clone)]
pub enum UiEvent {
    ReasoningDelta(String),
    ContentDelta(String),
    StepDone {
        entries: Vec<LogEntry>,
        outcome: String,
        prompt_tokens: Option<usize>,
        usage: Option<Usage>,
        state: Value,
    },
    /// `AskUser` message, `"yield"`, a failure/rejection reason, or
    /// `"cancelled"`.
    Parked(String),
    Activated(String),
    Done,
    Info(String),
    Error(String),
    Quit,
}

/// A `StepSink` that forwards reasoning/content deltas to an unbounded
/// channel. Unbounded because deltas are produced from inside the async
/// model call; a bounded `try_send` could drop under backpressure and a
/// bounded `blocking_send` could deadlock the executor thread running the
/// model call.
pub struct ChannelSink {
    tx: mpsc::UnboundedSender<UiEvent>,
}

impl ChannelSink {
    pub fn new(tx: mpsc::UnboundedSender<UiEvent>) -> Self {
        ChannelSink { tx }
    }
}

impl StepSink for ChannelSink {
    fn reasoning_delta(&mut self, s: &str) {
        let _ = self.tx.send(UiEvent::ReasoningDelta(s.to_string()));
    }

    fn content_delta(&mut self, s: &str) {
        let _ = self.tx.send(UiEvent::ContentDelta(s.to_string()));
    }
}

fn outcome_label(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Continue => "continue".to_string(),
        Outcome::Parked(Park::AskUser(_)) => "parked(ask_user)".to_string(),
        Outcome::Parked(Park::Yield) => "parked(yield)".to_string(),
        Outcome::Parked(Park::Failure(_)) => "parked(failure)".to_string(),
        Outcome::Parked(Park::Cancelled) => "parked(cancelled)".to_string(),
        Outcome::Done => "done".to_string(),
        Outcome::Handoff(name) => format!("handoff({name})"),
    }
}

fn park_message(park: &Park) -> String {
    match park {
        Park::AskUser(msg) => msg.clone(),
        Park::Yield => "yield".to_string(),
        Park::Failure(reason) => reason.clone(),
        Park::Cancelled => "cancelled".to_string(),
    }
}

/// Runs the driver loop against `session`, reading commands from `cmd_rx`
/// and emitting `UiEvent`s to `ev_tx`, until `DriverCmd::Quit` or the
/// command channel closes.
pub async fn run_driver<W>(
    mut session: Session<W>,
    registry: Arc<dyn SkillRegistry<W>>,
    opts: RunOptions,
    mut cmd_rx: mpsc::Receiver<DriverCmd>,
    ev_tx: mpsc::UnboundedSender<UiEvent>,
    cancel: CancelHandle,
) -> Result<(), TuiError>
where
    W: Clone + Serialize + DeserializeOwned + JsonSchema + Send + Sync + 'static,
{
    let _ = &cancel; // held by the caller (TUI task); the driver itself never cancels.

    let mut next_skill = opts.initial_skill.clone();
    let mut forced_next = false;
    // Messages to push as the very first `Event::User` steps after the
    // initial activation only.
    let mut pending_initial_messages = opts.initial_messages.clone();
    // A `Handoff` outcome means "continue autonomously" (ARCHITECTURE.md):
    // the new activation should be ticked immediately rather than waiting
    // for a command. A user-issued `/skill` switch, by contrast, waits.
    let mut autonomous_after_activate = false;

    'activation: loop {
        let Some(skill) = registry.get(&next_skill) else {
            return Err(TuiError::UnknownSkill(next_skill));
        };

        let mut active = if forced_next {
            session.activate_forced(skill).await?
        } else {
            session.activate(skill).await?
        };
        let _ = ev_tx.send(UiEvent::Activated(active.skill_name().to_string()));

        if autonomous_after_activate
            && let StepOutcome::Switch { name, forced } =
                run_one_step(&mut active, Event::Tick, &ev_tx, &opts).await?
        {
            next_skill = name;
            forced_next = forced;
            continue 'activation;
        }

        // Drain any messages queued for this activation as User steps,
        // running autonomously after each until parked/done/handoff/capped.
        let initial_msgs = std::mem::take(&mut pending_initial_messages);
        for msg in initial_msgs {
            match run_one_step(
                &mut active,
                Event::User(msg),
                &ev_tx,
                &opts,
            )
            .await?
            {
                StepOutcome::Switch { name, forced } => {
                    next_skill = name;
                    forced_next = forced;
                    autonomous_after_activate = true;
                    continue 'activation;
                }
                StepOutcome::Continue => {}
            }
        }

        // Main command loop for this activation.
        loop {
            let Some(cmd) = cmd_rx.recv().await else {
                return Ok(());
            };

            match cmd {
                DriverCmd::Quit => {
                    let _ = ev_tx.send(UiEvent::Quit);
                    return Ok(());
                }
                DriverCmd::Cancel => {
                    // No step is in flight here (we're between steps,
                    // waiting on a command); nothing to cancel.
                    let _ = ev_tx.send(UiEvent::Info(
                        "nothing to cancel".to_string(),
                    ));
                }
                DriverCmd::User(text) => {
                    match run_one_step(&mut active, Event::User(text), &ev_tx, &opts).await? {
                        StepOutcome::Switch { name, forced } => {
                            next_skill = name;
                            forced_next = forced;
                            autonomous_after_activate = true;
                            continue 'activation;
                        }
                        StepOutcome::Continue => {}
                    }
                }
                DriverCmd::Switch { name, forced } => {
                    next_skill = name;
                    forced_next = forced;
                    // A user-issued switch is not itself autonomous
                    // continuation; wait for the next command.
                    autonomous_after_activate = false;
                    continue 'activation;
                }
                DriverCmd::Edit(patch) => match active.user_patch(patch).await {
                    Ok(rec) => {
                        let state = current_state_value(active.session()).await;
                        let entries = rec.entries().to_vec();
                        let outcome = active.session().log().append_record(rec)?;
                        let _ = ev_tx.send(UiEvent::StepDone {
                            entries,
                            outcome: outcome_label(&outcome),
                            prompt_tokens: active.last_prompt_tokens(),
                            usage: active.last_usage(),
                            state,
                        });
                    }
                    Err(e) => {
                        let _ = ev_tx.send(UiEvent::Error(e.to_string()));
                    }
                },
                DriverCmd::Query(q) => {
                    let text = handle_query(&mut active, q).await?;
                    let _ = ev_tx.send(UiEvent::Info(text));
                }
            }
        }
    }
}

enum StepOutcome {
    Continue,
    Switch { name: String, forced: bool },
}

/// Runs one `Event` through `active`, then keeps ticking autonomously while
/// the outcome is `Continue`, up to `max_autonomous_steps` consecutive
/// continues. Every `StepRecord` is appended to the log (the only way to
/// obtain its `Outcome`) and reported as a `StepDone` event.
async fn run_one_step<'a, K>(
    active: &mut statie_core::Active<'a, K>,
    first: Event,
    ev_tx: &mpsc::UnboundedSender<UiEvent>,
    opts: &RunOptions,
) -> Result<StepOutcome, TuiError>
where
    K: statie_core::Skill + ?Sized,
    K::Workspace: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    let mut ev = first;
    let mut continues = 0usize;

    loop {
        // `ChannelSink` sends deltas straight to `ev_tx` as they're
        // produced from inside the model call; an unbounded channel means
        // this synchronous callback never blocks the async model future.
        let mut sink = ChannelSink::new(ev_tx.clone());
        let rec = active.step(ev.clone(), &mut sink).await?;

        let state = current_state_value(active.session()).await;
        let prompt_tokens = active.last_prompt_tokens();
        let usage = active.last_usage();
        let log = active.session().log();
        let entries = rec.entries().to_vec();
        let outcome = log.append_record(rec)?;

        let _ = ev_tx.send(UiEvent::StepDone {
            entries,
            outcome: outcome_label(&outcome),
            prompt_tokens,
            usage,
            state,
        });

        match outcome {
            Outcome::Continue => {
                continues += 1;
                if continues >= opts.max_autonomous_steps {
                    let _ = ev_tx.send(UiEvent::Error(
                        "autonomous step cap reached".to_string(),
                    ));
                    let _ = ev_tx.send(UiEvent::Parked("autonomous step cap reached".to_string()));
                    return Ok(StepOutcome::Continue);
                }
                ev = Event::Tick;
            }
            Outcome::Parked(park) => {
                let _ = ev_tx.send(UiEvent::Parked(park_message(&park)));
                return Ok(StepOutcome::Continue);
            }
            Outcome::Done => {
                let _ = ev_tx.send(UiEvent::Done);
                return Ok(StepOutcome::Continue);
            }
            Outcome::Handoff(name) => {
                // The caller must drop `active` (borrowing `session`) before
                // re-activating; return to the outer loop to do so.
                return Ok(StepOutcome::Switch {
                    name,
                    forced: false,
                });
            }
        }
    }
}

async fn current_state_value<W>(session: &mut Session<W>) -> Value
where
    W: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    match session.state().await {
        Ok((state, _version)) => serde_json::to_value(&state).unwrap_or(Value::Null),
        Err(_) => Value::Null,
    }
}

async fn handle_query<'a, K>(
    active: &mut statie_core::Active<'a, K>,
    q: Query,
) -> Result<String, TuiError>
where
    K: statie_core::Skill + ?Sized,
    K::Workspace: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    match q {
        Query::State => {
            let (state, version) = active.session().state().await?;
            let value = serde_json::to_value(&state).unwrap_or(Value::Null);
            Ok(format!(
                "version: {}\n{}",
                version.0,
                crate::driver::render_state(&value)
            ))
        }
        Query::Reasoning(step) => {
            let entries = active.session().log().entries(Some(LogKind::Reasoning), 10_000);
            let filtered: Vec<&LogEntry> = match step {
                Some(s) => entries.iter().filter(|e| e.step == s).collect(),
                None => {
                    let last = entries.last();
                    last.into_iter().collect()
                }
            };
            if filtered.is_empty() {
                Ok("(no reasoning recorded)".to_string())
            } else {
                Ok(filtered
                    .iter()
                    .map(|e| {
                        let text = e
                            .payload
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        format!("[step {}] {}", e.step, text)
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
        }
        Query::Patches(n) => {
            let patches = active.session().patch_log().await?;
            let start = patches.len().saturating_sub(n);
            Ok(render_patches(&patches[start..], n))
        }
        Query::Log(kind, n) => {
            let entries = active.session().log().entries(kind, n);
            Ok(entries
                .iter()
                .map(render_entry_line)
                .collect::<Vec<_>>()
                .join("\n"))
        }
        Query::Tools => {
            let (state, _version) = active.session().state().await?;
            Ok(state.harness.active_tools.join(", "))
        }
    }
}

// ---------------------------------------------------------------------
// Pure rendering helpers (unit-testable).
// ---------------------------------------------------------------------

/// Pretty-prints a `serde_json::Value` (e.g. `session.state()`).
pub fn render_state(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| "null".to_string())
}

/// One-line plain-text rendering of a single `LogEntry`.
pub fn render_entry_line(e: &LogEntry) -> String {
    let kind = match e.kind {
        LogKind::User => "user",
        LogKind::Reasoning => "reasoning",
        LogKind::Patch => "patch",
        LogKind::Action => "action",
        LogKind::ToolOutput => "tool_output",
        LogKind::System => "system",
    };
    let payload = serde_json::to_string(&e.payload).unwrap_or_default();
    format!("[step {}] {}: {}", e.step, kind, payload)
}

/// Multi-line rendering of a single `LogEntry`, split into `Line`-ish plain
/// text strings (one per line of output).
pub fn render_entry(e: &LogEntry) -> Vec<String> {
    vec![render_entry_line(e)]
}

/// Renders the last `n` logged patches as compact, truncated one-liners.
pub fn render_patches(patches: &[statie_core::LoggedPatch], n: usize) -> String {
    let start = patches.len().saturating_sub(n);
    patches[start..]
        .iter()
        .map(|p| {
            let compact = serde_json::to_string(&p.patch).unwrap_or_default();
            let truncated = if compact.chars().count() > 120 {
                let head: String = compact.chars().take(117).collect();
                format!("{head}...")
            } else {
                compact
            };
            format!("patch v{}: {}", p.version.0, truncated)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A parsed slash command. `parse_command` returns `None` for plain text
/// (which becomes `DriverCmd::User`).
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    State,
    Reasoning(Option<u64>),
    Patches(usize),
    Log(Option<LogKind>, usize),
    Tools,
    Skill(String, bool),
    Edit(Value),
    Help,
    Quit,
    Unknown(String),
}

fn parse_log_kind(s: &str) -> Option<LogKind> {
    match s {
        "user" => Some(LogKind::User),
        "reasoning" => Some(LogKind::Reasoning),
        "patch" => Some(LogKind::Patch),
        "action" => Some(LogKind::Action),
        "tool_output" => Some(LogKind::ToolOutput),
        "system" => Some(LogKind::System),
        _ => None,
    }
}

/// Parses a slash command. Returns `None` for anything not starting with
/// `/` (plain user text).
pub fn parse_command(input: &str) -> Option<Command> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim();

    Some(match cmd {
        "/state" => Command::State,
        "/reasoning" => {
            if rest.is_empty() {
                Command::Reasoning(None)
            } else {
                match rest.parse::<u64>() {
                    Ok(n) => Command::Reasoning(Some(n)),
                    Err(_) => Command::Unknown(trimmed.to_string()),
                }
            }
        }
        "/patches" => {
            if rest.is_empty() {
                Command::Patches(5)
            } else {
                match rest.parse::<usize>() {
                    Ok(n) => Command::Patches(n),
                    Err(_) => Command::Unknown(trimmed.to_string()),
                }
            }
        }
        "/log" => {
            if rest.is_empty() {
                Command::Log(None, 20)
            } else {
                let mut it = rest.splitn(2, char::is_whitespace);
                let kind_str = it.next().unwrap_or("");
                let n_str = it.next().unwrap_or("").trim();
                match parse_log_kind(kind_str) {
                    Some(kind) => {
                        if n_str.is_empty() {
                            Command::Log(Some(kind), 20)
                        } else {
                            match n_str.parse::<usize>() {
                                Ok(n) => Command::Log(Some(kind), n),
                                Err(_) => Command::Unknown(trimmed.to_string()),
                            }
                        }
                    }
                    None => Command::Unknown(trimmed.to_string()),
                }
            }
        }
        "/tools" => Command::Tools,
        "/skill" => {
            if rest.is_empty() {
                Command::Unknown(trimmed.to_string())
            } else {
                Command::Skill(rest.to_string(), false)
            }
        }
        "/skill!" => {
            if rest.is_empty() {
                Command::Unknown(trimmed.to_string())
            } else {
                Command::Skill(rest.to_string(), true)
            }
        }
        "/edit" => match serde_json::from_str::<Value>(rest) {
            Ok(v) => Command::Edit(v),
            Err(_) => Command::Unknown(trimmed.to_string()),
        },
        "/help" => Command::Help,
        "/quit" => Command::Quit,
        _ => Command::Unknown(trimmed.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_not_a_command() {
        assert_eq!(parse_command("hello there"), None);
    }

    #[test]
    fn state_command() {
        assert_eq!(parse_command("/state"), Some(Command::State));
    }

    #[test]
    fn reasoning_default_and_with_step() {
        assert_eq!(parse_command("/reasoning"), Some(Command::Reasoning(None)));
        assert_eq!(
            parse_command("/reasoning 7"),
            Some(Command::Reasoning(Some(7)))
        );
    }

    #[test]
    fn patches_default_is_5() {
        assert_eq!(parse_command("/patches"), Some(Command::Patches(5)));
        assert_eq!(parse_command("/patches 12"), Some(Command::Patches(12)));
    }

    #[test]
    fn log_default_is_all_kinds_last_20() {
        assert_eq!(parse_command("/log"), Some(Command::Log(None, 20)));
    }

    #[test]
    fn log_with_kind() {
        assert_eq!(
            parse_command("/log tool_output"),
            Some(Command::Log(Some(LogKind::ToolOutput), 20))
        );
        assert_eq!(
            parse_command("/log tool_output 5"),
            Some(Command::Log(Some(LogKind::ToolOutput), 5))
        );
    }

    #[test]
    fn tools_command() {
        assert_eq!(parse_command("/tools"), Some(Command::Tools));
    }

    #[test]
    fn skill_switch_and_forced() {
        assert_eq!(
            parse_command("/skill coding"),
            Some(Command::Skill("coding".to_string(), false))
        );
        assert_eq!(
            parse_command("/skill! coding"),
            Some(Command::Skill("coding".to_string(), true))
        );
    }

    #[test]
    fn edit_parses_json_merge_patch() {
        assert_eq!(
            parse_command(r#"/edit {"a":1}"#),
            Some(Command::Edit(serde_json::json!({"a": 1})))
        );
    }

    #[test]
    fn edit_invalid_json_is_unknown() {
        assert!(matches!(
            parse_command("/edit not json"),
            Some(Command::Unknown(_))
        ));
    }

    #[test]
    fn help_and_quit() {
        assert_eq!(parse_command("/help"), Some(Command::Help));
        assert_eq!(parse_command("/quit"), Some(Command::Quit));
    }

    #[test]
    fn bogus_command_is_unknown() {
        assert!(matches!(parse_command("/bogus"), Some(Command::Unknown(_))));
    }
}
