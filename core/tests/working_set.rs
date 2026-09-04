//! Unit-style tests for `WorkingSet::from_log` (statie-spec.md "10. Working
//! set (recent observations)"), built against a hand-made `MemoryLog`.

use serde_json::{Value, json};

use statie_core::{
    HeuristicTokenizer, LogEntry, LogKind, MemoryLog, SessionLog, Tokenizer, WorkingSet,
    WorkingSetConfig,
};

fn action(step: u64, name: &str, args: Value) -> LogEntry {
    LogEntry {
        step,
        kind: LogKind::Action,
        ts: "2026-01-01T00:00:00Z".to_string(),
        payload: json!({"type": "tool", "name": name, "args": args}),
        agent: None,
    }
}

fn tool_output(step: u64, name: &str, output: &str) -> LogEntry {
    LogEntry {
        step,
        kind: LogKind::ToolOutput,
        ts: "2026-01-01T00:00:00Z".to_string(),
        payload: json!({"name": name, "output": output, "truncated": false}),
        agent: None,
    }
}

#[test]
fn empty_log_returns_none() {
    let log = MemoryLog::new();
    let tok = HeuristicTokenizer;
    let cfg = WorkingSetConfig::default();
    assert!(WorkingSet::from_log(&log, &tok, 6000, &cfg, 1).is_none());
}

#[test]
fn zero_config_returns_none() {
    let log = MemoryLog::new();
    log.append(action(1, "read_file", json!({"path": "a.rs"})))
        .unwrap();
    log.append(tool_output(1, "read_file", "1: fn a() {}"))
        .unwrap();
    let tok = HeuristicTokenizer;
    let cfg = WorkingSetConfig {
        max_windows: 0,
        recent_calls: 0,
    };
    assert!(WorkingSet::from_log(&log, &tok, 6000, &cfg, 2).is_none());
}

#[test]
fn recent_calls_rendered_oldest_first_with_arrows() {
    let log = MemoryLog::new();
    log.append(action(1, "read_file", json!({"path": "a.rs"})))
        .unwrap();
    log.append(tool_output(1, "read_file", "1: fn a() {}"))
        .unwrap();
    log.append(action(2, "search", json!({"pattern": "fn"})))
        .unwrap();
    log.append(tool_output(2, "search", "a.rs:1: fn a() {}"))
        .unwrap();
    let tok = HeuristicTokenizer;
    let cfg = WorkingSetConfig {
        max_windows: 0,
        recent_calls: 6,
    };
    let ws = WorkingSet::from_log(&log, &tok, 6000, &cfg, 3).unwrap();
    let text = ws.as_str();
    let idx1 = text.find("step 1: read_file").unwrap();
    let idx2 = text.find("step 2: search").unwrap();
    assert!(idx1 < idx2, "expected step 1 before step 2:\n{text}");
    assert!(text.contains("-> 1: fn a() {}"), "{text}");
    assert!(text.contains("-> a.rs:1: fn a() {}"), "{text}");
}

#[test]
fn tool_error_step_shows_error_arrow() {
    let log = MemoryLog::new();
    log.append(action(1, "shell", json!({"cmd": "cargo build"})))
        .unwrap();
    log.append(LogEntry {
        step: 1,
        kind: LogKind::System,
        ts: "2026-01-01T00:00:00Z".to_string(),
        payload: json!({"event": "tool_error", "name": "shell", "reason": "boom\nmore"}),
        agent: None,
    })
    .unwrap();
    let tok = HeuristicTokenizer;
    let cfg = WorkingSetConfig {
        max_windows: 0,
        recent_calls: 6,
    };
    let ws = WorkingSet::from_log(&log, &tok, 6000, &cfg, 2).unwrap();
    assert!(ws.as_str().contains("-> error: boom"), "{}", ws.as_str());
    assert!(!ws.as_str().contains("-> error: boom\nmore"));
}

#[test]
fn skipped_redundant_step_shows_skipped_arrow() {
    let log = MemoryLog::new();
    log.append(action(1, "read_file", json!({"path": "a.rs"})))
        .unwrap();
    log.append(LogEntry {
        step: 1,
        kind: LogKind::System,
        ts: "2026-01-01T00:00:00Z".to_string(),
        payload: json!({"event": "skipped_redundant", "name": "read_file", "reason": "..."}),
        agent: None,
    })
    .unwrap();
    let tok = HeuristicTokenizer;
    let cfg = WorkingSetConfig {
        max_windows: 0,
        recent_calls: 6,
    };
    let ws = WorkingSet::from_log(&log, &tok, 6000, &cfg, 2).unwrap();
    assert!(
        ws.as_str().contains("-> skipped (redundant)"),
        "{}",
        ws.as_str()
    );
}

