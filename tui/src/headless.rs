//! `run_headless`: drives the shared engine (`driver::run_driver`) without a
//! terminal, printing a readable plain-text transcript and returning a
//! `HeadlessSummary`.

use std::io::Write;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::mpsc;

use statie_core::Session;

use crate::driver::{DriverCmd, UiEvent, run_driver};
use crate::error::TuiError;
use crate::registry::{RunOptions, SkillRegistry};

/// Summary of a headless run, returned once the driver parks, finishes, or
/// quits.
pub struct HeadlessSummary {
    pub steps: usize,
    pub final_outcome: String,
    pub prompt_tokens: Vec<usize>,
    /// Server-reported prompt token counts, one entry per model call that
    /// reported usage (may be shorter than `prompt_tokens` when some calls
    /// reported none, e.g. the mock backend).
    pub server_prompt_tokens: Vec<u64>,
    /// Server-reported completion token counts, aligned with
    /// `server_prompt_tokens`.
    pub server_completion_tokens: Vec<u64>,
    pub final_state: serde_json::Value,
}

/// Runs `session` headlessly: feeds `opts.initial_messages` through the
/// driver, prints every `UiEvent` to `out` in plain text, and stops at the
/// first `Parked`/`Done`/`Quit` (or if the driver task itself ends).
pub async fn run_headless<W>(
    session: Session<W>,
    registry: Arc<dyn SkillRegistry<W>>,
    opts: RunOptions,
    out: &mut (dyn Write + Send),
) -> Result<HeadlessSummary, TuiError>
where
    W: Clone + Serialize + DeserializeOwned + JsonSchema + Send + Sync + 'static,
{
    let cancel = session.cancel_handle();
    let (cmd_tx, cmd_rx) = mpsc::channel::<DriverCmd>(16);
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<UiEvent>();

    // The driver itself drains `opts.initial_messages` on activation, so we
    // don't need to send `DriverCmd::User` for them; just keep `cmd_tx`
    // alive so the driver's command loop doesn't see a closed channel while
    // we're still reading events, and let it drop (closing the channel) once
    // we're done -- there is nothing else headless mode wants to send.
    let driver_opts = opts.clone();
    let driver_handle = tokio::spawn(async move {
        run_driver(session, registry, driver_opts, cmd_rx, ev_tx, cancel).await
    });
    drop(cmd_tx);

    let mut steps = 0usize;
    let mut final_outcome = String::new();
    let mut prompt_tokens: Vec<usize> = Vec::new();
    let mut server_prompt_tokens: Vec<u64> = Vec::new();
    let mut server_completion_tokens: Vec<u64> = Vec::new();
    let mut final_state = Value::Null;
    let mut reasoning_open = false;

    // Set when every sender is gone, i.e. the driver task has returned (or is
    // about to); in that case its result must be awaited, never aborted.
    let mut channel_closed = false;
    loop {
        let Some(event) = ev_rx.recv().await else {
            channel_closed = true;
            break;
        };

        match event {
            UiEvent::ReasoningDelta(_s) => {
                if !reasoning_open {
                    let _ = writeln!(out, "[reasoning] ...");
                    reasoning_open = true;
                }
            }
            UiEvent::ContentDelta(_) => {
                // Not printed.
            }
            UiEvent::Replay {
                entries,
                step,
                skill,
                state: _,
            } => {
                let _ = writeln!(
                    out,
                    "[resumed] step {step} in {skill}; replaying {} entries",
                    entries.len()
                );
                for e in &entries {
                    let _ = writeln!(out, "  {}", crate::driver::render_entry_line(e));
                }
            }
            UiEvent::StepDone {
                entries,
                outcome,
                prompt_tokens: pt,
                usage,
                state,
            } => {
                reasoning_open = false;
                steps += 1;
                final_outcome = outcome.clone();
                if let Some(t) = pt {
                    prompt_tokens.push(t);
                }
                if let Some(u) = usage {
                    server_prompt_tokens.push(u.prompt_tokens);
                    server_completion_tokens.push(u.completion_tokens);
                }
                if !state.is_null() {
                    final_state = state;
                }
                match (pt, usage) {
                    (Some(t), Some(u)) => {
                        let _ = writeln!(
                            out,
                            "[step {steps}] outcome: {outcome} (prompt ≈{t} tokens; server prompt {}, completion {})",
                            u.prompt_tokens, u.completion_tokens
                        );
                    }
                    _ => {
                        let _ = writeln!(out, "[step {steps}] outcome: {outcome}");
                    }
                }
                for e in &entries {
                    let _ = writeln!(out, "  {}", crate::driver::render_entry_line(e));
                }
            }
            UiEvent::Parked(msg) => {
                let _ = writeln!(out, "[parked] {msg}");
                break;
            }
            UiEvent::Activated(name) => {
                let _ = writeln!(out, "[activated] {name}");
            }
            UiEvent::Done => {
                let _ = writeln!(out, "[done]");
                break;
            }
            UiEvent::Info(text) => {
                let _ = writeln!(out, "[info] {text}");
            }
            UiEvent::Error(text) => {
                let _ = writeln!(out, "[error] {text}");
            }
            UiEvent::Maintainer {
                notes_tokens,
                next_set,
            } => {
                let suffix = if next_set { ", set next" } else { "" };
                let _ = writeln!(out, "[maintainer] notes: {notes_tokens} tokens{suffix}");
            }
            UiEvent::MaintainerError(reason) => {
                let _ = writeln!(out, "[maintainer error] {reason}");
            }
            UiEvent::Quit => {
                break;
            }
        }
    }

    // If the driver task already finished, surface its error instead of
    // silently reporting an empty run; otherwise abort it so we never hang.
    if channel_closed || driver_handle.is_finished() {
        match driver_handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = writeln!(out, "[error] {e}");
                return Err(e);
            }
            Err(join) => return Err(TuiError::Other(format!("driver task panicked: {join}"))),
        }
    } else {
        driver_handle.abort();
        let _ = driver_handle.await;
    }

    Ok(HeadlessSummary {
        steps,
        final_outcome,
        prompt_tokens,
        server_prompt_tokens,
        server_completion_tokens,
        final_state,
    })
}
