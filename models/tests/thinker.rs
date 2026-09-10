//! Integration tests for `OllamaModel::chat_free` and `ModelThinker`,
//! against the same fake-TCP-server harness `ollama_model.rs` uses.

use std::sync::Arc;
use std::time::Duration;

use statie_core::{ThinkError, ThinkInput, Thinker};
use statie_models::{
    ChunkConfig, FreeReply, ModelThinker, OllamaConfig, OllamaFormat, OllamaModel,
};
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

/// Chunk 1 returns thinking only with `done_reason: "length"` (the model
/// was still reasoning when the cap landed); the carry-over call summarizes
/// it; chunk 2 restarts from the original prompt plus that summary and
/// returns content with an `ANSWER:` marker. Checks the answer, `chunks ==
/// 2`, summed `reasoning_chars`, that chunk 2's request carries "Reasoning
/// so far" and the summary text with `think: true`, that the carry-over
/// request has `think: false` and `num_predict: 600`, and that every
/// request (including the carry-over) carries the same `num_ctx`.
#[tokio::test]
async fn chunked_thinker_carries_over_and_answers_on_second_chunk() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let server = tokio::spawn(async move {
        // Chunk 1: thinking only, done_reason length.
        let (mut stream, _) = listener.accept().await.expect("accept 1");
        let body1 = read_request_body(&mut stream).await;
        write_ndjson_response(
            &mut stream,
            &[
                r#"{"message":{"thinking":"chunk one reasoning trace"},"done":false}"#,
                r#"{"message":{"content":""},"done":true,"done_reason":"length"}"#,
            ],
        )
        .await;

        // Carry-over: thinking off, short summary.
        let (mut stream, _) = listener.accept().await.expect("accept carry-over");
        let body_carry = read_request_body(&mut stream).await;
        write_ndjson_response(
            &mut stream,
            &[
                r#"{"message":{"content":"SUMMARY_MARKER: established X, open Y, next check Z"},"done":false}"#,
                r#"{"message":{"content":""},"done":true,"done_reason":"stop"}"#,
            ],
        )
        .await;

        // Chunk 2: answers.
        let (mut stream, _) = listener.accept().await.expect("accept 2");
        let body2 = read_request_body(&mut stream).await;
        write_ndjson_response(
            &mut stream,
            &[
                r#"{"message":{"thinking":"chunk two reasoning"},"done":false}"#,
                r#"{"message":{"content":"ANSWER:\nfix tax.rs line 3"},"done":false}"#,
                r#"{"message":{"content":""},"done":true,"done_reason":"stop"}"#,
            ],
        )
        .await;

        (body1, body_carry, body2)
    });

    let cfg = cfg_for(addr);
    let model = Arc::new(OllamaModel::new(cfg).expect("build model"));
    let thinker = ModelThinker::new(model, Duration::from_secs(5)).with_chunking(ChunkConfig {
        chunk_tokens: 6000,
        max_chunks: 4,
        carry_over_chars: 1500,
    });

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
    assert_eq!(out.chunks, 2);
    assert_eq!(
        out.reasoning_chars,
        "chunk one reasoning trace".chars().count() + "chunk two reasoning".chars().count()
    );

    let (body1_text, body_carry_text, body2_text) = server.await.expect("server task");

    let body1: serde_json::Value = serde_json::from_str(&body1_text).expect("valid json");
    assert_eq!(body1["think"], serde_json::json!(true));
    assert_eq!(body1["options"]["num_predict"], serde_json::json!(6000));
    assert_eq!(body1["options"]["num_ctx"], serde_json::json!(32768));

    let body_carry: serde_json::Value = serde_json::from_str(&body_carry_text).expect("valid json");
    assert_eq!(body_carry["think"], serde_json::json!(false));
    assert_eq!(body_carry["options"]["num_predict"], serde_json::json!(600));
    assert_eq!(body_carry["options"]["num_ctx"], serde_json::json!(32768));

    let body2: serde_json::Value = serde_json::from_str(&body2_text).expect("valid json");
    assert_eq!(body2["think"], serde_json::json!(true));
    assert_eq!(body2["options"]["num_ctx"], serde_json::json!(32768));
    let messages2 = body2["messages"].as_array().expect("messages array");
    let user2 = messages2[1]["content"].as_str().expect("user content");
    assert!(
        user2.contains("Reasoning so far"),
        "chunk 2 user message must carry the carry-over marker: {user2}"
    );
    assert!(
        user2.contains("SUMMARY_MARKER"),
        "chunk 2 user message must carry the carry-over summary text: {user2}"
    );
}

