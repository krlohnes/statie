//! `ModelFixer`: drives an `OllamaModel` backend for the `try_fixes`
//! proposer call -- thinking off, JSON output, one call per candidate.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use statie_core::{
    CandidateEdit, FIX_SYSTEM_PROMPT, FixError, FixInput, FixOutput, Fixer, parse_candidates,
};
use tokio_util::sync::CancellationToken;

use crate::ollama::OllamaModel;

/// `num_predict` for each candidate call: short output (one JSON object),
/// not a reasoning trace.
const FIXER_NUM_PREDICT: u32 = 1200;

/// Temperature for every candidate call: high enough that `k` calls at
/// different seeds actually diverge.
const FIXER_TEMPERATURE: f64 = 0.7;

/// Drives an `OllamaModel` for the `try_fixes` proposer: makes `input.k`
/// sequential calls (one GPU, thinking off, JSON format), each asking for
/// exactly one candidate fix at a different seed so the samples differ,
/// then unions and dedupes the parsed candidates across calls.
pub struct ModelFixer {
    model: Arc<OllamaModel>,
    timeout: Duration,
    /// Base seed; call `i` (0-indexed) uses `base_seed + i`.
    base_seed: u64,
}

impl ModelFixer {
    pub fn new(model: Arc<OllamaModel>, timeout: Duration) -> Self {
        ModelFixer {
            model,
            timeout,
            base_seed: 0,
        }
    }

    /// Overrides the base seed (default 0). Call `i` (0-indexed) uses
    /// `base_seed + i`.
    pub fn with_base_seed(mut self, base_seed: u64) -> Self {
        self.base_seed = base_seed;
        self
    }
}

#[async_trait]
impl Fixer for ModelFixer {
    async fn propose(
        &self,
        input: FixInput,
        cancel: CancellationToken,
    ) -> Result<FixOutput, FixError> {
        let user = format!(
            "{}\n\nFailing test output:\n{}\n\nPropose exactly 1 candidate fix as a \
             one-element JSON array.",
            input.user, input.failure
        );

        let mut candidates: Vec<CandidateEdit> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut usage = None;
        let mut any_call_succeeded = false;
        let mut last_err: Option<FixError> = None;

        for i in 0..input.k {
            let seed = self.base_seed + i as u64;
            let extra_options = json!({
                "temperature": FIXER_TEMPERATURE,
                "seed": seed,
                "num_predict": FIXER_NUM_PREDICT,
            });

            let call = tokio::time::timeout(
                self.timeout,
                self.model.chat_json_with(
                    FIX_SYSTEM_PROMPT,
                    &user,
                    false,
                    extra_options,
                    cancel.clone(),
                ),
            )
            .await;

            let reply = match call {
                Ok(Ok(reply)) => reply,
                Ok(Err(e)) => {
                    last_err = Some(FixError::Backend(e.to_string()));
                    continue;
                }
                Err(_elapsed) => {
                    last_err = Some(FixError::Timeout);
                    continue;
                }
            };
            any_call_succeeded = true;

            let content = reply.content.trim();
            if content.is_empty() {
                if reply.done_reason.as_deref() == Some("length") {
                    // The candidate call hit its num_predict cap before
                    // producing any content (mirrors the thinker's
                    // context-exhaustion case, but here it's the short
                    // output budget, not the context window). Skip this
                    // call like any other empty reply; only error overall
                    // if every call produced nothing usable.
                    tracing::debug!(
                        seed,
                        "fixer candidate call hit num_predict before producing content"
                    );
                }
                continue;
            }

            // Some backends leak thinking inline even with `think: false`;
            // strip it before parsing so a stray `<thoughts>` block doesn't
            // break the JSON scan.
            let (_, stripped) = crate::thoughts::strip_thoughts(content);
            let parse_source: &str = if stripped.is_empty() {
                content
            } else {
                &stripped
            };

            for cand in parse_candidates(parse_source) {
                let key = (cand.path.clone(), cand.old.clone(), cand.new.clone());
                if seen.insert(key) {
                    candidates.push(cand);
                }
            }
            usage = reply.usage.or(usage);
        }

        if candidates.is_empty() {
            if !any_call_succeeded && let Some(err) = last_err {
                return Err(err);
            }
            return Err(FixError::Empty);
        }

        Ok(FixOutput { candidates, usage })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixer_num_predict_and_temperature_are_fixed() {
        // Sanity check the constants used in the request body match the
        // unit's contract; the wire-level assertions live in the
        // integration test (models/tests/fixer.rs).
        assert_eq!(FIXER_NUM_PREDICT, 1200);
        assert_eq!(FIXER_TEMPERATURE, 0.7);
    }
}
