//! `OllamaModel`: a `Model` backend for a local Ollama server's `/api/chat`
//! endpoint.

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use statie_core::{
    BoundedPrompt, Capabilities, Model, ModelError, NullSink, RawOutput, Reasoning, Schema,
    StepSink, Usage,
};
use tokio_util::sync::CancellationToken;

use crate::thoughts::strip_thoughts;

/// Configuration for `OllamaModel`. Nothing about the endpoint is hardcoded
/// anywhere else in the crate — this struct is the only source.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OllamaConfig {
    pub base_url: String,
    pub model: String,
    pub think: bool,
    /// Context window requested on EVERY request. Pinned on purpose: Ollama
    /// sizes the runner's KV cache from `num_ctx` and reloads the model
    /// whenever a request arrives with a different value, so this must stay
    /// constant across runs (and match any other client of the same server).
    pub num_ctx: u32,
    /// Extra `options` passed through verbatim (never `num_ctx`; see above).
    pub options: Value,
    pub timeout_secs: u64,
    /// What to send as Ollama's `format`: `Schema` (the full JSON schema),
    /// `Json` (plain JSON mode), or `None`. Default `Json`: on Ollama 0.33
    /// with gemma4 the full-schema grammar was observed to *degrade* output
    /// (the model collapsed to `yield`/`done` while its thinking said "call
    /// list_files"; plain JSON mode produced the correct tool call). The
    /// harness validates every output against the schema regardless.
    pub format: OllamaFormat,
    /// Whether to report the output as schema-constrained (when false the
    /// harness pastes the schema into the system prompt). `None` derives it
    /// from `format` (`Schema` → true, otherwise false).
    pub schema_constrained: Option<bool>,
    /// Number of retries after a 5xx response or a transport-level failure
    /// (connection reset/refused, or an EOF mid-stream that isn't the
    /// configured timeout). Live Ollama runners sometimes crash and restart
    /// within seconds (observed: `llama-server process has terminated` /
    /// `model runner has unexpectedly stopped`), so a short retry keeps the
    /// run alive instead of parking it on the first transient 500. Not
    /// retried: 4xx responses, a parse error on an otherwise-successful
    /// response, and the configured request timeout.
    pub retries_on_5xx: u32,
    /// Delay between retries, in seconds. Cancellation aborts the wait
    /// immediately.
    pub retry_delay_secs: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OllamaFormat {
    Schema,
    #[default]
    Json,
    None,
}

impl OllamaConfig {
    pub fn effective_schema_constrained(&self) -> bool {
        self.schema_constrained
            .unwrap_or(self.format == OllamaFormat::Schema)
    }
}

impl Default for OllamaConfig {
    fn default() -> Self {
        OllamaConfig {
            base_url: "http://localhost:11434".to_string(),
            model: "gemma4:26b".to_string(),
            think: true,
            num_ctx: 32768,
            options: json!({}),
            timeout_secs: 600,
            format: OllamaFormat::Json,
            schema_constrained: None,
            retries_on_5xx: 2,
            retry_delay_secs: 15,
        }
    }
}

pub struct OllamaModel {
    cfg: OllamaConfig,
    client: reqwest::Client,
}

#[derive(Deserialize)]
struct ChatLine {
    #[serde(default)]
    message: Option<ChatMessage>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    error: Option<String>,
    /// Present on the final (`done: true`) line. With Ollama's prefix cache,
    /// this is the *full* prompt token count as reported by the server
    /// (observed on Ollama 0.33.2) — not just the newly-evaluated suffix.
    /// It reflects the model's real tokenizer and is authoritative,
    /// unlike statie's local `TiktokenTokenizer` (cl100k) estimate.
    #[serde(default)]
    prompt_eval_count: Option<u64>,
    /// Present on the final (`done: true`) line: number of tokens generated.
    #[serde(default)]
    eval_count: Option<u64>,
    /// Present on the final (`done: true`) line: why generation stopped
    /// (e.g. `"stop"`, `"length"`).
    #[serde(default)]
    done_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChatMessage {
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

/// A free-text (no output schema) chat reply from `OllamaModel::chat_free`.
pub struct FreeReply {
    pub content: String,
    pub thinking: String,
    pub usage: Option<Usage>,
    pub done_reason: Option<String>,
}

impl OllamaModel {
    pub fn new(cfg: OllamaConfig) -> Result<Self, ModelError> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(cfg.timeout_secs))
            .build()
            .map_err(|e| ModelError::Backend(e.to_string()))?;
        Ok(OllamaModel { cfg, client })
    }

    /// The context window this model was configured with (`OllamaConfig::
    /// num_ctx`, sent on every request). Used by `ModelThinker::think`
    /// (`statie-models::thinker`) to name the configured window in the error
    /// message when a reasoning call exhausts it before producing content.
    pub fn num_ctx(&self) -> u32 {
        self.cfg.num_ctx
    }

