//! `MockModel`: a scripted `Model` backend for deterministic tests without a
//! GPU. Replays a `Vec<MockTurn>` and records every `BoundedPrompt` it
//! received.

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use statie_core::{
    BoundedPrompt, Capabilities, Model, ModelError, RawOutput, Reasoning, Schema, StepSink, Usage,
    Version,
};
use tokio_util::sync::CancellationToken;

/// One scripted model response, or a scripted failure.
#[derive(Clone, Debug)]
pub struct MockTurn {
    pub reasoning: Option<String>,
    pub content: String,
    pub delay: Option<Duration>,
    pub usage: Option<Usage>,
    /// When set, this turn fails with this message (wrapped as
    /// `ModelError::Backend`) instead of returning `content`, e.g. to
    /// simulate a network/IO failure talking to a model server.
    pub error: Option<String>,
    /// Why the (simulated) backend stopped generating, e.g. `"stop"` or
    /// `"length"`. Default `None`.
    pub finish_reason: Option<String>,
}

impl MockTurn {
    pub fn new(content: impl Into<String>) -> Self {
        MockTurn {
            reasoning: None,
            content: content.into(),
            delay: None,
            usage: None,
            error: None,
            finish_reason: None,
        }
    }

    /// A turn that fails the `step` call with `ModelError::Backend(msg)`
    /// instead of producing output, e.g. to simulate an unreachable model
    /// server (`io error: error sending request for url (...)`).
    pub fn error(msg: impl Into<String>) -> Self {
        MockTurn {
            reasoning: None,
            content: String::new(),
            delay: None,
            usage: None,
            error: Some(msg.into()),
            finish_reason: None,
        }
    }

    pub fn with_reasoning(mut self, r: impl Into<String>) -> Self {
        self.reasoning = Some(r.into());
        self
    }

    pub fn with_delay(mut self, d: Duration) -> Self {
        self.delay = Some(d);
        self
    }

    pub fn with_usage(mut self, prompt_tokens: u64, completion_tokens: u64) -> Self {
        self.usage = Some(Usage {
            prompt_tokens,
            completion_tokens,
            prompt_eval_secs: None,
            eval_secs: None,
        });
        self
    }

    /// Sets `prompt_eval_secs` and `eval_secs` on this turn's reported
    /// `Usage`, for tests exercising prompt/eval timing (`peval=` in bench
    /// output, the session log's `usage` entry). Chain after
    /// [`Self::with_usage`] to also set token counts; if no `Usage` has been
    /// set yet, this starts one with `0` token counts.
    pub fn with_usage_timing(
        mut self,
        prompt_eval_secs: Option<f64>,
        eval_secs: Option<f64>,
    ) -> Self {
        let mut usage = self.usage.unwrap_or_default();
        usage.prompt_eval_secs = prompt_eval_secs;
        usage.eval_secs = eval_secs;
        self.usage = Some(usage);
        self
    }

    pub fn with_finish_reason(mut self, reason: impl Into<String>) -> Self {
        self.finish_reason = Some(reason.into());
        self
    }
}

/// A `BoundedPrompt`, flattened into an owned record, as seen by one
/// `MockModel::step` call.
#[derive(Clone, Debug)]
pub struct RecordedPrompt {
    pub system: String,
    pub user: String,
    pub tokens: usize,
    pub version: Version,
    pub schema: Value,
}

/// A scripted `Model`. Replays `turns` in order, recording every prompt it
/// is asked to answer.
pub struct MockModel {
    turns: Vec<MockTurn>,
    capabilities: Capabilities,
    repeat_last: bool,
    calls: Mutex<usize>,
    prompts: Mutex<Vec<RecordedPrompt>>,
}

impl MockModel {
    pub fn new(turns: Vec<MockTurn>) -> Self {
        MockModel {
            turns,
            capabilities: Capabilities {
                native_thinking: true,
                schema_constrained: true,
                prefix_cache: false,
            },
            repeat_last: false,
            calls: Mutex::new(0),
            prompts: Mutex::new(Vec::new()),
        }
    }

    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub fn repeat_last(mut self, repeat_last: bool) -> Self {
        self.repeat_last = repeat_last;
        self
    }

    /// All prompts recorded so far, in call order.
    pub fn prompts(&self) -> Vec<RecordedPrompt> {
        self.prompts
            .lock()
            .expect("mock model prompts mutex poisoned")
            .clone()
    }

    /// Number of `step` calls made so far.
    pub fn calls(&self) -> usize {
        *self.calls.lock().expect("mock model calls mutex poisoned")
    }
}

/// Streams `s` to `delta` in at least two chunks (split roughly in half) so
/// streaming consumers are exercised, even for very short strings.
fn stream_in_chunks(s: &str, mut delta: impl FnMut(&str)) {
    if s.is_empty() {
        return;
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 2 {
        delta(s);
        return;
    }
    let mid = chars.len().div_ceil(2);
    let first: String = chars[..mid].iter().collect();
    let second: String = chars[mid..].iter().collect();
    delta(&first);
    delta(&second);
}

#[async_trait]
impl Model for MockModel {
    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    async fn step(
        &self,
        prompt: BoundedPrompt,
        schema: &Schema,
        sink: &mut dyn StepSink,
        cancel: CancellationToken,
    ) -> Result<RawOutput, ModelError> {
        let idx = {
            let mut calls = self.calls.lock().expect("mock model calls mutex poisoned");
            let idx = *calls;
            *calls += 1;
            idx
        };

        self.prompts
            .lock()
            .expect("mock model prompts mutex poisoned")
            .push(RecordedPrompt {
                system: prompt.system().to_string(),
                user: prompt.user().to_string(),
                tokens: prompt.tokens(),
                version: prompt.version(),
                schema: schema.as_value().clone(),
            });

        let turn = if idx < self.turns.len() {
            Some(&self.turns[idx])
        } else if self.repeat_last {
            self.turns.last()
        } else {
            None
        };

        let Some(turn) = turn else {
            return Err(ModelError::Backend("mock script exhausted".to_string()));
        };

        if let Some(delay) = turn.delay {
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = cancel.cancelled() => {
                    return Err(ModelError::Cancelled);
                }
            }
        }

        if let Some(msg) = &turn.error {
            return Err(ModelError::Backend(msg.clone()));
        }

        if let Some(reasoning) = &turn.reasoning {
            stream_in_chunks(reasoning, |chunk| sink.reasoning_delta(chunk));
        }
        stream_in_chunks(&turn.content, |chunk| sink.content_delta(chunk));

        let mut out = RawOutput::new(
            prompt.version(),
            turn.reasoning.clone().map(Reasoning::new),
            turn.content.clone(),
        );
        if let Some(u) = turn.usage {
            out = out.with_usage(u);
        }
        if let Some(reason) = &turn.finish_reason {
            out = out.with_finish_reason(reason.clone());
        }
        Ok(out)
    }
}
