//! Shared fake-TCP-server test helpers for model backend integration tests.

use std::sync::Arc;

use statie_core::{
    AgentState, BoundedPrompt, Budgets, Capabilities, HarnessState, HarnessView,
    HeuristicTokenizer, PromptBuilder, Schema, SkillSpec, StepOutput, StepSink, SystemPrompt,
    ToolSet, Version,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[allow(dead_code)]
pub fn budgets() -> Budgets {
    Budgets {
        total: 1_000_000,
        observation: 1_000_000,
        augmentation: 1_000_000,
        notes: 1_000_000,
        working_set: 6000,
    }
}

#[allow(dead_code)]
pub fn schema() -> Schema {
    Schema::of::<StepOutput<serde_json::Value>>()
}

#[allow(dead_code)]
pub fn build_prompt(version: Version) -> BoundedPrompt {
    let tok = HeuristicTokenizer;
    let caps = Capabilities {
        native_thinking: true,
        schema_constrained: true,
        prefix_cache: true,
    };
    let sys = SystemPrompt::for_harness(
        &caps,
        "json-merge-patch (RFC 7386)",
        "merge patch",
        None,
        &[],
    );
    let spec = Arc::new(SkillSpec::new("toy", "instructions"));
    let tools = ToolSet::new().list_for(&[]);
    let agent = serde_json::json!({"x": 1});
    let agent_state = AgentState::new(&agent);
    let harness = HarnessState {
        inbox: vec![],
        active_tools: vec![],
        budgets: budgets(),
        activation: "toy".to_string(),
        step: 0,
        last_overflow: None,
        pinned: None,
        green_step: None,
        green_filtered: false,
        tool_calls: 0,
        last_test_cmd: None,
        edits_since_test: 0,
        last_think_step: None,
        repeated_failure_step: None,
        try_fixes_available: false,
        last_try_fixes_step: None,
        consecutive_non_question_asks: 0,
        last_failure_summary: None,
        last_skip: None,
    };
    let harness_view = HarnessView::new(&harness);

    PromptBuilder::new(&tok, budgets(), version)
        .system(&sys)
        .spec(&spec)
        .tools(&tools)
        .agent(&agent_state)
        .harness(&harness_view)
        .build()
        .expect("prompt within budget")
}

#[allow(dead_code)]
pub struct CollectingSink {
    pub reasoning: String,
    pub content: String,
}

#[allow(dead_code)]
impl CollectingSink {
    pub fn new() -> Self {
        CollectingSink {
            reasoning: String::new(),
            content: String::new(),
        }
    }
}

impl StepSink for CollectingSink {
    fn reasoning_delta(&mut self, s: &str) {
        self.reasoning.push_str(s);
    }
    fn content_delta(&mut self, s: &str) {
        self.content.push_str(s);
    }
}

/// Reads one HTTP request off `stream` up to and including its body (using
/// `Content-Length`), returning the captured JSON body as a string and the
/// raw header block (for header assertions, e.g. `Authorization`).
#[allow(dead_code)]
pub async fn read_request(stream: &mut tokio::net::TcpStream) -> (String, String) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end;
    let content_length;
    let header_text;
    loop {
        let n = stream.read(&mut chunk).await.expect("read request");
        assert!(n > 0, "connection closed before headers complete");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(idx) = find_subslice(&buf, b"\r\n\r\n") {
            header_end = idx + 4;
            header_text = String::from_utf8_lossy(&buf[..idx]).to_string();
            content_length = header_text
                .lines()
                .find_map(|line| {
                    let lower = line.to_ascii_lowercase();
                    lower
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().expect("valid content-length"))
                })
                .unwrap_or(0);
            break;
        }
    }

    while buf.len() < header_end + content_length {
        let n = stream.read(&mut chunk).await.expect("read body");
        assert!(n > 0, "connection closed before body complete");
        buf.extend_from_slice(&chunk[..n]);
    }

    let body = String::from_utf8(buf[header_end..header_end + content_length].to_vec())
        .expect("utf8 body");
    (body, header_text)
}

/// Backward-compatible helper that discards headers, for callers that only
/// need the body.
#[allow(dead_code)]
pub async fn read_request_body(stream: &mut tokio::net::TcpStream) -> String {
    read_request(stream).await.0
}

#[allow(dead_code)]
pub fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[allow(dead_code)]
pub async fn write_ndjson_response(stream: &mut tokio::net::TcpStream, lines: &[&str]) {
    let body = lines.join("\n") + "\n";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    stream
        .write_all(response.as_bytes())
        .await
        .expect("write response");
    stream.shutdown().await.expect("shutdown");
}

/// Writes an SSE response body from `lines`, each wrapped as `data: <line>`,
/// followed by `data: [DONE]`.
#[allow(dead_code)]
pub async fn write_sse_response(stream: &mut tokio::net::TcpStream, data_lines: &[&str]) {
    let mut body = String::new();
    for line in data_lines {
        body.push_str("data: ");
        body.push_str(line);
        body.push('\n');
    }
    body.push_str("data: [DONE]\n");
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    stream
        .write_all(response.as_bytes())
        .await
        .expect("write response");
    stream.shutdown().await.expect("shutdown");
}

#[allow(dead_code)]
pub async fn write_status_response(
    stream: &mut tokio::net::TcpStream,
    status_line: &str,
    body: &str,
) {
    let response = format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: text/plain\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    stream
        .write_all(response.as_bytes())
        .await
        .expect("write response");
    stream.shutdown().await.expect("shutdown");
}