#[test]
fn recent_call_line_capped_at_160_chars() {
    let log = MemoryLog::new();
    let long_output = "x".repeat(400);
    log.append(action(1, "read_file", json!({"path": "a.rs"})))
        .unwrap();
    log.append(tool_output(1, "read_file", &long_output))
        .unwrap();
    let tok = HeuristicTokenizer;
    let cfg = WorkingSetConfig {
        max_windows: 0,
        recent_calls: 6,
    };
    let ws = WorkingSet::from_log(&log, &tok, 60_000, &cfg, 2).unwrap();
    let line = ws.as_str().lines().nth(1).unwrap();
    assert!(line.chars().count() <= 160, "line too long: {line}");
    assert!(line.ends_with('…'));
}

#[test]
fn windows_rendered_newest_first_and_exclude_this_steps_observation() {
    let log = MemoryLog::new();
    log.append(action(1, "read_file", json!({"path": "a.rs"})))
        .unwrap();
    log.append(tool_output(1, "read_file", "1: fn a() {}"))
        .unwrap();
    log.append(action(2, "read_file", json!({"path": "b.rs"})))
        .unwrap();
    log.append(tool_output(2, "read_file", "1: fn b() {}"))
        .unwrap();
    // Step 3's ToolOutput is "this step's observation" -- must be
    // excluded from the windows even though it's read_file.
    log.append(action(3, "read_file", json!({"path": "c.rs"})))
        .unwrap();
    log.append(tool_output(3, "read_file", "1: fn c() {}"))
        .unwrap();

    let tok = HeuristicTokenizer;
    let cfg = WorkingSetConfig {
        max_windows: 4,
        recent_calls: 0,
    };
    let ws = WorkingSet::from_log(&log, &tok, 6000, &cfg, 4).unwrap();
    let text = ws.as_str();
    assert!(
        !text.contains("c.rs"),
        "this step's own read must not appear in windows:\n{text}"
    );
    let idx_b = text.find("b.rs").unwrap();
    let idx_a = text.find("a.rs").unwrap();
    assert!(
        idx_b < idx_a,
        "expected b.rs (step 2) before a.rs (step 1), newest first:\n{text}"
    );
}

#[test]
fn later_identical_call_replaces_earlier_window() {
    let log = MemoryLog::new();
    log.append(action(
        1,
        "read_file",
        json!({"path": "a.rs", "offset": 1, "limit": 5}),
    ))
    .unwrap();
    log.append(tool_output(1, "read_file", "1: fn a() {}"))
        .unwrap();
    log.append(action(
        2,
        "read_file",
        json!({"path": "a.rs", "offset": 1, "limit": 5}),
    ))
    .unwrap();
    log.append(tool_output(2, "read_file", "1: fn a() { /* updated */ }"))
        .unwrap();
    // A third step so step 2's output is not "this step's own".
    log.append(action(3, "search", json!({"pattern": "fn"})))
        .unwrap();
    log.append(tool_output(3, "search", "no matches")).unwrap();

    let tok = HeuristicTokenizer;
    let cfg = WorkingSetConfig {
        max_windows: 4,
        recent_calls: 0,
    };
    let ws = WorkingSet::from_log(&log, &tok, 6000, &cfg, 4).unwrap();
    let text = ws.as_str();
    assert!(text.contains("updated"), "{text}");
    assert!(text.matches("--- read_file a.rs").count() == 1, "{text}");
}