    fn chat_url(&self) -> String {
        format!("{}/api/chat", self.cfg.base_url.trim_end_matches('/'))
    }

    /// Builds the `/api/chat` request body shared by `step` and
    /// `chat_free`: model, streaming, `think`, the two messages, and
    /// `options` (with `num_ctx` always pinned last so it always wins). The
    /// caller adds `format` on top when it wants one -- `chat_free` never
    /// does.
    fn base_body(&self, system: &str, user: &str, think: bool) -> Value {
        let mut body = json!({
            "model": self.cfg.model,
            "stream": true,
            "think": think,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
        });
        let mut options = match &self.cfg.options {
            Value::Object(map) => map.clone(),
            _ => serde_json::Map::new(),
        };
        options.insert("num_ctx".to_string(), json!(self.cfg.num_ctx));
        body["options"] = Value::Object(options);
        body
    }

    /// A free-text chat call: no output schema, no `format` key regardless
    /// of `cfg.format`. Used by `ModelThinker` (`statie-models::thinker`) to
    /// run a reasoning subagent call against the same Ollama server, with
    /// native thinking on and nothing constraining the reply's shape.
    /// Shares request construction (`base_body`), retry behavior
    /// (`run_request_with_retries`), and the ndjson streaming parser
    /// (`run_request`/`process_line`) with `step`.
    pub async fn chat_free(
        &self,
        system: &str,
        user: &str,
        think: bool,
        cancel: CancellationToken,
    ) -> Result<FreeReply, ModelError> {
        let body = self.base_body(system, user, think);

        let request_text =
            serde_json::to_string(&body).map_err(|e| ModelError::Io(e.to_string()))?;
        tracing::debug!(
            chars = request_text.chars().count(),
            "ollama chat_free request"
        );
        tracing::trace!(body = %request_text, "ollama chat_free request body");

        let mut sink = NullSink;
        let result = tokio::select! {
            result = self.run_request_with_retries(&body, &mut sink, &cancel) => result,
            _ = cancel.cancelled() => Err(ModelError::Cancelled),
        };

        result.map(|(reasoning, content, usage, done_reason)| FreeReply {
            content,
            thinking: reasoning
                .map(|r| r.as_ref().to_string())
                .unwrap_or_default(),
            usage,
            done_reason,
        })
    }
}

#[async_trait::async_trait]
impl Model for OllamaModel {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            native_thinking: self.cfg.think,
            schema_constrained: self.cfg.effective_schema_constrained(),
            prefix_cache: true,
        }
    }

    async fn step(
        &self,
        prompt: BoundedPrompt,
        schema: &Schema,
        sink: &mut dyn StepSink,
        cancel: CancellationToken,
    ) -> Result<RawOutput, ModelError> {
        let mut body = self.base_body(prompt.system(), prompt.user(), self.cfg.think);
        match self.cfg.format {
            OllamaFormat::Schema => body["format"] = schema.as_value().clone(),
            OllamaFormat::Json => body["format"] = json!("json"),
            OllamaFormat::None => {}
        }

        let request_text =
            serde_json::to_string(&body).map_err(|e| ModelError::Io(e.to_string()))?;
        tracing::debug!(chars = request_text.chars().count(), "ollama request");
        tracing::trace!(body = %request_text, "ollama request body");

        tokio::select! {
            result = self.run_request_with_retries(&body, sink, &cancel) => result,
            _ = cancel.cancelled() => Err(ModelError::Cancelled),
        }
        .map(|(reasoning_native, content, usage, finish_reason)| {
            let (reasoning, content) = if self.cfg.think {
                (reasoning_native, content)
            } else if let Some(r) = reasoning_native {
                // Native thinking arrived even though `think` was false;
                // prefer it and leave the content untouched.
                (Some(r), content)
            } else {
                strip_thoughts(&content)
            };
            let mut out = RawOutput::new(prompt.version(), reasoning, content);
            if let Some(u) = usage {
                out = out.with_usage(u);
            }
            if let Some(reason) = finish_reason {
                out = out.with_finish_reason(reason);
            }
            out
        })
    }
}

/// A `ModelError` tagged with whether it's worth retrying: a 5xx response,
/// or a transport-level failure (connection reset/refused, or an EOF
/// mid-stream) that isn't the configured request timeout. 4xx responses,
/// NDJSON parse errors on an otherwise-successful response, and timeouts are
/// never retried.
struct RunError {
    err: ModelError,
    retryable: bool,
}

impl RunError {
    fn fatal(err: ModelError) -> Self {
        RunError {
            err,
            retryable: false,
        }
    }
}

