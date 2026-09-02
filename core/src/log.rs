//! Session log: append-only, user-facing history and its in-memory impl.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::LogError;
use crate::loop_types::Outcome;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogKind {
    User,
    Reasoning,
    Patch,
    Action,
    ToolOutput,
    System,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogEntry {
    pub step: u64,
    pub kind: LogKind,
    /// RFC3339 timestamp.
    pub ts: String,
    pub payload: Value,
}

/// Append-only session history, shared as `Arc<dyn SessionLog>`. Interior
/// mutability: implementors use their own locking.
pub trait SessionLog: Send + Sync {
    fn append(&self, e: LogEntry) -> Result<(), LogError>;
    /// The last `limit` entries of the given kind (or all kinds), oldest
    /// first ("newest last").
    fn entries(&self, kind: Option<LogKind>, limit: usize) -> Vec<LogEntry>;
    fn search(&self, query: &str, kind: Option<LogKind>, limit: usize) -> Vec<LogEntry>;
    fn last_step(&self) -> u64;
}

/// In-memory `SessionLog`. Substring search over the payload's JSON text;
/// BM25 lives in `statie-store`'s `JsonlLog`.
#[derive(Default)]
pub struct MemoryLog {
    entries: Mutex<Vec<LogEntry>>,
}

impl MemoryLog {
    pub fn new() -> Self {
        MemoryLog {
            entries: Mutex::new(Vec::new()),
        }
    }
}

impl SessionLog for MemoryLog {
    fn append(&self, e: LogEntry) -> Result<(), LogError> {
        self.entries
            .lock()
            .map_err(|_| LogError::Backend("lock poisoned".to_string()))?
            .push(e);
        Ok(())
    }

    fn entries(&self, kind: Option<LogKind>, limit: usize) -> Vec<LogEntry> {
        let guard = match self.entries.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let filtered: Vec<&LogEntry> = guard
            .iter()
            .filter(|e| kind.is_none_or(|k| e.kind == k))
            .collect();
        let start = filtered.len().saturating_sub(limit);
        filtered[start..].iter().map(|e| (*e).clone()).collect()
    }

    fn search(&self, query: &str, kind: Option<LogKind>, limit: usize) -> Vec<LogEntry> {
        let guard = match self.entries.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let needle = query.to_lowercase();
        let matches: Vec<&LogEntry> = guard
            .iter()
            .filter(|e| kind.is_none_or(|k| e.kind == k))
            .filter(|e| {
                let text = e.payload.to_string().to_lowercase();
                text.contains(&needle)
            })
            .collect();
        let start = matches.len().saturating_sub(limit);
        matches[start..].iter().map(|e| (*e).clone()).collect()
    }

    fn last_step(&self) -> u64 {
        let guard = match self.entries.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.iter().map(|e| e.step).max().unwrap_or(0)
    }
}

/// A step's log entries plus its outcome. Not complete until appended to a
/// `SessionLog` (B4).
#[must_use = "a step is not complete until its record is appended to the SessionLog"]
pub struct StepRecord {
    entries: Vec<LogEntry>,
    outcome: Outcome,
}

impl StepRecord {
    /// Constructor for the loop unit's `step()`, which is the only place a
    /// `StepRecord` is meant to be assembled.
    #[allow(dead_code)]
    pub(crate) fn new(entries: Vec<LogEntry>, outcome: Outcome) -> Self {
        StepRecord { entries, outcome }
    }

    /// Read-only view of the entries this step produced (for UIs that
    /// render them). The outcome is only reachable through
    /// [`SessionLogExt::append_record`].
    pub fn entries(&self) -> &[LogEntry] {
        &self.entries
    }
}

pub trait SessionLogExt {
    fn append_record(&self, r: StepRecord) -> Result<Outcome, LogError>;
}

impl<T: SessionLog + ?Sized> SessionLogExt for T {
    fn append_record(&self, r: StepRecord) -> Result<Outcome, LogError> {
        for entry in r.entries {
            self.append(entry)?;
        }
        Ok(r.outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(step: u64, kind: LogKind, payload: serde_json::Value) -> LogEntry {
        LogEntry {
            step,
            kind,
            ts: "2026-09-01T00:00:00Z".to_string(),
            payload,
        }
    }

    #[test]
    fn append_and_entries_kind_filter() {
        let log = MemoryLog::new();
        log.append(entry(1, LogKind::User, json!({"text": "hi"}))).unwrap();
        log.append(entry(1, LogKind::System, json!({"text": "note"}))).unwrap();
        log.append(entry(2, LogKind::User, json!({"text": "there"}))).unwrap();

        let all = log.entries(None, 10);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].step, 1);
        assert_eq!(all[2].step, 2);

        let users = log.entries(Some(LogKind::User), 10);
        assert_eq!(users.len(), 2);
        assert!(users.iter().all(|e| e.kind == LogKind::User));

        let limited = log.entries(None, 1);
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].step, 2);

        assert_eq!(log.last_step(), 2);
    }

    #[test]
    fn search_is_case_insensitive_substring_over_payload() {
        let log = MemoryLog::new();
        log.append(entry(1, LogKind::ToolOutput, json!({"text": "The Quick Brown Fox"})))
            .unwrap();
        log.append(entry(2, LogKind::ToolOutput, json!({"text": "nothing relevant"})))
            .unwrap();

        let hits = log.search("quick brown", None, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].step, 1);

        let none = log.search("giraffe", None, 10);
        assert!(none.is_empty());
    }

    #[test]
    fn step_record_append_record_returns_outcome() {
        let log = MemoryLog::new();
        let rec = StepRecord::new(
            vec![entry(1, LogKind::User, json!({"text": "hi"}))],
            Outcome::Continue,
        );
        let outcome = log.append_record(rec).unwrap();
        assert!(matches!(outcome, Outcome::Continue));
        assert_eq!(log.entries(None, 10).len(), 1);
    }
}