/// When every chunk keeps hitting `done_reason: "length"` with no content,
/// the thinker must give up after `max_chunks` -- but only after a forced,
/// thinking-off call attempts an answer from the reasoning gathered so far
/// (BACKLOG.md item 6b). Here that forced call also returns empty content,
/// so the final result must still be a named `Backend` error mentioning the
/// chunk count, with its message extended to say the forced answer was also
/// empty.
#[tokio::test]
async fn chunked_thinker_exhausts_max_chunks() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let max_chunks = 2u32;

    tokio::spawn(async move {
        // Chunk 1: length, no content.
        let (mut stream, _) = listener.accept().await.expect("accept 1");
        let _ = read_request_body(&mut stream).await;
        write_ndjson_response(
            &mut stream,
            &[r#"{"message":{"content":""},"done":true,"done_reason":"length"}"#],
        )
        .await;

        // Carry-over for chunk 1 -> 2.
        let (mut stream, _) = listener.accept().await.expect("accept carry-over");
        let _ = read_request_body(&mut stream).await;
        write_ndjson_response(
            &mut stream,
            &[r#"{"message":{"content":"summary"},"done":true,"done_reason":"stop"}"#],
        )
        .await;

        // Chunk 2 (== max_chunks): length again, no content -- loop must
        // stop here without a further per-chunk carry-over call.
        let (mut stream, _) = listener.accept().await.expect("accept 2");
        let _ = read_request_body(&mut stream).await;
        write_ndjson_response(
            &mut stream,
            &[r#"{"message":{"content":""},"done":true,"done_reason":"length"}"#],
        )
        .await;

        // Carry-over for the final chunk (produced the same way as every
        // earlier chunk's), feeding the forced call.
        let (mut stream, _) = listener.accept().await.expect("accept final carry-over");
        let _ = read_request_body(&mut stream).await;
        write_ndjson_response(
            &mut stream,
            &[r#"{"message":{"content":"final summary"},"done":true,"done_reason":"stop"}"#],
        )
        .await;

        // Forced call: empty content, so the exhausted-chunks error must
        // still fire.
        let (mut stream, _) = listener.accept().await.expect("accept forced");
        let _ = read_request_body(&mut stream).await;
        write_ndjson_response(
            &mut stream,
            &[r#"{"message":{"content":""},"done":true,"done_reason":"stop"}"#],
        )
        .await;
    });

    let cfg = cfg_for(addr);
    let model = Arc::new(OllamaModel::new(cfg).expect("build model"));
    let thinker = ModelThinker::new(model, Duration::from_secs(5)).with_chunking(ChunkConfig {
        chunk_tokens: 6000,
        max_chunks,
        carry_over_chars: 1500,
    });

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
        .expect_err("exhausted chunks should error");

    match err {
        ThinkError::Backend(msg) => {
            assert!(
                msg.contains(&max_chunks.to_string()),
                "msg should name the chunk count: {msg}"
            );
            assert!(
                msg.contains("forced answer was empty"),
                "msg should say the forced answer was empty: {msg}"
            );
        }
        other => panic!("expected Backend, got {other:?}"),
    }
}

/// BACKLOG.md item 6b: when both permitted chunks end with `done_reason:
/// "length"` and no content, the thinker must make one more call -- thinking
/// off, `num_predict: 700` -- carrying over the final chunk's reasoning the
/// same way the loop's own carry-over step does, and asking the model to
/// answer now. When that forced call returns an `ANSWER:`, `think` must
/// return it rather than erroring.
#[tokio::test]
async fn chunked_thinker_forces_answer_after_exhausting_chunks() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let max_chunks = 2u32;

    let server = tokio::spawn(async move {
        // Chunk 1: length, no content.
        let (mut stream, _) = listener.accept().await.expect("accept 1");
        let body1 = read_request_body(&mut stream).await;
        write_ndjson_response(
            &mut stream,
            &[
                r#"{"message":{"thinking":"chunk one reasoning"},"done":false}"#,
                r#"{"message":{"content":""},"done":true,"done_reason":"length"}"#,
            ],
        )
        .await;

        // Carry-over for chunk 1 -> 2.
        let (mut stream, _) = listener.accept().await.expect("accept carry-over 1");
        let body_carry1 = read_request_body(&mut stream).await;
        write_ndjson_response(
            &mut stream,
            &[r#"{"message":{"content":"first summary"},"done":true,"done_reason":"stop"}"#],
        )
        .await;

        // Chunk 2 (== max_chunks): length again, no content.
        let (mut stream, _) = listener.accept().await.expect("accept 2");
        let body2 = read_request_body(&mut stream).await;
        write_ndjson_response(
            &mut stream,
            &[
                r#"{"message":{"thinking":"chunk two reasoning"},"done":false}"#,
                r#"{"message":{"content":""},"done":true,"done_reason":"length"}"#,
            ],
        )
        .await;

        // Carry-over for the final (second) chunk.
        let (mut stream, _) = listener.accept().await.expect("accept carry-over 2");
        let body_carry2 = read_request_body(&mut stream).await;
        write_ndjson_response(
            &mut stream,
            &[r#"{"message":{"content":"second summary"},"done":true,"done_reason":"stop"}"#],
        )
        .await;

        // Forced call: answers.
        let (mut stream, _) = listener.accept().await.expect("accept forced");
        let body_forced = read_request_body(&mut stream).await;
        write_ndjson_response(
            &mut stream,
            &[
                r#"{"message":{"content":"ANSWER: fix the import in tax.rs"},"done":false}"#,
                r#"{"message":{"content":""},"done":true,"done_reason":"stop"}"#,
            ],
        )
        .await;

        (body1, body_carry1, body2, body_carry2, body_forced)
    });

    let cfg = cfg_for(addr);
    let model = Arc::new(OllamaModel::new(cfg).expect("build model"));
    let thinker = ModelThinker::new(model, Duration::from_secs(5)).with_chunking(ChunkConfig {
        chunk_tokens: 6000,
        max_chunks,
        carry_over_chars: 1500,
    });

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
        .expect("forced answer should succeed");

    assert_eq!(out.answer, "fix the import in tax.rs");
    assert!(out.forced_answer, "forced_answer should be true");

    let (_body1, _body_carry1, _body2, _body_carry2, body_forced_text) =
        server.await.expect("server task");

    let body_forced: serde_json::Value =
        serde_json::from_str(&body_forced_text).expect("valid json");
    assert_eq!(body_forced["think"], serde_json::json!(false));
    assert_eq!(
        body_forced["options"]["num_predict"],
        serde_json::json!(700)
    );
    let messages_forced = body_forced["messages"].as_array().expect("messages array");
    let user_forced = messages_forced[1]["content"]
        .as_str()
        .expect("user content");
    assert!(
        user_forced.contains("out of thinking budget"),
        "forced request user content must mention the thinking budget: {user_forced}"
    );
    assert!(
        user_forced.contains("second summary"),
        "forced request user content must carry the second (final chunk's) carry-over text: {user_forced}"
    );
}
