use std::sync::Arc;

use serde::Serialize;
use serde_json::json;

use statie_augment::SessionLogBm25;
use statie_core::{
    Augmenter, HeuristicTokenizer, LogEntry, LogKind, MemoryLog, SessionLog, TokenBudget,
};

#[derive(Serialize)]
struct S {
    task: String,
}

fn entry(step: u64, kind: LogKind, payload: serde_json::Value) -> LogEntry {
    LogEntry {
        step,
        kind,
        ts: "2026-09-01T00:00:00Z".to_string(),
        payload,
        agent: None,
    }
}

fn make_log() -> Arc<MemoryLog> {
    let log = Arc::new(MemoryLog::new());
    log.append(entry(
        1,
        LogKind::ToolOutput,
        json!({"output": "cargo test failed in parser"}),
    ))
    .unwrap();
    log.append(entry(
        2,
        LogKind::Reasoning,
        json!({"text": "parser parser parser parser"}),
    ))
    .unwrap();
    log.append(entry(
        3,
        LogKind::User,
        json!({"text": "please fix the parser"}),
    ))
    .unwrap();
    log
}

#[tokio::test]
async fn augment_includes_tool_output_and_user_but_never_reasoning() {
    let log = make_log();
    let augmenter = SessionLogBm25::new(log.clone() as Arc<dyn SessionLog>, vec![], 10);

    let state = S {
        task: "parser".to_string(),
    };
    let tok = HeuristicTokenizer;
    let budget = TokenBudget(5000);

    let snippets = augmenter.augment(&state, None, budget, &tok).await;

    assert!(
        snippets.iter().any(|s| s.source.contains("ToolOutput")),
        "expected a ToolOutput snippet: {:?}",
        snippets.iter().map(|s| &s.source).collect::<Vec<_>>()
    );
    assert!(
        snippets.iter().any(|s| s.source.contains("User")),
        "expected a User snippet: {:?}",
        snippets.iter().map(|s| &s.source).collect::<Vec<_>>()
    );
    assert!(
        !snippets
            .iter()
            .any(|s| s.text.contains("parser parser parser")),
        "no snippet should contain the reasoning text: {:?}",
        snippets.iter().map(|s| &s.text).collect::<Vec<_>>()
    );
    assert!(
        !snippets.iter().any(|s| s.source.contains("Reasoning")),
        "no snippet should be sourced from a Reasoning entry"
    );
}

#[tokio::test]
async fn constructing_with_reasoning_in_kinds_still_yields_no_reasoning_snippets() {
    let log = make_log();
    let augmenter = SessionLogBm25::new(
        log.clone() as Arc<dyn SessionLog>,
        vec![LogKind::Reasoning],
        10,
    );

    let state = S {
        task: "parser".to_string(),
    };
    let tok = HeuristicTokenizer;
    let budget = TokenBudget(5000);

    let snippets = augmenter.augment(&state, None, budget, &tok).await;

    assert!(
        snippets.is_empty(),
        "kinds=[Reasoning] should be stripped to empty, yielding no snippets: {:?}",
        snippets.iter().map(|s| &s.source).collect::<Vec<_>>()
    );
    assert!(
        !snippets
            .iter()
            .any(|s| s.text.contains("parser parser parser"))
    );
}
