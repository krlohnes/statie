//! `ModelThinker`: drives an `OllamaModel` backend for the `think`
//! reasoning subagent call -- thinking on, no output schema.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use statie_core::{
    THINK_SYSTEM_PROMPT, ThinkError, ThinkInput, ThinkOutput, Thinker, extract_answer,
};
use tokio_util::sync::CancellationToken;

use crate::ollama::OllamaModel;

/// Config for the Markovian-Thinker chunked reasoning loop (Aghajohari et
/// al., ICLR 2026): instead of one native-thinking call that can exhaust the
/// context window before ever emitting content, reasoning runs in fixed-size
/// chunks with a short summarized carry-over between them, so the whole pass
/// fits inside the acting model's own window.
#[derive(Clone, Copy, Debug)]
pub struct ChunkConfig {
    /// `num_predict` sent on each reasoning chunk's call.
    pub chunk_tokens: u32,
    /// Maximum number of chunks before giving up with `ThinkError::Backend`.
    pub max_chunks: u32,
    /// How many trailing characters of a chunk's thinking text to hand the
    /// carry-over summarization call when the chunk's thinking is longer
    /// than this.
    pub carry_over_chars: usize,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        ChunkConfig {
            chunk_tokens: 6000,
            max_chunks: 4,
            // The whole chunk's thinking, in practice: a 6000-token chunk
            // is ~20k chars, and the summariser runs with thinking off in
            // the same window, so it can see all of it. Summarising only
            // the tail lost what the chunk established early on.
            carry_over_chars: 24_000,
        }
    }
}

/// `num_predict` for the carry-over summarization call: a short prose
/// summary, not another reasoning pass.
const CARRY_OVER_NUM_PREDICT: u32 = 600;

const CARRY_OVER_INSTRUCTION: &str = "Summarize the reasoning so far in at most 10 lines: what \
                                       has been established, what is still open, and the next \
                                       step to check. Do not answer the question.";

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
/// extracts the conclusion after the last `ANSWER:` marker. With
/// `.with_chunking(..)`, instead runs the Markovian-Thinker chunked loop
/// (see `ChunkConfig`) so a long reasoning trace fits inside the model's own
/// context window rather than needing a separate, larger one.
pub struct ModelThinker {
    model: Arc<OllamaModel>,
    timeout: Duration,
    chunked: Option<ChunkConfig>,
}

impl ModelThinker {
    pub fn new(model: Arc<OllamaModel>, timeout: Duration) -> Self {
        ModelThinker {
            model,
            timeout,
            chunked: None,
        }
    }

    /// Enables the chunked reasoning loop with the given config.
    pub fn with_chunking(mut self, chunked: ChunkConfig) -> Self {
        self.chunked = Some(chunked);
        self
    }
}

/// Extracts the answer from a finished (non-empty-content) reply, folding in
/// any inline `<thoughts>` block the same way the unchunked path always has.
/// Returns `(answer, inline_reasoning_chars)`.
fn extract_answer_from_content(content: &str) -> (String, usize) {
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
    (extract_answer(answer_source), inline_reasoning_chars)
}

#[async_trait]
impl Thinker for ModelThinker {
    async fn think(
        &self,
        input: ThinkInput<'_>,
        cancel: CancellationToken,
    ) -> Result<ThinkOutput, ThinkError> {
        let user = build_think_prompt(&input);

        if let Some(chunk_cfg) = self.chunked {
            return self.think_chunked(&user, chunk_cfg, cancel).await;
        }

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

        let (answer, inline_reasoning_chars) = extract_answer_from_content(content);

        Ok(ThinkOutput {
            answer,
            reasoning_chars: reply.thinking.chars().count() + inline_reasoning_chars,
            content_chars: content.chars().count(),
            usage: reply.usage,
            chunks: 1,
        })
    }
}