#[test]
fn window_dropped_after_later_edit() {
    let log = MemoryLog::new();
    log.append(action(1, "read_file", json!({"path": "a.rs"})))
        .unwrap();
    log.append(tool_output(1, "read_file", "1: fn a() {}"))
        .unwrap();
    log.append(action(
        2,
        "edit_file",
        json!({"path": "a.rs", "old": "a", "new": "b"}),
    ))
    .unwrap();
    // A later read of a different file, so the working set has
    // something to show besides the dropped a.rs window.
    log.append(action(3, "read_file", json!({"path": "y.rs"})))
        .unwrap();
    log.append(tool_output(3, "read_file", "1: fn y() {}"))
        .unwrap();
    // And a further step so step 3's read isn't "this step's own"
    // (excluded as the newest ToolOutput) either.
    log.append(action(4, "read_file", json!({"path": "z.rs"})))
        .unwrap();
    log.append(tool_output(4, "read_file", "1: fn z() {}"))
        .unwrap();

    let tok = HeuristicTokenizer;
    let cfg = WorkingSetConfig {
        max_windows: 4,
        recent_calls: 0,
    };
    let ws = WorkingSet::from_log(&log, &tok, 6000, &cfg, 5).unwrap();
    let text = ws.as_str();
    assert!(
        !text.contains("a.rs"),
        "edited path's window must be dropped:\n{text}"
    );
    assert!(
        text.contains("y.rs"),
        "unedited later window should survive:\n{text}"
    );
}

#[test]
fn max_windows_honoured() {
    let log = MemoryLog::new();
    for i in 1..=5u64 {
        log.append(action(i, "read_file", json!({"path": format!("f{i}.rs")})))
            .unwrap();
        log.append(tool_output(i, "read_file", &format!("1: fn f{i}() {{}}")))
            .unwrap();
    }
    // One more step so step 5's output isn't excluded as "this step's".
    log.append(action(6, "search", json!({"pattern": "fn"})))
        .unwrap();
    log.append(tool_output(6, "search", "no matches")).unwrap();

    let tok = HeuristicTokenizer;
    let cfg = WorkingSetConfig {
        max_windows: 2,
        recent_calls: 0,
    };
    let ws = WorkingSet::from_log(&log, &tok, 60_000, &cfg, 7).unwrap();
    let text = ws.as_str();
    let count = text.matches("--- read_file").count();
    assert_eq!(count, 2, "{text}");
    assert!(text.contains("f5.rs"), "{text}");
    assert!(text.contains("f4.rs"), "{text}");
    assert!(!text.contains("f3.rs"), "{text}");
}

#[test]
fn budget_fitting_drops_oldest_window_first() {
    let log = MemoryLog::new();
    let big = "x".repeat(2000);
    log.append(action(1, "read_file", json!({"path": "a.rs"})))
        .unwrap();
    log.append(tool_output(1, "read_file", &big)).unwrap();
    log.append(action(2, "read_file", json!({"path": "b.rs"})))
        .unwrap();
    log.append(tool_output(2, "read_file", &big)).unwrap();
    log.append(action(3, "search", json!({"pattern": "fn"})))
        .unwrap();
    log.append(tool_output(3, "search", "no matches")).unwrap();

    let tok = HeuristicTokenizer;
    let cfg = WorkingSetConfig {
        max_windows: 4,
        recent_calls: 0,
    };
    // Budget large enough for one window but not both.
    let one_window_tokens = tok.count(&big) + 20;
    let ws = WorkingSet::from_log(&log, &tok, one_window_tokens, &cfg, 4).unwrap();
    let text = ws.as_str();
    assert!(
        text.contains("b.rs"),
        "newer window (b.rs) should survive:\n{text}"
    );
    assert!(
        !text.contains("a.rs"),
        "older window (a.rs) should be dropped:\n{text}"
    );
}

#[test]
fn successful_edit_replaces_earlier_read_window() {
    let log = MemoryLog::new();
    log.append(action(1, "read_file", json!({"path": "a.rs"})))
        .unwrap();
    log.append(tool_output(1, "read_file", "1: fn a() {}"))
        .unwrap();
    log.append(action(
        2,
        "edit_file",
        json!({"path": "a.rs", "old": "a", "new": "b"}),
    ))
    .unwrap();
    log.append(tool_output(2, "edit_file", "1: fn b() {}"))
        .unwrap();
    // A further step so step 2's edit output isn't "this step's own".
    log.append(action(3, "search", json!({"pattern": "fn"})))
        .unwrap();
    log.append(tool_output(3, "search", "no matches")).unwrap();

    let tok = HeuristicTokenizer;
    let cfg = WorkingSetConfig {
        max_windows: 4,
        recent_calls: 0,
    };
    let ws = WorkingSet::from_log(&log, &tok, 6000, &cfg, 4).unwrap();
    let text = ws.as_str();
    assert!(
        !text.contains("--- read_file a.rs"),
        "read window on a.rs must be gone:\n{text}"
    );
    assert!(text.contains("--- edit_file a.rs (step 2) ---"), "{text}");
    assert!(text.contains("1: fn b() {}"), "{text}");
}

