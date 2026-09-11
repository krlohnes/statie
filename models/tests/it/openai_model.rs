use std::time::Duration;

use statie_core::{Model, ModelError, NullSink, Usage, Version};
use statie_models::{OpenAiCompatConfig, OpenAiCompatModel};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use super::common;
use common::{
    CollectingSink, build_prompt, read_request, read_request_body, schema, write_sse_response,
    write_status_response,
};

fn cfg_for(addr: std::net::SocketAddr) -> OpenAiCompatConfig {
    OpenAiCompatConfig {
        base_url: format!("http://{addr}/v1"),
        model: "test-model".to_string(),
        api_key: None,
        extra_body: serde_json::json!({}),
        timeout_secs: 30,
        reasoning_field: Some("reasoning_content".to_string()),
        use_json_schema_format: true,
    }
}

#[tokio::test]
async fn happy_path_streams_reasoning_and_content_and_captures_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let (body, headers) = read_request(&mut stream).await;
        write_sse_response(
            &mut stream,
            &[
                r#"{"choices":[{"delta":{"reasoning_content":"let me "}}]}"#,
                r#"{"choices":[{"delta":{"reasoning_content":"think"}}]}"#,
                r#"{"choices":[{"delta":{"content":"{\"patch\":{}"}}]}"#,
                r#"{"choices":[{"delta":{"content":",\"action\":{\"type\":\"done\"}}"}}]}"#,
                r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":7}}"#,
            ],
        )
        .await;
        (body, headers)
    });

    let mut cfg = cfg_for(addr);
    cfg.api_key = Some("k".to_string());
    cfg.extra_body = serde_json::json!({ "temperature": 0 });
    let model = OpenAiCompatModel::new(cfg).expect("build model");
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
            prompt_tokens: 100,
            completion_tokens: 7,
        })
    );

    let (body_text, headers) = server.await.expect("server task");
    let body: serde_json::Value = serde_json::from_str(&body_text).expect("valid json body");

    assert_eq!(body["model"], serde_json::json!("test-model"));
    assert_eq!(body["stream"], serde_json::json!(true));
    assert_eq!(
        body["stream_options"]["include_usage"],
        serde_json::json!(true)
    );
    assert_eq!(
        &body["response_format"]["json_schema"]["schema"],
        schema.as_value()
    );
    assert_eq!(
        body["response_format"]["type"],
        serde_json::json!("json_schema")
    );
    assert_eq!(
        body["response_format"]["json_schema"]["strict"],
        serde_json::json!(true)
    );
    assert_eq!(body["temperature"], serde_json::json!(0));

    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], serde_json::json!("system"));
    assert_eq!(messages[0]["content"], serde_json::json!(system_text));
    assert_eq!(messages[1]["role"], serde_json::json!("user"));
    assert_eq!(messages[1]["content"], serde_json::json!(user_text));

    let headers_lower = headers.to_ascii_lowercase();
    assert!(
        headers_lower.contains("authorization: bearer k"),
        "headers = {headers}"
    );
}

#[tokio::test]
async fn no_native_reasoning_extracts_thoughts_block_from_content() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_body(&mut stream).await;
        write_sse_response(
            &mut stream,
            &[
                r#"{"choices":[{"delta":{"content":"<thoughts>hmm</thoughts>\n{\"patch\":{},\"action\":{\"type\":\"yield\"}}"}}]}"#,
            ],
        )
        .await;
    });

    let model = OpenAiCompatModel::new(cfg_for(addr)).expect("build model");
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
async fn non_2xx_status_returns_backend_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_body(&mut stream).await;
        write_status_response(&mut stream, "500 Internal Server Error", "boom").await;
    });

    let model = OpenAiCompatModel::new(cfg_for(addr)).expect("build model");
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
async fn error_chunk_returns_backend_error_with_message() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_body(&mut stream).await;
        write_sse_response(&mut stream, &[r#"{"error":{"message":"bad"}}"#]).await;
    });

    let model = OpenAiCompatModel::new(cfg_for(addr)).expect("build model");
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
        ModelError::Backend(msg) => assert!(msg.contains("bad"), "msg = {msg}"),
        other => panic!("expected Backend error, got {other:?}"),
    }
}

#[tokio::test]
async fn cancellation_returns_cancelled_promptly() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_body(&mut stream).await;
        // Send one SSE event, then hang well past the test's cancellation point.
        let line = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n";
        let response_head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n";
        stream
            .write_all(response_head.as_bytes())
            .await
            .expect("write head");
        let chunked = format!("{:x}\r\n{}\r\n", line.len(), line);
        stream
            .write_all(chunked.as_bytes())
            .await
            .expect("write chunk");
        tokio::time::sleep(Duration::from_secs(5)).await;
    });

    let model = OpenAiCompatModel::new(cfg_for(addr)).expect("build model");
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

/// A chunk whose first choice carries `finish_reason: "length"` (the server
/// cut generation off at a token/length limit, as opposed to a clean
/// `"stop"`) must surface as `RawOutput.finish_reason`, so the harness can
/// explain the resulting parse failure instead of leaving the model to
/// guess why its JSON was truncated.
#[tokio::test]
async fn finish_reason_length_is_reported() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_body(&mut stream).await;
        write_sse_response(
            &mut stream,
            &[
                r#"{"choices":[{"delta":{"content":"{\"patch\":{}"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"length"}],"usage":{"prompt_tokens":100,"completion_tokens":1100}}"#,
            ],
        )
        .await;
    });

    let model = OpenAiCompatModel::new(cfg_for(addr)).expect("build model");
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
            prompt_tokens: 100,
            completion_tokens: 1100,
        })
    );
}

#[tokio::test]
async fn use_json_schema_format_false_omits_response_format_and_capability() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let body = read_request_body(&mut stream).await;
        write_sse_response(
            &mut stream,
            &[r#"{"choices":[{"delta":{"content":"{\"patch\":{},\"action\":{\"type\":\"done\"}}"}}]}"#],
        )
        .await;
        body
    });

    let mut cfg = cfg_for(addr);
    cfg.use_json_schema_format = false;
    let model = OpenAiCompatModel::new(cfg).expect("build model");

    assert!(!model.capabilities().schema_constrained);

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
    assert_eq!(out.content, r#"{"patch":{},"action":{"type":"done"}}"#);

    let body_text = server.await.expect("server task");
    let body: serde_json::Value = serde_json::from_str(&body_text).expect("valid json body");
    assert!(
        body.get("response_format").is_none(),
        "response_format should be absent, body = {body}"
    );
}
