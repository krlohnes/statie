//! `TranscriptAugmenter`: the append-only baseline harness's augmenter
//! (statie-spec.md §10, milestone 7).
//!
//! This models the common "just keep appending to a messages array"
//! approach: every step, it re-injects the *entire* prior transcript --
//! every `User`, `Patch`, `Action`, and `ToolOutput` log entry, in
//! order -- as augmentation snippets. Never `Reasoning` (A1: reasoning must
//! never re-enter a prompt). It ignores its budget argument except that the
//! result must never exceed it -- the eval harness configures a huge
//! augmentation budget (e.g. `budgets.augmentation = budgets.total =
//! 1_000_000`) so this is effectively unbounded, which is the point: this
//! augmenter exists to demonstrate the token growth SKILL.state's mutable,
//! bounded workspace avoids.
//!
//! Conceptually this is a single giant user-message append-only log (an
//! "append everything and re-send" chat history), just routed through the
//! same `Augmentation` section, `BoundedPrompt` machinery, and tokenizer as
//! every other augmenter, so token growth is measured on equal footing.

use async_trait::async_trait;
use serde_json::Value;

use statie_core::{Augmenter, LogKind, Observation, SessionLog, Snippet, TokenBudget, Tokenizer};

/// Replays the whole prior session log (bar `Reasoning`) as augmentation
/// snippets, every step. See the module doc for why.
pub struct TranscriptAugmenter {
    log: std::sync::Arc<dyn SessionLog>,
}

impl TranscriptAugmenter {
    pub fn new(log: std::sync::Arc<dyn SessionLog>) -> Self {
        TranscriptAugmenter { log }
    }
}

fn payload_repr(payload: &Value) -> String {
    let mut leaves = Vec::new();
    collect_string_leaves(payload, &mut leaves);
    if leaves.is_empty() {
        // No string leaves (e.g. an all-numeric/boolean patch) -- fall back
        // to compact JSON so the entry isn't dropped silently.
        serde_json::to_string(payload).unwrap_or_default()
    } else {
        leaves.join(" ")
    }
}

fn collect_string_leaves(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => {
            for item in items {
                collect_string_leaves(item, out);
            }
        }
        Value::Object(map) => {
            for v in map.values() {
                collect_string_leaves(v, out);
            }
        }
        _ => {}
    }
}

fn kind_label(kind: LogKind) -> &'static str {
    match kind {
        LogKind::User => "user",
        LogKind::Reasoning => "reasoning",
        LogKind::Patch => "patch",
        LogKind::Action => "action",
        LogKind::ToolOutput => "tool_output",
        LogKind::System => "system",
    }
}

#[async_trait]
impl<W: Send + Sync> Augmenter<W> for TranscriptAugmenter {
    fn name(&self) -> &str {
        "transcript"
    }

    async fn augment(
        &self,
        _s: &W,
        _obs: Option<&Observation>,
        budget: TokenBudget,
        tok: &dyn Tokenizer,
    ) -> Vec<Snippet> {
        let entries = self.log.entries(None, usize::MAX);

        let mut total = 0usize;
        let mut snippets = Vec::new();
        for entry in entries {
            if entry.kind == LogKind::Reasoning {
                continue;
            }
            if !matches!(
                entry.kind,
                LogKind::User | LogKind::Patch | LogKind::Action | LogKind::ToolOutput
            ) {
                continue;
            }

            let text = format!(
                "[step {}] {}: {}",
                entry.step,
                kind_label(entry.kind),
                payload_repr(&entry.payload)
            );
            let snippet = Snippet {
                source: format!("transcript step {} {}", entry.step, kind_label(entry.kind)),
                text,
            };

            let rendered = format!("[{}]\n{}", snippet.source, snippet.text);
            let tokens = tok.count(&rendered);
            // Ignore the budget except as a hard ceiling: never exceed it,
            // but don't stop early either -- the whole point of this
            // baseline is that it keeps appending regardless of size.
            if total + tokens > budget.0 {
                break;
            }
            total += tokens;
            snippets.push(snippet);
        }

        snippets
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use statie_core::{HeuristicTokenizer, LogEntry, MemoryLog};
    use std::sync::Arc;

    fn entry(step: u64, kind: LogKind, payload: Value) -> LogEntry {
        LogEntry {
            step,
            kind,
            ts: "2026-09-01T00:00:00Z".to_string(),
            payload,
        }
    }

    #[tokio::test]
    async fn snippets_contain_user_patch_tool_output_in_order_and_never_reasoning() {
        let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
        log.append(entry(
            1,
            LogKind::User,
            serde_json::json!({"id": 1, "text": "do the thing"}),
        ))
        .unwrap();
        log.append(entry(
            1,
            LogKind::Reasoning,
            serde_json::json!({"text": "the secret reasoning nobody should see"}),
        ))
        .unwrap();
        log.append(entry(
            1,
            LogKind::Patch,
            serde_json::json!({"version": 1, "patch": {"counter": 1}}),
        ))
        .unwrap();
        log.append(entry(
            1,
            LogKind::ToolOutput,
            serde_json::json!({"name": "echo", "output": "echo: hello", "truncated": false}),
        ))
        .unwrap();

        let augmenter = TranscriptAugmenter::new(Arc::clone(&log));
        let tok = HeuristicTokenizer;
        let snippets: Vec<Snippet> =
            Augmenter::<()>::augment(&augmenter, &(), None, TokenBudget(1_000_000), &tok).await;

        // Exactly the three non-reasoning kinds, in log order.
        assert_eq!(snippets.len(), 3);
        assert!(snippets[0].text.contains("do the thing"));
        assert!(snippets[1].text.contains("counter"));
        assert!(snippets[2].text.contains("echo: hello"));

        let combined: String = snippets.iter().map(|s| s.text.as_str()).collect();
        assert!(
            !combined.contains("the secret reasoning nobody should see"),
            "reasoning text must never appear in augmentation snippets"
        );
    }

    #[tokio::test]
    async fn augmenter_name_is_transcript() {
        let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
        let augmenter = TranscriptAugmenter::new(log);
        assert_eq!(Augmenter::<()>::name(&augmenter), "transcript");
    }

    #[tokio::test]
    async fn never_exceeds_the_given_budget() {
        let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
        for i in 0..20u64 {
            log.append(entry(
                i,
                LogKind::ToolOutput,
                serde_json::json!({"name": "echo", "output": "x".repeat(200), "truncated": false}),
            ))
            .unwrap();
        }

        let augmenter = TranscriptAugmenter::new(Arc::clone(&log));
        let tok = HeuristicTokenizer;
        let budget_tokens = 50;
        let snippets: Vec<Snippet> =
            Augmenter::<()>::augment(&augmenter, &(), None, TokenBudget(budget_tokens), &tok).await;

        let total: usize = snippets
            .iter()
            .map(|s| tok.count(&format!("[{}]\n{}", s.source, s.text)))
            .sum();
        assert!(
            total <= budget_tokens,
            "total {total} exceeded budget {budget_tokens}"
        );
    }
}
