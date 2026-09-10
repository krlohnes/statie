//! Integration tests for `ModelFixer`, against the same fake-TCP-server
//! harness `thinker.rs` uses.

use std::sync::Arc;
use std::time::Duration;

use statie_core::{FixError, FixInput, Fixer};
use statie_models::{ModelFixer, OllamaConfig, OllamaFormat, OllamaModel};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

mod common;
use common::{read_request_body, write_ndjson_response};

fn cfg_for(addr: std::net::SocketAddr) -> OllamaConfig {
    OllamaConfig {
        base_url: format!("http://{addr}"),
        model: "test-model".to_string(),
        think: true,
        num_ctx: 32768,
        options: serde_json::json!({}),
        timeout_secs: 30,
        // Deliberately Schema: the fixer must send its own `format: json`
        // regardless of this setting (it uses `chat_json_with`, not
        // `step`).
        format: OllamaFormat::Schema,
        schema_constrained: None,
        retries_on_5xx: 0,
        retry_delay_secs: 0,
    }
}

fn candidate_line(old: &str, new: &str) -> String {
    serde_json::json!([{
        "path": "src/lib.rs",
        "old": old,
        "new": new,
        "rationale": "test rationale",
    }])
    .to_string()
}

/// Spawns a server that accepts `k` connections in sequence, each replying
/// with the corresponding NDJSON lines. Returns the join handle carrying the
/// captured request bodies in order.
fn spawn_multi_reply_server(
    listener: TcpListener,
    replies: Vec<Vec<String>>,
) -> tokio::task::JoinHandle<Vec<String>> {
    tokio::spawn(async move {
        let mut bodies = Vec::new();
        for lines in replies {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let body = read_request_body(&mut stream).await;
            bodies.push(body);
            let line_refs: Vec<&str> = lines.iter().map(String::as_str).collect();
            write_ndjson_response(&mut stream, &line_refs).await;
        }
        bodies
    })
}

#[tokio::test]
async fn model_fixer_makes_k_requests_with_distinct_seeds_and_json_format() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let replies = vec![
        vec![
            format!(
                r#"{{"message":{{"content":{}}},"done":false}}"#,
                serde_json::Value::String(candidate_line("foo1", "bar1"))
            ),
            r#"{"message":{"content":""},"done":true,"done_reason":"stop","prompt_eval_count":50,"eval_count":10}"#.to_string(),
        ],
        vec![
            format!(
                r#"{{"message":{{"content":{}}},"done":false}}"#,
                serde_json::Value::String(candidate_line("foo2", "bar2"))
            ),
            r#"{"message":{"content":""},"done":true,"done_reason":"stop","prompt_eval_count":50,"eval_count":10}"#.to_string(),
        ],
        vec![
            format!(
                r#"{{"message":{{"content":{}}},"done":false}}"#,
                serde_json::Value::String(candidate_line("foo3", "bar3"))
            ),
            r#"{"message":{"content":""},"done":true,"done_reason":"stop","prompt_eval_count":50,"eval_count":10}"#.to_string(),
        ],
    ];

    let server = spawn_multi_reply_server(listener, replies);

    let model = Arc::new(OllamaModel::new(cfg_for(addr)).expect("build model"));
    let fixer = ModelFixer::new(model, Duration::from_secs(5)).with_base_seed(100);

    let out = fixer
        .propose(
            FixInput {
                system: "sys".to_string(),
                user: "user prompt".to_string(),
                failure: "assertion failed".to_string(),
                k: 3,
            },
            CancellationToken::new(),
        )
        .await
        .expect("propose succeeds");

    assert_eq!(out.candidates.len(), 3);
    let olds: Vec<&str> = out.candidates.iter().map(|c| c.old.as_str()).collect();
    assert!(olds.contains(&"foo1"));
    assert!(olds.contains(&"foo2"));
    assert!(olds.contains(&"foo3"));

    let bodies = server.await.expect("server task");
    assert_eq!(bodies.len(), 3, "must make exactly k requests");

    let mut seeds = Vec::new();
    for body_text in &bodies {
        let body: serde_json::Value = serde_json::from_str(body_text).expect("valid json body");
        assert_eq!(
            body["think"],
            serde_json::json!(false),
            "thinking must be off"
        );
        assert_eq!(
            body["format"],
            serde_json::json!("json"),
            "must ask for JSON format"
        );
        assert_eq!(body["options"]["temperature"], serde_json::json!(0.7));
        assert_eq!(body["options"]["num_predict"], serde_json::json!(1200));

        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(
            messages[0]["content"],
            serde_json::json!(statie_core::FIX_SYSTEM_PROMPT),
            "the proposer sends its own system prompt, never the worker's"
        );
        let user_content = messages[1]["content"].as_str().expect("user content");
        assert!(user_content.contains("user prompt"));
        assert!(user_content.contains("Failing test output:"));
        assert!(user_content.contains("assertion failed"));
        assert!(user_content.contains("Propose exactly 1 candidate fix"));

        let seed = body["options"]["seed"]
            .as_u64()
            .expect("seed must be present");
        seeds.push(seed);
    }
    seeds.sort_unstable();
    assert_eq!(
        seeds,
        vec![100, 101, 102],
        "seeds must be distinct, base + i"
    );
}

