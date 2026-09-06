//! Integration tests for `OllamaModel::chat_free` and `ModelThinker`,
//! against the same fake-TCP-server harness `ollama_model.rs` uses.

use std::sync::Arc;
use std::time::Duration;

use statie_core::{ThinkError, ThinkInput, Thinker};
use statie_models::{FreeReply, ModelThinker, OllamaConfig, OllamaFormat, OllamaModel};
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
        options: serde_json::json!({ "temperature": 0 }),
        timeout_secs: 30,
        // Deliberately Schema: `chat_free` must never send `format`
        // regardless of this setting.
        format: OllamaFormat::Schema,
        schema_constrained: None,
        retries_on_5xx: 0,
        retry_delay_secs: 0,
    }
}

#[tokio::test]
async fn chat_free_sends_think_true_and_no_format_key() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let body = read_request_body(&mut stream).await;
        write_ndjson_response(
            &mut stream,
            &[
                r#"{"message":{"thinking":"let me "},"done":false}"#,
                r#"{"message":{"thinking":"think"},"done":false}"#,
                r#"{"message":{"content":"blah\n"},"done":false}"#,
                r#"{"message":{"content":"ANSWER:\n"},"done":false}"#,
                r#"{"message":{"content":"fix tax.rs line 3"},"done":false}"#,
                r#"{"message":{"content":""},"done":true,"done_reason":"stop","prompt_eval_count":100,"eval_count":20}"#,
            ],
        )
        .await;
        body
    });

    let model = OllamaModel::new(cfg_for(addr)).expect("build model");
    let reply: FreeReply = model
        .chat_free("sys prompt", "user prompt", true, CancellationToken::new())
        .await
        .expect("chat_free succeeds");

    assert_eq!(reply.thinking, "let me think");
    assert_eq!(reply.content, "blah\nANSWER:\nfix tax.rs line 3");
    assert_eq!(reply.done_reason.as_deref(), Some("stop"));
    assert_eq!(reply.usage.expect("usage").prompt_tokens, 100);

    let body_text = server.await.expect("server task");
    let body: serde_json::Value = serde_json::from_str(&body_text).expect("valid json body");
    assert_eq!(body["think"], serde_json::json!(true));
    assert!(
        body.get("format").is_none(),
        "chat_free must never send `format`, got {body}"
    );
    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages[0]["content"], serde_json::json!("sys prompt"));
    assert_eq!(messages[1]["content"], serde_json::json!("user prompt"));
}

#[tokio::test]
async fn model_thinker_extracts_answer_and_reasoning_chars() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_body(&mut stream).await;
        write_ndjson_response(
            &mut stream,
            &[
                r#"{"message":{"thinking":"reasoning trace"},"done":false}"#,
                r#"{"message":{"content":"blah\nANSWER:\nfix tax.rs line 3"},"done":false}"#,
                r#"{"message":{"content":""},"done":true}"#,
            ],
        )
        .await;
    });

    let model = Arc::new(OllamaModel::new(cfg_for(addr)).expect("build model"));
    let thinker = ModelThinker::new(model, Duration::from_secs(5));

    let out = thinker
        .think(
            ThinkInput {
                system: "sys",
                user: "usr",
                question: "what's wrong?",
            },
            CancellationToken::new(),
        )
        .await
        .expect("think succeeds");

    assert_eq!(out.answer, "fix tax.rs line 3");
    assert_eq!(out.reasoning_chars, "reasoning trace".chars().count());
    assert_eq!(
        out.content_chars,
        "blah\nANSWER:\nfix tax.rs line 3".chars().count()
    );
}

