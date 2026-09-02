//! `JsonlLog`: an append-only, JSONL-backed `SessionLog` with a write-time
//! BM25 index for `search`.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

use statie_core::{Bm25Index, LogEntry, LogError, LogKind, SessionLog};

struct Inner {
    file: BufWriter<File>,
    entries: Vec<LogEntry>,
    index: Bm25Index,
}

/// An append-only `SessionLog` backed by a JSONL file on disk, with an
/// in-memory BM25 index rebuilt on open and kept live as entries are
/// appended.
pub struct JsonlLog {
    path: PathBuf,
    inner: Mutex<Inner>,
}

/// Concatenate all string leaves of a JSON value (walking objects/arrays)
/// into a single searchable text blob. Object keys are not indexed, only
/// string values.
fn searchable_text(value: &Value) -> String {
    let mut out = String::new();
    collect_strings(value, &mut out);
    out
}

fn collect_strings(value: &Value, out: &mut String) {
    match value {
        Value::String(s) => {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(s);
        }
        Value::Array(items) => {
            for item in items {
                collect_strings(item, out);
            }
        }
        Value::Object(map) => {
            for v in map.values() {
                collect_strings(v, out);
            }
        }
        _ => {}
    }
}

impl JsonlLog {
    /// Open (creating if absent) the JSONL log at `path`. Parent directories
    /// are created as needed. Existing content is loaded into memory and
    /// indexed; a malformed line is a `LogError::Backend` naming the line
    /// number. The file is left open for append.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LogError> {
        let path = path.as_ref().to_path_buf();

        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|e| LogError::Io(e.to_string()))?;
        }

        let mut entries = Vec::new();
        let mut index = Bm25Index::new();

        if path.exists() {
            let file = File::open(&path).map_err(|e| LogError::Io(e.to_string()))?;
            let reader = BufReader::new(file);
            for (idx, line) in reader.lines().enumerate() {
                let line = line.map_err(|e| LogError::Io(e.to_string()))?;
                if line.trim().is_empty() {
                    continue;
                }
                let entry: LogEntry = serde_json::from_str(&line).map_err(|e| {
                    LogError::Backend(format!("malformed log line {}: {}", idx + 1, e))
                })?;
                let doc_id = entries.len() as u64;
                index.add(doc_id, &searchable_text(&entry.payload));
                entries.push(entry);
            }
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| LogError::Io(e.to_string()))?;

        Ok(JsonlLog {
            path,
            inner: Mutex::new(Inner {
                file: BufWriter::new(file),
                entries,
                index,
            }),
        })
    }

    pub fn len(&self) -> usize {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl SessionLog for JsonlLog {
    fn append(&self, e: LogEntry) -> Result<(), LogError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| LogError::Backend("lock poisoned".to_string()))?;

        let line = serde_json::to_string(&e).map_err(|e| LogError::Backend(e.to_string()))?;
        writeln!(guard.file, "{line}").map_err(|e| LogError::Io(e.to_string()))?;
        guard
            .file
            .flush()
            .map_err(|e| LogError::Io(e.to_string()))?;

        let doc_id = guard.entries.len() as u64;
        let text = searchable_text(&e.payload);
        guard.entries.push(e);
        guard.index.add(doc_id, &text);

        Ok(())
    }

    fn entries(&self, kind: Option<LogKind>, limit: usize) -> Vec<LogEntry> {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let filtered: Vec<&LogEntry> = guard
            .entries
            .iter()
            .filter(|e| kind.is_none_or(|k| e.kind == k))
            .collect();
        let start = filtered.len().saturating_sub(limit);
        filtered[start..].iter().map(|e| (*e).clone()).collect()
    }

    fn search(&self, query: &str, kind: Option<LogKind>, limit: usize) -> Vec<LogEntry> {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };

        let candidate_count = limit * 8 + 32;
        let hits = guard.index.search(query, candidate_count);

        let mut results = Vec::with_capacity(limit);
        for (doc_id, _score) in hits {
            let Some(entry) = guard.entries.get(doc_id as usize) else {
                continue;
            };
            if kind.is_some_and(|k| entry.kind != k) {
                continue;
            }
            results.push(entry.clone());
            if results.len() >= limit {
                break;
            }
        }
        results
    }

    fn last_step(&self) -> u64 {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.entries.iter().map(|e| e.step).max().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn entry(step: u64, kind: LogKind, payload: Value) -> LogEntry {
        LogEntry {
            step,
            kind,
            ts: "2026-09-01T00:00:00Z".to_string(),
            payload,
        }
    }

    fn entries_equal(a: &LogEntry, b: &LogEntry) -> bool {
        a.step == b.step && a.kind == b.kind && a.ts == b.ts && a.payload == b.payload
    }

    #[test]
    fn append_writes_one_json_line_per_entry_that_round_trips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let log = JsonlLog::open(&path).unwrap();

        let e1 = entry(1, LogKind::User, json!({"text": "hi"}));
        let e2 = entry(2, LogKind::Reasoning, json!({"text": "thinking"}));
        let e3 = entry(3, LogKind::System, json!({"text": "note"}));

        log.append(e1.clone()).unwrap();
        log.append(e2.clone()).unwrap();
        log.append(e3.clone()).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 3);

        let parsed: Vec<LogEntry> = lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert!(entries_equal(&parsed[0], &e1));
        assert!(entries_equal(&parsed[1], &e2));
        assert!(entries_equal(&parsed[2], &e3));
    }

    #[test]
    fn reopen_after_drop_preserves_entries_step_and_search() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");

        {
            let log = JsonlLog::open(&path).unwrap();
            log.append(entry(1, LogKind::User, json!({"text": "hello there"})))
                .unwrap();
            log.append(entry(
                2,
                LogKind::ToolOutput,
                json!({"output": "cargo test failed: assertion in parser"}),
            ))
            .unwrap();
            log.append(entry(3, LogKind::System, json!({"text": "note"})))
                .unwrap();
        }

        let reopened = JsonlLog::open(&path).unwrap();
        let all = reopened.entries(None, 10);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].step, 1);
        assert_eq!(all[1].step, 2);
        assert_eq!(all[2].step, 3);
        assert_eq!(reopened.last_step(), 3);

        let hits = reopened.search("parser assertion", None, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].step, 2);
    }

    fn seed_search_data(log: &JsonlLog) {
        log.append(entry(
            1,
            LogKind::ToolOutput,
            json!({"output": "cargo test failed: assertion in parser"}),
        ))
        .unwrap();
        log.append(entry(
            2,
            LogKind::ToolOutput,
            json!({"output": "wrote file src/main.rs"}),
        ))
        .unwrap();
        log.append(entry(3, LogKind::User, json!({"text": "user said hello"})))
            .unwrap();
    }

    #[test]
    fn search_ranks_best_match_first_and_excludes_unrelated() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let log = JsonlLog::open(&path).unwrap();
        seed_search_data(&log);

        let hits = log.search("parser assertion", None, 5);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].step, 1);
        assert!(hits.iter().all(|e| e.step != 3));
    }

    #[test]
    fn kind_filter_restricts_search_results() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let log = JsonlLog::open(&path).unwrap();
        seed_search_data(&log);

        let user_hits = log.search("parser", Some(LogKind::User), 5);
        assert!(user_hits.is_empty());

        let tool_hits = log.search("parser", Some(LogKind::ToolOutput), 5);
        assert_eq!(tool_hits.len(), 1);
        assert_eq!(tool_hits[0].step, 1);
    }

    #[test]
    fn malformed_line_on_open_errors_with_line_number() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");

        let good = serde_json::to_string(&entry(1, LogKind::User, json!({"text": "ok"}))).unwrap();
        fs::write(&path, format!("{good}\nnot valid json\n")).unwrap();

        match JsonlLog::open(&path) {
            Err(LogError::Backend(msg)) => {
                assert!(
                    msg.contains('2'),
                    "expected line number 2 in message: {msg}"
                );
            }
            Err(other) => panic!("expected LogError::Backend, got {other:?}"),
            Ok(_) => panic!("expected an error opening a malformed log"),
        }
    }

    #[test]
    fn entries_with_kind_and_limit_returns_newest_of_that_kind() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let log = JsonlLog::open(&path).unwrap();

        log.append(entry(1, LogKind::ToolOutput, json!({"output": "first"})))
            .unwrap();
        log.append(entry(2, LogKind::User, json!({"text": "middle"})))
            .unwrap();
        log.append(entry(3, LogKind::ToolOutput, json!({"output": "newest"})))
            .unwrap();

        let newest_tool = log.entries(Some(LogKind::ToolOutput), 1);
        assert_eq!(newest_tool.len(), 1);
        assert_eq!(newest_tool[0].step, 3);
    }

    #[test]
    fn open_creates_parent_dirs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("dir").join("session.jsonl");
        let log = JsonlLog::open(&path).unwrap();
        assert!(log.is_empty());
        assert_eq!(log.path(), path.as_path());
    }

    #[test]
    fn blank_lines_in_existing_file_are_skipped() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let good = serde_json::to_string(&entry(1, LogKind::User, json!({"text": "ok"}))).unwrap();
        fs::write(&path, format!("{good}\n\n\n")).unwrap();

        let log = JsonlLog::open(&path).unwrap();
        assert_eq!(log.len(), 1);
    }
}
