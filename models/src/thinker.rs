//! `ModelThinker`: drives an `OllamaModel` backend for the `think`
//! reasoning subagent call -- thinking on, no output schema.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use statie_core::{
    THINK_SYSTEM_PROMPT, ThinkError, ThinkInput, ThinkOutput, Thinker, extract_answer,
};
use tokio_util::sync::CancellationToken;

use crate::ollama::OllamaModel;

/// Builds the free-text user prompt sent to the reasoning model: the
/// calling step's own USER prompt, verbatim (its workspace state, harness
/// notes, and last tool output), followed by its question and an
/// instruction to reply in prose.
///
/// Deliberately does NOT include `input.system`: that is the acting model's
/// system prompt, which carries its own output contract (JSON shape,
/// `<thoughts>` tags, tool-call instructions). A first live battery showed
/// the reasoning subagent imitate that contract when it was included here --
/// writing a `<thoughts>` block, planning tool calls, and never emitting
/// `ANSWER:` -- so its entire 24,815-character reply became the "answer".
/// See `ThinkInput::system`'s doc (`core/src/thinker.rs`).
pub(crate) fn build_think_prompt(input: &ThinkInput<'_>) -> String {
    format!(
        "Below is everything the acting model currently sees (its workspace state, harness \
         notes, and the last tool output), followed by its question.\n\n{}\n\nQuestion: {}\n\n\
         Reply in prose. Do not imitate the format above, do not output JSON, and do not plan \
         tool calls; the acting model will act. End with a section headed exactly `ANSWER:` of \
         at most ten lines.",
        input.user, input.question
    )
}

/// Drives an `OllamaModel` for the `think` reasoning subagent: calls
/// `chat_free` with `think: true` and no schema, under a timeout, and
/// extracts the conclusion after the last `ANSWER:` marker.
pub struct ModelThinker {
    model: Arc<OllamaModel>,
    timeout: Duration,
}

impl ModelThinker {
    pub fn new(model: Arc<OllamaModel>, timeout: Duration) -> Self {
        ModelThinker { model, timeout }
    }
}

#[async_trait]
impl Thinker for ModelThinker {
    async fn think(
        &self,
        input: ThinkInput<'_>,
        cancel: CancellationToken,
    ) -> Result<ThinkOutput, ThinkError> {
        let user = build_think_prompt(&input);

        let call = tokio::time::timeout(
            self.timeout,
            self.model
                .chat_free(THINK_SYSTEM_PROMPT, &user, true, cancel),
        )
        .await;

        let reply = match call {
            Ok(Ok(reply)) => reply,
            Ok(Err(e)) => return Err(ThinkError::Backend(e.to_string())),
            Err(_elapsed) => return Err(ThinkError::Timeout),
        };

        let content = reply.content.trim();
        if content.is_empty() {
            if reply.done_reason.as_deref() == Some("length") {
                // A second live battery reproduced this exactly: with the
                // session's num_ctx of 16384 and a 5088-token prompt, the
                // reasoning model thought for 11296 tokens, hit the context
                // limit (`done_reason: "length"`), and produced no content
                // at all. Native thinking on this model routinely runs
                // 8k-11k tokens, so an exhausted window is a distinct,
                // actionable failure -- not the generic `Empty` (which
                // covers a model that simply said nothing, e.g. `done_reason:
                // "stop"` with no content).
                return Err(ThinkError::Backend(format!(
                    "thinking exhausted the context window (num_ctx {}) before answering; raise \
                     `thinker.num_ctx`",
                    self.model.num_ctx()
                )));
            }
            return Err(ThinkError::Empty);
        }

        // Some backends deliver reasoning inline as a leading
        // `<thoughts>...</thoughts>` block instead of (or in addition to)
        // native thinking. Strip it before extracting the answer so the
        // `ANSWER:` marker is found in the actual reply, not buried inside
        // the reasoning block. If the model put everything inside the tags
        // (remainder empty after stripping) but the tags held something,
        // fall back to the reasoning text itself rather than an empty
        // string.
        let (inline_reasoning, stripped_content) = crate::thoughts::strip_thoughts(content);
        let inline_reasoning_chars = inline_reasoning
            .as_ref()
            .map(|r| r.as_ref().chars().count())
            .unwrap_or(0);
        let answer_source: &str = if stripped_content.is_empty() {
            match &inline_reasoning {
                Some(r) if !r.as_ref().is_empty() => r.as_ref(),
                _ => content,
            }
        } else {
            &stripped_content
        };

        Ok(ThinkOutput {
            answer: extract_answer(answer_source),
            reasoning_chars: reply.thinking.chars().count() + inline_reasoning_chars,
            content_chars: content.chars().count(),
            usage: reply.usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_think_prompt_omits_system_and_includes_user_and_question() {
        let input = ThinkInput {
            system: "the system prompt",
            user: "the user prompt",
            question: "what's wrong here?",
        };
        let prompt = build_think_prompt(&input);
        assert!(!prompt.contains("the system prompt"));
        assert!(prompt.contains("the user prompt"));
        assert!(prompt.contains("Question: what's wrong here?"));
        assert!(prompt.contains("Reply in prose"));
    }
}