#[tokio::test]
async fn model_thinker_empty_content_is_empty_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_body(&mut stream).await;
        write_ndjson_response(
            &mut stream,
            &[
                r#"{"message":{"thinking":"thought but said nothing"},"done":false}"#,
                r#"{"message":{"content":""},"done":true}"#,
            ],
        )
        .await;
    });

    let model = Arc::new(OllamaModel::new(cfg_for(addr)).expect("build model"));
    let thinker = ModelThinker::new(model, Duration::from_secs(5));

    let err = thinker
        .think(
            ThinkInput {
                system: "sys",
                user: "usr",
                question: "q",
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("empty content should error");

    assert!(matches!(err, ThinkError::Empty), "got {err:?}");
}

/// A second live battery reproduced this exactly: with a 16384 `num_ctx`
/// shared with the acting model's prompt, native thinking ran long enough to
/// hit the context limit before ever emitting content -- `done_reason:
/// "length"` with empty content. This must surface as a named `Backend`
/// error mentioning `num_ctx`, not the generic `Empty` (which does not tell
/// anyone to raise `thinker.num_ctx`).
#[tokio::test]
async fn model_thinker_empty_content_with_length_done_reason_is_backend_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_body(&mut stream).await;
        write_ndjson_response(
            &mut stream,
            &[
                r#"{"message":{"thinking":"thinking forever"},"done":false}"#,
                r#"{"message":{"content":""},"done":true,"done_reason":"length"}"#,
            ],
        )
        .await;
    });

    let model = Arc::new(OllamaModel::new(cfg_for(addr)).expect("build model"));
    let thinker = ModelThinker::new(model, Duration::from_secs(5));

    let err = thinker
        .think(
            ThinkInput {
                system: "sys",
                user: "usr",
                question: "q",
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("exhausted context window should error");

    match err {
        ThinkError::Backend(msg) => {
            assert!(msg.contains("num_ctx"), "msg = {msg}");
            assert!(
                msg.contains("32768"),
                "msg should name the configured num_ctx: {msg}"
            );
        }
        other => panic!("expected Backend, got {other:?}"),
    }
}

/// Empty content with any OTHER `done_reason` (including `"stop"`) must
/// still yield the generic `Empty` error -- only `"length"` (context
/// exhausted) gets the more specific `Backend` message.
#[tokio::test]
async fn model_thinker_empty_content_with_stop_done_reason_is_still_empty() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_body(&mut stream).await;
        write_ndjson_response(
            &mut stream,
            &[r#"{"message":{"content":""},"done":true,"done_reason":"stop"}"#],
        )
        .await;
    });

    let model = Arc::new(OllamaModel::new(cfg_for(addr)).expect("build model"));
    let thinker = ModelThinker::new(model, Duration::from_secs(5));

    let err = thinker
        .think(
            ThinkInput {
                system: "sys",
                user: "usr",
                question: "q",
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("empty content should still error");

    assert!(matches!(err, ThinkError::Empty), "got {err:?}");
}

#[tokio::test]
async fn model_thinker_backend_error_is_backend() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_body(&mut stream).await;
        write_ndjson_response(&mut stream, &[r#"{"error":"model not found"}"#]).await;
    });

    let model = Arc::new(OllamaModel::new(cfg_for(addr)).expect("build model"));
    let thinker = ModelThinker::new(model, Duration::from_secs(5));

    let err = thinker
        .think(
            ThinkInput {
                system: "sys",
                user: "usr",
                question: "q",
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("backend error expected");

    match err {
        ThinkError::Backend(msg) => assert!(msg.contains("model not found"), "msg = {msg}"),
        other => panic!("expected Backend, got {other:?}"),
    }
}

/// `build_think_prompt` must not leak the acting model's system prompt into
/// the reasoning subagent's user message: a first live battery showed the
/// subagent imitate that prompt's output contract (JSON shape, `<thoughts>`
/// tags, tool-call planning) when it was included. The user message must
/// still carry the acting model's user prompt and the "Reply in prose"
/// instruction.
#[tokio::test]
async fn think_request_omits_system_text_and_carries_reply_in_prose_instruction() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let body = read_request_body(&mut stream).await;
        write_ndjson_response(
            &mut stream,
            &[
                r#"{"message":{"content":"ANSWER:\nfix it"},"done":false}"#,
                r#"{"message":{"content":""},"done":true}"#,
            ],
        )
        .await;
        body
    });

    let model = Arc::new(OllamaModel::new(cfg_for(addr)).expect("build model"));
    let thinker = ModelThinker::new(model, Duration::from_secs(5));

    let out = thinker
        .think(
            ThinkInput {
                system: "SECRET_SYSTEM_CONTRACT_MARKER json schema tool calls",
                user: "USER_PROMPT_MARKER workspace state",
                question: "what's wrong?",
            },
            CancellationToken::new(),
        )
        .await
        .expect("think succeeds");
    assert_eq!(out.answer, "fix it");

    let body_text = server.await.expect("server task");
    let body: serde_json::Value = serde_json::from_str(&body_text).expect("valid json body");
    let messages = body["messages"].as_array().expect("messages array");
    let user_message = messages[1]["content"].as_str().expect("user content");
    assert!(
        !user_message.contains("SECRET_SYSTEM_CONTRACT_MARKER"),
        "user message must not contain the acting model's system prompt: {user_message}"
    );
    assert!(user_message.contains("USER_PROMPT_MARKER"));
    assert!(user_message.contains("Reply in prose"));
}

