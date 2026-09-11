use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use statie_core::{Model, ModelError, NullSink, Usage, Version};
use statie_models::{OllamaConfig, OllamaFormat, OllamaModel, strip_thoughts};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use super::common;
use common::{
    CollectingSink, build_prompt, read_request_body, schema, write_ndjson_response,
    write_status_response,
};

fn cfg_for(addr: std::net::SocketAddr) -> OllamaConfig {
    OllamaConfig {
        base_url: format!("http://{addr}"),
        model: "test-model".to_string(),
        think: true,
        num_ctx: 32768,
        options: serde_json::json!({ "temperature": 0 }),
        timeout_secs: 30,
        format: OllamaFormat::Schema,
        schema_constrained: None,
        retries_on_5xx: 2,
        retry_delay_secs: 0,
    }
}

#[tokio::test]
async fn happy_path_streams_thinking_and_content_and_captures_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let body = read_request_body(&mut stream).await;
        write_ndjson_response(
            &mut stream,
            &[
                r#"{"message":{"role":"assistant","thinking":"let me "},"done":false}"#,
                r#"{"message":{"thinking":"think"},"done":false}"#,
                r#"{"message":{"content":"{\"patch\":{}"},"done":false}"#,
                r#"{"message":{"content":",\"action\":{\"type\":\"done\"}}"},"done":false}"#,
                r#"{"message":{"content":""},"done":true,"done_reason":"stop","prompt_eval_count":260,"eval_count":12}"#,
            ],
        )
        .await;
        body
    });

    let cfg = cfg_for(addr);
    let model = OllamaModel::new(cfg.clone()).expect("build model");
    let schema = schema();
    let prompt = build_prompt(Version(3));
    let system_text = prompt.system().to_string();
    let user_text = prompt.user().to_string();
    let mut sink = CollectingSink::new();

    let out = model
        .step(prompt, &schema, &mut sink, CancellationToken::new())
        .await
        .expect("step succeeds");

    assert_eq!(out.reasoning.expect("reasoning").as_ref(), "let me think");
    assert_eq!(out.content, r#"{"patch":{},"action":{"type":"done"}}"#);
    assert_eq!(sink.reasoning, "let me think");
    assert_eq!(sink.content, r#"{"patch":{},"action":{"type":"done"}}"#);
    assert_eq!(
        out.usage,
        Some(Usage {
            prompt_tokens: 260,
            completion_tokens: 12,
        })
    );

    let body_text = server.await.expect("server task");
    let body: serde_json::Value = serde_json::from_str(&body_text).expect("valid json body");

    assert_eq!(body["model"], serde_json::json!("test-model"));
    assert_eq!(body["stream"], serde_json::json!(true));
    assert_eq!(body["think"], serde_json::json!(true));
    assert_eq!(&body["format"], schema.as_value());
    assert_eq!(body["options"]["num_ctx"], serde_json::json!(32768));
    assert_eq!(body["options"]["temperature"], serde_json::json!(0));

    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], serde_json::json!("system"));
    assert_eq!(messages[0]["content"], serde_json::json!(system_text));
    assert_eq!(messages[1]["role"], serde_json::json!("user"));
    assert_eq!(messages[1]["content"], serde_json::json!(user_text));
}

#[tokio::test]
async fn done_line_without_usage_counts_reports_no_usage() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_body(&mut stream).await;
        write_ndjson_response(
            &mut stream,
            &[
                r#"{"message":{"content":"{\"patch\":{},\"action\":{\"type\":\"done\"}}"},"done":false}"#,
                r#"{"message":{"content":""},"done":true,"done_reason":"stop"}"#,
            ],
        )
        .await;
    });

    let model = OllamaModel::new(cfg_for(addr)).expect("build model");
    let schema = schema();

    let out = model
        .step(
            build_prompt(Version(0)),
            &schema,
            &mut NullSink,
            CancellationToken::new(),
        )
        .await
        .expect("step succeeds");

    assert!(out.usage.is_none(), "usage = {:?}", out.usage);
}

