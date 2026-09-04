//! `ModelMaintainer`: drives a `Model` backend for context maintenance
//! (statie-spec.md "9. Context maintenance") via a fixed system prompt and
//! schema-constrained `{notes, next?}` output.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use statie_core::{
    MaintainError, MaintainInput, MaintainOutput, Maintainer, MaintainerReply, Model, NullSink,
    Tokenizer, build_maintainer_prompt, maintainer_output_schema,
};
use tokio_util::sync::CancellationToken;

/// Strips a leading/trailing ```json (or bare ```) fence, if present. Not
/// `core::skill::strip_fence` — that helper is private to `statie-core`, so
/// this is a minimal, independent re-implementation of the same fence
/// stripping used by `ParsedOutput::parse`.
fn strip_json_fence(s: &str) -> &str {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("```json") {
        let rest = rest.trim_start();
        let rest = rest.strip_suffix("```").unwrap_or(rest);
        return rest.trim();
    }
    if let Some(rest) = s.strip_prefix("```") {
        let rest = rest.trim_start();
        let rest = rest.strip_suffix("```").unwrap_or(rest);
        return rest.trim();
    }
    s
}

/// Drives a `Model` backend for context maintenance: builds the maintainer
/// prompt, calls `model.step` under a timeout, parses `{notes, next?}` from
/// the response, and retries once (with a restated cap) if `notes` comes
/// back over `input.budget`.
pub struct ModelMaintainer {
    model: Arc<dyn Model>,
    tokenizer: Arc<dyn Tokenizer>,
    total_budget: usize,
    timeout: Duration,
}

impl ModelMaintainer {
    pub fn new(
        model: Arc<dyn Model>,
        tokenizer: Arc<dyn Tokenizer>,
        total_budget: usize,
        timeout: Duration,
    ) -> Self {
        ModelMaintainer {
            model,
            tokenizer,
            total_budget,
            timeout,
        }
    }

    /// One model call: build the prompt, call under the timeout, parse the
    /// reply. Does not handle the over-budget retry -- that's `maintain`'s
    /// job, since it needs to build a second prompt with `retry_note` set.
    async fn call_once(
        &self,
        input: &MaintainInput<'_>,
        retry_note: Option<&str>,
    ) -> Result<MaintainerReply, MaintainError> {
        let prompt = build_maintainer_prompt(
            self.tokenizer.as_ref(),
            input,
            self.total_budget,
            retry_note,
        )
        .map_err(|e| MaintainError::Backend(format!("maintainer prompt over budget: {e}")))?;

        let schema = maintainer_output_schema();
        let mut sink = NullSink;

        let step = tokio::time::timeout(
            self.timeout,
            self.model
                .step(prompt, &schema, &mut sink, CancellationToken::new()),
        )
        .await;

        let raw = match step {
            Ok(Ok(raw)) => raw,
            Ok(Err(e)) => return Err(MaintainError::Backend(e.to_string())),
            Err(_elapsed) => return Err(MaintainError::Timeout),
        };

        let cleaned = strip_json_fence(&raw.content);
        serde_json::from_str::<MaintainerReply>(cleaned).map_err(|e| {
            let preview: String = raw.content.chars().take(200).collect();
            MaintainError::Parse(format!("{e}: {preview}"))
        })
    }
}

#[async_trait]
impl Maintainer for ModelMaintainer {
    async fn maintain(&self, input: MaintainInput<'_>) -> Result<MaintainOutput, MaintainError> {
        let reply = self.call_once(&input, None).await?;
        let notes_tokens = self.tokenizer.count(&reply.notes);

        if notes_tokens <= input.budget.0 {
            return Ok(MaintainOutput {
                notes: reply.notes,
                next: reply.next,
            });
        }

        let retry_note = format!(
            "your previous notes were {notes_tokens} tokens; the cap is {}. Cut from the end.",
            input.budget.0
        );
        let retry_reply = self.call_once(&input, Some(&retry_note)).await?;
        let retry_tokens = self.tokenizer.count(&retry_reply.notes);

        if retry_tokens <= input.budget.0 {
            return Ok(MaintainOutput {
                notes: retry_reply.notes,
                next: retry_reply.next,
            });
        }

        Err(MaintainError::OverBudget {
            tokens: retry_tokens,
            budget: input.budget.0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_json_fence_removes_json_fence() {
        let s = "```json\n{\"a\":1}\n```";
        assert_eq!(strip_json_fence(s), "{\"a\":1}");
    }

    #[test]
    fn strip_json_fence_removes_bare_fence() {
        let s = "```\n{\"a\":1}\n```";
        assert_eq!(strip_json_fence(s), "{\"a\":1}");
    }

    #[test]
    fn strip_json_fence_passes_through_unfenced() {
        let s = "{\"a\":1}";
        assert_eq!(strip_json_fence(s), "{\"a\":1}");
    }
}