#[test]
fn only_newest_of_two_successful_edits_survives() {
    let log = MemoryLog::new();
    log.append(action(
        1,
        "edit_file",
        json!({"path": "a.rs", "old": "a", "new": "b"}),
    ))
    .unwrap();
    log.append(tool_output(1, "edit_file", "1: fn b() {}"))
        .unwrap();
    log.append(action(
        2,
        "edit_file",
        json!({"path": "a.rs", "old": "b", "new": "c"}),
    ))
    .unwrap();
    log.append(tool_output(2, "edit_file", "1: fn c() {}"))
        .unwrap();
    // A further step so step 2's edit output isn't "this step's own".
    log.append(action(3, "search", json!({"pattern": "fn"})))
        .unwrap();
    log.append(tool_output(3, "search", "no matches")).unwrap();

    let tok = HeuristicTokenizer;
    let cfg = WorkingSetConfig {
        max_windows: 4,
        recent_calls: 0,
    };
    let ws = WorkingSet::from_log(&log, &tok, 6000, &cfg, 4).unwrap();
    let text = ws.as_str();
    assert_eq!(
        text.matches("--- edit_file a.rs").count(),
        1,
        "only the newest edit window should survive:\n{text}"
    );
    assert!(text.contains("--- edit_file a.rs (step 2) ---"), "{text}");
    assert!(text.contains("1: fn c() {}"), "{text}");
    assert!(!text.contains("1: fn b() {}"), "{text}");
}

#[test]
fn edit_then_later_read_of_same_path_keeps_both_newest_first() {
    let log = MemoryLog::new();
    log.append(action(
        1,
        "edit_file",
        json!({"path": "a.rs", "old": "a", "new": "b"}),
    ))
    .unwrap();
    log.append(tool_output(1, "edit_file", "1: fn b() {}"))
        .unwrap();
    log.append(action(2, "read_file", json!({"path": "a.rs", "offset": 1})))
        .unwrap();
    log.append(tool_output(2, "read_file", "1: fn b() { /* re-read */ }"))
        .unwrap();
    // A further step so step 2's read output isn't "this step's own".
    log.append(action(3, "search", json!({"pattern": "fn"})))
        .unwrap();
    log.append(tool_output(3, "search", "no matches")).unwrap();

    let tok = HeuristicTokenizer;
    let cfg = WorkingSetConfig {
        max_windows: 4,
        recent_calls: 0,
    };
    let ws = WorkingSet::from_log(&log, &tok, 6000, &cfg, 4).unwrap();
    let text = ws.as_str();
    assert!(
        text.contains("--- edit_file a.rs (step 1) ---"),
        "edit window should still be present:\n{text}"
    );
    assert!(
        text.contains("--- read_file a.rs"),
        "later read window should still be present:\n{text}"
    );
    let idx_read = text.find("--- read_file a.rs").unwrap();
    let idx_edit = text.find("--- edit_file a.rs").unwrap();
    assert!(
        idx_read < idx_edit,
        "expected read (step 2) before edit (step 1), newest first:\n{text}"
    );
    assert!(text.contains("re-read"), "{text}");
    assert!(text.contains("1: fn b() {}"), "{text}");
}

#[test]
fn failed_edit_produces_no_window_and_keeps_earlier_read() {
    let log = MemoryLog::new();
    log.append(action(1, "read_file", json!({"path": "a.rs"})))
        .unwrap();
    log.append(tool_output(1, "read_file", "1: fn a() {}"))
        .unwrap();
    log.append(action(
        2,
        "edit_file",
        json!({"path": "a.rs", "old": "nonexistent", "new": "b"}),
    ))
    .unwrap();
    log.append(LogEntry {
        step: 2,
        kind: LogKind::System,
        ts: "2026-01-01T00:00:00Z".to_string(),
        payload: json!({"event": "tool_error", "name": "edit_file", "reason": "no match"}),
        agent: None,
    })
    .unwrap();
    // A further step so nothing at step 2 is "this step's own".
    log.append(action(3, "search", json!({"pattern": "fn"})))
        .unwrap();
    log.append(tool_output(3, "search", "no matches")).unwrap();

    let tok = HeuristicTokenizer;
    let cfg = WorkingSetConfig {
        max_windows: 4,
        recent_calls: 0,
    };
    let ws = WorkingSet::from_log(&log, &tok, 6000, &cfg, 4).unwrap();
    let text = ws.as_str();
    assert!(
        !text.contains("edit_file"),
        "a failed edit must not produce a window:\n{text}"
    );
    assert!(
        text.contains("--- read_file a.rs"),
        "earlier read window on a.rs must survive a failed edit:\n{text}"
    );
    assert!(text.contains("1: fn a() {}"), "{text}");
}

