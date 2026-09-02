//! `SessionLogBm25`: retrieval over the session log via `SessionLog::search`.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;

use statie_core::{Augmenter, LogKind, Observation, SessionLog, Snippet, TokenBudget, Tokenizer};

use crate::fit::fit_snippets;
use crate::query::query_text;

const DEFAULT_KINDS: [LogKind; 3] = [LogKind::ToolOutput, LogKind::User, LogKind::System];
const MAX_TEXT_CHARS: usize = 600;

/// Retrieves relevant prior session-log entries as snippets. Never returns
/// `LogKind::Reasoning` entries, even if the caller asks for them (A1):
/// reasoning must never re-enter a prompt.
pub struct SessionLogBm25 {
    log: Arc<dyn SessionLog>,
    kinds: Vec<LogKind>,
    max_snippets: usize,
}

impl SessionLogBm25 {
    /// `kinds`: an empty vec defaults to `[ToolOutput, User, System]`. If
    /// the caller includes `LogKind::Reasoning`, it is stripped and a
    /// warning is logged — reasoning must never re-enter a prompt.
    pub fn new(log: Arc<dyn SessionLog>, kinds: Vec<LogKind>, max_snippets: usize) -> Self {
        let mut kinds = if kinds.is_empty() {
            DEFAULT_KINDS.to_vec()
        } else {
            kinds
        };

        let had_reasoning = kinds.contains(&LogKind::Reasoning);
        if had_reasoning {
            tracing::warn!(
                "SessionLogBm25::new: LogKind::Reasoning was requested but is never \
                 permitted in augmentation; stripping it"
            );
            kinds.retain(|k| *k != LogKind::Reasoning);
        }

        SessionLogBm25 {
            log,
            kinds,
            max_snippets,
        }
    }
}

fn payload_text(payload: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    collect_string_leaves(payload, &mut parts);
    parts.join(" ")
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

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

#[async_trait]
impl<W: Serialize + Send + Sync> Augmenter<W> for SessionLogBm25 {
    fn name(&self) -> &str {
        "session_log_bm25"
    }

    async fn augment(
        &self,
        s: &W,
        obs: Option<&Observation>,
        budget: TokenBudget,
        tok: &dyn Tokenizer,
    ) -> Vec<Snippet> {
        let query = query_text(s, obs);
        if query.trim().is_empty() {
            return Vec::new();
        }

        let mut snippets: Vec<Snippet> = Vec::new();
        for kind in &self.kinds {
            // Defense in depth: never let a Reasoning entry through, even
            // if `self.kinds` were somehow constructed to contain one.
            if *kind == LogKind::Reasoning {
                continue;
            }
            for entry in self.log.search(&query, Some(*kind), self.max_snippets) {
                if entry.kind == LogKind::Reasoning {
                    continue;
                }
                let text = truncate_chars(&payload_text(&entry.payload), MAX_TEXT_CHARS);
                snippets.push(Snippet {
                    source: format!("session step {} {:?}", entry.step, entry.kind),
                    text,
                });
            }
        }

        snippets.truncate(self.max_snippets);
        fit_snippets(snippets, budget, tok)
    }
}
