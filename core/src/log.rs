//! Session log: append-only, user-facing history and its in-memory impl.

use std::path::Path;
use std::sync::{Arc, Mutex};

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
    /// The child's agent path (e.g. `"implement/ui-review#3"`) when this
    /// entry belongs to a subagent; `None` for the parent's own entries.
    /// `recall` defaults to the caller's own path and accepts `agent: "*"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
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

/// Reads a JSONL session log from disk without opening it for append and
/// without creating the file if it's missing (unlike `statie-store`'s
/// `JsonlLog::open`, which is write-oriented and will create an empty file
/// at `path`). Used by read-only tooling (e.g. `statie metrics`) that only
/// ever inspects a log another process wrote.
///
/// Errors clearly (`LogError::Io`) when `path` does not exist, and
/// (`LogError::Backend`, naming the 1-based line number) on a line that
/// isn't valid JSON or doesn't match `LogEntry`'s shape. Blank lines are
/// skipped, matching `JsonlLog`'s own tolerance.
pub fn read_jsonl(path: impl AsRef<Path>) -> Result<Vec<LogEntry>, LogError> {
    let path = path.as_ref();
    if !path.exists() {
        return Err(LogError::Io(format!("no such file: {}", path.display())));
    }
    let text = std::fs::read_to_string(path).map_err(|e| LogError::Io(e.to_string()))?;
    let mut entries = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: LogEntry = serde_json::from_str(line)
            .map_err(|e| LogError::Backend(format!("malformed log line {}: {}", idx + 1, e)))?;
        entries.push(entry);
    }
    Ok(entries)
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

/// Wraps a shared `SessionLog` so every entry appended through it is tagged
/// with a child's agent path (milestone 9, ARCHITECTURE.md "Configured
/// child skills": "the shared log (entries carry `agent: "<path>"`; the
/// parent's entries carry none"). `entries`/`search`/`last_step` pass
/// through to the inner log unfiltered -- agent-scoped `recall` is out of
/// scope for this unit.
pub struct TaggedLog {
    inner: Arc<dyn SessionLog>,
    agent: String,
}

impl TaggedLog {
    pub fn new(inner: Arc<dyn SessionLog>, agent: impl Into<String>) -> Self {
        TaggedLog {
            inner,
            agent: agent.into(),
        }
    }
}

impl SessionLog for TaggedLog {
    fn append(&self, mut e: LogEntry) -> Result<(), LogError> {
        e.agent = Some(self.agent.clone());
        self.inner.append(e)
    }

    fn entries(&self, kind: Option<LogKind>, limit: usize) -> Vec<LogEntry> {
        self.inner.entries(kind, limit)
    }

    fn search(&self, query: &str, kind: Option<LogKind>, limit: usize) -> Vec<LogEntry> {
        self.inner.search(query, kind, limit)
    }

    fn last_step(&self) -> u64 {
        self.inner.last_step()
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
            agent: None,
        }
    }

    #[test]
    fn append_and_entries_kind_filter() {
        let log = MemoryLog::new();
        log.append(entry(1, LogKind::User, json!({"text": "hi"})))
            .unwrap();
        log.append(entry(1, LogKind::System, json!({"text": "note"})))
            .unwrap();
        log.append(entry(2, LogKind::User, json!({"text": "there"})))
            .unwrap();

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
        log.append(entry(
            1,
            LogKind::ToolOutput,
            json!({"text": "The Quick Brown Fox"}),
        ))
        .unwrap();
        log.append(entry(
            2,
            LogKind::ToolOutput,
            json!({"text": "nothing relevant"}),
        ))
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

    #[test]
    fn tagged_log_stamps_agent_on_append_but_not_on_direct_inner_entries() {
        let inner: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
        inner
            .append(entry(1, LogKind::User, json!({"text": "parent"})))
            .unwrap();

        let tagged = TaggedLog::new(Arc::clone(&inner), "implement/ui-review#3");
        tagged
            .append(entry(2, LogKind::User, json!({"text": "child"})))
            .unwrap();

        let all = inner.entries(None, 10);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].agent, None);
        assert_eq!(all[1].agent, Some("implement/ui-review#3".to_string()));

        // Reads through the tagged wrapper see the same (unfiltered) entries.
        assert_eq!(tagged.entries(None, 10).len(), 2);
        assert_eq!(tagged.last_step(), 2);
    }

    #[test]
    fn read_jsonl_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.jsonl");
        let err = read_jsonl(&path).unwrap_err();
        assert!(matches!(err, LogError::Io(_)));
    }

    #[test]
    fn read_jsonl_parses_lines_and_skips_blanks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let line1 = serde_json::to_string(&entry(1, LogKind::User, json!({"text": "hi"}))).unwrap();
        let line2 =
            serde_json::to_string(&entry(2, LogKind::System, json!({"event": "note"}))).unwrap();
        std::fs::write(&path, format!("{line1}\n\n{line2}\n")).unwrap();

        let entries = read_jsonl(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].step, 1);
        assert_eq!(entries[1].step, 2);
        assert_eq!(entries[1].kind, LogKind::System);
    }

    #[test]
    fn read_jsonl_malformed_line_errors_with_line_number() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let line1 = serde_json::to_string(&entry(1, LogKind::User, json!({"text": "hi"}))).unwrap();
        std::fs::write(&path, format!("{line1}\nnot json\n")).unwrap();

        let err = read_jsonl(&path).unwrap_err();
        match err {
            LogError::Backend(msg) => assert!(msg.contains("line 2"), "message was: {msg}"),
            other => panic!("expected Backend error, got {other:?}"),
        }
    }
}