#[test]
fn edit_window_counts_toward_max_windows_and_drops_oldest_read() {
    let log = MemoryLog::new();
    log.append(action(1, "read_file", json!({"path": "old.rs"})))
        .unwrap();
    log.append(tool_output(1, "read_file", "1: fn old() {}"))
        .unwrap();
    log.append(action(2, "read_file", json!({"path": "mid.rs"})))
        .unwrap();
    log.append(tool_output(2, "read_file", "1: fn mid() {}"))
        .unwrap();
    log.append(action(
        3,
        "edit_file",
        json!({"path": "new.rs", "old": "a", "new": "b"}),
    ))
    .unwrap();
    log.append(tool_output(3, "edit_file", "1: fn new() {}"))
        .unwrap();
    // A further step so step 3's edit output isn't "this step's own".
    log.append(action(4, "search", json!({"pattern": "fn"})))
        .unwrap();
    log.append(tool_output(4, "search", "no matches")).unwrap();

    let tok = HeuristicTokenizer;
    let cfg = WorkingSetConfig {
        max_windows: 2,
        recent_calls: 0,
    };
    let ws = WorkingSet::from_log(&log, &tok, 60_000, &cfg, 5).unwrap();
    let text = ws.as_str();
    assert!(text.contains("--- edit_file new.rs (step 3) ---"), "{text}");
    assert!(
        text.contains("--- read_file mid.rs"),
        "newer read window should survive:\n{text}"
    );
    assert!(
        !text.contains("old.rs"),
        "oldest window should be dropped once max_windows is reached:\n{text}"
    );
}

#[test]
fn current_steps_edit_output_not_repeated_as_window() {
    let log = MemoryLog::new();
    log.append(action(1, "read_file", json!({"path": "a.rs"})))
        .unwrap();
    log.append(tool_output(1, "read_file", "1: fn a() {}"))
        .unwrap();
    log.append(action(
        2,
        "edit_file",
        json!({"path": "a.rs", "old": "a", "new": "b"}),
    ))
    .unwrap();
    log.append(tool_output(2, "edit_file", "1: fn b() {}"))
        .unwrap();

    let tok = HeuristicTokenizer;
    let cfg = WorkingSetConfig {
        max_windows: 4,
        recent_calls: 0,
    };
    // current_step = 2: step 2's edit output is this step's own observation
    // and must not be repeated as a window. The earlier read window on
    // a.rs is dropped by the (successful) edit regardless, so no window on
    // a.rs remains at all.
    let ws = WorkingSet::from_log(&log, &tok, 6000, &cfg, 2);
    assert!(
        ws.is_none() || !ws.as_ref().unwrap().as_str().contains("a.rs"),
        "{:?}",
        ws.map(|w| w.as_str().to_string())
    );
}

#[test]
fn single_oversized_window_is_head_truncated_with_marker() {
    let log = MemoryLog::new();
    let big = "x".repeat(5000);
    log.append(action(1, "read_file", json!({"path": "a.rs"})))
        .unwrap();
    log.append(tool_output(1, "read_file", &big)).unwrap();
    log.append(action(2, "search", json!({"pattern": "fn"})))
        .unwrap();
    log.append(tool_output(2, "search", "no matches")).unwrap();

    let tok = HeuristicTokenizer;
    let cfg = WorkingSetConfig {
        max_windows: 1,
        recent_calls: 0,
    };
    let ws = WorkingSet::from_log(&log, &tok, 100, &cfg, 3).unwrap();
    let text = ws.as_str();
    assert!(
        tok.count(text) <= 100,
        "over budget: {} tokens",
        tok.count(text)
    );
    assert!(text.contains("chars omitted"), "{text}");
}