#[tokio::test]
async fn error_line_returns_backend_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_body(&mut stream).await;
        write_ndjson_response(&mut stream, &[r#"{"error":"model not found"}"#]).await;
    });

    let model = OllamaModel::new(cfg_for(addr)).expect("build model");
    let schema = schema();

    let err = model
        .step(
            build_prompt(Version(0)),
            &schema,
            &mut NullSink,
            CancellationToken::new(),
        )
        .await
        .expect_err("expected backend error");

    match err {
        ModelError::Backend(msg) => assert!(msg.contains("model not found"), "msg = {msg}"),
        other => panic!("expected Backend error, got {other:?}"),
    }
}

#[tokio::test]
async fn non_2xx_status_returns_backend_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_body(&mut stream).await;
        write_status_response(&mut stream, "500 Internal Server Error", "boom").await;
    });

    // No retries configured: the server above only accepts one connection,
    // so this asserts the single-attempt behavior untouched by retry logic.
    let mut cfg = cfg_for(addr);
    cfg.retries_on_5xx = 0;
    let model = OllamaModel::new(cfg).expect("build model");
    let schema = schema();

    let err = model
        .step(
            build_prompt(Version(0)),
            &schema,
            &mut NullSink,
            CancellationToken::new(),
        )
        .await
        .expect_err("expected backend error");

    assert!(matches!(err, ModelError::Backend(_)));
}

#[tokio::test]
async fn think_false_strips_thoughts_block_from_content() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_body(&mut stream).await;
        write_ndjson_response(
            &mut stream,
            &[
                r#"{"message":{"content":"<thoughts>hmm</thoughts>\n{\"patch\":{},\"action\":{\"type\":\"yield\"}}"},"done":false}"#,
                r#"{"message":{"content":""},"done":true}"#,
            ],
        )
        .await;
    });

    let mut cfg = cfg_for(addr);
    cfg.think = false;
    let model = OllamaModel::new(cfg).expect("build model");
    let schema = schema();

    let out = model
        .step(
            build_prompt(Version(0)),
            &schema,
            &mut NullSink,
            CancellationToken::new(),
        )
        .await
        .expect("step succeeds");

    assert_eq!(out.reasoning.expect("reasoning").as_ref(), "hmm");
    assert_eq!(out.content, r#"{"patch":{},"action":{"type":"yield"}}"#);
}

#[tokio::test]
async fn cancellation_returns_cancelled_promptly() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_body(&mut stream).await;
        // Send one line, then hang well past the test's cancellation point.
        let line = r#"{"message":{"content":"partial"},"done":false}"#;
        let chunk = format!("{line}\n");
        let response_head = "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n";
        stream
            .write_all(response_head.as_bytes())
            .await
            .expect("write head");
        let chunked = format!("{:x}\r\n{}\r\n", chunk.len(), chunk);
        stream
            .write_all(chunked.as_bytes())
            .await
            .expect("write chunk");
        tokio::time::sleep(Duration::from_secs(5)).await;
    });

    let model = OllamaModel::new(cfg_for(addr)).expect("build model");
    let schema = schema();
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel_clone.cancel();
    });

    let start = std::time::Instant::now();
    let result = model
        .step(build_prompt(Version(0)), &schema, &mut NullSink, cancel)
        .await;
    let elapsed = start.elapsed();

    assert!(
        matches!(result, Err(ModelError::Cancelled)),
        "result = {result:?}"
    );
    assert!(elapsed < Duration::from_secs(1), "elapsed = {elapsed:?}");
}