#[tokio::test]
async fn model_fixer_returns_union_of_parsed_candidates() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let replies = vec![
        vec![
            format!(
                r#"{{"message":{{"content":{}}},"done":false}}"#,
                serde_json::Value::String(candidate_line("alpha", "beta"))
            ),
            r#"{"message":{"content":""},"done":true,"done_reason":"stop"}"#.to_string(),
        ],
        vec![
            format!(
                r#"{{"message":{{"content":{}}},"done":false}}"#,
                serde_json::Value::String(candidate_line("gamma", "delta"))
            ),
            r#"{"message":{"content":""},"done":true,"done_reason":"stop"}"#.to_string(),
        ],
    ];
    let server = spawn_multi_reply_server(listener, replies);

    let model = Arc::new(OllamaModel::new(cfg_for(addr)).expect("build model"));
    let fixer = ModelFixer::new(model, Duration::from_secs(5));

    let out = fixer
        .propose(
            FixInput {
                system: "sys".to_string(),
                user: "user".to_string(),
                failure: "fail".to_string(),
                k: 2,
            },
            CancellationToken::new(),
        )
        .await
        .expect("propose succeeds");

    assert_eq!(out.candidates.len(), 2);
    let olds: std::collections::HashSet<&str> =
        out.candidates.iter().map(|c| c.old.as_str()).collect();
    assert!(olds.contains("alpha"));
    assert!(olds.contains("gamma"));

    server.await.expect("server task");
}

/// One of the k calls returns garbage (unparseable / no JSON array), the
/// other returns a valid candidate: result must have exactly one candidate
/// and no error.
#[tokio::test]
async fn model_fixer_one_garbage_one_valid_yields_single_candidate_no_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let replies = vec![
        vec![
            r#"{"message":{"content":"not json at all, just prose"},"done":false}"#.to_string(),
            r#"{"message":{"content":""},"done":true,"done_reason":"stop"}"#.to_string(),
        ],
        vec![
            format!(
                r#"{{"message":{{"content":{}}},"done":false}}"#,
                serde_json::Value::String(candidate_line("real_old", "real_new"))
            ),
            r#"{"message":{"content":""},"done":true,"done_reason":"stop"}"#.to_string(),
        ],
    ];
    let server = spawn_multi_reply_server(listener, replies);

    let model = Arc::new(OllamaModel::new(cfg_for(addr)).expect("build model"));
    let fixer = ModelFixer::new(model, Duration::from_secs(5));

    let out = fixer
        .propose(
            FixInput {
                system: "sys".to_string(),
                user: "user".to_string(),
                failure: "fail".to_string(),
                k: 2,
            },
            CancellationToken::new(),
        )
        .await
        .expect("propose succeeds despite one garbage reply");

    assert_eq!(out.candidates.len(), 1);
    assert_eq!(out.candidates[0].old, "real_old");
    assert_eq!(out.candidates[0].new, "real_new");

    server.await.expect("server task");
}

/// Every call fails at the backend (transport) level: `propose` must error
/// with `Backend`, not `Empty`.
#[tokio::test]
async fn model_fixer_all_calls_backend_error_yields_backend_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let replies = vec![
        vec![r#"{"error":"model not found"}"#.to_string()],
        vec![r#"{"error":"model not found"}"#.to_string()],
    ];
    let server = spawn_multi_reply_server(listener, replies);

    let model = Arc::new(OllamaModel::new(cfg_for(addr)).expect("build model"));
    let fixer = ModelFixer::new(model, Duration::from_secs(5));

    let err = fixer
        .propose(
            FixInput {
                system: "sys".to_string(),
                user: "user".to_string(),
                failure: "fail".to_string(),
                k: 2,
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("all calls failing should error");

    match err {
        FixError::Backend(msg) => assert!(msg.contains("model not found"), "msg = {msg}"),
        other => panic!("expected Backend, got {other:?}"),
    }

    server.await.expect("server task");
}

/// Every call succeeds at the transport level but produces no usable
/// candidates (empty content): `propose` must error with `Empty`.
#[tokio::test]
async fn model_fixer_all_empty_content_yields_empty_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let replies = vec![
        vec![r#"{"message":{"content":""},"done":true,"done_reason":"stop"}"#.to_string()],
        vec![r#"{"message":{"content":""},"done":true,"done_reason":"stop"}"#.to_string()],
    ];
    let server = spawn_multi_reply_server(listener, replies);

    let model = Arc::new(OllamaModel::new(cfg_for(addr)).expect("build model"));
    let fixer = ModelFixer::new(model, Duration::from_secs(5));

    let err = fixer
        .propose(
            FixInput {
                system: "sys".to_string(),
                user: "user".to_string(),
                failure: "fail".to_string(),
                k: 2,
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("no candidates should error");

    assert!(matches!(err, FixError::Empty), "got {err:?}");

    server.await.expect("server task");
}