impl OllamaModel {
    /// Runs `run_request`, retrying up to `cfg.retries_on_5xx` times (with
    /// `cfg.retry_delay_secs` between attempts) when the failure is
    /// retryable. Reports each retry via `tracing::warn!` (no `StepSink`
    /// hook fits a generic backend-retry notice). `cancel` aborts the wait
    /// between attempts immediately.
    async fn run_request_with_retries(
        &self,
        body: &Value,
        sink: &mut dyn StepSink,
        cancel: &CancellationToken,
    ) -> Result<(Option<Reasoning>, String, Option<Usage>, Option<String>), ModelError> {
        let mut attempt = 0u32;
        loop {
            match self.run_request(body, sink).await {
                Ok(out) => return Ok(out),
                Err(run_err) if run_err.retryable && attempt < self.cfg.retries_on_5xx => {
                    attempt += 1;
                    tracing::warn!(
                        attempt,
                        max_attempts = self.cfg.retries_on_5xx,
                        delay_secs = self.cfg.retry_delay_secs,
                        error = %run_err.err,
                        "ollama backend error, retrying"
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(self.cfg.retry_delay_secs)) => {}
                        _ = cancel.cancelled() => return Err(ModelError::Cancelled),
                    }
                }
                Err(run_err) => return Err(run_err.err),
            }
        }
    }

    async fn run_request(
        &self,
        body: &Value,
        sink: &mut dyn StepSink,
    ) -> Result<(Option<Reasoning>, String, Option<Usage>, Option<String>), RunError> {
        let response = self.client.post(self.chat_url()).json(body).send().await;
        let response = match response {
            Ok(r) => r,
            Err(e) => {
                let retryable =
                    !e.is_timeout() && (e.is_connect() || e.is_body() || e.is_request());
                return Err(RunError {
                    err: ModelError::Io(e.to_string()),
                    retryable,
                });
            }
        };

        let status = response.status();
        if !status.is_success() {
            let text = response
                .text()
                .await
                .unwrap_or_else(|e| format!("<failed to read body: {e}>"));
            return Err(RunError {
                err: ModelError::Backend(format!("{status}: {text}")),
                retryable: status.is_server_error(),
            });
        }

        let mut reasoning_buf = String::new();
        let mut content_buf = String::new();
        let mut line_buf = String::new();

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    let retryable = !e.is_timeout() && (e.is_connect() || e.is_body());
                    return Err(RunError {
                        err: ModelError::Io(e.to_string()),
                        retryable,
                    });
                }
            };
            let text = String::from_utf8_lossy(&chunk);
            line_buf.push_str(&text);

            loop {
                let Some(newline_idx) = line_buf.find('\n') else {
                    break;
                };
                let line: String = line_buf.drain(..=newline_idx).collect();
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match process_line(line, &mut reasoning_buf, &mut content_buf, sink) {
                    Ok(Some((usage, finish_reason))) => {
                        return Ok((
                            (!reasoning_buf.is_empty()).then(|| Reasoning::new(reasoning_buf)),
                            content_buf,
                            usage,
                            finish_reason,
                        ));
                    }
                    Ok(None) => {}
                    Err(e) => return Err(RunError::fatal(e)),
                }
            }
        }

        // Stream ended without a trailing newline on the final line.
        let remainder = line_buf.trim();
        if !remainder.is_empty() {
            process_line(remainder, &mut reasoning_buf, &mut content_buf, sink)
                .map_err(RunError::fatal)?;
        }

        Ok((
            (!reasoning_buf.is_empty()).then(|| Reasoning::new(reasoning_buf)),
            content_buf,
            None,
            None,
        ))
    }
}

/// `(usage, done_reason)` reported on a `done: true` NDJSON line.
type DoneInfo = (Option<Usage>, Option<String>);

/// Parses one NDJSON line. Returns `Ok(Some((usage, done_reason)))` if this
/// line signals `done` (`usage` is `Some(Usage)` only when the server
/// reported `prompt_eval_count`), `Ok(None)` otherwise.
fn process_line(
    line: &str,
    reasoning_buf: &mut String,
    content_buf: &mut String,
    sink: &mut dyn StepSink,
) -> Result<Option<DoneInfo>, ModelError> {
    tracing::trace!(line, "ollama ndjson line");

    let parsed: ChatLine = serde_json::from_str(line)
        .map_err(|e| ModelError::Backend(format!("invalid ndjson line: {e}")))?;

    if let Some(err) = parsed.error {
        return Err(ModelError::Backend(err));
    }

    if let Some(message) = parsed.message {
        if let Some(thinking) = message.thinking
            && !thinking.is_empty()
        {
            reasoning_buf.push_str(&thinking);
            sink.reasoning_delta(&thinking);
        }
        if let Some(content) = message.content
            && !content.is_empty()
        {
            content_buf.push_str(&content);
            sink.content_delta(&content);
        }
    }

    if !parsed.done {
        return Ok(None);
    }

    let usage = parsed.prompt_eval_count.map(|prompt_tokens| Usage {
        prompt_tokens,
        completion_tokens: parsed.eval_count.unwrap_or(0),
    });
    Ok(Some((usage, parsed.done_reason)))
}