#[test]
fn strip_thoughts_with_block() {
    let (reasoning, content) =
        strip_thoughts("<thoughts>hmm</thoughts>\n{\"patch\":{},\"action\":{\"type\":\"yield\"}}");
    assert_eq!(reasoning.expect("reasoning").as_ref(), "hmm");
    assert_eq!(content, r#"{"patch":{},"action":{"type":"yield"}}"#);
}

#[test]
fn strip_thoughts_without_block() {
    let (reasoning, content) = strip_thoughts("  {\"patch\":{}}  ");
    assert!(reasoning.is_none());
    assert_eq!(content, r#"{"patch":{}}"#);
}

#[test]
fn strip_thoughts_unterminated_block() {
    // Documented behavior: an unterminated block is treated as reasoning
    // through end-of-string, with empty content, since there is no
    // reliable JSON left to recover.
    let (reasoning, content) = strip_thoughts("<thoughts>still going");
    assert_eq!(reasoning.expect("reasoning").as_ref(), "still going");
    assert_eq!(content, "");
}

/// A `done` line whose `done_reason` is `"length"` (the server cut the
/// output off after hitting a token/length limit, as opposed to a clean
/// `"stop"`) must surface as `RawOutput.finish_reason`, so the harness can
/// explain the resulting parse failure instead of leaving the model to
/// guess why its JSON was truncated.
#[tokio::test]
async fn done_reason_length_is_reported_as_finish_reason() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_body(&mut stream).await;
        write_ndjson_response(
            &mut stream,
            &[
                r#"{"message":{"content":"{\"patch\":{}"},"done":false}"#,
                r#"{"message":{"content":""},"done":true,"done_reason":"length","prompt_eval_count":260,"eval_count":1100}"#,
            ],
        )
        .await;
    });

    let model = OllamaModel::new(cfg_for(addr)).expect("build model");
    let schema = schema();

    let out = model
        .step(
            build_prompt(Version(0)),
            &schema,
            &mut NullSink,
            CancellationToken::new(),
        )
        .await
        .expect("step succeeds");

    assert_eq!(out.finish_reason.as_deref(), Some("length"));
    assert_eq!(
        out.usage,
        Some(Usage {
            prompt_tokens: 260,
            completion_tokens: 1100,
        })
    );
}

#[tokio::test]
async fn json_format_mode_sends_plain_json_and_reports_unconstrained() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let body = read_request_body(&mut sock).await;
        write_ndjson_response(
            &mut sock,
            &[r#"{"message":{"role":"assistant","content":"{\"patch\":{},\"action\":{\"type\":\"done\"}}"},"done":false}"#,
              r#"{"message":{"content":""},"done":true}"#],
        )
        .await;
        body
    });
    let mut cfg = cfg_for(addr);
    cfg.format = OllamaFormat::Json;
    let model = OllamaModel::new(cfg).unwrap();
    assert!(!model.capabilities().schema_constrained);
    let prompt = build_prompt(Version(0));
    let schema = schema();
    let mut sink = CollectingSink::new();
    let out = model
        .step(
            prompt,
            &schema,
            &mut sink,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(out.content, r#"{"patch":{},"action":{"type":"done"}}"#);
    let body: serde_json::Value = serde_json::from_str(&server.await.unwrap()).unwrap();
    assert_eq!(body["format"], serde_json::json!("json"));
}

/// A 500 followed by a valid response: `step` should retry and succeed,
/// having made exactly two requests.
#[tokio::test]
async fn retries_once_after_500_then_succeeds() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let request_count = Arc::new(AtomicUsize::new(0));
    let request_count_server = request_count.clone();

    tokio::spawn(async move {
        for i in 0..2 {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let _ = read_request_body(&mut stream).await;
            request_count_server.fetch_add(1, Ordering::SeqCst);
            if i == 0 {
                write_status_response(&mut stream, "500 Internal Server Error", "boom").await;
            } else {
                write_ndjson_response(
                    &mut stream,
                    &[
                        r#"{"message":{"content":"{\"patch\":{},\"action\":{\"type\":\"done\"}}"},"done":false}"#,
                        r#"{"message":{"content":""},"done":true}"#,
                    ],
                )
                .await;
            }
        }
    });

    let mut cfg = cfg_for(addr);
    cfg.retries_on_5xx = 2;
    cfg.retry_delay_secs = 0;
    let model = OllamaModel::new(cfg).expect("build model");
    let schema = schema();

    let out = model
        .step(
            build_prompt(Version(0)),
            &schema,
            &mut NullSink,
            CancellationToken::new(),
        )
        .await
        .expect("step should succeed after one retry");

    assert_eq!(out.content, r#"{"patch":{},"action":{"type":"done"}}"#);
    assert_eq!(request_count.load(Ordering::SeqCst), 2);
}

/// Three consecutive 500s with `retries_on_5xx: 2` exhaust the retry budget:
/// `step` returns `ModelError::Backend` containing "500", having made
/// exactly three requests (the original attempt plus two retries).
#[tokio::test]
async fn exhausts_retries_and_returns_backend_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let request_count = Arc::new(AtomicUsize::new(0));
    let request_count_server = request_count.clone();

    tokio::spawn(async move {
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let _ = read_request_body(&mut stream).await;
            request_count_server.fetch_add(1, Ordering::SeqCst);
            write_status_response(&mut stream, "500 Internal Server Error", "boom").await;
        }
    });

    let mut cfg = cfg_for(addr);
    cfg.retries_on_5xx = 2;
    cfg.retry_delay_secs = 0;
    let model = OllamaModel::new(cfg).expect("build model");
    let schema = schema();

    let err = model
        .step(
            build_prompt(Version(0)),
            &schema,
            &mut NullSink,
            CancellationToken::new(),
        )
        .await
        .expect_err("expected backend error after exhausting retries");

    match err {
        ModelError::Backend(msg) => assert!(msg.contains("500"), "msg = {msg}"),
        other => panic!("expected Backend error, got {other:?}"),
    }
    assert_eq!(request_count.load(Ordering::SeqCst), 3);
}

