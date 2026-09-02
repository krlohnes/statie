//! `OllamaModel`: a `Model` backend for a local Ollama server's `/api/chat`
//! endpoint.

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use statie_core::{
    BoundedPrompt, Capabilities, Model, ModelError, RawOutput, Reasoning, Schema, StepSink, Usage,
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

impl OllamaModel {
    pub fn new(cfg: OllamaConfig) -> Result<Self, ModelError> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(cfg.timeout_secs))
            .build()
            .map_err(|e| ModelError::Backend(e.to_string()))?;
        Ok(OllamaModel { cfg, client })
    }

    fn chat_url(&self) -> String {
        format!("{}/api/chat", self.cfg.base_url.trim_end_matches('/'))
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
        let mut body = json!({
            "model": self.cfg.model,
            "stream": true,
            "think": self.cfg.think,
            "messages": [
                { "role": "system", "content": prompt.system() },
                { "role": "user", "content": prompt.user() },
            ],
        });
        match self.cfg.format {
            OllamaFormat::Schema => body["format"] = schema.as_value().clone(),
            OllamaFormat::Json => body["format"] = json!("json"),
            OllamaFormat::None => {}
        }
        // `num_ctx` is always sent and always wins over `options`, so the
        // runner's context size never drifts between requests.
        let mut options = match &self.cfg.options {
            Value::Object(map) => map.clone(),
            _ => serde_json::Map::new(),
        };
        options.insert("num_ctx".to_string(), json!(self.cfg.num_ctx));
        body["options"] = Value::Object(options);

        let request_text =
            serde_json::to_string(&body).map_err(|e| ModelError::Io(e.to_string()))?;
        tracing::debug!(chars = request_text.chars().count(), "ollama request");
        tracing::trace!(body = %request_text, "ollama request body");

        tokio::select! {
            result = self.run_request(&body, sink) => result,
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

impl OllamaModel {
    async fn run_request(
        &self,
        body: &Value,
        sink: &mut dyn StepSink,
    ) -> Result<(Option<Reasoning>, String, Option<Usage>, Option<String>), ModelError> {
        let response = self
            .client
            .post(self.chat_url())
            .json(body)
            .send()
            .await
            .map_err(|e| ModelError::Io(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let text = response
                .text()
                .await
                .unwrap_or_else(|e| format!("<failed to read body: {e}>"));
            return Err(ModelError::Backend(format!("{status}: {text}")));
        }

        let mut reasoning_buf = String::new();
        let mut content_buf = String::new();
        let mut line_buf = String::new();

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| ModelError::Io(e.to_string()))?;
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
                if let Some((usage, finish_reason)) =
                    process_line(line, &mut reasoning_buf, &mut content_buf, sink)?
                {
                    return Ok((
                        (!reasoning_buf.is_empty()).then(|| Reasoning::new(reasoning_buf)),
                        content_buf,
                        usage,
                        finish_reason,
                    ));
                }
            }
        }

        // Stream ended without a trailing newline on the final line.
        let remainder = line_buf.trim();
        if !remainder.is_empty() {
            process_line(remainder, &mut reasoning_buf, &mut content_buf, sink)?;
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