/// A reply that wraps everything in a `<thoughts>...</thoughts>` block
/// (some backends deliver reasoning inline that way instead of, or as well
/// as, native thinking) with `ANSWER:` inside the tags must still yield the
/// right answer, and the stripped block's length must count toward
/// `reasoning_chars`.
#[tokio::test]
async fn model_thinker_extracts_answer_from_inline_thoughts_block() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_body(&mut stream).await;
        write_ndjson_response(
            &mut stream,
            &[
                r#"{"message":{"content":"<thoughts>reasoning here"},"done":false}"#,
                r#"{"message":{"content":"\nANSWER:\nfix tax.rs line 3</thoughts>"},"done":false}"#,
                r#"{"message":{"content":""},"done":true}"#,
            ],
        )
        .await;
    });

    let model = Arc::new(OllamaModel::new(cfg_for(addr)).expect("build model"));
    let thinker = ModelThinker::new(model, Duration::from_secs(5));

    let out = thinker
        .think(
            ThinkInput {
                system: "sys",
                user: "usr",
                question: "what's wrong?",
            },
            CancellationToken::new(),
        )
        .await
        .expect("think succeeds");

    assert_eq!(out.answer, "fix tax.rs line 3");
    assert!(
        out.reasoning_chars > 0,
        "stripped <thoughts> block should count as reasoning_chars"
    );
}

/// A reply with no `ANSWER:` marker anywhere must yield the tail (with its
/// prefix), not the whole reply -- a first live battery showed a
/// 24,815-character reply with no marker become the entire "answer".
#[tokio::test]
async fn model_thinker_no_marker_yields_prefixed_tail() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let long_reply = "x".repeat(3000);
    let content_line =
        serde_json::json!({"message": {"content": long_reply}, "done": false}).to_string();
    let done_line = r#"{"message":{"content":""},"done":true}"#.to_string();

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_body(&mut stream).await;
        write_ndjson_response(&mut stream, &[&content_line, &done_line]).await;
    });

    let model = Arc::new(OllamaModel::new(cfg_for(addr)).expect("build model"));
    let thinker = ModelThinker::new(model, Duration::from_secs(5));

    let out = thinker
        .think(
            ThinkInput {
                system: "sys",
                user: "usr",
                question: "what's wrong?",
            },
            CancellationToken::new(),
        )
        .await
        .expect("think succeeds");

    assert!(
        out.answer
            .starts_with("(no ANSWER section; tail of the reply follows)\n")
    );
    assert!(out.answer.ends_with(&"x".repeat(100)));
    assert!(
        out.answer.len() < 3000,
        "answer should be truncated to a tail, got len {}",
        out.answer.len()
    );
}

/// A `Thinker`-driving `OllamaModel` whose server never responds within the
/// configured timeout must surface `ThinkError::Timeout`.
#[tokio::test]
async fn model_thinker_times_out() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_body(&mut stream).await;
        // Never respond; the ModelThinker's own timeout should fire first.
        tokio::time::sleep(Duration::from_secs(5)).await;
        drop(stream);
    });

    let mut cfg = cfg_for(addr);
    cfg.timeout_secs = 30;
    let model = Arc::new(OllamaModel::new(cfg).expect("build model"));
    let thinker = ModelThinker::new(model, Duration::from_millis(100));

    let err = thinker
        .think(
            ThinkInput {
                system: "sys",
                user: "usr",
                question: "q",
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("timeout expected");

    assert!(matches!(err, ThinkError::Timeout), "got {err:?}");
}