/// A 404 must not be retried: `step` fails immediately with a single
/// request made.
#[tokio::test]
async fn does_not_retry_4xx() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let request_count = Arc::new(AtomicUsize::new(0));
    let request_count_server = request_count.clone();

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_body(&mut stream).await;
        request_count_server.fetch_add(1, Ordering::SeqCst);
        write_status_response(&mut stream, "404 Not Found", "nope").await;
    });

    let mut cfg = cfg_for(addr);
    cfg.retries_on_5xx = 2;
    cfg.retry_delay_secs = 0;
    let model = OllamaModel::new(cfg).expect("build model");
    let schema = schema();

    let err = model
        .step(
            build_prompt(Version(0)),
            &schema,
            &mut NullSink,
            CancellationToken::new(),
        )
        .await
        .expect_err("expected backend error");

    assert!(matches!(err, ModelError::Backend(_)));
    assert_eq!(request_count.load(Ordering::SeqCst), 1);
}

/// Cancellation during the retry delay returns promptly with the
/// cancellation error, and no second request is made.
#[tokio::test]
async fn cancellation_during_retry_delay_returns_promptly() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let request_count = Arc::new(AtomicUsize::new(0));
    let request_count_server = request_count.clone();

    tokio::spawn(async move {
        // Only ever accept one connection: a second request would indicate
        // the retry delay wasn't actually cancelled.
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_body(&mut stream).await;
        request_count_server.fetch_add(1, Ordering::SeqCst);
        write_status_response(&mut stream, "500 Internal Server Error", "boom").await;
    });

    let mut cfg = cfg_for(addr);
    cfg.retries_on_5xx = 2;
    cfg.retry_delay_secs = 5;
    let model = OllamaModel::new(cfg).expect("build model");
    let schema = schema();
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel_clone.cancel();
    });

    let start = std::time::Instant::now();
    let result = model
        .step(build_prompt(Version(0)), &schema, &mut NullSink, cancel)
        .await;
    let elapsed = start.elapsed();

    assert!(
        matches!(result, Err(ModelError::Cancelled)),
        "result = {result:?}"
    );
    assert!(elapsed < Duration::from_secs(1), "elapsed = {elapsed:?}");
    // Give the server task a moment to notice a stray second connection, if
    // any, before asserting the count.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(request_count.load(Ordering::SeqCst), 1);
}