impl ModelThinker {
    /// The Markovian-Thinker chunked reasoning loop: repeatedly calls
    /// `chat_free_with` capped at `chunk_cfg.chunk_tokens` via `num_predict`.
    /// When a chunk hits `done_reason: "length"` with no content (the model
    /// was still thinking when the cap landed), a short carry-over
    /// summarization call (thinking off) condenses that chunk's reasoning,
    /// and the next chunk restarts from the original prompt plus that
    /// summary. Stops as soon as a chunk produces content (the model
    /// finished thinking and answered), or after `chunk_cfg.max_chunks` with
    /// none.
    async fn think_chunked(
        &self,
        original_user: &str,
        chunk_cfg: ChunkConfig,
        cancel: CancellationToken,
    ) -> Result<ThinkOutput, ThinkError> {
        let mut user = original_user.to_string();
        let mut total_reasoning_chars = 0usize;

        for chunk_idx in 1..=chunk_cfg.max_chunks {
            let extra_options = json!({ "num_predict": chunk_cfg.chunk_tokens });
            let call = tokio::time::timeout(
                self.timeout,
                self.model.chat_free_with(
                    THINK_SYSTEM_PROMPT,
                    &user,
                    true,
                    extra_options,
                    cancel.clone(),
                ),
            )
            .await;

            let reply = match call {
                Ok(Ok(reply)) => reply,
                Ok(Err(e)) => return Err(ThinkError::Backend(e.to_string())),
                Err(_elapsed) => return Err(ThinkError::Timeout),
            };

            let thinking_chars = reply.thinking.chars().count();
            total_reasoning_chars += thinking_chars;

            tracing::debug!(
                chunk = chunk_idx,
                max_chunks = chunk_cfg.max_chunks,
                thinking_chars,
                done_reason = reply.done_reason.as_deref().unwrap_or("<none>"),
                "chunked thinker: chunk done"
            );

            let content = reply.content.trim();
            if !content.is_empty() {
                let (answer, inline_reasoning_chars) = extract_answer_from_content(content);
                return Ok(ThinkOutput {
                    answer,
                    reasoning_chars: total_reasoning_chars + inline_reasoning_chars,
                    content_chars: content.chars().count(),
                    usage: reply.usage,
                    chunks: chunk_idx,
                });
            }

            if reply.done_reason.as_deref() != Some("length") || chunk_idx == chunk_cfg.max_chunks {
                break;
            }

            // Carry over: summarize this chunk's thinking (last
            // `carry_over_chars` of it if longer), thinking off, then start
            // the next chunk from the ORIGINAL prompt plus that summary.
            let thinking_tail = tail_chars(&reply.thinking, chunk_cfg.carry_over_chars);
            let carry_over_prompt = format!("{thinking_tail}\n\n{CARRY_OVER_INSTRUCTION}");
            let carry_over_call = tokio::time::timeout(
                self.timeout,
                self.model.chat_free_with(
                    THINK_SYSTEM_PROMPT,
                    &carry_over_prompt,
                    false,
                    json!({ "num_predict": CARRY_OVER_NUM_PREDICT }),
                    cancel.clone(),
                ),
            )
            .await;

            let carry_over_reply = match carry_over_call {
                Ok(Ok(reply)) => reply,
                Ok(Err(e)) => return Err(ThinkError::Backend(e.to_string())),
                Err(_elapsed) => return Err(ThinkError::Timeout),
            };
            let summary = carry_over_reply.content.trim();

            user = format!(
                "{original_user}\n\nReasoning so far (from a previous pass):\n{summary}\n\
                 Continue from there."
            );
        }

        Err(ThinkError::Backend(format!(
            "thinking exhausted {} chunks of {} tokens without answering",
            chunk_cfg.max_chunks, chunk_cfg.chunk_tokens
        )))
    }
}

/// Returns the last `max_chars` characters of `s` (char-boundary safe), or
/// all of `s` if it's shorter.
fn tail_chars(s: &str, max_chars: usize) -> &str {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        return s;
    }
    let start_char = char_count - max_chars;
    let byte_start = s
        .char_indices()
        .nth(start_char)
        .map(|(i, _)| i)
        .unwrap_or(0);
    &s[byte_start..]
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
